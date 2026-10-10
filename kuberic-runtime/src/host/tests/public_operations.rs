use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Notify, watch};

use crate::application::{
    OpenContext, OperationDataStream, RoleChange, StateProvider, StatefulServiceReplica,
};
use crate::authority::{AdmittedAuthority, ReplicaAuthorityStore};
use crate::host::hosting::PodRuntime;
use crate::host::operation::{CallbackContainment, PartitionOperationRegistry};
use crate::host::operation_recovery::PartitionOperationRuntime;
use crate::host::service::AgentService;
use crate::host::sqlite_store::SqliteStore;
use crate::host::state::{
    AgentState, PublicOperationContainment, PublicOperationDisposition, PublicOperationStage,
    SCHEMA_VERSION, StorageIdentity,
};
use crate::host::store::AgentStore;
use crate::host::testing::PublicOperationPreviewRuntime;
use crate::protocol::public_operations::{
    PreviewLifecycleBinding, PublicOperationClass, PublicOperationIntent,
    PublicOperationPreviewIdentity, StatePersistence,
};
use crate::protocol::types::{
    AccessStatus, AgentGeneration, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy,
    Epoch, InitializationId, OperationId, PodUid, ProcessSessionId, PvcUid, ReplicaId,
    ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ResourceUid,
};
use crate::replicator::{
    PrimaryReplicator, ReplicaInformation, ReplicaSetConfiguration, ReplicaSetQuorumMode,
    Replicator, ReplicatorFactory, ReplicatorFactoryContext, ReplicatorInterfaces,
    ReplicatorSettings, StateReplicator,
};

use super::tempdir;

#[path = "public_operations/lifecycle.rs"]
mod lifecycle;
#[path = "public_operations/reconfiguration.rs"]
mod reconfiguration;

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
        lifecycle: None,
        program: None,
    }
}

fn preview_store(
    preview: &PublicOperationPreviewIdentity,
) -> (tempfile::TempDir, std::path::PathBuf, Arc<SqliteStore>) {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let store = SqliteStore::create_preview_bound_authorized(
        &path,
        AgentState::new(storage_identity()),
        PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: ResourceUid::new("resource-1"),
            spec_generation: 7,
            state_persistence: StatePersistence::Persisted,
        },
    )
    .unwrap();
    store.relax_durability_for_tests();
    (directory, path, Arc::new(store))
}

fn durable_preview_store(
    preview: &PublicOperationPreviewIdentity,
) -> (tempfile::TempDir, std::path::PathBuf, Arc<SqliteStore>) {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let store = SqliteStore::create_preview_bound_authorized(
        &path,
        AgentState::new(storage_identity()),
        PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: ResourceUid::new("resource-1"),
            spec_generation: 7,
            state_persistence: StatePersistence::Persisted,
        },
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
    let (_directory, path, store) = durable_preview_store(&preview);
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
        .spawn_root(CallbackContainment::ObjectOwnedOnInterruption, async move {
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
async fn transitive_containment_blockers_survive_multiple_supersessions() {
    let preview = PublicOperationPreviewIdentity::new(47);
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
    first
        .spawn_root(CallbackContainment::ObjectOwnedOnInterruption, async {
            std::future::pending::<()>().await;
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
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
    wait_for_stage(&first, PublicOperationStage::ContainmentPending).await;
    wait_for_stage(&second, PublicOperationStage::WaitingForContainment).await;

    let third = registry
        .admit(intent(
            &preview,
            "authority-3",
            3,
            PublicOperationClass::Authority,
            "session-1",
        ))
        .await
        .unwrap();
    wait_for_stage(&second, PublicOperationStage::Completed).await;
    let blocked = wait_for_stage(&third, PublicOperationStage::WaitingForContainment).await;
    assert!(blocked.blockers.contains(&first.intent().operation_id));
    assert!(blocked.blockers.contains(&second.intent().operation_id));

    registry
        .complete_containment(&first.intent().operation_id)
        .await
        .unwrap();
    wait_for_stage(&third, PublicOperationStage::Ready).await;
    registry.shutdown().await.unwrap();
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
async fn durable_supersession_fence_rejects_predecessor_callback_completion() {
    let preview = PublicOperationPreviewIdentity::new(34);
    let (_directory, _path, store) = preview_store(&preview);
    let first = intent(
        &preview,
        "first",
        1,
        PublicOperationClass::Authority,
        "session-1",
    );
    store
        .begin_public_operation(&first, &[], &[])
        .await
        .unwrap();
    store
        .advance_public_operation(
            &first.operation_id,
            first.revision,
            &first.process_session_id,
            PublicOperationStage::Ready,
            PublicOperationStage::Running,
            None,
        )
        .await
        .unwrap();
    let second = intent(
        &preview,
        "second",
        2,
        PublicOperationClass::Authority,
        "session-1",
    );
    store
        .begin_public_operation(
            &second,
            std::slice::from_ref(&first.operation_id),
            std::slice::from_ref(&first.operation_id),
        )
        .await
        .unwrap();
    assert!(matches!(
        store
            .advance_public_operation(
                &first.operation_id,
                first.revision,
                &first.process_session_id,
                PublicOperationStage::Running,
                PublicOperationStage::CallbackApplied,
                Some(PublicOperationDisposition::Succeeded),
            )
            .await,
        Err(crate::host::HostError::StaleEffectCompletion(message))
            if message.contains("superseded")
    ));
}

#[tokio::test]
async fn recovery_retains_containment_for_callback_applied_before_supersession() {
    let preview = PublicOperationPreviewIdentity::new(36);
    let (_directory, _path, store) = preview_store(&preview);
    let first = intent(
        &preview,
        "first",
        1,
        PublicOperationClass::Authority,
        "session-1",
    );
    store
        .begin_public_operation(&first, &[], &[])
        .await
        .unwrap();
    store
        .advance_public_operation(
            &first.operation_id,
            first.revision,
            &first.process_session_id,
            PublicOperationStage::Ready,
            PublicOperationStage::Running,
            None,
        )
        .await
        .unwrap();
    store
        .advance_public_operation(
            &first.operation_id,
            first.revision,
            &first.process_session_id,
            PublicOperationStage::Running,
            PublicOperationStage::CallbackApplied,
            Some(PublicOperationDisposition::Succeeded),
        )
        .await
        .unwrap();
    let second = intent(
        &preview,
        "second",
        2,
        PublicOperationClass::Authority,
        "session-1",
    );
    store
        .begin_public_operation(
            &second,
            std::slice::from_ref(&first.operation_id),
            std::slice::from_ref(&first.operation_id),
        )
        .await
        .unwrap();

    let registry =
        PartitionOperationRegistry::new(store, preview, ProcessSessionId::new("session-1"))
            .unwrap();
    registry.recover_unowned().await.unwrap();
    let recovered = registry
        .operation(&first.operation_id)
        .await
        .unwrap()
        .snapshot();
    assert_eq!(recovered.stage, PublicOperationStage::ContainmentPending);
    assert!(matches!(
        recovered.disposition,
        Some(PublicOperationDisposition::Ambiguous(message))
            if message.contains("before supersession")
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
        .spawn_root(CallbackContainment::ObjectOwnedOnInterruption, async {
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
    for target in 2..5 {
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
async fn repeatable_redelivery_reaps_a_finished_root_after_completion_persistence_failure() {
    let preview = PublicOperationPreviewIdentity::new(31);
    let (_directory, _path, store) = preview_store(&preview);
    let registry = PartitionOperationRegistry::new(
        store.clone(),
        preview.clone(),
        ProcessSessionId::new("session-1"),
    )
    .unwrap();
    let program =
        crate::protocol::public_operations::PublicOperationProgram::Progress { capability: false };
    let intent = PublicOperationIntent {
        preview,
        operation_id: OperationId::new("repeatable-persistence-cut"),
        revision: 1,
        process_session_id: ProcessSessionId::new("session-1"),
        class: PublicOperationClass::Authority,
        input_digest: program.digest(),
        lifecycle: None,
        program: Some(program),
    };
    let operation = registry.admit(intent.clone()).await.unwrap();
    let release = Arc::new(Notify::new());
    let applied = Arc::new(Notify::new());
    let callback_release = release.clone();
    let callback_applied = applied.clone();
    let callback_store = store.clone();
    let callback_intent = intent.clone();
    operation
        .spawn_root(CallbackContainment::RootTask, async move {
            callback_store
                .public_instruction(
                    &callback_intent,
                    0,
                    crate::host::state::PublicInstruction::Progress,
                    None,
                )
                .await
                .unwrap();
            callback_store
                .public_instruction(
                    &callback_intent,
                    0,
                    crate::host::state::PublicInstruction::Progress,
                    Some(crate::host::state::PublicInstructionOutcome::Progress(0)),
                )
                .await
                .unwrap();
            callback_applied.notify_one();
            callback_release.notified().await;
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
    applied.notified().await;
    store.fail_next_public_operation_advance();
    release.notify_one();
    assert!(
        operation
            .wait_for_terminal()
            .await
            .unwrap_err()
            .to_string()
            .contains("injected public-operation")
    );

    let duplicate = registry.admit(intent).await.unwrap();
    assert_eq!(duplicate.snapshot().stage, PublicOperationStage::Ready);
    duplicate
        .spawn_root(CallbackContainment::RootTask, async {
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
    assert_eq!(
        duplicate.wait_for_terminal().await.unwrap().stage,
        PublicOperationStage::Completed
    );
}

#[tokio::test]
async fn cancellation_persistence_failure_is_recovered_without_releasing_successor() {
    let preview = PublicOperationPreviewIdentity::new(32);
    let (_directory, _path, store) = preview_store(&preview);
    let registry = PartitionOperationRegistry::new(
        store.clone(),
        preview.clone(),
        ProcessSessionId::new("session-1"),
    )
    .unwrap();
    let runtime = PartitionOperationRuntime::start(registry.clone());
    let first = registry
        .admit(intent(
            &preview,
            "first",
            1,
            PublicOperationClass::Authority,
            "session-1",
        ))
        .await
        .unwrap();
    first
        .spawn_root(CallbackContainment::RootTask, async {
            std::future::pending::<()>().await;
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
    store.fail_next_public_operation_advance();
    let second = registry
        .admit(intent(
            &preview,
            "second",
            2,
            PublicOperationClass::Authority,
            "session-1",
        ))
        .await
        .unwrap();

    let recovered = wait_for_stage(&first, PublicOperationStage::ContainmentPending).await;
    assert!(matches!(
        recovered.disposition,
        Some(PublicOperationDisposition::Ambiguous(_))
    ));
    let blocked = wait_for_stage(&second, PublicOperationStage::ContainmentPending).await;
    assert!(matches!(
        blocked.disposition,
        Some(PublicOperationDisposition::Ambiguous(_))
    ));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_serializes_with_admission_and_leaves_no_unowned_runnable_work() {
    let preview = PublicOperationPreviewIdentity::new(33);
    let (_directory, _path, store) = preview_store(&preview);
    let registry = PartitionOperationRegistry::new(
        store.clone(),
        preview.clone(),
        ProcessSessionId::new("session-1"),
    )
    .unwrap();
    let runtime = PartitionOperationRuntime::start(registry.clone());
    let mut admissions = Vec::new();
    for target in 2..12 {
        let registry = registry.clone();
        let preview = preview.clone();
        admissions.push(tokio::spawn(async move {
            let operation = registry
                .admit(intent(
                    &preview,
                    &format!("shutdown-build-{target}"),
                    1,
                    PublicOperationClass::Build {
                        target: ReplicaId::new(target),
                    },
                    "session-1",
                ))
                .await?;
            operation
                .spawn_root(CallbackContainment::RootTask, async {
                    std::future::pending::<()>().await;
                    Ok::<(), Infallible>(())
                })
                .await
        }));
    }

    runtime.shutdown().await.unwrap();
    for admission in admissions {
        let _ = admission.await.unwrap();
    }
    assert!(matches!(
        registry
            .admit(intent(
                &preview,
                "after-shutdown",
                2,
                PublicOperationClass::Authority,
                "session-1",
            ))
            .await,
        Err(crate::host::HostError::CommandRejected(message))
            if message.contains("shutting down")
    ));
    for record in store.public_operation_records().await.unwrap() {
        assert!(
            matches!(
                record.stage,
                PublicOperationStage::Completed | PublicOperationStage::ContainmentPending
            ),
            "runnable record remained after shutdown: {record:?}"
        );
    }
}

#[tokio::test]
async fn shutdown_drains_other_roots_after_one_cancellation_persistence_failure() {
    let preview = PublicOperationPreviewIdentity::new(35);
    let (_directory, _path, store) = preview_store(&preview);
    let registry = PartitionOperationRegistry::new(
        store.clone(),
        preview.clone(),
        ProcessSessionId::new("session-1"),
    )
    .unwrap();
    let runtime = PartitionOperationRuntime::start(registry.clone());
    for target in [2, 3] {
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
        operation
            .spawn_root(CallbackContainment::RootTask, async {
                std::future::pending::<()>().await;
                Ok::<(), Infallible>(())
            })
            .await
            .unwrap();
    }
    store.fail_next_public_operation_advance();
    assert!(runtime.shutdown().await.is_err());
    for record in store.public_operation_records().await.unwrap() {
        assert!(
            matches!(
                record.stage,
                PublicOperationStage::Completed | PublicOperationStage::ContainmentPending
            ),
            "shutdown abandoned runnable operation: {record:?}"
        );
    }
}

#[tokio::test]
async fn terminal_supersession_drains_all_roots_after_one_persistence_failure() {
    let preview = PublicOperationPreviewIdentity::new(38);
    let (_directory, _path, store) = preview_store(&preview);
    let registry = PartitionOperationRegistry::new(
        store.clone(),
        preview.clone(),
        ProcessSessionId::new("session-1"),
    )
    .unwrap();
    let runtime = PartitionOperationRuntime::start(registry.clone());
    let mut builds = Vec::new();
    for target in [2, 3] {
        let build = registry
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
        build
            .spawn_root(CallbackContainment::RootTask, async {
                std::future::pending::<()>().await;
                Ok::<(), Infallible>(())
            })
            .await
            .unwrap();
        builds.push(build);
    }

    store.fail_next_public_operation_advance();
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

    wait_for_stage(&builds[0], PublicOperationStage::ContainmentPending).await;
    let drained = wait_for_stage(&builds[1], PublicOperationStage::Completed).await;
    assert_eq!(
        drained.disposition,
        Some(PublicOperationDisposition::Cancelled)
    );
    let blocked = wait_for_stage(&abort, PublicOperationStage::ContainmentPending).await;
    assert!(matches!(
        blocked.disposition,
        Some(PublicOperationDisposition::Ambiguous(_))
    ));
    runtime.shutdown().await.unwrap();
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
    let authority_5 = registry
        .admit(intent(
            &preview,
            "authority-5",
            5,
            PublicOperationClass::Authority,
            "session-1",
        ))
        .await
        .unwrap();
    authority_5
        .spawn_root(CallbackContainment::RootTask, async {
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
    authority_5.wait_for_terminal().await.unwrap();
    assert_eq!(
        registry
            .admit(intent(
                &preview,
                "build-current",
                5,
                PublicOperationClass::Build {
                    target: ReplicaId::new(2),
                },
                "session-1",
            ))
            .await
            .unwrap()
            .snapshot()
            .stage,
        PublicOperationStage::Ready
    );
    assert!(matches!(
        registry
            .admit(intent(
                &preview,
                "build-historical",
                4,
                PublicOperationClass::Build {
                    target: ReplicaId::new(3),
                },
                "session-1",
            ))
            .await,
        Err(crate::host::HostError::CommandRejected(message))
            if message.contains("retained authority")
                || message.contains("does not match current authority")
                || message.contains("build revisions do not match")
    ));

    let terminal_preview = PublicOperationPreviewIdentity::new(41);
    let (_directory, _path, store) = preview_store(&terminal_preview);
    let terminal_registry = PartitionOperationRegistry::new(
        store.clone(),
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

    let attachment = intent(
        &terminal_preview,
        "close-after-abort",
        3,
        PublicOperationClass::Close,
        "session-1",
    );
    store.fail_next_public_operation_attachment();
    assert!(matches!(
        terminal_registry.admit(attachment.clone()).await,
        Err(crate::host::HostError::CommandRejected(message))
            if message.contains("injected public-operation attachment failure")
    ));
    assert!(
        terminal_registry
            .operation(&attachment.operation_id)
            .await
            .is_none()
    );
    assert!(
        store
            .public_operation_records()
            .await
            .unwrap()
            .iter()
            .all(|record| record.intent.operation_id != attachment.operation_id)
    );

    let attached = terminal_registry.admit(attachment).await.unwrap();
    assert!(!Arc::ptr_eq(&attached, &abort));
    assert_eq!(attached.snapshot().stage, PublicOperationStage::Completed);
    assert_eq!(
        attached.snapshot().disposition,
        Some(PublicOperationDisposition::Attached(
            abort.intent().operation_id
        ))
    );
}

#[tokio::test]
async fn terminal_attachments_follow_the_authoritative_escalation_owner() {
    let preview = PublicOperationPreviewIdentity::new(42);
    let (_directory, _path, store) = preview_store(&preview);
    let registry =
        PartitionOperationRegistry::new(store, preview.clone(), ProcessSessionId::new("session-1"))
            .unwrap();
    let runtime = PartitionOperationRuntime::start(registry.clone());
    let close = registry
        .admit(intent(
            &preview,
            "a-close",
            1,
            PublicOperationClass::Close,
            "session-1",
        ))
        .await
        .unwrap();
    close
        .spawn_root(CallbackContainment::RootTask, async {
            std::future::pending::<()>().await;
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
    let abort = registry
        .admit(intent(
            &preview,
            "b-abort",
            2,
            PublicOperationClass::Abort,
            "session-1",
        ))
        .await
        .unwrap();
    wait_for_stage(&close, PublicOperationStage::Completed).await;
    wait_for_stage(&abort, PublicOperationStage::Ready).await;
    abort
        .spawn_root(CallbackContainment::RootTask, async {
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
    abort.wait_for_terminal().await.unwrap();

    let attached = registry
        .admit(intent(
            &preview,
            "c-later-close",
            3,
            PublicOperationClass::Close,
            "session-1",
        ))
        .await
        .unwrap();
    assert_eq!(
        attached.snapshot().disposition,
        Some(PublicOperationDisposition::Attached(
            abort.intent().operation_id
        ))
    );
    runtime.shutdown().await.unwrap();

    let preview = PublicOperationPreviewIdentity::new(43);
    let (_directory, _path, store) = preview_store(&preview);
    let registry =
        PartitionOperationRegistry::new(store, preview.clone(), ProcessSessionId::new("session-1"))
            .unwrap();
    let runtime = PartitionOperationRuntime::start(registry.clone());
    let transient = registry
        .admit(intent(
            &preview,
            "a-transient",
            1,
            PublicOperationClass::TransientFault,
            "session-1",
        ))
        .await
        .unwrap();
    transient
        .spawn_root(CallbackContainment::RootTask, async {
            std::future::pending::<()>().await;
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
    let permanent = registry
        .admit(intent(
            &preview,
            "b-permanent",
            2,
            PublicOperationClass::PermanentFault,
            "session-1",
        ))
        .await
        .unwrap();
    wait_for_stage(&transient, PublicOperationStage::Completed).await;
    wait_for_stage(&permanent, PublicOperationStage::Ready).await;
    permanent
        .spawn_root(CallbackContainment::RootTask, async {
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
    permanent.wait_for_terminal().await.unwrap();

    let attached = registry
        .admit(intent(
            &preview,
            "c-later-abort",
            3,
            PublicOperationClass::Abort,
            "session-1",
        ))
        .await
        .unwrap();
    assert_eq!(
        attached.snapshot().disposition,
        Some(PublicOperationDisposition::Attached(
            permanent.intent().operation_id
        ))
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn active_terminal_attachment_waits_for_the_owner_result() {
    let preview = PublicOperationPreviewIdentity::new(44);
    let (_directory, _path, store) = preview_store(&preview);
    let registry =
        PartitionOperationRegistry::new(store, preview.clone(), ProcessSessionId::new("session-1"))
            .unwrap();
    let runtime = PartitionOperationRuntime::start(registry.clone());
    let release = Arc::new(Notify::new());
    let callback_release = release.clone();
    let abort = registry
        .admit(intent(
            &preview,
            "abort",
            1,
            PublicOperationClass::Abort,
            "session-1",
        ))
        .await
        .unwrap();
    abort
        .spawn_root(CallbackContainment::RootTask, async move {
            callback_release.notified().await;
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
    let close = registry
        .admit(intent(
            &preview,
            "close",
            2,
            PublicOperationClass::Close,
            "session-1",
        ))
        .await
        .unwrap();
    assert_eq!(
        close.snapshot().stage,
        PublicOperationStage::WaitingForContainment
    );
    assert_eq!(
        close.snapshot().disposition,
        Some(PublicOperationDisposition::Attached(
            abort.intent().operation_id
        ))
    );

    release.notify_one();
    abort.wait_for_terminal().await.unwrap();
    let attached = wait_for_stage(&close, PublicOperationStage::Completed).await;
    assert_eq!(
        attached.disposition,
        Some(PublicOperationDisposition::Attached(
            abort.intent().operation_id
        ))
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancellation_failure_cannot_redirect_attachment_to_a_superseded_owner() {
    let preview = PublicOperationPreviewIdentity::new(46);
    let (_directory, _path, store) = preview_store(&preview);
    let registry = PartitionOperationRegistry::new(
        store.clone(),
        preview.clone(),
        ProcessSessionId::new("session-1"),
    )
    .unwrap();
    let close = registry
        .admit(intent(
            &preview,
            "a-close",
            1,
            PublicOperationClass::Close,
            "session-1",
        ))
        .await
        .unwrap();
    close
        .spawn_root(CallbackContainment::RootTask, async {
            std::future::pending::<()>().await;
            Ok::<(), Infallible>(())
        })
        .await
        .unwrap();
    store.fail_next_public_operation_advance();
    let abort = registry
        .admit(intent(
            &preview,
            "b-abort",
            2,
            PublicOperationClass::Abort,
            "session-1",
        ))
        .await
        .unwrap();
    let blocked = wait_for_stage(&abort, PublicOperationStage::ContainmentPending).await;
    assert!(matches!(
        blocked.disposition,
        Some(PublicOperationDisposition::Ambiguous(_))
    ));
    assert_eq!(
        close.snapshot().superseded_by,
        Some(abort.intent().operation_id)
    );

    let attached = registry
        .admit(intent(
            &preview,
            "c-permanent",
            3,
            PublicOperationClass::PermanentFault,
            "session-1",
        ))
        .await
        .unwrap();
    assert_eq!(
        attached.snapshot().disposition,
        Some(PublicOperationDisposition::Attached(
            abort.intent().operation_id
        ))
    );
    registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn fresh_session_recovers_unowned_work_as_ambiguous_containment() {
    let preview = PublicOperationPreviewIdentity::new(5);
    let (_directory, path, store) = durable_preview_store(&preview);
    let pending = intent(
        &preview,
        "interrupted",
        1,
        PublicOperationClass::Authority,
        "session-1",
    );
    store
        .begin_public_operation(&pending, &[], &[])
        .await
        .unwrap();
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
async fn fresh_preview_session_reconstructs_unassigned_and_access_closed() {
    let preview = PublicOperationPreviewIdentity::new(45);
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let mut state = AgentState::new(storage_identity());
    let current_configuration = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        ReplicaId::new(1),
        vec![ConfigurationMember {
            identity: replica_identity(),
            role: ReplicaRole::Primary,
        }],
        1,
    );
    state.current_configuration = Some(current_configuration.clone());
    state.role = ReplicaRole::Primary;
    state.read_status = AccessStatus::Granted;
    state.write_status = AccessStatus::Granted;
    let seeded = SqliteStore::create_preview_authorized(&path, state, preview.clone()).unwrap();
    seeded
        .admit(&AdmittedAuthority {
            local_identity: replica_identity(),
            transition_kind: None,
            previous_configuration: None,
            current_configuration,
            switchover_handoff: None,
            scale_up: None,
            secondary_removal: None,
        })
        .await
        .unwrap();
    drop(seeded);

    let store = Arc::new(
        SqliteStore::open_preview_existing(&path, Some(&storage_identity()), &preview).unwrap(),
    );
    let (_trace, application, _replicator, _state_replicator) = trace_fixture();
    let runtime = Arc::new(PodRuntime::new(
        replica_identity(),
        application,
        store.clone(),
    ));
    let service =
        AgentService::new(store.clone(), runtime.clone(), runtime.clone(), "token").unwrap();
    service.reconstruct_runtime().await.unwrap();

    let snapshot = runtime.snapshot().await;
    assert_eq!(snapshot.role, ReplicaRole::None);
    assert_eq!(snapshot.read_status, AccessStatus::NotPrimary);
    assert_eq!(snapshot.write_status, AccessStatus::NotPrimary);
    assert!(snapshot.authority.is_none());
    let durable = store.load_state().await.unwrap();
    assert_eq!(durable.role, ReplicaRole::Primary);
    assert_eq!(durable.read_status, AccessStatus::Granted);
    assert_eq!(durable.write_status, AccessStatus::Granted);
    assert!(store.load_admitted_authority().await.unwrap().is_some());
    runtime.abort();
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

#[tokio::test]
async fn agent_service_retains_and_drains_the_preview_runtime() {
    let preview = PublicOperationPreviewIdentity::new(37);
    let (_directory, _path, store) = preview_store(&preview);
    let (trace, application, _replicator, _state_replicator) = trace_fixture();
    let runtime = Arc::new(PodRuntime::new(
        replica_identity(),
        application,
        store.clone(),
    ));
    let service = AgentService::new(store, runtime.clone(), runtime, "token").unwrap();
    let session = service.sessions().local_session().clone();
    let registry = service
        .public_operation_preview_runtime(preview.clone())
        .await
        .unwrap();
    let operation = registry
        .admit(intent(
            &preview,
            "service-owned",
            1,
            PublicOperationClass::Authority,
            session.as_str(),
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

    let control = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let replication = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let (ready, mut ready_rx) = watch::channel(false);
    let (shutdown, shutdown_rx) = watch::channel(false);
    let mut server =
        tokio::spawn(service.serve_with_listeners(control, replication, ready, shutdown_rx));
    tokio::select! {
        ready = ready_rx.wait_for(|ready| *ready) => {
            if let Err(error) = ready {
                let result = server.await;
                panic!("preview service stopped before readiness ({error}): {result:?}");
            }
        }
        result = &mut server => {
            panic!("preview service stopped before readiness: {result:?}");
        }
    }
    shutdown.send_replace(true);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    assert!(
        trace
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "application.open.end")
    );
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
    arguments: Mutex<Vec<String>>,
    service_address: Mutex<Option<String>>,
    data_loss: Mutex<Option<std::result::Result<bool, String>>>,
    fail_role: AtomicBool,
    fail_close: Mutex<Vec<&'static str>>,
    configurations: Mutex<Vec<crate::protocol::public_operations::PublicConfiguration>>,
    installed: Mutex<Option<crate::protocol::public_operations::PublicConfiguration>>,
    waits: Mutex<
        Vec<(
            crate::protocol::public_operations::PublicConfiguration,
            ReplicaSetQuorumMode,
        )>,
    >,
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
        self.trace
            .data_loss
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(Ok(false))
            .map_err(crate::RuntimeError::Application)
    }
}

struct TraceStateReplicator {
    trace: Arc<Trace>,
    lifecycle_closed: Arc<AtomicBool>,
}

impl TraceStateReplicator {
    fn require_open(&self) -> crate::Result<()> {
        if self.lifecycle_closed.load(Ordering::Acquire) {
            Err(crate::RuntimeError::Closed)
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl StateReplicator for TraceStateReplicator {
    async fn replicate(&self, _data: bytes::Bytes) -> crate::Result<i64> {
        self.require_open()?;
        self.trace.callback("state_replicator.replicate").await;
        Ok(0)
    }

    async fn get_replication_stream(
        &self,
    ) -> crate::Result<crate::replicator::stream::OperationStream> {
        self.require_open()?;
        self.trace
            .callback("state_replicator.get_replication_stream")
            .await;
        Ok(crate::replicator::stream::OperationStream::channel(1).1)
    }

    async fn get_copy_stream(&self) -> crate::Result<crate::replicator::stream::OperationStream> {
        self.require_open()?;
        self.trace
            .callback("state_replicator.get_copy_stream")
            .await;
        Ok(crate::replicator::stream::OperationStream::channel(1).1)
    }

    async fn update_replicator_settings(&self, _settings: ReplicatorSettings) -> crate::Result<()> {
        self.require_open()?;
        self.trace
            .callback("state_replicator.update_settings")
            .await;
        Ok(())
    }
}

struct TraceApplication {
    trace: Arc<Trace>,
    replicator: Arc<TraceReplicator>,
    state_replicator: Arc<TraceStateReplicator>,
    convergent_open: AtomicBool,
}

struct TraceReplicatorFactory {
    replicator: Arc<TraceReplicator>,
    state_replicator: Arc<TraceStateReplicator>,
}

#[async_trait]
impl ReplicatorFactory for TraceReplicatorFactory {
    async fn create_replicator(
        &self,
        _context: ReplicatorFactoryContext,
        _state_provider: Option<Arc<dyn StateProvider>>,
        _settings: ReplicatorSettings,
    ) -> crate::Result<ReplicatorInterfaces> {
        Ok(ReplicatorInterfaces::primary(
            self.replicator.clone(),
            Some(self.state_replicator.clone()),
        ))
    }
}

impl TraceApplication {
    async fn trace_open(&self) -> crate::Result<Arc<dyn Replicator>> {
        self.trace.callback("application.open").await;
        Ok(self.replicator.clone())
    }
}

#[async_trait]
impl StatefulServiceReplica for TraceApplication {
    async fn open(self: Arc<Self>, context: OpenContext) -> crate::Result<Arc<dyn Replicator>> {
        if self.convergent_open.load(Ordering::Acquire) {
            return self.trace_open().await;
        }
        self.trace.callback("application.open").await;
        let interfaces = context
            .partition
            .with_factory(Arc::new(TraceReplicatorFactory {
                replicator: self.replicator.clone(),
                state_replicator: self.state_replicator.clone(),
            }))
            .create_replicator(Some(self.replicator.provider.clone()), None)
            .await?;
        Ok(interfaces.replicator())
    }

    async fn change_role(&self, role: ReplicaRole) -> crate::Result<RoleChange> {
        self.trace
            .arguments
            .lock()
            .unwrap()
            .push(format!("application.role:{role:?}"));
        self.trace.callback("application.change_role").await;
        if self.trace.fail_role.load(Ordering::Acquire) {
            return Err(crate::RuntimeError::Application("role failed".into()));
        }
        Ok(RoleChange {
            service_address: self.trace.service_address.lock().unwrap().clone(),
        })
    }

    async fn close(&self) -> crate::Result<()> {
        self.trace.callback("application.close").await;
        if self
            .trace
            .fail_close
            .lock()
            .unwrap()
            .contains(&"application")
        {
            return Err(crate::RuntimeError::Application(
                "application close failed".into(),
            ));
        }
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
    descendant_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    descendant_stopped: Arc<AtomicBool>,
    lifecycle_closed: Arc<AtomicBool>,
    containment: watch::Sender<bool>,
    cancel_descendant_with_root: AtomicBool,
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
        self.containment.send_replace(false);
        let (stop, mut receiver) = tokio::sync::watch::channel(false);
        *self.descendant_stop.lock().unwrap() = Some(stop);
        let stopped = self.descendant_stopped.clone();
        let trace = self.trace.clone();
        let containment = self.containment.clone();
        let task = tokio::spawn(async move {
            trace.record("provider.descendant.begin");
            let _ = receiver.wait_for(|stop| *stop).await;
            stopped.store(true, Ordering::Release);
            trace.record("provider.descendant.end");
            containment.send_replace(true);
        });
        *self.descendant_task.lock().unwrap() = Some(task);
    }
}

#[async_trait]
impl Replicator for TraceReplicator {
    async fn open(&self) -> crate::Result<String> {
        self.trace.callback("replicator.open").await;
        Ok("trace://replicator".into())
    }

    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> crate::Result<()> {
        self.trace
            .arguments
            .lock()
            .unwrap()
            .push(format!("replicator.role:{epoch:?}:{role:?}"));
        self.trace.callback("replicator.change_role").await;
        self.primary
            .store(role == ReplicaRole::Primary, Ordering::Release);
        Ok(())
    }

    async fn update_epoch(&self, epoch: Epoch) -> crate::Result<()> {
        self.trace
            .arguments
            .lock()
            .unwrap()
            .push(format!("replicator.epoch:{epoch:?}"));
        self.trace.callback("replicator.update_epoch").await;
        self.provider.update_epoch(epoch, 0).await
    }

    async fn close(&self) -> crate::Result<()> {
        self.trace.callback("replicator.close").await;
        if self
            .trace
            .fail_close
            .lock()
            .unwrap()
            .contains(&"replicator")
        {
            return Err(crate::RuntimeError::Application(
                "replicator close failed".into(),
            ));
        }
        self.lifecycle_closed.store(true, Ordering::Release);
        Ok(())
    }

    fn abort(&self) {
        self.trace.record("replicator.abort");
        self.lifecycle_closed.store(true, Ordering::Release);
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
        current: ReplicaSetConfiguration,
        previous: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        self.require_primary()?;
        self.trace
            .callback("primary.update_catch_up_configuration")
            .await;
        let configuration = crate::protocol::public_operations::PublicConfiguration {
            current: current.configuration,
            previous: Some(previous.configuration),
        };
        self.trace
            .configurations
            .lock()
            .unwrap()
            .push(configuration.clone());
        *self.trace.installed.lock().unwrap() = Some(configuration);
        Ok(())
    }

    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> crate::Result<()> {
        self.require_primary()?;
        if let Some(configuration) = self.trace.installed.lock().unwrap().clone() {
            self.trace.waits.lock().unwrap().push((configuration, mode));
        }
        self.trace.callback("primary.wait_for_catch_up").await;
        Ok(())
    }

    async fn update_current_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        self.require_primary()?;
        self.trace
            .callback("primary.update_current_configuration")
            .await;
        let configuration = crate::protocol::public_operations::PublicConfiguration {
            current: current.configuration,
            previous: None,
        };
        self.trace
            .configurations
            .lock()
            .unwrap()
            .push(configuration.clone());
        *self.trace.installed.lock().unwrap() = Some(configuration);
        Ok(())
    }

    async fn build_replica(&self, _replica: ReplicaInformation) -> crate::Result<()> {
        self.require_primary()?;
        self.start_descendant();
        struct CancelDescendant(Option<watch::Sender<bool>>);
        impl Drop for CancelDescendant {
            fn drop(&mut self) {
                if let Some(stop) = &self.0 {
                    stop.send_replace(true);
                }
            }
        }
        let _cancel = CancelDescendant(
            if self.cancel_descendant_with_root.load(Ordering::Acquire) {
                self.descendant_stop.lock().unwrap().clone()
            } else {
                None
            },
        );
        self.trace.callback("primary.build_replica").await;
        if let Some(stop) = self.descendant_stop.lock().unwrap().take() {
            stop.send_replace(true);
        }
        let task = self.descendant_task.lock().unwrap().take();
        if let Some(task) = task {
            task.await.map_err(|error| {
                crate::RuntimeError::Application(format!(
                    "trace provider descendant join failed: {error}"
                ))
            })?;
        }
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
    let lifecycle_closed = Arc::new(AtomicBool::new(false));
    let replicator = Arc::new(TraceReplicator {
        trace: trace.clone(),
        provider,
        primary: AtomicBool::new(false),
        descendant_stop: Mutex::new(None),
        descendant_task: Mutex::new(None),
        descendant_stopped: Arc::new(AtomicBool::new(false)),
        lifecycle_closed: lifecycle_closed.clone(),
        containment: watch::channel(true).0,
        cancel_descendant_with_root: AtomicBool::new(false),
    });
    let state_replicator = Arc::new(TraceStateReplicator {
        trace: trace.clone(),
        lifecycle_closed,
    });
    let application = Arc::new(TraceApplication {
        trace: trace.clone(),
        replicator: replicator.clone(),
        state_replicator: state_replicator.clone(),
        convergent_open: AtomicBool::new(false),
    });
    (trace, application, replicator, state_replicator)
}

async fn run_owned_trace<F, E>(
    runtime: &PublicOperationPreviewRuntime,
    preview: &PublicOperationPreviewIdentity,
    revision: u64,
    name: &str,
    future: F,
) -> crate::host::state::PublicOperationRecord
where
    F: std::future::Future<Output = std::result::Result<(), E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    tokio::time::timeout(
        Duration::from_secs(1),
        runtime.run_root(
            intent(
                preview,
                name,
                revision,
                PublicOperationClass::Authority,
                "session-1",
            ),
            CallbackContainment::RootTask,
            future,
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("owned trace callback {name} timed out"))
    .unwrap()
}

#[tokio::test]
async fn strict_trace_fixture_covers_the_public_callback_inventory_and_primary_guard() {
    let preview = PublicOperationPreviewIdentity::new(60);
    let (_directory, _path, store) = preview_store(&preview);
    let runtime = PublicOperationPreviewRuntime::start(
        store,
        preview.clone(),
        ProcessSessionId::new("session-1"),
    )
    .unwrap();
    let (trace, application, replicator, state_replicator) = trace_fixture();
    let callback = application.clone();
    run_owned_trace(&runtime, &preview, 1, "application-open", async move {
        callback.trace_open().await.map(|_| ())
    })
    .await;
    let callback = application.clone();
    run_owned_trace(&runtime, &preview, 2, "application-role", async move {
        callback.change_role(ReplicaRole::Primary).await.map(|_| ())
    })
    .await;
    let callback = application.clone();
    run_owned_trace(&runtime, &preview, 3, "application-close", async move {
        callback.close().await
    })
    .await;
    application.abort();

    let empty = ReplicaSetConfiguration::from(ConfigurationDescriptor::new(
        Epoch::default(),
        ReplicaId::new(1),
        Vec::new(),
        1,
    ));
    let callback = replicator.clone();
    run_owned_trace(&runtime, &preview, 4, "replicator-open", async move {
        callback.open().await.map(|_| ())
    })
    .await;
    let callback = replicator.clone();
    let configuration = empty.clone();
    let rejected = run_owned_trace(
        &runtime,
        &preview,
        5,
        "pre-primary-configuration",
        async move {
            callback
                .update_current_replica_set_configuration(configuration)
                .await
        },
    )
    .await;
    assert!(matches!(
        rejected.disposition,
        Some(PublicOperationDisposition::Failed(message))
            if message.contains("not primary")
    ));
    let callback = replicator.clone();
    run_owned_trace(&runtime, &preview, 6, "replicator-role", async move {
        callback
            .change_role(Epoch::default(), ReplicaRole::Primary)
            .await
    })
    .await;
    let callback = replicator.clone();
    run_owned_trace(&runtime, &preview, 7, "replicator-epoch", async move {
        callback.update_epoch(Epoch::default()).await
    })
    .await;
    let callback = replicator.clone();
    run_owned_trace(&runtime, &preview, 8, "current-progress", async move {
        callback.current_progress().await.map(|_| ())
    })
    .await;
    let callback = replicator.clone();
    run_owned_trace(&runtime, &preview, 9, "catch-up-capability", async move {
        callback.catch_up_capability().await.map(|_| ())
    })
    .await;
    let callback = replicator.clone();
    run_owned_trace(&runtime, &preview, 10, "data-loss", async move {
        callback.on_data_loss().await.map(|_| ())
    })
    .await;
    let callback = replicator.provider.clone();
    run_owned_trace(
        &runtime,
        &preview,
        11,
        "provider-last-committed",
        async move { callback.last_committed_lsn().await.map(|_| ()) },
    )
    .await;
    let callback = replicator.provider.clone();
    run_owned_trace(
        &runtime,
        &preview,
        12,
        "provider-copy-context",
        async move { callback.get_copy_context().await.map(|_| ()) },
    )
    .await;
    let callback = replicator.provider.clone();
    run_owned_trace(&runtime, &preview, 13, "provider-copy-state", async move {
        callback
            .get_copy_state(0, Box::pin(futures::stream::empty()))
            .await
            .map(|_| ())
    })
    .await;
    let callback = replicator.clone();
    let current = empty.clone();
    let previous = empty.clone();
    run_owned_trace(
        &runtime,
        &preview,
        14,
        "catch-up-configuration",
        async move {
            callback
                .update_catch_up_replica_set_configuration(current, previous)
                .await
        },
    )
    .await;
    let callback = replicator.clone();
    run_owned_trace(&runtime, &preview, 15, "catch-up-wait", async move {
        callback
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::All)
            .await
    })
    .await;
    let callback = replicator.clone();
    let configuration = empty;
    run_owned_trace(
        &runtime,
        &preview,
        16,
        "current-configuration",
        async move {
            callback
                .update_current_replica_set_configuration(configuration)
                .await
        },
    )
    .await;
    let callback = replicator.clone();
    run_owned_trace(&runtime, &preview, 17, "build", async move {
        callback
            .build_replica(ReplicaInformation::new(
                OperationId::new("build"),
                replica_identity(),
                "trace://target".into(),
            ))
            .await
    })
    .await;
    let callback = replicator.clone();
    run_owned_trace(&runtime, &preview, 18, "remove", async move {
        callback.remove_replica(ReplicaId::new(2)).await
    })
    .await;
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
    let callback = replicator.clone();
    run_owned_trace(&runtime, &preview, 19, "replicator-close", async move {
        callback.close().await
    })
    .await;
    assert!(matches!(
        state_replicator
            .replicate(bytes::Bytes::from_static(b"after-close"))
            .await,
        Err(crate::RuntimeError::Closed)
    ));
    replicator.abort();
    assert!(matches!(
        state_replicator.get_copy_stream().await,
        Err(crate::RuntimeError::Closed)
    ));
    runtime.shutdown().await.unwrap();

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
        "provider.last_committed_lsn.end",
        "provider.get_copy_context.end",
        "provider.get_copy_state.end",
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
async fn state_replicator_closes_on_abort_without_prior_close() {
    let (trace, _application, replicator, state_replicator) = trace_fixture();
    state_replicator
        .replicate(bytes::Bytes::from_static(b"before-abort"))
        .await
        .unwrap();
    replicator.abort();
    assert!(matches!(
        state_replicator
            .replicate(bytes::Bytes::from_static(b"after-abort"))
            .await,
        Err(crate::RuntimeError::Closed)
    ));
    assert!(
        trace
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "replicator.abort")
    );
}

async fn assert_root_callback_cancelled<F, E>(
    generation: u64,
    trace: Arc<Trace>,
    callback_name: &'static str,
    future: F,
) where
    F: std::future::Future<Output = std::result::Result<(), E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    trace.block(callback_name);
    let preview = PublicOperationPreviewIdentity::new(generation);
    let (_directory, _path, store) = preview_store(&preview);
    let registry =
        PartitionOperationRegistry::new(store, preview.clone(), ProcessSessionId::new("session-1"))
            .unwrap();
    let runtime = PartitionOperationRuntime::start(registry.clone());
    let operation = registry
        .admit(intent(
            &preview,
            "blocked-callback",
            1,
            PublicOperationClass::Authority,
            "session-1",
        ))
        .await
        .unwrap();
    operation
        .spawn_root(CallbackContainment::RootTask, future)
        .await
        .unwrap();
    trace.wait_for(&format!("{callback_name}.begin")).await;

    let successor = registry
        .admit(intent(
            &preview,
            "successor",
            2,
            PublicOperationClass::Authority,
            "session-1",
        ))
        .await
        .unwrap();
    let cancelled = wait_for_stage(&operation, PublicOperationStage::Completed).await;
    assert_eq!(
        cancelled.disposition,
        Some(PublicOperationDisposition::Cancelled)
    );
    trace.wait_for(&format!("{callback_name}.cancel")).await;
    wait_for_stage(&successor, PublicOperationStage::Ready).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn every_non_build_public_callback_is_independently_cancellable() {
    let (trace, application, _replicator, _state_replicator) = trace_fixture();
    let callback = application.clone();
    assert_root_callback_cancelled(70, trace, "application.open", async move {
        callback.trace_open().await.map(|_| ())
    })
    .await;

    let (trace, application, _replicator, _state_replicator) = trace_fixture();
    let callback = application.clone();
    assert_root_callback_cancelled(71, trace, "application.change_role", async move {
        callback.change_role(ReplicaRole::Primary).await.map(|_| ())
    })
    .await;

    let (trace, application, _replicator, _state_replicator) = trace_fixture();
    let callback = application.clone();
    assert_root_callback_cancelled(72, trace, "application.close", async move {
        callback.close().await
    })
    .await;

    let (trace, _application, replicator, _state_replicator) = trace_fixture();
    let callback = replicator.clone();
    assert_root_callback_cancelled(73, trace, "replicator.open", async move {
        callback.open().await.map(|_| ())
    })
    .await;

    let (trace, _application, replicator, _state_replicator) = trace_fixture();
    let callback = replicator.clone();
    assert_root_callback_cancelled(74, trace, "replicator.change_role", async move {
        callback
            .change_role(Epoch::default(), ReplicaRole::Primary)
            .await
    })
    .await;

    let (trace, _application, replicator, _state_replicator) = trace_fixture();
    let callback = replicator.clone();
    assert_root_callback_cancelled(75, trace, "replicator.update_epoch", async move {
        callback.update_epoch(Epoch::default()).await
    })
    .await;

    let (trace, _application, replicator, _state_replicator) = trace_fixture();
    let callback = replicator.clone();
    assert_root_callback_cancelled(76, trace, "replicator.current_progress", async move {
        callback.current_progress().await.map(|_| ())
    })
    .await;

    let (trace, _application, replicator, _state_replicator) = trace_fixture();
    let callback = replicator.clone();
    assert_root_callback_cancelled(77, trace, "replicator.catch_up_capability", async move {
        callback.catch_up_capability().await.map(|_| ())
    })
    .await;

    let (trace, _application, replicator, _state_replicator) = trace_fixture();
    let callback = replicator.clone();
    assert_root_callback_cancelled(78, trace, "replicator.close", async move {
        callback.close().await
    })
    .await;

    let (trace, _application, replicator, _state_replicator) = trace_fixture();
    replicator
        .change_role(Epoch::default(), ReplicaRole::Primary)
        .await
        .unwrap();
    let callback = replicator.clone();
    assert_root_callback_cancelled(79, trace, "primary.on_data_loss", async move {
        callback.on_data_loss().await.map(|_| ())
    })
    .await;

    let configuration = ReplicaSetConfiguration::from(ConfigurationDescriptor::new(
        Epoch::default(),
        ReplicaId::new(1),
        Vec::new(),
        1,
    ));
    let (trace, _application, replicator, _state_replicator) = trace_fixture();
    replicator
        .change_role(Epoch::default(), ReplicaRole::Primary)
        .await
        .unwrap();
    let callback = replicator.clone();
    let current = configuration.clone();
    let previous = configuration.clone();
    assert_root_callback_cancelled(
        80,
        trace,
        "primary.update_catch_up_configuration",
        async move {
            callback
                .update_catch_up_replica_set_configuration(current, previous)
                .await
        },
    )
    .await;

    let (trace, _application, replicator, _state_replicator) = trace_fixture();
    replicator
        .change_role(Epoch::default(), ReplicaRole::Primary)
        .await
        .unwrap();
    let callback = replicator.clone();
    assert_root_callback_cancelled(81, trace, "primary.wait_for_catch_up", async move {
        callback
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::All)
            .await
    })
    .await;

    let (trace, _application, replicator, _state_replicator) = trace_fixture();
    replicator
        .change_role(Epoch::default(), ReplicaRole::Primary)
        .await
        .unwrap();
    let callback = replicator.clone();
    let current = configuration;
    assert_root_callback_cancelled(
        82,
        trace,
        "primary.update_current_configuration",
        async move {
            callback
                .update_current_replica_set_configuration(current)
                .await
        },
    )
    .await;

    let (trace, _application, replicator, _state_replicator) = trace_fixture();
    replicator
        .change_role(Epoch::default(), ReplicaRole::Primary)
        .await
        .unwrap();
    let callback = replicator.clone();
    assert_root_callback_cancelled(83, trace, "primary.remove_replica", async move {
        callback.remove_replica(ReplicaId::new(2)).await
    })
    .await;
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
        .spawn_root(CallbackContainment::ObjectOwnedOnInterruption, async move {
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
