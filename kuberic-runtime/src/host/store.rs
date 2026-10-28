//! Public V1 agent store operations.

use async_trait::async_trait;

use crate::host::Result;
use crate::host::state::{
    AgentState, PublicOperationDisposition, PublicOperationRecord, PublicOperationStage,
    StorageIdentity,
};
use crate::protocol::public_operations::{
    DataLossRenewalDecision, PublicOperationIntent, PublicV1SessionRenewalStatus,
};
use crate::protocol::types::{
    FaultType, HistoryContext, HistoryInitializationId, LoadMetric, OperationId,
    ReplicaStorageIncarnationContext,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BeginPublicOperation {
    Ready(PublicOperationRecord),
    Waiting(PublicOperationRecord),
    Pending(PublicOperationRecord),
    Completed(PublicOperationRecord),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenewPublicDataLossAttempt {
    pub(crate) decision: DataLossRenewalDecision,
    pub(crate) predecessor: PublicOperationRecord,
    pub(crate) successor: Option<PublicOperationRecord>,
}

#[async_trait]
pub(crate) trait AgentStore: Send + Sync {
    async fn select_native_input(
        &self,
        _intent: &PublicOperationIntent,
        _input: &crate::host::state::NativeActionInput,
    ) -> Result<()> {
        Err(crate::host::HostError::CommandRejected(
            "native lifecycle input selection unavailable".into(),
        ))
    }

    async fn validate_creation_reconstruction(
        &self,
        _goal: &crate::protocol::public_operations::ReplicaGoal,
    ) -> Result<()> {
        Err(crate::host::HostError::CommandRejected(
            "checked creation reconstruction is unavailable".into(),
        ))
    }

    async fn acknowledge_ordinary_goal(
        &self,
        _acknowledgement: &crate::protocol::public_operations::ReplicaGoalAcknowledgement,
    ) -> Result<()> {
        Err(crate::host::HostError::CommandRejected(
            "ordinary goal acknowledgement unavailable".into(),
        ))
    }
    async fn finish_original_member_rebuild(
        &self,
        _goal: &crate::protocol::public_operations::ReplicaGoal,
    ) -> Result<()> {
        Err(crate::host::HostError::CommandRejected(
            "original-member final settlement unavailable".into(),
        ))
    }
    async fn identity(&self) -> Result<StorageIdentity>;

    async fn load_state(&self) -> Result<AgentState>;

    async fn complete_application_initialization(&self) -> Result<()>;

    async fn record_partition_reports(
        &self,
        load_metrics: Vec<LoadMetric>,
        reported_fault: Option<FaultType>,
    ) -> Result<()>;

    async fn initialize_public_v1_history(
        &self,
        _initialization_id: &HistoryInitializationId,
        _history: &HistoryContext,
        _storage: &ReplicaStorageIncarnationContext,
    ) -> Result<()> {
        Err(crate::host::HostError::CommandRejected(
            "Public V1 history is not enabled for this store".into(),
        ))
    }

    async fn begin_public_operation(
        &self,
        _intent: &PublicOperationIntent,
        _blockers: &[OperationId],
        _superseded: &[OperationId],
    ) -> Result<BeginPublicOperation> {
        Err(crate::host::HostError::CommandRejected(
            "Public V1 operations are not enabled for this store".into(),
        ))
    }

    async fn renew_public_v1_data_loss_attempt(
        &self,
        _intent: &PublicOperationIntent,
    ) -> Result<RenewPublicDataLossAttempt> {
        Err(crate::host::HostError::CommandRejected(
            "Public V1 data-loss renewal is not enabled for this store".into(),
        ))
    }

    async fn persist_public_v1_session_renewal(
        &self,
        _expected: Option<&PublicV1SessionRenewalStatus>,
        _next: &PublicV1SessionRenewalStatus,
    ) -> Result<()> {
        Err(crate::host::HostError::CommandRejected(
            "Public V1 session renewal is not enabled for this store".into(),
        ))
    }

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
            "Public V1 operations are not enabled for this store".into(),
        ))
    }

    async fn record_public_retry_failure(
        &self,
        _intent: &PublicOperationIntent,
        _attempt: u64,
        _failure: &crate::host::state::PublicCallbackFailure,
    ) -> Result<PublicOperationRecord> {
        Err(crate::host::HostError::CommandRejected(
            "returned callback retry is not enabled for this store".into(),
        ))
    }

    async fn resume_public_operation(
        &self,
        _intent: &PublicOperationIntent,
        _attempt: u64,
    ) -> Result<PublicOperationRecord> {
        Err(crate::host::HostError::CommandRejected(
            "returned callback retry is not enabled for this store".into(),
        ))
    }

    async fn attach_public_operation(
        &self,
        _intent: &PublicOperationIntent,
        _owner: &OperationId,
    ) -> Result<PublicOperationRecord> {
        Err(crate::host::HostError::CommandRejected(
            "Public V1 operations are not enabled for this store".into(),
        ))
    }

    async fn public_operation_records(&self) -> Result<Vec<PublicOperationRecord>> {
        Err(crate::host::HostError::CommandRejected(
            "Public V1 operations are not enabled for this store".into(),
        ))
    }

    async fn public_instruction(
        &self,
        _intent: &PublicOperationIntent,
        _index: usize,
        _instruction: crate::host::state::PublicInstruction,
        _outcome: Option<crate::host::state::PublicInstructionOutcome>,
    ) -> Result<()> {
        Err(crate::host::HostError::CommandRejected(
            "Public V1 operations are not enabled for this store".into(),
        ))
    }
}
