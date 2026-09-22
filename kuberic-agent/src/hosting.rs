//! Service Fabric-aligned process hosting boundary.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use kuberic_protocol::types::{AccessStatus, Epoch, ReplicaIdentity, ReplicaRole};
use kuberic_runtime::application::{
    ClientWrite, OpenContext, OpenMode, StateProvider, StatefulServiceReplica,
};
use kuberic_runtime::internal::{OutboundReplication, PendingReplication, PendingWrite};
use kuberic_runtime::replicator::copy::{PrepareCopyRequest, PreparedCopy};
use kuberic_runtime::replicator::{
    DefaultReplicatorDependencies, ManagedReplicator, PartitionAccessView, PrimaryReplicator,
    Replicator, ReplicatorCreationReservation, ReplicatorFactoryContext, ReplicatorInterfaces,
    ReplicatorRegistration, StatefulServicePartition,
};
use kuberic_runtime::{Result, RuntimeError};
use kuberic_runtime_internal::authority::{
    AuthorityStore, BuildAuthorityStore, BuildProgressStore, LocalWriteJournal,
    ReplicaAuthorityStore, ReplicationProgressStore,
};
use kuberic_runtime_internal::effects::{
    RoleTransition, RuntimeEffect, RuntimeEffectAction, RuntimeEffectResult, RuntimePostcondition,
    RuntimeSnapshot,
};
use kuberic_wire::proto;
use tokio::sync::{Mutex, RwLock};

const REPLICATOR_CREATION_AVAILABLE: u8 = 0;
const REPLICATOR_CREATION_RESERVED: u8 = 1;
const REPLICATOR_CREATION_REGISTERED: u8 = 2;
const REPLICATOR_RESERVATION_ID: u64 = 1;

#[async_trait]
pub trait RuntimeControlPlane: Send {
    async fn next_effect(&mut self) -> Result<Option<RuntimeEffect>>;

    async fn publish(&mut self, result: RuntimeEffectResult) -> Result<()>;
}

#[derive(Debug, Clone)]
struct AppliedEffect {
    effect: RuntimeEffect,
    result: RuntimeEffectResult,
}

#[derive(Debug)]
struct HostState {
    effects: BTreeMap<u64, AppliedEffect>,
    fallback_snapshot: RuntimeSnapshot,
}

struct RegisteredReplicator {
    control: Arc<dyn Replicator>,
    primary: Option<Arc<dyn PrimaryReplicator>>,
    provider: Arc<dyn StateProvider>,
    managed: Option<Arc<dyn ManagedReplicator>>,
}

pub struct PodRuntime {
    host: Arc<RuntimeHost>,
}

impl Drop for PodRuntime {
    fn drop(&mut self) {
        self.host.abort();
    }
}

impl PodRuntime {
    pub fn new<A, S>(
        identity: ReplicaIdentity,
        application: Arc<A>,
        authority_store: Arc<S>,
    ) -> Self
    where
        A: StatefulServiceReplica + 'static,
        S: AuthorityStore + 'static,
    {
        let application: Arc<dyn StatefulServiceReplica> = application;
        let replica_authority_store: Arc<dyn ReplicaAuthorityStore> = authority_store.clone();
        let replication_progress_store: Arc<dyn ReplicationProgressStore> = authority_store.clone();
        let local_write_journal: Arc<dyn LocalWriteJournal> = authority_store.clone();
        let build_authority_store: Arc<dyn BuildAuthorityStore> = authority_store.clone();
        let build_progress_store: Arc<dyn BuildProgressStore> = authority_store;
        let fallback_snapshot = empty_snapshot(identity.clone());
        Self {
            host: Arc::new_cyclic(|weak_self| RuntimeHost {
                identity,
                application: application.clone(),
                default_dependencies: DefaultReplicatorDependencies {
                    application,
                    replica_authority_store,
                    replication_progress_store,
                    local_write_journal,
                    build_authority_store,
                    build_progress_store,
                },
                state: RwLock::new(HostState {
                    effects: BTreeMap::new(),
                    fallback_snapshot,
                }),
                effect_lock: Mutex::new(()),
                registered: OnceLock::new(),
                weak_self: weak_self.clone(),
                aborted: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                replicator_creation: AtomicU8::new(REPLICATOR_CREATION_AVAILABLE),
            }),
        }
    }

    pub async fn serve<C: RuntimeControlPlane>(&self, control_plane: &mut C) -> Result<()> {
        while let Some(effect) = control_plane.next_effect().await? {
            let result = self.apply_effect(effect).await?;
            control_plane.publish(result).await?;
        }
        Ok(())
    }

    pub async fn restore_authority(&self) -> Result<()> {
        self.host.managed()?.restore_authority().await
    }

    pub async fn apply_effect(&self, effect: RuntimeEffect) -> Result<RuntimeEffectResult> {
        self.host.apply_effect(effect).await
    }

    pub async fn begin_write(&self, write: ClientWrite) -> Result<PendingWrite> {
        self.host.managed()?.begin_write(write).await
    }

    pub async fn accept_acknowledgement(
        &self,
        acknowledgement: proto::ReplicationAck,
    ) -> Result<()> {
        self.host
            .managed()?
            .accept_acknowledgement(acknowledgement)
            .await
    }

    pub async fn prepare_copy(&self, request: PrepareCopyRequest) -> Result<PreparedCopy> {
        self.host.managed()?.prepare_copy(request).await
    }

    pub async fn accept_copy_acknowledgement(&self, ack: proto::CopyAck) -> Result<()> {
        self.host.managed()?.accept_copy_acknowledgement(ack).await
    }

    pub async fn receive_copy_item(&self, item: proto::CopyItem) -> Result<proto::CopyAck> {
        self.host.managed()?.receive_copy_item(item).await
    }

    pub async fn receive_replication(
        &self,
        item: proto::ReplicationItem,
    ) -> Result<PendingReplication> {
        self.host.managed()?.receive_replication(item).await
    }

    pub async fn snapshot(&self) -> RuntimeSnapshot {
        self.host.snapshot().await
    }

    pub async fn next_outbound(&self) -> Option<OutboundReplication> {
        self.host.managed().ok()?.next_outbound().await
    }

    pub async fn primary_replicator(&self) -> Result<Arc<dyn PrimaryReplicator>> {
        self.host
            .registered
            .get()
            .and_then(|registered| registered.primary.clone())
            .ok_or(RuntimeError::NotOpen)
    }
}

struct RuntimeHost {
    identity: ReplicaIdentity,
    application: Arc<dyn StatefulServiceReplica>,
    default_dependencies: DefaultReplicatorDependencies,
    state: RwLock<HostState>,
    effect_lock: Mutex<()>,
    registered: OnceLock<RegisteredReplicator>,
    weak_self: Weak<Self>,
    aborted: AtomicBool,
    closed: AtomicBool,
    replicator_creation: AtomicU8,
}

struct HostAccessView {
    host: Weak<RuntimeHost>,
}

#[async_trait]
impl PartitionAccessView for HostAccessView {
    async fn write_status(&self) -> Result<AccessStatus> {
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        Ok(host.snapshot().await.write_status)
    }
}

#[async_trait]
impl ReplicatorRegistration for RuntimeHost {
    fn reserve_replicator_creation(&self) -> Result<ReplicatorCreationReservation> {
        self.replicator_creation
            .compare_exchange(
                REPLICATOR_CREATION_AVAILABLE,
                REPLICATOR_CREATION_RESERVED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| {
                RuntimeError::Application(
                    "CreateReplicator may be called only once per Open".into(),
                )
            })?;
        Ok(ReplicatorCreationReservation(REPLICATOR_RESERVATION_ID))
    }

    fn cancel_replicator_creation(&self, reservation: ReplicatorCreationReservation) {
        if reservation.0 == REPLICATOR_RESERVATION_ID {
            let _ = self.replicator_creation.compare_exchange(
                REPLICATOR_CREATION_RESERVED,
                REPLICATOR_CREATION_AVAILABLE,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }

    async fn register_interfaces(
        &self,
        interfaces: &ReplicatorInterfaces,
        provider: Arc<dyn StateProvider>,
        reservation: ReplicatorCreationReservation,
    ) -> Result<()> {
        if reservation.0 != REPLICATOR_RESERVATION_ID
            || self.replicator_creation.load(Ordering::Acquire) != REPLICATOR_CREATION_RESERVED
        {
            return Err(RuntimeError::Application(
                "CreateReplicator reservation is not active".into(),
            ));
        }
        let managed_replicator = interfaces.managed_replicator();
        if let Some(managed) = managed_replicator.as_ref() {
            managed
                .attach_interfaces(
                    interfaces.replicator.clone(),
                    interfaces.primary_replicator.clone(),
                )
                .await?;
        }
        self.registered
            .set(RegisteredReplicator {
                control: interfaces.replicator.clone(),
                primary: interfaces.primary_replicator.clone(),
                provider,
                managed: managed_replicator,
            })
            .map_err(|_| {
                RuntimeError::Application(
                    "CreateReplicator may be called only once per Open".into(),
                )
            })?;
        self.replicator_creation
            .compare_exchange(
                REPLICATOR_CREATION_RESERVED,
                REPLICATOR_CREATION_REGISTERED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| {
                RuntimeError::Application(
                    "CreateReplicator reservation was lost before registration".into(),
                )
            })?;
        Ok(())
    }
}

struct OpenAttempt<'a> {
    host: &'a RuntimeHost,
    complete: bool,
}

impl Drop for OpenAttempt<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.host.abort();
        }
    }
}

impl RuntimeHost {
    fn managed(&self) -> Result<Arc<dyn ManagedReplicator>> {
        let registered = self.registered.get().ok_or(RuntimeError::NotOpen)?;
        registered.managed.clone().ok_or_else(|| {
            RuntimeError::Application(
                "the selected replicator does not expose Kuberic managed data-plane capabilities"
                    .into(),
            )
        })
    }

    fn abort(&self) {
        if self.closed.load(Ordering::Acquire) || self.aborted.swap(true, Ordering::AcqRel) {
            return;
        }
        self.application.abort();
        if let Some(registered) = self.registered.get() {
            registered.control.abort();
            if let Some(managed) = registered.managed.as_ref() {
                managed.abort();
            }
        }
    }

    async fn apply_effect(&self, effect: RuntimeEffect) -> Result<RuntimeEffectResult> {
        let _guard = self.effect_lock.lock().await;
        {
            let state = self.state.read().await;
            if let Some(previous) = state.effects.get(&effect.sequence) {
                if effect == previous.effect {
                    return Ok(previous.result.clone());
                }
                return Err(RuntimeError::EffectConflict {
                    sequence: effect.sequence,
                });
            }
            let expected = state
                .effects
                .last_key_value()
                .map_or(1, |(sequence, _)| sequence + 1);
            if effect.sequence != expected {
                return Err(RuntimeError::EffectOutOfOrder {
                    expected,
                    observed: effect.sequence,
                });
            }
        }
        if !matches!(effect.action, RuntimeEffectAction::Abort)
            && (self.aborted.load(Ordering::Acquire) || self.closed.load(Ordering::Acquire))
        {
            return Err(RuntimeError::Closed);
        }
        match effect.action.clone() {
            RuntimeEffectAction::Open(mode) => self.open(mode).await?,
            RuntimeEffectAction::ChangeRole(role) => self.change_role(role).await?,
            RuntimeEffectAction::Close => self.close().await?,
            RuntimeEffectAction::Abort => self.abort_action().await,
            action => {
                if let Ok(managed) = self.managed() {
                    let result = managed.execute_action(action).await;
                    self.sync_access_projection(managed.as_ref()).await;
                    result?;
                } else {
                    self.execute_custom_action(action).await?;
                }
            }
        }
        let result = RuntimeEffectResult {
            operation_id: effect.operation_id.clone(),
            sequence: effect.sequence,
            postcondition: snapshot_postcondition(self.snapshot().await),
        };
        self.state.write().await.effects.insert(
            result.sequence,
            AppliedEffect {
                effect,
                result: result.clone(),
            },
        );
        Ok(result)
    }

    async fn open(&self, mode: OpenMode) -> Result<()> {
        if self.registered.get().is_some() {
            return Err(RuntimeError::Application("replica already opened".into()));
        }
        let mut attempt = OpenAttempt {
            host: self,
            complete: false,
        };
        let context = ReplicatorFactoryContext::new(
            self.identity.clone(),
            Arc::new(HostAccessView {
                host: self.weak_self.clone(),
            }),
            self.default_dependencies.clone(),
        );
        let registration: Arc<dyn ReplicatorRegistration> =
            self.weak_self.upgrade().ok_or(RuntimeError::Closed)?;
        let control = self
            .application
            .clone()
            .open(OpenContext {
                identity: self.identity.clone(),
                mode,
                partition: StatefulServicePartition::new(registration, context),
            })
            .await?;
        let registered = self.registered.get().ok_or(RuntimeError::NotOpen)?;
        if !Arc::ptr_eq(&control, &registered.control) {
            control.abort();
            return Err(RuntimeError::Application(
                "Open returned a different replicator than CreateReplicator".into(),
            ));
        }
        let address = registered.control.open().await?;
        if let Some(managed) = registered.managed.as_ref() {
            managed.complete_open(address.clone()).await?;
            self.sync_access_projection(managed.as_ref()).await;
        } else {
            let progress = registered.control.current_progress().await?;
            let committed = registered.provider.last_committed_lsn().await?;
            let mut state = self.state.write().await;
            state.fallback_snapshot.open = true;
            state.fallback_snapshot.current_progress = progress;
            state.fallback_snapshot.committed_lsn = committed;
        }
        {
            let mut state = self.state.write().await;
            state.fallback_snapshot.open = true;
            state.fallback_snapshot.replication_address = Some(address);
        }
        attempt.complete = true;
        Ok(())
    }

    async fn change_role(&self, role: ReplicaRole) -> Result<()> {
        let registered = self.registered.get().ok_or(RuntimeError::NotOpen)?;
        let snapshot = self.snapshot().await;
        if !snapshot.open {
            return Err(RuntimeError::NotOpen);
        }
        let epoch = snapshot
            .authority
            .as_ref()
            .map_or_else(Epoch::default, |authority| {
                authority.current_configuration.epoch
            });
        let transition = {
            let mut state = self.state.write().await;
            state.fallback_snapshot.write_status = AccessStatus::ReconfigurationPending;
            if let Some(transition) = state.fallback_snapshot.role_transition.clone() {
                if transition.target_role != role {
                    return Err(RuntimeError::ReconfigurationPending);
                }
                transition
            } else {
                let transition = RoleTransition {
                    completed_role: snapshot.role,
                    target_role: role,
                    replicator_completed: false,
                    application_completed: false,
                };
                state.fallback_snapshot.role_transition = Some(transition.clone());
                transition
            }
        };
        if let Ok(managed) = self.managed() {
            managed.fence_writes().await?;
        }
        if !transition.replicator_completed {
            registered.control.change_role(epoch, role).await?;
            let mut state = self.state.write().await;
            state
                .fallback_snapshot
                .role_transition
                .as_mut()
                .ok_or(RuntimeError::ReconfigurationPending)?
                .replicator_completed = true;
        }
        if !transition.application_completed {
            let _ = self.application.change_role(role).await?;
        }
        let mut state = self.state.write().await;
        state
            .fallback_snapshot
            .role_transition
            .as_mut()
            .ok_or(RuntimeError::ReconfigurationPending)?
            .application_completed = true;
        state.fallback_snapshot.role = role;
        state.fallback_snapshot.role_transition = None;
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        let registered = self.registered.get().ok_or(RuntimeError::NotOpen)?;
        {
            let mut state = self.state.write().await;
            state.fallback_snapshot.open = false;
            state.fallback_snapshot.write_status = AccessStatus::ReconfigurationPending;
        }
        if let Ok(managed) = self.managed() {
            managed.fence_writes().await?;
        }
        if let Err(error) = registered.control.close().await {
            self.abort();
            return Err(error);
        }
        if let Err(error) = self.application.close().await {
            self.application.abort();
            self.closed.store(true, Ordering::Release);
            let mut state = self.state.write().await;
            state.fallback_snapshot.role = ReplicaRole::None;
            state.fallback_snapshot.role_transition = None;
            state.fallback_snapshot.write_status = AccessStatus::NotPrimary;
            return Err(error);
        }
        self.closed.store(true, Ordering::Release);
        let mut state = self.state.write().await;
        state.fallback_snapshot.role = ReplicaRole::None;
        state.fallback_snapshot.role_transition = None;
        state.fallback_snapshot.write_status = AccessStatus::NotPrimary;
        Ok(())
    }

    async fn abort_action(&self) {
        {
            let mut state = self.state.write().await;
            state.fallback_snapshot.open = false;
            state.fallback_snapshot.role = ReplicaRole::None;
            state.fallback_snapshot.role_transition = None;
            state.fallback_snapshot.write_status = AccessStatus::NotPrimary;
        }
        self.abort();
    }

    async fn sync_access_projection(&self, managed: &dyn ManagedReplicator) {
        let managed_snapshot = managed.snapshot().await;
        let mut state = self.state.write().await;
        state.fallback_snapshot.write_status = managed_snapshot.write_status;
        state.fallback_snapshot.authority = managed_snapshot.authority;
    }

    async fn execute_custom_action(&self, action: RuntimeEffectAction) -> Result<()> {
        let registered = self.registered.get().ok_or(RuntimeError::NotOpen)?;
        match action {
            RuntimeEffectAction::SetWriteStatus(status) => {
                self.state.write().await.fallback_snapshot.write_status = status;
            }
            RuntimeEffectAction::RefreshApplicationProgress => {
                let current = registered.control.current_progress().await?;
                let committed = registered.provider.last_committed_lsn().await?;
                let mut state = self.state.write().await;
                state.fallback_snapshot.current_progress = current;
                state.fallback_snapshot.committed_lsn = committed;
            }
            RuntimeEffectAction::AdmitAuthority(_)
            | RuntimeEffectAction::AdmitBuildAuthority(_)
            | RuntimeEffectAction::RetireBuild(_) => {
                return Err(RuntimeError::Application(
                    "the selected custom replicator does not expose managed authority/build capabilities"
                        .into(),
                ));
            }
            RuntimeEffectAction::Open(_)
            | RuntimeEffectAction::ChangeRole(_)
            | RuntimeEffectAction::Close
            | RuntimeEffectAction::Abort => {
                return Err(RuntimeError::Application(
                    "application lifecycle actions belong to the hosting runtime".into(),
                ));
            }
        }
        Ok(())
    }

    async fn snapshot(&self) -> RuntimeSnapshot {
        if let Ok(managed) = self.managed() {
            let mut snapshot = managed.snapshot().await;
            let host = self.state.read().await.fallback_snapshot.clone();
            snapshot.open = host.open;
            snapshot.replication_address = host.replication_address;
            snapshot.role = host.role;
            snapshot.role_transition = host.role_transition;
            snapshot.write_status = host.write_status;
            snapshot.authority = host.authority.or(snapshot.authority);
            if self.aborted.load(Ordering::Acquire) {
                snapshot.open = false;
            }
            snapshot
        } else {
            let mut snapshot = self.state.read().await.fallback_snapshot.clone();
            if self.aborted.load(Ordering::Acquire) {
                snapshot.open = false;
            }
            snapshot
        }
    }
}

fn empty_snapshot(identity: ReplicaIdentity) -> RuntimeSnapshot {
    RuntimeSnapshot {
        identity,
        open: false,
        replication_address: None,
        role: ReplicaRole::None,
        role_transition: None,
        write_status: AccessStatus::NotPrimary,
        authority: None,
        current_progress: 0,
        verified_replication_lsn: None,
        committed_lsn: 0,
        current_configuration_quorum_progress: 0,
        catch_up_boundary: None,
        catch_up_complete: false,
        builds: Vec::new(),
    }
}

fn snapshot_postcondition(snapshot: RuntimeSnapshot) -> RuntimePostcondition {
    RuntimePostcondition {
        open: snapshot.open,
        role: snapshot.role,
        role_transition: snapshot.role_transition,
        write_status: snapshot.write_status,
        authority: snapshot.authority,
        current_progress: snapshot.current_progress,
        verified_replication_lsn: snapshot.verified_replication_lsn,
        committed_lsn: snapshot.committed_lsn,
        current_configuration_quorum_progress: snapshot.current_configuration_quorum_progress,
        catch_up_boundary: snapshot.catch_up_boundary,
        catch_up_complete: snapshot.catch_up_complete,
        builds: snapshot.builds,
    }
}
