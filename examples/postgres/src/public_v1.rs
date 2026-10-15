//! Dormant Public V1 PostgreSQL owner. No report poll drives this lifecycle.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use kuberic_runtime::protocol::public_operations::{
    LocalProviderRebuildRequired, ProviderReplicaIdentity, RemoteBuildTargetRebuildRequired,
    ReplicaRebuildReason, ReplicaRebuildRequired,
};
use kuberic_runtime::protocol::types::{ConfigurationId, ProcessSessionId, ResourceUid};
use kuberic_runtime::replicator::ReplicatorSettings;
use serde::{Deserialize, Serialize};

use super::public_transport::{NativeCredential, NativeGateway};
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
struct LifecycleContext {
    identity: ReplicaIdentity,
    role: ReplicaRole,
    epoch: Epoch,
    previous_configuration_id: Option<ConfigurationId>,
    current_configuration_id: Option<ConfigurationId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalProcessBinding {
    identity: ReplicaIdentity,
    process_session_id: ProcessSessionId,
    replication_address: String,
}

#[derive(Clone)]
struct LifecycleFence {
    local: Arc<LocalProcessBinding>,
    current: Arc<std::sync::Mutex<LifecycleContext>>,
}

impl LifecycleFence {
    fn new(
        identity: ReplicaIdentity,
        process_session_id: ProcessSessionId,
        replication_address: String,
    ) -> Self {
        Self {
            local: Arc::new(LocalProcessBinding {
                identity: identity.clone(),
                process_session_id,
                replication_address,
            }),
            current: Arc::new(std::sync::Mutex::new(LifecycleContext {
                identity,
                role: ReplicaRole::None,
                epoch: Epoch::default(),
                previous_configuration_id: None,
                current_configuration_id: None,
            })),
        }
    }

    fn context(&self) -> Result<LifecycleContext> {
        Ok(self.current.lock().map_err(application_error)?.clone())
    }

    fn update(&self, change: impl FnOnce(&mut LifecycleContext)) -> Result<LifecycleContext> {
        let mut context = self.current.lock().map_err(application_error)?;
        change(&mut context);
        Ok(context.clone())
    }

    fn validate_process_binding(&self, binding: &TransportBinding) -> Result<LifecycleContext> {
        let context = self.context()?;
        let local = if binding.source() == &context.identity {
            binding.source_endpoint()
        } else if binding.target() == &context.identity {
            binding.target_endpoint()
        } else {
            return Err(RuntimeError::AuthorityNotAdmitted);
        };
        if local.replica != self.local.identity
            || local.process_session_id != self.local.process_session_id
            || local.replication_address != self.local.replication_address
            || local.role != context.role
            || binding.epoch() != context.epoch
            || binding.source() == binding.target()
            || binding.source_endpoint().process_session_id.is_empty()
            || binding.target_endpoint().process_session_id.is_empty()
            || binding.source_address().is_empty()
            || binding.target_address().is_empty()
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(context)
    }

    fn validate_binding(&self, binding: &TransportBinding) -> Result<LifecycleContext> {
        let context = self.validate_process_binding(binding)?;
        if let TransportBinding::Peer(peer) = binding
            && (Some(&peer.current_configuration_id) != context.current_configuration_id.as_ref()
                || peer.previous_configuration_id != context.previous_configuration_id)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(context)
    }

    fn admit_peer(&self, binding: &PeerBinding) -> Result<LifecycleContext> {
        let transport = TransportBinding::Peer(binding.clone());
        let mut context = self.current.lock().map_err(application_error)?;
        let local = if transport.target() == &context.identity {
            transport.target_endpoint()
        } else {
            return Err(RuntimeError::AuthorityNotAdmitted);
        };
        if local.replica != self.local.identity
            || local.process_session_id != self.local.process_session_id
            || local.replication_address != self.local.replication_address
            || local.role != context.role
            || context.role != ReplicaRole::ActiveSecondary
            || binding.epoch != context.epoch
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let transition_admitted = match (
            context.current_configuration_id.as_ref(),
            context.previous_configuration_id.as_ref(),
        ) {
            (None, None) => true,
            (Some(current), None) => {
                &binding.current_configuration_id == current
                    || binding.previous_configuration_id.as_ref() == Some(current)
            }
            (Some(current), Some(previous)) => {
                (&binding.current_configuration_id == current
                    && (binding.previous_configuration_id.as_ref() == Some(previous)
                        || binding.previous_configuration_id.is_none()))
                    || binding.previous_configuration_id.as_ref() == Some(current)
            }
            (None, Some(_)) => false,
        };
        if !transition_admitted {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        context.previous_configuration_id = binding.previous_configuration_id.clone();
        context.current_configuration_id = Some(binding.current_configuration_id.clone());
        Ok(context.clone())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeEndpoint {
    replica: ReplicaIdentity,
    process_session_id: ProcessSessionId,
    replication_address: String,
    role: ReplicaRole,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BuildBinding {
    resource_uid: ResourceUid,
    epoch: Epoch,
    build_id: OperationId,
    source: NativeEndpoint,
    target: NativeEndpoint,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PeerBinding {
    epoch: Epoch,
    previous_configuration_id: Option<ConfigurationId>,
    current_configuration_id: ConfigurationId,
    primary: NativeEndpoint,
    secondary: NativeEndpoint,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "binding", deny_unknown_fields)]
pub(crate) enum TransportBinding {
    Build(BuildBinding),
    Peer(PeerBinding),
}

impl TransportBinding {
    fn source(&self) -> &ReplicaIdentity {
        match self {
            Self::Build(value) => &value.source.replica,
            Self::Peer(value) => &value.primary.replica,
        }
    }

    fn target(&self) -> &ReplicaIdentity {
        match self {
            Self::Build(value) => &value.target.replica,
            Self::Peer(value) => &value.secondary.replica,
        }
    }

    fn source_address(&self) -> &str {
        match self {
            Self::Build(value) => &value.source.replication_address,
            Self::Peer(value) => &value.primary.replication_address,
        }
    }

    fn target_address(&self) -> &str {
        match self {
            Self::Build(value) => &value.target.replication_address,
            Self::Peer(value) => &value.secondary.replication_address,
        }
    }

    fn source_endpoint(&self) -> &NativeEndpoint {
        match self {
            Self::Build(value) => &value.source,
            Self::Peer(value) => &value.primary,
        }
    }

    fn target_endpoint(&self) -> &NativeEndpoint {
        match self {
            Self::Build(value) => &value.target,
            Self::Peer(value) => &value.secondary,
        }
    }

    fn epoch(&self) -> Epoch {
        match self {
            Self::Build(value) => value.epoch,
            Self::Peer(value) => value.epoch,
        }
    }
}

pub(super) struct PublicPg {
    fence: LifecycleFence,
    state: Mutex<PublicState>,
    work: std::sync::Mutex<CancellationToken>,
    #[cfg(all(test, feature = "testing"))]
    completion_gate: std::sync::Mutex<Option<Arc<BuildCompletionGate>>>,
}

#[cfg(all(test, feature = "testing"))]
pub(crate) struct BuildCompletionGate {
    pub(crate) entered: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
}

impl PublicPg {
    fn new(
        identity: ReplicaIdentity,
        process_session_id: ProcessSessionId,
        replication_address: String,
    ) -> Self {
        Self {
            fence: LifecycleFence::new(identity, process_session_id, replication_address),
            state: Mutex::new(PublicState {
                opened: false,
                role: ReplicaRole::None,
                role_epoch: Epoch::default(),
                epoch: None,
                configuration: None,
                previous: None,
                grants: Vec::new(),
                receiver: None,
                access: None,
                built: None,
            }),
            work: std::sync::Mutex::new(CancellationToken::new()),
            #[cfg(all(test, feature = "testing"))]
            completion_gate: std::sync::Mutex::new(None),
        }
    }

    fn cancel(&self) {
        let mut work = self.work.lock().unwrap();
        work.cancel();
        *work = CancellationToken::new();
    }

    #[cfg(all(test, feature = "testing"))]
    fn pause_completion(&self) -> Arc<BuildCompletionGate> {
        let gate = Arc::new(BuildCompletionGate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *self.completion_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    #[cfg(all(test, feature = "testing"))]
    async fn completion_checkpoint(&self) {
        let gate = self.completion_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
            *self.completion_gate.lock().unwrap() = None;
        }
    }
}

struct PublicState {
    opened: bool,
    role: ReplicaRole,
    role_epoch: Epoch,
    epoch: Option<Epoch>,
    configuration: Option<ReplicaSetConfiguration>,
    previous: Option<ConfigurationDescriptor>,
    grants: Vec<Grant>,
    receiver: Option<TransportBinding>,
    access: Option<LifecycleContext>,
    built: Option<BuildCompletion>,
}

struct Grant {
    binding: TransportBinding,
    credential: NativeCredential,
    gateway: NativeGateway,
    active: CancellationToken,
    session: ProcessSessionId,
    lineage: PgLineage,
    boundary: i64,
}

struct BuildGrantGuard {
    active: CancellationToken,
    armed: bool,
}

impl Drop for BuildGrantGuard {
    fn drop(&mut self) {
        if self.armed {
            self.active.cancel();
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuildCompletion {
    binding: BuildBinding,
    lineage: PgLineage,
    boundary: i64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", deny_unknown_fields)]
pub(crate) enum Request {
    Inspect(TransportBinding),
    Build(BuildBinding),
    Peer(PeerBinding),
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", deny_unknown_fields)]
pub(crate) enum Response {
    Source {
        binding: TransportBinding,
        credential: NativeCredential,
        lineage: PgLineage,
        boundary: i64,
    },
    Built(BuildCompletion),
    Peer(PeerBinding),
    Rebuild(ReplicaRebuildRequired),
}

impl PgReplicator {
    pub(crate) fn is_public_v1(&self) -> bool {
        self.public_v1.get().is_some()
    }

    pub(crate) fn configure_public_v1(
        self: &Arc<Self>,
        settings: ReplicatorSettings,
        identity: ReplicaIdentity,
        process_session_id: ProcessSessionId,
    ) -> Result<()> {
        if settings.replication_address != self.coordination.local_endpoint {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.public_v1
            .set(PublicPg::new(
                identity,
                process_session_id,
                settings.replication_address,
            ))
            .map_err(|_| application_error("PostgreSQL construction is immutable"))?;
        self.start_public_owner();
        Ok(())
    }

    fn start_public_owner(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let cancelled = self.cancellation.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            loop {
                tokio::select! {
                    _ = cancelled.cancelled() => break,
                    _ = tick.tick() => {},
                }
                let Some(driver) = weak.upgrade() else {
                    break;
                };
                let public = driver.public_v1.get().expect("immutable owner");
                let (invalid, cancelled) =
                    {
                        let mut state = public.state.lock().await;
                        let invalid =
                            state.access.as_ref().is_some_and(|context| {
                                public
                                    .fence
                                    .context()
                                    .map_or(true, |current| &current != context)
                            }) || state.receiver.as_ref().is_some_and(|binding| {
                                public.fence.validate_binding(binding).is_err()
                            }) || state.grants.iter().any(|grant| {
                                public.fence.validate_binding(&grant.binding).is_err()
                            });
                        let mut cancelled = Vec::new();
                        let mut index = 0;
                        while index < state.grants.len() {
                            if state.grants[index].active.is_cancelled() {
                                cancelled.push(state.grants.remove(index));
                            } else {
                                index += 1;
                            }
                        }
                        (invalid, cancelled)
                    };
                for grant in cancelled {
                    if let Err(error) = grant.gateway.drain().await {
                        tracing::error!(%error, "cancelled public PostgreSQL grant drainage failed");
                        driver.abort();
                        return;
                    }
                }
                if invalid && let Err(error) = driver.public_revoke().await {
                    tracing::error!(%error, "public PostgreSQL owner drainage failed");
                    driver.abort();
                    break;
                } else if !invalid && let Err(error) = driver.public_reconcile_access().await {
                    tracing::error!(%error, "public PostgreSQL access reconciliation failed");
                    driver.abort();
                    break;
                }
            }
        });
    }

    fn public_owner(&self) -> &PublicPg {
        self.public_v1
            .get()
            .expect("immutable Public V1 construction")
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) fn pause_public_build_completion(&self) -> Arc<BuildCompletionGate> {
        self.public_owner().pause_completion()
    }

    async fn public_owned<T>(
        &self,
        work: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        let cancellation = self.public_owner().work.lock().unwrap().clone();
        let mut cleanup = BuildCleanup {
            instance: self.instance.clone(),
            armed: true,
        };
        let result = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(RuntimeError::Closed),
            _ = cancellation.cancelled() => Err(RuntimeError::OperationCancelled),
            result = tokio::time::timeout(Duration::from_secs(240), work) =>
                result.unwrap_or(Err(RuntimeError::OperationCancelled)),
        };
        if result.is_err() {
            self.instance.stop().await.map_err(application_error)?;
        }
        cleanup.armed = false;
        result
    }

    pub(super) async fn public_open(&self) -> Result<()> {
        let mut state = self.public_owner().state.lock().await;
        if state.opened || self.cancellation.is_cancelled() {
            return Err(RuntimeError::Closed);
        }
        self.durable.revalidate().await.map_err(application_error)?;
        self.instance
            .bind_generation_store(&self.durable)
            .await
            .map_err(application_error)?;
        state.opened = true;
        Ok(())
    }

    async fn public_drain(&self, state: &mut PublicState) -> Result<()> {
        state.access = None;
        // Stop first: this also joins pre-authentication SQL sockets, all native
        // senders/receivers, and in-flight base-backup helpers before replacement.
        self.instance.stop().await.map_err(application_error)?;
        for grant in state.grants.drain(..) {
            grant.gateway.drain().await?;
        }
        if self
            .instance
            .data_dir()
            .join("postgresql.auto.conf")
            .exists()
        {
            self.instance
                .config()
                .disconnect_receiver(self.instance.data_dir())
                .await
                .map_err(application_error)?;
            crate::config::PgConfig::public_replication_fence(self.instance.data_dir())
                .await
                .map_err(application_error)?;
        }
        state.receiver = None;
        state.configuration = None;
        state.previous = None;
        self.durable
            .update(|durable| {
                durable.external_access_closed = true;
                durable.postgres_stopped = true;
                durable.catch_up = None;
                Ok(())
            })
            .await
            .map_err(application_error)?;
        Ok(())
    }

    async fn public_revoke(&self) -> Result<()> {
        self.public_owner().cancel();
        let mut state = self.public_owner().state.lock().await;
        self.public_drain(&mut state).await
    }

    async fn public_reconcile_access(&self) -> Result<bool> {
        let mut state = self.public_owner().state.lock().await;
        let Ok(context) = self.public_owner().fence.context() else {
            return Ok(false);
        };
        if state.access.as_ref() == Some(&context) {
            return Ok(true);
        }
        if !state.opened
            || state.role != context.role
            || state.role_epoch != context.epoch
            || !matches!(
                state.role,
                ReplicaRole::Primary | ReplicaRole::ActiveSecondary
            )
        {
            return Ok(false);
        }
        let ready = match state.role {
            ReplicaRole::Primary => {
                if self.public_check_configuration(&state, &context).is_err() {
                    false
                } else {
                    match self.observer.snapshot().await {
                        Ok(snapshot) => snapshot.evidence.as_ref().is_some_and(|evidence| {
                            !evidence.in_recovery && snapshot.committed_lsn >= evidence.flush_lsn
                        }),
                        Err(error) if error.is_timeout() => false,
                        Err(error) => return Err(application_error(error)),
                    }
                }
            }
            ReplicaRole::ActiveSecondary => {
                state.epoch == Some(context.epoch)
                    && state.receiver.as_ref().is_some_and(|binding| {
                        self.public_owner().fence.validate_binding(binding).is_ok()
                    })
                    && self.instance.is_running().await
            }
            _ => false,
        };
        if !ready {
            return Ok(false);
        }
        PgAccessController::new(&self.instance)
            .grant_role_access()
            .await
            .map_err(application_error)?;
        if self.public_owner().fence.context()? != context {
            return Err(RuntimeError::OperationCancelled);
        }
        self.durable
            .update(|durable| {
                durable.external_access_closed = false;
                Ok(())
            })
            .await
            .map_err(application_error)?;
        state.access = Some(context);
        Ok(true)
    }

    pub(super) async fn public_close(&self) -> Result<()> {
        self.public_revoke().await?;
        self.public_owner().state.lock().await.opened = false;
        self.cancellation.cancel();
        Ok(())
    }

    async fn public_start(&self) -> Result<()> {
        crate::config::PgConfig::public_replication_fence(self.instance.data_dir())
            .await
            .map_err(application_error)?;
        self.instance
            .start_native_with_cancellation(self.fault_tx.clone(), self.cancellation.clone())
            .await
            .map_err(application_error)
    }

    async fn public_snapshot_and_persist(&self) -> Result<crate::native::PgObservation> {
        loop {
            match self.observer.snapshot_and_persist().await {
                Ok(snapshot) => return Ok(snapshot),
                Err(error) if error.is_timeout() => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => return Err(application_error(error)),
            }
        }
    }

    async fn public_local_rebuild(&self, reason: ReplicaRebuildReason) -> RuntimeError {
        RuntimeError::ReplicaRebuildRequired(Box::new(ReplicaRebuildRequired::LocalProvider(
            LocalProviderRebuildRequired {
                reason,
                provider: ProviderReplicaIdentity {
                    replica: self.durable.snapshot().await.identity.replica,
                },
            },
        )))
    }

    async fn public_lineage(&self) -> Result<PgLineage> {
        let (system, timeline) = if self.instance.is_running().await {
            let (client, _connection) = self.instance.connect().await.map_err(application_error)?;
            let system: String = client
                .query_one(
                    "SELECT system_identifier::text FROM pg_control_system()",
                    &[],
                )
                .await
                .map_err(application_error)?
                .get(0);
            let timeline: i32 = client
                .query_one(
                    "SELECT GREATEST(c.timeline_id, r.min_recovery_end_timeline, \
                     COALESCE((SELECT received_tli FROM pg_stat_wal_receiver LIMIT 1), 0))::int \
                     FROM pg_control_checkpoint() c, pg_control_recovery() r",
                    &[],
                )
                .await
                .map_err(application_error)?
                .get(0);
            (
                system,
                u32::try_from(timeline)
                    .map_err(|_| application_error("negative PostgreSQL timeline"))?,
            )
        } else {
            self.instance
                .control_identity()
                .await
                .map_err(application_error)?
        };
        PgLineage::read(self.instance.data_dir(), system, timeline)
            .await
            .map_err(application_error)
    }

    async fn public_validate_history(&self) -> Result<PgDurableState> {
        let durable = self.durable.revalidate().await.map_err(application_error)?;
        if durable.recovery_state != PgRecoveryState::Ready {
            return Err(self
                .public_local_rebuild(ReplicaRebuildReason::IncompatibleRetainedHistory)
                .await);
        }
        if let Some(system) = &durable.system_identifier {
            let lineage = self
                .public_lineage()
                .await
                .map_err(|_| RuntimeError::AuthorityNotAdmitted);
            let Ok(lineage) = lineage else {
                return Err(self
                    .public_local_rebuild(ReplicaRebuildReason::IncompatibleRetainedHistory)
                    .await);
            };
            if system != &lineage.system_identifier {
                return Err(self
                    .public_local_rebuild(ReplicaRebuildReason::ProviderMismatch)
                    .await);
            }
            let evidence = PgReplicationEvidence {
                engine: "postgres-physical".into(),
                system_identifier: lineage.system_identifier,
                timeline_id: lineage.timeline,
                in_recovery: durable.role == PgDurableRole::Standby,
                flush_lsn: durable.flush_lsn,
                received_lsn: durable.received_lsn,
                replay_lsn: durable.replay_lsn,
                metadata_generation: durable.generation,
                synchronous: None,
                wal_receiver_stopped: false,
            };
            let digest = timeline_history_digest(self.instance.data_dir(), &evidence)
                .await
                .map_err(application_error)?;
            if durable.timeline_id != Some(evidence.timeline_id)
                || durable.timeline_history_digest.as_ref() != Some(&digest)
            {
                return Err(self
                    .public_local_rebuild(ReplicaRebuildReason::IncompatibleRetainedHistory)
                    .await);
            }
        } else if !crate::service::is_empty(self.instance.data_dir()).map_err(application_error)? {
            return Err(self
                .public_local_rebuild(ReplicaRebuildReason::ProviderMismatch)
                .await);
        }
        Ok(durable)
    }

    pub(super) async fn public_change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()> {
        let context = self.public_owner().fence.context()?;
        if epoch < context.epoch || epoch < self.public_owner().state.lock().await.role_epoch {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.public_owner().cancel();
        self.public_owned(async {
            let mut state = self.public_owner().state.lock().await;
            if !state.opened {
                return Err(RuntimeError::Closed);
            }
            self.public_drain(&mut state).await?;
            let durable = self.public_validate_history().await?;
            let initial_primary =
                role == ReplicaRole::Primary && durable.system_identifier.is_none();
            match role {
                ReplicaRole::Primary => {
                    let fresh = initial_primary;
                    if fresh {
                        if !self.initializing {
                            return Err(self
                                .public_local_rebuild(ReplicaRebuildReason::ProviderMismatch)
                                .await);
                        }
                        self.instance.init_db().await.map_err(application_error)?;
                    }
                    self.public_start().await?;
                    if self
                        .observer
                        .snapshot()
                        .await
                        .map_err(application_error)?
                        .evidence
                        .is_some_and(|e| e.in_recovery)
                    {
                        self.instance.promote().await.map_err(application_error)?;
                        let (client, _connection) =
                            self.instance.connect().await.map_err(application_error)?;
                        client
                            .simple_query("CHECKPOINT")
                            .await
                            .map_err(application_error)?;
                    }
                    if fresh {
                        initialize_application_role(&self.instance)
                            .await
                            .map_err(application_error)?;
                    }
                    self.public_snapshot_and_persist().await?;
                }
                ReplicaRole::ActiveSecondary => {
                    if durable.system_identifier.is_some() && durable.role != PgDurableRole::Standby
                    {
                        return Err(self
                            .public_local_rebuild(ReplicaRebuildReason::IncompatibleRetainedHistory)
                            .await);
                    }
                }
                ReplicaRole::IdleSecondary | ReplicaRole::None => {}
            }
            state.role = role;
            state.role_epoch = epoch;
            state.epoch = initial_primary.then_some(epoch);
            self.public_owner().fence.update(|context| {
                context.role = role;
                context.epoch = epoch;
                context.previous_configuration_id = None;
                context.current_configuration_id = None;
            })?;
            Ok(())
        })
        .await
    }

    pub(super) async fn public_update_epoch(&self, epoch: Epoch) -> Result<()> {
        let context = self.public_owner().fence.context()?;
        let mut state = self.public_owner().state.lock().await;
        if !state.opened
            || epoch < context.epoch
            || epoch < state.role_epoch
            || state.epoch.is_some_and(|old| epoch < old)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if epoch != state.role_epoch {
            self.public_drain(&mut state).await?;
        }
        state.role_epoch = epoch;
        state.epoch = Some(epoch);
        self.public_owner().fence.update(|context| {
            context.epoch = epoch;
            context.previous_configuration_id = None;
            context.current_configuration_id = None;
        })?;
        Ok(())
    }

    pub(crate) async fn public_application_role(&self, role: ReplicaRole) -> Result<()> {
        if role == ReplicaRole::None {
            return self.public_revoke().await;
        }
        self.public_owned(async {
            let state = self.public_owner().state.lock().await;
            let context = self.public_owner().fence.context()?;
            if role != state.role || context.role != role || context.epoch != state.role_epoch {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            if !matches!(role, ReplicaRole::Primary | ReplicaRole::ActiveSecondary) {
                return Ok(());
            }
            drop(state);
            self.public_reconcile_access().await?;
            Ok(())
        })
        .await
    }

    fn public_check_configuration(
        &self,
        state: &PublicState,
        context: &LifecycleContext,
    ) -> Result<()> {
        let configuration = state
            .configuration
            .as_ref()
            .ok_or(RuntimeError::ReconfigurationPending)?;
        if state.role != ReplicaRole::Primary
            || context.role != ReplicaRole::Primary
            || state.epoch != Some(configuration.configuration.epoch)
            || context.epoch != configuration.configuration.epoch
            || context.current_configuration_id.as_ref()
                != Some(&configuration.configuration.configuration_id)
            || context.previous_configuration_id
                != state.previous.as_ref().map(|c| c.configuration_id.clone())
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        for grant in &state.grants {
            if grant.active.is_cancelled() {
                return Err(RuntimeError::OperationCancelled);
            }
            self.public_owner().fence.validate_binding(&grant.binding)?;
        }
        Ok(())
    }

    fn public_process_session(&self) -> ProcessSessionId {
        self.public_owner().fence.local.process_session_id.clone()
    }

    fn public_peer_binding(
        &self,
        context: &LifecycleContext,
        current: &ConfigurationDescriptor,
        previous: Option<&ConfigurationDescriptor>,
        replica: &ReplicaInformation,
    ) -> PeerBinding {
        PeerBinding {
            epoch: current.epoch,
            previous_configuration_id: previous
                .map(|configuration| configuration.configuration_id.clone()),
            current_configuration_id: current.configuration_id.clone(),
            primary: NativeEndpoint {
                replica: context.identity.clone(),
                process_session_id: self.public_process_session(),
                replication_address: self.coordination.local_endpoint.clone(),
                role: ReplicaRole::Primary,
            },
            secondary: NativeEndpoint {
                replica: replica.identity.clone(),
                process_session_id: replica.process_session_id.clone(),
                replication_address: replica.replication_address.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        }
    }

    async fn public_build_binding(&self, replica: &ReplicaInformation) -> Result<BuildBinding> {
        let durable = self.durable.revalidate().await.map_err(application_error)?;
        let state = self.public_owner().state.lock().await;
        if state.role != ReplicaRole::Primary || state.role_epoch != state.epoch.unwrap_or_default()
        {
            return Err(RuntimeError::NotPrimary);
        }
        Ok(BuildBinding {
            resource_uid: durable.identity.resource_uid,
            epoch: state.role_epoch,
            build_id: replica.build_id.clone(),
            source: NativeEndpoint {
                replica: durable.identity.replica,
                process_session_id: self.public_process_session(),
                replication_address: self.coordination.local_endpoint.clone(),
                role: ReplicaRole::Primary,
            },
            target: NativeEndpoint {
                replica: replica.identity.clone(),
                process_session_id: replica.process_session_id.clone(),
                replication_address: replica.replication_address.clone(),
                role: ReplicaRole::IdleSecondary,
            },
        })
    }

    async fn public_grant(
        &self,
        state: &mut PublicState,
        binding: TransportBinding,
    ) -> Result<(CancellationToken, bool)> {
        let context = self.public_owner().fence.validate_binding(&binding)?;
        if &context.identity != binding.source() || state.role != ReplicaRole::Primary {
            return Err(RuntimeError::NotPrimary);
        }
        if let Some(grant) = state.grants.iter().find(|grant| grant.binding == binding) {
            return Ok((grant.active.clone(), false));
        }
        if state.grants.len() >= MAX_BUILDS {
            return Err(RuntimeError::ReconfigurationPending);
        }
        let nonce = super::public_transport::random_bytes()?;
        let session =
            ProcessSessionId::new(nonce.iter().map(|b| format!("{b:02x}")).collect::<String>());
        let name = crate::native::replication_application_name(binding.target(), &session);
        let slot = crate::native::replication_slot_name(binding.target());
        let (client, _connection) = self.instance.connect().await.map_err(application_error)?;
        client
            .query(
                "SELECT pg_create_physical_replication_slot($1, true) \
             WHERE NOT EXISTS (SELECT 1 FROM pg_replication_slots WHERE slot_name = $1)",
                &[&slot],
            )
            .await
            .map_err(application_error)?;
        let lineage = self.public_lineage().await?;
        let boundary = self
            .observer
            .snapshot()
            .await
            .map_err(application_error)?
            .evidence
            .ok_or(RuntimeError::NotPrimary)?
            .flush_lsn;
        let fence = self.public_owner().fence.clone();
        let validity_binding = binding.clone();
        let instance = self.instance.clone();
        let generation = instance.generation_id();
        let active = CancellationToken::new();
        let admitted = active.clone();
        let valid = Arc::new(move || {
            !admitted.is_cancelled()
                && instance.generation_id() == generation
                && fence
                    .validate_binding(&validity_binding)
                    .is_ok_and(|current| current == context)
        });
        let (gateway, credential) = NativeGateway::start(
            self.instance.listen_host(),
            self.instance
                .socket_dir()
                .join(format!(".s.PGSQL.{}", self.instance.port())),
            name,
            slot,
            matches!(binding, TransportBinding::Build(_)),
            valid,
        )
        .await?;
        state.grants.push(Grant {
            binding,
            credential,
            gateway,
            active: active.clone(),
            session,
            lineage,
            boundary,
        });
        Ok((active, true))
    }

    pub(super) async fn public_configuration(
        &self,
        current: ReplicaSetConfiguration,
        previous: Option<ReplicaSetConfiguration>,
    ) -> Result<()> {
        kuberic_runtime::protocol::validation::validate_configuration(&current.configuration, None)
            .map_err(application_error)?;
        if let Some(previous) = &previous {
            kuberic_runtime::protocol::validation::validate_configuration(
                &previous.configuration,
                None,
            )
            .map_err(application_error)?;
        }
        let previous_descriptor = previous
            .as_ref()
            .map(|configuration| configuration.configuration.clone());
        let context = self.public_owner().fence.context()?;
        {
            let state = self.public_owner().state.lock().await;
            if state.role != ReplicaRole::Primary
                || context.role != ReplicaRole::Primary
                || state.epoch != Some(current.configuration.epoch)
                || context.epoch != current.configuration.epoch
                || current.configuration.primary_id != context.identity.replica_id
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
        }
        let context = self.public_owner().fence.update(|context| {
            context.previous_configuration_id = previous_descriptor
                .as_ref()
                .map(|configuration| configuration.configuration_id.clone());
            context.current_configuration_id = Some(current.configuration.configuration_id.clone());
        })?;
        let mut replicas = BTreeMap::new();
        for replica in current.replicas.iter().chain(
            previous
                .iter()
                .flat_map(|configuration| &configuration.replicas),
        ) {
            if let Some(existing) = replicas.insert(replica.identity.clone(), replica.clone())
                && existing != *replica
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
        }
        let mut identities = BTreeSet::new();
        let mut bindings = BTreeMap::new();
        for replica in replicas.values() {
            replica.validate_public_progress()?;
            if replica.role != ReplicaRole::ActiveSecondary || replica.process_session_id.is_empty()
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            let binding = self.public_peer_binding(
                &context,
                &current.configuration,
                previous_descriptor.as_ref(),
                replica,
            );
            self.public_owner()
                .fence
                .validate_binding(&TransportBinding::Peer(binding.clone()))?;
            if binding.primary.replica != context.identity
                || !identities.insert(replica.identity.clone())
                || !current
                    .configuration
                    .members
                    .iter()
                    .chain(
                        previous_descriptor
                            .iter()
                            .flat_map(|configuration| &configuration.members),
                    )
                    .any(|member| {
                        member.identity == replica.identity
                            && member.role == ReplicaRole::ActiveSecondary
                    })
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            bindings.insert(replica.identity.clone(), binding);
        }
        if current
            .configuration
            .members
            .iter()
            .chain(
                previous_descriptor
                    .iter()
                    .flat_map(|configuration| &configuration.members),
            )
            .any(|member| {
                member.identity != context.identity && !identities.contains(&member.identity)
            })
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.public_owner().cancel();
        // Publish grants before contacting targets: their source inspection is
        // a separate RPC and must not wait on this same lifecycle mutex.
        self.public_owned(async {
            let mut state = self.public_owner().state.lock().await;
            self.public_drain(&mut state).await?;
            self.public_start().await?;
            let snapshot = self.public_snapshot_and_persist().await?;
            if snapshot
                .evidence
                .as_ref()
                .is_none_or(|evidence| evidence.in_recovery)
            {
                return Err(self
                    .public_local_rebuild(ReplicaRebuildReason::IncompatibleRetainedHistory)
                    .await);
            }
            state.configuration = Some(current.clone());
            state.previous = previous_descriptor;
            for binding in bindings.values() {
                let _ = self
                    .public_grant(&mut state, TransportBinding::Peer(binding.clone()))
                    .await?;
            }
            let sessions = state
                .grants
                .iter()
                .filter_map(|grant| match &grant.binding {
                    TransportBinding::Peer(peer) => {
                        Some((peer.secondary.replica.clone(), grant.session.clone()))
                    }
                    _ => None,
                })
                .collect::<BTreeMap<_, _>>();
            let policy = compile_synchronous_configuration(
                state.previous.as_ref(),
                &current.configuration,
                &context.identity,
                &sessions,
                true,
            )
            .map_err(application_error)?;
            self.observer
                .apply_synchronous(policy)
                .await
                .map_err(application_error)?;
            Ok(())
        })
        .await?;
        for replica in replicas.values() {
            let binding = bindings
                .get(&replica.identity)
                .expect("validated peer binding")
                .clone();
            match self
                .public_rpc(
                    &replica.replication_address,
                    &Request::Peer(binding.clone()),
                )
                .await?
            {
                Response::Peer(completed) if completed == binding => {}
                _ => return Err(RuntimeError::AuthorityNotAdmitted),
            }
        }
        let state = self.public_owner().state.lock().await;
        self.public_check_configuration(&state, &self.public_owner().fence.context()?)
    }

    async fn public_wait_inner(
        &self,
        state: &PublicState,
        mode: ReplicaSetQuorumMode,
    ) -> Result<()> {
        self.public_check_configuration(state, &self.public_owner().fence.context()?)?;
        let (client, _connection) = self.instance.connect().await.map_err(application_error)?;
        let row = tokio::time::timeout(
            Duration::from_secs(10),
            client.query_one("SELECT pg_current_wal_flush_lsn()::text", &[]),
        )
        .await
        .map_err(|_| RuntimeError::OperationCancelled)?
        .map_err(application_error)?;
        let boundary = crate::monitor::parse_pg_lsn(row.get(0)).map_err(application_error)?;
        loop {
            self.public_check_configuration(state, &self.public_owner().fence.context()?)?;
            let complete = if mode == ReplicaSetQuorumMode::All {
                let sessions = state
                    .grants
                    .iter()
                    .filter_map(|grant| match &grant.binding {
                        TransportBinding::Peer(peer) => {
                            Some((peer.secondary.replica.clone(), grant.session.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                tokio::time::timeout(
                    Duration::from_secs(10),
                    self.observer.all_replayed(&sessions, boundary),
                )
                .await
                .map_err(|_| RuntimeError::OperationCancelled)?
                .map_err(application_error)?
            } else {
                self.observer
                    .snapshot()
                    .await
                    .map_err(application_error)?
                    .committed_lsn
                    >= boundary
            };
            if complete {
                self.public_snapshot_and_persist().await?;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub(super) async fn public_wait(&self, mode: ReplicaSetQuorumMode) -> Result<()> {
        self.public_owned(async {
            let state = self.public_owner().state.lock().await;
            self.public_wait_inner(&state, mode).await
        })
        .await
    }

    pub(super) async fn public_progress(&self) -> Result<i64> {
        let state = self.public_owner().state.lock().await;
        if !state.opened || self.cancellation.is_cancelled() {
            return Err(RuntimeError::Closed);
        }
        let durable = self.public_validate_history().await?;
        if state.role == ReplicaRole::IdleSecondary && state.built.is_none() {
            return Ok(-1);
        }
        if !self.instance.is_running().await {
            return Ok(match state.role {
                ReplicaRole::None => durable.flush_lsn,
                ReplicaRole::Primary => durable.policy_certified_lsn,
                _ => durable.received_lsn.or(durable.replay_lsn).unwrap_or(-1),
            });
        }
        let (client, _connection) = self.instance.connect().await.map_err(application_error)?;
        tokio::time::timeout(Duration::from_secs(30), async {
            if state.role == ReplicaRole::Primary {
                let row = client
                    .query_one("SELECT pg_current_wal_flush_lsn()::text", &[])
                    .await
                    .map_err(application_error)?;
                let flush = crate::monitor::parse_pg_lsn(row.get(0)).map_err(application_error)?;
                return crate::native::certify_primary_progress(
                    &client,
                    flush,
                    durable.synchronous.as_ref(),
                )
                .await
                .map(|progress| progress.max(durable.policy_certified_lsn))
                .map_err(application_error);
            }
            let in_recovery: bool = client
                .query_one("SELECT pg_is_in_recovery()", &[])
                .await
                .map_err(application_error)?
                .get(0);
            if !in_recovery {
                let row = client
                    .query_one("SELECT pg_current_wal_flush_lsn()::text", &[])
                    .await
                    .map_err(application_error)?;
                return crate::monitor::parse_pg_lsn(row.get(0)).map_err(application_error);
            }
            let row = client
                .query_one(
                    "SELECT pg_last_wal_replay_lsn()::text, pg_last_wal_receive_lsn()::text",
                    &[],
                )
                .await
                .map_err(application_error)?;
            let replay = row
                .get::<_, Option<&str>>(0)
                .map(crate::monitor::parse_pg_lsn)
                .transpose()
                .map_err(application_error)?;
            let received = row
                .get::<_, Option<&str>>(1)
                .map(crate::monitor::parse_pg_lsn)
                .transpose()
                .map_err(application_error)?;
            Ok(received.into_iter().chain(replay).max().unwrap_or(-1))
        })
        .await
        .map_err(|_| RuntimeError::OperationCancelled)?
    }

    pub(super) async fn public_capability(&self) -> Result<i64> {
        let _state = self.public_owner().state.lock().await;
        if self
            .public_validate_history()
            .await?
            .system_identifier
            .is_none()
        {
            return Ok(-1);
        }
        self.instance
            .retained_wal_boundary()
            .await
            .map_err(application_error)
    }

    pub(super) async fn public_data_loss(&self) -> Result<bool> {
        let _state = self.public_owner().state.lock().await;
        self.public_validate_history().await?;
        Ok(false)
    }

    async fn public_rpc(&self, endpoint: &str, request: &Request) -> Result<Response> {
        let request = tonic::Request::new(crate::proto::RecoveryRequest {
            envelope_json: encode(request).map_err(application_error)?,
        });
        let response = tokio::time::timeout(Duration::from_secs(120), async {
            let mut client = crate::proto::pg_data_service_client::PgDataServiceClient::connect(
                endpoint.to_owned(),
            )
            .await
            .map_err(application_error)?;
            client.public_v1(request).await.map_err(application_error)
        })
        .await
        .map_err(|_| RuntimeError::OperationCancelled)??;
        decode(&response.into_inner().envelope_json).map_err(application_error)
    }

    async fn public_source(
        &self,
        binding: &TransportBinding,
    ) -> Result<(NativeCredential, PgLineage, i64)> {
        self.public_owner().fence.validate_binding(binding)?;
        self.public_source_for_admission(binding).await
    }

    async fn public_source_for_admission(
        &self,
        binding: &TransportBinding,
    ) -> Result<(NativeCredential, PgLineage, i64)> {
        self.public_owner()
            .fence
            .validate_process_binding(binding)?;
        match self
            .public_rpc(binding.source_address(), &Request::Inspect(binding.clone()))
            .await?
        {
            Response::Source {
                binding: observed,
                credential,
                lineage,
                boundary,
            } if &observed == binding && boundary >= 0 => {
                self.public_owner()
                    .fence
                    .validate_process_binding(binding)?;
                lineage.validate().map_err(application_error)?;
                credential.conninfo()?;
                Ok((credential, lineage, boundary))
            }
            _ => Err(RuntimeError::AuthorityNotAdmitted),
        }
    }

    pub(super) async fn public_build(&self, replica: ReplicaInformation) -> Result<()> {
        replica.validate_public_progress()?;
        let build = self.public_build_binding(&replica).await?;
        let binding = TransportBinding::Build(build.clone());
        self.public_owner().fence.validate_binding(&binding)?;
        let mut grant = {
            let mut state = self.public_owner().state.lock().await;
            let (active, created) = self.public_grant(&mut state, binding.clone()).await?;
            BuildGrantGuard {
                active,
                armed: created,
            }
        };
        let result = match self
            .public_rpc(binding.target_address(), &Request::Build(build.clone()))
            .await?
        {
            Response::Built(completed) if completed.binding == build => {
                let state = self.public_owner().state.lock().await;
                self.public_owner().fence.validate_binding(&binding)?;
                let grant = state
                    .grants
                    .iter()
                    .find(|g| g.binding == binding)
                    .ok_or(RuntimeError::OperationCancelled)?;
                if completed.lineage != grant.lineage || completed.boundary != grant.boundary {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                Ok(())
            }
            Response::Rebuild(ReplicaRebuildRequired::RemoteBuildTarget(failure))
                if failure.epoch == build.epoch
                    && failure.target == build.target.replica
                    && failure.target_session == build.target.process_session_id =>
            {
                self.public_owner().fence.validate_binding(&binding)?;
                Err(RuntimeError::ReplicaRebuildRequired(Box::new(
                    ReplicaRebuildRequired::RemoteBuildTarget(failure),
                )))
            }
            _ => Err(RuntimeError::AuthorityNotAdmitted),
        };
        if result.is_ok() {
            grant.armed = false;
        }
        result
    }

    async fn public_receive_build(&self, build: BuildBinding) -> Result<BuildCompletion> {
        let binding = TransportBinding::Build(build.clone());
        let context = self.public_owner().fence.validate_binding(&binding)?;
        if context.identity != *binding.target() || context.role != ReplicaRole::IdleSecondary {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if self
            .public_owner()
            .state
            .lock()
            .await
            .built
            .as_ref()
            .is_some_and(|completed| completed.binding != build)
        {
            return Err(remote_rebuild(
                &build,
                ReplicaRebuildReason::RemoteCopyConflict,
            )?);
        }
        self.public_owned(async {
            let (credential, lineage, boundary) = self.public_source(&binding).await?;
            let mut state = self.public_owner().state.lock().await;
            if state.role != ReplicaRole::IdleSecondary {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            let durable = match self.public_validate_history().await {
                Ok(durable) => durable,
                Err(RuntimeError::ReplicaRebuildRequired(failure)) => {
                    let ReplicaRebuildRequired::LocalProvider(failure) = *failure else {
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    };
                    return Err(remote_rebuild(&build, failure.reason)?);
                }
                Err(error) => return Err(error),
            };
            if let Some(completed) = &state.built {
                if completed.binding != build || completed.lineage != lineage {
                    return Err(remote_rebuild(
                        &build,
                        ReplicaRebuildReason::RemoteCopyConflict,
                    )?);
                }
            } else if durable.system_identifier.is_some() {
                return Err(remote_rebuild(
                    &build,
                    ReplicaRebuildReason::IncompatibleRetainedHistory,
                )?);
            } else {
                self.public_drain(&mut state).await?;
                self.instance
                    .prepare_native_build()
                    .await
                    .map_err(application_error)?;
                self.public_owner().fence.validate_binding(&binding)?;
                self.instance
                    .public_base_backup(&credential.conninfo()?)
                    .await
                    .map_err(application_error)?;
                if self.public_lineage().await? != lineage {
                    return Err(remote_rebuild(
                        &build,
                        ReplicaRebuildReason::RemoteCopyConflict,
                    )?);
                }
            }
            let slot = crate::native::replication_slot_name(binding.target());
            self.instance
                .config()
                .public_receiver(
                    self.instance.data_dir(),
                    &credential.conninfo()?,
                    &slot,
                    &lineage,
                )
                .await
                .map_err(application_error)?;
            self.public_start().await?;
            state.receiver = Some(binding.clone());
            loop {
                self.public_owner().fence.validate_binding(&binding)?;
                let snapshot = self.observer.snapshot().await.map_err(application_error)?;
                if snapshot.evidence.as_ref().is_some_and(|e| {
                    e.in_recovery
                        && e.system_identifier == lineage.system_identifier
                        && e.timeline_id == lineage.timeline
                        && e.received_lsn.is_some_and(|lsn| lsn >= boundary)
                        && e.replay_lsn.is_some_and(|lsn| lsn >= boundary)
                }) {
                    let (client, _connection) =
                        self.instance.connect().await.map_err(application_error)?;
                    client
                        .simple_query("CHECKPOINT")
                        .await
                        .map_err(application_error)?;
                    self.public_source(&binding).await?;
                    #[cfg(all(test, feature = "testing"))]
                    self.public_owner().completion_checkpoint().await;
                    self.public_owner().fence.validate_binding(&binding)?;
                    self.public_snapshot_and_persist().await?;
                    self.public_owner().fence.validate_binding(&binding)?;
                    let completion = BuildCompletion {
                        binding: build,
                        lineage,
                        boundary,
                    };
                    state.built = Some(completion.clone());
                    return Ok(completion);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
    }

    async fn public_receive_peer(&self, binding: PeerBinding) -> Result<()> {
        let transport = TransportBinding::Peer(binding.clone());
        let context = self
            .public_owner()
            .fence
            .validate_process_binding(&transport)?;
        if &context.identity != transport.target()
            || context.role != ReplicaRole::ActiveSecondary
            || context.epoch != binding.epoch
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let (credential, lineage, _) = self.public_source_for_admission(&transport).await?;
        let context = self.public_owner().fence.admit_peer(&binding)?;
        self.public_owner().fence.validate_binding(&transport)?;
        self.public_owner().cancel();
        self.public_owned(async {
            let mut state = self.public_owner().state.lock().await;
            if state.role != ReplicaRole::ActiveSecondary
                || state.role_epoch != context.epoch
                || state.epoch != Some(context.epoch)
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            self.public_validate_history().await?;
            if self.public_lineage().await? != lineage {
                return Err(self
                    .public_local_rebuild(ReplicaRebuildReason::IncompatibleRetainedHistory)
                    .await);
            }
            self.public_drain(&mut state).await?;
            self.instance
                .config()
                .public_receiver(
                    self.instance.data_dir(),
                    &credential.conninfo()?,
                    &crate::native::replication_slot_name(transport.target()),
                    &lineage,
                )
                .await
                .map_err(application_error)?;
            self.public_owner().fence.validate_binding(&transport)?;
            self.public_source(&transport).await?;
            self.public_start().await?;
            let snapshot = self.public_snapshot_and_persist().await?;
            if snapshot.evidence.as_ref().is_none_or(|evidence| {
                !evidence.in_recovery
                    || evidence.system_identifier != lineage.system_identifier
                    || evidence.timeline_id != lineage.timeline
            }) {
                return Err(self
                    .public_local_rebuild(ReplicaRebuildReason::IncompatibleRetainedHistory)
                    .await);
            }
            state.receiver = Some(transport);
            Ok(())
        })
        .await
    }

    pub(crate) async fn public_request(&self, request: Request) -> Result<Response> {
        if !self.is_public_v1() || self.cancellation.is_cancelled() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        match request {
            Request::Inspect(binding) => {
                let context = self.public_owner().fence.validate_binding(&binding)?;
                if &context.identity != binding.source() {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                let state = self.public_owner().state.lock().await;
                let grant = state
                    .grants
                    .iter()
                    .find(|g| g.binding == binding)
                    .ok_or(RuntimeError::AuthorityNotAdmitted)?;
                if self.public_lineage().await? != grant.lineage {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                Ok(Response::Source {
                    binding,
                    credential: grant.credential.clone(),
                    lineage: grant.lineage.clone(),
                    boundary: grant.boundary,
                })
            }
            Request::Build(binding) => match self.public_receive_build(binding).await {
                Ok(completed) => Ok(Response::Built(completed)),
                Err(RuntimeError::ReplicaRebuildRequired(failure)) => {
                    Ok(Response::Rebuild(*failure))
                }
                Err(error) => Err(error),
            },
            Request::Peer(binding) => {
                self.public_receive_peer(binding.clone()).await?;
                Ok(Response::Peer(binding))
            }
        }
    }

    pub(super) async fn public_remove(&self, replica: ReplicaId) -> Result<()> {
        let state = self.public_owner().state.lock().await;
        if state
            .grants
            .iter()
            .any(|g| g.binding.target().replica_id == replica)
        {
            drop(state);
            self.public_revoke().await?;
        }
        Ok(())
    }
}

fn remote_rebuild(binding: &BuildBinding, reason: ReplicaRebuildReason) -> Result<RuntimeError> {
    Ok(RuntimeError::ReplicaRebuildRequired(Box::new(
        ReplicaRebuildRequired::RemoteBuildTarget(Box::new(RemoteBuildTargetRebuildRequired {
            reason,
            epoch: binding.epoch,
            target: binding.target.replica.clone(),
            target_session: binding.target.process_session_id.clone(),
        })),
    )))
}

#[cfg(all(test, feature = "testing"))]
#[path = "public_v1_tests.rs"]
pub(crate) mod tests;
