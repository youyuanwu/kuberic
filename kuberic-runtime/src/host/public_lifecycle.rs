//! Repository-only lifecycle execution. Legacy authority and access engines are
//! deliberately not used: their configuration recipe has different ordering.

use std::sync::Arc;

use crate::application::StatefulServiceReplica;
use crate::host::operation::{CallbackContainment, PartitionOperation, PartitionOperationRegistry};
use crate::host::state::{
    DataLossOutcome, PublicCallbackFailure, PublicCallbackFamily, PublicInstruction,
    PublicInstructionOutcome, PublicOperationDisposition, PublicOperationRecord,
    PublicOperationStage,
};
use crate::host::store::AgentStore;
use crate::host::{HostError, Result};
use crate::protocol::public_operations::{
    CanonicalReplicaRebuildRequired, CapturedPublicOperationIdentity, PossibleDataLossIntent,
    PublicBuildInput, PublicCatchUpMode, PublicConfiguration, PublicLifecycleInput,
    PublicLifecycleRecipe, PublicOperationIntent, PublicOperationProgram, PublicProgress,
    PublicV1AgentReport, PublicV1CallKind, PublicV1Identity, PublicV1OperationCompletion,
    PublicV1OperationResult, ReplicaRebuildRequired, ServiceLocation,
};
use crate::protocol::types::{
    AccessStatus, ConfigurationId, ProcessSessionId, ReplicaRole, ReplicatorCreationId,
};
use crate::replicator::{
    PrimaryReplicator, ReplicaSetQuorumMode, Replicator, ReplicatorCapabilities,
};

#[path = "public_lifecycle_program.rs"]
pub(super) mod program;
#[path = "public_lifecycle_terminal.rs"]
mod terminal;
pub(crate) use program::{operation_instructions, repeatable, replayable};
pub(crate) use terminal::recover as recover_terminal;

pub(crate) fn instruction_selected(
    record: &super::state::PublicOperationRecord,
    selected: super::state::PublicInstruction,
) -> bool {
    operation_instructions(&record.intent)
        .iter()
        .position(|instruction| *instruction == selected)
        .is_some_and(|index| {
            record.lifecycle.outcomes.len() > index || record.lifecycle.in_flight == Some(selected)
        })
}

#[cfg(all(test, feature = "testing"))]
#[path = "public_lifecycle_tests.rs"]
mod tests;

type ApplicationControlMatch = Arc<dyn Fn(&Arc<dyn Replicator>) -> bool + Send + Sync>;
pub(crate) type NativeRoleEpoch = Arc<
    dyn Fn() -> crate::Result<
            Option<(
                crate::replicator::v1::LifecycleObservation,
                Option<crate::protocol::types::Epoch>,
            )>,
        > + Send
        + Sync,
>;
pub(crate) type TerminalDrain =
    Arc<dyn Fn() -> futures::future::BoxFuture<'static, crate::Result<()>> + Send + Sync>;
pub(crate) type AccessFactory =
    Arc<dyn Fn(&Arc<PartitionOperation>) -> Option<Arc<super::hosting::AccessOwner>> + Send + Sync>;

#[derive(Clone, Default)]
pub(crate) struct PublicCallbackRetryProof {
    families: std::collections::BTreeSet<PublicCallbackFamily>,
    returned_failure: Arc<std::sync::Mutex<Option<PublicCallbackFailure>>>,
}

impl PublicCallbackRetryProof {
    fn for_attempt(&self) -> Self {
        Self {
            families: self.families.clone(),
            returned_failure: Default::default(),
        }
    }

    fn returned_error(
        &self,
        family: PublicCallbackFamily,
        instruction: PublicInstruction,
        error: &crate::RuntimeError,
    ) {
        if self.families.contains(&family)
            && family.allows(instruction)
            && !matches!(error, crate::RuntimeError::ReplicaRebuildRequired(_))
        {
            *self.returned_failure.lock().unwrap() = Some(PublicCallbackFailure {
                instruction,
                family,
                error: error.to_string(),
            });
        }
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) fn declared(families: impl IntoIterator<Item = PublicCallbackFamily>) -> Self {
        Self {
            families: families.into_iter().collect(),
            ..Default::default()
        }
    }
}

#[derive(Clone)]
pub(crate) struct PublicLifecycleCallbacks {
    pub(crate) application: Arc<dyn StatefulServiceReplica>,
    pub(crate) replicator: Arc<dyn Replicator>,
    pub(crate) primary: Arc<dyn PrimaryReplicator>,
    pub(crate) application_control_match: Option<ApplicationControlMatch>,
    pub(crate) native_role_epoch: Option<NativeRoleEpoch>,
    pub(crate) revoke_access: Arc<dyn Fn() -> crate::Result<()> + Send + Sync>,
    pub(crate) capabilities: ReplicatorCapabilities,
    pub(crate) replicator_creation_id: ReplicatorCreationId,
    pub(crate) open_context: Option<crate::application::OpenContext>,
    pub(crate) containment: Option<tokio::sync::watch::Receiver<bool>>,
    pub(crate) wait_for_containment: bool,
    pub(crate) preempt_retained_owner: bool,
    pub(crate) native_observer_callbacks: bool,
    pub(crate) retry_proof: PublicCallbackRetryProof,
    pub(crate) terminal_drain: TerminalDrain,
    pub(super) terminal_cleanup: Option<Arc<terminal::TerminalCleanup>>,
    pub(crate) access_factory: Option<AccessFactory>,
    pub(crate) access_owner: Option<Arc<super::hosting::AccessOwner>>,
    pub(crate) aborted: Arc<std::sync::Mutex<bool>>,
    #[cfg(feature = "testing")]
    pub(crate) cut: Option<Arc<PublicOperationCut>>,
}

pub(crate) fn replacement_role_operation_id(
    renewal: &crate::protocol::public_operations::PublicV1SessionRenewalStatus,
) -> crate::protocol::types::OperationId {
    crate::protocol::types::OperationId::new(format!(
        "{}:replacement-role",
        renewal.required.operation_id
    ))
}

pub(crate) fn is_replacement_role_replay(
    renewal: &crate::protocol::public_operations::PublicV1SessionRenewalStatus,
    local: &crate::protocol::types::ReplicaIdentity,
    intent: &PublicOperationIntent,
) -> bool {
    renewal.stage
        == crate::protocol::public_operations::PublicSessionRenewalStage::ReplacementRolePending
        && renewal
            .required
            .replacement
            .as_ref()
            .is_some_and(|endpoint| {
                &endpoint.identity == local
                    && endpoint.process_session_id == intent.process_session_id
            })
        && intent.operation_id == replacement_role_operation_id(renewal)
        && intent.class == crate::protocol::public_operations::PublicOperationClass::Authority
        && intent.program
            == Some(PublicOperationProgram::Role {
                epoch: renewal.required.renewal_epoch,
                role: renewal.required.predecessor.role,
            })
}

#[cfg(feature = "testing")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicCutPosition {
    BuildAuthorityRead,
    BeforeInboundBuildCommit,
    BeforeAccessReservation,
    BeforeInstruction,
    InstructionSelected,
    CallbackSuccess,
    InstructionApplied,
    CallbackApplied,
    Completed,
}

#[cfg(feature = "testing")]
pub struct PublicOperationCut {
    pub position: PublicCutPosition,
    pub index: usize,
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

#[cfg(feature = "testing")]
impl PublicOperationCut {
    pub(crate) async fn pause(&self, position: PublicCutPosition, index: usize) {
        if self.position == position && self.index == index {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
}

impl PublicLifecycleCallbacks {
    async fn change_primary_role(
        &self,
        epoch: crate::protocol::types::Epoch,
        instruction: PublicInstruction,
    ) -> crate::Result<()> {
        if self
            .access_owner
            .as_ref()
            .is_some_and(|owner| owner.resumes_primary())
            && let Some(
                native @ (
                    crate::replicator::v1::LifecycleObservation::Ready(ReplicaRole::Primary),
                    Some(_),
                ),
            ) = self
                .native_role_epoch
                .as_ref()
                .map(|observe| observe())
                .transpose()?
                .flatten()
        {
            // Final demotion catch-up can finish before its role callback. Reconcile
            // the observed native role; this promotion still requires fresh catch-up.
            return super::service::replay_role_at_epoch(
                self.replicator.as_ref(),
                Some(native),
                epoch,
                ReplicaRole::Primary,
            )
            .await;
        }
        let result = self
            .replicator
            .change_role(epoch, ReplicaRole::Primary)
            .await;
        if let Err(error) = &result {
            self.retry_proof.returned_error(
                PublicCallbackFamily::ReplicatorRole,
                instruction,
                error,
            );
        }
        result
    }

    pub(crate) fn application_control_matches(&self, control: &Arc<dyn Replicator>) -> bool {
        Arc::ptr_eq(control, &self.replicator)
            || self
                .application_control_match
                .as_ref()
                .is_some_and(|matches| matches(control))
    }

    pub(crate) fn abort_once(&self) {
        let mut aborted = self.aborted.lock().expect("preview abort lock");
        if !*aborted {
            *aborted = true;
            self.replicator.abort();
            self.application.abort();
        }
    }

    fn validate_intent_capabilities(&self, intent: &PublicOperationIntent) -> Result<()> {
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
    callbacks: PublicLifecycleCallbacks,
) -> Result<Arc<PartitionOperation>> {
    launch_attempt(registry, store, intent, callbacks)
        .await
        .map(|(operation, _)| operation)
}

pub(crate) async fn launch_attempt(
    registry: &Arc<PartitionOperationRegistry>,
    store: Arc<dyn AgentStore>,
    intent: PublicOperationIntent,
    callbacks: PublicLifecycleCallbacks,
) -> Result<(Arc<PartitionOperation>, u64)> {
    launch_bound_attempt(registry, store, intent, callbacks, |_| ())
        .await
        .map(|(operation, attempt, ())| (operation, attempt))
}

pub(crate) async fn launch_bound_attempt<T>(
    registry: &Arc<PartitionOperationRegistry>,
    store: Arc<dyn AgentStore>,
    intent: PublicOperationIntent,
    mut callbacks: PublicLifecycleCallbacks,
    bind_observer: impl FnOnce(&Arc<PartitionOperation>) -> T,
) -> Result<(Arc<PartitionOperation>, u64, T)> {
    let dispatch_lock = registry.dispatch_lock(&intent.operation_id);
    let _dispatch = dispatch_lock.lock().await;
    intent
        .validate()
        .map_err(|message| HostError::CommandRejected(message.into()))?;
    callbacks.validate_intent_capabilities(&intent)?;
    callbacks.aborted = registry.abort_guard();
    callbacks.retry_proof = callbacks.retry_proof.for_attempt();
    if intent.class.is_terminal()
        || intent
            .lifecycle
            .as_ref()
            .is_some_and(|input| input.possible_data_loss == PossibleDataLossIntent::Possible)
    {
        callbacks.retry_proof = Default::default();
    }
    if intent.lifecycle.is_none() && intent.program.is_none() {
        return Err(HostError::CommandRejected(
            "preview lifecycle input required".into(),
        ));
    }
    let operation = registry.admit(intent.clone()).await?;
    let retry_prefix = if operation.snapshot().stage == PublicOperationStage::RetryPending
        && operation
            .snapshot()
            .retry_failure
            .as_ref()
            .is_some_and(|failure| callbacks.retry_proof.families.contains(&failure.family))
    {
        let failed = operation.snapshot();
        operation.resume_returned_failure().await?;
        Some(failed)
    } else {
        None
    };
    let attempt = operation.snapshot().attempt;
    if !matches!(
        intent.program,
        Some(PublicOperationProgram::Progress { .. } | PublicOperationProgram::Build(_))
    ) && matches!(
        operation.snapshot().stage,
        PublicOperationStage::Ready | PublicOperationStage::WaitingForContainment
    ) && let Some(factory) = &callbacks.access_factory
    {
        #[cfg(feature = "testing")]
        if let Some(cut) = &callbacks.cut {
            cut.pause(PublicCutPosition::BeforeAccessReservation, 0)
                .await;
        }
        callbacks.access_owner = factory(&operation);
    }
    let observer = bind_observer(&operation);
    if intent.class.is_terminal() {
        registry.fence();
        if operation.snapshot().stage == PublicOperationStage::ContainmentPending {
            terminal::recover(registry, &operation, callbacks).await?;
            return Ok((operation, attempt, observer));
        }
        if callbacks.preempt_retained_owner
            && registry.has_uncontained_predecessors(&operation).await
        {
            callbacks.abort_once();
            if let Err(error) = (callbacks.terminal_drain)().await {
                // The owner retains the same outcome for the terminal
                // instruction/cleanup path; containment is not Close success.
                tracing::warn!(%error, "terminal preemption retained a child failure");
                operation.record_containment_failure(&HostError::from(error));
            }
        }
    }
    let terminal_registry = registry.clone();
    let dispatch = {
        let operation = operation.clone();
        async move {
            if !operation.wait_until_ready().await? {
                return Ok::<(), HostError>(());
            }
            let terminal_guard = if intent.class.is_terminal() {
                let (cleanup, owner) = terminal::bind(callbacks.clone());
                callbacks.terminal_cleanup = Some(cleanup.clone());
                let terminal_operation = operation.clone();
                terminal_registry.own_terminal_task(async move {
                    owner.await;
                    if let Err(error) = cleanup.wait().await {
                        // A cancelled root no longer observes Cleanup's result.
                        // Keep that failure independently of its disposition.
                        let _ = terminal_operation.wait_for_terminal().await;
                        terminal_operation.record_containment_failure(&error);
                    }
                });
                Some(terminal::TerminalRootGuard(callbacks.clone()))
            } else {
                None
            };
            if let Some(witness) = callbacks
                .terminal_cleanup
                .as_ref()
                .map(|cleanup| cleanup.witness())
                .or_else(|| callbacks.containment.clone())
            {
                operation
                    .track_containment(
                        witness,
                        intent.class.is_terminal() || callbacks.wait_for_containment,
                    )
                    .await?;
            }
            #[cfg(feature = "testing")]
            operation.track_cut(callbacks.cut.clone()).await;
            let containment = if intent.class.is_terminal() {
                CallbackContainment::RootTask
            } else if callbacks.native_observer_callbacks {
                CallbackContainment::NativeObserver
            } else {
                CallbackContainment::ObjectOwnedOnInterruption
            };
            operation
                .spawn_root_with_retry(
                    containment,
                    Some(callbacks.retry_proof.returned_failure.clone()),
                    async move {
                        let _terminal_guard = terminal_guard;
                        let mut access_guard =
                            super::hosting::AccessRootGuard::new(callbacks.access_owner.clone());
                        let result = async {
                            if let (Some(owner), Some(failed)) =
                                (&callbacks.access_owner, &retry_prefix)
                            {
                                owner.restore_retry_prefix(store.as_ref(), failed).await?;
                            }
                            if intent.program.is_some() {
                                program::execute(store, intent, callbacks).await
                            } else {
                                let input = intent.lifecycle.clone().expect("validated lifecycle");
                                execute(store, intent, input, callbacks).await
                            }
                        }
                        .await;
                        if result.is_ok() {
                            access_guard.succeeded();
                        }
                        result
                    },
                )
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
    Ok((operation, attempt, observer))
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
        #[cfg(feature = "testing")]
        if let Some(cut) = &callbacks.cut {
            cut.pause(PublicCutPosition::BeforeInstruction, index).await;
        }
        store
            .public_instruction(&intent, index, instruction, None)
            .await?;
        if let Some(owner) = &callbacks.access_owner {
            owner.selected(instruction);
        }
        let outcome = match instruction {
            ReplicatorPrimary => {
                local_callback(
                    callbacks
                        .change_primary_role(input.epoch, instruction)
                        .await,
                    store.as_ref(),
                    &intent,
                    index,
                    instruction,
                    input.epoch,
                    None,
                )
                .await?;
                PublicInstructionOutcome::Done
            }
            Epoch => {
                local_callback(
                    callbacks.replicator.update_epoch(input.epoch).await,
                    store.as_ref(),
                    &intent,
                    index,
                    instruction,
                    input.epoch,
                    Some((&callbacks.retry_proof, PublicCallbackFamily::Epoch)),
                )
                .await?;
                PublicInstructionOutcome::Done
            }
            ApplicationPrimary => {
                let role = local_callback(
                    callbacks
                        .application
                        .change_role(ReplicaRole::Primary)
                        .await,
                    store.as_ref(),
                    &intent,
                    index,
                    instruction,
                    input.epoch,
                    Some((
                        &callbacks.retry_proof,
                        PublicCallbackFamily::ApplicationRole,
                    )),
                )
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
            Progress => PublicInstructionOutcome::Progress(
                local_callback(
                    callbacks.replicator.current_progress().await,
                    store.as_ref(),
                    &intent,
                    index,
                    instruction,
                    input.epoch,
                    None,
                )
                .await?,
            ),
            CurrentConfiguration => {
                let configuration =
                    super::public_action_state::configuration_at(&intent, instruction)
                        .expect("current configuration instruction");
                local_callback(
                    program::install_current(&callbacks, &configuration, instruction).await,
                    store.as_ref(),
                    &intent,
                    index,
                    instruction,
                    input.epoch,
                    None,
                )
                .await?;
                PublicInstructionOutcome::Done
            }
            CatchUpConfiguration => {
                let configuration = input.configuration();
                local_callback(
                    program::install(
                        store.as_ref(),
                        &callbacks,
                        &configuration,
                        true,
                        instruction,
                    )
                    .await,
                    store.as_ref(),
                    &intent,
                    index,
                    instruction,
                    input.epoch,
                    None,
                )
                .await?;
                PublicInstructionOutcome::Done
            }
            CatchUp => {
                if index == start && start > 0 {
                    let configuration =
                        super::public_action_state::configuration_at(&intent, instruction)
                            .expect("catch-up configuration");
                    local_callback(
                        program::install(
                            store.as_ref(),
                            &callbacks,
                            &configuration,
                            true,
                            instruction,
                        )
                        .await,
                        store.as_ref(),
                        &intent,
                        index,
                        instruction,
                        input.epoch,
                        None,
                    )
                    .await?;
                    if let Some(owner) = &callbacks.access_owner {
                        owner.reinstalled(store.as_ref(), &configuration).await?;
                    }
                }
                local_callback(
                    callbacks
                        .primary
                        .wait_for_catch_up_quorum(ReplicaSetQuorumMode::WriteQuorum)
                        .await,
                    store.as_ref(),
                    &intent,
                    index,
                    instruction,
                    input.epoch,
                    Some((&callbacks.retry_proof, PublicCallbackFamily::CatchUp)),
                )
                .await?;
                PublicInstructionOutcome::Done
            }
            HistoryAdmission => PublicInstructionOutcome::Done,
            Access => {
                local_callback(
                    callbacks
                        .application
                        .change_role(ReplicaRole::Primary)
                        .await,
                    store.as_ref(),
                    &intent,
                    index,
                    instruction,
                    input.epoch,
                    None,
                )
                .await?;
                PublicInstructionOutcome::Done
            }
            _ => unreachable!("program instruction in role recipe"),
        };
        #[cfg(feature = "testing")]
        if let Some(cut) = &callbacks.cut {
            cut.pause(PublicCutPosition::CallbackSuccess, index).await;
        }
        if let Some(owner) = &callbacks.access_owner {
            owner.callback_succeeded(instruction);
        }
        store
            .public_instruction(&intent, index, instruction, Some(outcome.clone()))
            .await?;
        if let Some(owner) = &callbacks.access_owner {
            owner.receipt(store.as_ref(), instruction).await?;
        }
        #[cfg(feature = "testing")]
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
        identity: intent.identity.clone(),
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
    enrich_local_rebuild_at_epoch(identity, intent, input.epoch, &input.replica, failure)
}

pub(super) fn enrich_local_rebuild_at_epoch(
    identity: &crate::host::state::StorageIdentity,
    intent: &PublicOperationIntent,
    epoch: crate::protocol::types::Epoch,
    local_replica: &crate::protocol::types::ReplicaIdentity,
    failure: ReplicaRebuildRequired,
) -> Result<CanonicalReplicaRebuildRequired> {
    let ReplicaRebuildRequired::LocalProvider(failure) = failure else {
        return Err(HostError::CommandRejected(
            "remote failure cannot be enriched as a local provider failure".into(),
        ));
    };
    if failure.provider.replica != *local_replica
        || identity.local_identity != *local_replica
        || identity.resource_uid.is_empty()
    {
        return Err(HostError::StaleEffectCompletion(
            "provider-visible identity differs from captured local operation".into(),
        ));
    }
    Ok(CanonicalReplicaRebuildRequired::LocalProvider {
        reason: failure.reason,
        provider: failure.provider,
        operation: captured_operation(identity, intent, epoch),
        local_replica: identity.local_identity.clone(),
        local_storage: identity.pvc_uid.clone(),
    })
}

async fn local_callback<T>(
    result: crate::Result<T>,
    store: &dyn AgentStore,
    intent: &PublicOperationIntent,
    index: usize,
    instruction: PublicInstruction,
    epoch: crate::protocol::types::Epoch,
    retry: Option<(&PublicCallbackRetryProof, PublicCallbackFamily)>,
) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(crate::RuntimeError::ReplicaRebuildRequired(failure))
            if matches!(failure.as_ref(), ReplicaRebuildRequired::LocalProvider(_)) =>
        {
            let state = store.load_state().await?;
            let canonical = enrich_local_rebuild_at_epoch(
                &state.identity,
                intent,
                epoch,
                &state.identity.local_identity,
                *failure,
            )?;
            store
                .public_instruction(
                    intent,
                    index,
                    instruction,
                    Some(PublicInstructionOutcome::RebuildRequired(Box::new(
                        canonical,
                    ))),
                )
                .await?;
            Err(HostError::CommandRejected(
                "local provider requires replica rebuild".into(),
            ))
        }
        Err(crate::RuntimeError::ReplicaRebuildRequired(_)) => Err(HostError::CommandRejected(
            "remote build failure substituted for local-provider failure".into(),
        )),
        Err(error) => {
            if let Some((proof, family)) = retry {
                proof.returned_error(family, instruction, &error);
            }
            Err(error.into())
        }
    }
}

pub(super) fn enrich_remote_rebuild(
    identity: &crate::host::state::StorageIdentity,
    source_storage: &crate::protocol::types::ReplicaStorageIncarnationContext,
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
        || source_storage.replica != identity.local_identity
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
        source_storage: source_storage.clone(),
        source_session: intent.process_session_id.clone(),
        target: build.replica.clone(),
        target_storage: build.target_storage.clone(),
        target_session: build.process_session_id.clone(),
    };
    Ok(CanonicalReplicaRebuildRequired::RemoteBuildTarget {
        reason: failure.reason,
        operation: captured_operation(identity, intent, build.epoch),
        build: Box::new(build),
    })
}

pub(crate) struct PublicV1ReportProjection {
    pub(crate) catch_up_specific_quorum: Option<bool>,
    pub(crate) process_session_id: ProcessSessionId,
    pub(crate) replicator_creation_id: ReplicatorCreationId,
    pub(crate) report_sequence: u64,
    pub(crate) role: ReplicaRole,
    pub(crate) read_status: AccessStatus,
    pub(crate) write_status: AccessStatus,
    pub(crate) progress: PublicProgress,
    pub(crate) progress_history: Option<crate::protocol::types::HistoryContext>,
    pub(crate) engine_idle:
        Option<crate::protocol::public_operations::PublicV1EngineIdleObservation>,
    pub(crate) observation_alive: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
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
        Some(
            PublicOperationProgram::Remove(_) | PublicOperationProgram::RetireIdleReplica { .. },
        ) => Ok(PublicV1CallKind::Remove),
        Some(PublicOperationProgram::Close) => Ok(PublicV1CallKind::Close),
        Some(PublicOperationProgram::Abort) => Ok(PublicV1CallKind::Abort),
        None => Err(HostError::Corrupt(
            "replacement completion lacks a public call kind".into(),
        )),
    }
}

fn public_v1_role_epoch(intent: &PublicOperationIntent) -> Option<crate::protocol::types::Epoch> {
    intent
        .lifecycle
        .as_ref()
        .map(|lifecycle| lifecycle.epoch)
        .or(match intent.program.as_ref() {
            Some(PublicOperationProgram::Role { epoch, .. })
            | Some(PublicOperationProgram::Swap { epoch, .. }) => Some(*epoch),
            _ => None,
        })
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
            "replacement completion lacks public authority".into(),
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
        PublicOperationProgram::Build(build) => {
            (None, build.current_configuration_id.clone(), None, None)
        }
        PublicOperationProgram::Open { .. }
        | PublicOperationProgram::Role { .. }
        | PublicOperationProgram::Epoch { .. }
        | PublicOperationProgram::Progress { .. }
        | PublicOperationProgram::Remove(_)
        | PublicOperationProgram::RetireIdleReplica { .. }
        | PublicOperationProgram::Close
        | PublicOperationProgram::Abort => (None, None, None, None),
    })
}

pub(crate) fn installed_configuration_for_state(
    state: &crate::host::state::AgentState,
    process: &ProcessSessionId,
) -> Option<crate::protocol::public_operations::PublicConfiguration> {
    let preview = state.public_v1_operation_state.as_ref()?;
    preview.storage_incarnation.as_ref()?;
    let history = preview.history.as_ref()?;
    preview
        .operations
        .values()
        .filter(|record| {
            record.stage == PublicOperationStage::Completed
                && record.disposition == Some(PublicOperationDisposition::Succeeded)
                && super::public_action_state::current_scope(preview, process, record)
        })
        .filter_map(|record| {
            let configuration = installed_configuration_from_record(record)?;
            (configuration.current.epoch.data_loss_number == history.data_loss_number)
                .then_some((record.intent.revision, configuration))
        })
        .max_by_key(|(revision, _)| *revision)
        .map(|(_, configuration)| configuration)
}

pub(crate) fn installed_configuration_from_record(
    record: &PublicOperationRecord,
) -> Option<PublicConfiguration> {
    if record.stage != PublicOperationStage::Completed
        || record.disposition != Some(PublicOperationDisposition::Succeeded)
    {
        return None;
    }
    operation_instructions(&record.intent)
        .into_iter()
        .zip(&record.lifecycle.outcomes)
        .filter(|(instruction, outcome)| {
            configuration_instruction(*instruction) && **outcome == PublicInstructionOutcome::Done
        })
        .filter_map(|(instruction, _)| {
            super::public_action_state::configuration_at(&record.intent, instruction)
        })
        .next_back()
}

fn configuration_instruction(instruction: PublicInstruction) -> bool {
    matches!(
        instruction,
        PublicInstruction::Configuration
            | PublicInstruction::StartingConfiguration
            | PublicInstruction::RefreshedConfiguration
            | PublicInstruction::CurrentConfiguration
            | PublicInstruction::CatchUpConfiguration
    )
}

/// An optional engine query cannot be attributed to the last completed host
/// configuration across a newer or unfinished native topology mutation.
pub(crate) fn configuration_observation_safe(
    state: &crate::host::state::AgentState,
    process: &ProcessSessionId,
) -> bool {
    let Some(preview) = &state.public_v1_operation_state else {
        return false;
    };
    let Some(installed) = preview
        .operations
        .values()
        .filter(|record| {
            super::public_action_state::current_scope(preview, process, record)
                && installed_configuration_from_record(record).is_some()
        })
        .max_by_key(|record| record.intent.revision)
    else {
        return false;
    };
    !preview.operations.values().any(|record| {
        record.intent.operation_id != installed.intent.operation_id
            && super::public_action_state::current_scope(preview, process, record)
            && (record.intent.revision > installed.intent.revision
                || record.stage != PublicOperationStage::Completed)
            && operation_instructions(&record.intent)
                .into_iter()
                .any(|instruction| {
                    configuration_instruction(instruction)
                        || matches!(
                            instruction,
                            PublicInstruction::ReplicatorRole
                                | PublicInstruction::ReplicatorPrimary
                                | PublicInstruction::Epoch
                                | PublicInstruction::ProgramEpoch
                                | PublicInstruction::DataLoss
                        )
                })
    })
}

pub(crate) async fn public_v1_report(
    store: &dyn AgentStore,
    identity: &PublicV1Identity,
    projection: PublicV1ReportProjection,
) -> Result<PublicV1AgentReport> {
    if !identity.is_public_v1() {
        return Err(HostError::IdentityMismatch(
            "public-v1 report requires protocol 11".into(),
        ));
    }
    let state = store.load_state().await?;
    let configuration = installed_configuration_for_state(&state, &projection.process_session_id);
    let sample_safe = configuration_observation_safe(&state, &projection.process_session_id);
    let open_mode = state.application_open_mode();
    let preview = state.public_v1_operation_state.ok_or_else(|| {
        HostError::CommandRejected("Public V1 report requires replacement state".into())
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
        .filter(|record| record.intent.program.is_some() || record.intent.lifecycle.is_some())
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
                    HostError::Corrupt("replacement operation lacks a disposition".into())
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
                HostError::Corrupt("replacement completion lacks accepted history".into())
            })?;
            let storage_incarnation = record.accepted_storage.clone().ok_or_else(|| {
                HostError::Corrupt("replacement completion lacks accepted storage".into())
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
    let replication_address = preview
        .operations
        .values()
        .filter(|record| {
            record.stage == PublicOperationStage::Completed
                && record.disposition == Some(PublicOperationDisposition::Succeeded)
                && record.intent.process_session_id == projection.process_session_id
                && matches!(
                    record.intent.program,
                    Some(
                        PublicOperationProgram::Open { .. }
                            | PublicOperationProgram::Close
                            | PublicOperationProgram::Abort
                    )
                )
        })
        .max_by_key(|record| (&record.intent.revision, &record.intent.operation_id))
        .filter(|record| {
            matches!(
                record.intent.program,
                Some(PublicOperationProgram::Open { .. })
            )
        })
        .and_then(|record| {
            record.lifecycle.outcomes.iter().find_map(|outcome| {
                if let PublicInstructionOutcome::Endpoint(address) = outcome {
                    Some(address.clone())
                } else {
                    None
                }
            })
        });
    let service_location = if projection.role == ReplicaRole::Primary
        && projection.write_status == AccessStatus::Granted
    {
        preview
            .operations
            .values()
            .filter(|record| {
                record.stage == PublicOperationStage::Completed
                    && record.disposition == Some(PublicOperationDisposition::Succeeded)
                    && record.intent.process_session_id == projection.process_session_id
                    && public_v1_role_epoch(&record.intent).is_some()
            })
            .max_by_key(|record| record.intent.revision)
            .and_then(|record| {
                let address = record.lifecycle.outcomes.iter().rev().find_map(|outcome| {
                    if let PublicInstructionOutcome::ApplicationRole(address) = outcome {
                        address.clone().filter(|address| !address.is_empty())
                    } else {
                        None
                    }
                })?;
                Some(ServiceLocation {
                    identity: identity.clone(),
                    resource_uid: state.identity.resource_uid.clone(),
                    operation_id: record.intent.operation_id.clone(),
                    replica: state.identity.local_identity.clone(),
                    process_session_id: projection.process_session_id.clone(),
                    epoch: public_v1_role_epoch(&record.intent)
                        .expect("role operation epoch was filtered"),
                    revision: record.intent.revision,
                    address,
                })
            })
    } else {
        None
    };
    let mut progress = projection.progress;
    if projection.progress_history.as_ref() != Some(&history)
        || preview
            .data_loss_recovery
            .as_ref()
            .is_some_and(|recovery| history != recovery.proposed_history)
    {
        let pending = crate::protocol::public_operations::ProgressQueryResult::NotQueried(
            "native observations belong to a different or unresolved history".into(),
        );
        progress.current_progress = pending.clone();
        progress.catch_up_capability = pending;
    }
    let engine_idle = projection.engine_idle.filter(|sample| {
        let distinct: std::collections::BTreeSet<_> = sample.idle_replicas.iter().collect();
        let coherent = sample_safe
            && configuration.as_ref() == Some(&sample.configuration)
            && sample.history == history
            && sample.storage_incarnation == storage_incarnation
            && sample.process_session_id == projection.process_session_id
            && sample.replicator_creation_id == projection.replicator_creation_id
            && !preview.terminal
            && projection
                .observation_alive
                .as_ref()
                .is_some_and(|alive| alive())
            && distinct.len() == sample.idle_replicas.len()
            && sample.idle_replicas.iter().all(|id| {
                id.value() > 0
                    && !sample
                        .configuration
                        .current
                        .members
                        .iter()
                        .chain(
                            sample
                                .configuration
                                .previous
                                .iter()
                                .flat_map(|previous| &previous.members),
                        )
                        .any(|member| member.identity.replica_id == *id)
            });
        if !coherent {
            tracing::debug!("discarding incoherent or late engine idle sample");
        }
        coherent
    });
    let remote_builds = preview
        .remote_build_states(&projection.process_session_id)
        .into_iter()
        .map(|observation| {
            let build = observation.build;
            let input = preview
                .operations
                .get(&build.operation_id)
                .and_then(|record| match record.intent.program.as_ref() {
                    Some(PublicOperationProgram::Build(input)) => Some(input.clone()),
                    _ => None,
                })
                .ok_or_else(|| {
                    HostError::Corrupt("remote build lacks its exact source program".into())
                })?;
            Ok(
                crate::protocol::public_operations::PublicV1RemoteBuildObservation {
                    operation_id: build.operation_id,
                    revision: build.admission_revision,
                    build: input,
                    state: observation.state,
                },
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let report = PublicV1AgentReport {
        identity: identity.clone(),
        resource_uid: state.identity.resource_uid,
        replica: state.identity.local_identity,
        process_session_id: projection.process_session_id.clone(),
        replicator_creation_id: projection.replicator_creation_id,
        report_sequence: projection.report_sequence,
        role: projection.role,
        read_status: projection.read_status,
        write_status: projection.write_status,
        history: history.clone(),
        storage_incarnation: storage_incarnation.clone(),
        progress,
        configuration: configuration.map(|voting| {
            crate::protocol::public_operations::PublicV1InstalledConfiguration { voting }
        }),
        engine_idle,
        remote_builds,
        active_operations: preview
            .operations
            .values()
            .filter(|record| {
                record.stage == PublicOperationStage::Running
                    && record.superseded_by.is_none()
                    && record.intent.process_session_id == projection.process_session_id
                    && record.accepted_storage.as_ref() == Some(&storage_incarnation)
            })
            .map(|record| record.intent.clone())
            .collect(),
        retry_pending_operations: preview
            .operations
            .values()
            .filter(|record| {
                record.stage == PublicOperationStage::RetryPending
                    && record.superseded_by.is_none()
                    && record.intent.process_session_id == projection.process_session_id
                    && record.accepted_history.as_ref() == Some(&history)
                    && record.accepted_storage.as_ref() == Some(&storage_incarnation)
            })
            .map(|record| {
                let failure = record.retry_failure.as_ref().ok_or_else(|| {
                    HostError::Corrupt("retry-pending operation lacks callback provenance".into())
                })?;
                Ok(
                    crate::protocol::public_operations::PublicV1RetryPendingOperation {
                        intent: record.intent.clone(),
                        attempt: record.attempt,
                        failure: failure.error.clone(),
                    },
                )
            })
            .collect::<Result<Vec<_>>>()?,
        open_mode,
        replication_address,
        service_location,
        data_loss_recovery,
        completed_operations,
        extensions: crate::protocol::public_operations::PublicV1Extensions {
            catch_up_specific_quorum: projection.catch_up_specific_quorum,
            ..Default::default()
        },
    };
    report
        .validate()
        .map_err(|message| HostError::CommandRejected(message.into()))?;
    Ok(report)
}

pub(crate) fn completed_configuration(record: &PublicOperationRecord) -> bool {
    installed_configuration_from_record(record).is_some()
}

pub(crate) fn complete(record: &PublicOperationRecord) -> bool {
    (record.intent.lifecycle.is_none() && record.intent.program.is_none()) || {
        record.lifecycle.in_flight.is_none()
            && record.lifecycle.outcomes.len() == operation_instructions(&record.intent).len()
    }
}
