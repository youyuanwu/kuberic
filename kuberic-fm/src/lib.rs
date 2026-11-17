//! A volatile, fixed-topology external FM with native repair and planned handoff.

mod config;
mod runtime;
mod state;

pub use config::{Config, ServiceConfig};
pub use fube_node_runner::{Fm, Identity, Placement};
pub use runtime::{run, run_guarded, SwitchoverRequest};
pub use state::{Outbound, Phase, State, Status, Switchover};
