use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;

use crate::application::{
    OpenContext, OperationDataStream, RoleChange, StateProvider, StatefulServiceReplica,
};
use crate::host::operation::{CallbackContainment, PartitionOperationRegistry};
use crate::host::operation_recovery::PartitionOperationRuntime;
use crate::host::sqlite_store::SqliteStore;
use crate::host::state::{
    AgentState, PublicOperationContainment, PublicOperationDisposition, PublicOperationStage,
    SCHEMA_VERSION, StorageIdentity,
};
use crate::host::store::AgentStore;
use crate::protocol::public_operations::{
    PublicOperationClass, PublicOperationIntent, PublicOperationPreviewIdentity,
};
use crate::protocol::types::{
    AgentGeneration, ConfigurationDescriptor, EffectivePolicy, Epoch, InitializationId,
    OperationId, PodUid, ProcessSessionId, PvcUid, ReplicaId, ReplicaIdentity, ReplicaInstanceId,
    ReplicaRole, ResourceUid,
};
use crate::replicator::{
    PrimaryReplicator, ReplicaInformation, ReplicaSetConfiguration, ReplicaSetQuorumMode,
    Replicator, ReplicatorSettings, StateReplicator,
};

use super::tempdir;

fn replica_identity() -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(1),
        instance_id: ReplicaInstanceId::new("pod-1"),
        agent_generation: AgentGeneration::new("generation-1"),
    }
}

fn storage_identity() -> StorageIdentity {
    StorageIdentity {
        schema_version: SCHEMA_VERSION,
        resource_uid: ResourceUid::new("resource-1"),
        pod_uid: PodUid::new("pod-1"),
        pvc_uid: PvcUid::new("pvc-1"),
        initialization_id: InitializationId::new("init-1"),
        local_identity: replica_identity(),
        effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
    }
}

fn intent(
    preview: &PublicOperationPreviewIdentity,
    operation_id: &str,
    revision: u64,
    class: PublicOperationClass,
    session: &str,
) -> PublicOperationIntent {
    PublicOperationIntent {
        preview: preview.clone(),
        operation_id: OperationId::new(operation_id),
        revision,
        process_session_id: ProcessSessionId::new(session),
        class,
        input_digest: format!("digest-{operation_id}-{revision}"),
    }
}

fn preview_store(
    preview: &PublicOperationPreviewIdentity,
) -> (tempfile::TempDir, std::path::PathBuf, Arc<SqliteStore>) {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let store = SqliteStore::create_preview_authorized(
        &path,
        AgentState::new(storage_identity()),
        preview.clone(),
    )
    .unwrap();
    (directory, path, Arc::new(store))
}

async fn wait_for_stage(
    operation: &Arc<crate::host::operation::PartitionOperation>,
    expected: PublicOperationStage,
) -> crate::host::state::PublicOperationRecord {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let record = operation.snapshot();
            if record.stage == expected {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn preview_store_rejects_legacy_and_mixed_openers() {
    let preview = PublicOperationPreviewIdentity::new(1);
    let (_directory, path, store) = preview_store(&preview);
    drop(store);
    let connection = rusqlite::Connection::open(&path).unwrap();
    let version: u32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        version,
        crate::host::state::PUBLIC_OPERATION_PREVIEW_SCHEMA_VERSION
    );
    drop(connection);

    assert!(matches!(
        SqliteStore::open_existing(&path, None),
        Err(crate::host::HostError::SchemaMismatch {
            expected: SCHEMA_VERSION,
            observed: crate::host::state::PUBLIC_OPERATION_PREVIEW_SCHEMA_VERSION,
        })
    ));
    assert!(matches!(
        SqliteStore::open_preview_existing(
            &path,
            Some(&storage_identity()),
            &PublicOperationPreviewIdentity::new(2),
        ),
        Err(crate::host::HostError::IdentityMismatch(message))
            if message.contains("preview identity")
    ));
    SqliteStore::open_preview_existing(&path, Some(&storage_identity()), &preview).unwrap();

    let legacy_directory = tempdir().unwrap();
    let legacy_path = SqliteStore::metadata_database_path(legacy_directory.path());
    drop(
        SqliteStore::create_authorized(&legacy_path, AgentState::new(storage_identity())).unwrap(),
    );
    assert!(matches!(
        SqliteStore::open_preview_existing(&legacy_path, None, &preview),
        Err(crate::host::HostError::SchemaMismatch {
            expected: crate::host::state::PUBLIC_OPERATION_PREVIEW_SCHEMA_VERSION,
            observed: SCHEMA_VERSION,
        })
    ));
}

#[tokio::test]
async fn dropped_waiter_does_not_release_owned_callback_or_supersession() {
    let preview = PublicOperationPreviewIdentity::new(7);
    let (_directory, _path, store) = preview_store(&preview);
    let registry =
        PartitionOperationRegistry::new(store, preview.clone(), ProcessSessionId::new("session-1"))
            .unwrap();

    let first = registry
        .admit(intent(
            &preview,
            "authority-1",
            1,
            PublicOperationClass::Authority,
            "session-1",
        ))
        .await
        .unwrap();
    let blocked = Arc::new(Notify::new());
    let callback = blocked.clone();
    first
        .spawn_root(CallbackContainment::ObjectOwned, async move {
            callback.notified().await;
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();

    let waiter = {
        let first = first.clone();
        tokio::spawn(async move { first.wait_for_terminal().await })
    };
    waiter.abort();
    let _ = waiter.await;

    let second = registry
        .admit(intent(
            &preview,
            "authority-2",
            2,
            PublicOperationClass::Authority,
            "session-1",
        ))
        .await
        .unwrap();
    let second_id = second.intent().operation_id;
    drop(second);
    wait_for_stage(&first, PublicOperationStage::ContainmentPending).await;
    let second = registry.operation(&second_id).await.unwrap();
    wait_for_stage(&second, PublicOperationStage::WaitingForContainment).await;

    registry
        .complete_containment(&OperationId::new("authority-1"))
        .await
        .unwrap();
    assert_eq!(second.snapshot().stage, PublicOperationStage::Ready);
    second
        .spawn_root(CallbackContainment::RootTask, async {
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
    let completed = tokio::time::timeout(Duration::from_secs(1), second.wait_for_terminal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed.stage, PublicOperationStage::Completed);
    assert_eq!(
        completed.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    assert_eq!(completed.containment, PublicOperationContainment::Complete);
}

#[tokio::test]
async fn changed_duplicate_and_stale_revision_are_rejected() {
    let preview = PublicOperationPreviewIdentity::new(3);
    let (_directory, _path, store) = preview_store(&preview);
    let registry =
        PartitionOperationRegistry::new(store, preview.clone(), ProcessSessionId::new("session-1"))
            .unwrap();
    let original = intent(
        &preview,
        "authority",
        2,
        PublicOperationClass::Authority,
        "session-1",
    );
    let operation = registry.admit(original.clone()).await.unwrap();
    assert!(Arc::ptr_eq(
        &operation,
        &registry.admit(original).await.unwrap()
    ));

    let mut changed = operation.intent();
    changed.input_digest = "changed".into();
    assert!(matches!(
        registry.admit(changed).await,
        Err(crate::host::HostError::DurableEffectConflict(_))
    ));
    assert!(matches!(
        registry
            .admit(intent(
                &preview,
                "older",
                1,
                PublicOperationClass::Authority,
                "session-1",
            ))
            .await,
        Err(crate::host::HostError::CommandRejected(message))
            if message.contains("not newer")
    ));
}

#[tokio::test]
async fn terminal_admission_cancels_an_unstarted_operation_without_fake_containment() {
    let preview = PublicOperationPreviewIdentity::new(4);
    let (_directory, _path, store) = preview_store(&preview);
    let registry =
        PartitionOperationRegistry::new(store, preview.clone(), ProcessSessionId::new("session-1"))
            .unwrap();
    let authority = registry
        .admit(intent(
            &preview,
            "authority",
            1,
            PublicOperationClass::Authority,
            "session-1",
        ))
        .await
        .unwrap();
    let abort = registry
        .admit(intent(
            &preview,
            "abort",
            2,
            PublicOperationClass::Abort,
            "session-1",
        ))
        .await
        .unwrap();
    wait_for_stage(&authority, PublicOperationStage::Completed).await;
    assert_eq!(
        authority.snapshot().disposition,
        Some(PublicOperationDisposition::Cancelled)
    );
    wait_for_stage(&abort, PublicOperationStage::Ready).await;
}

#[tokio::test]
async fn failed_object_callback_retains_diagnostic_until_containment_completes() {
    let preview = PublicOperationPreviewIdentity::new(8);
    let (_directory, _path, store) = preview_store(&preview);
    let registry =
        PartitionOperationRegistry::new(store, preview.clone(), ProcessSessionId::new("session-1"))
            .unwrap();
    let operation = registry
        .admit(intent(
            &preview,
            "failed-object",
            1,
            PublicOperationClass::Authority,
            "session-1",
        ))
        .await
        .unwrap();
    operation
        .spawn_root(CallbackContainment::ObjectOwned, async {
            Err::<(), _>("callback failed")
        })
        .await
        .unwrap();
    let pending = operation.wait_for_terminal().await.unwrap();
    assert_eq!(pending.stage, PublicOperationStage::ContainmentPending);
    assert_eq!(pending.containment, PublicOperationContainment::Pending);
    assert!(matches!(
        &pending.disposition,
        Some(PublicOperationDisposition::Failed(message))
            if message == "callback failed"
    ));

    let completed = registry
        .complete_containment(&pending.intent.operation_id)
        .await
        .unwrap();
    assert_eq!(completed.stage, PublicOperationStage::Completed);
    assert_eq!(completed.containment, PublicOperationContainment::Complete);
    assert_eq!(completed.disposition, pending.disposition);
}

#[tokio::test]
async fn launch_and_cancel_are_serialized_without_detached_root_work() {
    for target in 2..8 {
        let preview = PublicOperationPreviewIdentity::new(target as u64);
        let (_directory, _path, store) = preview_store(&preview);
        let registry = PartitionOperationRegistry::new(
            store,
            preview.clone(),
            ProcessSessionId::new("session-1"),
        )
        .unwrap();
        let operation = registry
            .admit(intent(
                &preview,
                &format!("build-{target}"),
                1,
                PublicOperationClass::Build {
                    target: ReplicaId::new(target),
                },
                "session-1",
            ))
            .await
            .unwrap();
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_started = started.clone();
        let launch = {
            let operation = operation.clone();
            tokio::spawn(async move {
                operation
                    .spawn_root(CallbackContainment::RootTask, async move {
                        callback_started.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                        std::future::pending::<()>().await;
                        Ok::<(), Infallible>(())
                    })
                    .await
            })
        };
        let cancel = {
            let operation = operation.clone();
            tokio::spawn(async move { operation.cancel_root().await })
        };
        let (launch, cancel) = tokio::join!(launch, cancel);
        let launch = launch.unwrap();
        let cancel = cancel.unwrap();
        assert!(
            launch.is_ok() || matches!(launch, Err(crate::host::HostError::CommandRejected(_)))
        );
        cancel.unwrap();
        assert_eq!(operation.snapshot().stage, PublicOperationStage::Completed);
        let observed = started.load(std::sync::atomic::Ordering::Acquire);
        tokio::task::yield_now().await;
        assert_eq!(started.load(std::sync::atomic::Ordering::Acquire), observed);
    }
}

#[tokio::test]
async fn completion_persistence_failure_is_reported_and_recovered_as_ambiguous() {
    let preview = PublicOperationPreviewIdentity::new(30);
    let (_directory, _path, store) = preview_store(&preview);
    let registry = PartitionOperationRegistry::new(
        store.clone(),
        preview.clone(),
        ProcessSessionId::new("session-1"),
    )
    .unwrap();
    let operation = registry
        .admit(intent(
            &preview,
            "persistence-cut",
            1,
            PublicOperationClass::Authority,
            "session-1",
        ))
        .await
        .unwrap();
    let release = Arc::new(Notify::new());
    let callback_release = release.clone();
    operation
        .spawn_root(CallbackContainment::RootTask, async move {
            callback_release.notified().await;
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
    store.fail_next_public_operation_advance();
    release.notify_one();
    let error = tokio::time::timeout(Duration::from_secs(1), operation.wait_for_terminal())
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("injected public-operation"));

    let recovered = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            registry.recover_unowned().await.unwrap();
            let record = operation.snapshot();
            if record.stage == PublicOperationStage::ContainmentPending {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        recovered.disposition,
        Some(PublicOperationDisposition::Ambiguous(_))
    ));
}

#[tokio::test]
async fn retained_authority_and_abort_records_fence_later_admission() {
    let preview = PublicOperationPreviewIdentity::new(40);
    let (_directory, _path, store) = preview_store(&preview);
    let registry =
        PartitionOperationRegistry::new(store, preview.clone(), ProcessSessionId::new("session-1"))
            .unwrap();
    let authority = registry
        .admit(intent(
            &preview,
            "authority-4",
            4,
            PublicOperationClass::Authority,
            "session-1",
        ))
        .await
        .unwrap();
    authority
        .spawn_root(CallbackContainment::RootTask, async {
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
    authority.wait_for_terminal().await.unwrap();
    assert!(matches!(
        registry
            .admit(intent(
                &preview,
                "authority-stale",
                4,
                PublicOperationClass::Authority,
                "session-1",
            ))
            .await,
        Err(crate::host::HostError::CommandRejected(message))
            if message.contains("retained authority")
    ));
    assert_eq!(
        registry
            .admit(intent(
                &preview,
                "authority-5",
                5,
                PublicOperationClass::Authority,
                "session-1",
            ))
            .await
            .unwrap()
            .snapshot()
            .stage,
        PublicOperationStage::Ready
    );

    let terminal_preview = PublicOperationPreviewIdentity::new(41);
    let (_directory, _path, store) = preview_store(&terminal_preview);
    let terminal_registry = PartitionOperationRegistry::new(
        store,
        terminal_preview.clone(),
        ProcessSessionId::new("session-1"),
    )
    .unwrap();
    let abort = terminal_registry
        .admit(intent(
            &terminal_preview,
            "abort",
            1,
            PublicOperationClass::Abort,
            "session-1",
        ))
        .await
        .unwrap();
    abort
        .spawn_root(CallbackContainment::RootTask, async {
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
    abort.wait_for_terminal().await.unwrap();
    assert!(matches!(
        terminal_registry
            .admit(intent(
                &terminal_preview,
                "after-abort",
                2,
                PublicOperationClass::Authority,
                "session-1",
            ))
            .await,
        Err(crate::host::HostError::CommandRejected(message))
            if message.contains("durably terminal")
    ));
}

#[tokio::test]
async fn fresh_session_recovers_unowned_work_as_ambiguous_containment() {
    let preview = PublicOperationPreviewIdentity::new(5);
    let (_directory, path, store) = preview_store(&preview);
    let pending = intent(
        &preview,
        "interrupted",
        1,
        PublicOperationClass::Authority,
        "session-1",
    );
    store.begin_public_operation(&pending, &[]).await.unwrap();
    store
        .advance_public_operation(
            &pending.operation_id,
            pending.revision,
            &pending.process_session_id,
            PublicOperationStage::Ready,
            PublicOperationStage::Running,
            None,
        )
        .await
        .unwrap();
    drop(store);

    let reopened = Arc::new(
        SqliteStore::open_preview_existing(&path, Some(&storage_identity()), &preview).unwrap(),
    );
    let registry =
        PartitionOperationRegistry::new(reopened, preview, ProcessSessionId::new("session-2"))
            .unwrap();
    let runtime = PartitionOperationRuntime::start(registry.clone());
    let recovered = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(operation) = registry.operation(&pending.operation_id).await {
                break operation.snapshot();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
    assert_eq!(recovered.stage, PublicOperationStage::ContainmentPending);
    assert!(matches!(
        &recovered.disposition,
        Some(PublicOperationDisposition::Ambiguous(message))
            if message.contains("predecessor process session")
    ));
}

#[tokio::test]
async fn preview_runtime_shutdown_owns_and_drains_root_tasks() {
    let preview = PublicOperationPreviewIdentity::new(31);
    let (_directory, _path, store) = preview_store(&preview);
    let registry =
        PartitionOperationRegistry::new(store, preview.clone(), ProcessSessionId::new("session-1"))
            .unwrap();
    let runtime = PartitionOperationRuntime::start(registry.clone());
    assert!(Arc::ptr_eq(&runtime.registry(), &registry));
    let operation = registry
        .admit(intent(
            &preview,
            "shutdown-owned",
            1,
            PublicOperationClass::Authority,
            "session-1",
        ))
        .await
        .unwrap();
    operation
        .spawn_root(CallbackContainment::RootTask, async {
            std::future::pending::<()>().await;
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
    assert_eq!(operation.snapshot().stage, PublicOperationStage::Completed);
    assert_eq!(
        operation.snapshot().disposition,
        Some(PublicOperationDisposition::Cancelled)
    );
}

#[derive(Default)]
struct Trace {
    events: Mutex<Vec<String>>,
    gates: Mutex<BTreeMap<&'static str, Arc<Notify>>>,
}

struct TraceCallback {
    trace: Arc<Trace>,
    name: &'static str,
    completed: bool,
}

impl Drop for TraceCallback {
    fn drop(&mut self) {
        if !self.completed {
            self.trace.record(format!("{}.cancel", self.name));
        }
    }
}

impl Trace {
    fn record(&self, event: impl Into<String>) {
        self.events.lock().unwrap().push(event.into());
    }

    fn block(&self, name: &'static str) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        self.gates.lock().unwrap().insert(name, gate.clone());
        gate
    }

    async fn callback(self: &Arc<Self>, name: &'static str) {
        self.record(format!("{name}.begin"));
        let mut callback = TraceCallback {
            trace: self.clone(),
            name,
            completed: false,
        };
        let gate = self.gates.lock().unwrap().get(name).cloned();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.record(format!("{name}.end"));
        callback.completed = true;
    }

    async fn wait_for(&self, expected: &str) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if self
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| event == expected)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}

struct TraceProvider {
    trace: Arc<Trace>,
}

#[async_trait]
impl StateProvider for TraceProvider {
    async fn update_epoch(
        &self,
        _epoch: Epoch,
        _previous_epoch_last_lsn: i64,
    ) -> crate::Result<()> {
        self.trace.callback("provider.update_epoch").await;
        Ok(())
    }

    async fn last_committed_lsn(&self) -> crate::Result<i64> {
        self.trace.callback("provider.last_committed_lsn").await;
        Ok(0)
    }

    async fn get_copy_context(&self) -> crate::Result<OperationDataStream> {
        self.trace.callback("provider.get_copy_context").await;
        Ok(Box::pin(futures::stream::empty()))
    }

    async fn get_copy_state(
        &self,
        _up_to_lsn: i64,
        _copy_context: OperationDataStream,
    ) -> crate::Result<OperationDataStream> {
        self.trace.callback("provider.get_copy_state").await;
        Ok(Box::pin(futures::stream::empty()))
    }

    async fn on_data_loss(&self) -> crate::Result<bool> {
        self.trace.callback("provider.on_data_loss").await;
        Ok(false)
    }
}

struct TraceStateReplicator {
    trace: Arc<Trace>,
}

#[async_trait]
impl StateReplicator for TraceStateReplicator {
    async fn replicate(&self, _data: bytes::Bytes) -> crate::Result<i64> {
        self.trace.callback("state_replicator.replicate").await;
        Ok(0)
    }

    async fn get_replication_stream(
        &self,
    ) -> crate::Result<crate::replicator::stream::OperationStream> {
        self.trace
            .callback("state_replicator.get_replication_stream")
            .await;
        Ok(crate::replicator::stream::OperationStream::channel(1).1)
    }

    async fn get_copy_stream(&self) -> crate::Result<crate::replicator::stream::OperationStream> {
        self.trace
            .callback("state_replicator.get_copy_stream")
            .await;
        Ok(crate::replicator::stream::OperationStream::channel(1).1)
    }

    async fn update_replicator_settings(&self, _settings: ReplicatorSettings) -> crate::Result<()> {
        self.trace
            .callback("state_replicator.update_settings")
            .await;
        Ok(())
    }
}

struct TraceApplication {
    trace: Arc<Trace>,
    replicator: Arc<TraceReplicator>,
}

impl TraceApplication {
    async fn trace_open(&self) -> crate::Result<Arc<dyn Replicator>> {
        self.trace.callback("application.open").await;
        Ok(self.replicator.clone())
    }
}

#[async_trait]
impl StatefulServiceReplica for TraceApplication {
    async fn open(self: Arc<Self>, _context: OpenContext) -> crate::Result<Arc<dyn Replicator>> {
        self.trace_open().await
    }

    async fn change_role(&self, _role: ReplicaRole) -> crate::Result<RoleChange> {
        self.trace.callback("application.change_role").await;
        Ok(RoleChange {
            service_address: None,
        })
    }

    async fn close(&self) -> crate::Result<()> {
        self.trace.callback("application.close").await;
        Ok(())
    }

    fn abort(&self) {
        self.trace.record("application.abort");
    }
}

struct TraceReplicator {
    trace: Arc<Trace>,
    provider: Arc<TraceProvider>,
    primary: AtomicBool,
    descendant_stop: Mutex<Option<tokio::sync::watch::Sender<bool>>>,
    descendant_stopped: Arc<AtomicBool>,
}

impl TraceReplicator {
    fn require_primary(&self) -> crate::Result<()> {
        if self.primary.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(crate::RuntimeError::NotPrimary)
        }
    }

    fn start_descendant(&self) {
        let (stop, mut receiver) = tokio::sync::watch::channel(false);
        *self.descendant_stop.lock().unwrap() = Some(stop);
        let stopped = self.descendant_stopped.clone();
        let trace = self.trace.clone();
        tokio::spawn(async move {
            trace.record("provider.descendant.begin");
            let _ = receiver.wait_for(|stop| *stop).await;
            stopped.store(true, Ordering::Release);
            trace.record("provider.descendant.end");
        });
    }
}

#[async_trait]
impl Replicator for TraceReplicator {
    async fn open(&self) -> crate::Result<String> {
        self.trace.callback("replicator.open").await;
        Ok("trace://replicator".into())
    }

    async fn change_role(&self, _epoch: Epoch, role: ReplicaRole) -> crate::Result<()> {
        self.trace.callback("replicator.change_role").await;
        self.primary
            .store(role == ReplicaRole::Primary, Ordering::Release);
        Ok(())
    }

    async fn update_epoch(&self, epoch: Epoch) -> crate::Result<()> {
        self.trace.callback("replicator.update_epoch").await;
        self.provider.update_epoch(epoch, 0).await
    }

    async fn close(&self) -> crate::Result<()> {
        self.trace.callback("replicator.close").await;
        Ok(())
    }

    fn abort(&self) {
        self.trace.record("replicator.abort");
        if let Some(stop) = self.descendant_stop.lock().unwrap().take() {
            stop.send_replace(true);
        }
    }

    async fn current_progress(&self) -> crate::Result<i64> {
        self.trace.callback("replicator.current_progress").await;
        Ok(0)
    }

    async fn catch_up_capability(&self) -> crate::Result<i64> {
        self.trace.callback("replicator.catch_up_capability").await;
        Ok(0)
    }
}

#[async_trait]
impl PrimaryReplicator for TraceReplicator {
    async fn on_data_loss(&self) -> crate::Result<bool> {
        self.require_primary()?;
        self.trace.callback("primary.on_data_loss").await;
        self.provider.on_data_loss().await
    }

    async fn update_catch_up_replica_set_configuration(
        &self,
        _current: ReplicaSetConfiguration,
        _previous: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        self.require_primary()?;
        self.trace
            .callback("primary.update_catch_up_configuration")
            .await;
        Ok(())
    }

    async fn wait_for_catch_up_quorum(&self, _mode: ReplicaSetQuorumMode) -> crate::Result<()> {
        self.require_primary()?;
        self.trace.callback("primary.wait_for_catch_up").await;
        Ok(())
    }

    async fn update_current_replica_set_configuration(
        &self,
        _current: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        self.require_primary()?;
        self.trace
            .callback("primary.update_current_configuration")
            .await;
        Ok(())
    }

    async fn build_replica(&self, _replica: ReplicaInformation) -> crate::Result<()> {
        self.require_primary()?;
        self.start_descendant();
        self.trace.callback("primary.build_replica").await;
        Ok(())
    }

    async fn remove_replica(&self, _replica_id: ReplicaId) -> crate::Result<()> {
        self.require_primary()?;
        self.trace.callback("primary.remove_replica").await;
        Ok(())
    }
}

fn trace_fixture() -> (
    Arc<Trace>,
    Arc<TraceApplication>,
    Arc<TraceReplicator>,
    Arc<TraceStateReplicator>,
) {
    let trace = Arc::new(Trace::default());
    let provider = Arc::new(TraceProvider {
        trace: trace.clone(),
    });
    let replicator = Arc::new(TraceReplicator {
        trace: trace.clone(),
        provider,
        primary: AtomicBool::new(false),
        descendant_stop: Mutex::new(None),
        descendant_stopped: Arc::new(AtomicBool::new(false)),
    });
    let application = Arc::new(TraceApplication {
        trace: trace.clone(),
        replicator: replicator.clone(),
    });
    let state_replicator = Arc::new(TraceStateReplicator {
        trace: trace.clone(),
    });
    (trace, application, replicator, state_replicator)
}

#[tokio::test]
async fn strict_trace_fixture_covers_the_public_callback_inventory_and_primary_guard() {
    let (trace, application, replicator, state_replicator) = trace_fixture();
    application.trace_open().await.unwrap();
    application.change_role(ReplicaRole::Primary).await.unwrap();
    application.close().await.unwrap();
    application.abort();
    replicator.open().await.unwrap();

    let empty = ReplicaSetConfiguration::from(ConfigurationDescriptor::new(
        Epoch::default(),
        ReplicaId::new(1),
        Vec::new(),
        1,
    ));
    assert!(matches!(
        replicator
            .update_current_replica_set_configuration(empty.clone())
            .await,
        Err(crate::RuntimeError::NotPrimary)
    ));
    replicator
        .change_role(Epoch::default(), ReplicaRole::Primary)
        .await
        .unwrap();
    replicator.update_epoch(Epoch::default()).await.unwrap();
    replicator.current_progress().await.unwrap();
    replicator.catch_up_capability().await.unwrap();
    replicator.on_data_loss().await.unwrap();
    replicator
        .update_catch_up_replica_set_configuration(empty.clone(), empty.clone())
        .await
        .unwrap();
    replicator
        .wait_for_catch_up_quorum(ReplicaSetQuorumMode::All)
        .await
        .unwrap();
    replicator
        .update_current_replica_set_configuration(empty)
        .await
        .unwrap();
    replicator
        .build_replica(ReplicaInformation::new(
            OperationId::new("build"),
            replica_identity(),
            "trace://target".into(),
        ))
        .await
        .unwrap();
    replicator.remove_replica(ReplicaId::new(2)).await.unwrap();
    replicator.abort();
    trace.wait_for("provider.descendant.end").await;
    replicator.close().await.unwrap();
    state_replicator
        .replicate(bytes::Bytes::from_static(b"write"))
        .await
        .unwrap();
    state_replicator.get_replication_stream().await.unwrap();
    state_replicator.get_copy_stream().await.unwrap();
    state_replicator
        .update_replicator_settings(ReplicatorSettings::default())
        .await
        .unwrap();

    let events = trace.events.lock().unwrap().clone();
    for expected in [
        "application.open.begin",
        "application.open.end",
        "application.change_role.begin",
        "application.close.end",
        "application.abort",
        "replicator.open.begin",
        "replicator.change_role.end",
        "replicator.update_epoch.begin",
        "provider.update_epoch.end",
        "replicator.current_progress.end",
        "replicator.catch_up_capability.end",
        "primary.on_data_loss.begin",
        "provider.on_data_loss.end",
        "primary.update_catch_up_configuration.end",
        "primary.wait_for_catch_up.end",
        "primary.update_current_configuration.end",
        "primary.build_replica.begin",
        "provider.descendant.begin",
        "provider.descendant.end",
        "primary.remove_replica.end",
        "replicator.close.end",
        "replicator.abort",
        "state_replicator.replicate.end",
        "state_replicator.get_replication_stream.end",
        "state_replicator.get_copy_stream.end",
        "state_replicator.update_settings.end",
    ] {
        assert!(
            events.iter().any(|event| event == expected),
            "missing {expected}"
        );
    }
}

#[tokio::test]
async fn blocked_public_callback_is_owned_cancelled_and_contained_before_successor() {
    let preview = PublicOperationPreviewIdentity::new(50);
    let (_directory, _path, store) = preview_store(&preview);
    let registry =
        PartitionOperationRegistry::new(store, preview.clone(), ProcessSessionId::new("session-1"))
            .unwrap();
    let (trace, _application, replicator, _state_replicator) = trace_fixture();
    replicator
        .change_role(Epoch::default(), ReplicaRole::Primary)
        .await
        .unwrap();
    trace.block("primary.build_replica");
    let build = registry
        .admit(intent(
            &preview,
            "owned-build",
            1,
            PublicOperationClass::Build {
                target: ReplicaId::new(2),
            },
            "session-1",
        ))
        .await
        .unwrap();
    let callback = replicator.clone();
    build
        .spawn_root(CallbackContainment::ObjectOwned, async move {
            callback
                .build_replica(ReplicaInformation::new(
                    OperationId::new("build"),
                    replica_identity(),
                    "trace://target".into(),
                ))
                .await
        })
        .await
        .unwrap();
    trace.wait_for("primary.build_replica.begin").await;
    trace.wait_for("provider.descendant.begin").await;

    let remove = registry
        .admit(intent(
            &preview,
            "remove",
            1,
            PublicOperationClass::Remove {
                target: ReplicaId::new(2),
            },
            "session-1",
        ))
        .await
        .unwrap();
    wait_for_stage(&build, PublicOperationStage::ContainmentPending).await;
    trace.wait_for("primary.build_replica.cancel").await;
    assert_eq!(
        remove.snapshot().stage,
        PublicOperationStage::WaitingForContainment
    );
    assert!(!replicator.descendant_stopped.load(Ordering::Acquire));

    replicator.abort();
    trace.wait_for("provider.descendant.end").await;
    registry
        .complete_containment(&build.intent().operation_id)
        .await
        .unwrap();
    wait_for_stage(&remove, PublicOperationStage::Ready).await;
}
