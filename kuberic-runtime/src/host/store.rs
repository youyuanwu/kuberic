//! Narrow agent store operations.

use crate::effects::{RuntimeEffect, RuntimeEffectResult};
use crate::protocol::command::{EnsureConfiguration, EnsureReplicaBuild};
#[cfg(any(test, feature = "testing"))]
use crate::protocol::public_operations::PublicOperationIntent;
#[cfg(all(feature = "testing", kuberic_workspace_tests))]
use crate::protocol::public_operations::{
    PublicFaultAction, RestartActionRecord, RestartActionStage,
};
use crate::protocol::types::{FaultType, LoadMetric, OperationId};
use async_trait::async_trait;

use crate::authority::AdmittedAuthority;
use crate::host::Result;
#[cfg(test)]
use crate::host::state::RetainedResult;
use crate::host::state::{
    AgentState, CoordinatorStage, ReconfigurationRecord, RetainedCommandResult, StorageIdentity,
};
#[cfg(any(test, feature = "testing"))]
use crate::host::state::{PublicOperationDisposition, PublicOperationRecord, PublicOperationStage};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BeginEffect {
    Execute(RuntimeEffect),
    Pending(RuntimeEffect),
    Applied {
        effect: RuntimeEffect,
        result: Box<RuntimeEffectResult>,
    },
    Completed(Box<RuntimeEffectResult>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BeginConfiguration {
    Execute(ReconfigurationRecord),
    Pending(ReconfigurationRecord),
    Superseded(ReconfigurationRecord),
    Completed(RetainedCommandResult),
}

#[cfg(any(test, feature = "testing"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BeginPublicOperation {
    Ready(PublicOperationRecord),
    Waiting(PublicOperationRecord),
    Pending(PublicOperationRecord),
    Completed(PublicOperationRecord),
}

#[async_trait]
pub(crate) trait AgentStore: Send + Sync {
    async fn identity(&self) -> Result<StorageIdentity>;

    async fn load_state(&self) -> Result<AgentState>;

    async fn load_admitted_authority(&self) -> Result<Option<AdmittedAuthority>>;

    async fn complete_application_initialization(&self) -> Result<()>;

    async fn begin_effect(&self, effect: &RuntimeEffect) -> Result<BeginEffect>;

    async fn mark_effect_applied(
        &self,
        effect: &RuntimeEffect,
        result: &RuntimeEffectResult,
    ) -> Result<()>;

    async fn complete_effect(&self, result: &RuntimeEffectResult) -> Result<()>;

    async fn cancel_effect(&self, effect: &RuntimeEffect) -> Result<()>;

    async fn begin_configuration(
        &self,
        command: &EnsureConfiguration,
    ) -> Result<BeginConfiguration>;

    async fn journal_build(&self, command: &EnsureReplicaBuild) -> Result<EnsureReplicaBuild>;

    async fn abandon_build(&self, command: &EnsureReplicaBuild) -> Result<()>;

    async fn advance_configuration(
        &self,
        operation_id: &OperationId,
        expected: CoordinatorStage,
        next: CoordinatorStage,
        observed_lsn: Option<i64>,
    ) -> Result<ReconfigurationRecord>;

    async fn complete_configuration(
        &self,
        operation_id: &OperationId,
    ) -> Result<RetainedCommandResult>;

    #[cfg(test)]
    async fn retained_result(&self) -> Result<Option<RetainedResult>>;

    #[cfg(test)]
    async fn set_reconfiguration(&self, data: Option<String>) -> Result<()>;

    #[cfg(test)]
    async fn clear_reconfiguration(&self) -> Result<()>;

    #[cfg(test)]
    async fn migrate_schema(&self, expected_version: u32, target_version: u32) -> Result<()>;

    async fn record_partition_reports(
        &self,
        load_metrics: Vec<LoadMetric>,
        reported_fault: Option<FaultType>,
    ) -> Result<()>;

    #[cfg(any(test, feature = "testing"))]
    async fn begin_public_operation(
        &self,
        _intent: &PublicOperationIntent,
        _blockers: &[OperationId],
        _superseded: &[OperationId],
    ) -> Result<BeginPublicOperation> {
        Err(crate::host::HostError::CommandRejected(
            "public-operation preview is not enabled for this store".into(),
        ))
    }

    #[cfg(any(test, feature = "testing"))]
    async fn advance_public_operation(
        &self,
        _operation_id: &OperationId,
        _expected_revision: u64,
        _expected_process_session: &crate::protocol::types::ProcessSessionId,
        _expected: PublicOperationStage,
        _next: PublicOperationStage,
        _disposition: Option<PublicOperationDisposition>,
    ) -> Result<PublicOperationRecord> {
        Err(crate::host::HostError::CommandRejected(
            "public-operation preview is not enabled for this store".into(),
        ))
    }

    #[cfg(any(test, feature = "testing"))]
    async fn attach_public_operation(
        &self,
        _intent: &PublicOperationIntent,
        _owner: &OperationId,
    ) -> Result<PublicOperationRecord> {
        Err(crate::host::HostError::CommandRejected(
            "public-operation preview is not enabled for this store".into(),
        ))
    }

    #[cfg(any(test, feature = "testing"))]
    async fn public_operation_records(&self) -> Result<Vec<PublicOperationRecord>> {
        Err(crate::host::HostError::CommandRejected(
            "public-operation preview is not enabled for this store".into(),
        ))
    }

    #[cfg(any(test, feature = "testing"))]
    async fn public_instruction(
        &self,
        _intent: &PublicOperationIntent,
        _index: usize,
        _instruction: crate::host::state::PublicInstruction,
        _outcome: Option<crate::host::state::PublicInstructionOutcome>,
    ) -> Result<()> {
        Err(crate::host::HostError::CommandRejected(
            "public-operation preview is not enabled for this store".into(),
        ))
    }

    #[cfg(all(feature = "testing", kuberic_workspace_tests))]
    #[allow(dead_code)]
    async fn begin_restart_action(
        &self,
        _action: &PublicFaultAction,
    ) -> Result<RestartActionRecord> {
        Err(crate::host::HostError::CommandRejected(
            "public-operation preview is not enabled for this store".into(),
        ))
    }

    #[cfg(all(feature = "testing", kuberic_workspace_tests))]
    #[allow(dead_code)]
    async fn advance_restart_action(
        &self,
        _action: &PublicFaultAction,
        _expected: RestartActionStage,
        _next: RestartActionStage,
        _successor_session: Option<&crate::protocol::types::ProcessSessionId>,
    ) -> Result<RestartActionRecord> {
        Err(crate::host::HostError::CommandRejected(
            "public-operation preview is not enabled for this store".into(),
        ))
    }

    #[cfg(all(feature = "testing", kuberic_workspace_tests))]
    #[allow(dead_code)]
    async fn restart_action(&self) -> Result<Option<RestartActionRecord>> {
        Ok(None)
    }
}
