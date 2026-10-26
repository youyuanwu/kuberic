//! Immutable local endpoint facts for the classic V1 carrier.
//!
//! Transfer authorization and retirement belong to the engine session owners.

use serde::{Deserialize, Serialize};

use crate::protocol::types::{
    HistoryContext, ProcessSessionId, ReplicaIdentity, ReplicaStorageIncarnationContext,
};
use crate::{Result, RuntimeError};

use super::state::{AgentState, PublicOperationStage};

pub(crate) fn recovery_observations_pending(state: &AgentState) -> bool {
    state
        .public_v1_operation_state
        .as_ref()
        .is_some_and(|preview| {
            preview.data_loss_recovery.as_ref().is_some_and(|recovery| {
                preview.history.as_ref() != Some(&recovery.proposed_history)
            })
        })
}

pub(crate) fn transport_history<'a>(
    state: &'a AgentState,
    process: &ProcessSessionId,
) -> Result<&'a HistoryContext> {
    use crate::protocol::public_operations::PublicDataLossRecoveryStage;
    let preview = state
        .public_v1_operation_state
        .as_ref()
        .ok_or(RuntimeError::AuthorityNotAdmitted)?;
    let admitted = preview
        .history
        .as_ref()
        .ok_or(RuntimeError::AuthorityNotAdmitted)?;
    let Some(recovery) = &preview.data_loss_recovery else {
        return Ok(admitted);
    };
    if admitted == &recovery.proposed_history
        || !matches!(
            recovery.stage,
            PublicDataLossRecoveryStage::HistoryPending
                | PublicDataLossRecoveryStage::ConfigurationPending
        )
    {
        return Ok(admitted);
    }
    let record = preview
        .current_operation
        .as_ref()
        .and_then(|id| preview.operations.get(id))
        .ok_or(RuntimeError::AuthorityNotAdmitted)?;
    let input = record
        .intent
        .lifecycle
        .as_ref()
        .and_then(|input| input.data_loss_recovery.as_ref())
        .ok_or(RuntimeError::AuthorityNotAdmitted)?;
    if record.stage != PublicOperationStage::Running
        || record.superseded_by.is_some()
        || record.intent.process_session_id != *process
        || input.attempt != recovery.attempt
        || input.authorization != recovery.logical_authorization()
        || preview.data_loss_authorization.as_ref() != Some(&input.authorization)
        || admitted != &input.authorization.predecessor
        || preview.storage_incarnation.as_ref() != Some(&recovery.provisional_primary)
    {
        return Err(RuntimeError::AuthorityNotAdmitted);
    }
    Ok(&recovery.proposed_history)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeEndpoint {
    pub(crate) identity: ReplicaIdentity,
    pub(crate) process_session_id: ProcessSessionId,
    pub(crate) storage: ReplicaStorageIncarnationContext,
    pub(crate) address: String,
}

impl NativeEndpoint {
    pub(crate) fn validate(&self) -> Result<()> {
        self.storage
            .validate()
            .map_err(|error| RuntimeError::AuthorityMismatch(error.into()))?;
        if self.identity.replica_id.value() <= 0
            || self.identity.instance_id.is_empty()
            || self.identity.agent_generation.is_empty()
            || self.process_session_id.is_empty()
            || self.storage.replica != self.identity
            || self.address.is_empty()
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }
}
