//! Namespace-bound bootstrap reconciliation, qualified with mock Kubernetes
//! objects and local processes. Not a Kubernetes deployment or recovery tier.

mod crd;
mod fm;
mod plan;
mod reconciler;
pub mod resources;

pub use crd::{
    Allocation, Binding, ControllerPhase, FmAuthority, FrozenCohort, KubericSet, KubericSetSpec,
    KubericSetStatus, NodeAllocation, ServiceTemplate, Settings,
};
pub use fm::{FmApi, FmObservation, ProcessFm};
pub use plan::{evaluate, Observation, Plan};
pub use reconciler::{ReconcileOutcome, Reconciler};
