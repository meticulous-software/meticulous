mod proto_buf_conv;
pub mod spec;

pub mod proto {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/maelstrom_client_base.items.rs"));
}

// This hack makes some macros in maelstrom_test work correctly
#[cfg(test)]
extern crate self as maelstrom_client;

pub use proto_buf_conv::{IntoProtoBuf, TryFromProtoBuf};

use derive_more::{From, Into};
use enum_map::EnumMap;
use maelstrom_base::{ClientJobId, JobBrokerStatus, JobOutcomeResult, JobWorkerStatus};
use maelstrom_container::ContainerImageDepotDir;
use maelstrom_macro::{IntoProtoBuf, TryFromProtoBuf};
use maelstrom_util::{
    config::common::{BrokerAddr, CacheSize, InlineLimit, Slots},
    root::RootBuf,
};
use serde::Deserialize;
use std::fmt;

/// The project directory is used for two things. First, any relative paths in layer specifications
/// are resolved based on this path. Second, it's where the client process looks for the
/// `maelstrom-container-tags.lock` file.
pub struct ProjectDir;

/// According to the XDG base directories spec:
///
///   The state directory contains state data that should persist between (application) restarts,
///   but that is not important or portable enough to the user that it should be stored in
///   $XDG_DATA_HOME. It may contain:
///     - actions history (logs, history, recently used files, ...)
///     - current state of the application that can be reused on a restart (view, layout, open
///       files, undo history, ...)
///
/// For the client process, that currently just means the log files.
pub struct StateDir;

/// The cache directory is where we put a variety of different caches. The local worker's cache
/// directory lives inside of this client cache directory. Another cache in this directory is the
/// manifest cache.
pub struct CacheDir;

pub const MANIFEST_DIR: &str = "manifests";
pub const STUB_MANIFEST_DIR: &str = "manifests/stubs";
pub const SYMLINK_MANIFEST_DIR: &str = "manifests/symlinks";
pub const SO_LISTINGS_DIR: &str = "so_listings";

impl From<proto::Error> for anyhow::Error {
    fn from(e: proto::Error) -> Self {
        anyhow::Error::msg(e.message)
    }
}

#[derive(IntoProtoBuf, TryFromProtoBuf)]
#[proto(proto_buf_type = "proto::RemoteProgress")]
pub struct RemoteProgress {
    pub name: String,
    pub size: u64,
    pub progress: u64,
}

#[derive(IntoProtoBuf, TryFromProtoBuf)]
#[proto(proto_buf_type = "proto::IntrospectResponse")]
pub struct IntrospectResponse {
    #[proto(option)]
    pub job_state_counts: EnumMap<maelstrom_base::stats::JobState, u64>,
    pub artifact_uploads: Vec<RemoteProgress>,
    pub image_downloads: Vec<RemoteProgress>,
}

#[derive(Clone, Deserialize, From, Into, TryFromProtoBuf, IntoProtoBuf)]
#[proto(proto_buf_type = bool, try_from_into)]
#[serde(transparent)]
pub struct AcceptInvalidRemoteContainerTlsCerts(bool);

impl AcceptInvalidRemoteContainerTlsCerts {
    pub fn into_inner(self) -> bool {
        self.0
    }
}

impl fmt::Debug for AcceptInvalidRemoteContainerTlsCerts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, IntoProtoBuf, TryFromProtoBuf)]
#[proto(proto_buf_type = "proto::LogKeyValue")]
pub struct RpcLogKeyValue {
    pub key: String,
    pub value: String,
}

#[derive(Clone, IntoProtoBuf, TryFromProtoBuf)]
#[proto(proto_buf_type = "proto::LogMessage")]
pub struct RpcLogMessage {
    pub message: String,
    pub level: slog::Level,
    pub tag: String,
    pub key_values: Vec<RpcLogKeyValue>,
}

impl RpcLogMessage {
    pub fn log_to(self, log: &slog::Logger) {
        let location = slog::RecordLocation {
            file: "<remote-file>",
            line: 0,
            column: 0,
            function: "",
            module: "<remote-module>",
        };
        let rs = slog::RecordStatic {
            location: &location,
            level: self.level,
            tag: &self.tag,
        };
        let kv = SimpleKV(
            self.key_values
                .into_iter()
                .map(|e| slog::SingleKV::from((e.key, e.value)))
                .collect(),
        );
        log.log(&slog::Record::new(
            &rs,
            &format_args!("[client-process] {}", self.message),
            slog::BorrowedKV(&kv),
        ));
    }
}

struct SimpleKV(Vec<slog::SingleKV<String>>);

impl slog::KV for SimpleKV {
    fn serialize(
        &self,
        record: &slog::Record,
        serializer: &mut dyn slog::Serializer,
    ) -> slog::Result {
        for e in &self.0 {
            e.serialize(record, serializer)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, IntoProtoBuf, TryFromProtoBuf)]
#[proto(
    proto_buf_type = "proto::JobRunningStatus",
    enum_type = "proto::job_running_status::Status"
)]
pub enum JobRunningStatus {
    AtBroker(JobBrokerStatus),
    AtLocalWorker(JobWorkerStatus),
}

#[derive(Clone, Debug, From, PartialEq, Eq, PartialOrd, Ord, IntoProtoBuf, TryFromProtoBuf)]
#[proto(
    proto_buf_type = "proto::JobStatus",
    enum_type = "proto::job_status::Status"
)]
pub enum JobStatus {
    Running(JobRunningStatus),
    #[proto(proto_buf_type = "proto::JobCompletedStatus")]
    Completed {
        client_job_id: ClientJobId,
        #[proto(option)]
        result: JobOutcomeResult,
    },
}

//                                 _      __
//  _ __ ___  __ _ _   _  ___  ___| |_   / / __ ___  ___ _ __   ___  _ __  ___  ___
// | '__/ _ \/ _` | | | |/ _ \/ __| __| / / '__/ _ \/ __| '_ \ / _ \| '_ \/ __|/ _ \
// | | |  __/ (_| | |_| |  __/\__ \ |_ / /| | |  __/\__ \ |_) | (_) | | | \__ \  __/
// |_|  \___|\__, |\__,_|\___||___/\__/_/ |_|  \___||___/ .__/ \___/|_| |_|___/\___|
//              |_|                                     |_|

#[derive(Debug, IntoProtoBuf, TryFromProtoBuf)]
#[proto(proto_buf_type = "proto::StartRequest")]
pub struct StartRequest {
    pub broker_addr: Option<BrokerAddr>,
    pub project_dir: RootBuf<ProjectDir>,
    pub state_dir: RootBuf<StateDir>,
    pub container_image_depot_dir: RootBuf<ContainerImageDepotDir>,
    pub cache_dir: RootBuf<CacheDir>,
    pub cache_size: CacheSize,
    pub inline_limit: InlineLimit,
    pub slots: Slots,
    pub accept_invalid_remote_container_tls_certs: AcceptInvalidRemoteContainerTlsCerts,
}

#[derive(IntoProtoBuf, TryFromProtoBuf)]
#[proto(proto_buf_type = "proto::RunJobRequest")]
pub struct RunJobRequest {
    #[proto(option)]
    pub spec: spec::JobSpec,
}

#[derive(IntoProtoBuf, TryFromProtoBuf)]
#[proto(proto_buf_type = "proto::AddContainerRequest")]
pub struct AddContainerRequest {
    pub name: String,
    #[proto(option)]
    pub container: spec::ContainerSpec,
}
