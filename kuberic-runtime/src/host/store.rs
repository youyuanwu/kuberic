//! Public V1 agent store operations.

use async_trait::async_trait;

use crate::host::Result;
use crate::host::state::{
    AgentState, PublicOperationDisposition, PublicOperationRecord, PublicOperationStage,
    StorageIdentity,
};
use crate::protocol::public_operations::{
    DataLossRenewalDecision, PublicInboundBuildAuthority, PublicInboundBuildReceipt,
    PublicOperationIntent, PublicV1SessionRenewalStatus,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicInboundBuildAdmission {
    Current,
    Successor,
}

#[async_trait]
pub(crate) trait AgentStore: Send + Sync {
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

    async fn classify_public_v1_inbound_build_authority(
        &self,
        _authority: &PublicInboundBuildAuthority,
    ) -> Result<PublicInboundBuildAdmission> {
        Err(crate::host::HostError::CommandRejected(
            "Public V1 inbound build authority is not enabled for this store".into(),
        ))
    }

    async fn install_public_v1_inbound_build_authority(
        &self,
        _authority: &PublicInboundBuildAuthority,
    ) -> Result<()> {
        Err(crate::host::HostError::CommandRejected(
            "Public V1 inbound build authority is not enabled for this store".into(),
        ))
    }

    async fn public_v1_inbound_build_authority(
        &self,
        _build_id: &OperationId,
    ) -> Result<Option<PublicInboundBuildAuthority>> {
        Ok(None)
    }

    async fn public_v1_terminal_inbound_build_receipt(
        &self,
        _build_id: &OperationId,
    ) -> Result<Option<PublicInboundBuildReceipt>> {
        Ok(None)
    }

    async fn revoke_public_v1_inbound_build_authority(&self) -> Result<()> {
        Err(crate::host::HostError::CommandRejected(
            "Public V1 inbound build authority is not enabled for this store".into(),
        ))
    }

    async fn public_v1_outbound_build_authority(
        &self,
        _build_id: &OperationId,
        _current_configuration_id: &crate::protocol::types::ConfigurationId,
    ) -> Result<Option<PublicInboundBuildAuthority>> {
        Ok(None)
    }

    async fn complete_public_v1_inbound_build(
        &self,
        _receipt: &PublicInboundBuildReceipt,
    ) -> Result<()> {
        Err(crate::host::HostError::CommandRejected(
            "Public V1 inbound build authority is not enabled for this store".into(),
        ))
    }

    async fn public_v1_storage_is_retired(
        &self,
        _storage: &ReplicaStorageIncarnationContext,
    ) -> Result<bool> {
        Ok(false)
    }
}
