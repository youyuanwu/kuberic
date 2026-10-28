use super::*;
use crate::host::operation::PartitionOperation;
use crate::host::state::{
    PublicInstruction as I, PublicInstructionOutcome, PublicOperationDisposition,
    PublicOperationRecord, PublicOperationStage,
};
use crate::host::store::AgentStore;
use crate::protocol::public_operations::{
    PublicConfiguration, PublicLifecycleRecipe, PublicOperationProgram as P,
};
use crate::protocol::types::{HistoryContext, ReplicaStorageIncarnationContext};

type Status = (AccessStatus, AccessStatus);
const DENIED: Status = (AccessStatus::NotPrimary, AccessStatus::NotPrimary);
const PENDING: Status = (
    AccessStatus::ReconfigurationPending,
    AccessStatus::ReconfigurationPending,
);

type AccessContext = (
    HistoryContext,
    ReplicaStorageIncarnationContext,
    ReplicatorCreationIdentity,
);

struct PrimaryReconciliation {
    context: AccessContext,
    generation: u64,
}

#[derive(Default)]
pub(super) struct SelectedAccess {
    generation: u64,
    owner: Option<Weak<PartitionOperation>>,
    context: Option<AccessContext>,
    replicator_role: Option<ReplicaRole>,
    application_role: Option<ReplicaRole>,
    epoch: Option<Epoch>,
    promotion: bool,
    primary_reconciliation: Option<PrimaryReconciliation>,
    caught_up: bool,
    configuration: Option<PublicConfiguration>,
    local: Option<ReplicaIdentity>,
    minimum: usize,
    revoked: bool,
    demoting: bool,
    demoted: bool,
    joint_required: bool,
    valid: bool,
    active: bool,
    admitted: bool,
    addition_pending: bool,
    status: Option<Status>,
}

impl SelectedAccess {
    #[cfg(feature = "testing")]
    pub(super) fn addition_pending(&self) -> bool {
        self.addition_pending
    }

    pub(super) fn epoch(&self) -> Option<Epoch> {
        self.epoch
    }
    pub(super) fn initialized(&self) -> bool {
        self.status.is_some()
    }

    pub(super) fn invalidate(&mut self) {
        self.generation += 1;
        self.valid = false;
        self.replicator_role = None;
        self.application_role = None;
        self.primary_reconciliation = None;
        self.caught_up = false;
        self.configuration = None;
        self.status = Some(DENIED);
    }

    pub(super) fn revoke(&mut self) {
        self.revoked = true;
        self.status = Some(if self.valid && self.paired_primary() && !self.demoted {
            (
                self.status.unwrap_or(DENIED).0,
                AccessStatus::ReconfigurationPending,
            )
        } else {
            DENIED
        });
    }

    fn paired_primary(&self) -> bool {
        self.replicator_role == Some(ReplicaRole::Primary)
            && self.application_role == Some(ReplicaRole::Primary)
    }

    fn can_activate_primary(&self) -> bool {
        self.admitted
            && !self.demoted
            && self.paired_primary()
            && (!self.promotion || self.caught_up)
            && self.configuration.as_ref().is_some_and(|configuration| {
                Some(configuration.current.epoch) == self.epoch
                    && configuration.current.members.iter().any(|member| {
                        Some(&member.identity) == self.local.as_ref()
                            && member.identity.replica_id == configuration.current.primary_id
                            && member.role == ReplicaRole::Primary
                    })
            })
    }

    fn reserve_context(
        &mut self,
        history: Option<&HistoryContext>,
        storage: Option<&ReplicaStorageIncarnationContext>,
        creation: Option<ReplicatorCreationIdentity>,
    ) {
        if let Some(context) = &self.context
            && (history != Some(&context.0)
                || storage != Some(&context.1)
                || creation.is_some_and(|creation| creation != context.2))
        {
            *self = SelectedAccess {
                generation: self.generation,
                ..SelectedAccess::default()
            };
        }
        // Supersession retires the old owner, not compatible final-demotion
        // evidence. Rebind only that evidence, never promotion receipts.
        let reconciliation = self.primary_reconciliation.take().filter(|evidence| {
            evidence.generation == self.generation
                && Some(&evidence.context) == self.context.as_ref()
                && history == Some(&evidence.context.0)
                && storage == Some(&evidence.context.1)
                && creation == Some(evidence.context.2)
        });
        self.generation += 1;
        self.primary_reconciliation = reconciliation.map(|mut evidence| {
            evidence.generation = self.generation;
            evidence
        });
    }

    fn begin_promotion(&mut self) {
        self.demoted = false;
        self.promotion = true;
        self.replicator_role = None;
        self.application_role = None;
        self.epoch = None;
        self.configuration = None;
        self.joint_required = false;
        self.caught_up = false;
        self.status = Some(PENDING);
    }

    fn callback_succeeded(&mut self, instruction: I, program: Option<&P>) {
        match instruction {
            I::SecondCatchUp => {
                self.demoted = true;
                // Final-wait evidence is an obligation, not a role or access grant.
                self.primary_reconciliation =
                    self.context.clone().map(|context| PrimaryReconciliation {
                        context,
                        generation: self.generation,
                    });
                self.status = Some(DENIED);
            }
            I::ReplicatorRole
                if matches!(
                    program,
                    Some(P::Role { role, .. } | P::Swap { handoff: role, .. })
                        if *role != ReplicaRole::Primary
                ) =>
            {
                self.primary_reconciliation = None;
            }
            _ => {}
        }
    }

    fn same_primary(&self, configuration: &PublicConfiguration) -> bool {
        let local_primary = |set: &crate::protocol::types::ConfigurationDescriptor| {
            set.members.iter().any(|member| {
                Some(&member.identity) == self.local.as_ref()
                    && member.identity.replica_id == set.primary_id
                    && member.role == ReplicaRole::Primary
            })
        };
        local_primary(&configuration.current)
            && configuration.previous.as_ref().is_none_or(|previous| {
                previous.primary_id == configuration.current.primary_id && local_primary(previous)
            })
    }

    pub(super) fn status(&self) -> Option<Status> {
        self.status.map(|status| {
            if self.valid
                && self
                    .owner
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .is_some_and(|owner| owner.access_live())
            {
                if self.active || self.demoted {
                    status
                } else {
                    PENDING
                }
            } else {
                DENIED
            }
        })
    }

    pub(super) fn serving_role(&self) -> Option<ReplicaRole> {
        (self.paired_primary()
            && self.status().is_some_and(|(read, write)| {
                read == AccessStatus::Granted || write == AccessStatus::Granted
            }))
        .then_some(ReplicaRole::Primary)
    }

    fn calculate(&mut self) {
        self.status = Some(self.access());
    }

    fn access(&self) -> Status {
        if self.demoted || !self.admitted || self.addition_pending {
            return DENIED;
        }
        if self.promotion
            && (!self.paired_primary()
                || !self.caught_up
                || self
                    .configuration
                    .as_ref()
                    .is_some_and(|configuration| Some(configuration.current.epoch) != self.epoch))
        {
            return PENDING;
        }
        if !self.paired_primary() {
            return DENIED;
        }
        let Some(configuration) = &self.configuration else {
            return PENDING;
        };
        if self.joint_required
            && !self.demoting
            && !self.promotion
            && !self.same_primary(configuration)
        {
            return PENDING;
        }
        if !self.demoting
            && !configuration.current.members.iter().any(|member| {
                Some(&member.identity) == self.local.as_ref() && member.role == ReplicaRole::Primary
            })
        {
            return DENIED;
        }
        if self.revoked || self.promotion || Some(configuration.current.epoch) != self.epoch {
            (AccessStatus::Granted, AccessStatus::ReconfigurationPending)
        } else {
            (
                AccessStatus::Granted,
                quorum_status(
                    configuration,
                    self.local.as_ref(),
                    self.minimum,
                    self.joint_required,
                ),
            )
        }
    }
}

fn quorum_status(
    configuration: &PublicConfiguration,
    local: Option<&ReplicaIdentity>,
    minimum: usize,
    joint: bool,
) -> AccessStatus {
    let available = |set: &crate::protocol::types::ConfigurationDescriptor| {
        let voters = set
            .members
            .iter()
            .filter(|member| {
                matches!(
                    member.role,
                    ReplicaRole::Primary | ReplicaRole::ActiveSecondary
                )
            })
            .collect::<Vec<_>>();
        let ready = voters
            .iter()
            .filter(|member| {
                Some(&member.identity) == local
                    || configuration
                        .replicas
                        .iter()
                        .find(|peer| peer.identity == member.identity)
                        .is_some_and(|peer| peer.is_up && peer.is_ready)
            })
            .count();
        (voters.len(), ready)
    };
    let (size, ready) = available(&configuration.current);
    let previous = !joint
        || configuration.previous.as_ref().is_none_or(|previous| {
            let (size, ready) = available(previous);
            ready > size / 2
        });
    if size >= minimum && ready > size / 2 && previous {
        AccessStatus::Granted
    } else {
        AccessStatus::NoWriteQuorum
    }
}

pub(crate) struct AccessOwner {
    host: Weak<RuntimeHost>,
    operation: Weak<PartitionOperation>,
    generation: u64,
}

impl AccessOwner {
    pub(super) fn begin(
        host: Weak<RuntimeHost>,
        operation: &Arc<PartitionOperation>,
    ) -> Option<Arc<Self>> {
        operation.bind_access_owner(|| {
            let host = host.upgrade()?;
            let _admission = host.write_admission.lock().unwrap();
            let record = operation.snapshot();
            if !operation.access_live()
                || !matches!(
                    record.stage,
                    PublicOperationStage::Ready | PublicOperationStage::WaitingForContainment
                )
            {
                return None;
            }
            let mut access = host.selected_access.lock().unwrap();
            if access.valid
                && access
                    .owner
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .is_some_and(|owner| Arc::ptr_eq(&owner, operation))
            {
                return Some(Arc::new(Self {
                    host: Arc::downgrade(&host),
                    operation: Arc::downgrade(operation),
                    generation: access.generation,
                }));
            }
            if !record.intent.class.is_terminal()
                && access
                    .owner
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .is_some_and(|owner| owner.intent().revision > record.intent.revision)
            {
                return None;
            }
            let previously_valid = access.valid
                || access
                    .owner
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .is_some_and(|owner| {
                        let record = owner.snapshot();
                        record.stage == PublicOperationStage::Completed
                            && record.disposition == Some(PublicOperationDisposition::Succeeded)
                    });
            let creation = host.registered.get().map(|registered| registered.identity);
            access.reserve_context(
                record.accepted_history.as_ref(),
                record.accepted_storage.as_ref(),
                creation,
            );
            access.owner = Some(Arc::downgrade(operation));
            access.local = Some(host.identity.clone());
            access.valid = true;
            access.active = false;
            access.status = Some(PENDING);
            let selected = record
                .intent
                .goal()
                .and_then(super::super::public_lifecycle::program::ordinary_handoff)
                .or_else(|| record.intent.program.clone());
            access.demoting = matches!(selected, Some(P::Swap { .. }));
            if record.intent.class.is_terminal() {
                access.demoted = true;
                access.primary_reconciliation = None;
                access.status = Some(DENIED);
            } else {
                match selected.as_ref() {
                    Some(P::AgentGoal(goal)) if matches!(goal.desired,
                        crate::protocol::public_operations::ReplicaGoalKind::RefreshPrimary { .. }) =>
                    {
                        let retained = matches!(goal.desired,
                            crate::protocol::public_operations::ReplicaGoalKind::RefreshPrimary {
                                catch_up: Some(crate::protocol::public_operations::PublicCatchUpMode::WriteQuorum),
                            }) && goal.scope.configuration.previous.is_none()
                            && previously_valid && access.caught_up
                            && access.configuration.as_ref().is_some_and(|prior| {
                                prior.previous.is_some()
                                    && prior.current == goal.scope.configuration.current
                            });
                        access.revoked = true;
                        access.caught_up = retained;
                    }
                    Some(P::AgentGoal(goal)) if goal.role() == Some(ReplicaRole::Primary) => {
                        let addition = super::super::ordinary_goal::addition::selection(goal);
                        if addition.is_some_and(|addition| matches!(addition.phase,
                            crate::protocol::public_operations::MembershipAdditionPhase::Joint
                                | crate::protocol::public_operations::MembershipAdditionPhase::Current))
                        {
                            access.promotion = true;
                            access.revoked = true;
                            access.caught_up = false;
                        } else {
                            access.begin_promotion();
                        }
                        access.addition_pending = access.addition_pending
                            || addition.is_some()
                            || host.native_history.is_native() && matches!(goal.desired,
                                crate::protocol::public_operations::ReplicaGoalKind::Reconfigure {
                                    role: ReplicaRole::Primary, catch_up: None
                                }) && goal.scope.configuration.previous.as_ref().is_some_and(|previous|
                                    previous.primary_id != goal.scope.configuration.current.primary_id);
                    }
                    Some(P::Role {
                        role: ReplicaRole::Primary,
                        ..
                    }) => {
                        if access.promotion
                            || access.primary_reconciliation.is_some()
                            || access.demoted
                            || access.revoked
                            || matches!(
                                access.application_role,
                                Some(ReplicaRole::ActiveSecondary | ReplicaRole::IdleSecondary)
                            )
                        {
                            access.begin_promotion();
                        }
                    }
                    Some(P::Swap {
                        controller_composed,
                        starting,
                        ..
                    }) => {
                        access.revoked = *controller_composed;
                        if *controller_composed
                            && access.configuration.as_ref() == Some(starting)
                            && previously_valid
                            && access.paired_primary()
                        {
                            access.calculate();
                        } else if !controller_composed
                            && previously_valid
                            && access.paired_primary()
                        {
                            access.status =
                                Some((AccessStatus::Granted, AccessStatus::ReconfigurationPending));
                        }
                    }
                    Some(P::Epoch { .. } | P::Configuration(_) | P::CatchUp { .. })
                        if previously_valid =>
                    {
                        access.calculate();
                    }
                    _ => {}
                }
                if record
                    .intent
                    .lifecycle
                    .as_ref()
                    .is_some_and(|input| input.recipe == PublicLifecycleRecipe::FailoverPromotion)
                {
                    access.begin_promotion();
                }
            }
            host.replacement_authority_fenced
                .store(true, Ordering::Release);
            Some(Arc::new(Self {
                host: Arc::downgrade(&host),
                operation: Arc::downgrade(operation),
                generation: access.generation,
            }))
        })
    }

    fn update(&self, update: impl FnOnce(&mut SelectedAccess, &PublicOperationIntent)) {
        let (Some(host), Some(operation)) = (self.host.upgrade(), self.operation.upgrade()) else {
            return;
        };
        let _admission = host.write_admission.lock().unwrap();
        let mut access = host.selected_access.lock().unwrap();
        if access.generation == self.generation && access.valid && operation.access_live() {
            update(&mut access, &operation.intent());
        }
    }

    pub(crate) fn selected(&self, instruction: I) {
        self.update(|access, _| match instruction {
            I::ReplicatorRole | I::ReplicatorPrimary => {
                access.replicator_role = None;
                if access.promotion {
                    access.caught_up = false;
                }
                access.calculate();
            }
            I::ApplicationRole | I::ApplicationPrimary => {
                access.application_role = None;
                access.calculate();
            }
            I::Revoke => access.revoke(),
            I::Configuration
            | I::StartingConfiguration
            | I::RefreshedConfiguration
            | I::CurrentConfiguration
            | I::CatchUpConfiguration
            | I::ProgramEpoch
            | I::Epoch => {
                if access.promotion && matches!(instruction, I::ProgramEpoch | I::Epoch) {
                    access.caught_up = false;
                    access.calculate();
                }
                if access
                    .status
                    .is_some_and(|status| status.0 == AccessStatus::Granted)
                {
                    access.status =
                        Some((AccessStatus::Granted, AccessStatus::ReconfigurationPending));
                }
            }
            _ => {}
        });
    }

    pub(crate) fn callback_succeeded(&self, instruction: I) {
        self.update(|access, intent| {
            let selected =
                super::super::public_lifecycle::program::selected_program(intent, instruction);
            access.callback_succeeded(instruction, selected.as_ref())
        });
    }

    fn activate(&self) {
        self.update(|access, _| access.active = true);
    }

    async fn compatible(
        &self,
        store: &dyn AgentStore,
    ) -> crate::host::Result<crate::host::state::AgentState> {
        let state = store.load_state().await?;
        let operation = self
            .operation
            .upgrade()
            .ok_or_else(|| crate::host::HostError::CommandRejected("access owner lost".into()))?;
        let record = operation.snapshot();
        let preview = state.public_v1_operation_state.as_ref().ok_or_else(|| {
            crate::host::HostError::CommandRejected("selected access state missing".into())
        })?;
        let recovered_history = record.intent.lifecycle.as_ref()
            .and_then(|input| input.data_loss_recovery.as_ref())
            .is_some_and(|input| {
                preview.history.as_ref() == Some(&input.authorization.proposed)
                    && preview.data_loss_recovery.as_ref().is_some_and(|recovery| {
                        recovery.attempt == input.attempt
                            && recovery.stage == crate::protocol::public_operations::PublicDataLossRecoveryStage::Admitted
                    })
            });
        if !operation.access_live()
            || (record.accepted_history != preview.history && !recovered_history)
            || record.accepted_storage != preview.storage_incarnation
            || record.intent.identity != preview.identity
        {
            self.invalidate();
            return Err(crate::host::HostError::CommandRejected(
                "selected access context changed".into(),
            ));
        }
        self.update(|access, _| {
            access.admitted = preview.history_barriers.is_empty()
                && preview.history.is_some()
                && preview
                    .storage_incarnation
                    .as_ref()
                    .is_some_and(|storage| storage.replica == state.identity.local_identity)
                && !crate::host::native_transport::recovery_observations_pending(&state);
            if !access.admitted {
                access.status = Some(DENIED);
            }
        });
        Ok(state)
    }

    pub(crate) async fn reinstalled(
        &self,
        store: &dyn AgentStore,
        configuration: &PublicConfiguration,
    ) -> crate::host::Result<()> {
        self.compatible(store).await?;
        self.update(|access, _| {
            access.configuration = Some(configuration.clone());
            access.joint_required = configuration.previous.is_some();
            if access.promotion {
                access.caught_up = false;
            }
            access.calculate();
        });
        Ok(())
    }

    pub(crate) async fn restore_retry_prefix(
        &self,
        store: &dyn AgentStore,
        failed: &PublicOperationRecord,
    ) -> crate::host::Result<()> {
        let state = self.compatible(store).await?;
        let operation = self
            .operation
            .upgrade()
            .ok_or_else(|| crate::host::HostError::CommandRejected("access owner lost".into()))?;
        let current = operation.snapshot();
        let instructions = crate::host::public_lifecycle::record_instructions(failed);
        let retained = current.attempt_results.get(&failed.attempt);
        let durable = state
            .public_v1_operation_state
            .as_ref()
            .and_then(|preview| preview.operations.get(&failed.intent.operation_id));
        if failed.stage != PublicOperationStage::RetryPending
            || failed.attempt.checked_add(1) != Some(current.attempt)
            || current.stage != PublicOperationStage::Running
            || current.intent != failed.intent
            || current.accepted_history != failed.accepted_history
            || current.accepted_storage != failed.accepted_storage
            || current.lifecycle.outcomes != failed.lifecycle.outcomes
            || current.lifecycle.in_flight.is_some()
            || durable != Some(&current)
            || retained.is_none_or(|result| {
                result.stage != PublicOperationStage::RetryPending
                    || Some(&result.disposition) != failed.disposition.as_ref()
                    || result.lifecycle != failed.lifecycle
                    || result.retry_failure != failed.retry_failure
            })
            || failed.retry_failure.as_ref().is_none_or(|failure| {
                instructions.get(failed.lifecycle.outcomes.len()) != Some(&failure.instruction)
                    || failed.lifecycle.in_flight != Some(failure.instruction)
                    || !failure.family.allows(failure.instruction)
            })
            || failed.lifecycle.outcomes.iter().any(|outcome| {
                matches!(
                    outcome,
                    PublicInstructionOutcome::DataLoss(_)
                        | PublicInstructionOutcome::CloseFailure { .. }
                        | PublicInstructionOutcome::RebuildRequired(_)
                )
            })
        {
            self.invalidate();
            return Err(crate::host::HostError::StaleEffectCompletion(
                "access retry prefix differs from its exact retained attempt".into(),
            ));
        }
        // Only the just-resumed attempt supplies receipts. In particular, promotion
        // reservation cleared old catch-up; no other operation can restore it.
        for instruction in instructions
            .into_iter()
            .take(failed.lifecycle.outcomes.len())
        {
            self.selected(instruction);
            self.callback_succeeded(instruction);
            self.receipt(store, instruction).await?;
        }
        Ok(())
    }

    pub(crate) async fn receipt(
        &self,
        store: &dyn AgentStore,
        instruction: I,
    ) -> crate::host::Result<()> {
        let state = self.compatible(store).await?;
        let Some(host) = self.host.upgrade() else {
            return Ok(());
        };
        let Some(registered) = host.registered.get() else {
            return Ok(());
        };
        let preview = state.public_v1_operation_state.as_ref().unwrap();
        let context = preview
            .history
            .clone()
            .zip(preview.storage_incarnation.clone())
            .map(|(history, storage)| (history, storage, registered.identity));
        let sourced_current = self.operation.upgrade().is_some_and(|operation| {
            let record = operation.snapshot();
            record.intent.goal().is_some_and(|goal| {
                (super::super::original_member::round(&state, goal)
                    || super::super::ordinary_goal::normal_current_owner(&state, goal).is_some())
                    && record.ordinary.as_ref().is_some_and(|ordinary| {
                        ordinary.actions == super::super::original_member::final_actions(goal)
                            && ordinary.actions.contains(&I::CurrentConfiguration)
                    })
            })
        });
        self.update(|access, intent| {
            let selected =
                super::super::public_lifecycle::program::selected_program(intent, instruction);
            access.context = context;
            access.minimum = state.identity.effective_policy.write_quorum as usize;
            let role = match selected.as_ref() {
                Some(P::Role { role, .. }) => *role,
                Some(P::Swap { handoff, .. }) => *handoff,
                _ => ReplicaRole::Primary,
            };
            match instruction {
                I::ReplicatorRole | I::ReplicatorPrimary => {
                    access.replicator_role = Some(role);
                    access.epoch = intent.lifecycle.as_ref().map(|input| input.epoch).or(
                        match selected.as_ref() {
                            Some(P::Role { epoch, .. } | P::Swap { epoch, .. }) => Some(*epoch),
                            _ => None,
                        },
                    );
                }
                I::ApplicationRole | I::ApplicationPrimary => access.application_role = Some(role),
                I::ProgramEpoch | I::Epoch => {
                    access.epoch = intent.lifecycle.as_ref().map(|input| input.epoch).or(
                        match selected.as_ref() {
                            Some(P::Epoch { epoch } | P::Swap { epoch, .. }) => Some(*epoch),
                            _ => None,
                        },
                    );
                }
                I::Configuration
                | I::StartingConfiguration
                | I::RefreshedConfiguration
                | I::CurrentConfiguration
                | I::CatchUpConfiguration => {
                    let configuration =
                        super::super::public_action_state::configuration_at(intent, instruction);
                    let retained_catchup = (instruction == I::Configuration
                        || instruction == I::CurrentConfiguration && sourced_current)
                        && access.caught_up
                        && configuration.as_ref().is_some_and(|next| {
                            next.previous.is_none()
                                && access.configuration.as_ref().is_some_and(|old| {
                                    old.current == next.current
                                        && next.replicas.iter().all(|peer| {
                                            old.replicas.iter().any(|previous| {
                                                previous.identity == peer.identity
                                                    && previous.process_session_id
                                                        == peer.process_session_id
                                                    && previous.storage_incarnation
                                                        == peer.storage_incarnation
                                                    && previous.replication_address
                                                        == peer.replication_address
                                                    && previous.role == peer.role
                                            })
                                        })
                                })
                        });
                    access.configuration = configuration;
                    access.joint_required = access
                        .configuration
                        .as_ref()
                        .is_some_and(|configuration| configuration.previous.is_some());
                    if access.promotion && !retained_catchup {
                        access.caught_up = false;
                    }
                }
                I::FirstCatchUp | I::CatchUp => access.caught_up = true,
                _ => {}
            }
            access.calculate();
        });
        Ok(())
    }

    pub(crate) fn invalidate(&self) {
        if let Some(host) = self.host.upgrade() {
            let _admission = host.write_admission.lock().unwrap();
            let mut access = host.selected_access.lock().unwrap();
            if access.generation != self.generation {
                return;
            }
            access.valid = false;
            access.status = Some(DENIED);
        }
    }
}

pub(crate) struct AccessRootGuard {
    owner: Option<Arc<AccessOwner>>,
}

impl AccessRootGuard {
    pub(crate) fn new(owner: Option<Arc<AccessOwner>>) -> Self {
        if let Some(owner) = &owner {
            owner.activate();
        }
        Self { owner }
    }
    pub(crate) fn succeeded(&mut self) {
        self.owner = None;
    }
}

impl Drop for AccessRootGuard {
    fn drop(&mut self) {
        if let Some(owner) = &self.owner {
            owner.invalidate();
        }
    }
}

pub(super) fn complete(host: &Arc<RuntimeHost>, operation: &Arc<PartitionOperation>) {
    let _admission = host.write_admission.lock().unwrap();
    let mut access = host.selected_access.lock().unwrap();
    if !access.valid
        || !operation.access_live()
        || !access
            .owner
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|owner| Arc::ptr_eq(&owner, operation))
    {
        return;
    }

    let record = operation.snapshot();
    if record.stage != PublicOperationStage::Completed
        || record.disposition != Some(PublicOperationDisposition::Succeeded)
    {
        return;
    }
    let activates = match record.intent.program.as_ref() {
        Some(P::AgentGoal(goal)) => {
            let reconstructed = matches!(&goal.desired,
                crate::protocol::public_operations::ReplicaGoalKind::RecoverContainedCreation { recovery }
                    if recovery.role == ReplicaRole::Primary && recovery.catch_up.is_some())
                && host
                    .reconstructed_native_initialized
                    .get()
                    .is_some_and(|initialized| initialized.scope() == &goal.scope)
                && record.lifecycle.outcomes.iter().any(|outcome| {
                    matches!(outcome,
                    PublicInstructionOutcome::ReconstructedNativeInitialized(scope)
                        if scope.as_ref() == &goal.scope)
                });
            (reconstructed
                || super::super::ordinary_goal::addition::selection(goal).is_some_and(|addition| {
                    addition.phase
                        == crate::protocol::public_operations::MembershipAdditionPhase::Current
                })
                || matches!(
                    goal.desired,
                    crate::protocol::public_operations::ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::Primary,
                        catch_up: Some(_),
                    } | crate::protocol::public_operations::ReplicaGoalKind::RefreshPrimary {
                        catch_up: Some(_),
                    }
                ))
                && goal.scope.configuration.previous.is_none()
                && access.caught_up
                && record.ordinary.as_ref().is_some_and(|ordinary| {
                    (ordinary.actions
                        == super::super::public_lifecycle::operation_instructions(&record.intent)
                        || super::super::original_member::finished_current(&record))
                        && record.lifecycle.outcomes.len() == ordinary.actions.len()
                        && record.lifecycle.in_flight.is_none()
                })
        }
        Some(P::Configuration(configuration)) => {
            (!access.promotion || access.caught_up) && configuration.previous.is_none()
        }
        Some(P::CatchUp { configuration, .. }) => {
            access.caught_up && configuration.previous.is_none()
        }
        _ => record.intent.lifecycle.as_ref().is_some_and(|input| {
            matches!(
                input.recipe,
                PublicLifecycleRecipe::InitialPrimary | PublicLifecycleRecipe::FailoverPromotion
            )
        }),
    };
    if activates && access.can_activate_primary() {
        access.promotion = false;
        access.primary_reconciliation = None;
        access.revoked = false;
        if record.intent.lifecycle.is_some() {
            access.joint_required = false;
        }
    }
    access.calculate();
    host.replacement_authority_fenced
        .store(false, Ordering::Release);
}

pub(super) async fn accept_addition(
    host: &Arc<RuntimeHost>,
    store: &dyn AgentStore,
    goal: &crate::protocol::public_operations::ReplicaGoal,
) -> crate::host::Result<()> {
    let state = store.load_state().await?;
    let original = super::super::original_member::successful_final(&state, goal);
    if !original
        && super::super::ordinary_goal::addition::selection(goal).is_none_or(|addition| {
            addition.phase != crate::protocol::public_operations::MembershipAdditionPhase::Current
        })
    {
        return Err(crate::host::HostError::CommandRejected(
            "only final addition acceptance can release application access".into(),
        ));
    }
    if !original {
        super::super::ordinary_goal::addition::validate(&state, goal)?;
    }
    let accepted = original
        || state
            .public_v1_operation_state
            .as_ref()
            .and_then(|preview| preview.operations.get(&goal.operation_id))
            .is_some_and(|record| {
                record.intent == goal.intent()
                    && super::super::ordinary_goal::addition::successful_final(record, true)
            });
    if !accepted {
        return Err(crate::host::HostError::CommandRejected(
            "addition final acceptance missing".into(),
        ));
    }
    let owner = {
        let _admission = host.write_admission.lock().unwrap();
        let mut access = host.selected_access.lock().unwrap();
        let owner = access
            .owner
            .as_ref()
            .and_then(Weak::upgrade)
            .filter(|owner| owner.intent().goal() == Some(goal))
            .ok_or_else(|| {
                crate::host::HostError::CommandRejected("addition lost final access owner".into())
            })?;
        access.addition_pending = false;
        owner
    };
    complete(host, &owner);
    Ok(())
}

pub(super) fn completion_guard(
    host: &Arc<RuntimeHost>,
    operation: &Arc<PartitionOperation>,
) -> AccessRootGuard {
    let access = host.selected_access.lock().unwrap();
    AccessRootGuard {
        owner: access
            .owner
            .as_ref()
            .and_then(Weak::upgrade)
            .filter(|owner| Arc::ptr_eq(owner, operation))
            .map(|_| {
                Arc::new(AccessOwner {
                    host: Arc::downgrade(host),
                    operation: Arc::downgrade(operation),
                    generation: access.generation,
                })
            }),
    }
}

#[cfg(all(test, feature = "testing"))]
#[path = "public_lifecycle_access_retry_tests.rs"]
mod access_retry_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::*;

    fn demotion_evidence() -> SelectedAccess {
        let context = (
            HistoryContext {
                resource_uid: ResourceUid::new("lineage-resource"),
                protocol_generation: PublicProtocolGeneration::new(1),
                history_id: HistoryId::new("lineage-history"),
                data_loss_number: 0,
            },
            ReplicaStorageIncarnationContext {
                replica: ReplicaIdentity {
                    replica_id: ReplicaId::new(1),
                    instance_id: ReplicaInstanceId::new("lineage-instance"),
                    agent_generation: AgentGeneration::new("lineage-agent"),
                },
                storage: ReplicaStorageIdentity::Volatile {
                    identity: "lineage-storage".into(),
                },
                storage_generation: StorageGeneration::new("lineage-generation"),
            },
            ReplicatorCreationIdentity::new(RuntimeHostToken::new()),
        );
        SelectedAccess {
            generation: 7,
            context: Some(context.clone()),
            primary_reconciliation: Some(PrimaryReconciliation {
                context,
                generation: 7,
            }),
            demoted: true,
            admitted: true,
            ..SelectedAccess::default()
        }
    }

    fn primary_receipts() -> SelectedAccess {
        let mut access = demotion_evidence();
        let local = access.context.as_ref().unwrap().1.replica.clone();
        let epoch = Epoch::new(0, 3);
        access.demoted = false;
        access.local = Some(local.clone());
        access.replicator_role = Some(ReplicaRole::Primary);
        access.application_role = Some(ReplicaRole::Primary);
        access.epoch = Some(epoch);
        access.caught_up = true;
        access.configuration = Some(PublicConfiguration {
            current: ConfigurationDescriptor::new(
                epoch,
                local.replica_id,
                vec![ConfigurationMember {
                    identity: local,
                    role: ReplicaRole::Primary,
                }],
                1,
            ),
            previous: None,
            replicas: vec![],
        });
        access
    }

    #[test]
    fn serving_report_role_requires_actual_live_access_owner_not_only_role_receipts() {
        let mut access = primary_receipts();
        access.valid = true;
        access.active = true;
        access.status = Some((AccessStatus::Granted, AccessStatus::Granted));
        assert_eq!(access.serving_role(), None);
        access.replicator_role = None;
        assert_eq!(access.serving_role(), None);
        access.replicator_role = Some(ReplicaRole::Primary);
        access.application_role = Some(ReplicaRole::ActiveSecondary);
        assert_eq!(access.serving_role(), None);
        access.invalidate();
        assert_eq!(access.serving_role(), None);
    }

    #[test]
    fn primary_reconciliation_rebinds_only_exact_context_and_generation() {
        let mut compatible = demotion_evidence();
        let context = compatible.context.clone().unwrap();
        for generation in [8, 9, 10] {
            compatible.reserve_context(Some(&context.0), Some(&context.1), Some(context.2));
            let evidence = compatible.primary_reconciliation.as_ref().unwrap();
            assert_eq!(compatible.generation, generation);
            assert_eq!(evidence.generation, generation);
            assert_eq!(evidence.context, context);
        }
        for crossed in 0..8 {
            let mut access = demotion_evidence();
            let mut context = access.context.clone().unwrap();
            match crossed {
                0 => context.0.history_id = HistoryId::new("other-history"),
                1 => context.1.storage_generation = StorageGeneration::new("other-storage"),
                2 => context.2 = ReplicatorCreationIdentity::new(RuntimeHostToken::new()),
                3 => access.primary_reconciliation.as_mut().unwrap().generation -= 1,
                7 => access.context = None,
                _ => {}
            }
            access.reserve_context(
                (crossed != 4).then_some(&context.0),
                (crossed != 5).then_some(&context.1),
                (crossed != 6).then_some(context.2),
            );
            assert!(access.primary_reconciliation.is_none(), "crossed {crossed}");
        }
    }

    #[test]
    fn promotion_reservation_preserves_evidence_not_serving_receipts() {
        let mut access = primary_receipts();
        access.demoted = true;
        access.begin_promotion();
        assert!(!access.demoted);
        assert!(access.primary_reconciliation.is_some());
        assert!(access.replicator_role.is_none());
        assert!(access.application_role.is_none());
        assert!(access.epoch.is_none());
        assert!(access.configuration.is_none());
        assert!(!access.caught_up);
        assert_eq!(access.access(), PENDING);
        access.invalidate();
        assert!(access.primary_reconciliation.is_none());
        assert_eq!(access.status(), Some(DENIED));
    }

    #[test]
    fn reconciliation_retires_on_native_nonprimary_callback_not_other_receipts() {
        for role in [
            ReplicaRole::ActiveSecondary,
            ReplicaRole::IdleSecondary,
            ReplicaRole::None,
        ] {
            for swap in [false, true] {
                let mut access = primary_receipts();
                let configuration = access.configuration.clone().unwrap();
                let epoch = configuration.current.epoch;
                let program = if swap {
                    P::Swap {
                        starting: configuration.clone(),
                        refreshed: configuration,
                        epoch,
                        handoff: role,
                        catch_up_specific_quorum: true,
                        mode: crate::protocol::public_operations::PublicCatchUpMode::WriteQuorum,
                        controller_composed: true,
                    }
                } else {
                    P::Role { epoch, role }
                };
                for instruction in [
                    I::ApplicationRole,
                    I::Configuration,
                    I::FirstCatchUp,
                    I::ProgramEpoch,
                ] {
                    access.callback_succeeded(instruction, Some(&program));
                    assert!(access.primary_reconciliation.is_some());
                }
                access.callback_succeeded(
                    I::ReplicatorRole,
                    Some(&P::Role {
                        epoch,
                        role: ReplicaRole::Primary,
                    }),
                );
                assert!(access.primary_reconciliation.is_some());
                access.callback_succeeded(I::ReplicatorRole, Some(&program));
                assert!(access.primary_reconciliation.is_none());
            }
        }
    }

    #[test]
    fn activation_requires_current_primary_role_epoch_configuration_and_catchup() {
        let mut initial = primary_receipts();
        initial.caught_up = false;
        assert!(initial.can_activate_primary());
        for crossed in 0..11 {
            let mut access = primary_receipts();
            access.promotion = true;
            assert!(access.can_activate_primary());
            match crossed {
                0 => access.demoted = true,
                1 => access.replicator_role = None,
                2 => access.application_role = Some(ReplicaRole::ActiveSecondary),
                3 => access.epoch = Some(Epoch::new(0, 2)),
                4 => access.configuration = None,
                5 => access.local = None,
                6 => access.configuration.as_mut().unwrap().current.primary_id = ReplicaId::new(2),
                7 => {
                    access.configuration.as_mut().unwrap().current.members[0].role =
                        ReplicaRole::ActiveSecondary
                }
                8 => access.caught_up = false,
                9 => access.epoch = None,
                10 => access.admitted = false,
                _ => unreachable!(),
            }
            assert!(!access.can_activate_primary(), "crossed {crossed}");
            assert!(access.primary_reconciliation.is_some());
        }
    }
}
