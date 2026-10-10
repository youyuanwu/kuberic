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

#[path = "public_lifecycle_program.rs"]
mod program;
#[allow(unused_imports)]
pub(crate) use program::{abort, operation_instructions, repeatable, replayable};

#[derive(Clone)]
pub(crate) struct PublicLifecycleCallbacks {
    pub(crate) application: Arc<dyn StatefulServiceReplica>,
    pub(crate) replicator: Arc<dyn Replicator>,
    pub(crate) primary: Arc<dyn PrimaryReplicator>,
    pub(crate) open_context: Option<crate::application::OpenContext>,
    pub(crate) containment: Option<tokio::sync::watch::Receiver<bool>>,
    pub(crate) aborted: Arc<std::sync::Mutex<bool>>,
    #[cfg(all(test, feature = "testing"))]
    pub(crate) cut: Option<Arc<PublicOperationCut>>,
}

#[cfg(all(test, feature = "testing"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicCutPosition {
    BeforeInstruction,
    CallbackSuccess,
    InstructionApplied,
    CallbackApplied,
    Completed,
}

#[cfg(all(test, feature = "testing"))]
pub(crate) struct PublicOperationCut {
    pub(crate) position: PublicCutPosition,
    pub(crate) index: usize,
    pub(crate) entered: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
}

#[cfg(all(test, feature = "testing"))]
impl PublicOperationCut {
    pub(crate) async fn pause(&self, position: PublicCutPosition, index: usize) {
        if self.position == position && self.index == index {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
}

impl PublicLifecycleCallbacks {
    pub(crate) fn abort_once(&self) {
        let mut aborted = self.aborted.lock().expect("preview abort lock");
        if !*aborted {
            *aborted = true;
            self.replicator.abort();
            self.application.abort();
        }
    }
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
    mut callbacks: PublicLifecycleCallbacks,
) -> Result<Arc<PartitionOperation>> {
    intent
        .validate()
        .map_err(|message| HostError::CommandRejected(message.into()))?;
    callbacks.aborted = registry.abort_guard();
    if intent.lifecycle.is_none() && intent.program.is_none() {
        return Err(HostError::CommandRejected(
            "preview lifecycle input required".into(),
        ));
    }
    if intent.class.is_terminal() {
        registry.fence();
    }
    let operation = registry.admit(intent.clone()).await?;
    let dispatch = {
        let operation = operation.clone();
        async move {
            if !operation.wait_until_ready().await? {
                return Ok::<(), HostError>(());
            }
            if let Some(witness) = callbacks.containment.clone() {
                operation.track_containment(witness).await?;
            }
            #[cfg(all(test, feature = "testing"))]
            operation.track_cut(callbacks.cut.clone()).await;
            operation
                .spawn_root(CallbackContainment::ObjectOwnedOnInterruption, async move {
                    if intent.program.is_some() {
                        program::execute(store, intent, callbacks).await
                    } else {
                        let input = intent.lifecycle.clone().expect("validated lifecycle");
                        execute(store, intent, input, callbacks).await
                    }
                })
                .await?;
            Ok(())
        }
    };
    if operation.snapshot().stage == PublicOperationStage::Ready {
        dispatch.await?;
    } else if operation.snapshot().stage == PublicOperationStage::WaitingForContainment {
        registry.own_control_task(async move {
            if let Err(error) = dispatch.await {
                tracing::warn!(%error, "preview dispatch remains fenced");
            }
        });
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
    let record = store
        .public_operation_records()
        .await?
        .into_iter()
        .find(|record| record.intent == intent)
        .ok_or_else(|| HostError::Corrupt("missing lifecycle record".into()))?;
    let start = record.lifecycle.outcomes.len();
    for (index, instruction) in instructions(&input).into_iter().enumerate().skip(start) {
        #[cfg(all(test, feature = "testing"))]
        if let Some(cut) = &callbacks.cut {
            cut.pause(PublicCutPosition::BeforeInstruction, index).await;
        }
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
                if index == start && start > 0 {
                    callbacks
                        .primary
                        .update_catch_up_replica_set_configuration(
                            input.current.clone().into(),
                            input
                                .previous
                                .clone()
                                .expect("validated previous configuration")
                                .into(),
                        )
                        .await?;
                }
                callbacks
                    .primary
                    .wait_for_catch_up_quorum(ReplicaSetQuorumMode::WriteQuorum)
                    .await?;
                PublicInstructionOutcome::Done
            }
            Access => PublicInstructionOutcome::Done,
            _ => unreachable!("program instruction in role recipe"),
        };
        #[cfg(all(test, feature = "testing"))]
        if let Some(cut) = &callbacks.cut {
            cut.pause(PublicCutPosition::CallbackSuccess, index).await;
        }
        store
            .public_instruction(&intent, index, instruction, Some(outcome.clone()))
            .await?;
        #[cfg(all(test, feature = "testing"))]
        if let Some(cut) = &callbacks.cut {
            cut.pause(PublicCutPosition::InstructionApplied, index)
                .await;
        }
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
        || preview.writes_revoked
        || preview.terminal
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
        report.service_location = record.lifecycle.outcomes.iter().rev().find_map(|outcome| {
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
    (record.intent.lifecycle.is_none() && record.intent.program.is_none()) || {
        record.lifecycle.in_flight.is_none()
            && record.lifecycle.outcomes.len() == operation_instructions(&record.intent).len()
    }
}
