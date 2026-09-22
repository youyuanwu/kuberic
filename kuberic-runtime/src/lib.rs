//! Independent application and replication runtime for the level-triggered stack.

pub mod application;
#[doc(hidden)]
pub mod authority;
#[doc(hidden)]
pub mod effects;
pub mod engine;
pub mod replicator;
#[doc(hidden)]
pub mod runtime;

mod error;

pub use application::{StateProvider, StatefulServiceReplica};
pub use error::{Result, RuntimeError};
pub use replicator::{
    DefaultReplicator, DefaultReplicatorFactory, PrimaryReplicator, Replicator, ReplicatorFactory,
    ReplicatorInterfaces, StateReplicator, StatefulServicePartition,
};
