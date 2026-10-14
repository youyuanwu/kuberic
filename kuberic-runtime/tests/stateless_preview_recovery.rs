#![cfg(feature = "testing")]

// Hosted Public V1 preview lifecycle and recovery journeys.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use kuberic_controller::evaluator::test_bridge::{
    HostedPreviewReplica, HostedPublicV1Adapter, PreviewTransition,
    PublicOperationPreviewEvaluationConfig, PublicV1AcceptedTransitionIntent,
    PublicV1AcceptedTransitionStatus, PublicV1EvidenceApi, PublicV1EvidenceConsumer,
    PublicV1EvidenceResource, PublicV1EvidenceState, PublicV1HistoryApi, PublicV1OperationApi,
    PublicV1ResourceAuthority, PublicV1ResourceDisposition, PublicV1ResourceProof,
    PublicV1SessionRenewalApi, PublicV1SessionRenewalPlan, accept_public_v1_session_renewal,
    accept_public_v1_survivor_retired, execute_public_v1_history, execute_public_v1_operation,
    execute_public_v1_session_renewal, plan_public_lifecycle, plan_public_v1_data_loss_recovery,
    plan_public_v1_recovery_report, plan_public_v1_session_renewal, reconcile_public_v1_consumer,
    reconcile_public_v1_operation,
};
use kuberic_runtime::application::{
    CopyChunk, DurableApplicationProgress, OpenContext, Operation, OperationDataStream, RoleChange,
};
use kuberic_runtime::control::proto;
use kuberic_runtime::engine::{DurableState, RetainedOperationStream};
use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaHost, ReplicaProcessConfig, RunningReplica,
};
use kuberic_runtime::protocol::command::{ProtocolCommand, PublicV1OperationCommand};
use kuberic_runtime::protocol::public_operations::{
    DataLossAttempt, DataLossAuthorization, DataLossReplay, LocalProviderRebuildRequired,
    PossibleDataLossIntent, PreviewLifecycleBinding, ProcessAttemptContext,
    ProcessSessionRenewalRequired, ProviderProgressReset, ProviderReplicaIdentity,
    PublicBuildAttemptReference, PublicBuildInput, PublicConfiguration,
    PublicDataLossRecoveryInput, PublicInboundBuildAuthority, PublicLifecycleInput,
    PublicLifecycleRecipe, PublicOperationClass, PublicOperationIntent,
    PublicOperationPreviewIdentity, PublicOperationProgram, PublicOperationReplayDisposition,
    PublicReplicaDescriptor, PublicV1AcceptedStatus, PublicV1CallKind, PublicV1OperationCompletion,
    PublicV1OperationResult, ReplicaRebuildReason, ReplicaRebuildRequired, StatePersistence,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, AgentGeneration, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy,
    Epoch, InitializationId, OperationId, PodUid, ProcessSessionEndpoint, ProcessSessionId,
    ProcessSessionRenewalReason, PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity, ReplicaStorageIncarnationContext,
    ReplicatorCreationId, ResourceUid, StorageGeneration, derive_agent_generation,
    derive_bootstrap_history, derive_data_loss_history, derive_history_initialization_id,
    derive_initialization_id,
};
use kuberic_runtime::replicator::stateless::{
    StatelessReplicatorFactory, testing_pause_inbound_build_after_classification,
};
use kuberic_runtime::replicator::{
    DefaultReplicatorFactory, Replicator, ReplicatorFactory, ReplicatorFactoryContext,
    ReplicatorInterfaces, ReplicatorSettings, StateReplicator, StatefulServicePartition,
};
use kuberic_runtime::testing::sqlite_store::SqliteStore;
use kuberic_runtime::testing::state::{AgentState, SCHEMA_VERSION, StorageIdentity};
use kuberic_runtime::{Result, RuntimeError, StateProvider, StatefulServiceReplica};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{Notify, RwLock};
use tonic::Request;

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

fn test_root(name: &str) -> PathBuf {
    let root = std::env::current_dir()
        .unwrap()
        .join("target")
        .join("stateless-preview")
        .join(format!(
            "{name}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn free_address() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address
}

fn authorized<T>(message: T) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", "Bearer preview-token".parse().unwrap());
    request
}

type HungReplicationStream =
    Pin<Box<dyn Stream<Item = std::result::Result<proto::ReplicationAck, tonic::Status>> + Send>>;
type HungBuildStream =
    Pin<Box<dyn Stream<Item = std::result::Result<proto::CopyAck, tonic::Status>> + Send>>;

struct HungBuildService {
    entered: Arc<Notify>,
}

#[tonic::async_trait]
impl proto::replication_data_server::ReplicationData for HungBuildService {
    type ReplicateStream = HungReplicationStream;
    type BuildStream = HungBuildStream;

    async fn replicate(
        &self,
        _request: Request<tonic::Streaming<proto::ReplicationItem>>,
    ) -> std::result::Result<tonic::Response<Self::ReplicateStream>, tonic::Status> {
        Ok(tonic::Response::new(Box::pin(stream::empty())))
    }

    async fn build(
        &self,
        _request: Request<tonic::Streaming<proto::CopyItem>>,
    ) -> std::result::Result<tonic::Response<Self::BuildStream>, tonic::Status> {
        self.entered.notify_one();
        Ok(tonic::Response::new(Box::pin(stream::pending())))
    }
}

async fn drive_session_renewal(
    adapter: &HostedPublicV1Adapter,
    unreachable: &[ProcessSessionId],
    retire_unreachable: bool,
) {
    loop {
        let accepted = adapter
            .observe_public_v1_session_renewal()
            .await
            .unwrap()
            .unwrap();
        let plan = plan_public_v1_session_renewal(&accepted, unreachable).unwrap();
        match plan {
            PublicV1SessionRenewalPlan::Stable => break,
            PublicV1SessionRenewalPlan::WaitForSurvivor { survivor, .. } if retire_unreachable => {
                let next = accept_public_v1_survivor_retired(
                    &accepted,
                    &survivor,
                    OperationId::new(format!(
                        "{}-retire-unreachable-{}",
                        accepted.required.operation_id, survivor.identity.replica_id
                    )),
                )
                .unwrap();
                adapter
                    .persist_public_v1_session_renewal(Some(&accepted), &next)
                    .await
                    .unwrap();
            }
            plan => execute_public_v1_session_renewal(adapter, plan)
                .await
                .unwrap(),
        }
    }
}

fn replica() -> ReplicaIdentity {
    replica_named(1, "preview-pod-uid", "preview-agent")
}

fn replica_named(id: i64, instance: &str, generation: &str) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(instance),
        agent_generation: AgentGeneration::new(generation),
    }
}

fn storage_proto(storage: &ReplicaStorageIncarnationContext) -> proto::PublicV1StorageIncarnation {
    let (kind, pod_uid, pvc_uid, volatile_identity) = match &storage.storage {
        ReplicaStorageIdentity::Persistent { pod_uid, pvc_uid } => (
            proto::PublicV1StorageKind::Persistent as i32,
            pod_uid.to_string(),
            pvc_uid.to_string(),
            String::new(),
        ),
        ReplicaStorageIdentity::Volatile { identity } => (
            proto::PublicV1StorageKind::Volatile as i32,
            String::new(),
            String::new(),
            identity.clone(),
        ),
    };
    proto::PublicV1StorageIncarnation {
        replica: Some(storage.replica.clone().into()),
        kind,
        pod_uid,
        pvc_uid,
        volatile_identity,
        storage_generation: storage.storage_generation.to_string(),
    }
}

fn history_proto(
    history: &kuberic_runtime::protocol::types::HistoryContext,
) -> proto::PublicV1HistoryContext {
    proto::PublicV1HistoryContext {
        resource_uid: history.resource_uid.to_string(),
        protocol_generation: history.protocol_generation.value(),
        history_id: history.history_id.to_string(),
        data_loss_number: history.data_loss_number,
    }
}

fn storage_incarnation(replica: ReplicaIdentity) -> ReplicaStorageIncarnationContext {
    storage_named(replica, "preview-pod-uid", "preview-pvc-uid")
}

fn storage_named(
    replica: ReplicaIdentity,
    pod_uid: &str,
    pvc_uid: &str,
) -> ReplicaStorageIncarnationContext {
    ReplicaStorageIncarnationContext {
        replica,
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new(pod_uid),
            pvc_uid: PvcUid::new(pvc_uid),
        },
        storage_generation: StorageGeneration::new(format!("{pod_uid}-storage")),
    }
}

fn store_identity(resource_uid: ResourceUid) -> StorageIdentity {
    store_identity_named(
        resource_uid,
        replica(),
        "preview-pod-uid",
        "preview-pvc-uid",
    )
}

fn store_identity_named(
    resource_uid: ResourceUid,
    replica: ReplicaIdentity,
    pod_uid: &str,
    pvc_uid: &str,
) -> StorageIdentity {
    StorageIdentity {
        schema_version: SCHEMA_VERSION,
        resource_uid,
        pod_uid: PodUid::new(pod_uid),
        pvc_uid: PvcUid::new(pvc_uid),
        initialization_id: InitializationId::new(format!("{pod_uid}-initialization")),
        local_identity: replica,
        effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
    }
}

#[derive(Default)]
struct MemoryState {
    progress: Mutex<DurableApplicationProgress>,
    operations: Mutex<Vec<Operation>>,
    copy_chunks: Mutex<Vec<Bytes>>,
    epoch_tails: Mutex<Vec<i64>>,
    block_apply: AtomicBool,
    block_commit: AtomicBool,
    fail_commit: AtomicBool,
    block_copy: AtomicBool,
    block_finish_copy: AtomicBool,
    spawn_copy_descendant: AtomicBool,
    data_loss_changed: AtomicBool,
    fail_data_loss: AtomicBool,
    data_loss_rebuild: Mutex<Option<ReplicaIdentity>>,
    data_loss_calls: AtomicU64,
    commit_entered: Notify,
    release_commit: Notify,
    apply_entered: Notify,
    release_apply: Notify,
    copy_entered: Notify,
    release_copy: Notify,
    finish_copy_entered: Notify,
    release_finish_copy: Notify,
    copy_descendant_entered: Arc<Notify>,
    release_copy_descendant: Arc<Notify>,
    cancelled_copy_descendants: Arc<AtomicU64>,
    copy_descendant_mutations: Arc<AtomicU64>,
}

impl MemoryState {
    fn block_apply(&self) {
        self.block_apply.store(true, Ordering::Release);
    }

    fn block_commit(&self) {
        self.block_commit.store(true, Ordering::Release);
    }

    fn fail_next_commit(&self) {
        self.fail_commit.store(true, Ordering::Release);
    }

    fn block_copy(&self) {
        self.block_copy.store(true, Ordering::Release);
    }

    fn block_finish_copy(&self) {
        self.block_finish_copy.store(true, Ordering::Release);
    }

    fn block_copy_with_descendant(&self) {
        self.block_copy();
        self.spawn_copy_descendant.store(true, Ordering::Release);
    }

    fn change_on_data_loss(&self) {
        self.data_loss_changed.store(true, Ordering::Release);
    }

    fn fail_next_data_loss(&self) {
        self.fail_data_loss.store(true, Ordering::Release);
    }

    fn rebuild_on_data_loss(&self, replica: ReplicaIdentity) {
        *self.data_loss_rebuild.lock().unwrap() = Some(replica);
    }
}

#[async_trait]
impl StateProvider for MemoryState {
    async fn update_epoch(&self, _epoch: Epoch, previous_epoch_last_lsn: i64) -> Result<()> {
        if previous_epoch_last_lsn < 0 {
            return Err(RuntimeError::Application(
                "negative previous epoch LSN".into(),
            ));
        }
        self.epoch_tails
            .lock()
            .unwrap()
            .push(previous_epoch_last_lsn);
        Ok(())
    }

    async fn last_committed_lsn(&self) -> Result<i64> {
        Ok(self.progress.lock().unwrap().committed_lsn)
    }

    async fn get_copy_context(&self) -> Result<OperationDataStream> {
        Ok(Box::pin(stream::empty()))
    }

    async fn get_copy_state(
        &self,
        _up_to_lsn: i64,
        _copy_context: OperationDataStream,
    ) -> Result<OperationDataStream> {
        if self.block_copy.load(Ordering::Acquire) {
            struct AbortDescendant(tokio::task::JoinHandle<()>);

            impl Drop for AbortDescendant {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }

            let descendant = self
                .spawn_copy_descendant
                .swap(false, Ordering::AcqRel)
                .then(|| {
                    let entered = self.copy_descendant_entered.clone();
                    let release = self.release_copy_descendant.clone();
                    let cancelled = self.cancelled_copy_descendants.clone();
                    let mutations = self.copy_descendant_mutations.clone();
                    AbortDescendant(tokio::spawn(async move {
                        struct CancellationProbe {
                            cancelled: Arc<AtomicU64>,
                            completed: bool,
                        }

                        impl Drop for CancellationProbe {
                            fn drop(&mut self) {
                                if !self.completed {
                                    self.cancelled.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                        }

                        let mut probe = CancellationProbe {
                            cancelled,
                            completed: false,
                        };
                        entered.notify_one();
                        release.notified().await;
                        mutations.fetch_add(1, Ordering::SeqCst);
                        probe.completed = true;
                    }))
                });
            self.copy_entered.notify_waiters();
            self.release_copy.notified().await;
            self.block_copy.store(false, Ordering::Release);
            drop(descendant);
        }
        Ok(Box::pin(stream::iter(
            self.copy_chunks.lock().unwrap().clone().into_iter().map(Ok),
        )))
    }

    async fn on_data_loss(&self) -> Result<bool> {
        self.data_loss_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(replica) = self.data_loss_rebuild.lock().unwrap().take() {
            return Err(RuntimeError::ReplicaRebuildRequired(Box::new(
                ReplicaRebuildRequired::LocalProvider(LocalProviderRebuildRequired {
                    reason: ReplicaRebuildReason::IncompatibleRetainedHistory,
                    provider: ProviderReplicaIdentity { replica },
                }),
            )));
        }
        if self.fail_data_loss.swap(false, Ordering::AcqRel) {
            return Err(RuntimeError::Application(
                "injected data-loss callback failure".into(),
            ));
        }
        Ok(self.data_loss_changed.swap(false, Ordering::AcqRel))
    }
}

#[async_trait]
impl DurableState for MemoryState {
    async fn get_replication_operations(
        &self,
        from_lsn: i64,
        to_lsn: i64,
    ) -> Result<RetainedOperationStream> {
        let operations = self
            .operations
            .lock()
            .unwrap()
            .iter()
            .filter(|operation| operation.lsn >= from_lsn && operation.lsn <= to_lsn)
            .cloned()
            .map(Ok)
            .collect::<Vec<_>>();
        Ok(Box::pin(stream::iter(operations)))
    }

    async fn apply_copy_chunk(
        &self,
        _build_id: &OperationId,
        sequence: u64,
        chunk: CopyChunk,
    ) -> Result<()> {
        let mut chunks = self.copy_chunks.lock().unwrap();
        let index = sequence as usize;
        if let Some(existing) = chunks.get(index) {
            if existing != &chunk.data {
                return Err(RuntimeError::Application("copy chunk changed".into()));
            }
        } else if index == chunks.len() {
            chunks.push(chunk.data);
        } else {
            return Err(RuntimeError::Application("copy sequence has a gap".into()));
        }
        Ok(())
    }

    async fn verify_copy_chunk(
        &self,
        _build_id: &OperationId,
        sequence: u64,
        chunk: &CopyChunk,
    ) -> Result<bool> {
        Ok(self.copy_chunks.lock().unwrap().get(sequence as usize) == Some(&chunk.data))
    }

    async fn finish_copy(
        &self,
        _build_id: &OperationId,
        up_to_lsn: i64,
        committed_lsn: i64,
    ) -> Result<DurableApplicationProgress> {
        if self.block_finish_copy.load(Ordering::Acquire) {
            self.finish_copy_entered.notify_one();
            self.release_finish_copy.notified().await;
            self.block_finish_copy.store(false, Ordering::Release);
        }
        let mut progress = self.progress.lock().unwrap();
        progress.applied_lsn = up_to_lsn;
        progress.committed_lsn = committed_lsn;
        Ok(*progress)
    }

    async fn apply(&self, operation: Operation) -> Result<DurableApplicationProgress> {
        let result = {
            let mut operations = self.operations.lock().unwrap();
            if let Some(existing) = operations
                .iter()
                .find(|existing| existing.lsn == operation.lsn)
            {
                if existing != &operation {
                    return Err(RuntimeError::Application("operation bytes changed".into()));
                }
            } else {
                operations.push(operation.clone());
            }
            let mut progress = self.progress.lock().unwrap();
            progress.applied_lsn = progress.applied_lsn.max(operation.lsn);
            *progress
        };
        if self.block_apply.load(Ordering::Acquire) {
            self.apply_entered.notify_one();
            self.release_apply.notified().await;
            self.block_apply.store(false, Ordering::Release);
        }
        Ok(result)
    }

    async fn durable_progress(&self) -> Result<DurableApplicationProgress> {
        Ok(*self.progress.lock().unwrap())
    }

    async fn verify_applied(&self, operation: &Operation) -> Result<bool> {
        Ok(self.operations.lock().unwrap().contains(operation))
    }

    async fn commit(&self, committed_lsn: i64) -> Result<DurableApplicationProgress> {
        if self.block_commit.load(Ordering::Acquire) {
            self.commit_entered.notify_one();
            self.release_commit.notified().await;
            self.block_commit.store(false, Ordering::Release);
        }
        if self.fail_commit.swap(false, Ordering::AcqRel) {
            return Err(RuntimeError::Application(
                "injected provider commit failure".into(),
            ));
        }
        let mut progress = self.progress.lock().unwrap();
        if committed_lsn > progress.applied_lsn {
            return Err(RuntimeError::Application("commit exceeds apply".into()));
        }
        progress.committed_lsn = progress.committed_lsn.max(committed_lsn);
        Ok(*progress)
    }
}

struct CountingStatelessFactory {
    storage: Arc<MemoryState>,
    calls: Arc<AtomicU64>,
}

#[async_trait]
impl ReplicatorFactory for CountingStatelessFactory {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        state_provider: Option<Arc<dyn StateProvider>>,
        settings: ReplicatorSettings,
    ) -> Result<ReplicatorInterfaces> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        StatelessReplicatorFactory::new(self.storage.clone())
            .create_replicator(context, state_provider, settings)
            .await
    }
}

struct RejectingFactory {
    calls: Arc<AtomicU64>,
}

#[async_trait]
impl ReplicatorFactory for RejectingFactory {
    async fn create_replicator(
        &self,
        _context: ReplicatorFactoryContext,
        _state_provider: Option<Arc<dyn StateProvider>>,
        _settings: ReplicatorSettings,
    ) -> Result<ReplicatorInterfaces> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(RuntimeError::Application(
            "injected creation failure".into(),
        ))
    }
}

struct PreviewApplication {
    storage: Arc<MemoryState>,
    replication_address: String,
    control: RwLock<Option<Arc<dyn Replicator>>>,
    state_replicator: RwLock<Option<Arc<dyn StateReplicator>>>,
    #[allow(clippy::disallowed_types)]
    primary: RwLock<Option<Arc<dyn kuberic_runtime::replicator::PrimaryReplicator>>>,
    partition: RwLock<Option<StatefulServicePartition>>,
    legacy_factory: bool,
    fail_first_creation: AtomicBool,
    opens: AtomicU64,
    factory_calls: Arc<AtomicU64>,
    failed_factory_calls: Arc<AtomicU64>,
    metadata_database: Option<PathBuf>,
    instruction_seen_before_open: AtomicBool,
    fail_close: AtomicBool,
    abort_calls: AtomicU64,
    replicator_released_before_application_close: AtomicBool,
    lifecycle_events: Mutex<Vec<&'static str>>,
}

impl PreviewApplication {
    fn new(storage: Arc<MemoryState>, replication_address: String) -> Self {
        Self {
            storage,
            replication_address,
            control: RwLock::new(None),
            state_replicator: RwLock::new(None),
            primary: RwLock::new(None),
            partition: RwLock::new(None),
            legacy_factory: false,
            fail_first_creation: AtomicBool::new(false),
            opens: AtomicU64::new(0),
            factory_calls: Arc::new(AtomicU64::new(0)),
            failed_factory_calls: Arc::new(AtomicU64::new(0)),
            metadata_database: None,
            instruction_seen_before_open: AtomicBool::new(false),
            fail_close: AtomicBool::new(false),
            abort_calls: AtomicU64::new(0),
            replicator_released_before_application_close: AtomicBool::new(false),
            lifecycle_events: Mutex::new(Vec::new()),
        }
    }

    fn legacy(storage: Arc<MemoryState>, replication_address: String) -> Self {
        Self {
            legacy_factory: true,
            ..Self::new(storage, replication_address)
        }
    }

    fn with_open_probe(mut self, metadata_database: PathBuf) -> Self {
        self.metadata_database = Some(metadata_database);
        self
    }

    fn with_failed_creation(mut self) -> Self {
        self.fail_first_creation = AtomicBool::new(true);
        self
    }

    fn fail_next_close(&self) {
        self.fail_close.store(true, Ordering::Release);
    }

    async fn duplicate_creation(&self) -> Result<()> {
        let partition = self
            .partition
            .read()
            .await
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        partition
            .with_factory(Arc::new(CountingStatelessFactory {
                storage: self.storage.clone(),
                calls: self.factory_calls.clone(),
            }))
            .create_replicator(
                Some(self.storage.clone()),
                Some(ReplicatorSettings {
                    replication_address: self.replication_address.clone(),
                    public_v1: true,
                }),
            )
            .await
            .map(|_| ())
    }

    async fn replicate(&self, data: Bytes) -> Result<i64> {
        self.state_replicator
            .read()
            .await
            .as_ref()
            .ok_or(RuntimeError::NotOpen)?
            .replicate(data)
            .await
    }

    async fn on_data_loss(&self) -> Result<bool> {
        self.primary
            .read()
            .await
            .as_ref()
            .ok_or(RuntimeError::NotPrimary)?
            .on_data_loss()
            .await
    }

    async fn wait_for_write_quorum(&self) -> Result<()> {
        self.primary
            .read()
            .await
            .as_ref()
            .ok_or(RuntimeError::NotPrimary)?
            .wait_for_catch_up_quorum(
                kuberic_runtime::replicator::ReplicaSetQuorumMode::WriteQuorum,
            )
            .await
    }
}

#[async_trait]
impl StatefulServiceReplica for PreviewApplication {
    async fn open(self: Arc<Self>, context: OpenContext) -> Result<Arc<dyn Replicator>> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        if let Some(database) = &self.metadata_database {
            let connection = rusqlite::Connection::open(database).unwrap();
            let json: String = connection
                .query_row(
                    "SELECT state_json FROM agent_state WHERE singleton = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            self.instruction_seen_before_open.store(
                json.contains("ApplicationOpen") && json.contains("\"open-once\""),
                Ordering::Release,
            );
        }
        if let Some(control) = self.control.read().await.clone() {
            return Ok(control);
        }
        *self.partition.write().await = Some(context.partition.clone());
        if self.fail_first_creation.swap(false, Ordering::AcqRel) {
            let failed = context
                .partition
                .with_factory(Arc::new(RejectingFactory {
                    calls: self.failed_factory_calls.clone(),
                }))
                .create_replicator(
                    Some(self.storage.clone()),
                    Some(ReplicatorSettings {
                        replication_address: self.replication_address.clone(),
                        public_v1: true,
                    }),
                )
                .await;
            if failed.is_ok() {
                return Err(RuntimeError::Application(
                    "injected creation unexpectedly succeeded".into(),
                ));
            }
        }
        let factory: Arc<dyn ReplicatorFactory> = if self.legacy_factory {
            Arc::new(DefaultReplicatorFactory::new(self.storage.clone()))
        } else {
            Arc::new(CountingStatelessFactory {
                storage: self.storage.clone(),
                calls: self.factory_calls.clone(),
            })
        };
        let interfaces = context
            .partition
            .with_factory(factory)
            .create_replicator(
                Some(self.storage.clone()),
                Some(ReplicatorSettings {
                    replication_address: self.replication_address.clone(),
                    public_v1: !self.legacy_factory,
                }),
            )
            .await?;
        let control = interfaces.replicator();
        *self.primary.write().await = interfaces.primary_replicator();
        *self.state_replicator.write().await = interfaces.state_replicator();
        *self.control.write().await = Some(control.clone());
        Ok(control)
    }

    async fn change_role(&self, role: ReplicaRole) -> Result<RoleChange> {
        Ok(RoleChange {
            service_address: (role == ReplicaRole::Primary)
                .then(|| "http://127.0.0.1:8080".to_string()),
        })
    }

    async fn close(&self) -> Result<()> {
        self.lifecycle_events
            .lock()
            .unwrap()
            .push("application.close.begin");
        let address: std::net::SocketAddr = self
            .replication_address
            .strip_prefix("http://")
            .expect("preview replication address")
            .parse()
            .expect("preview socket address");
        if std::net::TcpListener::bind(address).is_ok() {
            self.replicator_released_before_application_close
                .store(true, Ordering::Release);
        }
        self.state_replicator.write().await.take();
        self.primary.write().await.take();
        self.control.write().await.take();
        self.lifecycle_events
            .lock()
            .unwrap()
            .push("application.close.end");
        if self.fail_close.swap(false, Ordering::AcqRel) {
            return Err(RuntimeError::Application(
                "injected application close failure".into(),
            ));
        }
        Ok(())
    }

    fn abort(&self) {
        self.abort_calls.fetch_add(1, Ordering::SeqCst);
        self.lifecycle_events
            .lock()
            .unwrap()
            .push("application.abort");
    }
}

struct Resolver;

impl kuberic_runtime::host::ReplicaEndpointResolver for Resolver {
    fn control_endpoint(&self, _identity: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }

    fn replication_endpoint(&self, _identity: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
}

fn intent(
    preview: &PublicOperationPreviewIdentity,
    session: kuberic_runtime::protocol::types::ProcessSessionId,
    revision: u64,
    name: &str,
    program: PublicOperationProgram,
) -> PublicOperationIntent {
    let class = match &program {
        PublicOperationProgram::Swap { .. } => PublicOperationClass::PlannedSwap,
        PublicOperationProgram::Build(build) => PublicOperationClass::Build {
            target: build.replica.replica_id,
        },
        PublicOperationProgram::Remove(build) => PublicOperationClass::Remove {
            target: build.replica.replica_id,
        },
        PublicOperationProgram::Close => PublicOperationClass::Close,
        PublicOperationProgram::Abort => PublicOperationClass::Abort,
        _ => PublicOperationClass::Authority,
    };
    PublicOperationIntent {
        preview: preview.clone(),
        operation_id: OperationId::new(name),
        revision,
        process_session_id: session,
        class,
        input_digest: program.digest(),
        replay_disposition: program.replay_disposition(),
        lifecycle: None,
        program: Some(program),
    }
}

async fn install_static_authority(
    handle: &kuberic_runtime::host::ReplicaHandle,
    preview: &PublicOperationPreviewIdentity,
    local: ReplicaIdentity,
    role: ReplicaRole,
    epoch: Epoch,
    configuration: PublicConfiguration,
) {
    let session = handle.process_session_id();
    handle
        .execute_stateless_preview(intent(
            preview,
            session.clone(),
            1,
            &format!("open-{}", local.replica_id.value()),
            PublicOperationProgram::Open {
                replica: local,
                existing: true,
            },
        ))
        .await
        .unwrap();
    handle
        .execute_stateless_preview(intent(
            preview,
            session.clone(),
            2,
            &format!("role-{}", role as u8),
            PublicOperationProgram::Role { epoch, role },
        ))
        .await
        .unwrap();
    handle
        .execute_stateless_preview(intent(
            preview,
            session.clone(),
            3,
            "epoch-static",
            PublicOperationProgram::Epoch { epoch },
        ))
        .await
        .unwrap();
    handle
        .execute_stateless_preview(intent(
            preview,
            session,
            4,
            "configuration-static",
            PublicOperationProgram::Configuration(configuration),
        ))
        .await
        .unwrap();
}

struct HostedMemoryReplica {
    identity: ReplicaIdentity,
    storage: ReplicaStorageIncarnationContext,
    root: PathBuf,
    address: std::net::SocketAddr,
    state: Arc<MemoryState>,
    application: Arc<PreviewApplication>,
    running: RunningReplica,
}

impl HostedMemoryReplica {
    async fn start(
        name: &str,
        resource_uid: &ResourceUid,
        preview: &PublicOperationPreviewIdentity,
        replica_id: i64,
    ) -> Self {
        let pod_uid = format!("{name}-pod");
        let pvc_uid = format!("{name}-pvc");
        let identity = replica_named(replica_id, &pod_uid, &format!("{name}-agent"));
        let storage = storage_named(identity.clone(), &pod_uid, &pvc_uid);
        let root = test_root(name);
        drop(
            SqliteStore::create_public_v1(
                SqliteStore::metadata_database_path(&root),
                preview.clone(),
                store_identity_named(resource_uid.clone(), identity.clone(), &pod_uid, &pvc_uid),
            )
            .unwrap(),
        );
        let address = free_address();
        let state = Arc::new(MemoryState::default());
        let application = Arc::new(PreviewApplication::new(
            state.clone(),
            format!("http://{address}"),
        ));
        let running = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(replica_id),
                pod_uid: PodUid::new(&pod_uid),
                pvc_uid: PvcUid::new(&pvc_uid),
                data_root: root.clone(),
                control_address: free_address(),
                replication_address: address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            application.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_stateless_preview(PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        })
        .start()
        .await
        .unwrap();
        Self {
            identity,
            storage,
            root,
            address,
            state,
            application,
            running,
        }
    }

    fn handle(&self) -> kuberic_runtime::host::ReplicaHandle {
        self.running.handle()
    }

    async fn initialize_history(
        &self,
        initialization: &kuberic_runtime::protocol::types::HistoryInitializationId,
        history: &kuberic_runtime::protocol::types::HistoryContext,
    ) {
        self.handle()
            .initialize_stateless_preview_history(initialization, history, &self.storage)
            .await
            .unwrap();
    }

    async fn stop(&mut self) {
        self.running.shutdown();
        self.running.wait().await.unwrap();
    }

    fn cleanup(self) {
        std::fs::remove_dir_all(self.root).unwrap();
    }
}

struct AuthorizedHostedPair {
    preview: PublicOperationPreviewIdentity,
    epoch: Epoch,
    primary: HostedMemoryReplica,
    secondary: HostedMemoryReplica,
}

impl AuthorizedHostedPair {
    async fn start(name: &str, generation: u64) -> Self {
        let resource_uid = ResourceUid::new(format!("{name}-resource"));
        let preview = PublicOperationPreviewIdentity::public_v1(generation);
        let primary =
            HostedMemoryReplica::start(&format!("{name}-primary"), &resource_uid, &preview, 1)
                .await;
        let secondary =
            HostedMemoryReplica::start(&format!("{name}-secondary"), &resource_uid, &preview, 2)
                .await;
        let protocol_generation = PublicProtocolGeneration::new(generation);
        let initialization = derive_history_initialization_id(&resource_uid, protocol_generation);
        let history = derive_bootstrap_history(&resource_uid, protocol_generation, &initialization);
        primary.initialize_history(&initialization, &history).await;
        secondary
            .initialize_history(&initialization, &history)
            .await;
        let epoch = Epoch::new(0, 1);
        let configuration = ConfigurationDescriptor::new(
            epoch,
            primary.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: primary.identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: secondary.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        install_static_authority(
            &primary.handle(),
            &preview,
            primary.identity.clone(),
            ReplicaRole::Primary,
            epoch,
            PublicConfiguration {
                current: configuration.clone(),
                previous: None,
                replicas: vec![PublicReplicaDescriptor {
                    build_id: OperationId::default(),
                    identity: secondary.identity.clone(),
                    replication_address: format!("http://{}", secondary.address),
                    process_session_id: secondary.handle().process_session_id(),
                    storage_incarnation: Some(secondary.storage.clone()),
                    role: ReplicaRole::ActiveSecondary,
                    current_progress: 0,
                    catch_up_capability: 0,
                    must_catchup: false,
                }],
            },
        )
        .await;
        install_static_authority(
            &secondary.handle(),
            &preview,
            secondary.identity.clone(),
            ReplicaRole::ActiveSecondary,
            epoch,
            PublicConfiguration {
                current: configuration,
                previous: None,
                replicas: vec![PublicReplicaDescriptor {
                    build_id: OperationId::default(),
                    identity: primary.identity.clone(),
                    replication_address: format!("http://{}", primary.address),
                    process_session_id: primary.handle().process_session_id(),
                    storage_incarnation: Some(primary.storage.clone()),
                    role: ReplicaRole::Primary,
                    current_progress: 0,
                    catch_up_capability: 0,
                    must_catchup: false,
                }],
            },
        )
        .await;
        Self {
            preview,
            epoch,
            primary,
            secondary,
        }
    }

    async fn cleanup(mut self) {
        self.primary.stop().await;
        self.secondary.stop().await;
        self.primary.cleanup();
        self.secondary.cleanup();
    }
}

#[tokio::test]
async fn empty_history_catch_up_requires_exact_live_peer_proof() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut pair = AuthorizedHostedPair::start("zero-progress-proof", 752).await;
        pair.primary
            .application
            .wait_for_write_quorum()
            .await
            .unwrap();
        assert_eq!(
            *pair.primary.state.progress.lock().unwrap(),
            DurableApplicationProgress::default()
        );
        pair.secondary.stop().await;
        assert!(matches!(
            pair.primary.application.wait_for_write_quorum().await,
            Err(RuntimeError::ReconfigurationPending)
        ));
        pair.primary.stop().await;
        pair.primary.cleanup();
        pair.secondary.cleanup();
    })
    .await
    .unwrap();
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
enum StoredCloseDiagnostic {
    CloseFailure {
        child: StoredCloseChild,
        error: String,
    },
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
enum StoredCloseChild {
    Replicator,
    Application,
}

#[tokio::test]
async fn preview_open_owns_registration_listener_and_writer_exactly_once() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = test_root("open-once");
        let resource_uid = ResourceUid::new("preview-open-once");
        let preview = PublicOperationPreviewIdentity::public_v1(7);
        let binding = PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let database = SqliteStore::metadata_database_path(&root);
        drop(
            SqliteStore::create_public_v1(
                &database,
                preview.clone(),
                store_identity(resource_uid.clone()),
            )
            .unwrap(),
        );
        let replication_address = free_address();
        let storage = Arc::new(MemoryState::default());
        let application = Arc::new(
            PreviewApplication::new(storage.clone(), format!("http://{replication_address}"))
                .with_open_probe(database)
                .with_failed_creation(),
        );
        let mut running = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("preview-pod-uid"),
                pvc_uid: PvcUid::new("preview-pvc-uid"),
                data_root: root.clone(),
                control_address: free_address(),
                replication_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            application.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding)
        .start()
        .await
        .unwrap();
        let handle = running.handle();
        assert_eq!(application.opens.load(Ordering::SeqCst), 0);
        assert_eq!(application.factory_calls.load(Ordering::SeqCst), 0);
        assert_eq!(application.failed_factory_calls.load(Ordering::SeqCst), 0);
        assert_eq!(handle.stateless_preview_provider_writer_count(), 0);
        drop(std::net::TcpListener::bind(replication_address).unwrap());

        let generation = PublicProtocolGeneration::new(7);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        handle
            .initialize_stateless_preview_history(
                &initialization,
                &history,
                &storage_incarnation(replica()),
            )
            .await
            .unwrap();
        let open = intent(
            &preview,
            handle.process_session_id(),
            1,
            "open-once",
            PublicOperationProgram::Open {
                replica: replica(),
                existing: true,
            },
        );
        handle
            .execute_stateless_preview(open.clone())
            .await
            .unwrap();
        assert_eq!(application.opens.load(Ordering::SeqCst), 1);
        assert_eq!(application.failed_factory_calls.load(Ordering::SeqCst), 1);
        assert_eq!(application.factory_calls.load(Ordering::SeqCst), 1);
        assert!(
            application
                .instruction_seen_before_open
                .load(Ordering::Acquire),
            "ApplicationOpen instruction was not durable before its effect"
        );
        assert_eq!(handle.stateless_preview_provider_writer_count(), 1);
        assert!(std::net::TcpListener::bind(replication_address).is_err());

        assert!(application.duplicate_creation().await.is_err());
        assert_eq!(application.factory_calls.load(Ordering::SeqCst), 1);
        assert_eq!(handle.stateless_preview_provider_writer_count(), 1);

        handle.execute_stateless_preview(open).await.unwrap();
        assert_eq!(application.opens.load(Ordering::SeqCst), 1);
        assert_eq!(application.factory_calls.load(Ordering::SeqCst), 1);
        assert!(std::net::TcpListener::bind(replication_address).is_err());

        handle
            .execute_stateless_preview(intent(
                &preview,
                handle.process_session_id(),
                2,
                "close-open-once",
                PublicOperationProgram::Close,
            ))
            .await
            .unwrap();
        drop(std::net::TcpListener::bind(replication_address).unwrap());
        running.shutdown();
        running.wait().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn hosted_preview_contained_close_restarts_without_replaying_interrupted_children() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let resource_uid = ResourceUid::new("preview-contained-close");
        let preview = PublicOperationPreviewIdentity::public_v1(12);
        let mut replica =
            HostedMemoryReplica::start("contained-close", &resource_uid, &preview, 1).await;
        let generation = PublicProtocolGeneration::new(12);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        replica.initialize_history(&initialization, &history).await;
        let epoch = Epoch::new(0, 1);
        install_static_authority(
            &replica.handle(),
            &preview,
            replica.identity.clone(),
            ReplicaRole::Primary,
            epoch,
            PublicConfiguration {
                current: ConfigurationDescriptor::new(
                    epoch,
                    replica.identity.replica_id,
                    vec![ConfigurationMember {
                        identity: replica.identity.clone(),
                        role: ReplicaRole::Primary,
                    }],
                    1,
                ),
                previous: None,
                replicas: Vec::new(),
            },
        )
        .await;

        replica.application.fail_next_close();
        let close_id = OperationId::new("preview-contained-close");
        replica
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                replica.handle().process_session_id(),
                5,
                close_id.as_str(),
                PublicOperationProgram::Close,
            ))
            .await
            .expect("child close failure must normalize to outer success");

        assert!(
            replica
                .application
                .replicator_released_before_application_close
                .load(Ordering::Acquire),
            "Replicator teardown did not precede application close"
        );
        assert_eq!(
            *replica.application.lifecycle_events.lock().unwrap(),
            [
                "application.close.begin",
                "application.close.end",
                "application.abort",
            ],
            "child failure did not trigger ordered abort containment"
        );
        assert_eq!(
            replica.application.abort_calls.load(Ordering::SeqCst),
            1,
            "close failure must abort each child bundle exactly once"
        );

        let database = SqliteStore::metadata_database_path(&replica.root);
        let connection = rusqlite::Connection::open(database).unwrap();
        let json: String = connection
            .query_row(
                "SELECT state_json FROM agent_state WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&json).unwrap();
        let close = &state["publicOperationPreview"]["operations"][close_id.as_str()];
        assert_eq!(
            close["disposition"]["kind"],
            serde_json::Value::String("succeeded".into()),
            "normalized outer close result was not retained as success"
        );
        let outcomes = close["lifecycle"]["outcomes"]
            .as_array()
            .expect("close lifecycle outcomes");
        let diagnostic = outcomes
            .iter()
            .find(|outcome| outcome.get("CloseFailure").is_some())
            .cloned()
            .expect("typed close failure was not retained");
        assert_eq!(
            serde_json::from_value::<StoredCloseDiagnostic>(diagnostic).unwrap(),
            StoredCloseDiagnostic::CloseFailure {
                child: StoredCloseChild::Application,
                error: "application error: injected application close failure".into(),
            }
        );

        let root = replica.root.clone();
        let identity = replica.identity.clone();
        let storage = replica.state.clone();
        let address = replica.address;
        replica.stop().await;
        let replay_application = Arc::new(PreviewApplication::new(
            storage,
            format!("http://{address}"),
        ));
        let mut restarted = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: identity.replica_id,
                pod_uid: PodUid::new("contained-close-pod"),
                pvc_uid: PvcUid::new("contained-close-pvc"),
                data_root: root.clone(),
                control_address: free_address(),
                replication_address: address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            replay_application.clone(),
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_stateless_preview(PreviewLifecycleBinding {
            preview,
            resource_uid,
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        })
        .start()
        .await
        .unwrap();
        assert_eq!(replay_application.opens.load(Ordering::SeqCst), 0);
        assert!(
            replay_application
                .lifecycle_events
                .lock()
                .unwrap()
                .is_empty()
        );
        assert_eq!(replay_application.abort_calls.load(Ordering::SeqCst), 0);
        restarted.shutdown();
        restarted.wait().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn hosted_preview_role_address_publishes_replaces_and_clears() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let resource_uid = ResourceUid::new("preview-role-address");
        let preview = PublicOperationPreviewIdentity::public_v1(71);
        let mut replica =
            HostedMemoryReplica::start("role-address", &resource_uid, &preview, 1).await;
        let initialization =
            derive_history_initialization_id(&resource_uid, PublicProtocolGeneration::new(71));
        let history = derive_bootstrap_history(
            &resource_uid,
            PublicProtocolGeneration::new(71),
            &initialization,
        );
        replica.initialize_history(&initialization, &history).await;
        let first_epoch = Epoch::new(0, 1);
        let first_configuration = ConfigurationDescriptor::new(
            first_epoch,
            replica.identity.replica_id,
            vec![ConfigurationMember {
                identity: replica.identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        install_static_authority(
            &replica.handle(),
            &preview,
            replica.identity.clone(),
            ReplicaRole::Primary,
            first_epoch,
            PublicConfiguration {
                current: first_configuration,
                previous: None,
                replicas: Vec::new(),
            },
        )
        .await;
        let published = replica.handle().stateless_preview_report().await.unwrap();
        let first_location = published
            .service_location
            .expect("primary address published");
        assert_eq!(first_location.address, "http://127.0.0.1:8080");

        let second_epoch = Epoch::new(0, 2);
        replica
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                replica.handle().process_session_id(),
                5,
                "role-address-epoch",
                PublicOperationProgram::Epoch {
                    epoch: second_epoch,
                },
            ))
            .await
            .unwrap();
        replica
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                replica.handle().process_session_id(),
                6,
                "role-address-replace",
                PublicOperationProgram::Role {
                    epoch: second_epoch,
                    role: ReplicaRole::Primary,
                },
            ))
            .await
            .unwrap();
        replica
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                replica.handle().process_session_id(),
                8,
                "role-address-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: ConfigurationDescriptor::new(
                        second_epoch,
                        replica.identity.replica_id,
                        vec![ConfigurationMember {
                            identity: replica.identity.clone(),
                            role: ReplicaRole::Primary,
                        }],
                        1,
                    ),
                    previous: None,
                    replicas: Vec::new(),
                }),
            ))
            .await
            .unwrap();
        let replaced = replica.handle().stateless_preview_report().await.unwrap();
        let second_location = replaced
            .service_location
            .expect("replacement address published");
        assert_ne!(first_location.operation_id, second_location.operation_id);
        assert_eq!(second_location.address, first_location.address);

        replica
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                replica.handle().process_session_id(),
                9,
                "role-address-clear",
                PublicOperationProgram::Role {
                    epoch: second_epoch,
                    role: ReplicaRole::ActiveSecondary,
                },
            ))
            .await
            .unwrap();
        assert!(
            replica
                .handle()
                .stateless_preview_report()
                .await
                .unwrap()
                .service_location
                .is_none()
        );
        replica.stop().await;
        replica.cleanup();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn primary_write_revocation_fences_provider_apply_and_peer_ack_interleavings() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let apply_pair = AuthorizedHostedPair::start("write-apply-cut", 72).await;
        apply_pair.primary.state.block_apply();
        let write = tokio::spawn({
            let application = apply_pair.primary.application.clone();
            async move {
                application
                    .replicate(Bytes::from_static(b"apply-cut"))
                    .await
            }
        });
        apply_pair.primary.state.apply_entered.notified().await;
        let demotion = tokio::spawn({
            let handle = apply_pair.primary.handle();
            let preview = apply_pair.preview.clone();
            let epoch = apply_pair.epoch;
            async move {
                handle
                    .execute_stateless_preview(intent(
                        &preview,
                        handle.process_session_id(),
                        5,
                        "demote-during-apply",
                        PublicOperationProgram::Role {
                            epoch,
                            role: ReplicaRole::ActiveSecondary,
                        },
                    ))
                    .await
            }
        });
        tokio::task::yield_now().await;
        apply_pair.primary.state.release_apply.notify_waiters();
        assert!(write.await.unwrap().is_err());
        demotion.await.unwrap().unwrap();
        assert_eq!(
            apply_pair
                .primary
                .state
                .progress
                .lock()
                .unwrap()
                .committed_lsn,
            0
        );
        assert_eq!(
            apply_pair
                .primary
                .handle()
                .stateless_preview_report()
                .await
                .unwrap()
                .write_status,
            AccessStatus::NotPrimary
        );
        apply_pair.cleanup().await;

        let ack_pair = AuthorizedHostedPair::start("write-ack-cut", 73).await;
        ack_pair.secondary.state.block_apply();
        let write = tokio::spawn({
            let application = ack_pair.primary.application.clone();
            async move { application.replicate(Bytes::from_static(b"ack-cut")).await }
        });
        ack_pair.secondary.state.apply_entered.notified().await;
        let demotion = tokio::spawn({
            let handle = ack_pair.primary.handle();
            let preview = ack_pair.preview.clone();
            let epoch = ack_pair.epoch;
            async move {
                handle
                    .execute_stateless_preview(intent(
                        &preview,
                        handle.process_session_id(),
                        5,
                        "demote-during-ack",
                        PublicOperationProgram::Role {
                            epoch,
                            role: ReplicaRole::ActiveSecondary,
                        },
                    ))
                    .await
            }
        });
        tokio::task::yield_now().await;
        ack_pair.secondary.state.release_apply.notify_waiters();
        assert!(write.await.unwrap().is_err());
        demotion.await.unwrap().unwrap();
        assert_eq!(
            ack_pair
                .primary
                .state
                .progress
                .lock()
                .unwrap()
                .committed_lsn,
            0
        );
        ack_pair.cleanup().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn primary_write_close_and_dropped_future_leave_conservative_fences() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let close_pair = AuthorizedHostedPair::start("write-close-cut", 74).await;
        close_pair.primary.state.block_commit();
        let write = tokio::spawn({
            let application = close_pair.primary.application.clone();
            async move {
                application
                    .replicate(Bytes::from_static(b"close-cut"))
                    .await
            }
        });
        close_pair.primary.state.commit_entered.notified().await;
        let close = tokio::spawn({
            let handle = close_pair.primary.handle();
            let preview = close_pair.preview.clone();
            async move {
                handle
                    .execute_stateless_preview(intent(
                        &preview,
                        handle.process_session_id(),
                        5,
                        "close-during-commit",
                        PublicOperationProgram::Close,
                    ))
                    .await
            }
        });
        tokio::task::yield_now().await;
        close_pair.primary.state.release_commit.notify_waiters();
        assert!(write.await.unwrap().is_err());
        close.await.unwrap().unwrap();
        assert!(
            close_pair
                .primary
                .application
                .replicate(Bytes::from_static(b"after-close-cut"))
                .await
                .is_err()
        );
        close_pair.cleanup().await;

        let epoch_pair = AuthorizedHostedPair::start("write-epoch-tail-cut", 751).await;
        epoch_pair.primary.state.block_commit();
        let write = tokio::spawn({
            let application = epoch_pair.primary.application.clone();
            async move {
                application
                    .replicate(Bytes::from_static(b"epoch-tail-cut"))
                    .await
            }
        });
        epoch_pair.primary.state.commit_entered.notified().await;
        let next_epoch = Epoch::new(0, 2);
        let epoch_update = tokio::spawn({
            let handle = epoch_pair.primary.handle();
            let preview = epoch_pair.preview.clone();
            async move {
                handle
                    .execute_stateless_preview(intent(
                        &preview,
                        handle.process_session_id(),
                        5,
                        "epoch-during-commit",
                        PublicOperationProgram::Epoch { epoch: next_epoch },
                    ))
                    .await
            }
        });
        tokio::task::yield_now().await;
        epoch_pair.primary.state.release_commit.notify_waiters();
        assert!(write.await.unwrap().is_err());
        epoch_update.await.unwrap().unwrap();
        assert_eq!(
            epoch_pair
                .primary
                .state
                .epoch_tails
                .lock()
                .unwrap()
                .last()
                .copied(),
            Some(1),
            "epoch callback did not receive the drained provider committed tail"
        );
        epoch_pair.cleanup().await;

        let dropped_pair = AuthorizedHostedPair::start("write-drop-cut", 75).await;
        dropped_pair.primary.state.block_apply();
        let write = tokio::spawn({
            let application = dropped_pair.primary.application.clone();
            async move { application.replicate(Bytes::from_static(b"drop-cut")).await }
        });
        dropped_pair.primary.state.apply_entered.notified().await;
        write.abort();
        let _ = write.await;
        dropped_pair.primary.state.release_apply.notify_waiters();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if dropped_pair
                    .primary
                    .handle()
                    .stateless_preview_report()
                    .await
                    .is_ok_and(|report| report.write_status == AccessStatus::NotPrimary)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropped post-apply write did not fence access");
        assert!(
            dropped_pair
                .primary
                .application
                .replicate(Bytes::from_static(b"after-drop-cut"))
                .await
                .is_err()
        );
        dropped_pair.cleanup().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn replication_ingress_accepts_only_exact_current_primary_in_three_replica_set() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let resource_uid = ResourceUid::new("three-replica-ingress");
        let preview = PublicOperationPreviewIdentity::public_v1(76);
        let primary =
            HostedMemoryReplica::start("ingress-primary", &resource_uid, &preview, 1).await;
        let receiver =
            HostedMemoryReplica::start("ingress-receiver", &resource_uid, &preview, 2).await;
        let other = HostedMemoryReplica::start("ingress-other", &resource_uid, &preview, 3).await;
        let initialization =
            derive_history_initialization_id(&resource_uid, PublicProtocolGeneration::new(76));
        let history = derive_bootstrap_history(
            &resource_uid,
            PublicProtocolGeneration::new(76),
            &initialization,
        );
        primary.initialize_history(&initialization, &history).await;
        receiver.initialize_history(&initialization, &history).await;
        other.initialize_history(&initialization, &history).await;
        let epoch = Epoch::new(0, 1);
        let configuration = ConfigurationDescriptor::new(
            epoch,
            primary.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: primary.identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: receiver.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: other.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        install_static_authority(
            &receiver.handle(),
            &preview,
            receiver.identity.clone(),
            ReplicaRole::ActiveSecondary,
            epoch,
            PublicConfiguration {
                current: configuration.clone(),
                previous: None,
                replicas: vec![
                    PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: primary.identity.clone(),
                        replication_address: format!("http://{}", primary.address),
                        process_session_id: primary.handle().process_session_id(),
                        storage_incarnation: Some(primary.storage.clone()),
                        role: ReplicaRole::Primary,
                        current_progress: 0,
                        catch_up_capability: 0,
                        must_catchup: false,
                    },
                    PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: other.identity.clone(),
                        replication_address: format!("http://{}", other.address),
                        process_session_id: other.handle().process_session_id(),
                        storage_incarnation: Some(other.storage.clone()),
                        role: ReplicaRole::ActiveSecondary,
                        current_progress: 0,
                        catch_up_capability: 0,
                        must_catchup: false,
                    },
                ],
            },
        )
        .await;
        let send = |sender: &HostedMemoryReplica, data: &'static [u8]| proto::ReplicationItem {
            protocol_version: 11,
            sender: Some(sender.identity.clone().into()),
            receiver: Some(receiver.identity.clone().into()),
            epoch: Some(epoch.into()),
            previous_configuration_id: String::new(),
            current_configuration_id: configuration.configuration_id.to_string(),
            lsn: 1,
            committed_lsn: 0,
            data: data.to_vec(),
            sender_session_id: sender.handle().process_session_id().to_string(),
            receiver_session_id: receiver.handle().process_session_id().to_string(),
        };
        let mut client = proto::replication_data_client::ReplicationDataClient::connect(format!(
            "http://{}",
            receiver.address
        ))
        .await
        .unwrap();
        let mut rejected = client
            .replicate(Request::new(stream::iter([send(
                &other,
                b"secondary-origin",
            )])))
            .await
            .unwrap()
            .into_inner();
        assert!(rejected.next().await.unwrap().is_err());
        assert_eq!(
            *receiver.state.progress.lock().unwrap(),
            DurableApplicationProgress::default()
        );
        assert!(receiver.state.operations.lock().unwrap().is_empty());

        let mut accepted = client
            .replicate(Request::new(stream::iter([send(
                &primary,
                b"primary-origin",
            )])))
            .await
            .unwrap()
            .into_inner();
        assert!(accepted.next().await.unwrap().is_ok());
        assert_eq!(receiver.state.progress.lock().unwrap().applied_lsn, 1);
        assert_eq!(
            receiver.state.operations.lock().unwrap()[0].data.as_ref(),
            b"primary-origin"
        );

        let mut primary = primary;
        let mut receiver = receiver;
        let mut other = other;
        primary.stop().await;
        receiver.stop().await;
        other.stop().await;
        primary.cleanup();
        receiver.cleanup();
        other.cleanup();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn bounded_concurrent_peer_work_ignores_hung_minority_and_drains_on_close() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let pair = AuthorizedHostedPair::start("hung-minority", 77).await;
        let hung_identity = replica_named(3, "hung-peer-pod", "hung-peer-agent");
        let hung_storage = storage_named(hung_identity.clone(), "hung-peer-pod", "hung-peer-pvc");
        let hung_session = ProcessSessionId::new("hung-peer-session");
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let hung_address = listener.local_addr().unwrap();
        let hung_task = tokio::spawn(async move {
            let (_connection, _) = listener.accept().await.unwrap();
            futures::future::pending::<()>().await;
        });
        let three = ConfigurationDescriptor::new(
            pair.epoch,
            pair.primary.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: pair.primary.identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: pair.secondary.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: hung_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        pair.primary
            .handle()
            .execute_stateless_preview(intent(
                &pair.preview,
                pair.primary.handle().process_session_id(),
                5,
                "hung-minority-primary-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: three.clone(),
                    previous: None,
                    replicas: vec![
                        PublicReplicaDescriptor {
                            build_id: OperationId::default(),
                            identity: pair.secondary.identity.clone(),
                            replication_address: format!("http://{}", pair.secondary.address),
                            process_session_id: pair.secondary.handle().process_session_id(),
                            storage_incarnation: Some(pair.secondary.storage.clone()),
                            role: ReplicaRole::ActiveSecondary,
                            current_progress: 0,
                            catch_up_capability: 0,
                            must_catchup: false,
                        },
                        PublicReplicaDescriptor {
                            build_id: OperationId::default(),
                            identity: hung_identity.clone(),
                            replication_address: format!("http://{hung_address}"),
                            process_session_id: hung_session.clone(),
                            storage_incarnation: Some(hung_storage.clone()),
                            role: ReplicaRole::ActiveSecondary,
                            current_progress: 0,
                            catch_up_capability: 0,
                            must_catchup: false,
                        },
                    ],
                }),
            ))
            .await
            .unwrap();
        pair.secondary
            .handle()
            .execute_stateless_preview(intent(
                &pair.preview,
                pair.secondary.handle().process_session_id(),
                5,
                "hung-minority-secondary-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: three,
                    previous: None,
                    replicas: vec![
                        PublicReplicaDescriptor {
                            build_id: OperationId::default(),
                            identity: pair.primary.identity.clone(),
                            replication_address: format!("http://{}", pair.primary.address),
                            process_session_id: pair.primary.handle().process_session_id(),
                            storage_incarnation: Some(pair.primary.storage.clone()),
                            role: ReplicaRole::Primary,
                            current_progress: 0,
                            catch_up_capability: 0,
                            must_catchup: false,
                        },
                        PublicReplicaDescriptor {
                            build_id: OperationId::default(),
                            identity: hung_identity.clone(),
                            replication_address: format!("http://{hung_address}"),
                            process_session_id: hung_session.clone(),
                            storage_incarnation: Some(hung_storage.clone()),
                            role: ReplicaRole::ActiveSecondary,
                            current_progress: 0,
                            catch_up_capability: 0,
                            must_catchup: false,
                        },
                    ],
                }),
            ))
            .await
            .unwrap();
        let started = std::time::Instant::now();
        assert_eq!(
            pair.primary
                .application
                .replicate(Bytes::from_static(b"healthy-quorum"))
                .await
                .unwrap(),
            1
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        hung_task.abort();
        let _ = hung_task.await;
        pair.cleanup().await;

        let resource_uid = ResourceUid::new("required-hung-peer");
        let preview = PublicOperationPreviewIdentity::public_v1(78);
        let mut primary =
            HostedMemoryReplica::start("required-hung-primary", &resource_uid, &preview, 1).await;
        let initialization =
            derive_history_initialization_id(&resource_uid, PublicProtocolGeneration::new(78));
        let history = derive_bootstrap_history(
            &resource_uid,
            PublicProtocolGeneration::new(78),
            &initialization,
        );
        primary.initialize_history(&initialization, &history).await;
        let epoch = Epoch::new(0, 1);
        let hung_identity = replica_named(2, "required-hung-pod", "required-hung-agent");
        let hung_storage = storage_named(
            hung_identity.clone(),
            "required-hung-pod",
            "required-hung-pvc",
        );
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let hung_address = listener.local_addr().unwrap();
        let hung_task = tokio::spawn(async move {
            let (_connection, _) = listener.accept().await.unwrap();
            futures::future::pending::<()>().await;
        });
        install_static_authority(
            &primary.handle(),
            &preview,
            primary.identity.clone(),
            ReplicaRole::Primary,
            epoch,
            PublicConfiguration {
                current: ConfigurationDescriptor::new(
                    epoch,
                    primary.identity.replica_id,
                    vec![
                        ConfigurationMember {
                            identity: primary.identity.clone(),
                            role: ReplicaRole::Primary,
                        },
                        ConfigurationMember {
                            identity: hung_identity.clone(),
                            role: ReplicaRole::ActiveSecondary,
                        },
                    ],
                    2,
                ),
                previous: None,
                replicas: vec![PublicReplicaDescriptor {
                    build_id: OperationId::default(),
                    identity: hung_identity,
                    replication_address: format!("http://{hung_address}"),
                    process_session_id: ProcessSessionId::new("required-hung-session"),
                    storage_incarnation: Some(hung_storage),
                    role: ReplicaRole::ActiveSecondary,
                    current_progress: 0,
                    catch_up_capability: 0,
                    must_catchup: false,
                }],
            },
        )
        .await;
        let started = std::time::Instant::now();
        assert!(
            primary
                .application
                .replicate(Bytes::from_static(b"required-hung"))
                .await
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(7));
        hung_task.abort();
        let _ = hung_task.await;
        primary.stop().await;
        primary.cleanup();

        let close_pair = AuthorizedHostedPair::start("hung-close", 79).await;
        close_pair.secondary.state.block_apply();
        let write = tokio::spawn({
            let application = close_pair.primary.application.clone();
            async move {
                application
                    .replicate(Bytes::from_static(b"close-drain"))
                    .await
            }
        });
        close_pair.secondary.state.apply_entered.notified().await;
        let close = tokio::spawn({
            let handle = close_pair.primary.handle();
            let preview = close_pair.preview.clone();
            async move {
                handle
                    .execute_stateless_preview(intent(
                        &preview,
                        handle.process_session_id(),
                        5,
                        "close-drains-peer-work",
                        PublicOperationProgram::Close,
                    ))
                    .await
            }
        });
        close_pair.secondary.state.release_apply.notify_waiters();
        assert!(write.await.unwrap().is_err());
        tokio::time::timeout(Duration::from_secs(7), close)
            .await
            .expect("Close did not drain bounded peer work")
            .unwrap()
            .unwrap();
        close_pair.cleanup().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn static_preview_is_store_free_access_gated_and_durable_before_ack() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = test_root("static");
        let resource_uid = ResourceUid::new("preview-resource");
        let preview = PublicOperationPreviewIdentity::public_v1(1);
        let binding = PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let database = SqliteStore::metadata_database_path(&root);
        drop(
            SqliteStore::create_public_v1(
                &database,
                preview.clone(),
                store_identity(resource_uid.clone()),
            )
            .unwrap(),
        );
        let control_address = free_address();
        let replication_address = free_address();
        let storage = Arc::new(MemoryState::default());
        let application = Arc::new(PreviewApplication::new(
            storage.clone(),
            format!("http://{replication_address}"),
        ));
        let mut running = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("preview-pod-uid"),
                pvc_uid: PvcUid::new("preview-pvc-uid"),
                data_root: root.clone(),
                control_address,
                replication_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            application.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding)
        .start()
        .await
        .unwrap();
        let handle = running.handle();
        assert_eq!(handle.stateless_preview_provider_writer_count(), 0);
        let generation = PublicProtocolGeneration::new(1);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        handle
            .initialize_stateless_preview_history(
                &initialization,
                &history,
                &storage_incarnation(replica()),
            )
            .await
            .unwrap();
        let session = handle.process_session_id();
        handle
            .execute_stateless_preview(intent(
                &preview,
                session.clone(),
                1,
                "open",
                PublicOperationProgram::Open {
                    replica: replica(),
                    existing: true,
                },
            ))
            .await
            .unwrap();
        assert_eq!(handle.stateless_preview_provider_writer_count(), 1);
        let epoch = Epoch::new(0, 1);
        handle
            .execute_stateless_preview(intent(
                &preview,
                session.clone(),
                2,
                "role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::Primary,
                },
            ))
            .await
            .unwrap();
        handle
            .execute_stateless_preview(intent(
                &preview,
                session.clone(),
                3,
                "epoch",
                PublicOperationProgram::Epoch { epoch },
            ))
            .await
            .unwrap();
        let before = handle.stateless_preview_report().await.unwrap();
        assert_eq!(
            before.write_status,
            kuberic_runtime::protocol::types::AccessStatus::NotPrimary
        );
        let configuration = ConfigurationDescriptor::new(
            epoch,
            ReplicaId::new(1),
            vec![ConfigurationMember {
                identity: replica(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        handle
            .execute_stateless_preview(intent(
                &preview,
                session,
                4,
                "configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: configuration,
                    previous: None,
                    replicas: Vec::new(),
                }),
            ))
            .await
            .unwrap();
        let ready = handle.stateless_preview_report().await.unwrap();
        assert_eq!(
            ready.write_status,
            kuberic_runtime::protocol::types::AccessStatus::Granted
        );

        storage.block_commit();
        let write = tokio::spawn({
            let application = application.clone();
            async move { application.replicate(Bytes::from_static(b"durable")).await }
        });
        storage.commit_entered.notified().await;
        assert!(
            !write.is_finished(),
            "ACK preceded provider commit durability"
        );
        storage.release_commit.notify_waiters();
        assert_eq!(write.await.unwrap().unwrap(), 1);
        assert_eq!(storage.progress.lock().unwrap().committed_lsn, 1);
        assert!(!application.on_data_loss().await.unwrap());
        storage.change_on_data_loss();
        assert!(application.on_data_loss().await.unwrap());
        storage.fail_next_data_loss();
        assert!(application.on_data_loss().await.is_err());
        assert!(
            application
                .replicate(Bytes::from_static(b"requires-history-readmission"))
                .await
                .is_err()
        );

        handle
            .execute_stateless_preview(intent(
                &preview,
                handle.process_session_id(),
                5,
                "close",
                PublicOperationProgram::Close,
            ))
            .await
            .unwrap();
        std::net::TcpListener::bind(replication_address).unwrap();
        running.shutdown();
        running.wait().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();

        let root = test_root("open-failure");
        let resource_uid = ResourceUid::new("preview-open-failure");
        let preview = PublicOperationPreviewIdentity::public_v1(5);
        let binding = PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        drop(
            SqliteStore::create_public_v1(
                SqliteStore::metadata_database_path(&root),
                preview.clone(),
                store_identity(resource_uid.clone()),
            )
            .unwrap(),
        );
        let blocker = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let replication_address = blocker.local_addr().unwrap();
        let mut running = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("preview-pod-uid"),
                pvc_uid: PvcUid::new("preview-pvc-uid"),
                data_root: root.clone(),
                control_address: free_address(),
                replication_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            Arc::new(PreviewApplication::new(
                Arc::new(MemoryState::default()),
                format!("http://{replication_address}"),
            )),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding)
        .start()
        .await
        .unwrap();
        let handle = running.handle();
        let generation = PublicProtocolGeneration::new(5);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        handle
            .initialize_stateless_preview_history(
                &initialization,
                &history,
                &storage_incarnation(replica()),
            )
            .await
            .unwrap();
        assert!(
            handle
                .execute_stateless_preview(intent(
                    &preview,
                    handle.process_session_id(),
                    1,
                    "open-failure",
                    PublicOperationProgram::Open {
                        replica: replica(),
                        existing: true,
                    },
                ))
                .await
                .is_err(),
            "preview Open failure must fail the durable Open"
        );
        assert!(handle.stateless_preview_report().await.is_err());
        drop(blocker);
        std::net::TcpListener::bind(replication_address).unwrap();
        running.shutdown();
        running.wait().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();

        let root = test_root("factory-mismatch");
        let resource_uid = ResourceUid::new("preview-factory-mismatch");
        let preview = PublicOperationPreviewIdentity::public_v1(3);
        let binding = PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let database = SqliteStore::metadata_database_path(&root);
        drop(
            SqliteStore::create_public_v1(
                &database,
                preview.clone(),
                store_identity(resource_uid.clone()),
            )
            .unwrap(),
        );
        let replication_address = free_address();
        let mut running = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("preview-pod-uid"),
                pvc_uid: PvcUid::new("preview-pvc-uid"),
                data_root: root.clone(),
                control_address: free_address(),
                replication_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            Arc::new(PreviewApplication::legacy(
                Arc::new(MemoryState::default()),
                format!("http://{replication_address}"),
            )),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding)
        .start()
        .await
        .unwrap();
        let handle = running.handle();
        let generation = PublicProtocolGeneration::new(3);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        handle
            .initialize_stateless_preview_history(
                &initialization,
                &history,
                &storage_incarnation(replica()),
            )
            .await
            .unwrap();
        assert!(
            handle
                .execute_stateless_preview(intent(
                    &preview,
                    handle.process_session_id(),
                    1,
                    "factory-mismatch",
                    PublicOperationProgram::Open {
                        replica: replica(),
                        existing: true,
                    },
                ))
                .await
                .is_err(),
            "crossed preview/managed factory selection must fail"
        );
        std::net::TcpListener::bind(replication_address).unwrap();
        running.shutdown();
        running.wait().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn commit_failure_permanently_contains_the_ambiguous_suffix() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = test_root("commit-ambiguity");
        let resource_uid = ResourceUid::new("preview-commit-ambiguity");
        let preview = PublicOperationPreviewIdentity::public_v1(8);
        let binding = PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        drop(
            SqliteStore::create_public_v1(
                SqliteStore::metadata_database_path(&root),
                preview.clone(),
                store_identity(resource_uid.clone()),
            )
            .unwrap(),
        );
        let replication_address = free_address();
        let storage = Arc::new(MemoryState::default());
        let application = Arc::new(PreviewApplication::new(
            storage.clone(),
            format!("http://{replication_address}"),
        ));
        let mut running = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("preview-pod-uid"),
                pvc_uid: PvcUid::new("preview-pvc-uid"),
                data_root: root.clone(),
                control_address: free_address(),
                replication_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            application.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding)
        .start()
        .await
        .unwrap();
        let handle = running.handle();
        let generation = PublicProtocolGeneration::new(8);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        handle
            .initialize_stateless_preview_history(
                &initialization,
                &history,
                &storage_incarnation(replica()),
            )
            .await
            .unwrap();
        let epoch = Epoch::new(0, 1);
        install_static_authority(
            &handle,
            &preview,
            replica(),
            ReplicaRole::Primary,
            epoch,
            PublicConfiguration {
                current: ConfigurationDescriptor::new(
                    epoch,
                    ReplicaId::new(1),
                    vec![ConfigurationMember {
                        identity: replica(),
                        role: ReplicaRole::Primary,
                    }],
                    1,
                ),
                previous: None,
                replicas: Vec::new(),
            },
        )
        .await;
        let ready = handle.stateless_preview_report().await.unwrap();
        assert_eq!(
            ready.write_status,
            kuberic_runtime::protocol::types::AccessStatus::Granted
        );

        storage.fail_next_commit();
        let error = application
            .replicate(Bytes::from_static(b"ambiguous"))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("injected provider commit failure")
        );
        assert_eq!(
            *storage.progress.lock().unwrap(),
            DurableApplicationProgress {
                applied_lsn: 1,
                committed_lsn: 0,
            }
        );
        let fenced = handle.stateless_preview_report().await.unwrap();
        assert_eq!(
            fenced.read_status,
            kuberic_runtime::protocol::types::AccessStatus::NotPrimary
        );
        assert_eq!(
            fenced.write_status,
            kuberic_runtime::protocol::types::AccessStatus::NotPrimary
        );
        assert!(
            application
                .replicate(Bytes::from_static(b"later"))
                .await
                .is_err()
        );
        assert_eq!(storage.operations.lock().unwrap().len(), 1);

        DurableState::commit(storage.as_ref(), 1).await.unwrap();
        handle
            .execute_stateless_preview(intent(
                &preview,
                handle.process_session_id(),
                5,
                "progress-after-ambiguity",
                PublicOperationProgram::Progress { capability: false },
            ))
            .await
            .unwrap();
        let still_fenced = handle.stateless_preview_report().await.unwrap();
        assert_eq!(still_fenced.progress.first, 1);
        assert_eq!(still_fenced.progress.last, 1);
        assert_eq!(
            still_fenced.read_status,
            kuberic_runtime::protocol::types::AccessStatus::NotPrimary
        );
        assert_eq!(
            still_fenced.write_status,
            kuberic_runtime::protocol::types::AccessStatus::NotPrimary
        );

        running.shutdown();
        running.wait().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn preview_rejects_schema6_and_crossed_factory_without_fallback() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = test_root("crossed");
        let resource_uid = ResourceUid::new("preview-crossed");
        let database = SqliteStore::metadata_database_path(&root);
        drop(
            SqliteStore::create_authorized(
                &database,
                AgentState::new(store_identity(resource_uid.clone())),
            )
            .unwrap(),
        );
        let replication_address = free_address();
        let preview = PublicOperationPreviewIdentity::public_v1(2);
        let binding = PreviewLifecycleBinding {
            preview,
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let storage = Arc::new(MemoryState::default());
        let application = Arc::new(PreviewApplication::new(
            storage,
            format!("http://{replication_address}"),
        ));
        let result = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid,
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("preview-pod-uid"),
                pvc_uid: PvcUid::new("preview-pvc-uid"),
                data_root: root.clone(),
                control_address: free_address(),
                replication_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            application,
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding)
        .start()
        .await;
        assert!(result.is_err(), "schema 6 must not fall back to legacy");
        std::net::TcpListener::bind(replication_address).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn preview_initialization_creates_exact_bound_schema11_before_open() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = test_root("fresh-schema11");
        let resource_uid = ResourceUid::new("preview-fresh-schema11");
        let preview = PublicOperationPreviewIdentity::public_v1(6);
        let binding = PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let control_address = free_address();
        let replication_address = free_address();
        let application = Arc::new(PreviewApplication::new(
            Arc::new(MemoryState::default()),
            format!("http://{replication_address}"),
        ));
        let host = tokio::spawn(
            ReplicaHost::new(
                ReplicaProcessConfig {
                    resource_uid: resource_uid.clone(),
                    replica_id: ReplicaId::new(1),
                    pod_uid: PodUid::new("fresh-pod"),
                    pvc_uid: PvcUid::new("fresh-pvc"),
                    data_root: root.clone(),
                    control_address,
                    replication_address,
                    bearer_token: "preview-token".into(),
                    rpc_deadline: Duration::from_secs(2),
                    transport_window_capacity: 8,
                },
                application,
                ApplicationStorageState::FreshEmpty,
                Arc::new(Resolver),
            )
            .with_stateless_preview(binding)
            .start(),
        );
        let mut client = loop {
            match proto::agent_control_client::AgentControlClient::connect(format!(
                "http://{control_address}"
            ))
            .await
            {
                Ok(client) => break client,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        };
        let status = client
            .get_status(authorized(proto::GetAgentStatusRequest {
                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                resource_uid: resource_uid.to_string(),
                replica_id: 1,
                expected_instance_id: "fresh-pod".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        let initialization = derive_initialization_id(
            &resource_uid,
            ReplicaId::new(1),
            &PodUid::new("fresh-pod"),
            &PvcUid::new("fresh-pvc"),
        );
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(1),
            instance_id: ReplicaInstanceId::new("fresh-pod"),
            agent_generation: derive_agent_generation(&initialization),
        };
        let configuration = ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            ReplicaId::new(1),
            vec![ConfigurationMember {
                identity: identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        client
            .execute(authorized(proto::ExecuteCommandRequest {
                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                resource_uid: resource_uid.to_string(),
                target: Some(identity.clone().into()),
                expected_process_session_id: status.process_session_id,
                command: Some(
                    proto::execute_command_request::Command::InitializeAgentStore(
                        proto::InitializeAgentStoreCommand {
                            initialization_id: initialization.to_string(),
                            resource_uid: resource_uid.to_string(),
                            local_replica_id: 1,
                            expected_instance_id: "fresh-pod".into(),
                            expected_pod_uid: "fresh-pod".into(),
                            expected_pvc_uid: "fresh-pvc".into(),
                            assigned_agent_generation: identity.agent_generation.to_string(),
                            effective_policy: Some(proto::EffectivePolicy {
                                replica_set_size: 1,
                                write_quorum: 1,
                                read_quorum: 1,
                                failover_delay_seconds: 30,
                            }),
                            bootstrap_configuration: Some(configuration.into()),
                            provisioning: None,
                        },
                    ),
                ),
            }))
            .await
            .unwrap();
        drop(client);
        let mut running = host.await.unwrap().unwrap();
        let database = SqliteStore::metadata_database_path(&root);
        let connection = rusqlite::Connection::open(&database).unwrap();
        let schema: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(schema, 11);
        drop(connection);
        drop(SqliteStore::open_public_v1(&database, &preview).unwrap());
        running.shutdown();
        running.wait().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn hosted_preview_authorized_data_loss_maps_only_the_effective_outcome() {
    #[derive(Clone, Copy)]
    enum Outcome {
        Unchanged,
        Changed,
        Error,
        AmbiguousReplacement,
    }

    tokio::time::timeout(Duration::from_secs(120), async {
        for (index, outcome) in [
            Outcome::Unchanged,
            Outcome::Changed,
            Outcome::Error,
            Outcome::AmbiguousReplacement,
        ]
        .into_iter()
        .enumerate()
        {
            let resource_uid = ResourceUid::new(format!("preview-data-loss-{index}"));
            let generation_number = 30 + u64::try_from(index).unwrap();
            let preview = PublicOperationPreviewIdentity::public_v1(generation_number);
            let mut replica = HostedMemoryReplica::start(
                &format!("data-loss-{index}"),
                &resource_uid,
                &preview,
                1,
            )
            .await;
            let generation = PublicProtocolGeneration::new(generation_number);
            let initialization = derive_history_initialization_id(&resource_uid, generation);
            let predecessor = derive_bootstrap_history(&resource_uid, generation, &initialization);
            replica
                .initialize_history(&initialization, &predecessor)
                .await;
            replica
                .handle()
                .execute_stateless_preview(intent(
                    &preview,
                    replica.handle().process_session_id(),
                    1,
                    &format!("recovery-open-{index}"),
                    PublicOperationProgram::Open {
                        replica: replica.identity.clone(),
                        existing: true,
                    },
                ))
                .await
                .unwrap();
            let before = replica.handle().stateless_preview_report().await.unwrap();
            assert_eq!(before.write_status, AccessStatus::NotPrimary);

            let authorization_id = OperationId::new(format!("authorize-data-loss-{index}"));
            let proposed = derive_data_loss_history(
                &authorization_id,
                &predecessor,
                predecessor.data_loss_number + 1,
                &replica.storage,
            )
            .unwrap();
            let authorization = DataLossAuthorization {
                authorization_id: authorization_id.clone(),
                predecessor: predecessor.clone(),
                proposed: proposed.clone(),
                provisional_primary: replica.storage.clone(),
                reset_scope: match outcome {
                    Outcome::Changed | Outcome::AmbiguousReplacement => {
                        ProviderProgressReset::ResetToEmpty
                    }
                    Outcome::Unchanged | Outcome::Error => ProviderProgressReset::Preserve,
                },
                replay_disposition: DataLossReplay::ReplaceOnAmbiguity,
            };
            let endpoint = replica
                .handle()
                .stateless_preview_process_endpoint(ReplicaRole::None)
                .await
                .unwrap();
            let recovery_epoch = Epoch::new(proposed.data_loss_number, 1);
            let attempt = DataLossAttempt {
                authorization_id,
                attempt_id: OperationId::new(format!("data-loss-attempt-{index}")),
                process: ProcessAttemptContext {
                    process_session_id: endpoint.process_session_id.clone(),
                    replicator_creation_id: endpoint.replicator_creation_id,
                    catch_up_specific_quorum: true,
                    data_loss_replay: DataLossReplay::ReplaceOnAmbiguity,
                    epoch: recovery_epoch,
                    attempt_revision: 2,
                },
            };
            let members = vec![ConfigurationMember {
                identity: replica.identity.clone(),
                role: ReplicaRole::Primary,
            }];
            let lifecycle = PublicLifecycleInput {
                recipe: PublicLifecycleRecipe::InitialPrimary,
                replica: replica.identity.clone(),
                epoch: recovery_epoch,
                possible_data_loss: PossibleDataLossIntent::Possible,
                current: ConfigurationDescriptor::new(
                    recovery_epoch,
                    replica.identity.replica_id,
                    members.clone(),
                    1,
                ),
                previous: Some(ConfigurationDescriptor::new(
                    Epoch::new(proposed.data_loss_number, 0),
                    replica.identity.replica_id,
                    members,
                    1,
                )),
                replicas: Vec::new(),
                data_loss_recovery: Some(PublicDataLossRecoveryInput {
                    authorization: authorization.clone(),
                    attempt: attempt.clone(),
                }),
            };
            let recovery_intent = PublicOperationIntent {
                preview: preview.clone(),
                operation_id: attempt.attempt_id.clone(),
                revision: attempt.process.attempt_revision,
                process_session_id: attempt.process.process_session_id.clone(),
                class: PublicOperationClass::Authority,
                input_digest: lifecycle.digest(),
                replay_disposition: PublicOperationReplayDisposition::DataLoss(
                    DataLossReplay::ReplaceOnAmbiguity,
                ),
                lifecycle: Some(lifecycle),
                program: None,
            };
            let accepted = PublicV1AcceptedStatus {
                identity: preview.clone(),
                history_initialization_id: initialization,
                history: predecessor.clone(),
                data_loss_recovery: None,
                completed_operations: Vec::new(),
                extensions: Default::default(),
            };
            let transition = PublicV1AcceptedTransitionStatus {
                resource_version: format!("data-loss-status-rv-{index}"),
                evidence: accepted.clone(),
                intent: PublicV1AcceptedTransitionIntent {
                    consumer: PublicV1EvidenceConsumer::DataLoss,
                    operation_id: recovery_intent.operation_id.clone(),
                    revision: recovery_intent.revision,
                    call_kind: PublicV1CallKind::InitialPrimary,
                    history: predecessor.clone(),
                    source: PublicV1ResourceAuthority {
                        storage_incarnation: replica.storage.clone(),
                        resource_version: format!("data-loss-resource-rv-{index}"),
                    },
                    target: None,
                    process_session_id: recovery_intent.process_session_id.clone(),
                    input_digest: recovery_intent.input_digest.clone(),
                    replay_disposition: recovery_intent.replay_disposition,
                    previous_configuration_id: None,
                    current_configuration_id: None,
                    catch_up_mode: None,
                    catch_up_specific_quorum: None,
                },
            };
            let evidence = Arc::new(
                PublicV1EvidenceState::new(
                    transition,
                    vec![PublicV1EvidenceResource {
                        storage_incarnation: replica.storage.clone(),
                        resource_version: format!("data-loss-resource-rv-{index}"),
                        proof: Some(PublicV1ResourceProof {
                            history: predecessor.clone(),
                            storage_incarnation: replica.storage.clone(),
                        }),
                        disposition: PublicV1ResourceDisposition::Active,
                    }],
                )
                .unwrap(),
            );
            let adapter = HostedPublicV1Adapter::new(
                vec![HostedPreviewReplica {
                    identity: replica.identity.clone(),
                    replication_address: format!("http://{}", replica.address),
                    handle: replica.handle(),
                }],
                Some(accepted.clone()),
                None,
                evidence,
            )
            .unwrap();

            let mut wrong_authorization = authorization.clone();
            wrong_authorization.authorization_id =
                OperationId::new(format!("wrong-data-loss-{index}"));
            assert!(
                plan_public_v1_data_loss_recovery(
                    &accepted,
                    &before,
                    &wrong_authorization,
                    &attempt,
                    &recovery_intent,
                    false,
                )
                .is_err()
            );
            let mut missing = recovery_intent.clone();
            missing.lifecycle.as_mut().unwrap().data_loss_recovery = None;
            missing.input_digest = missing.lifecycle.as_ref().unwrap().digest();
            assert!(missing.validate().is_err());
            let unchanged = replica.handle().stateless_preview_report().await.unwrap();
            assert_eq!(unchanged.history, before.history);
            assert_eq!(unchanged.process_session_id, before.process_session_id);
            assert_eq!(unchanged.read_status, before.read_status);
            assert_eq!(unchanged.write_status, before.write_status);
            assert_eq!(unchanged.progress, before.progress);
            assert_eq!(unchanged.data_loss_recovery, None);
            assert!(adapter.dispatched().is_empty());

            execute_public_v1_history(
                &adapter,
                plan_public_v1_data_loss_recovery(
                    &accepted,
                    &before,
                    &authorization,
                    &attempt,
                    &recovery_intent,
                    false,
                )
                .unwrap(),
            )
            .await
            .unwrap();
            let invalidated = adapter.observe_public_v1_status().await.unwrap().unwrap();
            match outcome {
                Outcome::Unchanged => {}
                Outcome::Changed => replica.state.change_on_data_loss(),
                Outcome::Error => replica.state.fail_next_data_loss(),
                Outcome::AmbiguousReplacement => {
                    replica.state.rebuild_on_data_loss(replica.identity.clone());
                }
            }
            let dispatched = execute_public_v1_history(
                &adapter,
                plan_public_v1_data_loss_recovery(
                    &invalidated,
                    &before,
                    &authorization,
                    &attempt,
                    &recovery_intent,
                    false,
                )
                .unwrap(),
            )
            .await;
            assert_eq!(adapter.dispatched().len(), 1);
            assert_eq!(
                replica.state.data_loss_calls.load(Ordering::SeqCst),
                1,
                "recovery dispatch did not invoke the callback: {dispatched:?}"
            );
            if matches!(outcome, Outcome::Unchanged | Outcome::Changed) {
                dispatched
                    .as_ref()
                    .unwrap_or_else(|error| panic!("data-loss outcome {index} failed: {error}"));
            }
            let report = adapter
                .report(&replica.identity)
                .await
                .unwrap_or_else(|error| panic!("data-loss outcome {index} report: {error}"));
            match outcome {
                Outcome::Unchanged | Outcome::Changed => {
                    assert_eq!(report.history, proposed);
                    assert_eq!(report.write_status, AccessStatus::Granted);
                    assert_eq!(
                        report.data_loss_recovery.as_ref().unwrap().stage,
                        kuberic_runtime::protocol::public_operations::
                            PublicDataLossRecoveryStage::Admitted
                    );
                }
                Outcome::Error => {
                    assert!(dispatched.is_err());
                    assert_eq!(report.history, predecessor);
                    assert_eq!(report.write_status, AccessStatus::NotPrimary);
                    assert_eq!(
                        report.data_loss_recovery.as_ref().unwrap().stage,
                        kuberic_runtime::protocol::public_operations::
                            PublicDataLossRecoveryStage::Failed
                    );
                }
                Outcome::AmbiguousReplacement => {
                    assert!(dispatched.is_err());
                    assert_eq!(report.history, predecessor);
                    assert_eq!(report.write_status, AccessStatus::NotPrimary);
                    assert_eq!(
                        report.data_loss_recovery.as_ref().unwrap().stage,
                        kuberic_runtime::protocol::public_operations::
                            PublicDataLossRecoveryStage::ReplacementRequired
                    );
                }
            }
            if !matches!(outcome, Outcome::Unchanged | Outcome::Changed) {
                execute_public_v1_history(
                    &adapter,
                    plan_public_v1_recovery_report(&invalidated, &report).unwrap(),
                )
                .await
                .unwrap();
            }
            replica.stop().await;
            replica.cleanup();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn hosted_preview_unreachable_survivor_blocks_access_until_return_or_retirement() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let resource_uid = ResourceUid::new("preview-unreachable-survivor");
        let preview = PublicOperationPreviewIdentity::public_v1(40);
        let mut primary =
            HostedMemoryReplica::start("fault-primary", &resource_uid, &preview, 1).await;
        let mut survivor =
            HostedMemoryReplica::start("fault-survivor", &resource_uid, &preview, 2).await;
        let generation = PublicProtocolGeneration::new(40);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        primary.initialize_history(&initialization, &history).await;
        survivor.initialize_history(&initialization, &history).await;
        let epoch = Epoch::new(0, 1);
        let configuration = ConfigurationDescriptor::new(
            epoch,
            primary.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: primary.identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: survivor.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        install_static_authority(
            &primary.handle(),
            &preview,
            primary.identity.clone(),
            ReplicaRole::Primary,
            epoch,
            PublicConfiguration {
                current: configuration.clone(),
                previous: None,
                replicas: vec![PublicReplicaDescriptor {
                    build_id: OperationId::default(),
                    identity: survivor.identity.clone(),
                    replication_address: format!("http://{}", survivor.address),
                    process_session_id: survivor.handle().process_session_id(),
                    storage_incarnation: Some(survivor.storage.clone()),
                    role: ReplicaRole::ActiveSecondary,
                    current_progress: 0,
                    catch_up_capability: 0,
                    must_catchup: false,
                }],
            },
        )
        .await;
        install_static_authority(
            &survivor.handle(),
            &preview,
            survivor.identity.clone(),
            ReplicaRole::ActiveSecondary,
            epoch,
            PublicConfiguration {
                current: configuration.clone(),
                previous: None,
                replicas: vec![PublicReplicaDescriptor {
                    build_id: OperationId::default(),
                    identity: primary.identity.clone(),
                    replication_address: format!("http://{}", primary.address),
                    process_session_id: primary.handle().process_session_id(),
                    storage_incarnation: Some(primary.storage.clone()),
                    role: ReplicaRole::Primary,
                    current_progress: 0,
                    catch_up_capability: 0,
                    must_catchup: false,
                }],
            },
        )
        .await;
        assert_eq!(
            primary
                .application
                .replicate(Bytes::from_static(b"before-survivor-loss"))
                .await
                .unwrap(),
            1
        );
        let exact_survivor_session = survivor.handle().process_session_id();
        primary.handle().fence_stateless_preview_access().await;
        assert_eq!(
            primary
                .handle()
                .stateless_preview_report()
                .await
                .unwrap()
                .write_status,
            AccessStatus::NotPrimary
        );
        primary
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                primary.handle().process_session_id(),
                5,
                "exact-survivor-return",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: configuration.clone(),
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: survivor.identity.clone(),
                        replication_address: format!("http://{}", survivor.address),
                        process_session_id: exact_survivor_session.clone(),
                        storage_incarnation: Some(survivor.storage.clone()),
                        role: ReplicaRole::ActiveSecondary,
                        current_progress: 1,
                        catch_up_capability: 1,
                        must_catchup: false,
                    }],
                }),
            ))
            .await
            .unwrap();
        assert_eq!(
            primary
                .application
                .replicate(Bytes::from_static(b"after-exact-session-return"))
                .await
                .unwrap(),
            2
        );

        primary.handle().fence_stateless_preview_access().await;
        let removal_program = PublicOperationProgram::Remove(PublicBuildInput {
            attempt: OperationId::new("retire-unreachable-survivor"),
            epoch,
            current_configuration_id: None,
            replica: survivor.identity.clone(),
            target_storage: survivor.storage.clone(),
            process_session_id: exact_survivor_session.clone(),
            replication_address: format!("http://{}", survivor.address),
            history: Some(history.clone()),
        });
        let removal_intent = intent(
            &preview,
            primary.handle().process_session_id(),
            6,
            "retire-unreachable-survivor",
            removal_program,
        );
        let accepted = PublicV1AcceptedStatus {
            identity: preview.clone(),
            history_initialization_id: initialization.clone(),
            history: history.clone(),
            data_loss_recovery: None,
            completed_operations: Vec::new(),
            extensions: Default::default(),
        };
        let transition = PublicV1AcceptedTransitionStatus {
            resource_version: "fault-retirement-status-rv-1".into(),
            evidence: accepted,
            intent: PublicV1AcceptedTransitionIntent {
                consumer: PublicV1EvidenceConsumer::Removal,
                operation_id: removal_intent.operation_id.clone(),
                revision: removal_intent.revision,
                call_kind: PublicV1CallKind::Remove,
                history: history.clone(),
                source: PublicV1ResourceAuthority {
                    storage_incarnation: primary.storage.clone(),
                    resource_version: "fault-primary-rv-1".into(),
                },
                target: Some(PublicV1ResourceAuthority {
                    storage_incarnation: survivor.storage.clone(),
                    resource_version: "fault-survivor-rv-1".into(),
                }),
                process_session_id: removal_intent.process_session_id.clone(),
                input_digest: removal_intent.input_digest.clone(),
                replay_disposition: removal_intent.replay_disposition,
                previous_configuration_id: None,
                current_configuration_id: None,
                catch_up_mode: None,
                catch_up_specific_quorum: None,
            },
        };
        let evidence = PublicV1EvidenceState::new(
            transition.clone(),
            vec![
                PublicV1EvidenceResource {
                    storage_incarnation: primary.storage.clone(),
                    resource_version: "fault-primary-rv-1".into(),
                    proof: Some(PublicV1ResourceProof {
                        history: history.clone(),
                        storage_incarnation: primary.storage.clone(),
                    }),
                    disposition: PublicV1ResourceDisposition::Active,
                },
                PublicV1EvidenceResource {
                    storage_incarnation: survivor.storage.clone(),
                    resource_version: "fault-survivor-rv-1".into(),
                    proof: Some(PublicV1ResourceProof {
                        history: history.clone(),
                        storage_incarnation: survivor.storage.clone(),
                    }),
                    disposition: PublicV1ResourceDisposition::Active,
                },
            ],
        )
        .unwrap();
        let completion = PublicV1OperationCompletion {
            operation_id: removal_intent.operation_id.clone(),
            revision: removal_intent.revision,
            call_kind: PublicV1CallKind::Remove,
            process_session_id: removal_intent.process_session_id.clone(),
            input_digest: removal_intent.input_digest.clone(),
            replay_disposition: removal_intent.replay_disposition,
            history: history.clone(),
            storage_incarnation: primary.storage.clone(),
            previous_configuration_id: None,
            current_configuration_id: None,
            catch_up_mode: None,
            catch_up_specific_quorum: None,
            result: PublicV1OperationResult::Ambiguous(
                "survivor did not return in its exact process session".into(),
            ),
        };
        let mut persisted_evidence = transition.evidence.clone();
        persisted_evidence.completed_operations.push(
            kuberic_runtime::protocol::public_operations::
                PublicV1AcceptedOperationCompletion::from_completion(&completion),
        );
        PublicV1EvidenceApi::persist_public_v1_evidence_status(
            &evidence,
            &transition,
            &persisted_evidence,
        )
        .await
        .unwrap();
        let persisted_transition = evidence.transition();
        PublicV1EvidenceApi::retire_public_v1_incarnation(
            &evidence,
            &persisted_transition,
            &completion,
            persisted_transition.intent.target.as_ref().unwrap(),
        )
        .await
        .unwrap();
        let retired = evidence
            .resources()
            .into_iter()
            .find(|resource| resource.storage_incarnation == survivor.storage)
            .unwrap();
        assert_eq!(retired.resource_version, "fault-survivor-rv-1-next");
        assert_eq!(retired.disposition, PublicV1ResourceDisposition::Retired);

        let single = ConfigurationDescriptor::new(
            Epoch::new(0, 2),
            primary.identity.replica_id,
            vec![ConfigurationMember {
                identity: primary.identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        primary
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                primary.handle().process_session_id(),
                6,
                "epoch-after-survivor-retirement",
                PublicOperationProgram::Epoch {
                    epoch: Epoch::new(0, 2),
                },
            ))
            .await
            .unwrap();
        primary
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                primary.handle().process_session_id(),
                7,
                "role-after-survivor-retirement",
                PublicOperationProgram::Role {
                    epoch: Epoch::new(0, 2),
                    role: ReplicaRole::Primary,
                },
            ))
            .await
            .unwrap();
        primary
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                primary.handle().process_session_id(),
                8,
                "configuration-after-survivor-retirement",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: single,
                    previous: None,
                    replicas: Vec::new(),
                }),
            ))
            .await
            .unwrap();
        assert_eq!(
            primary
                .application
                .replicate(Bytes::from_static(b"after-survivor-retirement"))
                .await
                .unwrap(),
            3
        );

        let retired_identity = survivor.identity.clone();
        let retired_progress = *survivor.state.progress.lock().unwrap();
        survivor.stop().await;
        let mut replacement =
            HostedMemoryReplica::start("fault-replacement", &resource_uid, &preview, 2).await;
        replacement
            .initialize_history(&initialization, &history)
            .await;
        replacement
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                replacement.handle().process_session_id(),
                1,
                "open-fault-replacement",
                PublicOperationProgram::Open {
                    replica: replacement.identity.clone(),
                    existing: true,
                },
            ))
            .await
            .unwrap();
        let replacement_before = *replacement.state.progress.lock().unwrap();
        let mut client = proto::replication_data_client::ReplicationDataClient::connect(format!(
            "http://{}",
            replacement.address
        ))
        .await
        .unwrap();
        let stale = proto::ReplicationItem {
            protocol_version: 11,
            sender: Some(primary.identity.clone().into()),
            receiver: Some(retired_identity.into()),
            epoch: Some(epoch.into()),
            previous_configuration_id: String::new(),
            current_configuration_id: configuration.configuration_id.to_string(),
            lsn: retired_progress.applied_lsn + 1,
            committed_lsn: retired_progress.committed_lsn,
            data: b"retired-session-payload".to_vec(),
            sender_session_id: primary.handle().process_session_id().to_string(),
            receiver_session_id: exact_survivor_session.to_string(),
        };
        let mut response = client
            .replicate(tonic::Request::new(stream::iter([stale])))
            .await
            .unwrap()
            .into_inner();
        assert!(response.next().await.unwrap().is_err());
        assert_eq!(
            *replacement.state.progress.lock().unwrap(),
            replacement_before
        );
        assert_eq!(
            primary
                .handle()
                .stateless_preview_report()
                .await
                .unwrap()
                .write_status,
            AccessStatus::Granted
        );

        primary.stop().await;
        replacement.stop().await;
        primary.cleanup();
        survivor.cleanup();
        replacement.cleanup();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn hosted_preview_switchover_dispatches_exact_intent_and_accepts_matching_completion() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let resource_uid = ResourceUid::new("preview-two-replica");
        let preview = PublicOperationPreviewIdentity::public_v1(4);
        let binding = PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let primary_identity = replica_named(1, "primary-pod", "primary-agent");
        let secondary_identity = replica_named(2, "secondary-pod", "secondary-agent");
        let primary_root = test_root("primary");
        let secondary_root = test_root("secondary");
        drop(
            SqliteStore::create_public_v1(
                SqliteStore::metadata_database_path(&primary_root),
                preview.clone(),
                store_identity_named(
                    resource_uid.clone(),
                    primary_identity.clone(),
                    "primary-pod",
                    "primary-pvc",
                ),
            )
            .unwrap(),
        );
        drop(
            SqliteStore::create_public_v1(
                SqliteStore::metadata_database_path(&secondary_root),
                preview.clone(),
                store_identity_named(
                    resource_uid.clone(),
                    secondary_identity.clone(),
                    "secondary-pod",
                    "secondary-pvc",
                ),
            )
            .unwrap(),
        );
        let primary_address = free_address();
        let secondary_address = free_address();
        let primary_state = Arc::new(MemoryState::default());
        let secondary_state = Arc::new(MemoryState::default());
        let primary_app = Arc::new(PreviewApplication::new(
            primary_state.clone(),
            format!("http://{primary_address}"),
        ));
        let secondary_app = Arc::new(PreviewApplication::new(
            secondary_state.clone(),
            format!("http://{secondary_address}"),
        ));
        let mut primary = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("primary-pod"),
                pvc_uid: PvcUid::new("primary-pvc"),
                data_root: primary_root.clone(),
                control_address: free_address(),
                replication_address: primary_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            primary_app.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding.clone())
        .start()
        .await
        .unwrap();
        let mut secondary = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("secondary-pod"),
                pvc_uid: PvcUid::new("secondary-pvc"),
                data_root: secondary_root.clone(),
                control_address: free_address(),
                replication_address: secondary_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            secondary_app.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding)
        .start()
        .await
        .unwrap();
        let primary_handle = primary.handle();
        let secondary_handle = secondary.handle();
        let generation = PublicProtocolGeneration::new(4);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        primary_handle
            .initialize_stateless_preview_history(
                &initialization,
                &history,
                &storage_named(
                    primary_identity.clone(),
                    "primary-pod",
                    "primary-pvc",
                ),
            )
            .await
            .unwrap();
        secondary_handle
            .initialize_stateless_preview_history(
                &initialization,
                &history,
                &storage_named(
                    secondary_identity.clone(),
                    "secondary-pod",
                    "secondary-pvc",
                ),
            )
            .await
            .unwrap();
        let epoch = Epoch::new(0, 1);
        let descriptor = ConfigurationDescriptor::new(
            epoch,
            ReplicaId::new(1),
            vec![
                ConfigurationMember {
                    identity: primary_identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: secondary_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        install_static_authority(
            &primary_handle,
            &preview,
            primary_identity.clone(),
            ReplicaRole::Primary,
            epoch,
            PublicConfiguration {
                current: descriptor.clone(),
                previous: None,
                replicas: vec![PublicReplicaDescriptor {
                    build_id: OperationId::default(),
                    identity: secondary_identity.clone(),
                    replication_address: format!("http://{secondary_address}"),
                    process_session_id: secondary_handle.process_session_id(),
                    storage_incarnation: Some(storage_named(
                        secondary_identity.clone(),
                        "secondary-pod",
                        "secondary-pvc",
                    )),
                    role: ReplicaRole::ActiveSecondary,
                    current_progress: 0,
                    catch_up_capability: 0,
                    must_catchup: false,
                }],
            },
        )
        .await;
        install_static_authority(
            &secondary_handle,
            &preview,
            secondary_identity.clone(),
            ReplicaRole::ActiveSecondary,
            epoch,
            PublicConfiguration {
                current: descriptor.clone(),
                previous: None,
                replicas: vec![PublicReplicaDescriptor {
                    build_id: OperationId::default(),
                    identity: primary_identity.clone(),
                    replication_address: format!("http://{primary_address}"),
                    process_session_id: primary_handle.process_session_id(),
                    storage_incarnation: Some(storage_named(
                        primary_identity.clone(),
                        "primary-pod",
                        "primary-pvc",
                    )),
                    role: ReplicaRole::Primary,
                    current_progress: 0,
                    catch_up_capability: 0,
                    must_catchup: false,
                }],
            },
        )
        .await;
        secondary_state.block_commit();
        let write = tokio::spawn({
            let primary_app = primary_app.clone();
            async move {
                primary_app
                    .replicate(Bytes::from_static(b"replicated"))
                    .await
            }
        });
        secondary_state.commit_entered.notified().await;
        assert_eq!(secondary_state.progress.lock().unwrap().applied_lsn, 1);
        assert_eq!(secondary_state.progress.lock().unwrap().committed_lsn, 0);
        let applied_only = secondary_handle.stateless_preview_report().await.unwrap();
        assert_eq!(
            applied_only.progress.role,
            kuberic_runtime::protocol::public_operations::PublicProgressRole::Recovery
        );
        assert_eq!(applied_only.history, history);
        assert_eq!(applied_only.progress.first, 0);
        assert_eq!(applied_only.progress.last, 1);
        assert_eq!(
            applied_only.read_status,
            kuberic_runtime::protocol::types::AccessStatus::NotPrimary
        );
        secondary_state.release_commit.notify_waiters();
        assert_eq!(write.await.unwrap().unwrap(), 1);
        assert_eq!(primary_state.progress.lock().unwrap().committed_lsn, 1);
        tokio::time::timeout(Duration::from_secs(10), async {
            while secondary_state.progress.lock().unwrap().committed_lsn < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("secondary did not receive committed watermark");
        let committed = secondary_handle.stateless_preview_report().await.unwrap();
        assert_eq!(
            committed.progress.role,
            kuberic_runtime::protocol::public_operations::PublicProgressRole::Secondary
        );
        assert_eq!(committed.history, history);
        assert_eq!(committed.progress.first, 1);
        assert_eq!(committed.progress.last, 1);
        assert_eq!(
            committed.read_status,
            kuberic_runtime::protocol::types::AccessStatus::Granted
        );

        let mut client = kuberic_runtime::control::proto::replication_data_client::ReplicationDataClient::connect(
            format!("http://{secondary_address}"),
        )
        .await
        .unwrap();
        let stale = kuberic_runtime::control::proto::ReplicationItem {
            protocol_version: 11,
            sender: Some(primary_identity.clone().into()),
            receiver: Some(secondary_identity.clone().into()),
            epoch: Some(epoch.into()),
            previous_configuration_id: String::new(),
            current_configuration_id: descriptor.configuration_id.to_string(),
            lsn: 2,
            committed_lsn: 1,
            data: b"stale".to_vec(),
            sender_session_id: "predecessor-session".into(),
            receiver_session_id: secondary_handle.process_session_id().to_string(),
        };
        let mut response = client
            .replicate(tonic::Request::new(stream::iter([stale])))
            .await
            .unwrap()
            .into_inner();
        assert!(response.next().await.unwrap().is_err());
        assert_eq!(secondary_state.progress.lock().unwrap().applied_lsn, 1);

        secondary_state.fail_next_commit();
        assert_eq!(
            primary_app
                .replicate(Bytes::from_static(b"lost-watermark"))
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            *primary_state.progress.lock().unwrap(),
            DurableApplicationProgress {
                applied_lsn: 2,
                committed_lsn: 2,
            }
        );
        assert_eq!(
            *secondary_state.progress.lock().unwrap(),
            DurableApplicationProgress {
                applied_lsn: 2,
                committed_lsn: 1,
            }
        );
        let lost = secondary_handle.stateless_preview_report().await.unwrap();
        assert_eq!(lost.progress.first, 1);
        assert_eq!(lost.progress.last, 2);
        assert_eq!(
            lost.read_status,
            kuberic_runtime::protocol::types::AccessStatus::NotPrimary
        );

        let refreshed = ConfigurationDescriptor::new(
            Epoch::new(0, 2),
            primary_identity.replica_id,
            descriptor.members.clone(),
            2,
        );
        primary_handle
            .execute_stateless_preview(intent(
                &preview,
                primary_handle.process_session_id(),
                5,
                "switchover-pre-catchup",
                PublicOperationProgram::CatchUp {
                    configuration: PublicConfiguration {
                        current: descriptor.clone(),
                        previous: None,
                        replicas: vec![PublicReplicaDescriptor {
                            build_id: OperationId::default(),
                            identity: secondary_identity.clone(),
                            replication_address: format!("http://{secondary_address}"),
                            process_session_id: secondary_handle.process_session_id(),
                            storage_incarnation: Some(storage_named(
                                secondary_identity.clone(),
                                "secondary-pod",
                                "secondary-pvc",
                            )),
                            role: ReplicaRole::ActiveSecondary,
                            current_progress: 1,
                            catch_up_capability: 1,
                            must_catchup: true,
                        }],
                    },
                    mode: kuberic_runtime::protocol::public_operations::PublicCatchUpMode::
                        WriteQuorum,
                },
            ))
            .await
            .unwrap();
        assert_eq!(secondary_state.progress.lock().unwrap().committed_lsn, 2);
            secondary_state.fail_next_commit();
            assert_eq!(
                primary_app
                    .replicate(Bytes::from_static(b"intervening-before-composed-swap"))
                    .await
                    .unwrap(),
                3
            );
            assert_eq!(
                *secondary_state.progress.lock().unwrap(),
                DurableApplicationProgress {
                    applied_lsn: 3,
                    committed_lsn: 2,
                },
                "designated successor was not lagging at composed swap admission"
            );
            let starting_secondary_descriptor = || PublicReplicaDescriptor {
            build_id: OperationId::default(),
            identity: secondary_identity.clone(),
            replication_address: format!("http://{secondary_address}"),
            process_session_id: secondary_handle.process_session_id(),
            storage_incarnation: Some(storage_named(
                secondary_identity.clone(),
                "secondary-pod",
                "secondary-pvc",
            )),
            role: ReplicaRole::ActiveSecondary,
            current_progress: 2,
            catch_up_capability: 2,
            must_catchup: true,
        };
        let refreshed_configuration_id = refreshed.configuration_id.clone();
        let switchover = intent(
            &preview,
            primary_handle.process_session_id(),
            7,
            "planned-switchover",
            PublicOperationProgram::Swap {
                starting: PublicConfiguration {
                    current: descriptor.clone(),
                    previous: None,
                    replicas: vec![starting_secondary_descriptor()],
                },
                refreshed: PublicConfiguration {
                    current: refreshed,
                    previous: None,
                    replicas: vec![starting_secondary_descriptor()],
                },
                epoch: Epoch::new(0, 2),
                handoff: ReplicaRole::ActiveSecondary,
                catch_up_specific_quorum: true,
                mode: kuberic_runtime::protocol::public_operations::PublicCatchUpMode::WriteQuorum,
                controller_composed: true,
            },
        );
        let accepted = PublicV1AcceptedStatus {
            identity: preview.clone(),
            history_initialization_id: initialization.clone(),
            history: history.clone(),
            data_loss_recovery: None,
            completed_operations: Vec::new(),
            extensions: Default::default(),
        };
        let source_storage = storage_named(
            primary_identity.clone(),
            "primary-pod",
            "primary-pvc",
        );
        let transition = PublicV1AcceptedTransitionStatus {
            resource_version: "switchover-status-1".into(),
            evidence: accepted.clone(),
            intent: PublicV1AcceptedTransitionIntent {
                consumer: PublicV1EvidenceConsumer::Switchover,
                operation_id: switchover.operation_id.clone(),
                revision: switchover.revision,
                call_kind: PublicV1CallKind::Swap,
                history: history.clone(),
                source: PublicV1ResourceAuthority {
                    storage_incarnation: source_storage.clone(),
                    resource_version: "primary-rv-1".into(),
                },
                target: Some(PublicV1ResourceAuthority {
                    storage_incarnation: storage_named(
                        secondary_identity.clone(),
                        "secondary-pod",
                        "secondary-pvc",
                    ),
                    resource_version: "secondary-rv-1".into(),
                }),
                process_session_id: switchover.process_session_id.clone(),
                input_digest: switchover.input_digest.clone(),
                replay_disposition: switchover.replay_disposition,
                previous_configuration_id: None,
                current_configuration_id: Some(refreshed_configuration_id),
                catch_up_mode: Some(
                    kuberic_runtime::protocol::public_operations::PublicCatchUpMode::WriteQuorum,
                ),
                catch_up_specific_quorum: Some(true),
            },
        };
        let evidence = Arc::new(
            PublicV1EvidenceState::new(
                transition.clone(),
                vec![
                    PublicV1EvidenceResource {
                        storage_incarnation: source_storage.clone(),
                        resource_version: "primary-rv-1".into(),
                        proof: Some(PublicV1ResourceProof {
                            history: history.clone(),
                            storage_incarnation: source_storage.clone(),
                        }),
                        disposition: PublicV1ResourceDisposition::Active,
                    },
                    PublicV1EvidenceResource {
                        storage_incarnation: storage_named(
                            secondary_identity.clone(),
                            "secondary-pod",
                            "secondary-pvc",
                        ),
                        resource_version: "secondary-rv-1".into(),
                        proof: Some(PublicV1ResourceProof {
                            history: history.clone(),
                            storage_incarnation: storage_named(
                                secondary_identity.clone(),
                                "secondary-pod",
                                "secondary-pvc",
                            ),
                        }),
                        disposition: PublicV1ResourceDisposition::Active,
                    },
                ],
            )
            .unwrap(),
        );
        let hosted_replicas = || {
            vec![
                HostedPreviewReplica {
                    identity: primary_identity.clone(),
                    replication_address: format!("http://{primary_address}"),
                    handle: primary_handle.clone(),
                },
                HostedPreviewReplica {
                    identity: secondary_identity.clone(),
                    replication_address: format!("http://{secondary_address}"),
                    handle: secondary_handle.clone(),
                },
            ]
        };
        let adapter = HostedPublicV1Adapter::new(
            hosted_replicas(),
            Some(accepted.clone()),
            None,
            evidence.clone(),
        )
        .unwrap();
        let command = ProtocolCommand::PublicV1Operation(Box::new(PublicV1OperationCommand {
            target: primary_identity.clone(),
            intent: switchover.clone(),
        }));
        let wrong_target =
            ProtocolCommand::PublicV1Operation(Box::new(PublicV1OperationCommand {
                target: secondary_identity.clone(),
                intent: switchover,
            }));
        let error = PublicV1OperationApi::dispatch_public_v1_operation(
            &adapter,
            &transition,
            &wrong_target,
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("target differs from accepted source incarnation"),
            "{error}"
        );
        assert!(adapter.dispatched().is_empty());
        assert!(evidence.effects().is_empty());
        assert!(
            adapter
                .report(&primary_identity)
                .await
                .unwrap()
                .completed_operations
                .iter()
                .all(|completion| completion.operation_id != transition.intent.operation_id)
        );
        adapter.cut_after_hosted_suboperations(3);
        assert!(
            execute_public_v1_operation(&adapter, &transition, command.clone())
                .await
                .unwrap_err()
                .contains("injected cut after hosted suboperation completion")
        );
        assert_eq!(
            primary_handle
                .stateless_preview_report()
                .await
                .unwrap()
                .write_status,
            kuberic_runtime::protocol::types::AccessStatus::Granted
        );
        assert_eq!(
            secondary_handle
                .stateless_preview_report()
                .await
                .unwrap()
                .write_status,
            kuberic_runtime::protocol::types::AccessStatus::NotPrimary
        );
        let partial_configuration_id =
            OperationId::new("planned-switchover-target-configuration-2");
        let partial_configuration = secondary_handle
            .stateless_preview_operation_intent(&partial_configuration_id)
            .await
            .unwrap()
            .expect("partial switchover configuration was not durable");
        drop(adapter);
        let adapter = HostedPublicV1Adapter::new(
            hosted_replicas(),
            Some(accepted.clone()),
            None,
            evidence.clone(),
        )
        .unwrap();
        execute_public_v1_operation(&adapter, &transition, command)
            .await
            .unwrap();
        assert_eq!(
            secondary_handle
                .stateless_preview_operation_intent(&partial_configuration_id)
                .await
                .unwrap(),
            Some(partial_configuration),
            "adapter recreation changed the durable switchover configuration intent"
        );
        let switchover_report = adapter.report(&primary_identity).await.unwrap();
        assert_eq!(
            reconcile_public_v1_consumer(&adapter, &switchover_report)
                .await
                .unwrap(),
            kuberic_controller::evaluator::test_bridge::PublicV1ReconciliationOutcome::
                EvidencePersisted
        );
        assert_eq!(
            reconcile_public_v1_consumer(&adapter, &switchover_report)
                .await
                .unwrap(),
            kuberic_controller::evaluator::test_bridge::PublicV1ReconciliationOutcome::
                EffectApplied
        );
        assert_eq!(evidence.effects().len(), 1);
        assert_eq!(secondary_state.progress.lock().unwrap().committed_lsn, 3);
        assert_eq!(
            primary_handle
                .stateless_preview_report()
                .await
                .unwrap()
                .write_status,
            kuberic_runtime::protocol::types::AccessStatus::NotPrimary
        );
        assert_eq!(
            secondary_handle
                .stateless_preview_report()
                .await
                .unwrap()
                .write_status,
            kuberic_runtime::protocol::types::AccessStatus::Granted
        );
        let next_revision = secondary_handle
            .stateless_preview_next_operation_revision()
            .await
            .unwrap();
        secondary_handle
            .execute_stateless_preview(intent(
                &preview,
                secondary_handle.process_session_id(),
                next_revision,
                "same-session-after-composed-swap",
                PublicOperationProgram::Progress { capability: true },
            ))
            .await
            .unwrap();
        assert_eq!(
            secondary_app
                .replicate(Bytes::from_static(b"after-switchover"))
                .await
                .unwrap(),
            4
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while primary_state.progress.lock().unwrap().committed_lsn < 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("new primary write did not reach old primary");
        let primary_predecessor = primary_handle
            .stateless_preview_process_endpoint(ReplicaRole::Primary)
            .await
            .unwrap();
        let secondary_predecessor = secondary_handle
            .stateless_preview_process_endpoint(ReplicaRole::ActiveSecondary)
            .await
            .unwrap();

        secondary.shutdown();
        secondary.wait().await.unwrap();
        assert!(
            primary_app
                .replicate(Bytes::from_static(b"after-close"))
                .await
                .is_err()
        );
        assert_eq!(
            *primary_state.progress.lock().unwrap(),
            DurableApplicationProgress {
                applied_lsn: 4,
                committed_lsn: 4,
            }
        );
        std::net::TcpListener::bind(secondary_address).unwrap();
        primary.shutdown();
        primary.wait().await.unwrap();
        let primary_restart_app = Arc::new(PreviewApplication::new(
            primary_state.clone(),
            format!("http://{primary_address}"),
        ));
        let secondary_restart_app = Arc::new(PreviewApplication::new(
            secondary_state.clone(),
            format!("http://{secondary_address}"),
        ));
        let restart_binding = PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let mut restarted_primary = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("primary-pod"),
                pvc_uid: PvcUid::new("primary-pvc"),
                data_root: primary_root.clone(),
                control_address: free_address(),
                replication_address: primary_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            primary_restart_app.clone(),
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_stateless_preview(restart_binding.clone())
        .start()
        .await
        .unwrap();
        let mut restarted_secondary = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("secondary-pod"),
                pvc_uid: PvcUid::new("secondary-pvc"),
                data_root: secondary_root.clone(),
                control_address: free_address(),
                replication_address: secondary_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            secondary_restart_app.clone(),
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_stateless_preview(restart_binding)
        .start()
        .await
        .unwrap();
        let restarted_primary_handle = restarted_primary.handle();
        let restarted_secondary_handle = restarted_secondary.handle();
        assert_eq!(
            restarted_primary_handle.stateless_preview_provider_writer_count(),
            0
        );
        assert_eq!(
            restarted_secondary_handle.stateless_preview_provider_writer_count(),
            0
        );
        assert_eq!(primary_restart_app.opens.load(Ordering::SeqCst), 0);
        assert_eq!(secondary_restart_app.opens.load(Ordering::SeqCst), 0);
        assert_eq!(
            restarted_primary_handle
                .stateless_preview_report()
                .await
                .unwrap()
                .write_status,
            AccessStatus::NotPrimary
        );
        assert_eq!(
            restarted_secondary_handle
                .stateless_preview_report()
                .await
                .unwrap()
                .write_status,
            AccessStatus::NotPrimary
        );
        std::net::TcpListener::bind(primary_address).unwrap();
        std::net::TcpListener::bind(secondary_address).unwrap();

        let failover_epoch = Epoch::new(0, 3);
        let renewal_configuration = ConfigurationDescriptor::new(
            failover_epoch,
            ReplicaId::new(1),
            vec![
                ConfigurationMember {
                    identity: primary_identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: secondary_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        let promotion_epoch = Epoch::new(0, 4);
        let failover_configuration = ConfigurationDescriptor::new(
            promotion_epoch,
            ReplicaId::new(2),
            vec![
                ConfigurationMember {
                    identity: primary_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: secondary_identity.clone(),
                    role: ReplicaRole::Primary,
                },
            ],
            2,
        );
        let hosted_replicas = || {
            vec![
                HostedPreviewReplica {
                    identity: primary_identity.clone(),
                    replication_address: format!("http://{primary_address}"),
                    handle: restarted_primary_handle.clone(),
                },
                HostedPreviewReplica {
                    identity: secondary_identity.clone(),
                    replication_address: format!("http://{secondary_address}"),
                    handle: restarted_secondary_handle.clone(),
                },
            ]
        };
        let single_primary_configuration = ConfigurationDescriptor::new(
            failover_epoch,
            ReplicaId::new(1),
            vec![ConfigurationMember {
                identity: primary_identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        let primary_replacement = ProcessSessionEndpoint {
            identity: primary_predecessor.identity.clone(),
            storage_incarnation: primary_predecessor.storage_incarnation.clone(),
            process_session_id: restarted_primary_handle.process_session_id(),
            replicator_creation_id: ReplicatorCreationId::new("renew-primary-process-creation"),
            role: ReplicaRole::None,
        };
        let primary_renewal = HostedPublicV1Adapter::new(
            hosted_replicas(),
            Some(transition.evidence.clone()),
            None,
            evidence.clone(),
        )
        .unwrap();
        let primary_report = restarted_primary_handle
            .stateless_preview_report()
            .await
            .unwrap();
        execute_public_v1_session_renewal(
            &primary_renewal,
            accept_public_v1_session_renewal(
                None,
                ProcessSessionRenewalRequired {
                    operation_id: OperationId::new("renew-primary-process"),
                    reason: ProcessSessionRenewalReason::PrimaryReplacement,
                    resource_uid: resource_uid.clone(),
                    history: history.clone(),
                    previous_epoch: Epoch::new(0, 2),
                    renewal_epoch: failover_epoch,
                    predecessor: primary_predecessor,
                    replacement: Some(primary_replacement),
                    surviving_sessions: vec![secondary_predecessor.clone()],
                    fresh_configuration: single_primary_configuration.clone(),
                    predecessor_contained: true,
                    data_loss: None,
                },
                Some(&primary_report),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        drive_session_renewal(
            &primary_renewal,
            std::slice::from_ref(&secondary_predecessor.process_session_id),
            true,
        )
        .await;
        primary_renewal
            .reconstruct_hosted_configuration(
                &OperationId::new("reconstruct-primary-after-renewal"),
                preview.generation,
                &single_primary_configuration,
            )
            .await
            .unwrap();
        let renewed_primary = restarted_primary_handle
            .stateless_preview_process_endpoint(ReplicaRole::Primary)
            .await
            .unwrap();

        let secondary_replacement = ProcessSessionEndpoint {
            identity: secondary_predecessor.identity.clone(),
            storage_incarnation: secondary_predecessor.storage_incarnation.clone(),
            process_session_id: restarted_secondary_handle.process_session_id(),
            replicator_creation_id: ReplicatorCreationId::new("renew-secondary-process-creation"),
            role: ReplicaRole::None,
        };
        let secondary_renewal = HostedPublicV1Adapter::new(
            hosted_replicas(),
            Some(transition.evidence.clone()),
            None,
            evidence.clone(),
        )
        .unwrap();
        let secondary_report = restarted_secondary_handle
            .stateless_preview_report()
            .await
            .unwrap();
        execute_public_v1_session_renewal(
            &secondary_renewal,
            accept_public_v1_session_renewal(
                None,
                ProcessSessionRenewalRequired {
                    operation_id: OperationId::new("renew-secondary-process"),
                    reason: ProcessSessionRenewalReason::ActiveSecondaryReplacement,
                    resource_uid: resource_uid.clone(),
                    history: history.clone(),
                    previous_epoch: Epoch::new(0, 2),
                    renewal_epoch: failover_epoch,
                    predecessor: secondary_predecessor,
                    replacement: Some(secondary_replacement),
                    surviving_sessions: vec![renewed_primary],
                    fresh_configuration: renewal_configuration.clone(),
                    predecessor_contained: true,
                    data_loss: None,
                },
                Some(&secondary_report),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        loop {
            let accepted = secondary_renewal
                .observe_public_v1_session_renewal()
                .await
                .unwrap()
                .unwrap();
            let plan = plan_public_v1_session_renewal(&accepted, &[]).unwrap();
            let is_epoch_dispatch = matches!(
                &plan,
                PublicV1SessionRenewalPlan::Dispatch { command, .. }
                    if matches!(
                        command,
                        ProtocolCommand::PublicV1History(public)
                            if matches!(
                                public.as_ref(),
                                kuberic_runtime::protocol::command::PublicV1HistoryCommand::
                                    SessionRenewal(renewal)
                                    if matches!(
                                        renewal.as_ref(),
                                        kuberic_runtime::protocol::command::
                                            PublicV1SessionRenewalCommand::UpdateEpochBarrier { .. }
                                    )
                            )
                    )
            );
            if is_epoch_dispatch {
                secondary_renewal.cut_after_hosted_suboperations(1);
                assert!(
                    execute_public_v1_session_renewal(&secondary_renewal, plan.clone())
                        .await
                        .unwrap_err()
                        .contains("injected cut after hosted suboperation completion")
                );
                assert_eq!(
                    secondary_renewal
                        .observe_public_v1_session_renewal()
                        .await
                        .unwrap(),
                    Some(accepted.clone())
                );
                let barrier_id = OperationId::new("renew-secondary-process-barrier-1");
                let durable_barrier = restarted_primary_handle
                    .stateless_preview_operation_intent(&barrier_id)
                    .await
                    .unwrap()
                    .expect("renewal barrier was not durable at the injected cut");
                drop(secondary_renewal);
                let secondary_renewal = HostedPublicV1Adapter::new(
                    hosted_replicas(),
                    Some(transition.evidence.clone()),
                    Some(accepted),
                    evidence.clone(),
                )
                .unwrap();
                execute_public_v1_session_renewal(&secondary_renewal, plan)
                    .await
                    .unwrap();
                assert_eq!(
                    restarted_primary_handle
                        .stateless_preview_operation_intent(&barrier_id)
                        .await
                        .unwrap(),
                    Some(durable_barrier),
                    "adapter recreation changed the durable renewal barrier intent"
                );
                drive_session_renewal(&secondary_renewal, &[], false).await;
                break;
            }
            execute_public_v1_session_renewal(&secondary_renewal, plan)
                .await
                .unwrap();
        }
        let primary_restart_progress = primary_state.progress.lock().unwrap().committed_lsn;
        let failover_lifecycle = PublicLifecycleInput {
            recipe: PublicLifecycleRecipe::FailoverPromotion,
            replica: secondary_identity.clone(),
            epoch: promotion_epoch,
            possible_data_loss: PossibleDataLossIntent::NotPossible,
            current: failover_configuration.clone(),
            previous: Some(renewal_configuration.clone()),
            replicas: vec![PublicReplicaDescriptor {
                build_id: OperationId::default(),
                identity: primary_identity.clone(),
                replication_address: format!("http://{primary_address}"),
                process_session_id: restarted_primary_handle.process_session_id(),
                storage_incarnation: Some(storage_named(
                    primary_identity.clone(),
                    "primary-pod",
                    "primary-pvc",
                )),
                role: ReplicaRole::ActiveSecondary,
                current_progress: primary_restart_progress,
                catch_up_capability: primary_restart_progress,
                must_catchup: true,
            }],
            data_loss_recovery: None,
        };
        let failover_revision = restarted_secondary_handle
            .stateless_preview_next_operation_revision()
            .await
            .unwrap();
        let failover_intent = plan_public_lifecycle(
            &PublicOperationPreviewEvaluationConfig::new(
                preview.clone(),
                Default::default(),
            ),
            OperationId::new("controller-restart-failover"),
            failover_revision,
            restarted_secondary_handle.process_session_id(),
            failover_lifecycle,
            PreviewTransition::Ordinary,
        )
        .unwrap();
        let failover_accepted = PublicV1AcceptedStatus {
            identity: preview.clone(),
            history_initialization_id: initialization.clone(),
            history: history.clone(),
            data_loss_recovery: None,
            completed_operations: Vec::new(),
            extensions: Default::default(),
        };
        let failover_source_storage = storage_named(
            secondary_identity.clone(),
            "secondary-pod",
            "secondary-pvc",
        );
        let failover_transition = PublicV1AcceptedTransitionStatus {
            resource_version: "restart-failover-status-1".into(),
            evidence: failover_accepted.clone(),
            intent: PublicV1AcceptedTransitionIntent {
                consumer: PublicV1EvidenceConsumer::Failover,
                operation_id: failover_intent.operation_id.clone(),
                revision: failover_intent.revision,
                call_kind: PublicV1CallKind::FailoverPromotion,
                history: history.clone(),
                source: PublicV1ResourceAuthority {
                    storage_incarnation: failover_source_storage.clone(),
                    resource_version: "restart-secondary-rv-1".into(),
                },
                target: None,
                process_session_id: failover_intent.process_session_id.clone(),
                input_digest: failover_intent.input_digest.clone(),
                replay_disposition: failover_intent.replay_disposition,
                previous_configuration_id: Some(
                    renewal_configuration.configuration_id.clone(),
                ),
                current_configuration_id: Some(
                    failover_configuration.configuration_id.clone(),
                ),
                catch_up_mode: None,
                catch_up_specific_quorum: None,
            },
        };
        let failover_evidence = Arc::new(
            PublicV1EvidenceState::new(
                failover_transition.clone(),
                vec![PublicV1EvidenceResource {
                    storage_incarnation: failover_source_storage.clone(),
                    resource_version: "restart-secondary-rv-1".into(),
                    proof: Some(PublicV1ResourceProof {
                        history: history.clone(),
                        storage_incarnation: failover_source_storage,
                    }),
                    disposition: PublicV1ResourceDisposition::Active,
                }],
            )
            .unwrap(),
        );
        let restart_adapter = HostedPublicV1Adapter::new(
            vec![
                HostedPreviewReplica {
                    identity: primary_identity.clone(),
                    replication_address: format!("http://{primary_address}"),
                    handle: restarted_primary_handle.clone(),
                },
                HostedPreviewReplica {
                    identity: secondary_identity.clone(),
                    replication_address: format!("http://{secondary_address}"),
                    handle: restarted_secondary_handle.clone(),
                },
            ],
            Some(failover_accepted),
            None,
            failover_evidence.clone(),
        )
        .unwrap();
        let failover_command =
            ProtocolCommand::PublicV1Operation(Box::new(PublicV1OperationCommand {
                target: secondary_identity.clone(),
                intent: failover_intent,
            }));
        assert_eq!(
            reconcile_public_v1_operation(&restart_adapter, failover_command.clone())
                .await
                .unwrap(),
            kuberic_controller::evaluator::test_bridge::PublicV1ReconciliationOutcome::
                EvidencePersisted
        );
        assert_eq!(
            reconcile_public_v1_operation(&restart_adapter, failover_command)
                .await
                .unwrap(),
            kuberic_controller::evaluator::test_bridge::PublicV1ReconciliationOutcome::
                EffectApplied
        );
        assert_eq!(failover_evidence.effects().len(), 1);
        let old_primary_report = restarted_primary_handle
            .stateless_preview_report()
            .await
            .unwrap();
        let new_primary_report = restarted_secondary_handle
            .stateless_preview_report()
            .await
            .unwrap();
        assert_eq!(
            old_primary_report.write_status,
            kuberic_runtime::protocol::types::AccessStatus::NotPrimary
        );
        assert_eq!(
            new_primary_report.write_status,
            kuberic_runtime::protocol::types::AccessStatus::Granted
        );
        assert!(
            primary_restart_app
                .replicate(Bytes::from_static(b"fenced-restart-predecessor"))
                .await
                .is_err()
        );
        assert_eq!(
            secondary_restart_app
                .replicate(Bytes::from_static(b"after-all-process-restart"))
                .await
                .unwrap(),
            5
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while primary_state.progress.lock().unwrap().committed_lsn < 5 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("restart failover write did not reach secondary");
        assert_eq!(secondary_state.progress.lock().unwrap().committed_lsn, 5);

        restarted_primary.shutdown();
        restarted_secondary.shutdown();
        restarted_primary.wait().await.unwrap();
        restarted_secondary.wait().await.unwrap();
        std::fs::remove_dir_all(primary_root).unwrap();
        std::fs::remove_dir_all(secondary_root).unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn hosted_preview_build_dispatches_exact_attempt_and_waits_for_matching_completion() {
    tokio::time::timeout(Duration::from_secs(150), async {
        let resource_uid = ResourceUid::new("preview-build");
        let preview = PublicOperationPreviewIdentity::public_v1(8);
        let binding = PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let source_identity = replica_named(1, "build-source-pod", "build-source-agent");
        let target_identity = replica_named(2, "build-target-pod", "build-target-agent");
        let source_root = test_root("build-source");
        let target_root = test_root("build-target");
        drop(
            SqliteStore::create_public_v1(
                SqliteStore::metadata_database_path(&source_root),
                preview.clone(),
                store_identity_named(
                    resource_uid.clone(),
                    source_identity.clone(),
                    "build-source-pod",
                    "build-source-pvc",
                ),
            )
            .unwrap(),
        );
        drop(
            SqliteStore::create_public_v1(
                SqliteStore::metadata_database_path(&target_root),
                preview.clone(),
                store_identity_named(
                    resource_uid.clone(),
                    target_identity.clone(),
                    "build-target-pod",
                    "build-target-pvc",
                ),
            )
            .unwrap(),
        );
        let source_address = free_address();
        let target_address = free_address();
        let source_state = Arc::new(MemoryState::default());
        let target_state = Arc::new(MemoryState::default());
        let source_app = Arc::new(PreviewApplication::new(
            source_state.clone(),
            format!("http://{source_address}"),
        ));
        let target_app = Arc::new(PreviewApplication::new(
            target_state.clone(),
            format!("http://{target_address}"),
        ));
        let mut source = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("build-source-pod"),
                pvc_uid: PvcUid::new("build-source-pvc"),
                data_root: source_root.clone(),
                control_address: free_address(),
                replication_address: source_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            source_app.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding.clone())
        .start()
        .await
        .unwrap();
        let mut target = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("build-target-pod"),
                pvc_uid: PvcUid::new("build-target-pvc"),
                data_root: target_root.clone(),
                control_address: free_address(),
                replication_address: target_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            target_app,
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding)
        .start()
        .await
        .unwrap();
        let source_handle = source.handle();
        let target_handle = target.handle();
        let generation = PublicProtocolGeneration::new(8);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        source_handle
            .initialize_stateless_preview_history(
                &initialization,
                &history,
                &storage_named(
                    source_identity.clone(),
                    "build-source-pod",
                    "build-source-pvc",
                ),
            )
            .await
            .unwrap();
        target_handle
            .initialize_stateless_preview_history(
                &initialization,
                &history,
                &storage_named(
                    target_identity.clone(),
                    "build-target-pod",
                    "build-target-pvc",
                ),
            )
            .await
            .unwrap();
        let epoch = Epoch::new(0, 1);
        let source_configuration = ConfigurationDescriptor::new(
            epoch,
            ReplicaId::new(1),
            vec![ConfigurationMember {
                identity: source_identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        install_static_authority(
            &source_handle,
            &preview,
            source_identity.clone(),
            ReplicaRole::Primary,
            epoch,
            PublicConfiguration {
                current: source_configuration.clone(),
                previous: None,
                replicas: Vec::new(),
            },
        )
        .await;
        let target_session = target_handle.process_session_id();
        for (revision, name, program) in [
            (
                1,
                "open-build-target",
                PublicOperationProgram::Open {
                    replica: target_identity.clone(),
                    existing: true,
                },
            ),
            (
                2,
                "role-build-target",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
            (
                3,
                "epoch-build-target",
                PublicOperationProgram::Epoch { epoch },
            ),
        ] {
            target_handle
                .execute_stateless_preview(intent(
                    &preview,
                    target_session.clone(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        assert_eq!(
            source_app
                .replicate(Bytes::from_static(b"snapshot-value"))
                .await
                .unwrap(),
            1
        );
        source_state
            .copy_chunks
            .lock()
            .unwrap()
            .push(Bytes::from_static(b"snapshot-bytes"));
        let build_input = PublicBuildInput {
            attempt: OperationId::new("preview-build-attempt"),
            epoch,
            current_configuration_id: Some(source_configuration.configuration_id.clone()),
            replica: target_identity.clone(),
            target_storage: storage_named(
                target_identity.clone(),
                "build-target-pod",
                "build-target-pvc",
            ),
            process_session_id: target_session.clone(),
            replication_address: format!("http://{target_address}"),
            history: Some(history.clone()),
        };
        let source_storage = storage_named(
            source_identity.clone(),
            "build-source-pod",
            "build-source-pvc",
        );
        let target_storage = build_input.target_storage.clone();
        let build_authority =
            |operation_id: &str, operation_revision, build_id: &str| PublicInboundBuildAuthority {
                operation_id: OperationId::new(operation_id),
                operation_revision,
                attempt: PublicBuildAttemptReference {
                    resource_uid: resource_uid.clone(),
                    epoch,
                    build_id: OperationId::new(build_id),
                    source: source_identity.clone(),
                    source_storage: source_storage.clone(),
                    source_session: source_handle.process_session_id(),
                    target: target_identity.clone(),
                    target_storage: target_storage.clone(),
                    target_session: target_session.clone(),
                },
                history: history.clone(),
                current_configuration_id: source_configuration.configuration_id.clone(),
                target_address: format!("http://{target_address}"),
            };
        let authority_classified = build_authority(
            "classified-configuration-successor",
            1,
            "classified-successor",
        );
        let authority_a = build_authority("superseded-build-a", 2, "superseded-build-a");
        let authority_b = build_authority("held-build-b", 3, "held-build-b");
        let copy_frame = |authority: &PublicInboundBuildAuthority,
                          sequence,
                          snapshot_chunk,
                          final_item| proto::CopyItem {
            protocol_version: 11,
            build_id: authority.attempt.build_id.to_string(),
            sender: Some(source_identity.clone().into()),
            receiver: Some(target_identity.clone().into()),
            epoch: Some(epoch.into()),
            current_configuration_id: source_configuration.configuration_id.to_string(),
            sequence,
            lsn: 1,
            committed_lsn: 1,
            replication_boundary_lsn: 1,
            final_item,
            data: if snapshot_chunk {
                b"snapshot-bytes".to_vec()
            } else {
                Vec::new()
            },
            snapshot_chunk,
            sender_session_id: source_handle.process_session_id().to_string(),
            receiver_session_id: target_session.to_string(),
            catch_up_boundary_lsn: final_item.then_some(1),
            source_storage: Some(storage_proto(&source_storage)),
            target_storage: Some(storage_proto(&target_storage)),
            history: Some(history_proto(&history)),
            target_address: format!("http://{target_address}"),
        };
        let classification_pause = testing_pause_inbound_build_after_classification(
            authority_classified.operation_id.clone(),
        );
        let classified_install = tokio::spawn({
            let target_handle = target_handle.clone();
            let authority = authority_classified.clone();
            async move {
                target_handle
                    .install_stateless_preview_inbound_build_authority(&authority)
                    .await
            }
        });
        classification_pause.wait_until_classified().await;
        let target_configuration = ConfigurationDescriptor::new(
            epoch,
            source_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source_identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: target_identity.clone(),
                    role: ReplicaRole::IdleSecondary,
                },
            ],
            1,
        );
        let boundary_source_identity = source_identity.clone();
        let boundary_source_session = source_handle.process_session_id();
        let boundary_source_storage = source_storage.clone();
        let configuration_boundary = tokio::spawn({
            let target_handle = target_handle.clone();
            let preview = preview.clone();
            let target_session = target_session.clone();
            async move {
                target_handle
                    .execute_stateless_preview(intent(
                        &preview,
                        target_session,
                        4,
                        "classified-successor-configuration-boundary",
                        PublicOperationProgram::Configuration(PublicConfiguration {
                            current: target_configuration,
                            previous: None,
                            replicas: vec![PublicReplicaDescriptor {
                                build_id: OperationId::default(),
                                identity: boundary_source_identity,
                                replication_address: format!("http://{source_address}"),
                                process_session_id: boundary_source_session,
                                storage_incarnation: Some(boundary_source_storage),
                                role: ReplicaRole::Primary,
                                current_progress: 1,
                                catch_up_capability: 1,
                                must_catchup: false,
                            }],
                        }),
                    ))
                    .await
            }
        });
        classification_pause.wait_until_boundary_waiting().await;
        classification_pause.release();
        classified_install.await.unwrap().unwrap();
        configuration_boundary.await.unwrap().unwrap();

        let connection =
            rusqlite::Connection::open(SqliteStore::metadata_database_path(&target_root)).unwrap();
        let json: String = connection
            .query_row(
                "SELECT state_json FROM agent_state WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let durable: serde_json::Value = serde_json::from_str(&json).unwrap();
        let preview_state = &durable["publicOperationPreview"];
        assert!(
            preview_state["currentInboundBuildAuthority"].is_null(),
            "classified successor survived completed configuration revocation"
        );
        assert_eq!(preview_state["inboundBuildGeneration"], 1);
        drop(connection);

        let mut client = proto::replication_data_client::ReplicationDataClient::connect(format!(
            "http://{target_address}"
        ))
        .await
        .unwrap();
        let mut stale_classified = client
            .build(Request::new(stream::iter([copy_frame(
                &authority_classified,
                0,
                true,
                false,
            )])))
            .await
            .unwrap()
            .into_inner();
        assert!(
            stale_classified.next().await.unwrap().is_err(),
            "revoked classified successor retained an inbound task or cursor"
        );
        assert_eq!(
            *target_state.progress.lock().unwrap(),
            DurableApplicationProgress::default()
        );
        assert!(target_state.copy_chunks.lock().unwrap().is_empty());
        assert!(target_state.operations.lock().unwrap().is_empty());

        target_handle
            .install_stateless_preview_inbound_build_authority(&authority_a)
            .await
            .unwrap();
        let mut client = proto::replication_data_client::ReplicationDataClient::connect(format!(
            "http://{target_address}"
        ))
        .await
        .unwrap();
        let mut first_a = client
            .build(Request::new(stream::iter([copy_frame(
                &authority_a,
                0,
                true,
                false,
            )])))
            .await
            .unwrap()
            .into_inner();
        assert!(
            !first_a.next().await.unwrap().unwrap().final_item,
            "fresh post-boundary authority did not start from an empty cursor"
        );

        target_state.block_finish_copy();
        let held_finish = tokio::spawn({
            let frame = copy_frame(&authority_a, 1, false, false);
            async move {
                let mut client = proto::replication_data_client::ReplicationDataClient::connect(
                    format!("http://{target_address}"),
                )
                .await
                .unwrap();
                client
                    .build(Request::new(stream::iter([frame])))
                    .await
                    .unwrap()
                    .into_inner()
                    .next()
                    .await
            }
        });
        target_state.finish_copy_entered.notified().await;
        target_handle
            .install_stateless_preview_inbound_build_authority(&authority_b)
            .await
            .unwrap();
        assert!(
            held_finish
                .await
                .unwrap()
                .is_none_or(|result| result.is_err()),
            "revoked target finish_copy returned a successful acknowledgement"
        );
        assert_eq!(
            *target_state.progress.lock().unwrap(),
            DurableApplicationProgress::default(),
            "superseded finish_copy changed target durable progress"
        );
        target_state
            .block_finish_copy
            .store(false, Ordering::Release);
        let mut first_b_frame = copy_frame(&authority_b, 0, true, false);
        first_b_frame.lsn = 0;
        first_b_frame.committed_lsn = 0;
        first_b_frame.replication_boundary_lsn = 0;
        let mut client = proto::replication_data_client::ReplicationDataClient::connect(format!(
            "http://{target_address}"
        ))
        .await
        .unwrap();
        let mut first_b = client
            .build(Request::new(stream::iter([first_b_frame])))
            .await
            .unwrap()
            .into_inner();
        assert!(!first_b.next().await.unwrap().unwrap().final_item);
        target_state.block_finish_copy();
        let mut held_b = tokio::spawn({
            let mut frame = copy_frame(&authority_b, 1, false, false);
            frame.lsn = 0;
            frame.committed_lsn = 0;
            frame.replication_boundary_lsn = 0;
            async move {
                let mut client = proto::replication_data_client::ReplicationDataClient::connect(
                    format!("http://{target_address}"),
                )
                .await
                .unwrap();
                client
                    .build(Request::new(stream::iter([frame])))
                    .await
                    .unwrap()
                    .into_inner()
                    .next()
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                _ = target_state.finish_copy_entered.notified() => {}
                result = &mut held_b => panic!("B completed before entering provider: {result:?}"),
            }
        })
        .await
        .expect("B did not enter the target provider");
        assert!(
            target_handle
                .install_stateless_preview_inbound_build_authority(&authority_a)
                .await
                .is_err(),
            "stale authority A unexpectedly replaced in-flight authority B"
        );
        target_handle
            .install_stateless_preview_inbound_build_authority(&authority_b)
            .await
            .unwrap();
        target_state.release_finish_copy.notify_one();
        let continued_b = tokio::time::timeout(Duration::from_secs(5), held_b)
            .await
            .expect("in-flight B did not continue after stale/exact admission")
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(continued_b.sequence, 1);
        assert!(!continued_b.final_item);
        assert_eq!(
            *target_state.progress.lock().unwrap(),
            DurableApplicationProgress::default(),
            "stale or exact admission reset B into a provider progress mutation"
        );
        source_state.block_copy();
        let build_program = PublicOperationProgram::Build(build_input.clone());
        let build_intent = intent(
            &preview,
            source_handle.process_session_id(),
            4,
            "preview-build-attempt",
            build_program,
        );
        let build_accepted = PublicV1AcceptedStatus {
            identity: preview.clone(),
            history_initialization_id: initialization.clone(),
            history: history.clone(),
            data_loss_recovery: None,
            completed_operations: Vec::new(),
            extensions: Default::default(),
        };
        let build_transition = PublicV1AcceptedTransitionStatus {
            resource_version: "build-status-1".into(),
            evidence: build_accepted.clone(),
            intent: PublicV1AcceptedTransitionIntent {
                consumer: PublicV1EvidenceConsumer::RepairReplacement,
                operation_id: build_intent.operation_id.clone(),
                revision: build_intent.revision,
                call_kind: PublicV1CallKind::Build,
                history: history.clone(),
                source: PublicV1ResourceAuthority {
                    storage_incarnation: source_storage.clone(),
                    resource_version: "build-source-rv-1".into(),
                },
                target: Some(PublicV1ResourceAuthority {
                    storage_incarnation: target_storage.clone(),
                    resource_version: "build-target-rv-1".into(),
                }),
                process_session_id: build_intent.process_session_id.clone(),
                input_digest: build_intent.input_digest.clone(),
                replay_disposition: build_intent.replay_disposition,
                previous_configuration_id: None,
                current_configuration_id: Some(source_configuration.configuration_id.clone()),
                catch_up_mode: None,
                catch_up_specific_quorum: None,
            },
        };
        let build_evidence = Arc::new(
            PublicV1EvidenceState::new(
                build_transition.clone(),
                vec![
                    PublicV1EvidenceResource {
                        storage_incarnation: source_storage.clone(),
                        resource_version: "build-source-rv-1".into(),
                        proof: Some(PublicV1ResourceProof {
                            history: history.clone(),
                            storage_incarnation: source_storage.clone(),
                        }),
                        disposition: PublicV1ResourceDisposition::Active,
                    },
                    PublicV1EvidenceResource {
                        storage_incarnation: target_storage.clone(),
                        resource_version: "build-target-rv-1".into(),
                        proof: Some(PublicV1ResourceProof {
                            history: history.clone(),
                            storage_incarnation: target_storage.clone(),
                        }),
                        disposition: PublicV1ResourceDisposition::Active,
                    },
                ],
            )
            .unwrap(),
        );
        let build_adapter = Arc::new(
            HostedPublicV1Adapter::new(
                vec![
                    HostedPreviewReplica {
                        identity: source_identity.clone(),
                        replication_address: format!("http://{source_address}"),
                        handle: source_handle.clone(),
                    },
                    HostedPreviewReplica {
                        identity: target_identity.clone(),
                        replication_address: format!("http://{target_address}"),
                        handle: target_handle.clone(),
                    },
                ],
                Some(build_accepted),
                None,
                build_evidence.clone(),
            )
            .unwrap(),
        );
        let build_command =
            ProtocolCommand::PublicV1Operation(Box::new(PublicV1OperationCommand {
                target: source_identity.clone(),
                intent: build_intent,
            }));
        let copy_entered = source_state.copy_entered.notified();
        let mut build = tokio::spawn({
            let build_adapter = build_adapter.clone();
            let build_transition = build_transition.clone();
            async move {
                execute_public_v1_operation(
                    build_adapter.as_ref(),
                    &build_transition,
                    build_command,
                )
                .await
            }
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                _ = copy_entered => {}
                result = &mut build => panic!("build completed before copy: {result:?}"),
            }
        })
        .await
        .expect("build did not request its frozen copy");
        let exact_frame = proto::CopyItem {
            protocol_version: 11,
            build_id: build_input.attempt.to_string(),
            sender: Some(source_identity.clone().into()),
            receiver: Some(target_identity.clone().into()),
            epoch: Some(epoch.into()),
            current_configuration_id: source_configuration.configuration_id.to_string(),
            sequence: 0,
            lsn: 1,
            committed_lsn: 1,
            replication_boundary_lsn: 1,
            final_item: false,
            data: b"unauthorized-copy".to_vec(),
            snapshot_chunk: true,
            sender_session_id: source_handle.process_session_id().to_string(),
            receiver_session_id: target_session.to_string(),
            catch_up_boundary_lsn: None,
            source_storage: Some(storage_proto(&source_storage)),
            target_storage: Some(storage_proto(&target_storage)),
            history: Some(history_proto(&history)),
            target_address: format!("http://{target_address}"),
        };
        let mut wrong_frames = Vec::new();
        let mut wrong = exact_frame.clone();
        wrong.sender = Some(replica_named(9, "wrong-source", "wrong-source-agent").into());
        wrong_frames.push(wrong);
        let mut wrong = exact_frame.clone();
        wrong.build_id = "wrong-build".into();
        wrong_frames.push(wrong);
        let mut wrong = exact_frame.clone();
        let mut wrong_storage = target_storage.clone();
        wrong_storage.storage_generation = StorageGeneration::new("wrong-target-storage");
        wrong.target_storage = Some(storage_proto(&wrong_storage));
        wrong_frames.push(wrong);
        let mut wrong = exact_frame.clone();
        wrong.history.as_mut().unwrap().history_id = "wrong-history".into();
        wrong_frames.push(wrong);
        let mut wrong = exact_frame.clone();
        wrong.sender_session_id = "wrong-source-session".into();
        wrong_frames.push(wrong);
        let mut wrong = exact_frame;
        wrong.epoch = Some(Epoch::new(0, 2).into());
        wrong_frames.push(wrong);
        for wrong in wrong_frames {
            let mut client = proto::replication_data_client::ReplicationDataClient::connect(
                format!("http://{target_address}"),
            )
            .await
            .unwrap();
            let mut response = client
                .build(Request::new(stream::iter([wrong])))
                .await
                .unwrap()
                .into_inner();
            assert!(response.next().await.unwrap().is_err());
            assert_eq!(
                *target_state.progress.lock().unwrap(),
                DurableApplicationProgress::default()
            );
            assert_eq!(
                target_state.copy_chunks.lock().unwrap().as_slice(),
                &[Bytes::from_static(b"snapshot-bytes")],
                "rejected frame changed the superseded target copy"
            );
        }
        assert_eq!(
            source_app
                .replicate(Bytes::from_static(b"retained-after-snapshot"))
                .await
                .unwrap(),
            2
        );
        source_state.release_copy.notify_waiters();
        build.await.unwrap().unwrap();
        let build_report = build_adapter.report(&source_identity).await.unwrap();
        assert_eq!(
            reconcile_public_v1_consumer(build_adapter.as_ref(), &build_report)
                .await
                .unwrap(),
            kuberic_controller::evaluator::test_bridge::PublicV1ReconciliationOutcome::
                EvidencePersisted
        );
        assert_eq!(
            reconcile_public_v1_consumer(build_adapter.as_ref(), &build_report)
                .await
                .unwrap(),
            kuberic_controller::evaluator::test_bridge::PublicV1ReconciliationOutcome::
                EffectApplied
        );
        assert_eq!(build_evidence.effects().len(), 1);
        assert_eq!(
            *target_state.progress.lock().unwrap(),
            DurableApplicationProgress {
                applied_lsn: 2,
                committed_lsn: 2,
            }
        );
        assert_eq!(
            target_state.copy_chunks.lock().unwrap().as_slice(),
            &[Bytes::from_static(b"snapshot-bytes")]
        );
        assert!(
            target_state
                .operations
                .lock()
                .unwrap()
                .iter()
                .any(|operation| {
                    operation.lsn == 2 && operation.data.as_ref() == b"retained-after-snapshot"
                })
        );
        let terminal_counts = (
            target_state.copy_chunks.lock().unwrap().len(),
            target_state.operations.lock().unwrap().len(),
        );
        let terminal_frame = proto::CopyItem {
            protocol_version: 11,
            build_id: build_input.attempt.to_string(),
            sender: Some(source_identity.clone().into()),
            receiver: Some(target_identity.clone().into()),
            epoch: Some(epoch.into()),
            current_configuration_id: source_configuration.configuration_id.to_string(),
            sequence: 3,
            lsn: 2,
            committed_lsn: 2,
            replication_boundary_lsn: 1,
            final_item: true,
            data: Vec::new(),
            snapshot_chunk: false,
            sender_session_id: source_handle.process_session_id().to_string(),
            receiver_session_id: target_session.to_string(),
            catch_up_boundary_lsn: Some(2),
            source_storage: Some(storage_proto(&source_storage)),
            target_storage: Some(storage_proto(&target_storage)),
            history: Some(history_proto(&history)),
            target_address: format!("http://{target_address}"),
        };
        let mut client = proto::replication_data_client::ReplicationDataClient::connect(format!(
            "http://{target_address}"
        ))
        .await
        .unwrap();
        let replayed_ack = client
            .build(Request::new(stream::iter([terminal_frame.clone()])))
            .await
            .unwrap()
            .into_inner()
            .next()
            .await
            .unwrap()
            .unwrap();
        assert!(replayed_ack.final_item);
        assert_eq!(replayed_ack.durable_lsn, 2);
        assert_eq!(
            terminal_counts,
            (
                target_state.copy_chunks.lock().unwrap().len(),
                target_state.operations.lock().unwrap().len(),
            ),
            "terminal ACK replay repeated provider mutation"
        );
        let mut changed_terminal = terminal_frame;
        changed_terminal.current_configuration_id = "changed-terminal-authority".into();
        let mut client = proto::replication_data_client::ReplicationDataClient::connect(format!(
            "http://{target_address}"
        ))
        .await
        .unwrap();
        assert!(
            client
                .build(Request::new(stream::iter([changed_terminal])))
                .await
                .unwrap()
                .into_inner()
                .next()
                .await
                .unwrap()
                .is_err()
        );
        let admitted = ConfigurationDescriptor::new(
            epoch,
            ReplicaId::new(1),
            vec![
                ConfigurationMember {
                    identity: source_identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: target_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        target_handle
            .execute_stateless_preview(intent(
                &preview,
                target_session.clone(),
                7,
                "admit-build-target-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::ActiveSecondary,
                },
            ))
            .await
            .unwrap();
        target_handle
            .execute_stateless_preview(intent(
                &preview,
                target_session.clone(),
                8,
                "admit-build-target-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: admitted.clone(),
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: source_identity.clone(),
                        replication_address: format!("http://{source_address}"),
                        process_session_id: source_handle.process_session_id(),
                        storage_incarnation: Some(source_storage.clone()),
                        role: ReplicaRole::Primary,
                        current_progress: 2,
                        catch_up_capability: 1,
                        must_catchup: false,
                    }],
                }),
            ))
            .await
            .unwrap();
        source_handle
            .execute_stateless_preview(intent(
                &preview,
                source_handle.process_session_id(),
                6,
                "admit-build-source-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: admitted,
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: target_identity.clone(),
                        replication_address: format!("http://{target_address}"),
                        process_session_id: target_session,
                        storage_incarnation: Some(target_storage.clone()),
                        role: ReplicaRole::ActiveSecondary,
                        current_progress: 2,
                        catch_up_capability: 2,
                        must_catchup: false,
                    }],
                }),
            ))
            .await
            .unwrap();
        assert_eq!(
            source_app
                .replicate(Bytes::from_static(b"after-admission"))
                .await
                .unwrap(),
            3
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while target_state.progress.lock().unwrap().committed_lsn < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("admitted build target did not observe committed write");
        source_handle
            .execute_stateless_preview(intent(
                &preview,
                source_handle.process_session_id(),
                6,
                "preview-remove-target",
                PublicOperationProgram::Remove(build_input),
            ))
            .await
            .unwrap();
        let single = ConfigurationDescriptor::new(
            epoch,
            ReplicaId::new(1),
            vec![ConfigurationMember {
                identity: source_identity,
                role: ReplicaRole::Primary,
            }],
            1,
        );
        source_handle
            .execute_stateless_preview(intent(
                &preview,
                source_handle.process_session_id(),
                8,
                "post-removal-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: single,
                    previous: None,
                    replicas: Vec::new(),
                }),
            ))
            .await
            .unwrap();
        target.shutdown();
        target.wait().await.unwrap();
        assert_eq!(
            source_app
                .replicate(Bytes::from_static(b"after-removal"))
                .await
                .unwrap(),
            4
        );

        source.shutdown();
        source.wait().await.unwrap();
        std::fs::remove_dir_all(source_root).unwrap();
        std::fs::remove_dir_all(target_root).unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn hosted_preview_supersession_cancels_build_root_and_drains_descendant_before_close() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let resource_uid = ResourceUid::new("preview-close-cancels-build");
        let preview = PublicOperationPreviewIdentity::public_v1(13);
        let mut source =
            HostedMemoryReplica::start("close-build-source", &resource_uid, &preview, 1).await;
        let generation = PublicProtocolGeneration::new(13);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        source.initialize_history(&initialization, &history).await;
        let epoch = Epoch::new(0, 1);
        let source_configuration = ConfigurationDescriptor::new(
            epoch,
            source.identity.replica_id,
            vec![ConfigurationMember {
                identity: source.identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        install_static_authority(
            &source.handle(),
            &preview,
            source.identity.clone(),
            ReplicaRole::Primary,
            epoch,
            PublicConfiguration {
                current: source_configuration.clone(),
                previous: None,
                replicas: Vec::new(),
            },
        )
        .await;
        assert_eq!(
            source
                .application
                .replicate(Bytes::from_static(b"snapshot-before-close"))
                .await
                .unwrap(),
            1
        );
        source
            .state
            .copy_chunks
            .lock()
            .unwrap()
            .push(Bytes::from_static(b"close-build-snapshot"));
        source.state.block_copy_with_descendant();
        let target_identity =
            replica_named(2, "close-build-target-pod", "close-build-target-agent");
        let build_input = PublicBuildInput {
            attempt: OperationId::new("build-superseded-by-close"),
            epoch,
            current_configuration_id: Some(source_configuration.configuration_id.clone()),
            replica: target_identity.clone(),
            target_storage: storage_named(
                target_identity,
                "close-build-target-pod",
                "close-build-target-pvc",
            ),
            process_session_id: kuberic_runtime::protocol::types::ProcessSessionId::new(
                "close-build-target-session",
            ),
            replication_address: "http://127.0.0.1:1".into(),
            history: Some(history),
        };
        let build_intent = intent(
            &preview,
            source.handle().process_session_id(),
            4,
            "build-superseded-by-close",
            PublicOperationProgram::Build(build_input),
        );
        let copy_entered = source.state.copy_entered.notified();
        let mut build = tokio::spawn({
            let handle = source.handle();
            async move { handle.execute_stateless_preview(build_intent).await }
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                _ = copy_entered => {}
                result = &mut build => panic!("build root completed before held callback: {result:?}"),
            }
        })
        .await
        .expect("build root did not enter the provider copy callback");
        assert!(!build.is_finished(), "build root was not held");
        tokio::time::timeout(
            Duration::from_secs(10),
            source.state.copy_descendant_entered.notified(),
        )
            .await
            .expect("provider callback descendant did not start");

        let successor = tokio::spawn({
            let handle = source.handle();
            let preview = preview.clone();
            async move {
                handle
                    .execute_stateless_preview(intent(
                        &preview,
                        handle.process_session_id(),
                        5,
                        "progress-supersedes-build",
                        PublicOperationProgram::Progress { capability: true },
                    ))
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if source
                    .state
                    .cancelled_copy_descendants
                    .load(Ordering::SeqCst)
                    == 1
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("successor did not cancel and drain the callback descendant");
        assert!(
            tokio::time::timeout(Duration::from_secs(10), build)
                .await
                .expect("superseded build root did not terminate")
                .unwrap()
                .is_err(),
            "superseded build root completed successfully"
        );
        tokio::time::timeout(Duration::from_secs(10), successor)
            .await
            .expect("successor did not complete after descendant drain")
            .unwrap()
            .unwrap();
        source
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                source.handle().process_session_id(),
                6,
                "close-after-build-supersession",
                PublicOperationProgram::Close,
            ))
            .await
            .unwrap();
        assert_eq!(
            source
                .state
                .copy_descendant_mutations
                .load(Ordering::SeqCst),
            0,
            "cancelled callback descendant mutated state"
        );
        source.state.release_copy_descendant.notify_waiters();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            source
                .state
                .copy_descendant_mutations
                .load(Ordering::SeqCst),
            0,
            "cancelled callback descendant performed a delayed mutation after close"
        );
        assert!(std::net::TcpListener::bind(source.address).is_ok());

        source.stop().await;
        source.cleanup();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn hung_build_peer_is_bounded_and_drained_before_successor_close() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let resource_uid = ResourceUid::new("hung-build-peer");
        let preview = PublicOperationPreviewIdentity::public_v1(753);
        let mut source =
            HostedMemoryReplica::start("hung-build-source", &resource_uid, &preview, 1).await;
        let generation = PublicProtocolGeneration::new(753);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        source.initialize_history(&initialization, &history).await;
        let epoch = Epoch::new(0, 1);
        let configuration = ConfigurationDescriptor::new(
            epoch,
            source.identity.replica_id,
            vec![ConfigurationMember {
                identity: source.identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        install_static_authority(
            &source.handle(),
            &preview,
            source.identity.clone(),
            ReplicaRole::Primary,
            epoch,
            PublicConfiguration {
                current: configuration.clone(),
                previous: None,
                replicas: Vec::new(),
            },
        )
        .await;
        assert_eq!(
            source
                .application
                .replicate(Bytes::from_static(b"hung-build-snapshot"))
                .await
                .unwrap(),
            1
        );
        let listener = TcpListener::bind(free_address()).await.unwrap();
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(Notify::new());
        let hang = tokio::spawn({
            let accepted = accepted.clone();
            async move {
                tonic::transport::Server::builder()
                    .add_service(proto::replication_data_server::ReplicationDataServer::new(
                        HungBuildService { entered: accepted },
                    ))
                    .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                    .await
            }
        });
        let target = replica_named(2, "hung-build-target-pod", "hung-build-target-agent");
        let build = tokio::spawn({
            let handle = source.handle();
            let preview = preview.clone();
            let history = history.clone();
            let configuration_id = configuration.configuration_id.clone();
            async move {
                handle
                    .execute_stateless_preview(intent(
                        &preview,
                        handle.process_session_id(),
                        4,
                        "hung-build-attempt",
                        PublicOperationProgram::Build(PublicBuildInput {
                            attempt: OperationId::new("hung-build-attempt"),
                            epoch,
                            current_configuration_id: Some(configuration_id),
                            replica: target.clone(),
                            target_storage: storage_named(
                                target,
                                "hung-build-target-pod",
                                "hung-build-target-pvc",
                            ),
                            process_session_id: ProcessSessionId::new("hung-build-target-session"),
                            replication_address: format!("http://{address}"),
                            history: Some(history),
                        }),
                    ))
                    .await
            }
        });
        accepted.notified().await;
        let deadline_started = tokio::time::Instant::now();
        assert!(
            tokio::time::timeout(Duration::from_secs(10), build)
                .await
                .expect("hung build exceeded its peer deadline")
                .unwrap()
                .is_err()
        );
        assert!(
            deadline_started.elapsed() >= Duration::from_secs(4),
            "hung build did not exercise the peer ACK deadline"
        );
        let successor = tokio::spawn({
            let handle = source.handle();
            let preview = preview.clone();
            async move {
                handle
                    .execute_stateless_preview(intent(
                        &preview,
                        handle.process_session_id(),
                        5,
                        "progress-after-hung-build",
                        PublicOperationProgram::Progress { capability: true },
                    ))
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(10), successor)
            .await
            .expect("successor did not drain the timed-out build")
            .unwrap()
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            source.handle().execute_stateless_preview(intent(
                &preview,
                source.handle().process_session_id(),
                6,
                "close-after-hung-build",
                PublicOperationProgram::Close,
            )),
        )
        .await
        .expect("Close was not bounded after a hung build")
        .unwrap();
        hang.abort();
        let _ = hang.await;
        source.stop().await;
        source.cleanup();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn build_retained_gap_corruption_replaces_source_without_contacting_target() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let resource_uid = ResourceUid::new("build-source-gap-corruption");
        let preview = PublicOperationPreviewIdentity::public_v1(754);
        let mut source =
            HostedMemoryReplica::start("build-gap-source", &resource_uid, &preview, 1).await;
        let generation = PublicProtocolGeneration::new(754);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        source.initialize_history(&initialization, &history).await;
        let epoch = Epoch::new(0, 1);
        let configuration = ConfigurationDescriptor::new(
            epoch,
            source.identity.replica_id,
            vec![ConfigurationMember {
                identity: source.identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        install_static_authority(
            &source.handle(),
            &preview,
            source.identity.clone(),
            ReplicaRole::Primary,
            epoch,
            PublicConfiguration {
                current: configuration.clone(),
                previous: None,
                replicas: Vec::new(),
            },
        )
        .await;
        assert_eq!(
            source
                .application
                .replicate(Bytes::from_static(b"source-snapshot"))
                .await
                .unwrap(),
            1
        );
        source.state.block_copy();
        let target_listener = TcpListener::bind(free_address()).await.unwrap();
        let target_address = target_listener.local_addr().unwrap();
        let target = replica_named(2, "healthy-target-pod", "healthy-target-agent");
        let operation_id = OperationId::new("build-with-corrupt-source-gap");
        let copy_entered = source.state.copy_entered.notified();
        let build = tokio::spawn({
            let handle = source.handle();
            let preview = preview.clone();
            let history = history.clone();
            let configuration_id = configuration.configuration_id.clone();
            let target = target.clone();
            async move {
                handle
                    .execute_stateless_preview(intent(
                        &preview,
                        handle.process_session_id(),
                        4,
                        "build-with-corrupt-source-gap",
                        PublicOperationProgram::Build(PublicBuildInput {
                            attempt: operation_id,
                            epoch,
                            current_configuration_id: Some(configuration_id),
                            replica: target.clone(),
                            target_storage: storage_named(
                                target,
                                "healthy-target-pod",
                                "healthy-target-pvc",
                            ),
                            process_session_id: ProcessSessionId::new("healthy-target-session"),
                            replication_address: format!("http://{target_address}"),
                            history: Some(history),
                        }),
                    ))
                    .await
            }
        });
        copy_entered.await;
        assert_eq!(
            source
                .application
                .replicate(Bytes::from_static(b"missing-retained-gap"))
                .await
                .unwrap(),
            2
        );
        source
            .state
            .operations
            .lock()
            .unwrap()
            .retain(|operation| operation.lsn != 2);
        source.state.release_copy.notify_waiters();
        assert!(
            build
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("local build source requires replacement")
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), target_listener.accept())
                .await
                .is_err(),
            "source corruption contacted or mutated the healthy target"
        );
        let partition = source.application.partition.read().await.clone().unwrap();
        assert_eq!(
            partition.get_read_status().await.unwrap(),
            AccessStatus::NotPrimary
        );
        assert_eq!(
            partition.get_write_status().await.unwrap(),
            AccessStatus::NotPrimary
        );
        assert!(
            source
                .application
                .replicate(Bytes::from_static(
                    b"must-not-write-after-source-corruption"
                ))
                .await
                .is_err(),
            "local rebuild-required returned before write authority was revoked"
        );
        source
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                source.handle().process_session_id(),
                5,
                "progress-after-source-corruption",
                PublicOperationProgram::Progress { capability: true },
            ))
            .await
            .unwrap();
        assert_eq!(
            partition.get_read_status().await.unwrap(),
            AccessStatus::NotPrimary
        );
        assert_eq!(
            partition.get_write_status().await.unwrap(),
            AccessStatus::NotPrimary
        );
        let report = source.handle().stateless_preview_report().await.unwrap();
        let completion = report
            .completed_operations
            .iter()
            .find(|completion| completion.operation_id.as_str() == "build-with-corrupt-source-gap")
            .unwrap();
        assert!(matches!(
            completion.result,
            PublicV1OperationResult::RebuildRequired(ref rebuild)
                if matches!(
                    rebuild.as_ref(),
                    kuberic_runtime::protocol::public_operations::
                        CanonicalReplicaRebuildRequired::LocalProvider {
                            local_replica,
                            ..
                        } if local_replica == &source.identity
                )
        ));
        source.stop().await;
        source.cleanup();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn hosted_preview_removal_drains_build_excludes_peer_and_rejects_stale_session() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let resource_uid = ResourceUid::new("preview-removal-drain");
        let preview = PublicOperationPreviewIdentity::public_v1(9);
        let mut source =
            HostedMemoryReplica::start("removal-source", &resource_uid, &preview, 1).await;
        let mut target =
            HostedMemoryReplica::start("removal-target", &resource_uid, &preview, 2).await;
        let generation = PublicProtocolGeneration::new(9);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        source.initialize_history(&initialization, &history).await;
        target.initialize_history(&initialization, &history).await;
        let epoch = Epoch::new(0, 1);
        let source_configuration = ConfigurationDescriptor::new(
            epoch,
            source.identity.replica_id,
            vec![ConfigurationMember {
                identity: source.identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        install_static_authority(
            &source.handle(),
            &preview,
            source.identity.clone(),
            ReplicaRole::Primary,
            epoch,
            PublicConfiguration {
                current: source_configuration.clone(),
                previous: None,
                replicas: Vec::new(),
            },
        )
        .await;
        let target_session = target.handle().process_session_id();
        for (revision, name, program) in [
            (
                1,
                "open-removal-target",
                PublicOperationProgram::Open {
                    replica: target.identity.clone(),
                    existing: true,
                },
            ),
            (
                2,
                "role-removal-target",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
            (
                3,
                "epoch-removal-target",
                PublicOperationProgram::Epoch { epoch },
            ),
        ] {
            target
                .handle()
                .execute_stateless_preview(intent(
                    &preview,
                    target_session.clone(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        assert_eq!(
            source
                .application
                .replicate(Bytes::from_static(b"snapshot-before-removal"))
                .await
                .unwrap(),
            1
        );
        source
            .state
            .copy_chunks
            .lock()
            .unwrap()
            .push(Bytes::from_static(b"frozen-build-snapshot"));
        source.state.block_copy();
        target.state.block_commit();
        let build_input = PublicBuildInput {
            attempt: OperationId::new("active-build-to-remove"),
            epoch,
            current_configuration_id: Some(source_configuration.configuration_id.clone()),
            replica: target.identity.clone(),
            target_storage: target.storage.clone(),
            process_session_id: target_session.clone(),
            replication_address: format!("http://{}", target.address),
            history: Some(history.clone()),
        };
        let build_intent = intent(
            &preview,
            source.handle().process_session_id(),
            4,
            "active-build-to-remove",
            PublicOperationProgram::Build(build_input.clone()),
        );
        target
            .handle()
            .install_stateless_preview_inbound_build_authority(&PublicInboundBuildAuthority {
                operation_id: OperationId::new("active-build-to-remove"),
                operation_revision: 4,
                attempt: PublicBuildAttemptReference {
                    resource_uid: resource_uid.clone(),
                    epoch,
                    build_id: build_input.attempt.clone(),
                    source: source.identity.clone(),
                    source_storage: source.storage.clone(),
                    source_session: source.handle().process_session_id(),
                    target: target.identity.clone(),
                    target_storage: target.storage.clone(),
                    target_session: target_session.clone(),
                },
                history: history.clone(),
                current_configuration_id: source_configuration.configuration_id.clone(),
                target_address: format!("http://{}", target.address),
            })
            .await
            .unwrap();
        let copy_entered = source.state.copy_entered.notified();
        let mut build = tokio::spawn({
            let handle = source.handle();
            async move { handle.execute_stateless_preview(build_intent).await }
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                _ = copy_entered => {}
                result = &mut build => panic!("build completed before frozen copy: {result:?}"),
            }
        })
        .await
        .expect("build root did not reach the frozen copy");
        assert!(!build.is_finished(), "build root was not active");
        assert_eq!(
            source
                .application
                .replicate(Bytes::from_static(b"retained-before-removal"))
                .await
                .unwrap(),
            2
        );
        let descendant_entered = target.state.commit_entered.notified();
        source.state.release_copy.notify_one();
        tokio::time::timeout(Duration::from_secs(10), descendant_entered)
            .await
            .expect("remote build descendant did not reach durable commit");
        assert!(!build.is_finished(), "build descendant was not active");

        let removal_intent = intent(
            &preview,
            source.handle().process_session_id(),
            4,
            "remove-active-build",
            PublicOperationProgram::Remove(build_input),
        );
        let accepted = PublicV1AcceptedStatus {
            identity: preview.clone(),
            history_initialization_id: initialization,
            history: history.clone(),
            data_loss_recovery: None,
            completed_operations: Vec::new(),
            extensions: Default::default(),
        };
        let transition = PublicV1AcceptedTransitionStatus {
            resource_version: "removal-status-rv-1".into(),
            evidence: accepted.clone(),
            intent: PublicV1AcceptedTransitionIntent {
                consumer: PublicV1EvidenceConsumer::Removal,
                operation_id: removal_intent.operation_id.clone(),
                revision: removal_intent.revision,
                call_kind: PublicV1CallKind::Remove,
                history: history.clone(),
                source: PublicV1ResourceAuthority {
                    storage_incarnation: source.storage.clone(),
                    resource_version: "removal-source-rv-1".into(),
                },
                target: Some(PublicV1ResourceAuthority {
                    storage_incarnation: target.storage.clone(),
                    resource_version: "removal-target-rv-1".into(),
                }),
                process_session_id: removal_intent.process_session_id.clone(),
                input_digest: removal_intent.input_digest.clone(),
                replay_disposition: removal_intent.replay_disposition,
                previous_configuration_id: None,
                current_configuration_id: None,
                catch_up_mode: None,
                catch_up_specific_quorum: None,
            },
        };
        let evidence = Arc::new(
            PublicV1EvidenceState::new(
                transition.clone(),
                vec![
                    PublicV1EvidenceResource {
                        storage_incarnation: source.storage.clone(),
                        resource_version: "removal-source-rv-1".into(),
                        proof: Some(PublicV1ResourceProof {
                            history: history.clone(),
                            storage_incarnation: source.storage.clone(),
                        }),
                        disposition: PublicV1ResourceDisposition::Active,
                    },
                    PublicV1EvidenceResource {
                        storage_incarnation: target.storage.clone(),
                        resource_version: "removal-target-rv-1".into(),
                        proof: Some(PublicV1ResourceProof {
                            history: history.clone(),
                            storage_incarnation: target.storage.clone(),
                        }),
                        disposition: PublicV1ResourceDisposition::Active,
                    },
                ],
            )
            .unwrap(),
        );
        let adapter = Arc::new(
            HostedPublicV1Adapter::new(
                vec![
                    HostedPreviewReplica {
                        identity: source.identity.clone(),
                        replication_address: format!("http://{}", source.address),
                        handle: source.handle(),
                    },
                    HostedPreviewReplica {
                        identity: target.identity.clone(),
                        replication_address: format!("http://{}", target.address),
                        handle: target.handle(),
                    },
                ],
                Some(accepted.clone()),
                None,
                evidence.clone(),
            )
            .unwrap(),
        );
        let removal = tokio::spawn({
            let adapter = adapter.clone();
            let transition = transition.clone();
            async move {
                execute_public_v1_operation(
                    adapter.as_ref(),
                    &transition,
                    ProtocolCommand::PublicV1Operation(Box::new(PublicV1OperationCommand {
                        target: transition.intent.source.storage_incarnation.replica.clone(),
                        intent: removal_intent,
                    })),
                )
                .await
            }
        });
        assert!(evidence.effects().is_empty());
        assert!(
            tokio::time::timeout(Duration::from_secs(10), build)
                .await
                .expect("cancelled build root did not drain")
                .unwrap()
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(10), removal)
            .await
            .expect("removal did not cancel and drain the build")
            .unwrap()
            .unwrap();
        let committed_before_release = target.state.progress.lock().unwrap().committed_lsn;
        target.state.release_commit.notify_one();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            target.state.progress.lock().unwrap().committed_lsn,
            committed_before_release,
            "cancelled target build committed after removal completed"
        );
        let report = adapter.report(&source.identity).await.unwrap();
        assert_eq!(
            reconcile_public_v1_consumer(adapter.as_ref(), &report)
                .await
                .unwrap(),
            kuberic_controller::evaluator::test_bridge::PublicV1ReconciliationOutcome::
                EvidencePersisted
        );
        assert_eq!(
            reconcile_public_v1_consumer(adapter.as_ref(), &report)
                .await
                .unwrap(),
            kuberic_controller::evaluator::test_bridge::PublicV1ReconciliationOutcome::
                EffectApplied
        );
        assert!(matches!(
            evidence.effects().as_slice(),
            [kuberic_controller::evaluator::test_bridge::PublicV1EvidenceEffect::Retired {
                consumer: PublicV1EvidenceConsumer::Removal,
                operation_id,
                revision: 4,
                storage,
            }] if operation_id == &OperationId::new("remove-active-build")
                && storage.as_ref() == &target.storage
        ));
        let target_resource = evidence
            .resources()
            .into_iter()
            .find(|resource| resource.storage_incarnation == target.storage)
            .unwrap();
        assert_eq!(target_resource.resource_version, "removal-target-rv-1-next");
        assert_eq!(
            target_resource.disposition,
            PublicV1ResourceDisposition::Retired
        );

        let before = *target.state.progress.lock().unwrap();
        let mut client = proto::replication_data_client::ReplicationDataClient::connect(format!(
            "http://{}",
            target.address
        ))
        .await
        .unwrap();
        let stale = proto::ReplicationItem {
            protocol_version: 11,
            sender: Some(source.identity.clone().into()),
            receiver: Some(target.identity.clone().into()),
            epoch: Some(epoch.into()),
            previous_configuration_id: String::new(),
            current_configuration_id: source_configuration.configuration_id.to_string(),
            lsn: before.applied_lsn + 1,
            committed_lsn: before.committed_lsn,
            data: b"stale-after-retirement".to_vec(),
            sender_session_id: source.handle().process_session_id().to_string(),
            receiver_session_id: target_session.to_string(),
        };
        let mut response = client
            .replicate(tonic::Request::new(stream::iter([stale])))
            .await
            .unwrap()
            .into_inner();
        assert!(response.next().await.unwrap().is_err());
        assert_eq!(*target.state.progress.lock().unwrap(), before);
        assert_eq!(
            source
                .application
                .replicate(Bytes::from_static(b"after-exact-removal"))
                .await
                .unwrap(),
            3
        );

        source.stop().await;
        target.stop().await;
        source.cleanup();
        target.cleanup();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn hosted_preview_rebuild_retires_and_restarts_fresh_replacement() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let resource_uid = ResourceUid::new("preview-controller-replacement");
        let preview = PublicOperationPreviewIdentity::public_v1(60);
        let mut source =
            HostedMemoryReplica::start("controller-source", &resource_uid, &preview, 1).await;
        let mut stale =
            HostedMemoryReplica::start("controller-stale", &resource_uid, &preview, 2).await;
        let generation = PublicProtocolGeneration::new(60);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        source.initialize_history(&initialization, &history).await;
        stale.initialize_history(&initialization, &history).await;
        let epoch = Epoch::new(0, 1);
        let source_configuration = ConfigurationDescriptor::new(
            epoch,
            source.identity.replica_id,
            vec![ConfigurationMember {
                identity: source.identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        install_static_authority(
            &source.handle(),
            &preview,
            source.identity.clone(),
            ReplicaRole::Primary,
            epoch,
            PublicConfiguration {
                current: source_configuration.clone(),
                previous: None,
                replicas: Vec::new(),
            },
        )
        .await;
        let stale_session = stale.handle().process_session_id();
        for (revision, name, program) in [
            (
                1,
                "controller-stale-open",
                PublicOperationProgram::Open {
                    replica: stale.identity.clone(),
                    existing: true,
                },
            ),
            (
                2,
                "controller-stale-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
            (
                3,
                "controller-stale-epoch",
                PublicOperationProgram::Epoch { epoch },
            ),
        ] {
            stale
                .handle()
                .execute_stateless_preview(intent(
                    &preview,
                    stale_session.clone(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        assert_eq!(
            source
                .application
                .replicate(Bytes::from_static(b"controller-payload"))
                .await
                .unwrap(),
            1
        );
        source
            .state
            .copy_chunks
            .lock()
            .unwrap()
            .push(Bytes::from_static(b"controller-snapshot"));
        stale
            .state
            .copy_chunks
            .lock()
            .unwrap()
            .push(Bytes::from_static(b"stale-higher-progress"));
        stale.state.operations.lock().unwrap().push(Operation {
            lsn: 9,
            committed_lsn: 8,
            data: Bytes::from_static(b"stale-higher-progress"),
        });
        *stale.state.progress.lock().unwrap() = DurableApplicationProgress {
            applied_lsn: 9,
            committed_lsn: 9,
        };
        let stale_before = *stale.state.progress.lock().unwrap();
        let stale_build = PublicBuildInput {
            attempt: OperationId::new("controller-rebuild-required"),
            epoch,
            current_configuration_id: Some(source_configuration.configuration_id.clone()),
            replica: stale.identity.clone(),
            target_storage: stale.storage.clone(),
            process_session_id: stale_session.clone(),
            replication_address: format!("http://{}", stale.address),
            history: Some(history.clone()),
        };
        let stale_build_intent = intent(
            &preview,
            source.handle().process_session_id(),
            4,
            "controller-rebuild-required",
            PublicOperationProgram::Build(stale_build.clone()),
        );
        let accepted = PublicV1AcceptedStatus {
            identity: preview.clone(),
            history_initialization_id: initialization.clone(),
            history: history.clone(),
            data_loss_recovery: None,
            completed_operations: Vec::new(),
            extensions: Default::default(),
        };
        let build_transition = PublicV1AcceptedTransitionStatus {
            resource_version: "controller-build-status-rv-1".into(),
            evidence: accepted.clone(),
            intent: PublicV1AcceptedTransitionIntent {
                consumer: PublicV1EvidenceConsumer::RepairReplacement,
                operation_id: stale_build_intent.operation_id.clone(),
                revision: stale_build_intent.revision,
                call_kind: PublicV1CallKind::Build,
                history: history.clone(),
                source: PublicV1ResourceAuthority {
                    storage_incarnation: source.storage.clone(),
                    resource_version: "controller-source-rv-1".into(),
                },
                target: Some(PublicV1ResourceAuthority {
                    storage_incarnation: stale.storage.clone(),
                    resource_version: "controller-stale-rv-1".into(),
                }),
                process_session_id: stale_build_intent.process_session_id.clone(),
                input_digest: stale_build_intent.input_digest.clone(),
                replay_disposition: stale_build_intent.replay_disposition,
                previous_configuration_id: None,
                current_configuration_id: Some(source_configuration.configuration_id.clone()),
                catch_up_mode: None,
                catch_up_specific_quorum: None,
            },
        };
        let build_evidence = Arc::new(
            PublicV1EvidenceState::new(
                build_transition.clone(),
                vec![
                    PublicV1EvidenceResource {
                        storage_incarnation: source.storage.clone(),
                        resource_version: "controller-source-rv-1".into(),
                        proof: Some(PublicV1ResourceProof {
                            history: history.clone(),
                            storage_incarnation: source.storage.clone(),
                        }),
                        disposition: PublicV1ResourceDisposition::Active,
                    },
                    PublicV1EvidenceResource {
                        storage_incarnation: stale.storage.clone(),
                        resource_version: "controller-stale-rv-1".into(),
                        proof: Some(PublicV1ResourceProof {
                            history: history.clone(),
                            storage_incarnation: stale.storage.clone(),
                        }),
                        disposition: PublicV1ResourceDisposition::Active,
                    },
                ],
            )
            .unwrap(),
        );
        let build_adapter = HostedPublicV1Adapter::new(
            vec![
                HostedPreviewReplica {
                    identity: source.identity.clone(),
                    replication_address: format!("http://{}", source.address),
                    handle: source.handle(),
                },
                HostedPreviewReplica {
                    identity: stale.identity.clone(),
                    replication_address: format!("http://{}", stale.address),
                    handle: stale.handle(),
                },
            ],
            Some(accepted.clone()),
            None,
            build_evidence.clone(),
        )
        .unwrap();
        let build_error = execute_public_v1_operation(
            &build_adapter,
            &build_transition,
            ProtocolCommand::PublicV1Operation(Box::new(PublicV1OperationCommand {
                target: source.identity.clone(),
                intent: stale_build_intent,
            })),
        )
        .await
        .unwrap_err();
        let rebuild_report = build_adapter.report(&source.identity).await.unwrap();
        let rebuild_outcome = reconcile_public_v1_consumer(&build_adapter, &rebuild_report)
            .await
            .unwrap();
        assert_eq!(
            rebuild_outcome,
            kuberic_controller::evaluator::test_bridge::PublicV1ReconciliationOutcome::
                EvidencePersisted,
            "build error: {build_error}; unexpected rebuild report: {rebuild_report:#?}"
        );
        assert_eq!(
            reconcile_public_v1_consumer(&build_adapter, &rebuild_report)
                .await
                .unwrap(),
            kuberic_controller::evaluator::test_bridge::PublicV1ReconciliationOutcome::
                EffectApplied
        );
        assert!(matches!(
            build_evidence.effects().as_slice(),
            [kuberic_controller::evaluator::test_bridge::PublicV1EvidenceEffect::Replacement {
                storage,
                ..
            }] if storage.as_ref() == &stale.storage
        ));
        let replacement_requested = build_evidence
            .resources()
            .into_iter()
            .find(|resource| resource.storage_incarnation == stale.storage)
            .unwrap();
        assert_eq!(
            replacement_requested.resource_version,
            "controller-stale-rv-1-next"
        );
        assert_eq!(
            replacement_requested.disposition,
            PublicV1ResourceDisposition::ReplacementRequested
        );

        let removal_intent = intent(
            &preview,
            source.handle().process_session_id(),
            4,
            "controller-retire-stale",
            PublicOperationProgram::Remove(stale_build),
        );
        let removal_transition = PublicV1AcceptedTransitionStatus {
            resource_version: "controller-removal-status-rv-1".into(),
            evidence: build_evidence.transition().evidence,
            intent: PublicV1AcceptedTransitionIntent {
                consumer: PublicV1EvidenceConsumer::Removal,
                operation_id: removal_intent.operation_id.clone(),
                revision: removal_intent.revision,
                call_kind: PublicV1CallKind::Remove,
                history: history.clone(),
                source: PublicV1ResourceAuthority {
                    storage_incarnation: source.storage.clone(),
                    resource_version: "controller-source-rv-1".into(),
                },
                target: Some(PublicV1ResourceAuthority {
                    storage_incarnation: stale.storage.clone(),
                    resource_version: "controller-stale-rv-1-next".into(),
                }),
                process_session_id: removal_intent.process_session_id.clone(),
                input_digest: removal_intent.input_digest.clone(),
                replay_disposition: removal_intent.replay_disposition,
                previous_configuration_id: None,
                current_configuration_id: None,
                catch_up_mode: None,
                catch_up_specific_quorum: None,
            },
        };
        let removal_evidence = Arc::new(
            PublicV1EvidenceState::new(removal_transition.clone(), build_evidence.resources())
                .unwrap(),
        );
        let removal_adapter = HostedPublicV1Adapter::new(
            vec![
                HostedPreviewReplica {
                    identity: source.identity.clone(),
                    replication_address: format!("http://{}", source.address),
                    handle: source.handle(),
                },
                HostedPreviewReplica {
                    identity: stale.identity.clone(),
                    replication_address: format!("http://{}", stale.address),
                    handle: stale.handle(),
                },
            ],
            None,
            None,
            removal_evidence.clone(),
        )
        .unwrap();
        execute_public_v1_operation(
            &removal_adapter,
            &removal_transition,
            ProtocolCommand::PublicV1Operation(Box::new(PublicV1OperationCommand {
                target: source.identity.clone(),
                intent: removal_intent,
            })),
        )
        .await
        .unwrap();
        let removal_report = removal_adapter.report(&source.identity).await.unwrap();
        reconcile_public_v1_consumer(&removal_adapter, &removal_report)
            .await
            .unwrap();
        reconcile_public_v1_consumer(&removal_adapter, &removal_report)
            .await
            .unwrap();
        let retired = removal_evidence
            .resources()
            .into_iter()
            .find(|resource| resource.storage_incarnation == stale.storage)
            .unwrap();
        assert_eq!(retired.resource_version, "controller-stale-rv-1-next-next");
        assert_eq!(retired.disposition, PublicV1ResourceDisposition::Retired);
        assert_eq!(*stale.state.progress.lock().unwrap(), stale_before);
        let readmit_before_restart = ConfigurationDescriptor::new(
            epoch,
            source.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source.identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: stale.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            1,
        );
        assert!(
            source
                .handle()
                .execute_stateless_preview(intent(
                    &preview,
                    source.handle().process_session_id(),
                    5,
                    "readmit-retired-configuration-before-restart",
                    PublicOperationProgram::Configuration(PublicConfiguration {
                        current: readmit_before_restart.clone(),
                        previous: None,
                        replicas: vec![PublicReplicaDescriptor {
                            build_id: OperationId::default(),
                            identity: stale.identity.clone(),
                            replication_address: format!("http://{}", stale.address),
                            process_session_id: stale_session.clone(),
                            storage_incarnation: Some(stale.storage.clone()),
                            role: ReplicaRole::ActiveSecondary,
                            current_progress: 99,
                            catch_up_capability: 99,
                            must_catchup: false,
                        }],
                    }),
                ))
                .await
                .is_err()
        );
        let retired_build = PublicBuildInput {
            attempt: OperationId::new("readmit-retired-build-before-restart"),
            epoch,
            current_configuration_id: Some(source_configuration.configuration_id.clone()),
            replica: stale.identity.clone(),
            target_storage: stale.storage.clone(),
            process_session_id: stale_session.clone(),
            replication_address: format!("http://{}", stale.address),
            history: Some(history.clone()),
        };
        assert!(
            source
                .handle()
                .execute_stateless_preview(intent(
                    &preview,
                    source.handle().process_session_id(),
                    5,
                    "readmit-retired-build-before-restart",
                    PublicOperationProgram::Build(retired_build),
                ))
                .await
                .is_err()
        );
        assert_eq!(*stale.state.progress.lock().unwrap(), stale_before);

        stale.stop().await;
        let retired_identity = stale.identity.clone();
        let mut fresh =
            HostedMemoryReplica::start("controller-fresh", &resource_uid, &preview, 2).await;
        fresh.initialize_history(&initialization, &history).await;
        let fresh_session = fresh.handle().process_session_id();
        for (revision, name, program) in [
            (
                1,
                "controller-fresh-open",
                PublicOperationProgram::Open {
                    replica: fresh.identity.clone(),
                    existing: true,
                },
            ),
            (
                2,
                "controller-fresh-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
            (
                3,
                "controller-fresh-epoch",
                PublicOperationProgram::Epoch { epoch },
            ),
        ] {
            fresh
                .handle()
                .execute_stateless_preview(intent(
                    &preview,
                    fresh_session.clone(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        source
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                source.handle().process_session_id(),
                7,
                "controller-authority-after-retirement",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: source_configuration.clone(),
                    previous: None,
                    replicas: Vec::new(),
                }),
            ))
            .await
            .unwrap();
        let fresh_build = PublicBuildInput {
            attempt: OperationId::new("controller-fresh-build"),
            epoch,
            current_configuration_id: Some(source_configuration.configuration_id.clone()),
            replica: fresh.identity.clone(),
            target_storage: fresh.storage.clone(),
            process_session_id: fresh_session.clone(),
            replication_address: format!("http://{}", fresh.address),
            history: Some(history.clone()),
        };
        fresh
            .handle()
            .install_stateless_preview_inbound_build_authority(&PublicInboundBuildAuthority {
                operation_id: OperationId::new("controller-fresh-build"),
                operation_revision: 7,
                attempt: PublicBuildAttemptReference {
                    resource_uid: resource_uid.clone(),
                    epoch,
                    build_id: fresh_build.attempt.clone(),
                    source: source.identity.clone(),
                    source_storage: source.storage.clone(),
                    source_session: source.handle().process_session_id(),
                    target: fresh.identity.clone(),
                    target_storage: fresh.storage.clone(),
                    target_session: fresh_session.clone(),
                },
                history: history.clone(),
                current_configuration_id: source_configuration.configuration_id.clone(),
                target_address: format!("http://{}", fresh.address),
            })
            .await
            .unwrap();
        source
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                source.handle().process_session_id(),
                7,
                "controller-fresh-build",
                PublicOperationProgram::Build(fresh_build),
            ))
            .await
            .unwrap();
        assert_eq!(
            *fresh.state.progress.lock().unwrap(),
            DurableApplicationProgress {
                applied_lsn: 1,
                committed_lsn: 1,
            }
        );
        assert_eq!(
            fresh.state.copy_chunks.lock().unwrap().as_slice(),
            &[Bytes::from_static(b"controller-snapshot")]
        );

        let promoted_epoch = Epoch::new(0, 2);
        let promoted = ConfigurationDescriptor::new(
            promoted_epoch,
            fresh.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: fresh.identity.clone(),
                    role: ReplicaRole::Primary,
                },
            ],
            2,
        );
        for (handle, revision, name, program) in [
            (
                source.handle(),
                9,
                "controller-source-promoted-epoch",
                PublicOperationProgram::Epoch {
                    epoch: promoted_epoch,
                },
            ),
            (
                source.handle(),
                10,
                "controller-source-secondary-role",
                PublicOperationProgram::Role {
                    epoch: promoted_epoch,
                    role: ReplicaRole::ActiveSecondary,
                },
            ),
            (
                fresh.handle(),
                4,
                "controller-fresh-promoted-epoch",
                PublicOperationProgram::Epoch {
                    epoch: promoted_epoch,
                },
            ),
            (
                fresh.handle(),
                5,
                "controller-fresh-primary-role",
                PublicOperationProgram::Role {
                    epoch: promoted_epoch,
                    role: ReplicaRole::Primary,
                },
            ),
        ] {
            handle
                .execute_stateless_preview(intent(
                    &preview,
                    handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        source
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                source.handle().process_session_id(),
                11,
                "controller-source-secondary-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: promoted.clone(),
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: fresh.identity.clone(),
                        replication_address: format!("http://{}", fresh.address),
                        process_session_id: fresh.handle().process_session_id(),
                        storage_incarnation: Some(fresh.storage.clone()),
                        role: ReplicaRole::Primary,
                        current_progress: 1,
                        catch_up_capability: 1,
                        must_catchup: false,
                    }],
                }),
            ))
            .await
            .unwrap();
        fresh
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                fresh.handle().process_session_id(),
                6,
                "controller-fresh-primary-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: promoted,
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: source.identity.clone(),
                        replication_address: format!("http://{}", source.address),
                        process_session_id: source.handle().process_session_id(),
                        storage_incarnation: Some(source.storage.clone()),
                        role: ReplicaRole::ActiveSecondary,
                        current_progress: 1,
                        catch_up_capability: 1,
                        must_catchup: false,
                    }],
                }),
            ))
            .await
            .unwrap();
        assert_eq!(
            source
                .handle()
                .stateless_preview_report()
                .await
                .unwrap()
                .write_status,
            AccessStatus::NotPrimary
        );
        assert_eq!(
            fresh
                .handle()
                .stateless_preview_report()
                .await
                .unwrap()
                .write_status,
            AccessStatus::Granted
        );
        assert_eq!(
            fresh
                .application
                .replicate(Bytes::from_static(b"replacement-writable-payload"))
                .await
                .unwrap(),
            2
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while source.state.progress.lock().unwrap().committed_lsn < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement primary write did not reach source secondary");

        let predecessor = fresh
            .handle()
            .stateless_preview_process_endpoint(ReplicaRole::Primary)
            .await
            .unwrap();
        let predecessor_session = predecessor.process_session_id.clone();
        let fresh_root = fresh.root.clone();
        let fresh_identity = fresh.identity.clone();
        let fresh_storage = fresh.storage.clone();
        let fresh_state = fresh.state.clone();
        fresh.stop().await;
        let restarted_address = free_address();
        let restarted_application = Arc::new(PreviewApplication::new(
            fresh_state.clone(),
            format!("http://{restarted_address}"),
        ));
        let mut restarted = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("controller-fresh-pod"),
                pvc_uid: PvcUid::new("controller-fresh-pvc"),
                data_root: fresh_root.clone(),
                control_address: free_address(),
                replication_address: restarted_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            restarted_application.clone(),
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_stateless_preview(PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        })
        .start()
        .await
        .unwrap();
        let restarted_handle = restarted.handle();
        let renewal_epoch = Epoch::new(0, 3);
        let source_endpoint = source
            .handle()
            .stateless_preview_process_endpoint(ReplicaRole::ActiveSecondary)
            .await
            .unwrap();
        let renewal_configuration = ConfigurationDescriptor::new(
            renewal_epoch,
            fresh_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: fresh_identity.clone(),
                    role: ReplicaRole::Primary,
                },
            ],
            2,
        );
        restarted_handle
            .authorize_stateless_preview_session_renewal(ProcessSessionRenewalRequired {
                operation_id: OperationId::new("controller-fresh-restart"),
                reason: ProcessSessionRenewalReason::PrimaryReplacement,
                resource_uid: resource_uid.clone(),
                history: history.clone(),
                previous_epoch: promoted_epoch,
                renewal_epoch,
                predecessor,
                replacement: Some(ProcessSessionEndpoint {
                    identity: fresh_identity.clone(),
                    storage_incarnation: fresh_storage.clone(),
                    process_session_id: restarted_handle.process_session_id(),
                    replicator_creation_id: ReplicatorCreationId::new(
                        "controller-fresh-restart-creation",
                    ),
                    role: ReplicaRole::None,
                }),
                surviving_sessions: vec![source_endpoint],
                fresh_configuration: renewal_configuration.clone(),
                predecessor_contained: true,
                data_loss: None,
            })
            .await
            .unwrap();
        source
            .handle()
            .execute_stateless_preview(intent(
                &preview,
                source.handle().process_session_id(),
                12,
                "controller-source-renewal-epoch",
                PublicOperationProgram::Epoch {
                    epoch: renewal_epoch,
                },
            ))
            .await
            .unwrap();
        for (revision, name, program) in [
            (
                7,
                "controller-restart-open",
                PublicOperationProgram::Open {
                    replica: fresh_identity.clone(),
                    existing: true,
                },
            ),
            (
                8,
                "controller-restart-role",
                PublicOperationProgram::Role {
                    epoch: renewal_epoch,
                    role: ReplicaRole::Primary,
                },
            ),
            (
                9,
                "controller-restart-epoch",
                PublicOperationProgram::Epoch {
                    epoch: renewal_epoch,
                },
            ),
            (
                10,
                "controller-restart-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: renewal_configuration,
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: source.identity.clone(),
                        replication_address: format!("http://{}", source.address),
                        process_session_id: source.handle().process_session_id(),
                        storage_incarnation: Some(source.storage.clone()),
                        role: ReplicaRole::ActiveSecondary,
                        current_progress: 2,
                        catch_up_capability: 2,
                        must_catchup: false,
                    }],
                }),
            ),
        ] {
            restarted_handle
                .execute_stateless_preview(intent(
                    &preview,
                    restarted_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        assert_eq!(
            restarted_handle
                .stateless_preview_report()
                .await
                .unwrap()
                .write_status,
            AccessStatus::Granted
        );
        assert!(
            fresh_state
                .operations
                .lock()
                .unwrap()
                .iter()
                .any(|operation| {
                    operation.lsn == 2 && operation.data.as_ref() == b"replacement-writable-payload"
                })
        );
        let before = *fresh_state.progress.lock().unwrap();
        assert!(
            restarted_handle
                .execute_stateless_preview(intent(
                    &preview,
                    predecessor_session,
                    11,
                    "controller-predecessor-session",
                    PublicOperationProgram::Progress { capability: false },
                ))
                .await
                .is_err()
        );
        let mut client = proto::replication_data_client::ReplicationDataClient::connect(format!(
            "http://{restarted_address}"
        ))
        .await
        .unwrap();
        let stale_item = proto::ReplicationItem {
            protocol_version: 11,
            sender: Some(source.identity.clone().into()),
            receiver: Some(retired_identity.into()),
            epoch: Some(epoch.into()),
            previous_configuration_id: String::new(),
            current_configuration_id: "retired-controller-configuration".into(),
            lsn: 99,
            committed_lsn: 98,
            data: b"retired-controller-payload".to_vec(),
            sender_session_id: source.handle().process_session_id().to_string(),
            receiver_session_id: stale_session.to_string(),
        };
        let mut response = client
            .replicate(tonic::Request::new(stream::iter([stale_item])))
            .await
            .unwrap()
            .into_inner();
        assert!(response.next().await.unwrap().is_err());
        assert_eq!(*fresh_state.progress.lock().unwrap(), before);

        let source_predecessor = source
            .handle()
            .stateless_preview_process_endpoint(ReplicaRole::ActiveSecondary)
            .await
            .unwrap();
        let source_root = source.root.clone();
        let source_state = source.state.clone();
        source.stop().await;
        let source_restart_application = Arc::new(PreviewApplication::new(
            source_state,
            format!("http://{}", source.address),
        ));
        source.application = source_restart_application;
        source.running = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: source.identity.replica_id,
                pod_uid: PodUid::new("controller-source-pod"),
                pvc_uid: PvcUid::new("controller-source-pvc"),
                data_root: source_root,
                control_address: free_address(),
                replication_address: source.address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            source.application.clone(),
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_stateless_preview(PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        })
        .start()
        .await
        .unwrap();
        let post_restart_epoch = Epoch::new(0, 4);
        let post_restart_configuration = ConfigurationDescriptor::new(
            post_restart_epoch,
            fresh_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: fresh_identity.clone(),
                    role: ReplicaRole::Primary,
                },
            ],
            2,
        );
        source
            .handle()
            .authorize_stateless_preview_session_renewal(ProcessSessionRenewalRequired {
                operation_id: OperationId::new("controller-source-restart"),
                reason: ProcessSessionRenewalReason::ActiveSecondaryReplacement,
                resource_uid: resource_uid.clone(),
                history: history.clone(),
                previous_epoch: renewal_epoch,
                renewal_epoch: post_restart_epoch,
                predecessor: source_predecessor,
                replacement: Some(ProcessSessionEndpoint {
                    identity: source.identity.clone(),
                    storage_incarnation: source.storage.clone(),
                    process_session_id: source.handle().process_session_id(),
                    replicator_creation_id: ReplicatorCreationId::new(
                        "controller-source-restart-creation",
                    ),
                    role: ReplicaRole::None,
                }),
                surviving_sessions: vec![
                    restarted_handle
                        .stateless_preview_process_endpoint(ReplicaRole::Primary)
                        .await
                        .unwrap(),
                ],
                fresh_configuration: post_restart_configuration.clone(),
                predecessor_contained: true,
                data_loss: None,
            })
            .await
            .unwrap();
        let post_restart_adapter = HostedPublicV1Adapter::new(
            vec![
                HostedPreviewReplica {
                    identity: source.identity.clone(),
                    replication_address: format!("http://{}", source.address),
                    handle: source.handle(),
                },
                HostedPreviewReplica {
                    identity: fresh_identity.clone(),
                    replication_address: format!("http://{restarted_address}"),
                    handle: restarted_handle.clone(),
                },
            ],
            Some(accepted.clone()),
            None,
            build_evidence.clone(),
        )
        .unwrap();
        post_restart_adapter
            .reconstruct_hosted_configuration(
                &OperationId::new("controller-source-restart-authority"),
                preview.generation,
                &post_restart_configuration,
            )
            .await
            .unwrap();
        let readmit_after_restart = ConfigurationDescriptor::new(
            post_restart_epoch,
            fresh_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: fresh_identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: stale.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        assert!(
            source
                .handle()
                .execute_stateless_preview(intent(
                    &preview,
                    source.handle().process_session_id(),
                    u64::MAX - 2,
                    "readmit-retired-configuration-after-restart",
                    PublicOperationProgram::Configuration(PublicConfiguration {
                        current: readmit_after_restart,
                        previous: None,
                        replicas: vec![
                            PublicReplicaDescriptor {
                                build_id: OperationId::default(),
                                identity: fresh_identity.clone(),
                                replication_address: format!("http://{restarted_address}"),
                                process_session_id: restarted_handle.process_session_id(),
                                storage_incarnation: Some(fresh_storage.clone()),
                                role: ReplicaRole::Primary,
                                current_progress: 2,
                                catch_up_capability: 2,
                                must_catchup: false,
                            },
                            PublicReplicaDescriptor {
                                build_id: OperationId::default(),
                                identity: stale.identity.clone(),
                                replication_address: format!("http://{}", stale.address),
                                process_session_id: stale_session.clone(),
                                storage_incarnation: Some(stale.storage.clone()),
                                role: ReplicaRole::ActiveSecondary,
                                current_progress: 99,
                                catch_up_capability: 99,
                                must_catchup: false,
                            },
                        ],
                    }),
                ))
                .await
                .is_err()
        );
        let retired_build = PublicBuildInput {
            attempt: OperationId::new("readmit-retired-build-after-restart"),
            epoch: post_restart_epoch,
            current_configuration_id: Some(post_restart_configuration.configuration_id.clone()),
            replica: stale.identity.clone(),
            target_storage: stale.storage.clone(),
            process_session_id: stale_session.clone(),
            replication_address: format!("http://{}", stale.address),
            history: Some(history.clone()),
        };
        assert!(
            source
                .handle()
                .execute_stateless_preview(intent(
                    &preview,
                    source.handle().process_session_id(),
                    u64::MAX - 2,
                    "readmit-retired-build-after-restart",
                    PublicOperationProgram::Build(retired_build),
                ))
                .await
                .is_err()
        );

        source.stop().await;
        restarted.shutdown();
        restarted.wait().await.unwrap();
        source.cleanup();
        stale.cleanup();
        std::fs::remove_dir_all(fresh_root).unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn ambiguous_suffix_exactly_replays_quorum_peers_despite_higher_advertised_progress() {
    tokio::time::timeout(Duration::from_secs(180), async {
        for (case, divergent, unavailable, unverifiable) in [
            ("identical", false, false, false),
            ("divergent", true, false, false),
            ("unavailable", false, true, false),
            ("unverifiable", false, false, true),
        ] {
            let resource_uid = ResourceUid::new(format!("suffix-{case}"));
            let preview = PublicOperationPreviewIdentity::public_v1(12);
            let binding = PreviewLifecycleBinding {
                preview: preview.clone(),
                resource_uid: resource_uid.clone(),
                spec_generation: 1,
                state_persistence: StatePersistence::Persisted,
            };
            let primary_identity = replica_named(
                1,
                &format!("{case}-primary-pod"),
                &format!("{case}-primary-agent"),
            );
            let secondary_identity = replica_named(
                2,
                &format!("{case}-secondary-pod"),
                &format!("{case}-secondary-agent"),
            );
            let primary_root = test_root(&format!("{case}-suffix-primary"));
            let secondary_root = test_root(&format!("{case}-suffix-secondary"));
            for (root, identity, pod, pvc) in [
                (
                    &primary_root,
                    primary_identity.clone(),
                    format!("{case}-primary-pod"),
                    format!("{case}-primary-pvc"),
                ),
                (
                    &secondary_root,
                    secondary_identity.clone(),
                    format!("{case}-secondary-pod"),
                    format!("{case}-secondary-pvc"),
                ),
            ] {
                drop(
                    SqliteStore::create_public_v1(
                        SqliteStore::metadata_database_path(root),
                        preview.clone(),
                        store_identity_named(
                            resource_uid.clone(),
                            identity,
                            pod.as_str(),
                            pvc.as_str(),
                        ),
                    )
                    .unwrap(),
                );
            }
            let primary_address = free_address();
            let secondary_address = free_address();
            let primary_state = Arc::new(MemoryState::default());
            let secondary_state = Arc::new(MemoryState::default());
            let primary_app = Arc::new(PreviewApplication::new(
                primary_state.clone(),
                format!("http://{primary_address}"),
            ));
            let mut primary = ReplicaHost::new(
                ReplicaProcessConfig {
                    resource_uid: resource_uid.clone(),
                    replica_id: ReplicaId::new(1),
                    pod_uid: PodUid::new(format!("{case}-primary-pod")),
                    pvc_uid: PvcUid::new(format!("{case}-primary-pvc")),
                    data_root: primary_root.clone(),
                    control_address: free_address(),
                    replication_address: primary_address,
                    bearer_token: "preview-token".into(),
                    rpc_deadline: Duration::from_secs(2),
                    transport_window_capacity: 8,
                },
                primary_app.clone(),
                ApplicationStorageState::FreshEmpty,
                Arc::new(Resolver),
            )
            .with_stateless_preview(binding.clone())
            .start()
            .await
            .unwrap();
            let mut secondary = ReplicaHost::new(
                ReplicaProcessConfig {
                    resource_uid: resource_uid.clone(),
                    replica_id: ReplicaId::new(2),
                    pod_uid: PodUid::new(format!("{case}-secondary-pod")),
                    pvc_uid: PvcUid::new(format!("{case}-secondary-pvc")),
                    data_root: secondary_root.clone(),
                    control_address: free_address(),
                    replication_address: secondary_address,
                    bearer_token: "preview-token".into(),
                    rpc_deadline: Duration::from_secs(2),
                    transport_window_capacity: 8,
                },
                Arc::new(PreviewApplication::new(
                    secondary_state.clone(),
                    format!("http://{secondary_address}"),
                )),
                ApplicationStorageState::FreshEmpty,
                Arc::new(Resolver),
            )
            .with_stateless_preview(binding)
            .start()
            .await
            .unwrap();
            let primary_handle = primary.handle();
            let secondary_handle = secondary.handle();
            let initialization =
                derive_history_initialization_id(&resource_uid, PublicProtocolGeneration::new(12));
            let history = derive_bootstrap_history(
                &resource_uid,
                PublicProtocolGeneration::new(12),
                &initialization,
            );
            for (handle, identity, pod, pvc) in [
                (
                    &primary_handle,
                    primary_identity.clone(),
                    format!("{case}-primary-pod"),
                    format!("{case}-primary-pvc"),
                ),
                (
                    &secondary_handle,
                    secondary_identity.clone(),
                    format!("{case}-secondary-pod"),
                    format!("{case}-secondary-pvc"),
                ),
            ] {
                handle
                    .initialize_stateless_preview_history(
                        &initialization,
                        &history,
                        &storage_named(identity, pod.as_str(), pvc.as_str()),
                    )
                    .await
                    .unwrap();
            }
            let epoch = Epoch::new(0, 1);
            let descriptor = ConfigurationDescriptor::new(
                epoch,
                ReplicaId::new(1),
                vec![
                    ConfigurationMember {
                        identity: primary_identity.clone(),
                        role: ReplicaRole::Primary,
                    },
                    ConfigurationMember {
                        identity: secondary_identity.clone(),
                        role: ReplicaRole::ActiveSecondary,
                    },
                ],
                2,
            );
            install_static_authority(
                &primary_handle,
                &preview,
                primary_identity.clone(),
                ReplicaRole::Primary,
                epoch,
                PublicConfiguration {
                    current: descriptor.clone(),
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: secondary_identity.clone(),
                        replication_address: format!("http://{secondary_address}"),
                        process_session_id: secondary_handle.process_session_id(),
                        storage_incarnation: Some(storage_named(
                            secondary_identity.clone(),
                            "secondary-pod",
                            "secondary-pvc",
                        )),
                        role: ReplicaRole::ActiveSecondary,
                        current_progress: 0,
                        catch_up_capability: 0,
                        must_catchup: true,
                    }],
                },
            )
            .await;
            install_static_authority(
                &secondary_handle,
                &preview,
                secondary_identity.clone(),
                ReplicaRole::ActiveSecondary,
                epoch,
                PublicConfiguration {
                    current: descriptor.clone(),
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: primary_identity.clone(),
                        replication_address: format!("http://{primary_address}"),
                        process_session_id: primary_handle.process_session_id(),
                        storage_incarnation: Some(storage_named(
                            primary_identity.clone(),
                            "primary-pod",
                            "primary-pvc",
                        )),
                        role: ReplicaRole::Primary,
                        current_progress: 0,
                        catch_up_capability: 0,
                        must_catchup: false,
                    }],
                },
            )
            .await;
            assert_eq!(
                primary_app
                    .replicate(Bytes::from_static(b"committed"))
                    .await
                    .unwrap(),
                1
            );
            primary_state.fail_next_commit();
            assert!(
                primary_app
                    .replicate(Bytes::from_static(b"ambiguous"))
                    .await
                    .is_err()
            );
            assert_eq!(
                *primary_state.progress.lock().unwrap(),
                DurableApplicationProgress {
                    applied_lsn: 2,
                    committed_lsn: 1,
                }
            );
            secondary_state
                .apply(Operation {
                    lsn: 3,
                    committed_lsn: 2,
                    data: Bytes::from_static(b"peer-higher-progress"),
                })
                .await
                .unwrap();
            secondary_state.commit(3).await.unwrap();
            if divergent {
                secondary_state
                    .operations
                    .lock()
                    .unwrap()
                    .iter_mut()
                    .find(|operation| operation.lsn == 2)
                    .unwrap()
                    .data = Bytes::from_static(b"divergent");
            }
            if unverifiable {
                primary_state
                    .operations
                    .lock()
                    .unwrap()
                    .retain(|operation| operation.lsn != 2);
            }
            if unavailable {
                secondary.shutdown();
                secondary.wait().await.unwrap();
            }
            primary_handle
                .execute_stateless_preview(intent(
                    &preview,
                    primary_handle.process_session_id(),
                    5,
                    &format!("advertise-higher-{case}"),
                    PublicOperationProgram::Configuration(PublicConfiguration {
                        current: descriptor,
                        previous: None,
                        replicas: vec![PublicReplicaDescriptor {
                            build_id: OperationId::default(),
                            identity: secondary_identity.clone(),
                            replication_address: format!("http://{secondary_address}"),
                            process_session_id: secondary_handle.process_session_id(),
                            storage_incarnation: Some(storage_named(
                                secondary_identity.clone(),
                                "secondary-pod",
                                "secondary-pvc",
                            )),
                            role: ReplicaRole::ActiveSecondary,
                            current_progress: 99,
                            catch_up_capability: 99,
                            must_catchup: true,
                        }],
                    }),
                ))
                .await
                .unwrap();
            let settlement = primary_app.wait_for_write_quorum().await;
            let report = primary_handle.stateless_preview_report().await.unwrap();
            if divergent {
                assert!(matches!(
                    settlement,
                    Err(RuntimeError::ReplicaRebuildRequired(rebuild))
                        if matches!(
                            rebuild.as_ref(),
                            kuberic_runtime::protocol::public_operations::
                                ReplicaRebuildRequired::RemoteBuildTarget(target)
                                if target.target == secondary_identity
                                    && target.reason
                                        == kuberic_runtime::protocol::public_operations::
                                            ReplicaRebuildReason::IncompatibleRetainedHistory
                        )
                ));
                assert_eq!(
                    report.write_status,
                    kuberic_runtime::protocol::types::AccessStatus::NotPrimary
                );
                assert_eq!(primary_state.progress.lock().unwrap().committed_lsn, 1);
            } else if unverifiable {
                assert!(matches!(
                    settlement,
                    Err(RuntimeError::ReplicaRebuildRequired(rebuild))
                        if matches!(
                            rebuild.as_ref(),
                            kuberic_runtime::protocol::public_operations::
                                ReplicaRebuildRequired::LocalProvider(local)
                                if local.provider.replica == primary_identity
                                    && local.reason
                                        == kuberic_runtime::protocol::public_operations::
                                            ReplicaRebuildReason::IncompatibleRetainedHistory
                        )
                ));
                assert_eq!(
                    report.write_status,
                    kuberic_runtime::protocol::types::AccessStatus::NotPrimary
                );
                assert_eq!(primary_state.progress.lock().unwrap().committed_lsn, 1);
            } else if unavailable {
                assert!(matches!(
                    settlement,
                    Err(RuntimeError::ReconfigurationPending)
                ));
                assert_eq!(
                    report.write_status,
                    kuberic_runtime::protocol::types::AccessStatus::NotPrimary
                );
                assert_eq!(primary_state.progress.lock().unwrap().committed_lsn, 1);
            } else {
                settlement.unwrap();
                assert_eq!(primary_state.progress.lock().unwrap().committed_lsn, 2);
                assert_eq!(secondary_state.progress.lock().unwrap().committed_lsn, 3);
                primary_handle
                    .execute_stateless_preview(intent(
                        &preview,
                        primary_handle.process_session_id(),
                        6,
                        "refresh-settled-progress",
                        PublicOperationProgram::Progress { capability: false },
                    ))
                    .await
                    .unwrap();
                let report = primary_handle.stateless_preview_report().await.unwrap();
                assert_eq!(
                    report.write_status,
                    kuberic_runtime::protocol::types::AccessStatus::Granted
                );
            }

            primary.shutdown();
            primary.wait().await.unwrap();
            if !unavailable {
                secondary.shutdown();
                secondary.wait().await.unwrap();
            }
            std::fs::remove_dir_all(primary_root).unwrap();
            std::fs::remove_dir_all(secondary_root).unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn production_config_has_no_preview_selector_and_stateless_source_has_no_legacy_store_or_auth()
 {
    tokio::time::timeout(Duration::from_secs(30), async {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let process = std::fs::read_to_string(manifest.join("src/host/process.rs")).unwrap();
        let config = process
            .split("pub struct ReplicaProcessConfig")
            .nth(1)
            .unwrap()
            .split('}')
            .next()
            .unwrap();
        assert!(!config.contains("preview"));
        let stateless =
            std::fs::read_to_string(manifest.join("src/replicator/stateless/mod.rs")).unwrap();
        for forbidden in [
            "ManagedReplicaStore",
            "ReplicationProgressStore",
            "LocalWriteJournal",
            "BuildAuthorityStore",
            "BuildProgressStore",
            "bearer_token",
            "authorize_request",
        ] {
            assert!(
                !stateless.contains(forbidden),
                "stateless preview contains forbidden dependency: {forbidden}"
            );
        }
    })
    .await
    .unwrap();
}
