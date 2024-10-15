use crate::{
    dispatcher::{self, Message},
    fetcher,
    types::{DispatcherSender, TempFileFactory},
};
use maelstrom_base::Sha256Digest;
use maelstrom_util::{
    cache::{fs::TempFile as _, GotArtifact},
    config::common::BrokerAddr,
};
use slog::{debug, o, Logger};
use std::{sync::Arc, thread};
use std_semaphore::Semaphore;

pub const MAX_ARTIFACT_FETCHES: u64 = 10;

pub struct ArtifactFetcher {
    broker_addr: BrokerAddr,
    dispatcher_sender: DispatcherSender,
    log: Logger,
    semaphore: Arc<Semaphore>,
    temp_file_factory: TempFileFactory,
}

impl ArtifactFetcher {
    pub fn new(
        dispatcher_sender: DispatcherSender,
        broker_addr: BrokerAddr,
        log: Logger,
        temp_file_factory: TempFileFactory,
    ) -> Self {
        ArtifactFetcher {
            broker_addr,
            dispatcher_sender,
            log,
            semaphore: Arc::new(Semaphore::new(MAX_ARTIFACT_FETCHES as isize)),
            temp_file_factory,
        }
    }
}

impl dispatcher::ArtifactFetcher for ArtifactFetcher {
    fn start_artifact_fetch(&mut self, digest: Sha256Digest) {
        let log = self.log.new(o!(
            "digest" => digest.to_string(),
            "broker_addr" => self.broker_addr.to_string()
        ));
        main(
            self.broker_addr,
            digest,
            self.dispatcher_sender.clone(),
            log,
            self.semaphore.clone(),
            self.temp_file_factory.clone(),
        );
    }
}

fn main(
    broker_addr: BrokerAddr,
    digest: Sha256Digest,
    dispatcher_sender: DispatcherSender,
    mut log: Logger,
    semaphore: Arc<Semaphore>,
    temp_file_factory: TempFileFactory,
) {
    match temp_file_factory.temp_file() {
        Err(err) => {
            debug!(log, "artifact fetcher failed to get a temporary file"; "err" => ?err);
            dispatcher_sender
                .send(Message::ArtifactFetchCompleted(digest, Err(err)))
                .ok();
        }
        Ok(temp_file) => {
            debug!(log, "artifact fetcher starting");
            thread::spawn(move || {
                let _permit = semaphore.access();
                let result =
                    fetcher::main(&digest, temp_file.path().to_owned(), broker_addr, &mut log);
                debug!(log, "artifact fetcher completed"; "result" => ?result);
                dispatcher_sender
                    .send(Message::ArtifactFetchCompleted(
                        digest,
                        result.map(|_| GotArtifact::File { source: temp_file }),
                    ))
                    .ok();
            });
        }
    }
}
