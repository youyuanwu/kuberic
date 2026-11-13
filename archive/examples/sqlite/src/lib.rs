//! Replicated SQLite with service-owned WAL commitment and publication.

mod commit_barrier;
mod connection;
pub mod framelog;
pub mod frames;
pub mod server;
pub mod service;
pub mod state;
mod v1;
pub use state::{RecoveryState, SqlitePersistence};

#[cfg(test)]
mod persistence_tests;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[doc(hidden)]
#[cfg(feature = "testing")]
pub mod v1_test_bridge {
    pub use crate::v1::{Access, Admission, Assignment, Committer, FailureCut, Write};
}

pub mod proto {
    tonic::include_proto!("sqlitestore.v1");
}
