use anyhow::{anyhow, Error, Result};
use clap::Args;
use maelstrom_base::{
    ClientJobId, JobCompleted, JobEffects, JobError, JobOutcome, JobOutcomeResult, JobOutputResult,
    JobStatus, JobTty, WindowSize,
};
use maelstrom_client::{
    AcceptInvalidRemoteContainerTlsCerts, CacheDir, Client, ClientBgProcess,
    ContainerImageDepotDir, JobSpec, ProjectDir, StateDir,
};
use maelstrom_linux::{self as linux, Fd, Signal, SockaddrUnStorage, SocketDomain, SocketType};
use maelstrom_macro::Config;
use maelstrom_run::spec::job_spec_iter_from_reader;
use maelstrom_util::{
    config::common::{BrokerAddr, CacheSize, InlineLimit, LogLevel, Slots},
    fs::Fs,
    log,
    process::{ExitCode, ExitCodeAccumulator},
    root::{Root, RootBuf},
};
use slog::Logger;
use std::{
    env,
    io::{self, IsTerminal as _, Read, Write as _},
    mem,
    os::{
        fd::OwnedFd,
        unix::net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::{mpsc, Arc, Condvar, Mutex},
    thread,
};
use xdg::BaseDirectories;

#[derive(Config, Debug)]
pub struct Config {
    /// Socket address of broker. If not provided, all jobs will be run locally.
    #[config(
        option,
        short = 'b',
        value_name = "SOCKADDR",
        default = r#""standalone mode""#
    )]
    pub broker: Option<BrokerAddr>,

    /// Minimum log level to output.
    #[config(short = 'l', value_name = "LEVEL", default = r#""info""#)]
    pub log_level: LogLevel,

    /// Directory in which to put cached container images.
    #[config(
        value_name = "PATH",
        default = r#"|bd: &BaseDirectories| {
            bd.get_cache_home()
                .parent()
                .unwrap()
                .join("container/")
                .into_os_string()
                .into_string()
                .unwrap()
        }"#
    )]
    pub container_image_depot_root: RootBuf<ContainerImageDepotDir>,

    /// Directory for state that persists between runs, including the client's log file.
    #[config(
        value_name = "PATH",
        default = r#"|bd: &BaseDirectories| {
            bd.get_state_home()
                .into_os_string()
                .into_string()
                .unwrap()
        }"#
    )]
    pub state_root: RootBuf<StateDir>,

    /// Directory to use for the cache. The local worker's cache will be contained within it.
    #[config(
        value_name = "PATH",
        default = r#"|bd: &BaseDirectories| {
            bd.get_cache_home()
                .into_os_string()
                .into_string()
                .unwrap()
        }"#
    )]
    pub cache_root: RootBuf<CacheDir>,

    /// The target amount of disk space to use for the cache. This bound won't be followed
    /// strictly, so it's best to be conservative. SI and binary suffixes are supported.
    #[config(
        value_name = "BYTES",
        default = "CacheSize::default()",
        next_help_heading = "Local Worker Options"
    )]
    pub cache_size: CacheSize,

    /// The maximum amount of bytes to return inline for captured stdout and stderr.
    #[config(value_name = "BYTES", default = "InlineLimit::default()")]
    pub inline_limit: InlineLimit,

    /// The number of job slots available.
    #[config(value_name = "N", default = "Slots::default()")]
    pub slots: Slots,

    /// Whether to accept invalid TLS certificates when downloading container images.
    #[config(flag, value_name = "ACCEPT_INVALID_REMOTE_CONTAINER_TLS_CERTS")]
    pub accept_invalid_remote_container_tls_certs: AcceptInvalidRemoteContainerTlsCerts,
}

#[derive(Args)]
#[command(next_help_heading = "Other Command-Line Options")]
pub struct ExtraCommandLineOptions {
    #[arg(
        long,
        short = 'f',
        value_name = "PATH",
        help = "Read the job specifications from the provided file, instead of from standard \
            input."
    )]
    pub file: Option<PathBuf>,

    #[command(flatten)]
    pub one_or_tty: OneOrTty,

    #[arg(
        num_args = 0..,
        requires = "OneOrTty",
        value_name = "PROGRAM-AND-ARGUMENTS",
        help = "Program and arguments override. Can only be used with --one or --tty. If provided \
            these will be used for the program and arguments, ignoring whatever is in the job \
            specification."
    )]
    pub args: Vec<String>,
}

#[derive(Args)]
#[group(multiple = false)]
pub struct OneOrTty {
    #[arg(
        long,
        short = '1',
        help = "Execute only one job. If multiple job specifications are provided, all but the \
            first are ignored. Optionally, positional arguments can be provided to override the \
            job's program and arguments"
    )]
    pub one: bool,

    #[arg(
        long,
        short = 't',
        help = "Execute only one job, with its standard input, output, and error assigned to \
            a TTY. The TTY in turn will be connected to this process's standard input, output. \
            This process's standard input and output must be connected to a TTY. If multiple job \
            specifications are provided, all but the first are ignored. Optionally, positional \
            arguments can be provided to override the job's program and arguments."
    )]
    pub tty: bool,
}

impl OneOrTty {
    fn any(&self) -> bool {
        self.one || self.tty
    }
}

fn print_effects(
    cjid: Option<ClientJobId>,
    JobEffects {
        stdout,
        stderr,
        duration: _,
    }: JobEffects,
) -> Result<()> {
    match stdout {
        JobOutputResult::None => {}
        JobOutputResult::Inline(bytes) => {
            io::stdout().lock().write_all(&bytes)?;
        }
        JobOutputResult::Truncated { first, truncated } => {
            io::stdout().lock().write_all(&first)?;
            io::stdout().lock().flush()?;
            if let Some(cjid) = cjid {
                eprintln!("job {cjid}: stdout truncated, {truncated} bytes lost");
            } else {
                eprintln!("stdout truncated, {truncated} bytes lost");
            }
        }
    }
    match stderr {
        JobOutputResult::None => {}
        JobOutputResult::Inline(bytes) => {
            io::stderr().lock().write_all(&bytes)?;
        }
        JobOutputResult::Truncated { first, truncated } => {
            io::stderr().lock().write_all(&first)?;
            if let Some(cjid) = cjid {
                eprintln!("job {cjid}: stderr truncated, {truncated} bytes lost");
            } else {
                eprintln!("stderr truncated, {truncated} bytes lost");
            }
        }
    }
    Ok(())
}

fn visitor(res: Result<(ClientJobId, JobOutcomeResult)>, tracker: Arc<JobTracker>) {
    let exit_code = match res {
        Ok((cjid, Ok(JobOutcome::Completed(JobCompleted { status, effects })))) => {
            print_effects(Some(cjid), effects).ok();
            match status {
                JobStatus::Exited(0) => ExitCode::SUCCESS,
                JobStatus::Exited(code) => {
                    io::stdout().lock().flush().ok();
                    eprintln!("job {cjid}: exited with code {code}");
                    ExitCode::from(code)
                }
                JobStatus::Signaled(signum) => {
                    io::stdout().lock().flush().ok();
                    eprintln!("job {cjid}: killed by signal {signum}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok((cjid, Ok(JobOutcome::TimedOut(effects)))) => {
            print_effects(Some(cjid), effects).ok();
            io::stdout().lock().flush().ok();
            eprintln!("job {cjid}: timed out");
            ExitCode::FAILURE
        }
        Ok((cjid, Err(JobError::Execution(err)))) => {
            eprintln!("job {cjid}: execution error: {err}");
            ExitCode::FAILURE
        }
        Ok((cjid, Err(JobError::System(err)))) => {
            eprintln!("job {cjid}: system error: {err}");
            ExitCode::FAILURE
        }
        Err(err) => {
            eprintln!("remote error: {err}");
            ExitCode::FAILURE
        }
    };
    tracker.job_completed(exit_code);
}

#[derive(Default)]
struct JobTracker {
    condvar: Condvar,
    outstanding: Mutex<usize>,
    accum: ExitCodeAccumulator,
}

impl JobTracker {
    fn add_outstanding(&self) {
        let mut locked = self.outstanding.lock().unwrap();
        *locked += 1;
    }

    fn job_completed(&self, exit_code: ExitCode) {
        let mut locked = self.outstanding.lock().unwrap();
        *locked -= 1;
        self.accum.add(exit_code);
        self.condvar.notify_one();
    }

    fn wait_for_outstanding(&self) {
        let mut locked = self.outstanding.lock().unwrap();
        while *locked > 0 {
            locked = self.condvar.wait(locked).unwrap();
        }
    }
}

fn mimic_child_death(res: JobOutcomeResult) -> Result<ExitCode> {
    Ok(match res {
        Ok(JobOutcome::Completed(JobCompleted { status, effects })) => {
            print_effects(None, effects)?;
            match status {
                JobStatus::Exited(code) => code.into(),
                JobStatus::Signaled(signo) => {
                    let _ = linux::raise(signo.into());
                    let _ = linux::raise(Signal::KILL);
                    unreachable!()
                }
            }
        }
        Ok(JobOutcome::TimedOut(effects)) => {
            print_effects(None, effects)?;
            io::stdout().lock().flush()?;
            eprintln!("timed out");
            ExitCode::FAILURE
        }
        Err(JobError::Execution(err)) => {
            eprintln!("execution error: {err}");
            ExitCode::FAILURE
        }
        Err(JobError::System(err)) => {
            eprintln!("system error: {err}");
            ExitCode::FAILURE
        }
    })
}

fn one_main(client: Client, job_spec: JobSpec) -> Result<ExitCode> {
    mimic_child_death(client.run_job(job_spec)?.1)
}

#[allow(clippy::large_enum_variant)]
enum TtyMainMessage {
    Error(Error),
    JobCompleted(JobOutcomeResult),
    JobConnected(UnixStream, UnixStream),
    JobOutput([u8; 1024], usize),
}

fn tty_listener_main(sock: linux::OwnedFd) -> Result<(UnixStream, UnixStream)> {
    let listener = UnixListener::from(OwnedFd::from(sock));
    let sock = listener.accept()?.0;
    let sock_clone = sock.try_clone()?;
    Ok((sock, sock_clone))
}

fn tty_main(client: Client, mut job_spec: JobSpec) -> Result<ExitCode> {
    let sock = linux::socket(SocketDomain::UNIX, SocketType::STREAM, Default::default())?;
    linux::bind(sock.as_fd(), &SockaddrUnStorage::new_autobind())?;
    linux::listen(sock.as_fd(), 1)?;
    let sockaddr = linux::getsockname(sock.as_fd())?;
    let sockaddr = sockaddr
        .as_sockaddr_un()
        .ok_or_else(|| anyhow!("socket is not a unix domain socket"))?;
    let (rows, columns) = linux::ioctl_tiocgwinsz(Fd::STDIN)?;
    job_spec.allocate_tty = Some(JobTty::new(
        sockaddr.path().try_into()?,
        WindowSize::new(rows, columns),
    ));

    let (sender, receiver) = mpsc::sync_channel(0);

    let sender_clone = sender.clone();
    thread::spawn(move || {
        let _ = sender_clone.send(match client.run_job(job_spec) {
            Ok((_cjid, result)) => TtyMainMessage::JobCompleted(result),
            Err(err) => TtyMainMessage::Error(err.context("client error")),
        });
    });

    println!("waiting for job to start");
    let sender_clone = sender.clone();
    thread::spawn(move || {
        let _ = sender_clone.send(match tty_listener_main(sock) {
            Ok((sock1, sock2)) => TtyMainMessage::JobConnected(sock1, sock2),
            Err(err) => TtyMainMessage::Error(err.context("job connecting")),
        });
    });

    let mut in_raw_mode = false;
    let result = loop {
        match receiver.recv()? {
            TtyMainMessage::Error(err) => {
                break Err(err);
            }
            TtyMainMessage::JobCompleted(result) => {
                break Ok(result);
            }
            TtyMainMessage::JobConnected(mut sock1, mut sock2) => {
                println!("job started, going into raw mode");
                crossterm::terminal::enable_raw_mode()?;
                in_raw_mode = true;
                let sender_clone = sender.clone();
                thread::spawn(move || -> Result<()> {
                    let mut bytes = [0u8; 1024];
                    loop {
                        match sock1.read(&mut bytes) {
                            Err(err) => {
                                sender_clone.send(TtyMainMessage::Error(
                                    Error::new(err).context("reading from job"),
                                ))?;
                                break;
                            }
                            Ok(0) => {
                                break;
                            }
                            Ok(n) => {
                                sender_clone.send(TtyMainMessage::JobOutput(bytes, n))?;
                            }
                        }
                    }
                    Ok(())
                });
                thread::spawn(move || {
                    let _ = io::copy(&mut io::stdin(), &mut sock2);
                });
            }
            TtyMainMessage::JobOutput(bytes, n) => {
                io::stdout().write_all(&bytes[..n])?;
                io::stdout().flush()?;
            }
        }
    };
    if in_raw_mode {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    mimic_child_death(result?)
}

fn main_with_logger(
    config: Config,
    mut extra_options: ExtraCommandLineOptions,
    bg_proc: ClientBgProcess,
    log: Logger,
) -> Result<ExitCode> {
    let fs = Fs::new();
    let reader: Box<dyn Read> = match extra_options.file {
        Some(path) => Box::new(fs.open_file(path)?),
        None => Box::new(io::stdin().lock()),
    };
    fs.create_dir_all(&config.cache_root)?;
    fs.create_dir_all(&config.state_root)?;
    fs.create_dir_all(&config.container_image_depot_root)?;
    let client = Client::new(
        bg_proc,
        config.broker,
        Root::<ProjectDir>::new(".".as_ref()),
        config.state_root,
        config.container_image_depot_root,
        config.cache_root,
        config.cache_size,
        config.inline_limit,
        config.slots,
        config.accept_invalid_remote_container_tls_certs,
        log,
    )?;
    let mut job_specs = job_spec_iter_from_reader(reader, |layer| client.add_layer(layer));
    if extra_options.one_or_tty.any() {
        let mut job_spec = job_specs
            .next()
            .ok_or_else(|| anyhow!("no job specification provided"))??;
        drop(job_specs);
        match &mem::take(&mut extra_options.args)[..] {
            [] => {}
            [program, arguments @ ..] => {
                job_spec.program = program.into();
                job_spec.arguments = arguments.to_vec();
            }
        }
        if extra_options.one_or_tty.tty {
            tty_main(client, job_spec)
        } else {
            one_main(client, job_spec)
        }
    } else {
        let tracker = Arc::new(JobTracker::default());
        for job_spec in job_specs {
            let tracker = tracker.clone();
            tracker.add_outstanding();
            client.add_job(job_spec?, move |res| visitor(res, tracker))?;
        }
        tracker.wait_for_outstanding();
        Ok(tracker.accum.get())
    }
}

fn main() -> Result<ExitCode> {
    let (config, extra_options): (_, ExtraCommandLineOptions) =
        Config::new_with_extra_from_args("maelstrom/run", "MAELSTROM_RUN", env::args())?;
    if extra_options.one_or_tty.tty {
        if !io::stdin().is_terminal() {
            eprintln!("error: standard input is not a terminal");
            return Ok(ExitCode::FAILURE);
        }
        if !io::stdout().is_terminal() {
            eprintln!("error: standard output is not a terminal");
            return Ok(ExitCode::FAILURE);
        }
    }

    let bg_proc = ClientBgProcess::new_from_fork(config.log_level)?;

    log::run_with_logger(config.log_level, |log| {
        main_with_logger(config, extra_options, bg_proc, log)
    })
}
