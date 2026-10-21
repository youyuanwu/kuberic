//! Public V1 application and Replicator hosting.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};

use async_trait::async_trait;
use tokio::sync::{Mutex, RwLock};

use crate::application::{OpenContext, OpenMode, StateProvider, StatefulServiceReplica};
use crate::capabilities::{ReplicatorCreationIdentity, RuntimeHostToken};
use crate::protocol::public_operations::{
    PublicOperationClass, PublicOperationIntent, PublicV1Identity,
};
use crate::protocol::types::{
    AccessStatus, Epoch, FaultType, LoadMetric, OperationId, PartitionId, PartitionInformation,
    ProcessSessionId, ReplicaIdentity, ReplicaRole,
};
use crate::replicator::{
    PartitionAccessView, PrimaryReplicator, Replicator, ReplicatorAttachment,
    ReplicatorCapabilities, ReplicatorCreationReservation, ReplicatorFactoryContext,
    ReplicatorRegistration, StatefulServicePartition,
};
use crate::{Result, RuntimeError};

pub(crate) struct PodRuntime {
    host: Arc<RuntimeHost>,
}

#[cfg(kuberic_workspace_tests)]
pub(crate) const PUBLIC_V1_HOST_ATTACHMENTS: &[&str] = &[
    "PrimaryReplicator",
    "Replicator",
    "StateProvider",
    "NativeRuntimeControl",
];

#[derive(Debug)]
struct ProviderWriterLease {
    consumed: bool,
}

enum ReplicatorCreationState {
    Available(ReplicatorCreationIdentity),
    Reserved {
        identity: ReplicatorCreationIdentity,
        provider_writer: ProviderWriterLease,
    },
    Registered,
}

struct RegisteredReplicator {
    control: Arc<dyn Replicator>,
    primary: Option<Arc<dyn PrimaryReplicator>>,
    identity: ReplicatorCreationIdentity,
    callback_containment: Option<tokio::sync::watch::Receiver<bool>>,
    native_runtime_control: Option<Arc<dyn crate::replicator::NativeRuntimeControl>>,
    capabilities: ReplicatorCapabilities,
    provider_writer_lease_consumed: Arc<AtomicBool>,
}

impl RegisteredReplicator {
    fn primary(&self) -> Option<Arc<dyn PrimaryReplicator>> {
        self.primary.clone()
    }

    async fn native_progress(
        &self,
    ) -> (
        crate::protocol::public_operations::ProgressQueryResult,
        crate::protocol::public_operations::ProgressQueryResult,
    ) {
        use crate::protocol::public_operations::ProgressQueryResult as Query;
        if let Some(runtime) = &self.native_runtime_control {
            match runtime.progress_query_readiness() {
                Ok(true) => {}
                Ok(false) => {
                    return (
                        Query::NotQueried("native copy is incomplete".into()),
                        Query::NotQueried("native copy is incomplete".into()),
                    );
                }
                Err(error) => {
                    return (
                        Query::Failed(error.to_string()),
                        Query::Failed(error.to_string()),
                    );
                }
            }
        }
        fn observe(result: Result<i64>) -> Query {
            match result {
                Ok(value) => Query::Value(value),
                Err(RuntimeError::ProgressNotReady) => Query::NotReady,
                Err(error) => Query::Failed(error.to_string()),
            }
        }
        (
            observe(self.control.current_progress().await),
            observe(self.control.catch_up_capability().await),
        )
    }

    fn abort(&self) {
        self.control.abort();
    }
}

#[derive(Clone)]
struct HostProjection {
    open: bool,
    replication_address: Option<String>,
    role: ReplicaRole,
    read_status: AccessStatus,
    write_status: AccessStatus,
    current_progress: crate::protocol::public_operations::ProgressQueryResult,
    catch_up_capability: crate::protocol::public_operations::ProgressQueryResult,
}

fn renewal_projection_is_current(
    renewal_epoch: Epoch,
    renewal_role: ReplicaRole,
    epoch: Option<Epoch>,
    role: Option<(Epoch, ReplicaRole)>,
    configuration: Option<&crate::protocol::public_operations::PublicConfiguration>,
) -> bool {
    epoch.is_none_or(|epoch| epoch <= renewal_epoch)
        && role.is_none_or(|(epoch, role)| {
            epoch < renewal_epoch || (epoch == renewal_epoch && role == renewal_role)
        })
        && configuration.is_none_or(|configuration| configuration.current.epoch <= renewal_epoch)
}

impl HostProjection {
    fn new() -> Self {
        Self {
            open: false,
            replication_address: None,
            role: ReplicaRole::None,
            read_status: AccessStatus::NotPrimary,
            write_status: AccessStatus::NotPrimary,
            current_progress: crate::protocol::public_operations::ProgressQueryResult::NotReady,
            catch_up_capability: crate::protocol::public_operations::ProgressQueryResult::NotReady,
        }
    }
}

struct HostState {
    projection: HostProjection,
    partition_information: PartitionInformation,
    load_metrics: BTreeMap<String, i64>,
    reported_fault: Option<FaultType>,
}

struct RuntimeHost {
    replica_session: OnceLock<(crate::protocol::types::ResourceUid, ProcessSessionId)>,
    native_credentials: OnceLock<Arc<str>>,
    native_address: OnceLock<String>,
    native_lifecycle: StdMutex<Option<crate::replicator::v1::LifecycleObservation>>,
    identity: ReplicaIdentity,
    application: Arc<dyn StatefulServiceReplica>,
    public_v1_binding: crate::protocol::public_operations::PublicV1LifecycleBinding,
    public_v1_replication_address: std::net::SocketAddr,
    state: RwLock<HostState>,
    registered: OnceLock<RegisteredReplicator>,
    replicator_creation: StdMutex<ReplicatorCreationState>,
    aborted: AtomicBool,
    closed: AtomicBool,
    replacement_fault_fenced: AtomicBool,
    replacement_authority_fenced: AtomicBool,
    write_admission: Arc<StdMutex<()>>,
    replacement_fault_bridge: Mutex<Option<Arc<ReplacementFaultBridge>>>,
}

struct DeferredReplicator {
    control: Arc<dyn Fn() -> Result<Arc<dyn Replicator>> + Send + Sync>,
    primary: Arc<dyn Fn() -> Result<Arc<dyn PrimaryReplicator>> + Send + Sync>,
    abort_unregistered: Arc<dyn Fn() + Send + Sync>,
}

impl DeferredReplicator {
    fn control(&self) -> Result<Arc<dyn Replicator>> {
        (self.control)()
    }

    fn primary(&self) -> Result<Arc<dyn PrimaryReplicator>> {
        (self.primary)()
    }
}

#[async_trait]
impl Replicator for DeferredReplicator {
    async fn open(&self) -> Result<String> {
        self.control()?.open().await
    }

    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()> {
        self.control()?.change_role(epoch, role).await
    }

    async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        self.control()?.update_epoch(epoch).await
    }

    async fn close(&self) -> Result<()> {
        self.control()?.close().await
    }

    fn abort(&self) {
        if let Ok(control) = self.control() {
            control.abort();
        } else {
            (self.abort_unregistered)();
        }
    }

    async fn current_progress(&self) -> Result<i64> {
        self.control()?.current_progress().await
    }

    async fn catch_up_capability(&self) -> Result<i64> {
        self.control()?.catch_up_capability().await
    }
}

#[async_trait]
impl PrimaryReplicator for DeferredReplicator {
    async fn on_data_loss(&self) -> Result<bool> {
        self.primary()?.on_data_loss().await
    }

    async fn update_catch_up_replica_set_configuration(
        &self,
        current: crate::replicator::ReplicaSetConfiguration,
        previous: crate::replicator::ReplicaSetConfiguration,
    ) -> Result<()> {
        self.primary()?
            .update_catch_up_replica_set_configuration(current, previous)
            .await
    }

    async fn wait_for_catch_up_quorum(
        &self,
        mode: crate::replicator::ReplicaSetQuorumMode,
    ) -> Result<()> {
        self.primary()?.wait_for_catch_up_quorum(mode).await
    }

    async fn update_current_replica_set_configuration(
        &self,
        current: crate::replicator::ReplicaSetConfiguration,
    ) -> Result<()> {
        self.primary()?
            .update_current_replica_set_configuration(current)
            .await
    }

    async fn build_replica(&self, replica: crate::replicator::ReplicaInformation) -> Result<()> {
        self.primary()?.build_replica(replica).await
    }

    async fn remove_replica(&self, replica_id: crate::protocol::types::ReplicaId) -> Result<()> {
        self.primary()?.remove_replica(replica_id).await
    }
}

impl Drop for PodRuntime {
    fn drop(&mut self) {
        self.host.abort();
    }
}

impl PodRuntime {
    pub(crate) fn new_replacement<A, S>(
        identity: ReplicaIdentity,
        application: Arc<A>,
        _store: Arc<S>,
        binding: crate::protocol::public_operations::PublicV1LifecycleBinding,
        replication_address: std::net::SocketAddr,
    ) -> Self
    where
        A: StatefulServiceReplica + 'static,
        S: Send + Sync + 'static,
    {
        let partition_id = PartitionId::new(binding.resource_uid.as_str());
        let application: Arc<dyn StatefulServiceReplica> = application;
        Self {
            host: Arc::new_cyclic(|_weak_self| RuntimeHost {
                identity,
                application,
                public_v1_binding: binding,
                public_v1_replication_address: replication_address,
                state: RwLock::new(HostState {
                    projection: HostProjection::new(),
                    partition_information: PartitionInformation { partition_id },
                    load_metrics: BTreeMap::new(),
                    reported_fault: None,
                }),
                registered: OnceLock::new(),
                replicator_creation: StdMutex::new(ReplicatorCreationState::Available(
                    ReplicatorCreationIdentity::new(RuntimeHostToken::new()),
                )),
                aborted: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                replacement_fault_fenced: AtomicBool::new(false),
                replacement_authority_fenced: AtomicBool::new(false),
                write_admission: Arc::new(StdMutex::new(())),
                replacement_fault_bridge: Mutex::new(None),
                replica_session: OnceLock::new(),
                native_credentials: OnceLock::new(),
                native_address: OnceLock::new(),
                native_lifecycle: StdMutex::new(None),
            }),
        }
    }

    pub(crate) fn configure_native_transport(&self, bearer_token: Arc<str>) -> Result<()> {
        if bearer_token.is_empty() || self.host.registered.get().is_some() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.host
            .native_credentials
            .set(bearer_token)
            .map_err(|_| RuntimeError::AuthorityNotAdmitted)
    }

    pub(crate) async fn public_open_context(&self, mode: OpenMode) -> OpenContext {
        let context = ReplicatorFactoryContext::new(
            RuntimeHostToken::new(),
            self.host.identity.clone(),
            self.host
                .replica_session
                .get()
                .map(|(_, session)| session.clone())
                .unwrap_or_default(),
            Arc::new(HostAccessView {
                host: Arc::downgrade(&self.host),
                partition_information: self.host.state.read().await.partition_information.clone(),
            }),
            self.host.public_v1_binding.clone(),
            self.host.public_v1_replication_address,
        );
        OpenContext {
            identity: self.host.identity.clone(),
            mode,
            partition: StatefulServicePartition::new(
                RuntimeHostToken::new(),
                self.host.clone(),
                context,
            ),
        }
    }

    pub(crate) fn public_lifecycle_callbacks(
        &self,
    ) -> Result<crate::host::public_lifecycle::PublicLifecycleCallbacks> {
        let role_host = Arc::downgrade(&self.host);
        let native_role_epoch: crate::host::public_lifecycle::NativeRoleEpoch =
            Arc::new(move || {
                let host = role_host.upgrade().ok_or(RuntimeError::Closed)?;
                let Some(native) = host
                    .registered
                    .get()
                    .and_then(|registered| registered.native_runtime_control.as_ref())
                else {
                    return Ok(None);
                };
                let observation = host.native_lifecycle.lock().unwrap().ok_or_else(|| {
                    RuntimeError::ControlPlane("native lifecycle is not opened".into())
                })?;
                Ok(Some((observation, native.selected_epoch())))
            });
        let drain_host = Arc::downgrade(&self.host);
        let revoke_host = Arc::downgrade(&self.host);
        let revoke_access: Arc<dyn Fn() -> Result<()> + Send + Sync> = Arc::new(move || {
            let host = revoke_host.upgrade().ok_or(RuntimeError::Closed)?;
            let _admission = host.write_admission.lock().unwrap();
            host.replacement_authority_fenced
                .store(true, Ordering::Release);
            Ok(())
        });
        let terminal_drain: crate::host::public_lifecycle::TerminalDrain = Arc::new(move || {
            let host = drain_host.upgrade();
            Box::pin(async move {
                let host = host.ok_or(RuntimeError::Closed)?;
                if let Some(runtime) = host
                    .registered
                    .get()
                    .and_then(|registered| registered.native_runtime_control.clone())
                {
                    runtime.shutdown_and_drain().await?;
                }
                Ok(())
            })
        });
        if let Some(registered) = self.host.registered.get() {
            return Ok(crate::host::public_lifecycle::PublicLifecycleCallbacks {
                application: self.host.application.clone(),
                replicator: registered.control.clone(),
                primary: registered.primary().ok_or(RuntimeError::NotPrimary)?,
                application_control_match: None,
                native_role_epoch: Some(native_role_epoch),
                revoke_access,
                capabilities: registered.capabilities,
                replicator_creation_id: registered.identity.to_public_id(),
                open_context: None,
                containment: registered
                    .native_runtime_control
                    .as_ref()
                    .map(|runtime| runtime.containment())
                    .or_else(|| registered.callback_containment.clone()),
                wait_for_containment: registered.native_runtime_control.is_some()
                    || registered.callback_containment.is_some(),
                preempt_retained_owner: registered.native_runtime_control.is_some(),
                terminal_drain,
                terminal_cleanup: None,
                aborted: Arc::new(std::sync::Mutex::new(false)),
                #[cfg(all(test, feature = "testing"))]
                cut: None,
            });
        }
        let control_host = Arc::downgrade(&self.host);
        let primary_host = Arc::downgrade(&self.host);
        let abort_host = Arc::downgrade(&self.host);
        let deferred = Arc::new(DeferredReplicator {
            control: Arc::new(move || {
                let host = control_host.upgrade().ok_or(RuntimeError::Closed)?;
                Ok(host
                    .registered
                    .get()
                    .ok_or(RuntimeError::NotOpen)?
                    .control
                    .clone())
            }),
            primary: Arc::new(move || {
                let host = primary_host.upgrade().ok_or(RuntimeError::Closed)?;
                host.registered
                    .get()
                    .ok_or(RuntimeError::NotOpen)?
                    .primary()
                    .ok_or(RuntimeError::NotPrimary)
            }),
            abort_unregistered: Arc::new(move || {
                if let Some(host) = abort_host.upgrade() {
                    host.application.abort();
                }
            }),
        });
        let replicator: Arc<dyn Replicator> = deferred.clone();
        let primary: Arc<dyn PrimaryReplicator> = deferred;
        let host = Arc::downgrade(&self.host);
        Ok(crate::host::public_lifecycle::PublicLifecycleCallbacks {
            application: self.host.application.clone(),
            replicator,
            primary,
            application_control_match: Some(Arc::new(move |control| {
                host.upgrade()
                    .and_then(|host| {
                        host.registered
                            .get()
                            .map(|registered| Arc::ptr_eq(control, &registered.control))
                    })
                    .unwrap_or(false)
            })),
            native_role_epoch: Some(native_role_epoch),
            revoke_access,
            capabilities: ReplicatorCapabilities::conservative(),
            replicator_creation_id: self.replacement_replicator_creation_id()?,
            open_context: None,
            containment: None,
            wait_for_containment: false,
            preempt_retained_owner: false,
            terminal_drain,
            terminal_cleanup: None,
            aborted: Arc::new(std::sync::Mutex::new(false)),
            #[cfg(all(test, feature = "testing"))]
            cut: None,
        })
    }

    pub(crate) async fn refresh_replacement_projection(
        &self,
        store: &dyn crate::host::store::AgentStore,
        process_session: &ProcessSessionId,
        allow_reopen: bool,
    ) -> Result<()> {
        use crate::host::state::{PublicOperationDisposition, PublicOperationStage};
        use crate::protocol::public_operations::PublicOperationProgram;

        let persisted = store
            .load_state()
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let preview = persisted
            .public_v1_operation_state
            .as_ref()
            .ok_or_else(|| {
                RuntimeError::Application("Public V1 requires replacement state".into())
            })?;
        let mut open = false;
        let mut role = None;
        let mut epoch = None;
        let mut configuration = None;
        let mut dynamic_pending = false;
        let mut records = preview
            .operations
            .values()
            .filter(|record| &record.intent.process_session_id == process_session)
            .collect::<Vec<_>>();
        records.sort_by(|left, right| {
            left.intent
                .revision
                .cmp(&right.intent.revision)
                .then_with(|| left.intent.operation_id.cmp(&right.intent.operation_id))
        });
        let completed_configuration_revision = records
            .iter()
            .filter(|record| crate::host::public_lifecycle::completed_configuration(record))
            .map(|record| record.intent.revision)
            .max()
            .unwrap_or(0);
        for record in records {
            if record.stage == PublicOperationStage::Completed
                && record.disposition != Some(PublicOperationDisposition::Succeeded)
                && !record.lifecycle.outcomes.iter().any(|outcome| {
                    matches!(
                        outcome,
                        crate::host::state::PublicInstructionOutcome::RebuildRequired(_)
                    )
                })
            {
                let superseded_catch_up = record.intent.replay_disposition
                    == crate::protocol::public_operations::PublicOperationReplayDisposition::
                        ReevaluateCatchUp
                    && record.intent.revision < completed_configuration_revision;
                let retired_build = matches!(
                    record.intent.program,
                    Some(PublicOperationProgram::Build(_))
                ) && preview
                    .build_records
                    .get(&record.intent.operation_id)
                    .is_some_and(|build| {
                        build.retirement
                            == crate::host::public_build_store::PublicV1BuildRetirement::Retired
                            && build.source_session == record.intent.process_session_id
                    });
                if superseded_catch_up || retired_build {
                    continue;
                }
            }
            if record.stage != PublicOperationStage::Completed
                || record.disposition != Some(PublicOperationDisposition::Succeeded)
            {
                let before_swap_revoke = record.stage != PublicOperationStage::Completed
                    && matches!(
                        record.intent.program,
                        Some(PublicOperationProgram::Swap { .. })
                    )
                    && !crate::host::public_lifecycle::instruction_selected(
                        record,
                        crate::host::state::PublicInstruction::Revoke,
                    );
                if before_swap_revoke {
                    continue;
                }
                if matches!(
                    record.intent.program.as_ref(),
                    Some(
                        PublicOperationProgram::CatchUp { .. }
                            | PublicOperationProgram::Swap { .. }
                            | PublicOperationProgram::Build(_)
                            | PublicOperationProgram::Remove(_)
                    )
                ) || record.intent.lifecycle.is_some()
                {
                    dynamic_pending = true;
                }
                continue;
            }
            match record.intent.program.as_ref() {
                Some(PublicOperationProgram::Open { .. }) => open = true,
                Some(PublicOperationProgram::Role {
                    epoch: role_epoch,
                    role: selected,
                }) => role = Some((*role_epoch, *selected)),
                Some(PublicOperationProgram::Epoch {
                    epoch: selected_epoch,
                }) => epoch = Some(*selected_epoch),
                Some(PublicOperationProgram::Configuration(selected))
                | Some(PublicOperationProgram::CatchUp {
                    configuration: selected,
                    ..
                }) => configuration = Some(selected.clone()),
                Some(PublicOperationProgram::Swap {
                    refreshed,
                    epoch: selected_epoch,
                    handoff,
                    ..
                }) => {
                    role = Some((*selected_epoch, *handoff));
                    epoch = Some(*selected_epoch);
                    configuration = Some(refreshed.clone());
                }
                Some(
                    PublicOperationProgram::Build(_)
                    | PublicOperationProgram::Remove(_)
                    | PublicOperationProgram::RetireIdleReplica { .. },
                ) => {}
                None if record.intent.lifecycle.is_some() => {
                    let lifecycle = record.intent.lifecycle.as_ref().expect("checked lifecycle");
                    open = true;
                    epoch = Some(lifecycle.epoch);
                    role = Some((
                        lifecycle.epoch,
                        if lifecycle.recipe
                            == crate::protocol::public_operations::PublicLifecycleRecipe::
                                SecondaryEpochAdvance
                        {
                            ReplicaRole::ActiveSecondary
                        } else {
                            ReplicaRole::Primary
                        },
                    ));
                    configuration = Some(lifecycle.configuration());
                }
                Some(PublicOperationProgram::Close | PublicOperationProgram::Abort) => {
                    open = false;
                    role = Some((Epoch::default(), ReplicaRole::None));
                    epoch = None;
                    configuration = None;
                }
                Some(PublicOperationProgram::Progress { .. }) | None => {}
            }
        }
        // The renewal journal can advance survivor epochs, but cannot replace
        // the replacement process's successful application-role operation.
        if let Some(renewal) = preview.session_renewal.as_ref().filter(|renewal| {
            matches!(
                renewal.stage,
                crate::protocol::public_operations::PublicSessionRenewalStage::ConfigurationPending
                    | crate::protocol::public_operations::PublicSessionRenewalStage::Authorized
            )
        }) && renewal
            .required
            .replacement
            .iter()
            .chain(renewal.required.surviving_sessions.iter())
            .any(|endpoint| {
                endpoint.identity == self.host.identity
                    && &endpoint.process_session_id == process_session
            })
            && let Some(member) = renewal
                .required
                .fresh_configuration
                .members
                .iter()
                .find(|member| member.identity == self.host.identity)
            && renewal_projection_is_current(
                renewal.required.renewal_epoch,
                member.role,
                epoch,
                role,
                configuration.as_ref(),
            )
            && (renewal
                .required
                .replacement
                .as_ref()
                .is_none_or(|endpoint| {
                    &endpoint.process_session_id != process_session
                        || endpoint.identity != self.host.identity
                })
                || role == Some((renewal.required.renewal_epoch, member.role)))
        {
            epoch = Some(renewal.required.renewal_epoch);
            role = Some((renewal.required.renewal_epoch, member.role));
        }
        let (current_progress, catch_up_capability) = match self.host.registered.get() {
            _ if crate::host::native_transport::recovery_observations_pending(&persisted) => {
                let pending = crate::protocol::public_operations::ProgressQueryResult::NotQueried(
                    "native observations belong to an unresolved recovery history".into(),
                );
                (pending.clone(), pending)
            }
            Some(registered) => registered.native_progress().await,
            None => (
                crate::protocol::public_operations::ProgressQueryResult::NotReady,
                crate::protocol::public_operations::ProgressQueryResult::NotReady,
            ),
        };
        let (role_epoch, selected_role) = role.unwrap_or((Epoch::default(), ReplicaRole::None));
        let candidate_ready = !dynamic_pending
            && open
            && configuration.as_ref().is_some_and(|selected| {
                epoch.is_some_and(|epoch| epoch >= role_epoch && selected.current.epoch == epoch)
                    && selected.current.members.iter().any(|member| {
                        member.identity == self.host.identity && member.role == selected_role
                    })
            });
        if allow_reopen && candidate_ready {
            self.host
                .replacement_authority_fenced
                .store(false, Ordering::Release);
        }
        let ready = candidate_ready
            && !self
                .host
                .replacement_authority_fenced
                .load(Ordering::Acquire)
            && !self.host.replacement_fault_fenced.load(Ordering::Acquire);
        let observation = *self.host.native_lifecycle.lock().unwrap();
        let role = if !open {
            ReplicaRole::None
        } else if let Some(crate::replicator::v1::LifecycleObservation::Ready(role)) = observation {
            role
        } else {
            selected_role
        };
        let native_live = !matches!(
            observation,
            Some(
                crate::replicator::v1::LifecycleObservation::Closed
                    | crate::replicator::v1::LifecycleObservation::Faulted
            )
        );
        let (read_status, write_status) = if ready && native_live && role == selected_role {
            let remotes = configuration
                .as_ref()
                .into_iter()
                .flat_map(|configuration| &configuration.replicas)
                .filter(|replica| replica.identity != self.host.identity)
                .map(|replica| crate::host::v1_lifecycle::RemoteReplica {
                    information: replica.clone().into(),
                    previous: configuration
                        .as_ref()
                        .and_then(|configuration| configuration.previous.as_ref())
                        .and_then(|previous| {
                            previous
                                .members
                                .iter()
                                .find(|member| member.identity == replica.identity)
                        })
                        .map_or(ReplicaRole::None, |member| member.role),
                    current: replica.role,
                    is_up: true,
                    ready: true,
                })
                .collect();
            crate::host::v1_lifecycle::activated_access(
                role,
                persisted.identity.effective_policy.write_quorum as usize,
                remotes,
            )
            .map_err(|error| RuntimeError::ControlPlane(error.to_string()))?
        } else {
            (AccessStatus::NotPrimary, AccessStatus::NotPrimary)
        };
        let mut state = self.host.state.write().await;
        state.projection.open = open;
        state.projection.role = role;
        state.projection.read_status = read_status;
        state.projection.write_status = write_status;
        state.projection.current_progress = current_progress;
        state.projection.catch_up_capability = catch_up_capability;
        Ok(())
    }

    pub(crate) fn provider_writer_lease_count(&self) -> usize {
        self.host.registered.get().is_some_and(|registered| {
            registered
                .provider_writer_lease_consumed
                .load(Ordering::Acquire)
        }) as usize
    }

    pub(crate) async fn fence_replacement_access(&self) {
        {
            let _admission = self.host.write_admission.lock().unwrap();
            self.host
                .replacement_authority_fenced
                .store(true, Ordering::Release);
        }
        let mut state = self.host.state.write().await;
        state.projection.read_status = AccessStatus::NotPrimary;
        state.projection.write_status = AccessStatus::NotPrimary;
    }

    pub(crate) async fn complete_replacement_open(&self) {
        self.host.state.write().await.projection.open = true;
    }

    pub(crate) async fn install_replacement_inbound_build_authority(
        &self,
        authority: &crate::protocol::public_operations::PublicInboundBuildAuthority,
    ) -> Result<()> {
        self.host
            .registered
            .get()
            .and_then(|registered| registered.native_runtime_control.clone())
            .ok_or(RuntimeError::NotOpen)?
            .install_inbound_build_authority(authority)
            .await
    }

    pub(crate) async fn revoke_replacement_inbound_build_authority(
        &self,
        authority: &crate::protocol::public_operations::PublicInboundBuildAuthority,
    ) -> Result<()> {
        self.host
            .registered
            .get()
            .and_then(|registered| registered.native_runtime_control.clone())
            .ok_or(RuntimeError::NotOpen)?
            .revoke_inbound_build_authority(authority)
            .await
    }

    pub(crate) fn replacement_raw_retained_task_cardinality(&self) -> Result<(usize, usize)> {
        self.host
            .registered
            .get()
            .and_then(|registered| registered.native_runtime_control.clone())
            .ok_or(RuntimeError::NotOpen)
            .map(|control| control.raw_retained_task_cardinality())
    }

    pub(crate) async fn replacement_report_projection(
        &self,
        process_session_id: ProcessSessionId,
        report_sequence: u64,
    ) -> Result<crate::host::public_lifecycle::PublicV1ReportProjection> {
        use crate::protocol::public_operations::{PublicProgress, PublicProgressRole};

        let bridge = self.host.replacement_fault_bridge.lock().await.clone();
        let observation_state = match bridge {
            Some(bridge) => Some(
                bridge
                    .store
                    .load_state()
                    .await
                    .map_err(|error| RuntimeError::Application(error.to_string()))?,
            ),
            None if self.host.registered.get().is_none() => None,
            None => return Err(RuntimeError::AuthorityNotAdmitted),
        };
        let (current_progress, catch_up_capability) = match self.host.registered.get() {
            _ if observation_state
                .as_ref()
                .is_some_and(crate::host::native_transport::recovery_observations_pending) =>
            {
                let pending = crate::protocol::public_operations::ProgressQueryResult::NotQueried(
                    "native observations belong to an unresolved recovery history".into(),
                );
                (pending.clone(), pending)
            }
            Some(registered) => registered.native_progress().await,
            None => (
                crate::protocol::public_operations::ProgressQueryResult::NotReady,
                crate::protocol::public_operations::ProgressQueryResult::NotReady,
            ),
        };
        let state = self.host.state.read().await;
        let role = state.projection.role;
        let replicator_creation_id = self.replacement_replicator_creation_id()?;
        Ok(crate::host::public_lifecycle::PublicV1ReportProjection {
            catch_up_specific_quorum: self
                .host
                .registered
                .get()
                .map(|registered| registered.capabilities.catch_up_specific_quorum),
            process_session_id,
            replicator_creation_id,
            report_sequence,
            role,
            read_status: state.projection.read_status,
            write_status: state.projection.write_status,
            progress: PublicProgress {
                role: match role {
                    ReplicaRole::Primary => PublicProgressRole::Primary,
                    ReplicaRole::ActiveSecondary | ReplicaRole::IdleSecondary => {
                        PublicProgressRole::Secondary
                    }
                    _ => PublicProgressRole::Recovery,
                },
                current_progress,
                catch_up_capability,
            },
            progress_history: observation_state
                .and_then(|state| state.public_v1_operation_state)
                .and_then(|preview| preview.history),
            native_idle_replicas: self
                .host
                .registered
                .get()
                .and_then(|registered| registered.native_runtime_control.as_ref())
                .and_then(|runtime| match runtime.idle_replicas() {
                    Ok(ids) => Some(ids),
                    Err(error) => {
                        tracing::debug!(%error, "native idle observations are unavailable");
                        None
                    }
                }),
        })
    }

    pub(crate) async fn bind_public_fault_runtime(
        &self,
        store: Arc<dyn crate::host::store::AgentStore>,
        registry: Arc<crate::host::operation::PartitionOperationRegistry>,
        preview: PublicV1Identity,
        process_session: ProcessSessionId,
    ) -> Result<()> {
        let mut bridge = self.host.replacement_fault_bridge.lock().await;
        if bridge.is_some() {
            return Err(RuntimeError::Application(
                "public fault runtime is already bound".into(),
            ));
        }
        *bridge = Some(Arc::new(ReplacementFaultBridge {
            store,
            registry,
            identity: preview,
            process_session,
            admission: Mutex::new(()),
        }));
        Ok(())
    }

    pub(crate) fn bind_replica_session(
        &self,
        resource: crate::protocol::types::ResourceUid,
        session: ProcessSessionId,
    ) -> Result<()> {
        let binding = (resource, session);
        if self.host.replica_session.get() == Some(&binding) {
            return Ok(());
        }
        if self.host.registered.get().is_some() || binding.0.is_empty() || binding.1.is_empty() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.host
            .replica_session
            .set(binding)
            .map_err(|_| RuntimeError::AuthorityNotAdmitted)
    }

    pub(crate) fn replacement_replicator_creation_id(
        &self,
    ) -> Result<crate::protocol::types::ReplicatorCreationId> {
        if let Some(registered) = self.host.registered.get() {
            return Ok(registered.identity.to_public_id());
        }
        let creation = self
            .host
            .replicator_creation
            .lock()
            .map_err(|_| RuntimeError::Closed)?;
        match &*creation {
            ReplicatorCreationState::Available(identity)
            | ReplicatorCreationState::Reserved { identity, .. } => Ok(identity.to_public_id()),
            ReplicatorCreationState::Registered => self
                .host
                .registered
                .get()
                .map(|registered| registered.identity.to_public_id())
                .ok_or_else(|| {
                    RuntimeError::ControlPlane(
                        "registered replicator creation lost its interface bundle".into(),
                    )
                }),
        }
    }

    pub(crate) fn native_role_epoch(
        &self,
    ) -> Result<Option<(crate::replicator::v1::LifecycleObservation, Option<Epoch>)>> {
        let Some(native) = self
            .host
            .registered
            .get()
            .and_then(|registered| registered.native_runtime_control.as_ref())
        else {
            return Ok(None);
        };
        let observation =
            self.host.native_lifecycle.lock().unwrap().ok_or_else(|| {
                RuntimeError::ControlPlane("native lifecycle is not opened".into())
            })?;
        Ok(Some((observation, native.selected_epoch())))
    }

    pub(crate) async fn diagnostic_projection(
        &self,
    ) -> (
        bool,
        Option<String>,
        ReplicaRole,
        AccessStatus,
        AccessStatus,
        crate::protocol::public_operations::ProgressQueryResult,
        crate::protocol::public_operations::ProgressQueryResult,
    ) {
        let projection = self.host.state.read().await.projection.clone();
        (
            projection.open,
            projection.replication_address,
            projection.role,
            projection.read_status,
            projection.write_status,
            projection.current_progress,
            projection.catch_up_capability,
        )
    }

    pub(crate) async fn partition_report(&self) -> (Vec<LoadMetric>, Option<FaultType>) {
        let state = self.host.state.read().await;
        (
            state
                .load_metrics
                .iter()
                .map(|(name, value)| LoadMetric {
                    name: name.clone(),
                    value: *value,
                })
                .collect(),
            state.reported_fault,
        )
    }

    pub(crate) fn abort(&self) {
        self.host.abort();
    }

    pub(crate) async fn shutdown_and_drain(&self) -> Result<()> {
        self.abort();
        if let Some(control) = self
            .host
            .registered
            .get()
            .and_then(|registered| registered.native_runtime_control.clone())
        {
            control.shutdown_and_drain().await
        } else {
            Ok(())
        }
    }
}

impl RuntimeHost {
    fn abort(&self) {
        if self.closed.load(Ordering::Acquire) || self.aborted.swap(true, Ordering::AcqRel) {
            return;
        }
        // Registration and Abort select child ownership under the same guard.
        // Otherwise a late Open could publish a child after teardown saw None.
        let creation = self
            .replicator_creation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(registered) = self.registered.get() {
            registered
                .provider_writer_lease_consumed
                .store(false, Ordering::Release);
            registered.abort();
        }
        drop(creation);
        self.application.abort();
    }
}

struct HostAccessView {
    host: Weak<RuntimeHost>,
    partition_information: PartitionInformation,
}

struct ReplacementFaultBridge {
    store: Arc<dyn crate::host::store::AgentStore>,
    registry: Arc<crate::host::operation::PartitionOperationRegistry>,
    identity: PublicV1Identity,
    process_session: ProcessSessionId,
    admission: Mutex<()>,
}

impl ReplacementFaultBridge {
    fn fence(&self) {
        self.registry.fence();
    }

    async fn report(&self, fault: FaultType) -> crate::host::Result<()> {
        let _admission = self.admission.lock().await;
        let state = self.store.load_state().await?;
        let preview = state.public_v1_operation_state.as_ref().ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "public fault requires replacement state".into(),
            )
        })?;
        if preview.identity != self.identity
            || preview.binding.is_none()
            || state.identity.resource_uid
                != preview
                    .binding
                    .as_ref()
                    .expect("binding checked")
                    .resource_uid
        {
            return Err(crate::host::HostError::IdentityMismatch(
                "public fault store binding changed".into(),
            ));
        }
        let class = match fault {
            FaultType::Transient => PublicOperationClass::TransientFault,
            FaultType::Permanent => PublicOperationClass::PermanentFault,
        };
        if let Some(intent) = preview
            .operations
            .values()
            .filter(|record| {
                record.intent.process_session_id == self.process_session
                    && record.intent.class == class
                    && record.superseded_by.is_none()
            })
            .max_by_key(|record| record.intent.revision)
            .map(|record| record.intent.clone())
        {
            self.registry.report_fault(intent).await?;
            self.registry.fence_and_schedule_fault_containment();
            return Ok(());
        }
        let revision = preview
            .operations
            .values()
            .map(|record| record.intent.revision)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                crate::host::HostError::CommandRejected("public fault revision overflow".into())
            })?;
        let intent = PublicOperationIntent {
            identity: self.identity.clone(),
            operation_id: OperationId::new(format!(
                "fault-{}-{revision}-{fault:?}",
                self.process_session
            )),
            revision,
            process_session_id: self.process_session.clone(),
            class,
            input_digest:
                crate::protocol::public_operations::CanonicalOperationInputDigest::opaque_v1(
                    "kuberic-public-fault-input-v1",
                    format!("{fault:?}:{revision}").as_bytes(),
                ),
            replay_disposition:
                crate::protocol::public_operations::PublicOperationReplayDisposition::Containment,
            lifecycle: None,
            program: None,
        };
        self.registry.report_fault(intent).await?;
        self.registry.fence_and_schedule_fault_containment();
        Ok(())
    }
}

#[async_trait]
impl PartitionAccessView for HostAccessView {
    fn native_write_admission(&self) -> Result<Arc<StdMutex<()>>> {
        Ok(self
            .host
            .upgrade()
            .ok_or(RuntimeError::Closed)?
            .write_admission
            .clone())
    }

    fn partition_information(&self) -> PartitionInformation {
        self.partition_information.clone()
    }

    async fn read_status(&self) -> Result<AccessStatus> {
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        if host.replacement_fault_fenced.load(Ordering::Acquire)
            || host.replacement_authority_fenced.load(Ordering::Acquire)
            || matches!(
                *host.native_lifecycle.lock().unwrap(),
                Some(
                    crate::replicator::v1::LifecycleObservation::Closed
                        | crate::replicator::v1::LifecycleObservation::Faulted
                )
            )
        {
            return Ok(AccessStatus::NotPrimary);
        }
        Ok(host.state.read().await.projection.read_status)
    }

    async fn write_status(&self) -> Result<AccessStatus> {
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        if host.replacement_fault_fenced.load(Ordering::Acquire)
            || host.replacement_authority_fenced.load(Ordering::Acquire)
            || matches!(
                *host.native_lifecycle.lock().unwrap(),
                Some(
                    crate::replicator::v1::LifecycleObservation::Closed
                        | crate::replicator::v1::LifecycleObservation::Faulted
                )
            )
        {
            return Ok(AccessStatus::NotPrimary);
        }
        Ok(host.state.read().await.projection.write_status)
    }

    fn native_write_status(&self) -> AccessStatus {
        let Some(host) = self.host.upgrade() else {
            return AccessStatus::NotPrimary;
        };
        if host.closed.load(Ordering::Acquire)
            || host.aborted.load(Ordering::Acquire)
            || host.replacement_fault_fenced.load(Ordering::Acquire)
            || host.replacement_authority_fenced.load(Ordering::Acquire)
        {
            return AccessStatus::NotPrimary;
        }

        if matches!(
            *host.native_lifecycle.lock().unwrap(),
            Some(
                crate::replicator::v1::LifecycleObservation::Closed
                    | crate::replicator::v1::LifecycleObservation::Faulted
            )
        ) {
            return AccessStatus::NotPrimary;
        }
        host.state
            .try_read()
            .map_or(AccessStatus::ReconfigurationPending, |state| {
                state.projection.write_status
            })
    }

    fn native_read_status(&self) -> AccessStatus {
        let Some(host) = self.host.upgrade() else {
            return AccessStatus::NotPrimary;
        };
        if host.closed.load(Ordering::Acquire)
            || host.aborted.load(Ordering::Acquire)
            || host.replacement_fault_fenced.load(Ordering::Acquire)
            || host.replacement_authority_fenced.load(Ordering::Acquire)
            || matches!(
                *host.native_lifecycle.lock().unwrap(),
                Some(
                    crate::replicator::v1::LifecycleObservation::Closed
                        | crate::replicator::v1::LifecycleObservation::Faulted
                )
            )
        {
            return AccessStatus::NotPrimary;
        }
        host.state
            .try_read()
            .map_or(AccessStatus::ReconfigurationPending, |state| {
                state.projection.read_status
            })
    }

    fn validate_native_creation(&self, reservation: ReplicatorCreationReservation) -> Result<()> {
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        if host.closed.load(Ordering::Acquire) || host.aborted.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let creation = host
            .replicator_creation
            .lock()
            .map_err(|_| RuntimeError::Closed)?;
        if !matches!(&*creation, ReplicatorCreationState::Reserved { identity, .. }
            if *identity == reservation.identity(RuntimeHostToken::new()))
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }

    fn observe_native_lifecycle(
        &self,
        reservation: ReplicatorCreationReservation,
        observation: crate::replicator::v1::LifecycleObservation,
    ) {
        if let Some(host) = self.host.upgrade() {
            let identity = reservation.identity(RuntimeHostToken::new());
            let current = host.registered.get().map_or_else(
                || host.replicator_creation.try_lock().is_ok_and(|creation| {
                    matches!(&*creation, ReplicatorCreationState::Reserved { identity: current, .. } if *current == identity)
                }),
                |registered| registered.identity == identity,
            );
            if !current {
                return;
            }
            if (host.closed.load(Ordering::Acquire) || host.aborted.load(Ordering::Acquire))
                && matches!(
                    observation,
                    crate::replicator::v1::LifecycleObservation::Opened
                        | crate::replicator::v1::LifecycleObservation::Ready(_)
                )
            {
                return;
            }
            *host.native_lifecycle.lock().unwrap() = Some(observation);
        }
    }

    async fn native_credentials(&self) -> Result<Arc<str>> {
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        host.native_credentials
            .get()
            .cloned()
            .ok_or(RuntimeError::AuthorityNotAdmitted)
    }

    async fn native_local_endpoint(
        &self,
        address: String,
    ) -> Result<(
        crate::protocol::types::HistoryContext,
        crate::host::native_transport::NativeEndpoint,
    )> {
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        if host.closed.load(Ordering::Acquire) || host.aborted.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        if host.native_address.get().is_none() {
            let _ = host.native_address.set(address.clone());
        }
        if host.native_address.get() != Some(&address) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let state = self
            .bridge()
            .await?
            .store
            .load_state()
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let preview = state
            .public_v1_operation_state
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if preview.terminal
            && !crate::host::operation::contained_transient_reopen(
                &state,
                &host
                    .replica_session
                    .get()
                    .ok_or(RuntimeError::AuthorityNotAdmitted)?
                    .1,
            )
        {
            return Err(RuntimeError::Closed);
        }
        let endpoint = crate::host::native_transport::NativeEndpoint {
            identity: host.identity.clone(),
            process_session_id: host
                .replica_session
                .get()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?
                .1
                .clone(),
            storage: preview
                .storage_incarnation
                .clone()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?,
            address,
        };
        endpoint.validate()?;
        Ok((
            crate::host::native_transport::transport_history(&state, &endpoint.process_session_id)?
                .clone(),
            endpoint,
        ))
    }

    async fn validate_native_binding(
        &self,
        binding: &crate::host::native_transport::NativeBinding,
    ) -> Result<()> {
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        if host.closed.load(Ordering::Acquire)
            || host.aborted.load(Ordering::Acquire)
            || host.replacement_fault_fenced.load(Ordering::Acquire)
        {
            return Err(RuntimeError::Closed);
        }
        let state = self
            .bridge()
            .await?
            .store
            .load_state()
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        binding.validate_for_state(
            &state,
            &host
                .replica_session
                .get()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?
                .1,
            host.native_address
                .get()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?,
        )
    }

    async fn native_outbound_build_authority(
        &self,
        build: &OperationId,
    ) -> Result<crate::protocol::public_operations::PublicInboundBuildAuthority> {
        let bridge = self.bridge().await?;
        let state = bridge
            .store
            .load_state()
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let record = state
            .public_v1_operation_state
            .as_ref()
            .and_then(|preview| preview.operations.get(build))
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let Some(crate::protocol::public_operations::PublicOperationProgram::Build(input)) =
            record.intent.program.as_ref()
        else {
            return Err(RuntimeError::AuthorityNotAdmitted);
        };
        let configuration = input
            .current_configuration_id
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        bridge
            .store
            .public_v1_outbound_build_authority(build, configuration)
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))?
            .ok_or(RuntimeError::AuthorityNotAdmitted)
    }

    async fn classify_replacement_inbound_build_authority(
        &self,
        authority: &crate::protocol::public_operations::PublicInboundBuildAuthority,
    ) -> Result<crate::host::store::PublicInboundBuildAdmission> {
        self.bridge()
            .await?
            .store
            .classify_public_v1_inbound_build_authority(authority)
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))
    }

    async fn install_replacement_inbound_build_authority(
        &self,
        authority: &crate::protocol::public_operations::PublicInboundBuildAuthority,
    ) -> Result<()> {
        self.bridge()
            .await?
            .store
            .install_public_v1_inbound_build_authority(authority)
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))
    }

    async fn revoke_replacement_inbound_build_authority(&self) -> Result<()> {
        self.bridge()
            .await?
            .store
            .revoke_public_v1_inbound_build_authority()
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))
    }

    fn release_provider_writer(&self) {
        if let Some(host) = self.host.upgrade()
            && let Some(registered) = host.registered.get()
        {
            registered
                .provider_writer_lease_consumed
                .store(false, Ordering::Release);
        }
    }

    async fn report_load(&self, metrics: Vec<LoadMetric>) -> Result<()> {
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        if host.closed.load(Ordering::Acquire) || host.aborted.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let mut names = BTreeSet::new();
        if metrics.iter().any(|metric| {
            metric.name.is_empty() || metric.value < 0 || !names.insert(metric.name.clone())
        }) {
            return Err(RuntimeError::Application(
                "load metrics require unique nonempty names and nonnegative values".into(),
            ));
        }
        host.state.write().await.load_metrics = metrics
            .into_iter()
            .map(|metric| (metric.name, metric.value))
            .collect();
        Ok(())
    }

    async fn report_fault(&self, fault: FaultType) -> Result<()> {
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        let bridge = host.replacement_fault_bridge.lock().await.clone();
        if let Some(bridge) = &bridge {
            bridge.fence();
            host.replacement_fault_fenced.store(true, Ordering::Release);
        }
        {
            let mut state = host.state.write().await;
            if host.closed.load(Ordering::Acquire) || host.aborted.load(Ordering::Acquire) {
                return Err(RuntimeError::Closed);
            }
            if state.reported_fault != Some(FaultType::Permanent) {
                state.reported_fault = Some(fault);
            }
            state.projection.role = ReplicaRole::None;
            state.projection.read_status = AccessStatus::NotPrimary;
            state.projection.write_status = AccessStatus::NotPrimary;
        }
        if let Some(bridge) = bridge {
            bridge
                .report(fault)
                .await
                .map_err(|error| RuntimeError::Application(error.to_string()))?;
        }
        Ok(())
    }
}

impl HostAccessView {
    async fn bridge(&self) -> Result<Arc<ReplacementFaultBridge>> {
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        host.replacement_fault_bridge
            .lock()
            .await
            .clone()
            .ok_or(RuntimeError::AuthorityNotAdmitted)
    }
}

#[async_trait]
impl ReplicatorRegistration for RuntimeHost {
    fn reserve_replicator_creation(&self) -> Result<ReplicatorCreationReservation> {
        if self.closed.load(Ordering::Acquire) || self.aborted.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let mut creation = self.replicator_creation.lock().map_err(|_| {
            RuntimeError::Application("replicator creation state was poisoned".into())
        })?;
        if self.closed.load(Ordering::Acquire) || self.aborted.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        match &*creation {
            ReplicatorCreationState::Available(identity) => {
                let reservation = ReplicatorCreationReservation::from_identity(
                    RuntimeHostToken::new(),
                    *identity,
                );
                *creation = ReplicatorCreationState::Reserved {
                    identity: reservation.identity(RuntimeHostToken::new()),
                    provider_writer: ProviderWriterLease { consumed: false },
                };
                Ok(reservation)
            }
            ReplicatorCreationState::Reserved { .. } | ReplicatorCreationState::Registered => {
                Err(RuntimeError::Application(
                    "CreateReplicator may be called only once per Open".into(),
                ))
            }
        }
    }

    fn cancel_replicator_creation(&self, reservation: ReplicatorCreationReservation) {
        if let Ok(mut creation) = self.replicator_creation.lock()
            && matches!(
                &*creation,
                ReplicatorCreationState::Reserved { identity, .. }
                    if *identity == reservation.identity(RuntimeHostToken::new())
            )
        {
            *creation = ReplicatorCreationState::Available(ReplicatorCreationIdentity::new(
                RuntimeHostToken::new(),
            ));
        }
    }

    async fn register_interfaces(
        &self,
        attachment: &ReplicatorAttachment,
        provider: Option<Arc<dyn StateProvider>>,
        reservation: ReplicatorCreationReservation,
    ) -> Result<()> {
        let reservation_identity = reservation.identity(RuntimeHostToken::new());
        let mut creation = self.replicator_creation.lock().map_err(|_| {
            RuntimeError::Application("replicator creation state was poisoned".into())
        })?;
        if self.closed.load(Ordering::Acquire) || self.aborted.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let ReplicatorCreationState::Reserved {
            identity,
            provider_writer,
        } = &mut *creation
        else {
            return Err(RuntimeError::Application(
                "CreateReplicator reservation is not active".into(),
            ));
        };
        if *identity != reservation_identity
            || attachment.identity(RuntimeHostToken::new()) != *identity
        {
            return Err(RuntimeError::Application(
                "CreateReplicator reservation changed before registration".into(),
            ));
        }
        let consumes_provider_writer =
            provider.is_some() && attachment.has_provider_writer(RuntimeHostToken::new());
        if consumes_provider_writer && provider_writer.consumed {
            return Err(RuntimeError::Application(
                "provider writer lease was already consumed".into(),
            ));
        }
        let provider_writer_lease_consumed = Arc::new(AtomicBool::new(consumes_provider_writer));
        self.registered
            .set(RegisteredReplicator {
                control: attachment.replicator(RuntimeHostToken::new()),
                primary: attachment.primary_replicator(RuntimeHostToken::new()),
                identity: *identity,
                callback_containment: attachment.callback_containment(RuntimeHostToken::new()),
                native_runtime_control: attachment.native_runtime_control(RuntimeHostToken::new()),
                capabilities: attachment.capabilities(RuntimeHostToken::new()),
                provider_writer_lease_consumed,
            })
            .map_err(|_| {
                RuntimeError::Application(
                    "CreateReplicator may be called only once per Open".into(),
                )
            })?;
        provider_writer.consumed = consumes_provider_writer;
        *creation = ReplicatorCreationState::Registered;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::renewal_projection_is_current;
    use crate::protocol::public_operations::PublicConfiguration;
    use crate::protocol::types::{
        AgentGeneration, ConfigurationDescriptor, ConfigurationMember, Epoch, ReplicaId,
        ReplicaIdentity, ReplicaInstanceId, ReplicaRole,
    };

    fn configuration(epoch: Epoch) -> PublicConfiguration {
        PublicConfiguration {
            current: ConfigurationDescriptor::new(
                epoch,
                ReplicaId::new(1),
                vec![ConfigurationMember {
                    identity: ReplicaIdentity {
                        replica_id: ReplicaId::new(1),
                        instance_id: ReplicaInstanceId::new("pod-1"),
                        agent_generation: AgentGeneration::new("generation-1"),
                    },
                    role: ReplicaRole::Primary,
                }],
                1,
            ),
            previous: None,
            replicas: Vec::new(),
        }
    }

    #[tokio::test]
    async fn completed_authority_does_not_latch_an_independent_fault_fence() {
        use super::{HostAccessView, PodRuntime};
        use crate::application::{OpenContext, RoleChange, StatefulServiceReplica};
        use crate::host::state::*;
        use crate::host::store::AgentStore;
        use crate::protocol::public_operations::*;
        use crate::protocol::types::*;
        use crate::replicator::PartitionAccessView;
        use std::sync::Arc;

        struct ProjectionStore(AgentState);
        #[async_trait::async_trait]
        impl AgentStore for ProjectionStore {
            async fn identity(&self) -> crate::host::Result<StorageIdentity> {
                Ok(self.0.identity.clone())
            }
            async fn load_state(&self) -> crate::host::Result<AgentState> {
                Ok(self.0.clone())
            }
            async fn complete_application_initialization(&self) -> crate::host::Result<()> {
                unreachable!("projection does not initialize application storage")
            }
            async fn record_partition_reports(
                &self,
                _: Vec<LoadMetric>,
                _: Option<FaultType>,
            ) -> crate::host::Result<()> {
                unreachable!("projection does not record partition reports")
            }
        }
        struct Application;
        #[async_trait::async_trait]
        impl StatefulServiceReplica for Application {
            async fn open(
                self: Arc<Self>,
                _: OpenContext,
            ) -> crate::Result<Arc<dyn crate::replicator::Replicator>> {
                unreachable!("projection does not invoke application callbacks")
            }
            async fn change_role(&self, _: ReplicaRole) -> crate::Result<RoleChange> {
                unreachable!("projection does not invoke application callbacks")
            }
            async fn close(&self) -> crate::Result<()> {
                unreachable!("projection does not invoke application callbacks")
            }
            fn abort(&self) {}
        }
        let configuration = configuration(Epoch::new(0, 1));
        let local = configuration.current.members[0].identity.clone();
        let resource = ResourceUid::new("projection-resource");
        let identity = PublicV1Identity::new(1);
        let binding = PublicV1LifecycleBinding {
            identity: identity.clone(),
            resource_uid: resource.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let storage = ReplicaStorageIncarnationContext {
            replica: local.clone(),
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new("pod-1"),
                pvc_uid: PvcUid::new("pvc-1"),
            },
            storage_generation: StorageGeneration::new("storage"),
        };
        let initialization =
            derive_history_initialization_id(&resource, PublicProtocolGeneration::new(1));
        let history =
            derive_bootstrap_history(&resource, PublicProtocolGeneration::new(1), &initialization);
        let mut state = AgentState::new(StorageIdentity {
            schema_version: SCHEMA_VERSION,
            resource_uid: resource,
            pod_uid: PodUid::new("pod-1"),
            pvc_uid: PvcUid::new("pvc-1"),
            initialization_id: InitializationId::new("initialization"),
            local_identity: local.clone(),
            effective_policy: EffectivePolicy::fixed(1, 1).unwrap(),
        });
        let session = ProcessSessionId::new("session");
        let input = PublicLifecycleInput {
            recipe: PublicLifecycleRecipe::InitialPrimary,
            replica: local.clone(),
            epoch: configuration.current.epoch,
            possible_data_loss: PossibleDataLossIntent::NotPossible,
            current: configuration.current,
            previous: None,
            replicas: Vec::new(),
            data_loss_recovery: None,
        };
        input.validate().unwrap();
        let intent = PublicOperationIntent {
            identity,
            operation_id: OperationId::new("completed-authority"),
            revision: 1,
            process_session_id: session.clone(),
            class: PublicOperationClass::Authority,
            input_digest: input.digest(),
            replay_disposition: PublicOperationReplayDisposition::Convergent,
            lifecycle: Some(input),
            program: None,
        };
        intent.validate().unwrap();
        let mut preview = PublicV1OperationState::new_bound(binding.clone());
        preview.history_initialization_id = Some(initialization);
        preview.history = Some(history.clone());
        preview.storage_incarnation = Some(storage.clone());
        preview.operations.insert(
            intent.operation_id.clone(),
            PublicOperationRecord {
                intent,
                accepted_history: Some(history),
                accepted_storage: Some(storage),
                stage: PublicOperationStage::Completed,
                disposition: Some(PublicOperationDisposition::Succeeded),
                containment: PublicOperationContainment::Complete,
                superseded_by: None,
                blockers: Default::default(),
                lifecycle: PublicLifecycleRecord {
                    in_flight: None,
                    outcomes: vec![
                        PublicInstructionOutcome::Done,
                        PublicInstructionOutcome::ApplicationRole(None),
                        PublicInstructionOutcome::Done,
                        PublicInstructionOutcome::Done,
                    ],
                },
            },
        );
        state.public_v1_operation_state = Some(preview);
        let store = Arc::new(ProjectionStore(state));
        let runtime = PodRuntime::new_replacement(
            local,
            Arc::new(Application),
            store.clone(),
            binding,
            "127.0.0.1:0".parse().unwrap(),
        );
        let access = HostAccessView {
            host: Arc::downgrade(&runtime.host),
            partition_information: runtime
                .host
                .state
                .read()
                .await
                .partition_information
                .clone(),
        };
        runtime.fence_replacement_access().await;
        runtime
            .host
            .replacement_fault_fenced
            .store(true, std::sync::atomic::Ordering::Release);
        runtime
            .refresh_replacement_projection(store.as_ref(), &session, true)
            .await
            .unwrap();
        assert_eq!(
            access.write_status().await.unwrap(),
            AccessStatus::NotPrimary
        );
        assert_eq!(
            access.read_status().await.unwrap(),
            AccessStatus::NotPrimary
        );

        runtime
            .host
            .replacement_fault_fenced
            .store(false, std::sync::atomic::Ordering::Release);
        runtime
            .refresh_replacement_projection(store.as_ref(), &session, false)
            .await
            .unwrap();
        assert_eq!(access.write_status().await.unwrap(), AccessStatus::Granted);
        assert_eq!(access.read_status().await.unwrap(), AccessStatus::Granted);

        runtime.fence_replacement_access().await;
        runtime
            .refresh_replacement_projection(store.as_ref(), &session, false)
            .await
            .unwrap();
        assert_eq!(
            access.write_status().await.unwrap(),
            AccessStatus::NotPrimary
        );
        let mut recovery_store = ProjectionStore(store.0.clone());
        let preview = recovery_store.0.public_v1_operation_state.as_mut().unwrap();
        let original = preview.operations.values().next().unwrap().clone();
        let configuration = original.intent.lifecycle.as_ref().unwrap().configuration();
        let program = PublicOperationProgram::CatchUp {
            configuration: configuration.clone(),
            mode: PublicCatchUpMode::All,
        };
        let mut failed = original.clone();
        failed.intent.operation_id = OperationId::new("obsolete-recovery");
        failed.intent.revision = 2;
        failed.intent.input_digest = program.digest();
        failed.intent.replay_disposition = program.replay_disposition();
        failed.intent.lifecycle = None;
        failed.intent.program = Some(program);
        failed.disposition = Some(PublicOperationDisposition::Failed("lost peer".into()));
        failed.lifecycle = PublicLifecycleRecord {
            in_flight: Some(PublicInstruction::FirstCatchUp),
            outcomes: vec![PublicInstructionOutcome::Done],
        };
        preview
            .operations
            .insert(failed.intent.operation_id.clone(), failed);
        runtime
            .refresh_replacement_projection(&recovery_store, &session, true)
            .await
            .unwrap();
        assert_eq!(
            access.write_status().await.unwrap(),
            AccessStatus::NotPrimary,
            "an unresolved failed recovery must remain fenced"
        );
        let program = PublicOperationProgram::Configuration(configuration);
        let mut successor = original;
        successor.intent.operation_id = OperationId::new("successor-configuration");
        successor.intent.revision = 3;
        successor.intent.input_digest = program.digest();
        successor.intent.replay_disposition = program.replay_disposition();
        successor.intent.lifecycle = None;
        successor.intent.program = Some(program);
        successor.lifecycle.outcomes = vec![PublicInstructionOutcome::Done];
        recovery_store
            .0
            .public_v1_operation_state
            .as_mut()
            .unwrap()
            .operations
            .insert(successor.intent.operation_id.clone(), successor);
        runtime
            .refresh_replacement_projection(&recovery_store, &session, true)
            .await
            .unwrap();
        assert_eq!(
            access.write_status().await.unwrap(),
            AccessStatus::Granted,
            "historical recovery failure cannot override completed successor configuration"
        );
    }

    #[test]
    fn stale_session_renewal_does_not_override_newer_topology() {
        let renewal_epoch = Epoch::new(0, 2);
        assert!(renewal_projection_is_current(
            renewal_epoch,
            ReplicaRole::Primary,
            None,
            None,
            None
        ));
        assert!(renewal_projection_is_current(
            renewal_epoch,
            ReplicaRole::Primary,
            None,
            None,
            Some(&configuration(Epoch::new(0, 2)))
        ));
        assert!(!renewal_projection_is_current(
            renewal_epoch,
            ReplicaRole::Primary,
            None,
            None,
            Some(&configuration(Epoch::new(0, 3)))
        ));
        for (epoch, role) in [
            (Some(Epoch::new(0, 3)), None),
            (None, Some((Epoch::new(0, 3), ReplicaRole::Primary))),
            (None, Some((renewal_epoch, ReplicaRole::IdleSecondary))),
        ] {
            assert!(!renewal_projection_is_current(
                renewal_epoch,
                ReplicaRole::Primary,
                epoch,
                role,
                Some(&configuration(Epoch::new(0, 2))),
            ));
        }
    }
}
