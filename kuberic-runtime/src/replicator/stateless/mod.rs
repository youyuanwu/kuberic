use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, StreamExt, stream, stream::FuturesUnordered};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock, watch};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status};

use crate::application::{CopyChunk, Operation, OperationData, StateProvider};
use crate::control::proto;
use crate::engine::DurableState;
use crate::protocol::public_operations::{
    LocalProviderRebuildRequired, PUBLIC_V1_PROTOCOL_VERSION, ProviderReplicaIdentity,
    PublicInboundBuildAuthority, PublicInboundBuildReceipt, RemoteBuildTargetRebuildRequired,
    ReplicaRebuildReason, ReplicaRebuildRequired,
};
use crate::protocol::types::{
    ConfigurationDescriptor, Epoch, ProcessSessionId, ReplicaId, ReplicaIdentity, ReplicaRole,
};
use crate::replicator::stream::{OperationMetadata, OperationStream, ServiceStreams};
use crate::replicator::{
    PrimaryReplicator, ReplicaInformation, ReplicaSetConfiguration, ReplicaSetQuorumMode,
    Replicator, ReplicatorCapabilities, ReplicatorFactory, ReplicatorFactoryContext,
    ReplicatorInterfaces, ReplicatorSettings, StateReplicator,
};
use crate::{Result, RuntimeError};

enum ReplicationSendFailure {
    Unavailable(String),
    Rejected(String),
}

#[cfg(feature = "testing")]
#[doc(hidden)]
#[derive(Clone)]
pub struct InboundBuildClassificationPause {
    operation_id: crate::protocol::types::OperationId,
    entered: Arc<tokio::sync::Notify>,
    boundary_waiting: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[cfg(feature = "testing")]
impl InboundBuildClassificationPause {
    pub async fn wait_until_classified(&self) {
        self.entered.notified().await;
    }

    pub async fn wait_until_boundary_waiting(&self) {
        self.boundary_waiting.notified().await;
    }

    pub fn release(&self) {
        self.release.notify_one();
    }
}

#[cfg(feature = "testing")]
static INBOUND_BUILD_CLASSIFICATION_PAUSE: OnceLock<
    StdMutex<Option<InboundBuildClassificationPause>>,
> = OnceLock::new();

#[cfg(feature = "testing")]
#[doc(hidden)]
pub fn testing_pause_inbound_build_after_classification(
    operation_id: crate::protocol::types::OperationId,
) -> InboundBuildClassificationPause {
    let pause = InboundBuildClassificationPause {
        operation_id,
        entered: Arc::new(tokio::sync::Notify::new()),
        boundary_waiting: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    let previous = INBOUND_BUILD_CLASSIFICATION_PAUSE
        .get_or_init(|| StdMutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .replace(pause.clone());
    assert!(
        previous.is_none(),
        "an inbound build classification pause is already armed"
    );
    pause
}

#[cfg(feature = "testing")]
async fn pause_inbound_build_after_classification(authority: &PublicInboundBuildAuthority) {
    let pause = INBOUND_BUILD_CLASSIFICATION_PAUSE
        .get_or_init(|| StdMutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .filter(|pause| pause.operation_id == authority.operation_id)
        .cloned();
    if let Some(pause) = pause {
        pause.entered.notify_one();
        pause.release.notified().await;
        INBOUND_BUILD_CLASSIFICATION_PAUSE
            .get_or_init(|| StdMutex::new(None))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_if(|active| active.operation_id == authority.operation_id);
    }
}

#[cfg(feature = "testing")]
fn notify_inbound_build_boundary_waiting() {
    if let Some(pause) = INBOUND_BUILD_CLASSIFICATION_PAUSE
        .get_or_init(|| StdMutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .cloned()
    {
        pause.boundary_waiting.notify_one();
    }
}

#[async_trait]
impl crate::replicator::StatelessRuntimeControl for StatelessReplicator {
    async fn reported_progress(&self) -> Result<(i64, i64)> {
        let state = self.state.lock().await;
        if state.closed || self.closed.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        if state.role != ReplicaRole::Primary {
            return Ok((state.applied_lsn, state.committed_lsn));
        }
        // Primary-local persistence retains its historical reporting contract.
        // Read commitment first so a concurrent local write cannot pair newer
        // commitment with an older applied snapshot.
        drop(state);
        let committed = self.provider.last_committed_lsn().await?;
        let state = self.state.lock().await;
        if state.closed || self.closed.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        Ok((
            state.applied_lsn,
            if state.role == ReplicaRole::Primary {
                committed
            } else {
                state.committed_lsn
            },
        ))
    }

    async fn shutdown_and_drain(&self) -> Result<()> {
        self.abort();
        self.descendants.stop();
        self.inbound_descendants.stop();
        let owner = self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let listener = async {
            if let Some(mut owner) = owner {
                owner.shutdown.send_replace(true);
                match (&mut owner.task).await {
                    Ok(result) => result,
                    Err(error) if error.is_cancelled() => Ok(()),
                    Err(error) => Err(RuntimeError::Application(error.to_string())),
                }
            } else {
                Ok(())
            }
        };
        let (listener, outbound, inbound, ()) = tokio::join!(
            listener,
            self.descendants.drain(),
            self.inbound_descendants.drain(),
            self.streams.drain()
        );
        let failures = [listener, outbound, inbound]
            .into_iter()
            .filter_map(|result| result.err().map(|error| error.to_string()))
            .collect::<Vec<_>>();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(RuntimeError::Application(failures.join("; ")))
        }
    }

    async fn install_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()> {
        let _admission = self.inbound_build_admission_gate.lock().await;
        if self.closed.load(Ordering::Acquire) || self.state.lock().await.closed {
            return Err(RuntimeError::Closed);
        }
        match self
            .context
            .classify_replacement_inbound_build_authority(authority)
            .await?
        {
            crate::host::store::PublicInboundBuildAdmission::Current => return Ok(()),
            crate::host::store::PublicInboundBuildAdmission::Successor => {}
        }
        #[cfg(feature = "testing")]
        pause_inbound_build_after_classification(authority).await;
        self.revoke_generation();
        self.cancel_inbound_effects();
        let _effect_drain = self.provider_effect_gate.lock().await;
        self.streams.drain().await;
        self.state.lock().await.inbound_build = None;
        self.context
            .install_replacement_inbound_build_authority(authority)
            .await
    }

    async fn revoke_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()> {
        let _admission = self.inbound_build_admission_gate.lock().await;
        let current = self
            .context
            .replacement_inbound_build_authority(&authority.attempt.build_id)
            .await?;
        if current.as_ref() != Some(authority) {
            let terminal = self
                .context
                .replacement_terminal_inbound_build_receipt(&authority.attempt.build_id)
                .await?;
            if terminal
                .as_ref()
                .is_some_and(|receipt| receipt.authority == *authority)
            {
                return Ok(());
            }
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.revoke_generation();
        self.cancel_inbound_effects();
        let _effect_drain = self.provider_effect_gate.lock().await;
        self.streams.drain().await;
        self.state.lock().await.inbound_build = None;
        self.context
            .revoke_replacement_inbound_build_authority()
            .await
    }

    fn raw_retained_task_cardinality(&self) -> (usize, usize) {
        self.raw_retained_task_cardinality()
    }
}

const PEER_RPC_TIMEOUT: Duration = Duration::from_secs(5);
const LISTENER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn copy_can_replace_tail(applied: i64, committed: i64, copy_boundary: i64) -> bool {
    (applied == 0 && committed == 0) || (applied > committed && copy_boundary >= committed)
}

#[test]
fn inbound_copy_recovery_preserves_committed_floor() {
    assert!(copy_can_replace_tail(0, 0, 0));
    assert!(copy_can_replace_tail(2, 1, 2));
    assert!(copy_can_replace_tail(2, 1, 1));
    assert!(!copy_can_replace_tail(2, 1, 0));
    assert!(!copy_can_replace_tail(2, 2, 2));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicV1DataServiceRegistration {
    ReplicationData,
}

impl PublicV1DataServiceRegistration {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::ReplicationData => "ReplicationDataServer",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ReplicationDataEndpoint;

impl ReplicationDataEndpoint {
    fn bind<S>(self, service: S) -> proto::replication_data_server::ReplicationDataServer<S>
    where
        S: proto::replication_data_server::ReplicationData,
    {
        proto::replication_data_server::ReplicationDataServer::new(service)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PublicV1DataServiceManifest {
    replication_data: ReplicationDataEndpoint,
}

impl PublicV1DataServiceManifest {
    pub(crate) const fn registrations(self) -> [PublicV1DataServiceRegistration; 1] {
        [PublicV1DataServiceRegistration::ReplicationData]
    }

    fn replication_data<S>(
        self,
        service: S,
    ) -> proto::replication_data_server::ReplicationDataServer<S>
    where
        S: proto::replication_data_server::ReplicationData,
    {
        self.replication_data.bind(service)
    }
}

pub(crate) const PUBLIC_V1_DATA_SERVICES: PublicV1DataServiceManifest =
    PublicV1DataServiceManifest {
        replication_data: ReplicationDataEndpoint,
    };

pub(crate) fn validate_public_v1_data_services(
    manifest: PublicV1DataServiceManifest,
) -> Result<()> {
    if manifest.registrations() != [PublicV1DataServiceRegistration::ReplicationData] {
        return Err(RuntimeError::Application(
            "Public V1 data service registrations changed".into(),
        ));
    }
    Ok(())
}

pub struct StatelessReplicatorFactory {
    storage: Arc<dyn DurableState>,
}

impl StatelessReplicatorFactory {
    pub fn new(storage: Arc<dyn DurableState>) -> Self {
        super::PUBLIC_V1_FACTORIES.construct_stateless(storage)
    }

    pub(super) fn from_public_v1_manifest(storage: Arc<dyn DurableState>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl ReplicatorFactory for StatelessReplicatorFactory {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        state_provider: Option<Arc<dyn StateProvider>>,
        settings: ReplicatorSettings,
    ) -> Result<ReplicatorInterfaces> {
        let binding = context.public_v1_binding().clone();
        if !binding.identity.is_public_v1() {
            return Err(RuntimeError::Application(
                "stateless replacement requires exact Public V1 process authority".into(),
            ));
        }
        if settings.replication_address.is_empty() {
            return Err(RuntimeError::Application(
                "stateless replacement requires a replication address".into(),
            ));
        }
        let bind_address = context.public_v1_replication_address();
        let provider = state_provider.ok_or_else(|| {
            RuntimeError::Application("stateless replacement requires a state provider".into())
        })?;
        let engine = Arc::new(StatelessReplicator::new(
            context.identity()?,
            context.process_session_id()?,
            context,
            provider,
            self.storage.clone(),
            settings,
            bind_address,
        ));
        engine
            .self_ref
            .set(Arc::downgrade(&engine))
            .expect("stateless replacement self reference is unset");
        let containment = engine.callback_containment();
        Ok(ReplicatorInterfaces::primary(
            engine.clone(),
            Some(engine.clone()),
            ReplicatorCapabilities {
                catch_up_specific_quorum: true,
                data_loss_replay:
                    crate::protocol::public_operations::DataLossReplay::ReplaceOnAmbiguity,
            },
        )
        .with_callback_containment(containment)
        .with_stateless_runtime_control(engine))
    }
}

#[derive(Clone, PartialEq, Eq)]
struct ConfigurationAuthority {
    epoch: Epoch,
    current: ConfigurationDescriptor,
    previous: Option<ConfigurationDescriptor>,
    peers: BTreeMap<ReplicaIdentity, ReplicaInformation>,
}

#[derive(Clone, PartialEq, Eq)]
struct InboundBuild {
    authority: PublicInboundBuildAuthority,
    replication_boundary_lsn: i64,
    catch_up_boundary_lsn: Option<i64>,
    next_sequence: u64,
    copy_finished: bool,
    retained_lsn: i64,
}

struct EngineState {
    opened: bool,
    role: ReplicaRole,
    role_epoch: Option<Epoch>,
    updated_epoch: Option<Epoch>,
    authority: Option<ConfigurationAuthority>,
    retired: BTreeSet<ReplicaIdentity>,
    inbound_build: Option<InboundBuild>,
    applied_lsn: i64,
    committed_lsn: i64,
    ambiguous_suffix: bool,
    local_rebuild_required: bool,
    closed: bool,
}

struct ListenerOwner {
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<()>>,
}

async fn drain_listener(owner: ListenerOwner) -> Result<()> {
    drain_listener_with_timeout(owner, LISTENER_SHUTDOWN_TIMEOUT).await
}

async fn drain_listener_with_timeout(mut owner: ListenerOwner, timeout: Duration) -> Result<()> {
    owner.shutdown.send_replace(true);
    match tokio::time::timeout(timeout, &mut owner.task).await {
        Ok(result) => result.map_err(|error| RuntimeError::Application(error.to_string()))?,
        Err(_) => {
            owner.task.abort();
            let _ = owner.task.await;
            Err(RuntimeError::OperationCancelled)
        }
    }
}

#[derive(Clone)]
struct TaskRegistry {
    inner: Arc<TaskRegistryInner>,
}

struct TaskRegistryInner {
    next_id: AtomicU64,
    tasks: StdMutex<BTreeMap<u64, tokio::task::AbortHandle>>,
    stopped: AtomicBool,
    failure: StdMutex<Option<String>>,
    completed: tokio::sync::Notify,
}

struct TaskCompletion {
    id: u64,
    inner: Arc<TaskRegistryInner>,
}

impl Drop for TaskCompletion {
    fn drop(&mut self) {
        if std::thread::panicking() {
            *self
                .inner
                .failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(format!("replicator descendant {} panicked", self.id));
        }
        self.inner
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
        self.inner.completed.notify_waiters();
    }
}

impl TaskRegistry {
    fn new() -> Self {
        Self {
            inner: Arc::new(TaskRegistryInner {
                next_id: AtomicU64::new(1),
                tasks: StdMutex::new(BTreeMap::new()),
                stopped: AtomicBool::new(false),
                failure: StdMutex::new(None),
                completed: tokio::sync::Notify::new(),
            }),
        }
    }

    fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let completion = TaskCompletion {
            id,
            inner: self.inner.clone(),
        };
        let (start, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _completion = completion;
            let _ = started.await;
            future.await
        });
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tasks.insert(id, task.abort_handle());
        if self.inner.stopped.load(Ordering::Acquire) {
            task.abort();
        }
        drop(tasks);
        let _ = start.send(());
        task
    }

    fn abort_all(&self) {
        for task in self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            task.abort();
        }
    }

    fn stop(&self) {
        self.inner.stopped.store(true, Ordering::Release);
        self.abort_all();
    }

    async fn drain(&self) -> Result<()> {
        loop {
            let completed = self.inner.completed.notified();
            tokio::pin!(completed);
            completed.as_mut().enable();
            if self.raw_len() == 0 {
                return match self
                    .inner
                    .failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                {
                    Some(error) => Err(RuntimeError::Application(error.clone())),
                    None => Ok(()),
                };
            }
            completed.await;
        }
    }

    fn raw_len(&self) -> usize {
        self.inner
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

struct CallbackContainmentGuard {
    engine: Arc<StatelessReplicator>,
}

struct AppliedWriteGuard {
    engine: Weak<StatelessReplicator>,
    lsn: i64,
    armed: bool,
}

impl AppliedWriteGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AppliedWriteGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(engine) = self.engine.upgrade() else {
            return;
        };
        let lsn = self.lsn;
        engine
            .abandoned_applied_floor
            .fetch_max(lsn, Ordering::AcqRel);
        let descendants = engine.descendants.clone();
        descendants.spawn(async move {
            engine.contain_ambiguous_suffix(lsn).await;
        });
    }
}

impl Drop for CallbackContainmentGuard {
    fn drop(&mut self) {
        if self
            .engine
            .active_callbacks
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
            == 1
        {
            self.engine.callback_containment.send_replace(true);
        }
    }
}

pub struct StatelessReplicator {
    identity: ReplicaIdentity,
    process_session: ProcessSessionId,
    context: ReplicatorFactoryContext,
    provider: Arc<dyn StateProvider>,
    storage: Arc<dyn DurableState>,
    settings: RwLock<ReplicatorSettings>,
    bind_address: SocketAddr,
    streams: Arc<ServiceStreams>,
    state: Mutex<EngineState>,
    closed: AtomicBool,
    authority_generation: AtomicU64,
    abandoned_applied_floor: AtomicI64,
    open_gate: Mutex<()>,
    write_gate: Mutex<()>,
    provider_effect_gate: Mutex<()>,
    inbound_build_admission_gate: Mutex<()>,
    listener: StdMutex<Option<ListenerOwner>>,
    descendants: TaskRegistry,
    inbound_descendants: TaskRegistry,
    callback_containment: watch::Sender<bool>,
    active_callbacks: std::sync::atomic::AtomicUsize,
    self_ref: OnceLock<Weak<StatelessReplicator>>,
}

impl StatelessReplicator {
    fn raw_retained_task_cardinality(&self) -> (usize, usize) {
        (
            self.descendants.raw_len(),
            self.inbound_descendants.raw_len(),
        )
    }

    fn new(
        identity: ReplicaIdentity,
        process_session: ProcessSessionId,
        context: ReplicatorFactoryContext,
        provider: Arc<dyn StateProvider>,
        storage: Arc<dyn DurableState>,
        settings: ReplicatorSettings,
        bind_address: SocketAddr,
    ) -> Self {
        let (callback_containment, _) = watch::channel(true);
        Self {
            identity,
            process_session,
            context,
            provider,
            storage,
            settings: RwLock::new(settings),
            bind_address,
            streams: Arc::new(ServiceStreams::new()),
            state: Mutex::new(EngineState {
                opened: false,
                role: ReplicaRole::None,
                role_epoch: None,
                updated_epoch: None,
                authority: None,
                retired: BTreeSet::new(),
                inbound_build: None,
                applied_lsn: 0,
                committed_lsn: 0,
                ambiguous_suffix: false,
                local_rebuild_required: false,
                closed: false,
            }),
            closed: AtomicBool::new(false),
            authority_generation: AtomicU64::new(1),
            abandoned_applied_floor: AtomicI64::new(0),
            open_gate: Mutex::new(()),
            write_gate: Mutex::new(()),
            provider_effect_gate: Mutex::new(()),
            inbound_build_admission_gate: Mutex::new(()),
            listener: StdMutex::new(None),
            descendants: TaskRegistry::new(),
            inbound_descendants: TaskRegistry::new(),
            callback_containment,
            active_callbacks: std::sync::atomic::AtomicUsize::new(0),
            self_ref: OnceLock::new(),
        }
    }

    fn generation(&self) -> u64 {
        self.authority_generation.load(Ordering::Acquire)
    }

    fn revoke_generation(&self) -> u64 {
        self.authority_generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    async fn exact_authority_is_current(
        &self,
        authority: &ConfigurationAuthority,
        generation: u64,
        role: ReplicaRole,
    ) -> bool {
        if self.closed.load(Ordering::Acquire) || self.generation() != generation {
            return false;
        }
        let state = self.state.lock().await;
        !state.closed
            && state.role == role
            && state.authority.as_ref() == Some(authority)
            && (role != ReplicaRole::Primary || state.role_epoch == Some(authority.epoch))
            && state.updated_epoch == Some(authority.epoch)
    }

    async fn fence_stale_write(&self, lsn: i64) -> RuntimeError {
        self.contain_ambiguous_suffix(lsn).await;
        RuntimeError::OperationCancelled
    }

    fn callback_containment(&self) -> watch::Receiver<bool> {
        self.callback_containment.subscribe()
    }

    async fn send_contained_build(&self, replica: ReplicaInformation) -> Result<()> {
        let engine = self.self_ref.get().and_then(Weak::upgrade).ok_or_else(|| {
            RuntimeError::Application("replacement creation is unattached".into())
        })?;
        if engine
            .active_callbacks
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            == 0
        {
            engine.callback_containment.send_replace(false);
        }
        let _guard = CallbackContainmentGuard {
            engine: engine.clone(),
        };
        let _effect = engine.provider_effect_gate.lock().await;
        let result = engine.send_build(&replica).await;
        if matches!(
            &result,
            Err(RuntimeError::ReplicaRebuildRequired(rebuild))
                if matches!(rebuild.as_ref(), ReplicaRebuildRequired::LocalProvider(_))
        ) {
            engine.fence_local_build_source().await;
        }
        result
    }

    async fn require_primary(&self) -> Result<ConfigurationAuthority> {
        if self.closed.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let state = self.state.lock().await;
        if state.closed {
            return Err(RuntimeError::Closed);
        }
        if !state.opened {
            return Err(RuntimeError::NotOpen);
        }
        if state.role != ReplicaRole::Primary {
            return Err(RuntimeError::NotPrimary);
        }
        if state.ambiguous_suffix || state.local_rebuild_required {
            return Err(RuntimeError::ReconfigurationPending);
        }
        if self.abandoned_applied_floor.load(Ordering::Acquire) > 0 {
            return Err(RuntimeError::ReconfigurationPending);
        }
        let authority = state.authority.clone().ok_or_else(|| {
            RuntimeError::AuthorityMismatch("no static configuration authority is admitted".into())
        })?;
        if state.role_epoch != Some(authority.epoch) {
            return Err(RuntimeError::AuthorityMismatch(format!(
                "primary role epoch {:?} differs from admitted epoch {:?}",
                state.role_epoch, authority.epoch
            )));
        }
        if state.updated_epoch != Some(authority.epoch) {
            return Err(RuntimeError::AuthorityMismatch(format!(
                "updated epoch {:?} differs from admitted epoch {:?}",
                state.updated_epoch, authority.epoch
            )));
        }
        if authority.current.primary_id != self.identity.replica_id {
            return Err(RuntimeError::AuthorityMismatch(
                "admitted configuration selects another primary".into(),
            ));
        }
        Ok(authority)
    }

    async fn publish_progress(&self, ambiguous: bool) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let (applied_lsn, committed_lsn, local_rebuild_required) = {
            let state = self.state.lock().await;
            (
                state.applied_lsn,
                state.committed_lsn,
                state.local_rebuild_required,
            )
        };
        self.context
            .publish_replacement_progress(
                applied_lsn,
                committed_lsn,
                ambiguous || local_rebuild_required,
            )
            .await
    }

    async fn contain_ambiguous_suffix(&self, applied_floor: i64) {
        self.abandoned_applied_floor
            .fetch_max(applied_floor, Ordering::AcqRel);
        {
            let mut state = self.state.lock().await;
            state.applied_lsn = state.applied_lsn.max(applied_floor);
            state.ambiguous_suffix = true;
        }
        let _ = self.publish_progress(true).await;
    }

    async fn verify_retained_payload(&self, lsn: i64, data: &[u8]) -> Result<()> {
        let mut retained = self.storage.get_replication_operations(lsn, lsn).await?;
        let operation = retained.next().await.ok_or_else(|| {
            RuntimeError::InvalidReplication("committed watermark lacks retained operation".into())
        })??;
        if operation.lsn != lsn
            || operation.data.as_ref() != data
            || retained.next().await.is_some()
        {
            return Err(RuntimeError::InvalidReplication(
                "committed watermark changed retained operation".into(),
            ));
        }
        Ok(())
    }

    async fn refresh_provider_progress(&self) -> Result<()> {
        let provider_committed = self.provider.last_committed_lsn().await?;
        let progress = self.storage.durable_progress().await?;
        if provider_committed != progress.committed_lsn
            || progress.applied_lsn < progress.committed_lsn
            || progress.committed_lsn < 0
        {
            return Err(RuntimeError::Application(
                "provider and DurableState progress disagree".into(),
            ));
        }
        let mut state = self.state.lock().await;
        state.applied_lsn = progress.applied_lsn;
        state.committed_lsn = progress.committed_lsn;
        state.ambiguous_suffix = progress.applied_lsn > progress.committed_lsn;
        if !state.ambiguous_suffix {
            self.abandoned_applied_floor.store(0, Ordering::Release);
        }
        drop(state);
        self.publish_progress(progress.applied_lsn > progress.committed_lsn)
            .await
    }

    async fn fence_local_build_source(&self) {
        self.revoke_generation();
        self.cancel_inbound_effects();
        {
            let mut state = self.state.lock().await;
            state.authority = None;
            state.inbound_build = None;
            state.local_rebuild_required = true;
        }
        let _ = self.publish_progress(true).await;
    }

    async fn reconcile_provider_committed_tail(&self) -> Result<i64> {
        let provider_committed = self.provider.last_committed_lsn().await?;
        let progress = self.storage.durable_progress().await?;
        if provider_committed != progress.committed_lsn
            || progress.applied_lsn < progress.committed_lsn
            || progress.committed_lsn < 0
        {
            return Err(RuntimeError::Application(
                "provider and DurableState progress disagree after effect drainage".into(),
            ));
        }
        let mut state = self.state.lock().await;
        state.applied_lsn = state.applied_lsn.max(progress.applied_lsn);
        state.committed_lsn = state.committed_lsn.max(progress.committed_lsn);
        state.ambiguous_suffix = state.applied_lsn > state.committed_lsn;
        if !state.ambiguous_suffix {
            self.abandoned_applied_floor.store(0, Ordering::Release);
        }
        Ok(progress.committed_lsn)
    }

    fn cancel_inbound_effects(&self) {
        self.inbound_descendants.abort_all();
    }

    async fn revoke_inbound_effects(&self) -> Result<()> {
        self.context
            .revoke_replacement_inbound_build_authority()
            .await?;
        self.cancel_inbound_effects();
        self.state.lock().await.inbound_build = None;
        Ok(())
    }

    async fn accept_replication(
        &self,
        item: proto::ReplicationItem,
    ) -> std::result::Result<proto::ReplicationAck, Status> {
        let _effect = self.provider_effect_gate.lock().await;
        self.streams.drain().await;
        if item.protocol_version != PUBLIC_V1_PROTOCOL_VERSION {
            return Err(Status::failed_precondition(
                "stateless replacement requires Public V1",
            ));
        }
        let sender: ReplicaIdentity = item
            .sender
            .clone()
            .ok_or_else(|| Status::invalid_argument("missing sender"))?
            .try_into()
            .map_err(|error: crate::control::WireError| {
                Status::invalid_argument(error.to_string())
            })?;
        let receiver: ReplicaIdentity = item
            .receiver
            .clone()
            .ok_or_else(|| Status::invalid_argument("missing receiver"))?
            .try_into()
            .map_err(|error: crate::control::WireError| {
                Status::invalid_argument(error.to_string())
            })?;
        let epoch: Epoch = item
            .epoch
            .ok_or_else(|| Status::invalid_argument("missing epoch"))?
            .into();
        if receiver != self.identity
            || item.receiver_session_id != self.process_session.as_str()
            || item.lsn < 0
            || item.committed_lsn < 0
            || item.committed_lsn > item.lsn
        {
            return Err(Status::failed_precondition(
                "replication receiver, session, or progress is stale",
            ));
        }
        let generation = self.generation();
        let (authority, prior_applied, prior_committed) = {
            let state = self.state.lock().await;
            if state.closed || !state.opened || state.role != ReplicaRole::ActiveSecondary {
                return Err(Status::failed_precondition(
                    "replica is not an admitted active secondary",
                ));
            }
            let authority = state.authority.clone().ok_or_else(|| {
                Status::failed_precondition("static configuration is not admitted")
            })?;
            let peer = authority.peers.get(&sender).ok_or_else(|| {
                Status::failed_precondition("sender is not in the exact configuration")
            })?;
            let expected_previous = authority
                .previous
                .as_ref()
                .map_or_else(String::new, |previous| {
                    previous.configuration_id.to_string()
                });
            if epoch != authority.epoch
                || sender.replica_id != authority.current.primary_id
                || peer.role != ReplicaRole::Primary
                || item.current_configuration_id != authority.current.configuration_id.as_str()
                || item.previous_configuration_id != expected_previous
                || item.sender_session_id != peer.process_session_id.as_str()
            {
                return Err(Status::failed_precondition(
                    "replication authority or predecessor session is stale",
                ));
            }
            (authority, state.applied_lsn, state.committed_lsn)
        };
        if item.lsn == 0 {
            if item.committed_lsn != 0 || !item.data.is_empty() {
                return Err(Status::failed_precondition(
                    "zero-progress proof changed the empty durable boundary",
                ));
            }
            let progress = self
                .storage
                .durable_progress()
                .await
                .map_err(runtime_status)?;
            let provider_committed = self
                .provider
                .last_committed_lsn()
                .await
                .map_err(runtime_status)?;
            if progress.applied_lsn != 0
                || progress.committed_lsn != 0
                || provider_committed != 0
                || prior_applied != 0
                || prior_committed != 0
                || !self
                    .exact_authority_is_current(
                        &authority,
                        generation,
                        ReplicaRole::ActiveSecondary,
                    )
                    .await
            {
                return Err(Status::failed_precondition(
                    "zero-progress proof is not the exact live empty boundary",
                ));
            }
            return Ok(proto::ReplicationAck {
                protocol_version: PUBLIC_V1_PROTOCOL_VERSION,
                sender: Some(sender.into()),
                receiver: Some(receiver.into()),
                epoch: Some(epoch.into()),
                previous_configuration_id: authority
                    .previous
                    .as_ref()
                    .map_or_else(String::new, |previous| {
                        previous.configuration_id.to_string()
                    }),
                current_configuration_id: authority.current.configuration_id.to_string(),
                received_lsn: 0,
                applied_lsn: 0,
                committed_lsn: 0,
                sender_session_id: item.sender_session_id,
                receiver_session_id: item.receiver_session_id,
            });
        }
        if item.lsn > prior_applied.saturating_add(1)
            || (item.lsn > prior_applied && item.committed_lsn < prior_committed)
        {
            return Err(Status::failed_precondition(
                "replication progress is not contiguous",
            ));
        }
        if item.lsn <= prior_applied
            && let Err(error) = self
                .verify_retained_payload(item.lsn, item.data.as_slice())
                .await
        {
            self.contain_ambiguous_suffix(prior_applied).await;
            return Err(runtime_status(error));
        }
        // Service execution outlives the ingress future. Fence access before
        // delivery, without crediting any applied/committed progress. A dropped
        // request leaves this fence in place until verified ACK or recovery.
        self.state.lock().await.ambiguous_suffix = true;
        self.publish_progress(true).await.map_err(runtime_status)?;
        let committed = self
            .streams
            .deliver(
                OperationMetadata::Replication {
                    lsn: item.lsn,
                    committed_lsn: item.committed_lsn,
                },
                item.data.clone().into(),
            )
            .await
            .map_err(runtime_status)?;
        if committed.committed_lsn < prior_committed.max(item.committed_lsn)
            || committed.committed_lsn > committed.applied_lsn
            || committed.applied_lsn < prior_applied.max(item.lsn)
            || self
                .storage
                .durable_progress()
                .await
                .map_err(runtime_status)?
                != committed
        {
            return Err(Status::failed_precondition(
                "provider returned inconsistent durable commit progress",
            ));
        }
        self.verify_retained_payload(item.lsn, item.data.as_slice())
            .await
            .map_err(runtime_status)?;
        if committed.applied_lsn > prior_applied.max(item.lsn) {
            let mut verified = prior_applied.max(item.lsn);
            let mut retained = self
                .storage
                .get_replication_operations(verified + 1, committed.applied_lsn)
                .await
                .map_err(runtime_status)?;
            while let Some(operation) = retained.next().await {
                let operation = operation.map_err(runtime_status)?;
                if Some(operation.lsn) != verified.checked_add(1)
                    || operation.committed_lsn < 0
                    || operation.committed_lsn > operation.lsn
                {
                    return Err(Status::failed_precondition(
                        "ACK lacks contiguous durable retained history",
                    ));
                }
                verified = operation.lsn;
            }
            if verified != committed.applied_lsn {
                return Err(Status::failed_precondition(
                    "ACK exceeds verified durable retained history",
                ));
            }
        }
        {
            if !self
                .exact_authority_is_current(&authority, generation, ReplicaRole::ActiveSecondary)
                .await
            {
                self.contain_ambiguous_suffix(item.lsn).await;
                return Err(Status::failed_precondition(
                    "replication authority changed during provider commit",
                ));
            }
            let mut state = self.state.lock().await;
            state.applied_lsn = state.applied_lsn.max(committed.applied_lsn);
            state.committed_lsn = state.committed_lsn.max(committed.committed_lsn);
            state.ambiguous_suffix = state.applied_lsn > state.committed_lsn;
        }
        let ambiguous = {
            let state = self.state.lock().await;
            state.ambiguous_suffix
        };
        self.publish_progress(ambiguous)
            .await
            .map_err(runtime_status)?;
        if !self
            .exact_authority_is_current(&authority, generation, ReplicaRole::ActiveSecondary)
            .await
        {
            self.contain_ambiguous_suffix(item.lsn).await;
            return Err(Status::failed_precondition(
                "replication authority changed before acknowledgement",
            ));
        }
        Ok(proto::ReplicationAck {
            protocol_version: PUBLIC_V1_PROTOCOL_VERSION,
            sender: Some(sender.into()),
            receiver: Some(receiver.into()),
            epoch: Some(epoch.into()),
            previous_configuration_id: authority
                .previous
                .as_ref()
                .map_or_else(String::new, |previous| {
                    previous.configuration_id.to_string()
                }),
            current_configuration_id: authority.current.configuration_id.to_string(),
            received_lsn: item.lsn,
            applied_lsn: committed.applied_lsn,
            committed_lsn: committed.committed_lsn,
            sender_session_id: item.sender_session_id,
            receiver_session_id: item.receiver_session_id,
        })
    }

    async fn send_replication_proof(
        &self,
        authority: &ConfigurationAuthority,
        peer: &ReplicaInformation,
        operation: &Operation,
        committed_lsn: i64,
    ) -> std::result::Result<(), ReplicationSendFailure> {
        match self
            .send_replication_classified(authority, peer, operation, committed_lsn)
            .await
        {
            Err(ReplicationSendFailure::Unavailable(_)) => {
                self.send_replication_classified(authority, peer, operation, committed_lsn)
                    .await
            }
            result => result,
        }
    }

    async fn send_replication_classified(
        &self,
        authority: &ConfigurationAuthority,
        peer: &ReplicaInformation,
        operation: &Operation,
        committed_lsn: i64,
    ) -> std::result::Result<(), ReplicationSendFailure> {
        let endpoint = if peer.replication_address.starts_with("http://")
            || peer.replication_address.starts_with("https://")
        {
            peer.replication_address.clone()
        } else {
            format!("http://{}", peer.replication_address)
        };
        tokio::time::timeout(PEER_RPC_TIMEOUT, async {
            #[cfg(feature = "testing")]
            let endpoint = testing_transport::route(
                self.identity.instance_id.as_str(),
                peer.identity.instance_id.as_str(),
                endpoint,
            )
            .await
            .map_err(|error| ReplicationSendFailure::Unavailable(error.to_string()))?;
            let mut client =
                proto::replication_data_client::ReplicationDataClient::connect(endpoint)
                    .await
                    .map_err(|error| ReplicationSendFailure::Unavailable(error.to_string()))?;
            let item = proto::ReplicationItem {
                protocol_version: PUBLIC_V1_PROTOCOL_VERSION,
                sender: Some(self.identity.clone().into()),
                receiver: Some(peer.identity.clone().into()),
                epoch: Some(authority.epoch.into()),
                previous_configuration_id: authority
                    .previous
                    .as_ref()
                    .map_or_else(String::new, |previous| {
                        previous.configuration_id.to_string()
                    }),
                current_configuration_id: authority.current.configuration_id.to_string(),
                lsn: operation.lsn,
                committed_lsn,
                data: operation.data.to_vec(),
                sender_session_id: self.process_session.to_string(),
                receiver_session_id: peer.process_session_id.to_string(),
            };
            let response = client
                .replicate(Request::new(stream::iter([item])))
                .await
                .map_err(|error| match error.code() {
                    tonic::Code::Cancelled
                    | tonic::Code::DeadlineExceeded
                    | tonic::Code::Unavailable => {
                        ReplicationSendFailure::Unavailable(error.to_string())
                    }
                    _ => ReplicationSendFailure::Rejected(error.to_string()),
                })?;
            let acknowledgement = response
                .into_inner()
                .next()
                .await
                .ok_or_else(|| {
                    ReplicationSendFailure::Rejected("replication acknowledgement closed".into())
                })?
                .map_err(|error| ReplicationSendFailure::Rejected(error.to_string()))?;
            let acknowledgement_sender: ReplicaIdentity = acknowledgement
                .sender
                .clone()
                .ok_or_else(|| ReplicationSendFailure::Rejected("ACK sender is absent".into()))?
                .try_into()
                .map_err(|error: crate::control::WireError| {
                    ReplicationSendFailure::Rejected(error.to_string())
                })?;
            let acknowledgement_receiver: ReplicaIdentity = acknowledgement
                .receiver
                .clone()
                .ok_or_else(|| ReplicationSendFailure::Rejected("ACK receiver is absent".into()))?
                .try_into()
                .map_err(|error: crate::control::WireError| {
                    ReplicationSendFailure::Rejected(error.to_string())
                })?;
            let acknowledgement_epoch: Epoch = acknowledgement
                .epoch
                .ok_or_else(|| ReplicationSendFailure::Rejected("ACK epoch is absent".into()))?
                .into();
            if acknowledgement.protocol_version != PUBLIC_V1_PROTOCOL_VERSION
                || acknowledgement_sender != self.identity
                || acknowledgement_receiver != peer.identity
                || acknowledgement_epoch != authority.epoch
                || acknowledgement.sender_session_id != self.process_session.as_str()
                || acknowledgement.receiver_session_id != peer.process_session_id.as_str()
                || acknowledgement.current_configuration_id
                    != authority.current.configuration_id.as_str()
                || acknowledgement.previous_configuration_id
                    != authority
                        .previous
                        .as_ref()
                        .map_or_else(String::new, |previous| {
                            previous.configuration_id.to_string()
                        })
                || acknowledgement.received_lsn != operation.lsn
                || acknowledgement.applied_lsn < operation.lsn
                || acknowledgement.committed_lsn < committed_lsn
            {
                return Err(ReplicationSendFailure::Rejected(
                    "stateless replacement ACK changed exact authority".into(),
                ));
            }
            Ok(())
        })
        .await
        .unwrap_or_else(|_| {
            Err(ReplicationSendFailure::Unavailable(
                "replication peer deadline elapsed".into(),
            ))
        })
    }

    async fn send_replication(
        &self,
        authority: &ConfigurationAuthority,
        peer: &ReplicaInformation,
        operation: &Operation,
        committed_lsn: i64,
    ) -> Result<()> {
        self.send_replication_classified(authority, peer, operation, committed_lsn)
            .await
            .map_err(|error| match error {
                ReplicationSendFailure::Unavailable(detail)
                | ReplicationSendFailure::Rejected(detail) => RuntimeError::Application(detail),
            })
    }

    fn remote_rebuild_required(
        authority: &ConfigurationAuthority,
        peer: &ReplicaInformation,
    ) -> RuntimeError {
        RuntimeError::ReplicaRebuildRequired(Box::new(ReplicaRebuildRequired::RemoteBuildTarget(
            Box::new(RemoteBuildTargetRebuildRequired {
                reason: ReplicaRebuildReason::IncompatibleRetainedHistory,
                epoch: authority.epoch,
                target: peer.identity.clone(),
                target_session: peer.process_session_id.clone(),
            }),
        )))
    }

    fn local_rebuild_required(&self) -> RuntimeError {
        RuntimeError::ReplicaRebuildRequired(Box::new(ReplicaRebuildRequired::LocalProvider(
            LocalProviderRebuildRequired {
                reason: ReplicaRebuildReason::IncompatibleRetainedHistory,
                provider: ProviderReplicaIdentity {
                    replica: self.identity.clone(),
                },
            },
        )))
    }

    async fn verify_peer_applied_suffix(
        &self,
        authority: &ConfigurationAuthority,
        peer: &ReplicaInformation,
        committed_lsn: i64,
        applied_lsn: i64,
    ) -> Result<Option<i64>> {
        // A peer may also lack committed entries. Replay that prefix before
        // verifying every ambiguous entry, even if its advertised tail is ahead.
        let mut retained_peer = peer.clone();
        retained_peer.current_progress = peer.current_progress.min(committed_lsn);
        match self
            .catch_up_peer(authority, &retained_peer, applied_lsn, committed_lsn)
            .await
        {
            Ok(progress) => Ok(Some(progress)),
            Err(RuntimeError::Application(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn catch_up_peer(
        &self,
        authority: &ConfigurationAuthority,
        peer: &ReplicaInformation,
        through_lsn: i64,
        committed_lsn: i64,
    ) -> Result<i64> {
        if through_lsn < 0 {
            return Err(RuntimeError::ReconfigurationPending);
        }
        if through_lsn == 0 {
            let proof = Operation {
                lsn: 0,
                committed_lsn: 0,
                data: OperationData::default(),
            };
            return match self
                .send_replication_proof(authority, peer, &proof, 0)
                .await
            {
                Ok(()) => Ok(0),
                Err(ReplicationSendFailure::Unavailable(detail)) => {
                    Err(RuntimeError::Application(detail))
                }
                Err(ReplicationSendFailure::Rejected(_)) => {
                    Err(Self::remote_rebuild_required(authority, peer))
                }
            };
        }
        let first = if peer.current_progress >= through_lsn {
            through_lsn
        } else {
            peer.current_progress.saturating_add(1).max(1)
        };
        let mut retained = self
            .storage
            .get_replication_operations(first, through_lsn)
            .await?;
        let mut expected = first;
        while let Some(operation) = retained.next().await {
            let operation = operation?;
            if operation.lsn != expected || !self.storage.verify_applied(&operation).await? {
                return Err(self.local_rebuild_required());
            }
            match self
                .send_replication_proof(
                    authority,
                    peer,
                    &operation,
                    committed_lsn.min(operation.lsn),
                )
                .await
            {
                Ok(()) => {}
                Err(ReplicationSendFailure::Unavailable(detail)) => {
                    return Err(RuntimeError::Application(detail));
                }
                Err(ReplicationSendFailure::Rejected(_)) => {
                    return Err(Self::remote_rebuild_required(authority, peer));
                }
            }
            expected = expected.saturating_add(1);
        }
        if expected <= through_lsn {
            return Err(self.local_rebuild_required());
        }
        Ok(through_lsn)
    }

    async fn settle_applied_suffix(
        &self,
        authority: &ConfigurationAuthority,
        mode: ReplicaSetQuorumMode,
    ) -> Result<BTreeMap<ReplicaIdentity, i64>> {
        let (applied_lsn, committed_lsn) = {
            let state = self.state.lock().await;
            (state.applied_lsn, state.committed_lsn)
        };
        let mut peer_progress = BTreeMap::new();
        let mut pending = FuturesUnordered::new();
        for peer in authority.peers.values() {
            pending.push(async move {
                let result = if applied_lsn > committed_lsn {
                    self.verify_peer_applied_suffix(authority, peer, committed_lsn, applied_lsn)
                        .await
                } else {
                    self.catch_up_peer(authority, peer, applied_lsn, committed_lsn)
                        .await
                        .map(Some)
                };
                (peer.identity.clone(), result)
            });
        }
        while let Some((identity, result)) = pending.next().await {
            match result {
                Ok(Some(progress)) => {
                    peer_progress.insert(identity, progress);
                }
                Ok(None) => {}
                Err(error @ RuntimeError::ReplicaRebuildRequired(_)) => return Err(error),
                Err(_) => {}
            }
            if mode == ReplicaSetQuorumMode::WriteQuorum {
                let ready = std::iter::once(&self.identity)
                    .chain(peer_progress.keys())
                    .cloned()
                    .collect::<BTreeSet<_>>();
                let quorum = |configuration: &ConfigurationDescriptor| {
                    configuration
                        .members
                        .iter()
                        .filter(|member| ready.contains(&member.identity))
                        .count()
                        >= configuration.write_quorum as usize
                };
                let successor_ready = authority
                    .peers
                    .values()
                    .find(|peer| peer.must_catchup)
                    .is_none_or(|peer| ready.contains(&peer.identity));
                if quorum(&authority.current)
                    && authority.previous.as_ref().is_none_or(quorum)
                    && successor_ready
                {
                    break;
                }
            }
        }
        if applied_lsn > committed_lsn {
            let ready = std::iter::once(self.identity.clone())
                .chain(peer_progress.keys().cloned())
                .collect::<BTreeSet<_>>();
            let quorum = |configuration: &ConfigurationDescriptor| {
                configuration
                    .members
                    .iter()
                    .filter(|member| ready.contains(&member.identity))
                    .count()
                    >= configuration.write_quorum as usize
            };
            if !quorum(&authority.current)
                || authority
                    .previous
                    .as_ref()
                    .is_some_and(|previous| !quorum(previous))
            {
                return Err(RuntimeError::ReconfigurationPending);
            }
            let committed = self.storage.commit(applied_lsn).await?;
            if committed.committed_lsn < applied_lsn
                || committed.committed_lsn > committed.applied_lsn
            {
                self.contain_ambiguous_suffix(applied_lsn).await;
                return Err(RuntimeError::Application(
                    "provider could not settle its applied suffix".into(),
                ));
            }
            {
                let mut state = self.state.lock().await;
                if state.authority.as_ref() != Some(authority) {
                    return Err(RuntimeError::OperationCancelled);
                }
                state.applied_lsn = committed.applied_lsn;
                state.committed_lsn = committed.committed_lsn;
                state.ambiguous_suffix = false;
            }
            self.abandoned_applied_floor.store(0, Ordering::Release);
            self.publish_progress(false).await?;
            for peer in authority.peers.values() {
                if peer_progress.contains_key(&peer.identity) {
                    let retained = self
                        .storage
                        .get_replication_operations(applied_lsn, applied_lsn)
                        .await?;
                    futures::pin_mut!(retained);
                    if let Some(Ok(operation)) = retained.next().await {
                        let _ = self
                            .send_replication(authority, peer, &operation, applied_lsn)
                            .await;
                    }
                }
            }
        }
        Ok(peer_progress)
    }

    async fn send_build(&self, replica: &ReplicaInformation) -> Result<()> {
        let authority = self.require_primary().await.map_err(|error| {
            RuntimeError::AuthorityMismatch(format!(
                "build source primary authority is unavailable: {error}"
            ))
        })?;
        if replica.role != ReplicaRole::IdleSecondary
            || replica.build_id.is_empty()
            || replica.process_session_id.is_empty()
            || replica.replication_address.is_empty()
            || replica.current_progress != crate::application::INVALID_LSN
            || replica.catch_up_capability != crate::application::INVALID_LSN
            || replica
                .storage_incarnation
                .as_ref()
                .is_none_or(|storage| storage.replica != replica.identity)
        {
            return Err(RuntimeError::AuthorityMismatch(
                "build target lacks exact idle build authority".into(),
            ));
        }
        {
            let state = self.state.lock().await;
            if state.retired.contains(&replica.identity)
                || authority
                    .current
                    .members
                    .iter()
                    .any(|member| member.identity == replica.identity)
                || authority.previous.as_ref().is_some_and(|configuration| {
                    configuration
                        .members
                        .iter()
                        .any(|member| member.identity == replica.identity)
                })
            {
                return Err(RuntimeError::AuthorityMismatch(
                    "build target is retired or already in PC/CC".into(),
                ));
            }
        }
        let build_authority = self
            .context
            .replacement_outbound_build_authority(
                &replica.build_id,
                &authority.current.configuration_id,
            )
            .await
            .map_err(|error| {
                RuntimeError::AuthorityMismatch(format!(
                    "outbound build authority lookup failed: {error}"
                ))
            })?
            .ok_or_else(|| {
                RuntimeError::AuthorityMismatch(
                    "outbound build authority is not durably admitted".into(),
                )
            })?;
        if build_authority.attempt.build_id != replica.build_id
            || build_authority.attempt.source != self.identity
            || build_authority.attempt.source_session != self.process_session
            || build_authority.attempt.target != replica.identity
            || build_authority.attempt.target_session != replica.process_session_id
            || replica.storage_incarnation.as_ref() != Some(&build_authority.attempt.target_storage)
            || build_authority.target_address != replica.replication_address
            || build_authority.current_configuration_id != authority.current.configuration_id
            || build_authority.attempt.epoch != authority.epoch
        {
            return Err(RuntimeError::AuthorityMismatch(
                "outbound build differs from durable exact authority".into(),
            ));
        }
        let replication_boundary_lsn = {
            let state = self.state.lock().await;
            state.committed_lsn
        };
        let copy_context = self.provider.get_copy_context().await?;
        let mut copy = self
            .provider
            .get_copy_state(replication_boundary_lsn, copy_context)
            .await?;
        let mut items = Vec::new();
        let mut sequence = 0_u64;
        while let Some(chunk) = copy.next().await {
            items.push(proto::CopyItem {
                protocol_version: PUBLIC_V1_PROTOCOL_VERSION,
                build_id: replica.build_id.to_string(),
                sender: Some(self.identity.clone().into()),
                receiver: Some(replica.identity.clone().into()),
                epoch: Some(authority.epoch.into()),
                current_configuration_id: authority.current.configuration_id.to_string(),
                sequence,
                lsn: replication_boundary_lsn,
                committed_lsn: replication_boundary_lsn,
                replication_boundary_lsn,
                final_item: false,
                data: chunk?.to_vec(),
                snapshot_chunk: true,
                sender_session_id: self.process_session.to_string(),
                receiver_session_id: replica.process_session_id.to_string(),
                catch_up_boundary_lsn: None,
                source_storage: Some(crate::control::convert::public_v1_storage_to_proto(
                    &build_authority.attempt.source_storage,
                )),
                target_storage: Some(crate::control::convert::public_v1_storage_to_proto(
                    &build_authority.attempt.target_storage,
                )),
                history: Some(crate::control::convert::public_v1_history_to_proto(
                    &build_authority.history,
                )),
                target_address: build_authority.target_address.clone(),
            });
            sequence = sequence.saturating_add(1);
        }
        items.push(proto::CopyItem {
            protocol_version: PUBLIC_V1_PROTOCOL_VERSION,
            build_id: replica.build_id.to_string(),
            sender: Some(self.identity.clone().into()),
            receiver: Some(replica.identity.clone().into()),
            epoch: Some(authority.epoch.into()),
            current_configuration_id: authority.current.configuration_id.to_string(),
            sequence,
            lsn: replication_boundary_lsn,
            committed_lsn: replication_boundary_lsn,
            replication_boundary_lsn,
            final_item: false,
            data: Vec::new(),
            snapshot_chunk: false,
            sender_session_id: self.process_session.to_string(),
            receiver_session_id: replica.process_session_id.to_string(),
            catch_up_boundary_lsn: None,
            source_storage: Some(crate::control::convert::public_v1_storage_to_proto(
                &build_authority.attempt.source_storage,
            )),
            target_storage: Some(crate::control::convert::public_v1_storage_to_proto(
                &build_authority.attempt.target_storage,
            )),
            history: Some(crate::control::convert::public_v1_history_to_proto(
                &build_authority.history,
            )),
            target_address: build_authority.target_address.clone(),
        });
        sequence = sequence.saturating_add(1);
        let catch_up_boundary_lsn = self.storage.durable_progress().await?.committed_lsn;
        if catch_up_boundary_lsn < replication_boundary_lsn {
            return Err(RuntimeError::Application(
                "provider committed progress regressed during build".into(),
            ));
        }
        if catch_up_boundary_lsn > replication_boundary_lsn {
            let mut retained = self
                .storage
                .get_replication_operations(
                    replication_boundary_lsn.saturating_add(1),
                    catch_up_boundary_lsn,
                )
                .await?;
            let mut expected = replication_boundary_lsn.saturating_add(1);
            while let Some(operation) = retained.next().await {
                let operation = operation?;
                if operation.lsn != expected || !self.storage.verify_applied(&operation).await? {
                    return Err(self.local_rebuild_required());
                }
                items.push(proto::CopyItem {
                    protocol_version: PUBLIC_V1_PROTOCOL_VERSION,
                    build_id: replica.build_id.to_string(),
                    sender: Some(self.identity.clone().into()),
                    receiver: Some(replica.identity.clone().into()),
                    epoch: Some(authority.epoch.into()),
                    current_configuration_id: authority.current.configuration_id.to_string(),
                    sequence,
                    lsn: operation.lsn,
                    committed_lsn: operation.lsn,
                    replication_boundary_lsn,
                    final_item: false,
                    data: operation.data.to_vec(),
                    snapshot_chunk: false,
                    sender_session_id: self.process_session.to_string(),
                    receiver_session_id: replica.process_session_id.to_string(),
                    catch_up_boundary_lsn: Some(catch_up_boundary_lsn),
                    source_storage: Some(crate::control::convert::public_v1_storage_to_proto(
                        &build_authority.attempt.source_storage,
                    )),
                    target_storage: Some(crate::control::convert::public_v1_storage_to_proto(
                        &build_authority.attempt.target_storage,
                    )),
                    history: Some(crate::control::convert::public_v1_history_to_proto(
                        &build_authority.history,
                    )),
                    target_address: build_authority.target_address.clone(),
                });
                sequence = sequence.saturating_add(1);
                expected = expected.saturating_add(1);
            }
            if expected <= catch_up_boundary_lsn {
                return Err(self.local_rebuild_required());
            }
        }
        items.push(proto::CopyItem {
            protocol_version: PUBLIC_V1_PROTOCOL_VERSION,
            build_id: replica.build_id.to_string(),
            sender: Some(self.identity.clone().into()),
            receiver: Some(replica.identity.clone().into()),
            epoch: Some(authority.epoch.into()),
            current_configuration_id: authority.current.configuration_id.to_string(),
            sequence,
            lsn: catch_up_boundary_lsn,
            committed_lsn: catch_up_boundary_lsn,
            replication_boundary_lsn,
            final_item: true,
            data: Vec::new(),
            snapshot_chunk: false,
            sender_session_id: self.process_session.to_string(),
            receiver_session_id: replica.process_session_id.to_string(),
            catch_up_boundary_lsn: Some(catch_up_boundary_lsn),
            source_storage: Some(crate::control::convert::public_v1_storage_to_proto(
                &build_authority.attempt.source_storage,
            )),
            target_storage: Some(crate::control::convert::public_v1_storage_to_proto(
                &build_authority.attempt.target_storage,
            )),
            history: Some(crate::control::convert::public_v1_history_to_proto(
                &build_authority.history,
            )),
            target_address: build_authority.target_address.clone(),
        });

        let endpoint = if replica.replication_address.starts_with("http://")
            || replica.replication_address.starts_with("https://")
        {
            replica.replication_address.clone()
        } else {
            format!("http://{}", replica.replication_address)
        };
        let remote_conflict = || {
            RuntimeError::ReplicaRebuildRequired(Box::new(
                ReplicaRebuildRequired::RemoteBuildTarget(Box::new(
                    RemoteBuildTargetRebuildRequired {
                        reason: ReplicaRebuildReason::RemoteCopyConflict,
                        epoch: authority.epoch,
                        target: replica.identity.clone(),
                        target_session: replica.process_session_id.clone(),
                    },
                )),
            ))
        };
        let final_ack = tokio::time::timeout(PEER_RPC_TIMEOUT, async {
            #[cfg(feature = "testing")]
            let endpoint = testing_transport::route(
                self.identity.instance_id.as_str(),
                replica.identity.instance_id.as_str(),
                endpoint,
            )
            .await
            .map_err(|_| remote_conflict())?;
            let mut client =
                proto::replication_data_client::ReplicationDataClient::connect(endpoint)
                    .await
                    .map_err(|_| remote_conflict())?;
            let response = client
                .build(Request::new(stream::iter(items)))
                .await
                .map_err(|_| remote_conflict())?;
            let mut acknowledgements = response.into_inner();
            let mut final_ack = None;
            while let Some(acknowledgement) = acknowledgements.next().await {
                let acknowledgement = acknowledgement.map_err(|_| remote_conflict())?;
                if acknowledgement.protocol_version != PUBLIC_V1_PROTOCOL_VERSION
                    || acknowledgement.build_id != replica.build_id.as_str()
                    || acknowledgement.sender_session_id != replica.process_session_id.as_str()
                    || acknowledgement.receiver_session_id != self.process_session.as_str()
                    || acknowledgement.sender.as_ref() != Some(&replica.identity.clone().into())
                    || acknowledgement.receiver.as_ref() != Some(&self.identity.clone().into())
                    || acknowledgement.epoch.as_ref() != Some(&authority.epoch.into())
                    || acknowledgement.current_configuration_id
                        != authority.current.configuration_id.as_str()
                    || acknowledgement.replication_boundary_lsn != replication_boundary_lsn
                {
                    return Err(RuntimeError::InvalidReplication(
                        "build ACK changed exact authority".into(),
                    ));
                }
                if acknowledgement.final_item {
                    final_ack = Some(acknowledgement);
                }
            }
            final_ack.ok_or(RuntimeError::WriteCompletionClosed)
        })
        .await
        .map_err(|_| RuntimeError::OperationCancelled)??;
        if final_ack.durable_lsn < catch_up_boundary_lsn
            || final_ack.catch_up_boundary_lsn != Some(catch_up_boundary_lsn)
        {
            return Err(RuntimeError::ReplicaRebuildRequired(Box::new(
                ReplicaRebuildRequired::RemoteBuildTarget(Box::new(
                    RemoteBuildTargetRebuildRequired {
                        reason: ReplicaRebuildReason::IncompatibleRetainedHistory,
                        epoch: authority.epoch,
                        target: replica.identity.clone(),
                        target_session: replica.process_session_id.clone(),
                    },
                )),
            )));
        }
        Ok(())
    }

    async fn accept_copy(
        &self,
        item: proto::CopyItem,
    ) -> std::result::Result<proto::CopyAck, Status> {
        let _effect = self.provider_effect_gate.lock().await;
        self.streams.drain().await;
        if item.protocol_version != PUBLIC_V1_PROTOCOL_VERSION {
            return Err(Status::failed_precondition(
                "stateless replacement requires Public V1",
            ));
        }
        let build_id = crate::protocol::types::OperationId::new(item.build_id.clone());
        let sender: ReplicaIdentity = item
            .sender
            .clone()
            .ok_or_else(|| Status::invalid_argument("missing build sender"))?
            .try_into()
            .map_err(|error: crate::control::WireError| {
                Status::invalid_argument(error.to_string())
            })?;
        let receiver: ReplicaIdentity = item
            .receiver
            .clone()
            .ok_or_else(|| Status::invalid_argument("missing build receiver"))?
            .try_into()
            .map_err(|error: crate::control::WireError| {
                Status::invalid_argument(error.to_string())
            })?;
        let epoch: Epoch = item
            .epoch
            .ok_or_else(|| Status::invalid_argument("missing build epoch"))?
            .into();
        let source_storage = crate::control::convert::public_v1_storage_from_proto(
            item.source_storage
                .clone()
                .ok_or_else(|| Status::invalid_argument("missing source storage"))?,
        )
        .map_err(|error| Status::invalid_argument(error.to_string()))?;
        let target_storage = crate::control::convert::public_v1_storage_from_proto(
            item.target_storage
                .clone()
                .ok_or_else(|| Status::invalid_argument("missing target storage"))?,
        )
        .map_err(|error| Status::invalid_argument(error.to_string()))?;
        let history = crate::control::convert::public_v1_history_from_proto(
            item.history
                .clone()
                .ok_or_else(|| Status::invalid_argument("missing build history"))?,
        )
        .map_err(|error| Status::invalid_argument(error.to_string()))?;
        if build_id.is_empty()
            || receiver != self.identity
            || item.sender_session_id.is_empty()
            || item.current_configuration_id.is_empty()
            || item.replication_boundary_lsn < 0
            || item.committed_lsn < 0
            || item.target_address != self.settings.read().await.replication_address
        {
            return Err(Status::failed_precondition(
                "build identity, session, or progress is stale",
            ));
        }
        let current_configuration_id =
            crate::protocol::types::ConfigurationId::new(item.current_configuration_id.clone());
        if let Some(receipt) = self
            .context
            .replacement_terminal_inbound_build_receipt(&build_id)
            .await
            .map_err(runtime_status)?
        {
            let authority = &receipt.authority;
            if authority.attempt.source != sender
                || authority.attempt.source_storage != source_storage
                || authority.attempt.source_session.as_str() != item.sender_session_id
                || authority.attempt.target != receiver
                || authority.attempt.target_storage != target_storage
                || authority.attempt.target_session.as_str() != item.receiver_session_id
                || authority.attempt.epoch != epoch
                || authority.history != history
                || authority.current_configuration_id != current_configuration_id
                || authority.target_address != item.target_address
                || !item.final_item
                || item.snapshot_chunk
                || !item.data.is_empty()
                || item.sequence != receipt.final_sequence
                || item.replication_boundary_lsn != receipt.replication_boundary_lsn
                || item.catch_up_boundary_lsn != Some(receipt.catch_up_boundary_lsn)
                || item.lsn != receipt.catch_up_boundary_lsn
                || item.committed_lsn != receipt.catch_up_boundary_lsn
            {
                return Err(Status::failed_precondition(
                    "completed build replay changed terminal authority",
                ));
            }
            return Ok(proto::CopyAck {
                protocol_version: PUBLIC_V1_PROTOCOL_VERSION,
                build_id: build_id.to_string(),
                sender: Some(receiver.into()),
                receiver: Some(sender.into()),
                epoch: Some(epoch.into()),
                current_configuration_id: current_configuration_id.to_string(),
                sequence: item.sequence,
                durable_lsn: receipt.durable_lsn,
                replication_boundary_lsn: receipt.replication_boundary_lsn,
                final_item: true,
                snapshot_chunk: false,
                sender_session_id: item.receiver_session_id,
                receiver_session_id: item.sender_session_id,
                catch_up_boundary_lsn: Some(receipt.catch_up_boundary_lsn),
            });
        }
        if item.receiver_session_id != self.process_session.as_str() {
            return Err(Status::failed_precondition(
                "active build receiver session is stale",
            ));
        }
        let durable_authority = self
            .context
            .replacement_inbound_build_authority(&build_id)
            .await
            .map_err(runtime_status)?
            .ok_or_else(|| Status::failed_precondition("build lacks durable inbound authority"))?;
        if durable_authority.attempt.build_id != build_id
            || durable_authority.attempt.source != sender
            || durable_authority.attempt.source_storage != source_storage
            || durable_authority.attempt.source_session.as_str() != item.sender_session_id
            || durable_authority.attempt.target != receiver
            || durable_authority.attempt.target_storage != target_storage
            || durable_authority.attempt.target_session.as_str() != item.receiver_session_id
            || durable_authority.attempt.epoch != epoch
            || durable_authority.history != history
            || durable_authority.current_configuration_id != current_configuration_id
            || durable_authority.target_address != item.target_address
        {
            return Err(Status::failed_precondition(
                "copy frame differs from durable inbound authority",
            ));
        }
        let generation = self.generation();
        let target_progress = self
            .storage
            .durable_progress()
            .await
            .map_err(runtime_status)?;
        let mut build = {
            let mut state = self.state.lock().await;
            if state.closed
                || !state.opened
                || state.role != ReplicaRole::IdleSecondary
                || state.role_epoch != Some(epoch)
                || state.updated_epoch != Some(epoch)
                || state.retired.contains(&receiver)
            {
                return Err(Status::failed_precondition(
                    "build target is not an admitted idle secondary",
                ));
            }
            let candidate = InboundBuild {
                authority: durable_authority.clone(),
                replication_boundary_lsn: item.replication_boundary_lsn,
                catch_up_boundary_lsn: item.catch_up_boundary_lsn,
                next_sequence: 0,
                copy_finished: false,
                retained_lsn: item.replication_boundary_lsn,
            };
            match &state.inbound_build {
                Some(existing)
                    if existing.authority != candidate.authority
                        || existing.replication_boundary_lsn
                            != candidate.replication_boundary_lsn =>
                {
                    return Err(Status::failed_precondition(
                        "another exact build attempt owns the target",
                    ));
                }
                Some(existing) => existing.clone(),
                None => {
                    if !copy_can_replace_tail(
                        target_progress.applied_lsn,
                        target_progress.committed_lsn,
                        item.replication_boundary_lsn,
                    ) {
                        return Err(Status::failed_precondition(
                            "inbound build cannot discard an established committed tail",
                        ));
                    }

                    state.inbound_build = Some(candidate.clone());
                    candidate
                }
            }
        };
        if item.sequence > build.next_sequence {
            return Err(Status::failed_precondition("build sequence has a gap"));
        }
        if item.sequence < build.next_sequence {
            if item.snapshot_chunk {
                let chunk = CopyChunk {
                    data: item.data.clone().into(),
                };
                if !self
                    .storage
                    .verify_copy_chunk(&build_id, item.sequence, &chunk)
                    .await
                    .map_err(runtime_status)?
                {
                    return Err(Status::failed_precondition(
                        "replayed copy chunk changed bytes",
                    ));
                }
                if self.generation() != generation
                    || self
                        .context
                        .replacement_inbound_build_authority(&build_id)
                        .await
                        .map_err(runtime_status)?
                        .as_ref()
                        != Some(&durable_authority)
                {
                    return Err(Status::failed_precondition(
                        "build authority changed during replay verification",
                    ));
                }
            } else if !item.final_item && item.lsn > build.replication_boundary_lsn {
                let operation = Operation {
                    lsn: item.lsn,
                    committed_lsn: item.committed_lsn.saturating_sub(1),
                    data: item.data.clone().into(),
                };
                if !self
                    .storage
                    .verify_applied(&operation)
                    .await
                    .map_err(runtime_status)?
                {
                    return Err(Status::failed_precondition(
                        "replayed retained build operation changed bytes",
                    ));
                }
            }
        } else if item.snapshot_chunk {
            if build.copy_finished {
                return Err(Status::failed_precondition(
                    "snapshot chunk arrived after copy completion",
                ));
            }
            let progress = self
                .streams
                .deliver(
                    OperationMetadata::Copy {
                        build_id: build_id.clone(),
                        sequence: item.sequence,
                    },
                    item.data.clone().into(),
                )
                .await
                .map_err(runtime_status)?;
            if progress
                != self
                    .storage
                    .durable_progress()
                    .await
                    .map_err(runtime_status)?
                || progress.applied_lsn < 0
                || progress.committed_lsn < 0
                || progress.committed_lsn > progress.applied_lsn
                || !self
                    .storage
                    .verify_copy_chunk(
                        &build_id,
                        item.sequence,
                        &CopyChunk {
                            data: item.data.clone().into(),
                        },
                    )
                    .await
                    .map_err(runtime_status)?
            {
                return Err(Status::failed_precondition(
                    "invalid durable copy acknowledgement",
                ));
            }
            if self.generation() != generation
                || self
                    .context
                    .replacement_inbound_build_authority(&build_id)
                    .await
                    .map_err(runtime_status)?
                    .as_ref()
                    != Some(&durable_authority)
            {
                return Err(Status::failed_precondition(
                    "build authority changed during copy apply",
                ));
            }
            build.next_sequence = build.next_sequence.saturating_add(1);
        } else {
            if !build.copy_finished {
                let progress = self
                    .streams
                    .deliver(
                        OperationMetadata::CopyComplete {
                            build_id: build_id.clone(),
                            up_to_lsn: build.replication_boundary_lsn,
                            committed_lsn: build.replication_boundary_lsn,
                        },
                        bytes::Bytes::new(),
                    )
                    .await
                    .map_err(runtime_status)?;
                if self.generation() != generation
                    || self
                        .context
                        .replacement_inbound_build_authority(&build_id)
                        .await
                        .map_err(runtime_status)?
                        .as_ref()
                        != Some(&durable_authority)
                {
                    return Err(Status::failed_precondition(
                        "build authority changed during copy completion",
                    ));
                }
                if progress.applied_lsn != build.replication_boundary_lsn
                    || progress.committed_lsn != build.replication_boundary_lsn
                    || progress
                        != self
                            .storage
                            .durable_progress()
                            .await
                            .map_err(runtime_status)?
                {
                    return Err(Status::failed_precondition(
                        "copy completion did not reach the frozen boundary",
                    ));
                }
                build.copy_finished = true;
            }
            if !item.final_item && item.lsn > build.replication_boundary_lsn {
                if Some(item.lsn) != build.retained_lsn.checked_add(1) {
                    return Err(Status::failed_precondition("retained build LSN has a gap"));
                }
                let progress = self
                    .streams
                    .deliver(
                        OperationMetadata::Replication {
                            lsn: item.lsn,
                            committed_lsn: item.committed_lsn,
                        },
                        item.data.clone().into(),
                    )
                    .await
                    .map_err(runtime_status)?;
                if progress.applied_lsn != item.lsn
                    || progress.committed_lsn != item.committed_lsn
                    || progress.committed_lsn > progress.applied_lsn
                    || progress
                        != self
                            .storage
                            .durable_progress()
                            .await
                            .map_err(runtime_status)?
                {
                    return Err(Status::failed_precondition(
                        "retained build operation was not durable",
                    ));
                }
                self.verify_retained_payload(item.lsn, item.data.as_slice())
                    .await
                    .map_err(runtime_status)?;
                if self.generation() != generation
                    || self
                        .context
                        .replacement_inbound_build_authority(&build_id)
                        .await
                        .map_err(runtime_status)?
                        .as_ref()
                        != Some(&durable_authority)
                {
                    return Err(Status::failed_precondition(
                        "build authority changed during retained apply",
                    ));
                }
                if progress.committed_lsn < item.committed_lsn {
                    return Err(Status::failed_precondition(
                        "retained build operation was not committed",
                    ));
                }
                build.retained_lsn = item.lsn;
            }
            build.catch_up_boundary_lsn = item.catch_up_boundary_lsn;
            build.next_sequence = build.next_sequence.saturating_add(1);
        }
        let progress = self
            .storage
            .durable_progress()
            .await
            .map_err(runtime_status)?;
        if self.generation() != generation
            || self
                .context
                .replacement_inbound_build_authority(&build_id)
                .await
                .map_err(runtime_status)?
                .as_ref()
                != Some(&durable_authority)
        {
            return Err(Status::failed_precondition(
                "build authority changed before acknowledgement",
            ));
        }
        {
            let mut state = self.state.lock().await;
            if state.closed
                || state.role != ReplicaRole::IdleSecondary
                || state.role_epoch != Some(epoch)
                || state.updated_epoch != Some(epoch)
                || state
                    .inbound_build
                    .as_ref()
                    .is_some_and(|active| active.authority != durable_authority)
            {
                return Err(Status::failed_precondition(
                    "build target authority changed after provider effect",
                ));
            }
            if item.final_item {
                if item.catch_up_boundary_lsn != Some(item.lsn)
                    || item.lsn != build.retained_lsn
                    || progress.committed_lsn < item.lsn
                {
                    return Err(Status::failed_precondition(
                        "build final admission precedes retained gap closure",
                    ));
                }
                state.applied_lsn = progress.applied_lsn;
                state.committed_lsn = progress.committed_lsn;
                state.ambiguous_suffix = progress.applied_lsn > progress.committed_lsn;
            } else {
                state.inbound_build = Some(build.clone());
            }
        }
        if item.final_item {
            self.context
                .complete_replacement_inbound_build(&PublicInboundBuildReceipt {
                    authority: durable_authority.clone(),
                    final_sequence: item.sequence,
                    replication_boundary_lsn: item.replication_boundary_lsn,
                    catch_up_boundary_lsn: item
                        .catch_up_boundary_lsn
                        .expect("validated final catch-up boundary"),
                    durable_lsn: progress.committed_lsn,
                })
                .await
                .map_err(runtime_status)?;
            self.state.lock().await.inbound_build = None;
        }
        self.publish_progress(progress.applied_lsn > progress.committed_lsn)
            .await
            .map_err(runtime_status)?;
        Ok(proto::CopyAck {
            protocol_version: PUBLIC_V1_PROTOCOL_VERSION,
            build_id: build_id.to_string(),
            sender: Some(receiver.into()),
            receiver: Some(sender.into()),
            epoch: Some(epoch.into()),
            current_configuration_id: current_configuration_id.to_string(),
            sequence: item.sequence,
            durable_lsn: progress.committed_lsn,
            replication_boundary_lsn: item.replication_boundary_lsn,
            final_item: item.final_item,
            snapshot_chunk: item.snapshot_chunk,
            sender_session_id: item.receiver_session_id,
            receiver_session_id: item.sender_session_id,
            catch_up_boundary_lsn: item.catch_up_boundary_lsn,
        })
    }

    fn abort_listener(&self) {
        if let Some(owner) = self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            owner.shutdown.send_replace(true);
            owner.task.abort();
        }
        self.descendants.abort_all();
        self.cancel_inbound_effects();
    }

    async fn fence_authority(&self) {
        let mut state = self.state.lock().await;
        state.authority = None;
    }
}

#[async_trait]
impl Replicator for StatelessReplicator {
    async fn open(&self) -> Result<String> {
        let _open = self.open_gate.lock().await;
        {
            let state = self.state.lock().await;
            if self.closed.load(Ordering::Acquire) || state.closed {
                return Err(RuntimeError::Closed);
            }
            if state.opened {
                return Ok(self.settings.read().await.replication_address.clone());
            }
        }
        let provider_committed = self.provider.last_committed_lsn().await?;
        let progress = self.storage.durable_progress().await?;
        if provider_committed != progress.committed_lsn
            || progress.applied_lsn < progress.committed_lsn
            || progress.committed_lsn < 0
        {
            return Err(RuntimeError::Application(
                "provider and DurableState progress disagree".into(),
            ));
        }
        if progress.applied_lsn > progress.committed_lsn {
            let mut retained = self
                .storage
                .get_replication_operations(
                    progress.committed_lsn.saturating_add(1),
                    progress.applied_lsn,
                )
                .await?;
            let mut expected = progress.committed_lsn.saturating_add(1);
            while let Some(operation) = retained.next().await {
                let operation = operation?;
                if operation.lsn != expected || !self.storage.verify_applied(&operation).await? {
                    return Err(RuntimeError::Application(
                        "provider retained suffix is incomplete or changed".into(),
                    ));
                }
                expected = expected.saturating_add(1);
            }
            if expected <= progress.applied_lsn {
                return Err(RuntimeError::Application(
                    "provider cannot reconstruct its applied suffix".into(),
                ));
            }
        }
        let listener = TcpListener::bind(self.bind_address)
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let address = {
            let mut settings = self.settings.write().await;
            if self.bind_address.port() == 0 {
                let bound = listener
                    .local_addr()
                    .map_err(|error| RuntimeError::Application(error.to_string()))?;
                settings.replication_address =
                    bound_replication_endpoint(&settings.replication_address, bound.port())?;
            }
            settings.replication_address.clone()
        };
        validate_public_v1_data_services(PUBLIC_V1_DATA_SERVICES)?;
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let service = ReplacementReplicationService {
            engine: self.self_ref.get().cloned().ok_or_else(|| {
                RuntimeError::Application("replacement creation is unattached".into())
            })?,
        };
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(PUBLIC_V1_DATA_SERVICES.replication_data(service))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                    while !*shutdown_rx.borrow_and_update() {
                        if shutdown_rx.changed().await.is_err() {
                            break;
                        }
                    }
                })
                .await
                .map_err(|error| RuntimeError::Application(error.to_string()))
        });
        *self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(ListenerOwner { shutdown, task });
        let mut state = self.state.lock().await;
        if state.closed {
            drop(state);
            self.abort_listener();
            return Err(RuntimeError::Closed);
        }
        state.opened = true;
        state.applied_lsn = progress.applied_lsn;
        state.committed_lsn = progress.committed_lsn;
        state.ambiguous_suffix = progress.applied_lsn > progress.committed_lsn;
        let ambiguous = state.ambiguous_suffix;
        drop(state);
        if let Err(error) = self.publish_progress(ambiguous).await {
            self.abort_listener();
            return Err(error);
        }
        Ok(address)
    }

    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()> {
        #[cfg(feature = "testing")]
        notify_inbound_build_boundary_waiting();
        let _admission = self.inbound_build_admission_gate.lock().await;
        {
            let mut state = self.state.lock().await;
            if self.closed.load(Ordering::Acquire) || state.closed {
                return Err(RuntimeError::Closed);
            }
            if !state.opened {
                return Err(RuntimeError::NotOpen);
            }
            if state.role == role && state.role_epoch == Some(epoch) {
                return Ok(());
            }
            self.revoke_generation();
            state.authority = None;
            state.inbound_build = None;
        }
        self.revoke_inbound_effects().await?;
        let _drain = self.write_gate.lock().await;
        let _effect_drain = self.provider_effect_gate.lock().await;
        self.streams.drain().await;
        let mut state = self.state.lock().await;
        if role == ReplicaRole::None {
            state.role = role;
            state.role_epoch = Some(epoch);
            state.updated_epoch = None;
            return Ok(());
        }
        if let Some(current) = state.role_epoch
            && (epoch.data_loss_number < current.data_loss_number
                || (epoch.data_loss_number == current.data_loss_number
                    && epoch.configuration_number < current.configuration_number))
        {
            return Err(RuntimeError::AuthorityMismatch(
                "role replay regressed the exact epoch".into(),
            ));
        }
        let failover_promotion = state.role == ReplicaRole::ActiveSecondary
            && role == ReplicaRole::Primary
            && state.role_epoch.is_some_and(|current| epoch > current);
        if state.role != ReplicaRole::None
            && !failover_promotion
            && state.updated_epoch != Some(epoch)
            && state.role_epoch != Some(epoch)
        {
            return Err(RuntimeError::ReconfigurationPending);
        }
        state.role = role;
        state.role_epoch = Some(epoch);
        Ok(())
    }

    async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        #[cfg(feature = "testing")]
        notify_inbound_build_boundary_waiting();
        let _admission = self.inbound_build_admission_gate.lock().await;
        {
            let mut state = self.state.lock().await;
            if self.closed.load(Ordering::Acquire) || state.closed {
                return Err(RuntimeError::Closed);
            }
            if let Some(current) = state.role_epoch
                && (epoch.data_loss_number < current.data_loss_number
                    || (epoch.data_loss_number == current.data_loss_number
                        && epoch.configuration_number < current.configuration_number))
            {
                return Err(RuntimeError::AuthorityMismatch(
                    "epoch regressed from the exact role replay".into(),
                ));
            }
            if state.updated_epoch == Some(epoch) {
                return Ok(());
            }
            self.revoke_generation();
            state.authority = None;
            state.inbound_build = None;
        }
        self.revoke_inbound_effects().await?;
        let _drain = self.write_gate.lock().await;
        let _effect_drain = self.provider_effect_gate.lock().await;
        self.streams.drain().await;
        let previous_epoch_last_lsn = self.reconcile_provider_committed_tail().await?;
        self.provider
            .update_epoch(epoch, previous_epoch_last_lsn)
            .await?;
        let mut state = self.state.lock().await;
        if self.closed.load(Ordering::Acquire) || state.closed {
            return Err(RuntimeError::Closed);
        }
        state.updated_epoch = Some(epoch);
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::Release);
        self.context.release_provider_writer();
        #[cfg(feature = "testing")]
        notify_inbound_build_boundary_waiting();
        let _admission = self.inbound_build_admission_gate.lock().await;
        self.revoke_generation();
        {
            let mut state = self.state.lock().await;
            state.closed = true;
            state.role = ReplicaRole::None;
            state.authority = None;
            state.inbound_build = None;
        }
        self.revoke_inbound_effects().await?;
        let _drain = self.write_gate.lock().await;
        let _effect_drain = self.provider_effect_gate.lock().await;
        self.streams.drain().await;
        let owner = self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.abort_listener();
        self.streams.shutdown();
        if let Some(owner) = owner {
            drain_listener(owner).await?;
        }
        Ok(())
    }

    fn abort(&self) {
        self.closed.store(true, Ordering::Release);
        self.context.release_provider_writer();
        self.revoke_generation();
        self.streams.shutdown();
        self.abort_listener();
        if let Ok(mut state) = self.state.try_lock() {
            state.closed = true;
            state.role = ReplicaRole::None;
            state.authority = None;
            state.inbound_build = None;
        }
    }

    async fn current_progress(&self) -> Result<i64> {
        let state = self.state.lock().await;
        if state.closed {
            return Err(RuntimeError::Closed);
        }
        Ok(state.applied_lsn)
    }

    async fn catch_up_capability(&self) -> Result<i64> {
        let state = self.state.lock().await;
        if state.closed {
            return Err(RuntimeError::Closed);
        }
        Ok(if state.opened {
            state.committed_lsn
        } else {
            -1
        })
    }
}

#[async_trait]
impl PrimaryReplicator for StatelessReplicator {
    async fn on_data_loss(&self) -> Result<bool> {
        self.revoke_generation();
        self.fence_authority().await;
        let _drain = self.write_gate.lock().await;
        let changed = self.provider.on_data_loss().await?;
        self.state.lock().await.local_rebuild_required = false;
        self.refresh_provider_progress().await?;
        Ok(changed)
    }

    async fn update_catch_up_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
        previous: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.install_static_configuration(current, Some(previous))
            .await
    }

    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()> {
        let authority = {
            let state = self.state.lock().await;
            state
                .authority
                .clone()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?
        };
        let peer_progress = self.settle_applied_suffix(&authority, mode).await?;
        let state = self.state.lock().await;
        let ready = std::iter::once(&self.identity)
            .chain(
                authority
                    .peers
                    .values()
                    .filter(|peer| {
                        peer_progress
                            .get(&peer.identity)
                            .copied()
                            .unwrap_or(crate::application::INVALID_LSN)
                            >= state.committed_lsn
                    })
                    .map(|peer| &peer.identity),
            )
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let quorum_ready = |configuration: &ConfigurationDescriptor| {
            configuration
                .members
                .iter()
                .filter(|member| ready.contains(&member.identity))
                .count()
                >= configuration.write_quorum as usize
        };
        let all_ready = 1 + authority
            .peers
            .values()
            .filter(|peer| {
                peer_progress
                    .get(&peer.identity)
                    .copied()
                    .unwrap_or(crate::application::INVALID_LSN)
                    >= state.committed_lsn
            })
            .count();
        let successor_ready = authority
            .peers
            .values()
            .find(|peer| peer.must_catchup)
            .is_none_or(|peer| {
                peer_progress
                    .get(&peer.identity)
                    .copied()
                    .unwrap_or(crate::application::INVALID_LSN)
                    >= state.committed_lsn
            });
        let satisfied = match mode {
            ReplicaSetQuorumMode::WriteQuorum => {
                quorum_ready(&authority.current)
                    && authority.previous.as_ref().is_none_or(quorum_ready)
                    && successor_ready
            }
            ReplicaSetQuorumMode::All => {
                all_ready
                    == authority
                        .current
                        .members
                        .iter()
                        .chain(
                            authority
                                .previous
                                .iter()
                                .flat_map(|previous| previous.members.iter()),
                        )
                        .map(|member| &member.identity)
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
            }
        };
        if !satisfied {
            return Err(RuntimeError::ReconfigurationPending);
        }
        Ok(())
    }

    async fn update_current_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.install_static_configuration(current, None).await
    }

    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()> {
        self.send_contained_build(replica).await
    }

    async fn remove_replica(&self, replica_id: ReplicaId) -> Result<()> {
        let mut state = self.state.lock().await;
        let removed = state.authority.as_mut().and_then(|authority| {
            let identity = authority
                .peers
                .keys()
                .find(|identity| identity.replica_id == replica_id)
                .cloned()?;
            authority.peers.remove(&identity);
            Some(identity)
        });
        if let Some(removed) = removed {
            state.retired.insert(removed);
        }
        Ok(())
    }
}

impl StatelessReplicator {
    async fn install_static_configuration(
        &self,
        current: ReplicaSetConfiguration,
        previous: Option<ReplicaSetConfiguration>,
    ) -> Result<()> {
        if current.configuration.configuration_id != current.configuration.expected_id()
            || current.configuration.write_quorum == 0
            || current.configuration.write_quorum as usize > current.configuration.members.len()
            || previous.as_ref().is_some_and(|previous| {
                previous.configuration.configuration_id != previous.configuration.expected_id()
                    || previous.configuration.write_quorum == 0
                    || previous.configuration.write_quorum as usize
                        > previous.configuration.members.len()
                    || previous.configuration.epoch >= current.configuration.epoch
            })
        {
            return Err(RuntimeError::AuthorityMismatch(
                "invalid exact static configuration".into(),
            ));
        }
        let local_role = current
            .configuration
            .members
            .iter()
            .find(|member| member.identity == self.identity)
            .map(|member| member.role)
            .ok_or_else(|| {
                RuntimeError::AuthorityMismatch(
                    "local replica is absent from static configuration".into(),
                )
            })?;
        let mut peers = BTreeMap::new();
        for peer in current.replicas.iter().chain(
            previous
                .iter()
                .flat_map(|previous| previous.replicas.iter()),
        ) {
            peer.validate_public_progress()?;
            if peer.replication_address.is_empty()
                || peer.process_session_id.is_empty()
                || !matches!(
                    peer.role,
                    ReplicaRole::Primary | ReplicaRole::ActiveSecondary
                )
                || !std::iter::once(&current.configuration)
                    .chain(previous.iter().map(|previous| &previous.configuration))
                    .any(|configuration| {
                        configuration.members.iter().any(|member| {
                            member.identity == peer.identity && member.role == peer.role
                        })
                    })
            {
                return Err(RuntimeError::AuthorityMismatch(
                    "static peer lacks exact active session authority".into(),
                ));
            }
            if let Some(existing) = peers.insert(peer.identity.clone(), peer.clone())
                && existing != *peer
            {
                return Err(RuntimeError::AuthorityMismatch(
                    "static configuration changes an exact peer descriptor".into(),
                ));
            }
        }
        let expected_peers = std::iter::once(&current.configuration)
            .chain(previous.iter().map(|previous| &previous.configuration))
            .flat_map(|configuration| configuration.members.iter())
            .filter(|member| member.identity != self.identity)
            .map(|member| member.identity.clone())
            .collect::<std::collections::BTreeSet<_>>();
        if peers
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            != expected_peers
        {
            return Err(RuntimeError::AuthorityMismatch(
                "static configuration lacks exact remote descriptors".into(),
            ));
        }
        for peer in peers.values() {
            if let Some(storage) = &peer.storage_incarnation
                && self.context.replacement_storage_is_retired(storage).await?
            {
                return Err(RuntimeError::AuthorityMismatch(
                    "static configuration readmits a durably retired storage incarnation".into(),
                ));
            }
        }
        let authority = ConfigurationAuthority {
            epoch: current.configuration.epoch,
            current: current.configuration,
            previous: previous.map(|previous| previous.configuration),
            peers,
        };
        #[cfg(feature = "testing")]
        notify_inbound_build_boundary_waiting();
        let _admission = self.inbound_build_admission_gate.lock().await;
        {
            let mut state = self.state.lock().await;
            if state.authority.as_ref() == Some(&authority) {
                return Ok(());
            }
            self.revoke_generation();
            state.authority = None;
            state.inbound_build = None;
        }
        self.revoke_inbound_effects().await?;
        let _drain = self.write_gate.lock().await;
        let _effect_drain = self.provider_effect_gate.lock().await;
        self.streams.drain().await;
        self.reconcile_provider_committed_tail().await?;
        let mut state = self.state.lock().await;
        if state.role != local_role
            || (state.role_epoch != Some(authority.epoch)
                && state.updated_epoch != Some(authority.epoch))
            || state.updated_epoch != Some(authority.epoch)
        {
            return Err(RuntimeError::AuthorityMismatch(
                "configuration differs from static role and epoch replay".into(),
            ));
        }
        state.authority = Some(authority);
        Ok(())
    }
}

#[async_trait]
impl StateReplicator for StatelessReplicator {
    async fn replicate(&self, data: OperationData) -> Result<i64> {
        let _write = self.write_gate.lock().await;
        let authority = self.require_primary().await?;
        let generation = self.generation();
        if !self
            .exact_authority_is_current(&authority, generation, ReplicaRole::Primary)
            .await
        {
            return Err(RuntimeError::OperationCancelled);
        }
        let (lsn, committed_lsn) = {
            let state = self.state.lock().await;
            (
                state
                    .applied_lsn
                    .checked_add(1)
                    .ok_or_else(|| RuntimeError::Application("LSN exhausted".into()))?,
                state.committed_lsn,
            )
        };
        let operation = Operation {
            lsn,
            committed_lsn,
            data,
        };
        let mut applied_guard = AppliedWriteGuard {
            engine: self.self_ref.get().cloned().unwrap_or_default(),
            lsn,
            armed: true,
        };
        let applied = match self.storage.apply(operation.clone()).await {
            Ok(applied) => applied,
            Err(error) => {
                self.contain_ambiguous_suffix(lsn).await;
                return Err(error);
            }
        };
        if applied.applied_lsn < lsn || applied.committed_lsn > applied.applied_lsn {
            return Err(RuntimeError::Application(
                "provider returned inconsistent durable apply progress".into(),
            ));
        }
        if !self
            .exact_authority_is_current(&authority, generation, ReplicaRole::Primary)
            .await
        {
            return Err(self.fence_stale_write(lsn).await);
        }
        {
            let mut state = self.state.lock().await;
            state.applied_lsn = state.applied_lsn.max(applied.applied_lsn);
            state.ambiguous_suffix = state.applied_lsn > state.committed_lsn;
        }
        self.publish_progress(true).await?;
        let mut acknowledgements = std::collections::BTreeSet::from([self.identity.clone()]);
        let quorum = |acknowledgements: &BTreeSet<ReplicaIdentity>,
                      configuration: &ConfigurationDescriptor| {
            configuration
                .members
                .iter()
                .filter(|member| acknowledgements.contains(&member.identity))
                .count()
                >= configuration.write_quorum as usize
        };
        let quorum_satisfied = |acknowledgements: &BTreeSet<ReplicaIdentity>| {
            quorum(acknowledgements, &authority.current)
                && authority
                    .previous
                    .as_ref()
                    .is_none_or(|previous| quorum(acknowledgements, previous))
        };
        let mut pending = FuturesUnordered::new();
        for peer in authority.peers.values() {
            pending.push(async {
                (
                    peer.identity.clone(),
                    self.send_replication_classified(&authority, peer, &operation, committed_lsn)
                        .await,
                )
            });
        }
        while !quorum_satisfied(&acknowledgements) {
            let Some((identity, result)) = pending.next().await else {
                break;
            };
            if result.is_ok() {
                acknowledgements.insert(identity);
            }
            if !self
                .exact_authority_is_current(&authority, generation, ReplicaRole::Primary)
                .await
            {
                return Err(self.fence_stale_write(lsn).await);
            }
        }
        drop(pending);
        if !quorum_satisfied(&acknowledgements) {
            return Err(RuntimeError::OperationCancelled);
        }
        if !self
            .exact_authority_is_current(&authority, generation, ReplicaRole::Primary)
            .await
        {
            return Err(self.fence_stale_write(lsn).await);
        }
        let committed = match self.storage.commit(lsn).await {
            Ok(committed) => committed,
            Err(error) => {
                self.contain_ambiguous_suffix(lsn).await;
                return Err(error);
            }
        };
        if committed.committed_lsn < lsn || committed.committed_lsn > committed.applied_lsn {
            self.contain_ambiguous_suffix(lsn).await;
            return Err(RuntimeError::Application(
                "provider returned inconsistent durable commit progress".into(),
            ));
        }
        if !self
            .exact_authority_is_current(&authority, generation, ReplicaRole::Primary)
            .await
        {
            return Err(self.fence_stale_write(lsn).await);
        }
        let mut state = self.state.lock().await;
        state.applied_lsn = state.applied_lsn.max(committed.applied_lsn);
        state.committed_lsn = state.committed_lsn.max(committed.committed_lsn);
        state.ambiguous_suffix = state.applied_lsn > state.committed_lsn;
        drop(state);
        self.publish_progress(false).await?;
        if !self
            .exact_authority_is_current(&authority, generation, ReplicaRole::Primary)
            .await
        {
            return Err(self.fence_stale_write(lsn).await);
        }
        let engine = self.self_ref.get().and_then(Weak::upgrade).ok_or_else(|| {
            RuntimeError::Application("replacement creation is unattached".into())
        })?;
        for peer in authority.peers.values() {
            let engine = engine.clone();
            let authority = authority.clone();
            let operation = operation.clone();
            let peer = peer.clone();
            self.descendants.spawn(async move {
                let _ = engine
                    .send_replication(&authority, &peer, &operation, lsn)
                    .await;
            });
        }
        if !self
            .exact_authority_is_current(&authority, generation, ReplicaRole::Primary)
            .await
        {
            return Err(self.fence_stale_write(lsn).await);
        }
        applied_guard.disarm();
        Ok(lsn)
    }

    async fn get_replication_stream(&self) -> Result<OperationStream> {
        self.streams.take_replication().await
    }

    async fn get_copy_stream(&self) -> Result<OperationStream> {
        self.streams.take_copy().await
    }

    async fn update_replicator_settings(&self, settings: ReplicatorSettings) -> Result<()> {
        let mut current = self.settings.write().await;
        if self.state.lock().await.opened
            && current.replication_address != settings.replication_address
        {
            return Err(RuntimeError::Application(
                "stateless replacement listener address is immutable after Open".into(),
            ));
        }
        *current = settings;
        Ok(())
    }
}

type ReplicationResponseStream =
    Pin<Box<dyn Stream<Item = std::result::Result<proto::ReplicationAck, Status>> + Send>>;
type CopyResponseStream =
    Pin<Box<dyn Stream<Item = std::result::Result<proto::CopyAck, Status>> + Send>>;

struct ReplacementReplicationService {
    engine: Weak<StatelessReplicator>,
}

#[tonic::async_trait]
impl proto::replication_data_server::ReplicationData for ReplacementReplicationService {
    type ReplicateStream = ReplicationResponseStream;
    type BuildStream = CopyResponseStream;

    async fn replicate(
        &self,
        request: Request<tonic::Streaming<proto::ReplicationItem>>,
    ) -> std::result::Result<Response<Self::ReplicateStream>, Status> {
        let engine = self
            .engine
            .upgrade()
            .ok_or_else(|| Status::unavailable("stateless replacement is closed"))?;
        let mut incoming = request.into_inner();
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        let task_engine = engine.clone();
        engine.inbound_descendants.spawn(async move {
            tokio::select! {
                biased;
                _ = sender.closed() => {},
                _ = async {
            while let Some(item) = incoming.next().await {
                let result = match item {
                    Ok(item) => task_engine.accept_replication(item).await,
                    Err(error) => Err(error),
                };
                if sender.send(result).await.is_err() {
                    break;
                }
            }
                } => {},
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn build(
        &self,
        request: Request<tonic::Streaming<proto::CopyItem>>,
    ) -> std::result::Result<Response<Self::BuildStream>, Status> {
        let engine = self
            .engine
            .upgrade()
            .ok_or_else(|| Status::unavailable("stateless replacement is closed"))?;
        let mut incoming = request.into_inner();
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        let task_engine = engine.clone();
        engine.inbound_descendants.spawn(async move {
            tokio::select! {
                biased;
                _ = sender.closed() => {},
                _ = async {
            while let Some(item) = incoming.next().await {
                let result = match item {
                    Ok(item) => task_engine.accept_copy(item).await,
                    Err(error) => Err(error),
                };
                if sender.send(result).await.is_err() {
                    break;
                }
            }
                } => {},
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

fn bound_replication_endpoint(advertised: &str, port: u16) -> Result<String> {
    let endpoint = tonic::transport::Endpoint::from_shared(advertised.to_owned())
        .map_err(|error| RuntimeError::Application(error.to_string()))?;
    let mut parts = endpoint.uri().clone().into_parts();
    let authority = parts.authority.as_ref().ok_or_else(|| {
        RuntimeError::Application("replication endpoint lacks an authority".into())
    })?;
    if authority.port_u16() != Some(0) {
        return Ok(advertised.to_owned());
    }
    parts.authority = Some(format!("{}:{port}", authority.host()).parse().map_err(
        |error: tonic::codegen::http::uri::InvalidUri| RuntimeError::Application(error.to_string()),
    )?);
    tonic::codegen::http::Uri::from_parts(parts)
        .map(|uri| uri.to_string())
        .map_err(|error| RuntimeError::Application(error.to_string()))
}

fn runtime_status(error: RuntimeError) -> Status {
    match error {
        RuntimeError::Application(detail) => Status::unavailable(detail),
        RuntimeError::OperationCancelled | RuntimeError::WriteCompletionClosed => {
            Status::cancelled(error.to_string())
        }
        _ => Status::failed_precondition(error.to_string()),
    }
}

#[cfg(test)]
mod task_registry_tests {
    use super::{
        LISTENER_SHUTDOWN_TIMEOUT, ListenerOwner, RuntimeError, TaskRegistry,
        drain_listener_with_timeout,
    };
    use std::time::Duration;

    #[test]
    fn dynamic_replication_endpoint_keeps_advertised_host_and_explicit_fixed_port() {
        for advertised in [
            "http://127.0.0.1:0",
            "http://[::1]:0",
            "http://replica.test:0",
        ] {
            let endpoint = super::bound_replication_endpoint(advertised, 41001).unwrap();
            let before: tonic::codegen::http::Uri = advertised.parse().unwrap();
            let after: tonic::codegen::http::Uri = endpoint.parse().unwrap();
            assert_eq!(before.host(), after.host());
            assert_eq!(after.port_u16(), Some(41001));
        }
        assert_eq!(
            super::bound_replication_endpoint("https://advertised.test:443", 41001).unwrap(),
            "https://advertised.test:443",
        );
        assert!(super::bound_replication_endpoint("not an endpoint", 41001).is_err());
    }

    #[tokio::test]
    async fn completed_and_aborted_tasks_leave_no_retained_entries() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let registry = TaskRegistry::new();
            for _ in 0..2 {
                let tasks = (0..1_000)
                    .map(|_| registry.spawn(async {}))
                    .collect::<Vec<_>>();
                for task in tasks {
                    task.await.unwrap();
                }
                assert_eq!(registry.raw_len(), 0);
            }

            let tasks = (0..32)
                .map(|_| registry.spawn(std::future::pending::<()>()))
                .collect::<Vec<_>>();
            assert_eq!(registry.raw_len(), tasks.len());
            registry.abort_all();
            for task in tasks {
                assert!(task.await.unwrap_err().is_cancelled());
            }
            registry.drain().await.unwrap();
            assert_eq!(registry.raw_len(), 0);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn registry_drain_observes_cancelled_work_and_panics() {
        let registry = TaskRegistry::new();
        let task = registry.spawn(std::future::pending::<()>());
        registry.stop();
        registry.drain().await.unwrap();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(registry.raw_len(), 0);
        let late = registry.spawn(std::future::pending::<()>());
        assert!(late.await.unwrap_err().is_cancelled());
        registry.drain().await.unwrap();

        let registry = TaskRegistry::new();
        let task = registry.spawn(async { panic!("descendant failed") });
        assert!(task.await.unwrap_err().is_panic());
        assert!(
            registry
                .drain()
                .await
                .unwrap_err()
                .to_string()
                .contains("panicked")
        );
    }

    #[tokio::test]
    async fn listener_shutdown_uses_the_exact_five_second_deadline() {
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(std::future::pending::<super::Result<()>>());
        assert!(matches!(
            drain_listener_with_timeout(
                ListenerOwner { shutdown, task },
                Duration::from_millis(10)
            )
            .await,
            Err(RuntimeError::OperationCancelled)
        ));
        assert_eq!(LISTENER_SHUTDOWN_TIMEOUT, Duration::from_secs(5));
    }
}
#[cfg(feature = "testing")]
mod testing_transport;
#[cfg(feature = "testing")]
pub use testing_transport::TestingTcpRoute;
