//! Namespace-bound bootstrap and owned teardown, qualified with mock Kubernetes
//! objects and local processes. Not a Kubernetes deployment or recovery tier.

mod admission;
mod crd;
mod fm;
mod plan;
mod process;
mod reconciler;
pub mod resources;
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
pub use reconciler::{ReconcileOutcome, Reconciler};
pub use settings::Settings;
