//! Isolated KinD qualification, not a production operator or restart-recovery backend.
#![forbid(unsafe_code)]
#![cfg(target_os = "linux")]

pub mod driver;
pub mod host;
mod node;
mod processes;
mod shutdown;

pub use node::run as run_node;
