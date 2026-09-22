//! Independent application and replication runtime for the level-triggered stack.

pub mod application;
pub mod authority;
pub mod effects;
pub mod replicator;
pub mod runtime;

mod error;

pub use error::{Result, RuntimeError};
