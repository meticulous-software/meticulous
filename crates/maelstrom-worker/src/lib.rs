//! Code for the worker binary.

pub mod config;
pub mod local_worker;

mod artifact_fetcher;
mod connection;
mod dispatcher;
mod dispatcher_adapter;
mod executor;
mod layer_fs;
mod manifest_digest_cache;
mod types;

use anyhow::{anyhow, bail, Error, Result};
use artifact_fetcher::{GitHubArtifactFetcher, TcpArtifactFetcher};
use config::Config;
use connection::{BrokerConnection, BrokerReadConnection as _, BrokerWriteConnection as _};
use dispatcher::{Dispatcher, Message};
use dispatcher_adapter::DispatcherAdapter;
use executor::{MountDir, TmpfsDir};
use maelstrom_github::{GitHubClient, GitHubQueue};
use maelstrom_layer_fs::BlobDir;
use maelstrom_linux::{self as linux};
use maelstrom_util::{
    cache::{self, fs::std::Fs as StdFs, TempFileFactory},
    config::common::{ArtifactTransferStrategy, CacheSize, InlineLimit, Slots},
    root::RootBuf,
    signal,
};
use slog::{debug, error, info, Logger};
use std::{future::Future, process, sync::Arc};
use tokio::{
    net::TcpStream,
    sync::mpsc,
    task::{self, JoinHandle},
};
use types::{
    BrokerSender, BrokerSocketOutgoingSender, Cache, DispatcherReceiver, DispatcherSender,
};

fn env_or_error(key: &str) -> Result<String> {
    std::env::var(key).map_err(|_| anyhow!("{key} environment variable missing"))
}

fn github_client_factory() -> Result<Arc<GitHubClient>> {
    // XXX remi: I would prefer if we didn't read these from environment variables.
    let token = env_or_error("ACTIONS_RUNTIME_TOKEN")?;
    let base_url = url::Url::parse(&env_or_error("ACTIONS_RESULTS_URL")?)?;
    Ok(Arc::new(GitHubClient::new(&token, base_url)?))
}

const MAX_PENDING_LAYERS_BUILDS: usize = 10;
const MAX_ARTIFACT_FETCHES: usize = 1;

pub fn main(config: Config, log: Logger) -> Result<()> {
    use maelstrom_util::config::common::BrokerConnection::*;

    info!(log, "started"; "config" => ?config, "pid" => process::id());
    let err = match config.broker_connection {
        Tcp => main_inner::<TcpStream>(config, &log).unwrap_err(),
        GitHub => main_inner::<GitHubQueue>(config, &log).unwrap_err(),
    };
    error!(log, "exiting"; "error" => %err);
    Err(err)
}

/// The main function for the worker. This should be called on a task of its own. It will return
/// when a signal is received or when one of the worker tasks completes because of an error.
#[tokio::main]
async fn main_inner<ConnectionT: BrokerConnection>(config: Config, log: &Logger) -> Result<()> {
    check_open_file_limit(log, config.slots, 0)?;

    let (read_stream, write_stream) =
        ConnectionT::connect(&config.broker, config.slots, log).await?;

    let (dispatcher_sender, dispatcher_receiver) = mpsc::unbounded_channel();
    let (broker_socket_outgoing_sender, broker_socket_outgoing_receiver) =
        mpsc::unbounded_channel();

    let log_clone = log.clone();
    let dispatcher_sender_clone = dispatcher_sender.clone();
    task::spawn(shutdown_on_error(
        read_stream.read_messages(dispatcher_sender_clone, log_clone),
        dispatcher_sender.clone(),
    ));

    let log_clone = log.clone();
    task::spawn(shutdown_on_error(
        write_stream.write_messages(broker_socket_outgoing_receiver, log_clone),
        dispatcher_sender.clone(),
    ));

    task::spawn(shutdown_on_error(
        wait_for_signal(log.clone()),
        dispatcher_sender.clone(),
    ));

    Err(start_dispatcher_task(
        config,
        dispatcher_receiver,
        dispatcher_sender,
        broker_socket_outgoing_sender,
        log,
    )?
    .await?)
}

/// Check if the open file limit is high enough to fit our estimate of how many files we need.
pub fn check_open_file_limit(log: &Logger, slots: Slots, extra: u64) -> Result<()> {
    let limit = linux::getrlimit(linux::RlimitResource::NoFile)?;
    let estimate = open_file_max(slots) + extra;
    debug!(log, "checking open file limit"; "limit" => ?limit.current, "estimate" => estimate);
    if limit.current < estimate {
        let estimate = round_to_multiple(estimate, 1024);
        bail!("Open file limit is too low. Increase limit by running `ulimit -n {estimate}`");
    }
    Ok(())
}

/// For the number of slots, what is the maximum number of files we will open. This attempts to
/// come up with a number by doing some math, but nothing is guaranteeing the result.
fn open_file_max(slots: Slots) -> u64 {
    let existing_open_files: u64 = 3 /* stdout, stdin, stderr */;
    let per_slot_estimate: u64 = 6 /* unix socket, FUSE connection, (stdout, stderr) * 2 */ +
        maelstrom_fuse::MAX_PENDING as u64 /* each FUSE request opens a file */;
    existing_open_files
        + (maelstrom_layer_fs::READER_CACHE_SIZE * 2) // 1 for socket, 1 for the file
        + MAX_ARTIFACT_FETCHES as u64
        + per_slot_estimate * u16::from(slots) as u64
        + (MAX_PENDING_LAYERS_BUILDS * maelstrom_layer_fs::LAYER_BUILDING_FILE_MAX) as u64
}

fn round_to_multiple(n: u64, k: u64) -> u64 {
    if n % k == 0 {
        n
    } else {
        n + (k - (n % k))
    }
}

async fn shutdown_on_error(
    fut: impl Future<Output = Result<()>>,
    dispatcher_sender: DispatcherSender,
) {
    if let Err(error) = fut.await {
        let _ = dispatcher_sender.send(Message::ShutDown(error));
    }
}

async fn wait_for_signal(log: Logger) -> Result<()> {
    let signal = signal::wait_for_signal(log).await;
    Err(anyhow!("signal {signal}"))
}

fn start_dispatcher_task(
    config: Config,
    dispatcher_receiver: DispatcherReceiver,
    dispatcher_sender: DispatcherSender,
    broker_socket_outgoing_sender: BrokerSocketOutgoingSender,
    log: &Logger,
) -> Result<JoinHandle<Error>> {
    let log_clone = log.clone();
    let dispatcher_sender_clone = dispatcher_sender.clone();
    let max_simultaneous_fetches = u32::try_from(MAX_ARTIFACT_FETCHES)
        .unwrap()
        .try_into()
        .unwrap();
    let broker_sender = BrokerSender::new(broker_socket_outgoing_sender);

    let args = DispatcherArgs {
        broker_sender,
        cache_size: config.cache_size,
        cache_root: config.cache_root,
        dispatcher_receiver,
        dispatcher_sender,
        inline_limit: config.inline_limit,
        log: log.clone(),
        log_initial_cache_message_at_info: true,
        slots: config.slots,
    };

    match config.artifact_transfer_strategy {
        ArtifactTransferStrategy::TcpUpload => {
            let artifact_fetcher_factory = move |temp_file_factory| {
                TcpArtifactFetcher::new(
                    max_simultaneous_fetches,
                    dispatcher_sender_clone,
                    config.broker,
                    log_clone,
                    temp_file_factory,
                )
            };
            start_dispatcher_task_common(artifact_fetcher_factory, args)
        }
        ArtifactTransferStrategy::GitHub => {
            let github_client = github_client_factory()?;
            let artifact_fetcher_factory = move |temp_file_factory| {
                GitHubArtifactFetcher::new(
                    max_simultaneous_fetches,
                    github_client,
                    dispatcher_sender_clone,
                    log_clone,
                    temp_file_factory,
                )
            };
            start_dispatcher_task_common(artifact_fetcher_factory, args)
        }
    }
}

struct DispatcherArgs<BrokerSenderT> {
    broker_sender: BrokerSenderT,
    cache_size: CacheSize,
    cache_root: RootBuf<config::CacheDir>,
    dispatcher_receiver: DispatcherReceiver,
    dispatcher_sender: DispatcherSender,
    inline_limit: InlineLimit,
    log: Logger,
    log_initial_cache_message_at_info: bool,
    slots: Slots,
}

#[allow(clippy::too_many_arguments)]
fn start_dispatcher_task_common<
    ArtifactFetcherT: dispatcher::ArtifactFetcher + Send + 'static,
    ArtifactFetcherFactoryT: FnOnce(TempFileFactory<StdFs>) -> ArtifactFetcherT,
    BrokerSenderT: dispatcher::BrokerSender + Send + 'static,
>(
    artifact_fetcher_factory: ArtifactFetcherFactoryT,
    args: DispatcherArgs<BrokerSenderT>,
) -> Result<JoinHandle<Error>> {
    let (cache, temp_file_factory) = Cache::new(
        StdFs,
        args.cache_root.join::<cache::CacheDir>("artifacts"),
        args.cache_size,
        args.log.clone(),
        args.log_initial_cache_message_at_info,
    )?;

    let artifact_fetcher = artifact_fetcher_factory(temp_file_factory.clone());

    let dispatcher_adapter = DispatcherAdapter::new(
        args.dispatcher_sender,
        args.inline_limit,
        args.log.clone(),
        args.cache_root.join::<MountDir>("mount"),
        args.cache_root.join::<TmpfsDir>("upper"),
        cache.root().join::<BlobDir>("sha256/blob"),
        temp_file_factory,
    )?;

    let mut dispatcher = Dispatcher::new(
        dispatcher_adapter,
        artifact_fetcher,
        args.broker_sender,
        cache,
        args.slots,
    );

    let mut dispatcher_receiver = args.dispatcher_receiver;
    let dispatcher_main = async move {
        loop {
            let msg = dispatcher_receiver
                .recv()
                .await
                .expect("missing shut down message");
            if let Err(err) = dispatcher.receive_message(msg) {
                break err;
            }
        }
    };
    Ok(task::spawn(dispatcher_main))
}
