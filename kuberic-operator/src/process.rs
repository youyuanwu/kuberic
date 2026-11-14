use crate::{Allocation, FrozenCohort};
use anyhow::{ensure, Result};
use async_trait::async_trait;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupReceipt {
    pub node_reaped: bool,
    pub node_exit_success: bool,
    pub application_reaped: bool,
    pub resources_joined: bool,
    pub forced: bool,
}

impl CleanupReceipt {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.node_reaped
                && self.node_exit_success
                && self.application_reaped
                && self.resources_joined
                && !self.forced,
            "original node/application cleanup is incomplete, failed or forced"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupState {
    Pending,
    Complete(CleanupReceipt),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeCleanup {
    pub pod_uid: String,
    pub outcome: CleanupState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessObservation {
    pub cohort: FrozenCohort,
    pub nodes: Vec<NodeCleanup>,
}

#[async_trait]
pub trait ProcessApi: Send + Sync {
    /// Reserve only before first allocation persistence and resource creation.
    async fn reserve(&self, allocation: &Allocation) -> Result<()>;
    async fn allocation(&self) -> Result<Option<Allocation>>;

    /// Retain exact original-child receipts through finalizer completion.
    /// Pod disappearance and native/channel completion are not cleanup receipts.
    async fn observe(&self, cohort: &FrozenCohort) -> Result<Option<ProcessObservation>>;
}
