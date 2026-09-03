pub mod add_replica;
pub mod driver;
pub mod error;
pub mod events;
pub mod grpc;
pub mod handles;
pub mod noop;
pub mod pod;
pub mod remove_replica;
pub mod replica_agent;
pub mod replica_lifecycle;
pub mod replicator;
pub mod runtime;
pub mod types;

pub mod proto {
    tonic::include_proto!("kuberic.replication.v1");
}

pub use error::{KubericError, RecoveryError, Result};
