pub mod driver;
pub mod error;
pub mod events;
pub mod grpc;
pub mod handles;
pub mod noop;
pub mod pod;
pub mod replicator;
pub mod runtime;
pub mod types;

pub mod proto {
    tonic::include_proto!("kuberic.replication.v1");
}

pub use error::{KubericError, Result};
