//! A fresh, fixed-topology external FM. No reconnect, recovery or failover.

mod config;
mod runtime;
mod state;

pub use config::{Config, ServiceConfig};
pub use fube_node_runner::{Fm, Identity, Placement};
pub use runtime::run;
pub use state::{Outbound, Phase, State, Status};
