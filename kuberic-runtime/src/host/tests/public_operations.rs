use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;

use crate::application::{OpenContext, RoleChange, StatefulServiceReplica};
use crate::host::operation::{CallbackContainment, PartitionOperationRegistry};
use crate::host::operation_recovery::PartitionOperationRecoveryOwner;
use crate::host::sqlite_store::SqliteStore;
use crate::host::state::{
    AgentState, PublicOperationDisposition, PublicOperationStage, SCHEMA_VERSION, StorageIdentity,
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
    Replicator,
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

#[tokio::test]
async fn preview_store_rejects_legacy_and_mixed_openers() {
    let preview = PublicOperationPreviewIdentity::new(1);
    let (_directory, path, store) = preview_store(&preview);
    drop(store);

    assert!(matches!(
        SqliteStore::open_existing(&path, None),
        Err(crate::host::HostError::InitializationNotAuthorized(message))
            if message.contains("legacy store opener")
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
        Err(crate::host::HostError::InitializationNotAuthorized(message))
            if message.contains("rejected legacy state")
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
    assert_eq!(
        first.snapshot().stage,
        PublicOperationStage::ContainmentPending
    );
    assert_eq!(
        second.snapshot().stage,
        PublicOperationStage::WaitingForContainment
    );

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
        .unwrap();
    assert_eq!(completed.stage, PublicOperationStage::Completed);
    assert_eq!(
        completed.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
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
    assert_eq!(authority.snapshot().stage, PublicOperationStage::Completed);
    assert_eq!(
        authority.snapshot().disposition,
        Some(PublicOperationDisposition::Cancelled)
    );
    assert_eq!(abort.snapshot().stage, PublicOperationStage::Ready);
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
    let owner = PartitionOperationRecoveryOwner::new(registry.clone());
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move { owner.run(receiver).await });
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
    shutdown.send_replace(true);
    task.await.unwrap().unwrap();
    assert_eq!(recovered.stage, PublicOperationStage::ContainmentPending);
    assert!(matches!(
        &recovered.disposition,
        Some(PublicOperationDisposition::Ambiguous(message))
            if message.contains("predecessor process session")
    ));
}

#[derive(Default)]
struct Trace {
    events: Mutex<Vec<&'static str>>,
}

impl Trace {
    fn record(&self, event: &'static str) {
        self.events.lock().unwrap().push(event);
    }
}

struct TraceApplication {
    trace: Arc<Trace>,
    replicator: Arc<TraceReplicator>,
}

#[async_trait]
impl StatefulServiceReplica for TraceApplication {
    async fn open(self: Arc<Self>, _context: OpenContext) -> crate::Result<Arc<dyn Replicator>> {
        self.trace.record("application.open");
        Ok(self.replicator.clone())
    }

    async fn change_role(&self, _role: ReplicaRole) -> crate::Result<RoleChange> {
        self.trace.record("application.change_role");
        Ok(RoleChange {
            service_address: None,
        })
    }

    async fn close(&self) -> crate::Result<()> {
        self.trace.record("application.close");
        Ok(())
    }

    fn abort(&self) {
        self.trace.record("application.abort");
    }
}

struct TraceReplicator {
    trace: Arc<Trace>,
}

#[async_trait]
impl Replicator for TraceReplicator {
    async fn open(&self) -> crate::Result<String> {
        self.trace.record("replicator.open");
        Ok("trace://replicator".into())
    }

    async fn change_role(&self, _epoch: Epoch, _role: ReplicaRole) -> crate::Result<()> {
        self.trace.record("replicator.change_role");
        Ok(())
    }

    async fn update_epoch(&self, _epoch: Epoch) -> crate::Result<()> {
        self.trace.record("replicator.update_epoch");
        Ok(())
    }

    async fn close(&self) -> crate::Result<()> {
        self.trace.record("replicator.close");
        Ok(())
    }

    fn abort(&self) {
        self.trace.record("replicator.abort");
    }

    async fn current_progress(&self) -> crate::Result<i64> {
        self.trace.record("replicator.current_progress");
        Ok(0)
    }

    async fn catch_up_capability(&self) -> crate::Result<i64> {
        self.trace.record("replicator.catch_up_capability");
        Ok(0)
    }
}

#[async_trait]
impl PrimaryReplicator for TraceReplicator {
    async fn on_data_loss(&self) -> crate::Result<bool> {
        self.trace.record("primary.on_data_loss");
        Ok(false)
    }

    async fn update_catch_up_replica_set_configuration(
        &self,
        _current: ReplicaSetConfiguration,
        _previous: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        self.trace.record("primary.update_catch_up_configuration");
        Ok(())
    }

    async fn wait_for_catch_up_quorum(&self, _mode: ReplicaSetQuorumMode) -> crate::Result<()> {
        self.trace.record("primary.wait_for_catch_up");
        Ok(())
    }

    async fn update_current_replica_set_configuration(
        &self,
        _current: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        self.trace.record("primary.update_current_configuration");
        Ok(())
    }

    async fn build_replica(&self, _replica: ReplicaInformation) -> crate::Result<()> {
        self.trace.record("primary.build_replica");
        Ok(())
    }

    async fn remove_replica(&self, _replica_id: ReplicaId) -> crate::Result<()> {
        self.trace.record("primary.remove_replica");
        Ok(())
    }
}

#[tokio::test]
async fn strict_trace_fixture_covers_the_host_root_callback_inventory() {
    let trace = Arc::new(Trace::default());
    let replicator = Arc::new(TraceReplicator {
        trace: trace.clone(),
    });
    let application = TraceApplication {
        trace: trace.clone(),
        replicator: replicator.clone(),
    };
    application.change_role(ReplicaRole::Primary).await.unwrap();
    application.close().await.unwrap();
    application.abort();
    replicator.open().await.unwrap();
    replicator
        .change_role(Epoch::default(), ReplicaRole::Primary)
        .await
        .unwrap();
    replicator.update_epoch(Epoch::default()).await.unwrap();
    replicator.current_progress().await.unwrap();
    replicator.catch_up_capability().await.unwrap();
    replicator.on_data_loss().await.unwrap();
    let configuration = ReplicaSetConfiguration::from(ConfigurationDescriptor::new(
        Epoch::default(),
        ReplicaId::new(1),
        Vec::new(),
        1,
    ));
    replicator
        .update_catch_up_replica_set_configuration(configuration.clone(), configuration.clone())
        .await
        .unwrap();
    replicator
        .wait_for_catch_up_quorum(ReplicaSetQuorumMode::All)
        .await
        .unwrap();
    replicator
        .update_current_replica_set_configuration(configuration)
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
    replicator.close().await.unwrap();
    replicator.abort();

    let events = trace.events.lock().unwrap().clone();
    for expected in [
        "application.change_role",
        "application.close",
        "application.abort",
        "replicator.open",
        "replicator.change_role",
        "replicator.update_epoch",
        "replicator.current_progress",
        "replicator.catch_up_capability",
        "primary.on_data_loss",
        "primary.update_catch_up_configuration",
        "primary.wait_for_catch_up",
        "primary.update_current_configuration",
        "primary.build_replica",
        "primary.remove_replica",
        "replicator.close",
        "replicator.abort",
    ] {
        assert!(events.contains(&expected), "missing trace event {expected}");
    }
}
