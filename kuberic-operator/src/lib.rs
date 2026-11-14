//! Reusable reconciliation and an experimental single-instance namespace operator.
//! Native ownership and cleanup receipts are retained in memory; a new runtime
//! cannot adopt an old allocation.

mod admission;
mod crd;
pub mod deploy;
mod fm;
pub mod node;
mod plan;
mod process;
pub mod processes;
mod reconciler;
pub mod resources;
mod runtime;
mod settings;
mod teardown;

pub use admission::BackendAdmission;
pub use crd::{
    Allocation, Binding, BootstrapWait, ControllerPhase, FmAuthority, FrozenCohort, KubericSet,
    KubericSetSpec, KubericSetStatus, NodeAllocation, ServiceTemplate, Teardown, TeardownStage,
};
pub use fm::{FmApi, FmObservation, ProcessFm};
pub use plan::{evaluate, Observation, Plan};
pub use process::{CleanupReceipt, CleanupState, NodeCleanup, ProcessApi, ProcessObservation};
pub use reconciler::{ReconcileOutcome, Reconciler, StaleObservation};
pub use runtime::{Config, Operator};
pub use settings::Settings;
