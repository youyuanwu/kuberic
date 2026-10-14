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

#[derive(Debug)]
struct ProviderWriterLease {
    consumed: bool,
}

enum ReplicatorCreationState {
    Available,
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
    stateless_runtime_control: Option<Arc<dyn crate::replicator::StatelessRuntimeControl>>,
    capabilities: ReplicatorCapabilities,
    provider: Option<Arc<dyn StateProvider>>,
    provider_writer_lease_consumed: Arc<AtomicBool>,
}

impl RegisteredReplicator {
    fn primary(&self) -> Option<Arc<dyn PrimaryReplicator>> {
        self.primary.clone()
    }

    async fn current_progress(&self) -> Result<i64> {
        self.control.current_progress().await
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
    current_progress: i64,
    committed_lsn: i64,
}

impl HostProjection {
    fn new() -> Self {
        Self {
            open: false,
            replication_address: None,
            role: ReplicaRole::None,
            read_status: AccessStatus::NotPrimary,
            write_status: AccessStatus::NotPrimary,
            current_progress: 0,
            committed_lsn: 0,
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
    replacement_ambiguity_fenced: AtomicBool,
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
        let partition_id =
            PartitionId::new(format!("partition-{}", identity.agent_generation.as_str()));
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
                replicator_creation: StdMutex::new(ReplicatorCreationState::Available),
                aborted: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                replacement_fault_fenced: AtomicBool::new(false),
                replacement_authority_fenced: AtomicBool::new(false),
                replacement_ambiguity_fenced: AtomicBool::new(false),
                replacement_fault_bridge: Mutex::new(None),
                replica_session: OnceLock::new(),
            }),
        }
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
        if let Some(registered) = self.host.registered.get() {
            return Ok(crate::host::public_lifecycle::PublicLifecycleCallbacks {
                application: self.host.application.clone(),
                replicator: registered.control.clone(),
                primary: registered.primary().ok_or(RuntimeError::NotPrimary)?,
                application_control_match: None,
                capabilities: registered.capabilities,
                replicator_creation_id: registered.identity.to_public_id(),
                open_context: None,
                containment: registered.callback_containment.clone(),
                wait_for_containment: registered.callback_containment.is_some(),
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
            capabilities: ReplicatorCapabilities::conservative(),
            replicator_creation_id: crate::protocol::types::ReplicatorCreationId::new(
                "pending-public-v1-open",
            ),
            open_context: None,
            containment: None,
            wait_for_containment: false,
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
        let preview = persisted.public_v1_operation_state.ok_or_else(|| {
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
        for record in records {
            if record.stage != PublicOperationStage::Completed
                || record.disposition != Some(PublicOperationDisposition::Succeeded)
            {
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
                Some(PublicOperationProgram::Build(_) | PublicOperationProgram::Remove(_)) => {}
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
        let (applied, committed) = match self.host.registered.get() {
            Some(registered) => (
                registered.current_progress().await?,
                match registered.provider.as_ref() {
                    Some(provider) => provider.last_committed_lsn().await?,
                    None => 0,
                },
            ),
            None => (0, 0),
        };
        let (role_epoch, selected_role) = role.unwrap_or((Epoch::default(), ReplicaRole::None));
        let candidate_ready = !dynamic_pending
            && open
            && epoch == Some(role_epoch)
            && configuration.as_ref().is_some_and(|selected| {
                selected.current.epoch == role_epoch
                    && selected.current.members.iter().any(|member| {
                        member.identity == self.host.identity && member.role == selected_role
                    })
            });
        let ambiguity_fenced = self
            .host
            .replacement_ambiguity_fenced
            .load(Ordering::Acquire);
        if allow_reopen && candidate_ready && !ambiguity_fenced {
            self.host
                .replacement_authority_fenced
                .store(false, Ordering::Release);
        }
        let ready = candidate_ready
            && applied == committed
            && !ambiguity_fenced
            && !self
                .host
                .replacement_authority_fenced
                .load(Ordering::Acquire)
            && !self.host.replacement_fault_fenced.load(Ordering::Acquire);
        let (role, read_status, write_status) = if ready {
            match selected_role {
                ReplicaRole::Primary => {
                    (selected_role, AccessStatus::Granted, AccessStatus::Granted)
                }
                ReplicaRole::ActiveSecondary => (
                    selected_role,
                    AccessStatus::Granted,
                    AccessStatus::NotPrimary,
                ),
                _ => (
                    ReplicaRole::None,
                    AccessStatus::NotPrimary,
                    AccessStatus::NotPrimary,
                ),
            }
        } else {
            (
                ReplicaRole::None,
                AccessStatus::NotPrimary,
                AccessStatus::NotPrimary,
            )
        };
        let mut state = self.host.state.write().await;
        state.projection.open = open;
        state.projection.role = role;
        state.projection.read_status = read_status;
        state.projection.write_status = write_status;
        state.projection.current_progress = applied;
        state.projection.committed_lsn = committed;
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
        self.host
            .replacement_authority_fenced
            .store(true, Ordering::Release);
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
            .and_then(|registered| registered.stateless_runtime_control.clone())
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
            .and_then(|registered| registered.stateless_runtime_control.clone())
            .ok_or(RuntimeError::NotOpen)?
            .revoke_inbound_build_authority(authority)
            .await
    }

    pub(crate) fn replacement_raw_retained_task_cardinality(&self) -> Result<(usize, usize)> {
        self.host
            .registered
            .get()
            .and_then(|registered| registered.stateless_runtime_control.clone())
            .ok_or(RuntimeError::NotOpen)
            .map(|control| control.raw_retained_task_cardinality())
    }

    pub(crate) async fn replacement_report_projection(
        &self,
        process_session_id: ProcessSessionId,
        report_sequence: u64,
    ) -> Result<crate::host::public_lifecycle::PublicV1ReportProjection> {
        use crate::protocol::public_operations::{PublicProgress, PublicProgressRole};

        let (applied, committed) = match self.host.registered.get() {
            Some(registered) => (
                registered.current_progress().await?,
                match registered.provider.as_ref() {
                    Some(provider) => provider.last_committed_lsn().await?,
                    None => 0,
                },
            ),
            None => (0, 0),
        };
        let state = self.host.state.read().await;
        let role = state.projection.role;
        Ok(crate::host::public_lifecycle::PublicV1ReportProjection {
            process_session_id,
            report_sequence,
            role,
            read_status: state.projection.read_status,
            write_status: state.projection.write_status,
            progress: PublicProgress {
                role: match role {
                    ReplicaRole::Primary => PublicProgressRole::Primary,
                    ReplicaRole::ActiveSecondary => PublicProgressRole::Secondary,
                    _ => PublicProgressRole::Recovery,
                },
                first: committed,
                last: applied,
            },
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
        Ok(self
            .host
            .registered
            .get()
            .ok_or(RuntimeError::NotOpen)?
            .identity
            .to_public_id())
    }

    pub(crate) async fn diagnostic_projection(
        &self,
    ) -> (
        bool,
        Option<String>,
        ReplicaRole,
        AccessStatus,
        AccessStatus,
        i64,
        i64,
    ) {
        let projection = self.host.state.read().await.projection.clone();
        (
            projection.open,
            projection.replication_address,
            projection.role,
            projection.read_status,
            projection.write_status,
            projection.current_progress,
            projection.committed_lsn,
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
}

impl RuntimeHost {
    fn abort(&self) {
        if self.closed.load(Ordering::Acquire) || self.aborted.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(registered) = self.registered.get() {
            registered
                .provider_writer_lease_consumed
                .store(false, Ordering::Release);
            registered.abort();
        }
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
    fn partition_information(&self) -> PartitionInformation {
        self.partition_information.clone()
    }

    async fn read_status(&self) -> Result<AccessStatus> {
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        if host.replacement_fault_fenced.load(Ordering::Acquire) {
            return Ok(AccessStatus::NotPrimary);
        }
        Ok(host.state.read().await.projection.read_status)
    }

    async fn write_status(&self) -> Result<AccessStatus> {
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        if host.replacement_fault_fenced.load(Ordering::Acquire) {
            return Ok(AccessStatus::NotPrimary);
        }
        Ok(host.state.read().await.projection.write_status)
    }

    async fn publish_replacement_progress(
        &self,
        applied_lsn: i64,
        committed_lsn: i64,
        ambiguous: bool,
    ) -> Result<()> {
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        if host.closed.load(Ordering::Acquire) || host.aborted.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        host.replacement_ambiguity_fenced
            .store(ambiguous, Ordering::Release);
        let fenced = ambiguous
            || applied_lsn != committed_lsn
            || host.replacement_authority_fenced.load(Ordering::Acquire)
            || host.replacement_fault_fenced.load(Ordering::Acquire);
        let mut state = host.state.write().await;
        state.projection.current_progress = applied_lsn;
        state.projection.committed_lsn = committed_lsn;
        let (read, write) = if fenced {
            (AccessStatus::NotPrimary, AccessStatus::NotPrimary)
        } else {
            match state.projection.role {
                ReplicaRole::Primary => (AccessStatus::Granted, AccessStatus::Granted),
                ReplicaRole::ActiveSecondary => (AccessStatus::Granted, AccessStatus::NotPrimary),
                _ => (AccessStatus::NotPrimary, AccessStatus::NotPrimary),
            }
        };
        state.projection.read_status = read;
        state.projection.write_status = write;
        Ok(())
    }

    async fn replacement_inbound_build_authority(
        &self,
        build_id: &OperationId,
    ) -> Result<Option<crate::protocol::public_operations::PublicInboundBuildAuthority>> {
        self.bridge()
            .await?
            .store
            .public_v1_inbound_build_authority(build_id)
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))
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

    async fn replacement_terminal_inbound_build_receipt(
        &self,
        build_id: &OperationId,
    ) -> Result<Option<crate::protocol::public_operations::PublicInboundBuildReceipt>> {
        self.bridge()
            .await?
            .store
            .public_v1_terminal_inbound_build_receipt(build_id)
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

    async fn replacement_outbound_build_authority(
        &self,
        build_id: &OperationId,
        current_configuration_id: &crate::protocol::types::ConfigurationId,
    ) -> Result<Option<crate::protocol::public_operations::PublicInboundBuildAuthority>> {
        self.bridge()
            .await?
            .store
            .public_v1_outbound_build_authority(build_id, current_configuration_id)
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))
    }

    async fn complete_replacement_inbound_build(
        &self,
        receipt: &crate::protocol::public_operations::PublicInboundBuildReceipt,
    ) -> Result<()> {
        self.bridge()
            .await?
            .store
            .complete_public_v1_inbound_build(receipt)
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))
    }

    async fn replacement_storage_is_retired(
        &self,
        storage: &crate::protocol::types::ReplicaStorageIncarnationContext,
    ) -> Result<bool> {
        self.bridge()
            .await?
            .store
            .public_v1_storage_is_retired(storage)
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
        let reservation = ReplicatorCreationReservation::new(RuntimeHostToken::new());
        let mut creation = self.replicator_creation.lock().map_err(|_| {
            RuntimeError::Application("replicator creation state was poisoned".into())
        })?;
        match &*creation {
            ReplicatorCreationState::Available => {
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
            *creation = ReplicatorCreationState::Available;
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
                stateless_runtime_control: attachment
                    .stateless_runtime_control(RuntimeHostToken::new()),
                capabilities: attachment.capabilities(RuntimeHostToken::new()),
                provider,
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
