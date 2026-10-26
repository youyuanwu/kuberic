//! Controller-driven, cluster-free application testing. Construct an inert
//! application with [`ApplicationFactory`], drive production reconciliation,
//! and explicitly [`TestCluster::finish`] to acknowledge asynchronous cleanup.
//!
//! A cluster has one explicit reconciliation driver. [`TestCluster::drive`]
//! runs an application future alongside it; [`TestCluster::probe`] does not
//! reconcile and is usable while a typed [`Pause`] is retained. All scenario
//! operations share one deadline. A timed-out mutation is not rolled back.
//!
//! [`ReplicaRef`] and [`ResourceRef`] are exact, cluster-scoped observations.
//! Old service `Arc`s remain old services, so applications can assert genuine
//! fencing. TCP faults require actual built-in engine integration, not a factory
//! declaration. Successful finalization acknowledges task/listener drain; errors
//! and early Drop retain local storage and do not certify cleanup.
mod application;
mod cluster;
mod error;
mod faults;
mod kube;
mod pod;
#[cfg(test)]
mod tests;
mod transport;

pub use application::{ApplicationFactory, CreatedApplication, ReplicaContext};
pub use cluster::{
    ClusterBuilder, Observation, ObservationFailure, ObservationFailureKind, ReplicaRef,
    ResourceKind, ResourceObservation, ResourceRef, SupportResource, TestCluster, Topology,
};
pub use error::{ErrorKind, Result, TestError};
pub use faults::{
    Barrier, BarrierStage, CompletionConflict, DeletionHold, OperationEvidence, Pause,
    SwitchoverRequest,
};
pub use transport::Isolation;
