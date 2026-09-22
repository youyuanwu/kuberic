//! Independent application and replication runtime for the level-triggered stack.

pub mod application;
pub mod authority;
pub mod effects;
pub mod engine;
pub mod replicator;
pub mod runtime;

mod error;

pub use application::{StateProvider, StatefulServiceReplica};
pub use error::{Result, RuntimeError};
pub use replicator::{
    DefaultReplicator, DefaultReplicatorFactory, ManagedReplicator, PartitionAccessView,
    PrimaryReplicator, Replicator, ReplicatorFactory, ReplicatorInterfaces, StateReplicator,
    StatefulServicePartition,
};
