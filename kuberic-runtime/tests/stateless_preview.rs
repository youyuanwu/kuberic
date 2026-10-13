#![cfg(feature = "testing")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, stream};
use kuberic_runtime::application::{
    CopyChunk, DurableApplicationProgress, OpenContext, Operation, OperationDataStream, RoleChange,
};
use kuberic_runtime::control::proto;
use kuberic_runtime::engine::{DurableState, RetainedOperationStream};
use kuberic_runtime::host::{ApplicationStorageState, ReplicaHost, ReplicaProcessConfig};
use kuberic_runtime::protocol::public_operations::{
    PreviewLifecycleBinding, PublicConfiguration, PublicOperationClass, PublicOperationIntent,
    PublicOperationPreviewIdentity, PublicOperationProgram, PublicReplicaDescriptor,
    StatePersistence,
};
use kuberic_runtime::protocol::types::{
    AgentGeneration, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy, Epoch,
    InitializationId, OperationId, PodUid, PublicProtocolGeneration, PvcUid, ReplicaId,
    ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity,
    ReplicaStorageIncarnationContext, ResourceUid, StorageGeneration, derive_agent_generation,
    derive_bootstrap_history, derive_history_initialization_id, derive_initialization_id,
};
use kuberic_runtime::replicator::stateless::StatelessReplicatorFactory;
use kuberic_runtime::replicator::{
    DefaultReplicatorFactory, Replicator, ReplicatorFactory, ReplicatorFactoryContext,
    ReplicatorInterfaces, ReplicatorSettings, StateReplicator, StatefulServicePartition,
};
use kuberic_runtime::testing::sqlite_store::SqliteStore;
use kuberic_runtime::testing::state::{AgentState, SCHEMA_VERSION, StorageIdentity};
use kuberic_runtime::{Result, RuntimeError, StateProvider, StatefulServiceReplica};
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
    block_commit: AtomicBool,
    fail_commit: AtomicBool,
    commit_entered: Notify,
    release_commit: Notify,
}

impl MemoryState {
    fn block_commit(&self) {
        self.block_commit.store(true, Ordering::Release);
    }

    fn fail_next_commit(&self) {
        self.fail_commit.store(true, Ordering::Release);
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
        Ok(Box::pin(stream::empty()))
    }

    async fn on_data_loss(&self) -> Result<bool> {
        Ok(false)
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
        _sequence: u64,
        _chunk: CopyChunk,
    ) -> Result<()> {
        Ok(())
    }

    async fn verify_copy_chunk(
        &self,
        _build_id: &OperationId,
        _sequence: u64,
        _chunk: &CopyChunk,
    ) -> Result<bool> {
        Ok(true)
    }

    async fn finish_copy(
        &self,
        _build_id: &OperationId,
        up_to_lsn: i64,
        committed_lsn: i64,
    ) -> Result<DurableApplicationProgress> {
        Ok(DurableApplicationProgress {
            applied_lsn: up_to_lsn,
            committed_lsn,
        })
    }

    async fn apply(&self, operation: Operation) -> Result<DurableApplicationProgress> {
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
        Ok(*progress)
    }

    async fn durable_progress(&self) -> Result<DurableApplicationProgress> {
        Ok(*self.progress.lock().unwrap())
    }

    async fn verify_applied(&self, operation: &Operation) -> Result<bool> {
        Ok(self.operations.lock().unwrap().contains(operation))
    }

    async fn commit(&self, committed_lsn: i64) -> Result<DurableApplicationProgress> {
        if self.block_commit.load(Ordering::Acquire) {
            self.commit_entered.notify_waiters();
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
    partition: RwLock<Option<StatefulServicePartition>>,
    legacy_factory: bool,
    fail_first_creation: AtomicBool,
    opens: AtomicU64,
    factory_calls: Arc<AtomicU64>,
    failed_factory_calls: Arc<AtomicU64>,
    metadata_database: Option<PathBuf>,
    instruction_seen_before_open: AtomicBool,
}

impl PreviewApplication {
    fn new(storage: Arc<MemoryState>, replication_address: String) -> Self {
        Self {
            storage,
            replication_address,
            control: RwLock::new(None),
            state_replicator: RwLock::new(None),
            partition: RwLock::new(None),
            legacy_factory: false,
            fail_first_creation: AtomicBool::new(false),
            opens: AtomicU64::new(0),
            factory_calls: Arc::new(AtomicU64::new(0)),
            failed_factory_calls: Arc::new(AtomicU64::new(0)),
            metadata_database: None,
            instruction_seen_before_open: AtomicBool::new(false),
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
        self.state_replicator.write().await.take();
        self.control.write().await.take();
        Ok(())
    }

    fn abort(&self) {}
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
async fn preview_transport_fences_predecessor_sessions_and_closes_with_its_owner() {
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
            secondary_app,
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
        assert_eq!(applied_only.progress.first, 0);
        assert_eq!(applied_only.progress.last, 1);
        assert_eq!(
            applied_only.read_status,
            kuberic_runtime::protocol::types::AccessStatus::NotPrimary
        );
        secondary_state.release_commit.notify_waiters();
        assert_eq!(write.await.unwrap().unwrap(), 1);
        assert_eq!(primary_state.progress.lock().unwrap().committed_lsn, 1);
        assert_eq!(secondary_state.progress.lock().unwrap().committed_lsn, 1);
        let committed = secondary_handle.stateless_preview_report().await.unwrap();
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
            ReplicaId::new(1),
            descriptor.members.clone(),
            2,
        );
        let secondary_descriptor = || PublicReplicaDescriptor {
            build_id: OperationId::default(),
            identity: secondary_identity.clone(),
            replication_address: format!("http://{secondary_address}"),
            process_session_id: secondary_handle.process_session_id(),
            role: ReplicaRole::ActiveSecondary,
            current_progress: 1,
            catch_up_capability: 1,
            must_catchup: true,
        };
        let mismatch = intent(
            &preview,
            primary_handle.process_session_id(),
            5,
            "capability-mismatch",
            PublicOperationProgram::Swap {
                starting: PublicConfiguration {
                    current: descriptor.clone(),
                    previous: None,
                    replicas: vec![secondary_descriptor()],
                },
                refreshed: PublicConfiguration {
                    current: refreshed,
                    previous: None,
                    replicas: vec![secondary_descriptor()],
                },
                epoch: Epoch::new(0, 2),
                handoff: ReplicaRole::ActiveSecondary,
                catch_up_specific_quorum: true,
                mode: kuberic_runtime::protocol::public_operations::PublicCatchUpMode::WriteQuorum,
            },
        );
        assert!(primary_handle.execute_stateless_preview(mismatch).await.is_err());
        assert_eq!(
            primary_handle
                .stateless_preview_report()
                .await
                .unwrap()
                .write_status,
            kuberic_runtime::protocol::types::AccessStatus::NotPrimary
        );

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
                applied_lsn: 3,
                committed_lsn: 2,
            }
        );
        std::net::TcpListener::bind(secondary_address).unwrap();
        primary.shutdown();
        primary.wait().await.unwrap();
        let restart_app = Arc::new(PreviewApplication::new(
            primary_state,
            format!("http://{primary_address}"),
        ));
        let mut restarted = ReplicaHost::new(
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
            restart_app.clone(),
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_stateless_preview(PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid,
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        })
        .start()
        .await
        .unwrap();
        let restarted_handle = restarted.handle();
        assert_eq!(restarted_handle.stateless_preview_provider_writer_count(), 0);
        assert_eq!(restart_app.opens.load(Ordering::SeqCst), 0);
        assert!(restarted_handle.stateless_preview_report().await.is_err());
        std::net::TcpListener::bind(primary_address).unwrap();
        restarted.shutdown();
        restarted.wait().await.unwrap();
        std::fs::remove_dir_all(primary_root).unwrap();
        std::fs::remove_dir_all(secondary_root).unwrap();
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
