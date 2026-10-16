//! Independent application and replication runtime for the level-triggered stack.

#![deny(private_bounds, private_interfaces)]
#![cfg_attr(not(test), deny(unreachable_pub))]
#![allow(clippy::disallowed_types)]

#[cfg(feature = "host")]
pub mod application;
#[cfg(feature = "host")]
mod capabilities;
pub mod control;
#[cfg(feature = "host")]
pub mod engine;
#[cfg(feature = "host")]
/// Replica process hosting and peer transport, enabled by the `host` feature.
pub mod host;
pub mod protocol;
#[cfg(feature = "host")]
pub mod replicator;

#[cfg(feature = "testing")]
/// Isolated host fixtures and deterministic transports; no production capabilities.
pub mod testing;

#[cfg(feature = "host")]
mod error;

#[cfg(feature = "host")]
pub use application::{StateProvider, StatefulServiceReplica};
#[cfg(feature = "host")]
pub use error::{Result, RuntimeError};
#[cfg(feature = "host")]
pub use replicator::{
    DefaultReplicatorFactory, PrimaryReplicator, Replicator, ReplicatorFactory,
    ReplicatorInterfaces, StateReplicator, StatefulServicePartition,
};
