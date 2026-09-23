use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{env, process::Command};

use async_trait::async_trait;
use kuberic_agent::recovery::{RecoveryDecision, inspect_recovery, recover_pending};
use kuberic_agent::runtime_adapter::{RuntimeAdapter, RuntimeEffectExecutor};
use kuberic_agent::sqlite_store::SqliteStore;
use kuberic_agent::state::{AgentState, SCHEMA_VERSION, StorageIdentity};
use kuberic_agent::store::{AgentStore, BeginEffect};
use kuberic_agent::{AgentError, Result};
use kuberic_protocol::types::{
    AccessStatus, AgentGeneration, EffectivePolicy, Epoch, InitializationId, OperationId, PodUid,
    PvcUid, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ResourceUid,
};
use kuberic_runtime_internal::effects::{
    OpenMode, RuntimeEffect, RuntimeEffectAction, RuntimeEffectResult, RuntimePostcondition,
    RuntimeSnapshot,
};
use tempfile::tempdir;

struct FakeRuntime {
    calls: AtomicUsize,
    result: RuntimeEffectResult,
}

#[async_trait]
impl RuntimeEffectExecutor for FakeRuntime {
    async fn apply_runtime_effect(&self, _effect: RuntimeEffect) -> Result<RuntimeEffectResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.result.clone())
    }
}

fn storage_identity() -> StorageIdentity {
    StorageIdentity {
        schema_version: SCHEMA_VERSION,
        resource_uid: ResourceUid::new("resource-1"),
        pod_uid: PodUid::new("pod-1"),
        pvc_uid: PvcUid::new("pvc-1"),
        initialization_id: InitializationId::new("init-1"),
        local_identity: ReplicaIdentity {
            replica_id: ReplicaId::new(1),
            instance_id: ReplicaInstanceId::new("instance-1"),
            agent_generation: AgentGeneration::new("generation-1"),
        },
        effective_policy: EffectivePolicy::fixed(3, 30).unwrap(),
    }
}

fn effect() -> RuntimeEffect {
    RuntimeEffect {
        operation_id: OperationId::new("effect-1"),
        sequence: 1,
        action: RuntimeEffectAction::Open(OpenMode::Existing),
    }
}

fn result() -> RuntimeEffectResult {
    RuntimeEffectResult {
        operation_id: OperationId::new("effect-1"),
        sequence: 1,
        postcondition: RuntimePostcondition {
            open: true,
            role: ReplicaRole::None,
            role_transition: None,
            read_status: AccessStatus::NotPrimary,
            write_status: AccessStatus::NotPrimary,
            authority: None,
            current_progress: 0,
            verified_replication_lsn: None,
            committed_lsn: 0,
            current_configuration_quorum_progress: 0,
            catch_up_boundary: None,
            catch_up_complete: false,
            builds: Vec::new(),
        },
    }
}

fn snapshot(write_status: AccessStatus) -> RuntimeSnapshot {
    RuntimeSnapshot {
        identity: storage_identity().local_identity,
        open: false,
        replication_address: None,
        role: ReplicaRole::None,
        role_transition: None,
        read_status: AccessStatus::NotPrimary,
        write_status,
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

#[tokio::test]
async fn recovery_reissues_committed_intent_without_reporting_completion() {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let store = SqliteStore::create_authorized(&path, AgentState::new(storage_identity())).unwrap();
    assert_eq!(
        store.begin_effect(&effect()).await.unwrap(),
        BeginEffect::Execute(effect())
    );
    drop(store);

    let reopened = Arc::new(SqliteStore::open_existing(&path, None).unwrap());
    assert_eq!(
        inspect_recovery(reopened.as_ref(), &snapshot(AccessStatus::NotPrimary))
            .await
            .unwrap(),
        RecoveryDecision::Reissue(effect())
    );
    let runtime = Arc::new(FakeRuntime {
        calls: AtomicUsize::new(0),
        result: result(),
    });
    let adapter = RuntimeAdapter::new(reopened.clone(), runtime.clone());
    assert_eq!(
        recover_pending(&adapter, &snapshot(AccessStatus::NotPrimary))
            .await
            .unwrap(),
        Some(result())
    );
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    assert!(
        reopened
            .load_state()
            .await
            .unwrap()
            .pending_effect
            .is_none()
    );
}

#[tokio::test]
async fn recovery_reobserves_effect_applied_before_completion_persistence() {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let store = SqliteStore::create_authorized(&path, AgentState::new(storage_identity())).unwrap();
    store.begin_effect(&effect()).await.unwrap();
    store.mark_effect_applied(&effect()).await.unwrap();
    drop(store);

    let reopened = Arc::new(SqliteStore::open_existing(&path, None).unwrap());
    let runtime = Arc::new(FakeRuntime {
        calls: AtomicUsize::new(0),
        result: result(),
    });
    let adapter = RuntimeAdapter::new(reopened.clone(), runtime.clone());
    let recovered = recover_pending(&adapter, &snapshot(AccessStatus::NotPrimary))
        .await
        .unwrap();
    assert_eq!(recovered, Some(result()));
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        reopened.retained_result().await.unwrap().unwrap().result,
        result()
    );
}

#[tokio::test]
async fn bootstrap_terminal_result_survives_transition_clearing_and_reopen() {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let store = Arc::new(
        SqliteStore::create_authorized(&path, AgentState::new(storage_identity())).unwrap(),
    );
    let runtime = Arc::new(FakeRuntime {
        calls: AtomicUsize::new(0),
        result: result(),
    });
    let adapter = RuntimeAdapter::new(store.clone(), runtime);
    adapter.execute(effect()).await.unwrap();

    store
        .set_reconfiguration(Some("transition-stage".into()))
        .await
        .unwrap();
    store.clear_reconfiguration().await.unwrap();
    drop(adapter);
    drop(store);

    let reopened = SqliteStore::open_existing(&path, None).unwrap();
    let state = reopened.load_state().await.unwrap();
    assert!(state.reconfiguration_data.is_none());
    assert_eq!(state.retained_result.unwrap().result, result());
}

#[tokio::test]
async fn recovery_refuses_a_runtime_that_did_not_start_write_closed() {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let store = SqliteStore::create_authorized(&path, AgentState::new(storage_identity())).unwrap();
    assert!(matches!(
        inspect_recovery(&store, &snapshot(AccessStatus::Granted)).await,
        Err(AgentError::EffectConflict(_))
    ));
}

#[test]
fn process_session_changes_without_changing_durable_generation() {
    let first = kuberic_agent::session::ProcessSession::new();
    let second = kuberic_agent::session::ProcessSession::new();
    assert_ne!(first.id(), second.id());
    assert_eq!(first.next_report_sequence(), 1);
    assert_eq!(first.next_report_sequence(), 2);
    assert_eq!(
        storage_identity().local_identity.agent_generation,
        AgentGeneration::new("generation-1")
    );
    assert_eq!(Epoch::default(), Epoch::new(0, 0));
}

#[test]
fn sqlite_commits_survive_process_termination_without_destructors() {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let output = Command::new(env::current_exe().unwrap())
        .args(["--ignored", "--exact", "crash_writer_process"])
        .env("KUBERIC_CRASH_WRITER_PATH", &path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let reopened = SqliteStore::open_existing(&path, Some(&storage_identity())).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let state = runtime.block_on(reopened.load_state()).unwrap();
    assert_eq!(state.pending_effect.unwrap().effect, effect());
}

#[test]
#[ignore = "helper process for sqlite_commits_survive_process_termination_without_destructors"]
fn crash_writer_process() {
    let Ok(path) = env::var("KUBERIC_CRASH_WRITER_PATH") else {
        return;
    };
    let store = SqliteStore::create_authorized(path, AgentState::new(storage_identity())).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(store.begin_effect(&effect())).unwrap();
    std::process::exit(0);
}
