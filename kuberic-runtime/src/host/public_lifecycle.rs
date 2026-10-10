//! Repository-only lifecycle execution. Legacy authority and access engines are
//! deliberately not used: their configuration recipe has different ordering.

use std::sync::Arc;

use crate::application::StatefulServiceReplica;
use crate::host::operation::{CallbackContainment, PartitionOperation, PartitionOperationRegistry};
use crate::host::state::{
    DataLossOutcome, PublicInstruction, PublicInstructionOutcome, PublicOperationDisposition,
    PublicOperationRecord, PublicOperationStage,
};
use crate::host::store::AgentStore;
use crate::host::{HostError, Result};
use crate::protocol::public_operations::{
    PossibleDataLossIntent, PublicLifecycleInput, PublicLifecycleRecipe, PublicLifecycleReport,
    PublicOperationIntent, PublicOperationPreviewIdentity, ServiceLocation,
};
use crate::protocol::types::{ProcessSessionId, ReplicaRole};
use crate::replicator::{PrimaryReplicator, ReplicaSetQuorumMode, Replicator};

pub(crate) struct PublicLifecycleCallbacks {
    pub(crate) application: Arc<dyn StatefulServiceReplica>,
    pub(crate) replicator: Arc<dyn Replicator>,
    pub(crate) primary: Arc<dyn PrimaryReplicator>,
}

pub(crate) fn instructions(input: &PublicLifecycleInput) -> Vec<PublicInstruction> {
    use PublicInstruction::*;
    if input.recipe == PublicLifecycleRecipe::SecondaryEpochAdvance {
        return vec![Epoch];
    }
    let mut result = vec![ReplicatorPrimary];
    if input.recipe == PublicLifecycleRecipe::FailoverPromotion {
        result.push(Epoch);
    }
    result.push(ApplicationPrimary);
    if input.possible_data_loss == PossibleDataLossIntent::Possible {
        result.push(DataLoss);
        return result;
    }
    match input.recipe {
        PublicLifecycleRecipe::InitialPrimary => result.push(CurrentConfiguration),
        PublicLifecycleRecipe::FailoverPromotion => {
            result.extend([CatchUpConfiguration, CatchUp]);
        }
        PublicLifecycleRecipe::SecondaryEpochAdvance => unreachable!(),
    }
    result.push(Access);
    result
}

pub(crate) async fn launch(
    registry: &Arc<PartitionOperationRegistry>,
    store: Arc<dyn AgentStore>,
    intent: PublicOperationIntent,
    callbacks: PublicLifecycleCallbacks,
) -> Result<Arc<PartitionOperation>> {
    let input = intent
        .lifecycle
        .clone()
        .ok_or_else(|| HostError::CommandRejected("preview lifecycle input required".into()))?;
    let operation = registry.admit(intent.clone()).await?;
    if operation.snapshot().stage == PublicOperationStage::Ready {
        operation
            .spawn_root(CallbackContainment::ObjectOwnedOnInterruption, async move {
                execute(store, intent, input, callbacks).await
            })
            .await?;
    }
    Ok(operation)
}

async fn execute(
    store: Arc<dyn AgentStore>,
    intent: PublicOperationIntent,
    input: PublicLifecycleInput,
    callbacks: PublicLifecycleCallbacks,
) -> Result<()> {
    use PublicInstruction::*;
    for (index, instruction) in instructions(&input).into_iter().enumerate() {
        store
            .public_instruction(&intent, index, instruction, None)
            .await?;
        let outcome = match instruction {
            ReplicatorPrimary => {
                callbacks
                    .replicator
                    .change_role(input.epoch, ReplicaRole::Primary)
                    .await?;
                PublicInstructionOutcome::Done
            }
            Epoch => {
                callbacks.replicator.update_epoch(input.epoch).await?;
                PublicInstructionOutcome::Done
            }
            ApplicationPrimary => {
                let role = callbacks
                    .application
                    .change_role(ReplicaRole::Primary)
                    .await?;
                PublicInstructionOutcome::ApplicationRole(role.service_address)
            }
            DataLoss => {
                let outcome = match callbacks.primary.on_data_loss().await {
                    Ok(false) => DataLossOutcome::False,
                    Ok(true) => DataLossOutcome::True,
                    Err(error) => DataLossOutcome::Error(error.to_string()),
                };
                PublicInstructionOutcome::DataLoss(outcome)
            }
            CurrentConfiguration => {
                callbacks
                    .primary
                    .update_current_replica_set_configuration(input.current.clone().into())
                    .await?;
                PublicInstructionOutcome::Done
            }
            CatchUpConfiguration => {
                callbacks
                    .primary
                    .update_catch_up_replica_set_configuration(
                        input.current.clone().into(),
                        input
                            .previous
                            .clone()
                            .expect("validated failover previous configuration")
                            .into(),
                    )
                    .await?;
                PublicInstructionOutcome::Done
            }
            CatchUp => {
                callbacks
                    .primary
                    .wait_for_catch_up_quorum(ReplicaSetQuorumMode::WriteQuorum)
                    .await?;
                PublicInstructionOutcome::Done
            }
            Access => PublicInstructionOutcome::Done,
        };
        store
            .public_instruction(&intent, index, instruction, Some(outcome.clone()))
            .await?;
        if let PublicInstructionOutcome::DataLoss(DataLossOutcome::Error(error)) = outcome {
            return Err(HostError::CommandRejected(error));
        }
    }
    Ok(())
}

pub(crate) async fn report(
    store: &dyn AgentStore,
    identity: &PublicOperationPreviewIdentity,
    session: &ProcessSessionId,
) -> Result<PublicLifecycleReport> {
    let state = store.load_state().await?;
    let preview = state.public_operation_preview.ok_or_else(|| {
        HostError::CommandRejected("preview report requires preview store".into())
    })?;
    if &preview.identity != identity {
        return Err(HostError::IdentityMismatch(
            "preview report identity changed".into(),
        ));
    }
    let record = preview
        .current_operation
        .as_ref()
        .and_then(|id| preview.operations.get(id));
    let mut report = PublicLifecycleReport {
        preview: identity.clone(),
        resource_uid: state.identity.resource_uid.clone(),
        replica: state.identity.local_identity,
        process_session_id: session.clone(),
        revision: preview
            .operations
            .values()
            .map(|record| record.intent.revision)
            .max()
            .unwrap_or(0),
        operation_id: record.map(|record| record.intent.operation_id.clone()),
        role: ReplicaRole::None,
        write_access: false,
        service_location: None,
    };
    let Some(record) = record else {
        return Ok(report);
    };
    let Some(input) = &record.intent.lifecycle else {
        return Ok(report);
    };
    if &record.intent.process_session_id != session
        || record.stage != PublicOperationStage::Completed
        || record.superseded_by.is_some()
        || record.disposition != Some(PublicOperationDisposition::Succeeded)
        || !preview.history_barriers.is_empty()
        || !complete(record)
    {
        return Ok(report);
    }
    report.revision = record.intent.revision;
    report.role = if input.recipe == PublicLifecycleRecipe::SecondaryEpochAdvance {
        ReplicaRole::ActiveSecondary
    } else {
        ReplicaRole::Primary
    };
    report.write_access = report.role == ReplicaRole::Primary;
    if report.write_access {
        report.service_location = record.lifecycle.outcomes.iter().find_map(|outcome| {
            if let PublicInstructionOutcome::ApplicationRole(Some(address)) = outcome {
                Some(ServiceLocation {
                    preview: identity.clone(),
                    resource_uid: state.identity.resource_uid.clone(),
                    operation_id: record.intent.operation_id.clone(),
                    replica: report.replica.clone(),
                    process_session_id: session.clone(),
                    epoch: input.epoch,
                    revision: record.intent.revision,
                    address: address.clone(),
                })
            } else {
                None
            }
        });
    }
    Ok(report)
}

pub(crate) fn complete(record: &PublicOperationRecord) -> bool {
    record.intent.lifecycle.as_ref().is_none_or(|input| {
        record.lifecycle.in_flight.is_none()
            && record.lifecycle.outcomes.len() == instructions(input).len()
    })
}
