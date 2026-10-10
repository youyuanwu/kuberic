use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::host::process::{
    PREVIEW_RESTART_DISPOSITION, PreviewChildCommand, PreviewChildEvidence, PreviewRestartCut,
    ReplicaProcessSupervisor,
};
use crate::host::sqlite_store::SqliteStore;
use crate::host::state::{AgentState, PUBLIC_OPERATION_PREVIEW_SCHEMA_VERSION, StorageIdentity};
use crate::host::store::AgentStore;
use crate::protocol::public_operations::{
    FrozenReplicaResources, PreviewLifecycleBinding, PublicFaultAction, PublicFaultActionKind,
    PublicOperationPreviewIdentity, RestartActionStage, StatePersistence,
};
use crate::protocol::types::{
    AccessStatus, AgentGeneration, EffectivePolicy, FaultType, InitializationId, OperationId,
    PodUid, ProcessSessionId, PvcUid, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole,
    ResourceUid,
};

const CHILD_OUTPUT: &str = "KUBERIC_PREVIEW_CHILD_OUTPUT";

#[test]
fn preview_process_child_entrypoint() {
    let Ok(output) = std::env::var(CHILD_OUTPUT) else {
        return;
    };
    let data_root = PathBuf::from(std::env::var("KUBERIC_PREVIEW_DATA_ROOT").unwrap());
    let provider = data_root.join("provider.sentinel");
    let sentinel = if provider.exists() {
        std::fs::read_to_string(&provider).unwrap()
    } else {
        let sentinel = "provider-state-survives".to_string();
        std::fs::create_dir_all(&data_root).unwrap();
        std::fs::write(&provider, &sentinel).unwrap();
        sentinel
    };
    let constructions = data_root.join("child-constructions");
    let count = std::fs::read_to_string(&constructions)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        + 1;
    std::fs::write(&constructions, count.to_string()).unwrap();
    let evidence = PreviewChildEvidence {
        process_session: ProcessSessionId::new(uuid::Uuid::new_v4().to_string()),
        data_root: data_root.clone(),
        pod_uid: PodUid::new(std::env::var("KUBERIC_PREVIEW_POD_UID").unwrap()),
        pvc_uid: PvcUid::new(std::env::var("KUBERIC_PREVIEW_PVC_UID").unwrap()),
        provider_sentinel: sentinel,
        application_constructed: true,
        replicator_constructed: true,
    };
    if let Some(parent) = Path::new(&output).parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(output, serde_json::to_vec(&evidence).unwrap()).unwrap();
    if std::env::var("KUBERIC_PREVIEW_CHILD_MODE").unwrap() == "predecessor" {
        let release = PathBuf::from(std::env::var("KUBERIC_PREVIEW_RELEASE").unwrap());
        while !release.exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::process::exit(PREVIEW_RESTART_DISPOSITION);
    }
}

#[tokio::test]
async fn persisted_restart_uses_fresh_child_on_the_same_storage() {
    let fixture = Fixture::new();
    let (mut predecessor, predecessor_evidence) = fixture.spawn_predecessor().await;
    let action = fixture.action(predecessor_evidence.process_session.clone());
    std::fs::write(&fixture.release, b"restart").unwrap();

    let result = fixture
        .supervisor
        .restart_with_child(&action, &mut predecessor, None)
        .await
        .unwrap();
    assert_eq!(result.record.stage, RestartActionStage::SuccessorStarted);
    assert_ne!(
        result.successor.process_session,
        predecessor_evidence.process_session
    );
    assert_eq!(result.successor.data_root, fixture.data_root);
    assert_eq!(
        result.successor.provider_sentinel,
        predecessor_evidence.provider_sentinel
    );
    assert!(predecessor.try_wait().unwrap().is_some());
    assert_eq!(
        std::fs::read_to_string(fixture.data_root.join("child-constructions")).unwrap(),
        "2"
    );
    let state = fixture.store.load_state().await.unwrap();
    assert_eq!(state.role, ReplicaRole::None);
    assert_eq!(state.read_status, AccessStatus::NotPrimary);
    assert_eq!(state.write_status, AccessStatus::NotPrimary);
    assert!(state.previous_configuration.is_none());
    assert!(state.current_configuration.is_none());
    drop(state);
    drop(result);
    let database = SqliteStore::metadata_database_path(&fixture.data_root);
    SqliteStore::open_preview_bound_existing(&database, None, &fixture.binding).unwrap();
    let mut changed = fixture.binding.clone();
    changed.state_persistence = StatePersistence::Volatile;
    assert!(matches!(
        SqliteStore::open_preview_bound_existing(&database, None, &changed),
        Err(crate::host::HostError::IdentityMismatch(_))
    ));
}

#[tokio::test]
async fn restart_crash_cuts_resume_exactly_once_and_uncontained_work_stays_closed() {
    let fixture = Fixture::new();
    let action = fixture.action(ProcessSessionId::new("predecessor-session"));
    assert!(
        fixture
            .supervisor
            .resume(&action, true, Some(PreviewRestartCut::Accepted))
            .await
            .unwrap_err()
            .to_string()
            .contains("after durable restart acceptance")
    );
    assert!(
        fixture
            .supervisor
            .resume(&action, false, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("containment is unproven")
    );
    assert_eq!(
        fixture.store.restart_action().await.unwrap().unwrap().stage,
        RestartActionStage::Accepted
    );
    assert!(!fixture.data_root.join("child-constructions").exists());

    assert!(
        fixture
            .supervisor
            .resume(&action, true, Some(PreviewRestartCut::PredecessorContained),)
            .await
            .unwrap_err()
            .to_string()
            .contains("after predecessor containment")
    );
    assert_eq!(
        fixture.store.restart_action().await.unwrap().unwrap().stage,
        RestartActionStage::PredecessorContained
    );

    assert!(
        fixture
            .supervisor
            .resume(&action, true, Some(PreviewRestartCut::SuccessorStarted),)
            .await
            .unwrap_err()
            .to_string()
            .contains("after successor start")
    );
    let started = fixture.store.restart_action().await.unwrap().unwrap();
    assert_eq!(started.stage, RestartActionStage::SuccessorStarted);
    let session = started.successor_session.clone().unwrap();
    let completed = fixture
        .supervisor
        .resume(&action, false, None)
        .await
        .unwrap();
    assert_eq!(completed.record.successor_session, Some(session));
    assert_eq!(
        std::fs::read_to_string(fixture.data_root.join("child-constructions")).unwrap(),
        "1"
    );
}

struct Fixture {
    _directory: tempfile::TempDir,
    data_root: PathBuf,
    release: PathBuf,
    store: Arc<SqliteStore>,
    binding: PreviewLifecycleBinding,
    identity: ReplicaIdentity,
    supervisor: ReplicaProcessSupervisor,
}

impl Fixture {
    fn new() -> Self {
        let directory = super::tempdir().unwrap();
        let data_root = directory.path().join("data");
        let release = directory.path().join("release");
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(1),
            instance_id: ReplicaInstanceId::new("pod-1"),
            agent_generation: AgentGeneration::new("agent-1"),
        };
        let binding = PreviewLifecycleBinding {
            preview: PublicOperationPreviewIdentity::new(44),
            resource_uid: ResourceUid::new("resource-1"),
            spec_generation: 9,
            state_persistence: StatePersistence::Persisted,
        };
        let storage = StorageIdentity {
            schema_version: PUBLIC_OPERATION_PREVIEW_SCHEMA_VERSION,
            resource_uid: binding.resource_uid.clone(),
            pod_uid: PodUid::new("pod-1"),
            pvc_uid: PvcUid::new("pvc-1"),
            initialization_id: InitializationId::new("init-1"),
            local_identity: identity.clone(),
            effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
        };
        let database = SqliteStore::metadata_database_path(&data_root);
        let store = Arc::new(
            SqliteStore::create_preview_bound_authorized(
                database,
                AgentState::new(storage),
                binding.clone(),
            )
            .unwrap(),
        );
        let child = PreviewChildCommand {
            executable: std::env::current_exe().unwrap(),
            arguments: vec![
                "preview_process_child_entrypoint".into(),
                "--nocapture".into(),
            ],
            environment: BTreeMap::new(),
        };
        let supervisor = ReplicaProcessSupervisor::new(store.clone(), data_root.clone(), child);
        Self {
            _directory: directory,
            data_root,
            release,
            store,
            binding,
            identity,
            supervisor,
        }
    }

    async fn spawn_predecessor(&self) -> (tokio::process::Child, PreviewChildEvidence) {
        let output = self.data_root.join("predecessor.json");
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
        child
            .arg("preview_process_child_entrypoint")
            .arg("--nocapture")
            .env(CHILD_OUTPUT, &output)
            .env("KUBERIC_PREVIEW_DATA_ROOT", &self.data_root)
            .env("KUBERIC_PREVIEW_CHILD_MODE", "predecessor")
            .env("KUBERIC_PREVIEW_RELEASE", &self.release)
            .env("KUBERIC_PREVIEW_POD_UID", "pod-1")
            .env("KUBERIC_PREVIEW_PVC_UID", "pvc-1")
            .kill_on_drop(true);
        let child = child.spawn().unwrap();
        for _ in 0..200 {
            if output.exists() {
                let evidence = serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
                return (child, evidence);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("predecessor child did not report");
    }

    fn action(&self, predecessor_session: ProcessSessionId) -> PublicFaultAction {
        let mut action = PublicFaultAction {
            action_id: OperationId::new("pending"),
            binding: self.binding.clone(),
            target: self.identity.clone(),
            resources: FrozenReplicaResources {
                pod_name: "replica-1".into(),
                pod_uid: PodUid::new("pod-1"),
                pvc_name: "replica-1-data".into(),
                pvc_uid: PvcUid::new("pvc-1"),
            },
            predecessor_session,
            fault_revision: 3,
            fault: FaultType::Transient,
            kind: PublicFaultActionKind::Restart,
        };
        action.action_id = action.expected_id();
        action
    }
}
