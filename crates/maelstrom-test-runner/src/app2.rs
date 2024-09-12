mod job_output;
mod main_app;

#[cfg(test)]
mod tests;

use crate::config::{Repeat, StopAfter};
use crate::metadata::{AllMetadata, TestMetadata};
use crate::test_db::{TestDb, TestDbStore};
use crate::ui::{Ui, UiJobId as JobId, UiMessage};
use crate::*;
use maelstrom_base::Timeout;
use maelstrom_client::{
    spec::{JobSpec, LayerSpec},
    JobStatus, ProjectDir, StateDir,
};
use maelstrom_util::{fs::Fs, process::ExitCode, root::Root};
use main_app::MainApp;
use std::sync::mpsc::{Receiver, Sender};

type ArtifactM<DepsT> = <<DepsT as Deps>::TestCollector as CollectTests>::Artifact;
type ArtifactKeyM<DepsT> = <<DepsT as Deps>::TestCollector as CollectTests>::ArtifactKey;
type CaseMetadataM<DepsT> = <<DepsT as Deps>::TestCollector as CollectTests>::CaseMetadata;
type CollectOptionsM<DepsT> = <<DepsT as Deps>::TestCollector as CollectTests>::Options;
type PackageM<DepsT> = <<DepsT as Deps>::TestCollector as CollectTests>::Package;
type PackageIdM<DepsT> = <<DepsT as Deps>::TestCollector as CollectTests>::PackageId;
type TestFilterM<DepsT> = <<DepsT as Deps>::TestCollector as CollectTests>::TestFilter;
type AllMetadataM<DepsT> = AllMetadata<TestFilterM<DepsT>>;
type TestDbM<DepsT> = TestDb<ArtifactKeyM<DepsT>, CaseMetadataM<DepsT>>;

trait Deps {
    type TestCollector: CollectTests;

    fn start_collection(
        &self,
        color: bool,
        options: &CollectOptionsM<Self>,
        packages: Vec<&PackageM<Self>>,
    );
    fn get_packages(&self);
    fn add_job(&self, job_id: JobId, spec: JobSpec);
    fn list_tests(&self, artifact: ArtifactM<Self>);
    fn start_shutdown(&self);
    fn get_test_layers(
        &self,
        artifact: &ArtifactM<Self>,
        metadata: &TestMetadata,
    ) -> Vec<LayerSpec>;
    fn send_ui_msg(&self, msg: UiMessage);
}

pub struct MainAppCombinedDeps<MainAppDepsT: MainAppDeps> {
    abstract_deps: MainAppDepsT,
    log: slog::Logger,
    test_metadata: AllMetadata<super::TestFilterM<MainAppDepsT>>,
    collector_options: super::CollectOptionsM<MainAppDepsT>,
    test_db_store:
        TestDbStore<super::ArtifactKeyM<MainAppDepsT>, super::CaseMetadataM<MainAppDepsT>>,
}

impl<MainAppDepsT: MainAppDeps> MainAppCombinedDeps<MainAppDepsT> {
    /// Creates a new `MainAppCombinedDeps`
    ///
    /// `bg_proc`: handle to background client process
    /// `include_filter`: tests which match any of the patterns in this filter are run
    /// `exclude_filter`: tests which match any of the patterns in this filter are not run
    /// `list_action`: if some, tests aren't run, instead tests or other things are listed
    /// `stderr_color`: should terminal color codes be written to `stderr` or not
    /// `project_dir`: the path to the root of the project
    /// `broker_addr`: the network address of the broker which we connect to
    /// `client_driver`: an object which drives the background work of the `Client`
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        abstract_deps: MainAppDepsT,
        _include_filter: Vec<String>,
        _exclude_filter: Vec<String>,
        _list_action: Option<ListAction>,
        _repeat: Repeat,
        _stop_after: Option<StopAfter>,
        _stderr_color: bool,
        project_dir: impl AsRef<Root<ProjectDir>>,
        state_dir: impl AsRef<Root<StateDir>>,
        collector_options: super::CollectOptionsM<MainAppDepsT>,
        log: slog::Logger,
    ) -> Result<Self> {
        let mut test_metadata = AllMetadata::load(
            log.clone(),
            project_dir,
            MainAppDepsT::TEST_METADATA_FILE_NAME,
            MainAppDepsT::DEFAULT_TEST_METADATA_CONTENTS,
        )?;
        let test_db_store = TestDbStore::new(Fs::new(), &state_dir);

        let vars = abstract_deps.get_template_vars(&collector_options)?;
        test_metadata.replace_template_vars(&vars)?;

        Ok(Self {
            abstract_deps,
            log,
            test_metadata,
            test_db_store,
            collector_options,
        })
    }
}

enum MainAppMessage<PackageT: 'static, ArtifactT: 'static, CaseMetadataT: 'static> {
    Start,
    Packages {
        packages: Vec<PackageT>,
    },
    ArtifactBuilt {
        artifact: ArtifactT,
    },
    TestsListed {
        artifact: ArtifactT,
        listing: Vec<(String, CaseMetadataT)>,
    },
    FatalError {
        error: anyhow::Error,
    },
    JobUpdate {
        job_id: JobId,
        result: Result<JobStatus>,
    },
    CollectionFinished,
    Shutdown,
}

type MainAppMessageM<DepsT> =
    MainAppMessage<PackageM<DepsT>, ArtifactM<DepsT>, CaseMetadataM<DepsT>>;

struct MainAppDepsAdapter<'deps, 'scope, MainAppDepsT: MainAppDeps> {
    deps: &'deps MainAppDepsT,
    scope: &'scope std::thread::Scope<'scope, 'deps>,
    main_app_sender: Sender<MainAppMessageM<Self>>,
    ui: UiSender,
}

impl<'deps, 'scope, MainAppDepsT: MainAppDeps> Deps
    for MainAppDepsAdapter<'deps, 'scope, MainAppDepsT>
{
    type TestCollector = MainAppDepsT::TestCollector;

    fn start_collection(
        &self,
        color: bool,
        options: &CollectOptionsM<Self>,
        packages: Vec<&PackageM<Self>>,
    ) {
        let sender = self.main_app_sender.clone();
        match self
            .deps
            .test_collector()
            .start(color, options, packages, &self.ui)
        {
            Ok((build_handle, artifact_stream)) => {
                self.scope.spawn(move || {
                    for artifact in artifact_stream {
                        match artifact {
                            Ok(artifact) => {
                                if sender
                                    .send(MainAppMessage::ArtifactBuilt { artifact })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(error) => {
                                let _ = sender.send(MainAppMessage::FatalError { error });
                                break;
                            }
                        }
                    }
                    if let Err(error) = build_handle.wait() {
                        let _ = sender.send(MainAppMessage::FatalError { error });
                    } else {
                        let _ = sender.send(MainAppMessage::CollectionFinished);
                    }
                });
            }
            Err(error) => {
                let _ = sender.send(MainAppMessage::FatalError { error });
            }
        }
    }

    fn get_packages(&self) {
        let deps = self.deps;
        let sender = self.main_app_sender.clone();
        let ui = self.ui.clone();
        self.scope
            .spawn(move || match deps.test_collector().get_packages(&ui) {
                Ok(packages) => {
                    let _ = sender.send(MainAppMessage::Packages { packages });
                }
                Err(error) => {
                    let _ = sender.send(MainAppMessage::FatalError { error });
                }
            });
    }

    fn add_job(&self, job_id: JobId, spec: JobSpec) {
        let sender = self.main_app_sender.clone();
        let res = self.deps.client().add_job(spec, move |result| {
            let _ = sender.send(MainAppMessage::JobUpdate { job_id, result });
        });
        if let Err(error) = res {
            let _ = self
                .main_app_sender
                .send(MainAppMessage::FatalError { error });
        }
    }

    fn list_tests(&self, artifact: ArtifactM<Self>) {
        let sender = self.main_app_sender.clone();
        self.scope.spawn(move || match artifact.list_tests() {
            Ok(listing) => {
                let _ = sender.send(MainAppMessage::TestsListed { artifact, listing });
            }
            Err(error) => {
                let _ = sender.send(MainAppMessage::FatalError { error });
            }
        });
    }

    fn start_shutdown(&self) {
        let _ = self.main_app_sender.send(MainAppMessage::Shutdown);
    }

    fn get_test_layers(
        &self,
        artifact: &ArtifactM<Self>,
        metadata: &TestMetadata,
    ) -> Vec<LayerSpec> {
        self.deps
            .test_collector()
            .get_test_layers(artifact, metadata, &self.ui)
            .expect("XXX the python pip package creation needs to change")
    }

    fn send_ui_msg(&self, msg: UiMessage) {
        self.ui.send_raw(msg);
    }
}

fn main_app_channel_reader<DepsT: Deps>(
    app: &mut MainApp<DepsT>,
    main_app_receiver: Receiver<MainAppMessageM<DepsT>>,
) -> Result<ExitCode> {
    loop {
        let msg = main_app_receiver.recv()?;
        if matches!(msg, MainAppMessage::Shutdown) {
            break app.main_return_value();
        } else {
            app.receive_message(msg);
        }
    }
}

/// Run the given `[Ui]` implementation on a background thread, and run the main test-runner
/// application on this thread using the UI until it is completed.
pub fn run_app_with_ui_multithreaded<MainAppDepsT>(
    deps: MainAppCombinedDeps<MainAppDepsT>,
    logging_output: LoggingOutput,
    timeout_override: Option<Option<Timeout>>,
    ui: impl Ui,
) -> Result<ExitCode>
where
    MainAppDepsT: MainAppDeps,
{
    let (main_app_sender, main_app_receiver) = std::sync::mpsc::channel();
    let (ui_handle, ui_sender) = ui.start_ui_thread(logging_output, deps.log.clone());

    let test_metadata = &deps.test_metadata;
    let collector_options = &deps.collector_options;
    let test_db = deps.test_db_store.load()?;
    let abs_deps = &deps.abstract_deps;

    let exit_code = std::thread::scope(move |scope| {
        main_app_sender.send(MainAppMessage::Start).unwrap();
        let deps = MainAppDepsAdapter {
            deps: abs_deps,
            scope,
            main_app_sender,
            ui: ui_sender,
        };

        let mut app = MainApp::new(
            &deps,
            test_metadata,
            test_db,
            timeout_override,
            collector_options,
        );
        main_app_channel_reader(&mut app, main_app_receiver)
    })?;

    ui_handle.join()?;

    Ok(exit_code)
}
