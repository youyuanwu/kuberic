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
#[allow(clippy::disallowed_types)]
pub(crate) mod v1_lifecycle;

pub use error::{HostError, Result};

#[cfg(kuberic_workspace_tests)]
pub(crate) use process::PUBLIC_V1_PROCESS_STARTUP_OWNERS;
pub use process::{
    ApplicationStorageState, ReplicaBuildDiagnostics, ReplicaDiagnostics, ReplicaHandle,
    ReplicaHost, ReplicaProcessConfig, RunningReplica,
};
pub use transport::{KubernetesDnsResolver, ReplicaEndpointResolver};

pub(crate) fn session_renewal_supersedes_local_journal(
    next: &crate::protocol::public_operations::PublicV1SessionRenewalStatus,
    previous: &crate::protocol::public_operations::PublicV1SessionRenewalStatus,
) -> bool {
    next.required != previous.required
        && next.required.resource_uid == previous.required.resource_uid
        && next.required.history.resource_uid == previous.required.history.resource_uid
        && next.required.history.protocol_generation
            == previous.required.history.protocol_generation
        && next.required.history.data_loss_number >= previous.required.history.data_loss_number
        && next.required.previous_epoch >= previous.required.renewal_epoch
}

#[cfg(test)]
mod tests {
    pub(crate) fn tempdir() -> std::io::Result<tempfile::TempDir> {
        tempfile::tempdir()
    }
}
