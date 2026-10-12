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
    CanonicalReplicaRebuildRequired, CapturedPublicOperationIdentity, PossibleDataLossIntent,
    PublicBuildInput, PublicCatchUpMode, PublicLifecycleInput, PublicLifecycleRecipe,
    PublicLifecycleReport, PublicOperationIntent, PublicOperationPreviewIdentity,
    PublicOperationProgram, PublicProgress, PublicV1AgentReport, PublicV1CallKind,
    PublicV1OperationCompletion, PublicV1OperationResult, ReplicaRebuildRequired, ServiceLocation,
};
use crate::protocol::types::{
    AccessStatus, ConfigurationId, ProcessSessionId, ReplicaRole, ReplicatorCreationId,
};
use crate::replicator::{
    PrimaryReplicator, ReplicaSetQuorumMode, Replicator, ReplicatorCapabilities,
};

#[path = "public_lifecycle_program.rs"]
mod program;
#[allow(unused_imports)]
pub(crate) use program::{abort, operation_instructions, repeatable, replayable};

#[derive(Clone)]
pub(crate) struct PublicLifecycleCallbacks {
    pub(crate) application: Arc<dyn StatefulServiceReplica>,
    pub(crate) replicator: Arc<dyn Replicator>,
    pub(crate) primary: Arc<dyn PrimaryReplicator>,
    pub(crate) capabilities: ReplicatorCapabilities,
    pub(crate) replicator_creation_id: ReplicatorCreationId,
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

    fn validate_intent_capabilities(&self, intent: &PublicOperationIntent) -> Result<()> {
        if !intent.preview.is_public_v1() {
            return Ok(());
        }
        if let Some(PublicOperationProgram::Swap {
            catch_up_specific_quorum,
            ..
        }) = &intent.program
            && *catch_up_specific_quorum != self.capabilities.catch_up_specific_quorum
        {
            return Err(HostError::CommandRejected(
                "captured swap capability differs from attached Replicator creation".into(),
            ));
        }
        if let Some(recovery) = intent
            .lifecycle
            .as_ref()
            .filter(|input| input.possible_data_loss == PossibleDataLossIntent::Possible)
            .and_then(|input| input.data_loss_recovery.as_ref())
            && (recovery.attempt.process.replicator_creation_id != self.replicator_creation_id
                || recovery.attempt.process.data_loss_replay != self.capabilities.data_loss_replay
                || recovery.attempt.process.catch_up_specific_quorum
                    != self.capabilities.catch_up_specific_quorum)
        {
            return Err(HostError::CommandRejected(
                "data-loss attempt capabilities differ from attached Replicator creation".into(),
            ));
        }
        Ok(())
    }
}

pub(crate) fn instructions(input: &PublicLifecycleInput) -> Vec<PublicInstruction> {
    use PublicInstruction::*;
    if input.recipe == PublicLifecycleRecipe::SecondaryEpochAdvance {
        return vec![Epoch];
    }
    let mut result = vec![ReplicatorPrimary];
    if input.recipe == PublicLifecycleRecipe::FailoverPromotion
        || input.data_loss_recovery.is_some()
    {
        result.push(Epoch);
    }
    result.push(ApplicationPrimary);
    if input.possible_data_loss == PossibleDataLossIntent::Possible {
        result.push(DataLoss);
        if input.data_loss_recovery.is_some() {
            result.extend([
                Progress,
                CurrentConfiguration,
                CatchUp,
                HistoryAdmission,
                Access,
            ]);
        }
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
    callbacks.validate_intent_capabilities(&intent)?;
    callbacks.aborted = registry.abort_guard();
    if intent.lifecycle.is_none() && intent.program.is_none() {
        return Err(HostError::CommandRejected(
            "preview lifecycle input required".into(),
        ));
    }
    let operation = registry.admit(intent.clone()).await?;
    if intent.class.is_terminal() {
        registry.fence();
    }
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
    if let Some(PublicInstructionOutcome::DataLoss(DataLossOutcome::Error(error))) =
        record.lifecycle.outcomes.last()
    {
        return Err(HostError::CommandRejected(error.clone()));
    }
    if matches!(
        record.lifecycle.outcomes.last(),
        Some(PublicInstructionOutcome::RebuildRequired(_))
    ) {
        return Err(HostError::CommandRejected(
            "retained replica rebuild requirement".into(),
        ));
    }
    let captured_state = store.load_state().await?;
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
                    Err(crate::RuntimeError::ReplicaRebuildRequired(failure))
                        if matches!(failure.as_ref(), ReplicaRebuildRequired::LocalProvider(_)) =>
                    {
                        let canonical = enrich_local_rebuild(
                            &captured_state.identity,
                            &intent,
                            &input,
                            *failure,
                        )?;
                        store
                            .public_instruction(
                                &intent,
                                index,
                                instruction,
                                Some(PublicInstructionOutcome::RebuildRequired(Box::new(
                                    canonical,
                                ))),
                            )
                            .await?;
                        return Err(HostError::CommandRejected(
                            "local provider requires replica rebuild".into(),
                        ));
                    }
                    Err(crate::RuntimeError::ReplicaRebuildRequired(_)) => {
                        return Err(HostError::CommandRejected(
                            "remote build failure substituted for local-provider failure".into(),
                        ));
                    }
                    Err(error) => DataLossOutcome::Error(error.to_string()),
                };
                PublicInstructionOutcome::DataLoss(outcome)
            }
            Progress => {
                PublicInstructionOutcome::Progress(callbacks.replicator.current_progress().await?)
            }
            CurrentConfiguration => {
                let configuration = input.configuration();
                program::install_current(&callbacks, &configuration).await?;
                PublicInstructionOutcome::Done
            }
            CatchUpConfiguration => {
                let configuration = input.configuration();
                program::install(&callbacks, &configuration).await?;
                PublicInstructionOutcome::Done
            }
            CatchUp => {
                if index == start && start > 0 {
                    let configuration = input.configuration();
                    program::install(&callbacks, &configuration).await?;
                }
                callbacks
                    .primary
                    .wait_for_catch_up_quorum(ReplicaSetQuorumMode::WriteQuorum)
                    .await?;
                PublicInstructionOutcome::Done
            }
            HistoryAdmission => PublicInstructionOutcome::Done,
            Access => {
                callbacks
                    .application
                    .change_role(ReplicaRole::Primary)
                    .await?;
                PublicInstructionOutcome::Done
            }
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

fn captured_operation(
    identity: &crate::host::state::StorageIdentity,
    intent: &PublicOperationIntent,
    epoch: crate::protocol::types::Epoch,
) -> CapturedPublicOperationIdentity {
    CapturedPublicOperationIdentity {
        resource_uid: identity.resource_uid.clone(),
        preview: intent.preview.clone(),
        epoch,
        operation_id: intent.operation_id.clone(),
        revision: intent.revision,
        detecting_session: intent.process_session_id.clone(),
    }
}

pub(super) fn enrich_local_rebuild(
    identity: &crate::host::state::StorageIdentity,
    intent: &PublicOperationIntent,
    input: &PublicLifecycleInput,
    failure: ReplicaRebuildRequired,
) -> Result<CanonicalReplicaRebuildRequired> {
    let ReplicaRebuildRequired::LocalProvider(failure) = failure else {
        return Err(HostError::CommandRejected(
            "remote failure cannot be enriched as a local provider failure".into(),
        ));
    };
    if failure.provider.replica != input.replica
        || identity.local_identity != input.replica
        || identity.resource_uid.is_empty()
    {
        return Err(HostError::StaleEffectCompletion(
            "provider-visible identity differs from captured local operation".into(),
        ));
    }
    Ok(CanonicalReplicaRebuildRequired::LocalProvider {
        reason: failure.reason,
        provider: failure.provider,
        operation: captured_operation(identity, intent, input.epoch),
        local_replica: identity.local_identity.clone(),
        local_storage: identity.pvc_uid.clone(),
    })
}

pub(super) fn enrich_remote_rebuild(
    identity: &crate::host::state::StorageIdentity,
    intent: &PublicOperationIntent,
    build: &PublicBuildInput,
    failure: ReplicaRebuildRequired,
) -> Result<CanonicalReplicaRebuildRequired> {
    let ReplicaRebuildRequired::RemoteBuildTarget(failure) = failure else {
        return Err(HostError::CommandRejected(
            "local failure cannot be enriched as a remote build failure".into(),
        ));
    };
    let failure = *failure;
    if failure.epoch != build.epoch
        || failure.target != build.replica
        || failure.target_session != build.process_session_id
    {
        return Err(HostError::StaleEffectCompletion(
            "remote rebuild failure differs from the captured build attempt".into(),
        ));
    }
    let build = crate::protocol::public_operations::PublicBuildAttemptReference {
        resource_uid: identity.resource_uid.clone(),
        epoch: build.epoch,
        build_id: intent.operation_id.clone(),
        source: identity.local_identity.clone(),
        source_session: intent.process_session_id.clone(),
        target: build.replica.clone(),
        target_session: build.process_session_id.clone(),
    };
    Ok(CanonicalReplicaRebuildRequired::RemoteBuildTarget {
        reason: failure.reason,
        operation: captured_operation(identity, intent, build.epoch),
        build,
    })
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
        binding: preview.binding.clone(),
        resource_uid: state.identity.resource_uid.clone(),
        replica: state.identity.local_identity,
        process_session_id: session.clone(),
        process_id: std::process::id(),
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

pub(crate) struct PublicV1ReportProjection {
    pub(crate) process_session_id: ProcessSessionId,
    pub(crate) report_sequence: u64,
    pub(crate) role: ReplicaRole,
    pub(crate) read_status: AccessStatus,
    pub(crate) write_status: AccessStatus,
    pub(crate) progress: PublicProgress,
}

fn public_v1_call_kind(intent: &PublicOperationIntent) -> Result<PublicV1CallKind> {
    if let Some(lifecycle) = &intent.lifecycle {
        return Ok(match lifecycle.recipe {
            PublicLifecycleRecipe::InitialPrimary => PublicV1CallKind::InitialPrimary,
            PublicLifecycleRecipe::FailoverPromotion => PublicV1CallKind::FailoverPromotion,
            PublicLifecycleRecipe::SecondaryEpochAdvance => PublicV1CallKind::SecondaryEpochAdvance,
        });
    }
    match intent.program.as_ref() {
        Some(PublicOperationProgram::Open { .. }) => Ok(PublicV1CallKind::Open),
        Some(PublicOperationProgram::Role { .. }) => Ok(PublicV1CallKind::Role),
        Some(PublicOperationProgram::Epoch { .. }) => Ok(PublicV1CallKind::Epoch),
        Some(PublicOperationProgram::Configuration(_)) => Ok(PublicV1CallKind::Configuration),
        Some(PublicOperationProgram::CatchUp { .. }) => Ok(PublicV1CallKind::CatchUp),
        Some(PublicOperationProgram::Progress { .. }) => Ok(PublicV1CallKind::Progress),
        Some(PublicOperationProgram::Swap { .. }) => Ok(PublicV1CallKind::Swap),
        Some(PublicOperationProgram::Build(_)) => Ok(PublicV1CallKind::Build),
        Some(PublicOperationProgram::Remove(_)) => Ok(PublicV1CallKind::Remove),
        Some(PublicOperationProgram::Close) => Ok(PublicV1CallKind::Close),
        Some(PublicOperationProgram::Abort) => Ok(PublicV1CallKind::Abort),
        None => Err(HostError::Corrupt(
            "schema-11 completion lacks a public call kind".into(),
        )),
    }
}

#[allow(clippy::type_complexity)]
fn public_v1_completion_authority(
    intent: &PublicOperationIntent,
) -> Result<(
    Option<ConfigurationId>,
    Option<ConfigurationId>,
    Option<PublicCatchUpMode>,
    Option<bool>,
)> {
    let configuration_ids =
        |configuration: &crate::protocol::public_operations::PublicConfiguration| {
            (
                configuration
                    .previous
                    .as_ref()
                    .map(|configuration| configuration.configuration_id.clone()),
                Some(configuration.current.configuration_id.clone()),
            )
        };
    if let Some(lifecycle) = &intent.lifecycle {
        return Ok((
            lifecycle
                .previous
                .as_ref()
                .map(|configuration| configuration.configuration_id.clone()),
            Some(lifecycle.current.configuration_id.clone()),
            None,
            None,
        ));
    }
    let Some(program) = intent.program.as_ref() else {
        return Err(HostError::Corrupt(
            "schema-11 completion lacks public authority".into(),
        ));
    };
    Ok(match program {
        PublicOperationProgram::Configuration(configuration) => {
            let (previous, current) = configuration_ids(configuration);
            (previous, current, None, None)
        }
        PublicOperationProgram::CatchUp {
            configuration,
            mode,
        } => {
            let (previous, current) = configuration_ids(configuration);
            (previous, current, Some(*mode), None)
        }
        PublicOperationProgram::Swap {
            refreshed,
            mode,
            catch_up_specific_quorum,
            ..
        } => {
            let (previous, current) = configuration_ids(refreshed);
            (
                previous,
                current,
                Some(*mode),
                Some(*catch_up_specific_quorum),
            )
        }
        PublicOperationProgram::Open { .. }
        | PublicOperationProgram::Role { .. }
        | PublicOperationProgram::Epoch { .. }
        | PublicOperationProgram::Progress { .. }
        | PublicOperationProgram::Build(_)
        | PublicOperationProgram::Remove(_)
        | PublicOperationProgram::Close
        | PublicOperationProgram::Abort => (None, None, None, None),
    })
}

pub(crate) async fn public_v1_report(
    store: &dyn AgentStore,
    identity: &PublicOperationPreviewIdentity,
    projection: PublicV1ReportProjection,
) -> Result<PublicV1AgentReport> {
    if !identity.is_public_v1() {
        return Err(HostError::IdentityMismatch(
            "public-v1 report requires protocol 11".into(),
        ));
    }
    let state = store.load_state().await?;
    let preview = state.public_operation_preview.ok_or_else(|| {
        HostError::CommandRejected("public-v1 report requires preview store".into())
    })?;
    if &preview.identity != identity {
        return Err(HostError::IdentityMismatch(
            "public-v1 report identity changed".into(),
        ));
    }
    let history = preview.history.clone().ok_or_else(|| {
        HostError::CommandRejected("public-v1 report requires initialized history".into())
    })?;
    let storage_incarnation = preview.storage_incarnation.clone().ok_or_else(|| {
        HostError::CommandRejected("public-v1 report requires storage incarnation".into())
    })?;
    let data_loss_recovery = preview.data_loss_recovery.clone();
    let completed_operations = preview
        .operations
        .values()
        .filter(|record| record.stage == PublicOperationStage::Completed)
        .filter(|record| record.intent.process_session_id == projection.process_session_id)
        .map(|record| {
            let rebuild = record.lifecycle.outcomes.iter().find_map(|outcome| {
                if let PublicInstructionOutcome::RebuildRequired(failure) = outcome {
                    Some(failure.clone())
                } else {
                    None
                }
            });
            let result = if let Some(rebuild) = rebuild {
                PublicV1OperationResult::RebuildRequired(rebuild)
            } else {
                match record.disposition.as_ref().ok_or_else(|| {
                    HostError::Corrupt("schema-11 completed operation lacks a disposition".into())
                })? {
                    PublicOperationDisposition::Succeeded => PublicV1OperationResult::Succeeded,
                    PublicOperationDisposition::Failed(message) => {
                        PublicV1OperationResult::Failed(message.clone())
                    }
                    PublicOperationDisposition::Ambiguous(message) => {
                        PublicV1OperationResult::Ambiguous(message.clone())
                    }
                    PublicOperationDisposition::Cancelled => PublicV1OperationResult::Cancelled,
                    PublicOperationDisposition::Attached(owner) => {
                        PublicV1OperationResult::Ambiguous(format!("attached to {owner}"))
                    }
                }
            };
            let history = record.accepted_history.clone().ok_or_else(|| {
                HostError::Corrupt("schema-11 completion lacks accepted history".into())
            })?;
            let storage_incarnation = record.accepted_storage.clone().ok_or_else(|| {
                HostError::Corrupt("schema-11 completion lacks accepted storage".into())
            })?;
            let (
                previous_configuration_id,
                current_configuration_id,
                catch_up_mode,
                catch_up_specific_quorum,
            ) = public_v1_completion_authority(&record.intent)?;
            Ok(PublicV1OperationCompletion {
                operation_id: record.intent.operation_id.clone(),
                revision: record.intent.revision,
                call_kind: public_v1_call_kind(&record.intent)?,
                process_session_id: record.intent.process_session_id.clone(),
                input_digest: record.intent.input_digest.clone(),
                replay_disposition: record.intent.replay_disposition,
                history,
                storage_incarnation,
                previous_configuration_id,
                current_configuration_id,
                catch_up_mode,
                catch_up_specific_quorum,
                result,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let report = PublicV1AgentReport {
        identity: identity.clone(),
        resource_uid: state.identity.resource_uid,
        replica: state.identity.local_identity,
        process_session_id: projection.process_session_id,
        report_sequence: projection.report_sequence,
        role: projection.role,
        read_status: projection.read_status,
        write_status: projection.write_status,
        history,
        storage_incarnation,
        progress: projection.progress,
        data_loss_recovery,
        completed_operations,
        extensions: Default::default(),
    };
    report
        .validate()
        .map_err(|message| HostError::CommandRejected(message.into()))?;
    Ok(report)
}

pub(crate) fn complete(record: &PublicOperationRecord) -> bool {
    (record.intent.lifecycle.is_none() && record.intent.program.is_none()) || {
        record.lifecycle.in_flight.is_none()
            && record.lifecycle.outcomes.len() == operation_instructions(&record.intent).len()
    }
}
