use super::*;
use crate::host::operation::PartitionOperation;
use crate::host::state::{
    PublicInstruction as I, PublicOperationDisposition, PublicOperationStage,
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

#[derive(Default)]
pub(super) struct SelectedAccess {
    generation: u64,
    owner: Option<Weak<PartitionOperation>>,
    context: Option<(
        HistoryContext,
        ReplicaStorageIncarnationContext,
        ReplicatorCreationIdentity,
    )>,
    replicator_role: Option<ReplicaRole>,
    application_role: Option<ReplicaRole>,
    epoch: Option<Epoch>,
    promotion: bool,
    caught_up: bool,
    configuration: Option<PublicConfiguration>,
    local: Option<ReplicaIdentity>,
    minimum: usize,
    revoked: bool,
    demoted: bool,
    joint_required: bool,
    valid: bool,
    active: bool,
    admitted: bool,
    status: Option<Status>,
}

impl SelectedAccess {
    pub(super) fn initialized(&self) -> bool {
        self.status.is_some()
    }

    pub(super) fn invalidate(&mut self) {
        self.generation += 1;
        self.valid = false;
        self.replicator_role = None;
        self.application_role = None;
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

    fn calculate(&mut self) {
        self.status = Some(self.access());
    }

    fn access(&self) -> Status {
        if self.demoted || !self.admitted {
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
        if !self.revoked
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
            if let Some(context) = &access.context
                && (record.accepted_history.as_ref() != Some(&context.0)
                    || record.accepted_storage.as_ref() != Some(&context.1)
                    || creation.is_some_and(|creation| creation != context.2))
            {
                *access = SelectedAccess {
                    generation: access.generation,
                    ..SelectedAccess::default()
                };
            }
            access.generation += 1;
            access.owner = Some(Arc::downgrade(operation));
            access.local = Some(host.identity.clone());
            access.valid = true;
            access.active = false;
            access.status = Some(PENDING);
            if record.intent.class.is_terminal() {
                access.demoted = true;
                access.status = Some(DENIED);
            } else {
                match record.intent.program.as_ref() {
                    Some(P::Role {
                        role: ReplicaRole::Primary,
                        ..
                    }) => {
                        access.promotion |= matches!(
                            access.application_role,
                            Some(ReplicaRole::ActiveSecondary | ReplicaRole::IdleSecondary)
                        );
                    }
                    Some(P::Swap {
                        controller_composed,
                        starting,
                        ..
                    }) => {
                        access.demoted = false;
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
                    access.promotion = true;
                    access.caught_up = false;
                    access.status = Some(PENDING);
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
        if instruction == I::SecondCatchUp {
            self.update(|access, _| {
                access.demoted = true;
                access.status = Some(DENIED);
            });
        }
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
        self.update(|access, intent| {
            access.context = context;
            access.minimum = state.identity.effective_policy.write_quorum as usize;
            let role = match intent.program.as_ref() {
                Some(P::Role { role, .. }) => *role,
                Some(P::Swap { handoff, .. }) => *handoff,
                _ => ReplicaRole::Primary,
            };
            match instruction {
                I::ReplicatorRole | I::ReplicatorPrimary => {
                    access.replicator_role = Some(role);
                    access.epoch = intent.lifecycle.as_ref().map(|input| input.epoch).or(
                        match intent.program.as_ref() {
                            Some(P::Role { epoch, .. } | P::Swap { epoch, .. }) => Some(*epoch),
                            _ => None,
                        },
                    );
                }
                I::ApplicationRole | I::ApplicationPrimary => access.application_role = Some(role),
                I::ProgramEpoch | I::Epoch => {
                    access.epoch = intent.lifecycle.as_ref().map(|input| input.epoch).or(
                        match intent.program.as_ref() {
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
                    let configuration = match intent.program.as_ref() {
                        Some(
                            P::Configuration(configuration) | P::CatchUp { configuration, .. },
                        ) => Some(configuration.clone()),
                        Some(P::Swap {
                            starting,
                            refreshed,
                            ..
                        }) => Some(
                            if instruction == I::StartingConfiguration {
                                starting
                            } else {
                                refreshed
                            }
                            .clone(),
                        ),
                        _ => intent.lifecycle.as_ref().map(|input| input.configuration()),
                    };
                    let retained_catchup = instruction == I::Configuration
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
        Some(P::Configuration(configuration)) => {
            (!access.promotion || access.caught_up) && configuration.previous.is_none()
        }
        Some(P::CatchUp { configuration, .. }) => {
            access.caught_up && configuration.previous.is_none()
        }
        _ => record.intent.lifecycle.is_some(),
    };
    if activates && access.paired_primary() {
        access.promotion = false;
        access.revoked = false;
        if record.intent.lifecycle.is_some() {
            access.joint_required = false;
        }
    }
    access.calculate();
    host.replacement_authority_fenced
        .store(false, Ordering::Release);
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
