//! Hosts application replicas with durable metadata, recovery, and fenced RPC sessions.

#![deny(clippy::disallowed_types)]

mod command;
mod error;
#[allow(clippy::disallowed_types)]
pub(crate) mod hosting;
mod operation;
mod operation_recovery;
#[allow(clippy::disallowed_types)]
mod process;
mod provisioning;
pub(crate) mod public_build_store;
#[allow(clippy::disallowed_types)]
pub(crate) mod public_lifecycle;
#[allow(clippy::disallowed_types)]
pub(crate) mod service;
pub(crate) mod session;
pub(crate) mod sqlite_store;
pub(crate) mod state;
pub(crate) mod store;
pub(crate) mod transport;

pub use error::{HostError, Result};

pub(crate) use process::PUBLIC_V1_PROCESS_STARTUP_OWNERS;
pub use process::{
    ApplicationStorageState, ReplicaBuildDiagnostics, ReplicaDiagnostics, ReplicaHandle,
    ReplicaHost, ReplicaProcessConfig, RunningReplica,
};
pub use transport::{KubernetesDnsResolver, ReplicaEndpointResolver};

#[cfg(test)]
mod tests {
    pub(crate) fn tempdir() -> std::io::Result<tempfile::TempDir> {
        tempfile::tempdir()
    }
}
