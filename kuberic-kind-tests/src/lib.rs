//! Isolated KinD qualification, not a production operator or restart-recovery backend.
#![forbid(unsafe_code)]
#![cfg(target_os = "linux")]

mod deployed;
pub mod driver;
pub mod host;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    Bootstrap,
    DeleteDuringBootstrap,
    StaggeredShutdown,
    PrimaryAppFailover,
}

impl Scenario {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bootstrap => "bootstrap",
            Self::DeleteDuringBootstrap => "delete-during-bootstrap",
            Self::StaggeredShutdown => "staggered-shutdown",
            Self::PrimaryAppFailover => "primary-app-failover",
        }
    }
}
