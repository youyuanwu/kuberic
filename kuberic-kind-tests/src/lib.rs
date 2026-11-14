//! Isolated KinD qualification, not a production operator or restart-recovery backend.
#![forbid(unsafe_code)]
#![cfg(target_os = "linux")]

pub mod driver;
pub mod host;
mod node;
mod processes;

pub use node::run as run_node;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    Bootstrap,
    DeleteDuringBootstrap,
    StaggeredShutdown,
}

impl Scenario {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bootstrap => "bootstrap",
            Self::DeleteDuringBootstrap => "delete-during-bootstrap",
            Self::StaggeredShutdown => "staggered-shutdown",
        }
    }
}
