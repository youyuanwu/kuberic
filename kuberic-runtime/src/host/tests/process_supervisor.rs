use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::Result as RuntimeResult;
use crate::application::{OpenContext, RoleChange, StatefulServiceReplica};
use crate::host::process::{
    PREVIEW_RESTART_DISPOSITION, PreviewChildCommand, PreviewChildEvidence, PreviewChildProcess,
    PreviewRestartCut, ReplicaProcessSupervisor,
};
use crate::host::sqlite_store::SqliteStore;
use crate::host::state::{AgentState, PUBLIC_OPERATION_PREVIEW_SCHEMA_VERSION, StorageIdentity};
use crate::host::store::AgentStore;
use crate::protocol::public_operations::{
    FrozenReplicaResources, PreviewLifecycleBinding, PublicFaultAction, PublicFaultActionKind,
    PublicOperationPreviewIdentity, RestartActionStage, StatePersistence,
};
use crate::protocol::types::{
    AccessStatus, EffectivePolicy, Epoch, FaultType, OperationId, PodUid, ProcessSessionId, PvcUid,
    ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ResourceUid,
};
use crate::replicator::Replicator;

const CHILD_OUTPUT: &str = "KUBERIC_PREVIEW_CHILD_OUTPUT";
const PARENT_MARKER_ROOT: &str = "KUBERIC_PREVIEW_PARENT_MARKER_ROOT";

fn wire<T: serde::Serialize, U: serde::de::DeserializeOwned>(value: T) -> U {
    serde_json::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap()
}

struct PreviewChildReplicator {
    instance_id: String,
}

#[async_trait]
impl Replicator for PreviewChildReplicator {
    async fn open(&self) -> RuntimeResult<String> {
        Ok("preview://child".into())
    }

    async fn change_role(&self, _epoch: Epoch, _role: ReplicaRole) -> RuntimeResult<()> {
        Ok(())
    }

    async fn update_epoch(&self, _epoch: Epoch) -> RuntimeResult<()> {
        Ok(())
    }

    async fn close(&self) -> RuntimeResult<()> {
        Ok(())
    }

    fn abort(&self) {}

    async fn current_progress(&self) -> RuntimeResult<i64> {
        Ok(0)
    }

    async fn catch_up_capability(&self) -> RuntimeResult<i64> {
        Ok(0)
    }
}

struct PreviewChildApplication {
    instance_id: String,
    replicator: Arc<PreviewChildReplicator>,
}

#[async_trait]
impl StatefulServiceReplica for PreviewChildApplication {
    async fn open(self: Arc<Self>, _context: OpenContext) -> RuntimeResult<Arc<dyn Replicator>> {
        Ok(self.replicator.clone())
    }

    async fn change_role(&self, _role: ReplicaRole) -> RuntimeResult<RoleChange> {
        Ok(RoleChange {
            service_address: None,
        })
    }

    async fn close(&self) -> RuntimeResult<()> {
        Ok(())
    }

    fn abort(&self) {}
}

struct ControllerSupervisorExecutor {
    supervisor: Arc<ReplicaProcessSupervisor>,
    predecessor: tokio::sync::Mutex<Option<PreviewChildProcess>>,
    store: Arc<SqliteStore>,
    binding: PreviewLifecycleBinding,
    identity: ReplicaIdentity,
}

#[async_trait]
impl kuberic_controller::cluster_api::PreviewFaultCommandExecutor for ControllerSupervisorExecutor {
    async fn execute_restart(
        &self,
        action: &kuberic_controller::protocol::public_operations::PublicFaultAction,
    ) -> kuberic_controller::Result<kuberic_controller::cluster_api::PreviewRestartExecution> {
        let action: PublicFaultAction = wire(action);
        let mut predecessor = self.predecessor.lock().await.take().ok_or_else(|| {
            kuberic_controller::ControllerError::Effect(
                "predecessor child was already consumed".into(),
            )
        })?;
        let result = self
            .supervisor
            .restart_with_child(&action, &mut predecessor, None)
            .await
            .map_err(|error| kuberic_controller::ControllerError::Effect(error.to_string()))?;
        let successor = result.record.successor_session.clone().ok_or_else(|| {
            kuberic_controller::ControllerError::Effect(
                "supervisor omitted successor session".into(),
            )
        })?;
        let mut lifecycle = crate::host::public_lifecycle::report(
            self.store.as_ref(),
            &self.binding.preview,
            &successor,
        )
        .await
        .map_err(|error| kuberic_controller::ControllerError::Effect(error.to_string()))?;
        lifecycle.process_id = result.successor.child_pid;
        let state = self
            .store
            .load_state()
            .await
            .map_err(|error| kuberic_controller::ControllerError::Effect(error.to_string()))?;
        let report = crate::protocol::observation::AgentReport {
            protocol_version: crate::protocol::PROTOCOL_VERSION,
            resource_uid: self.binding.resource_uid.clone(),
            identity: self.identity.clone(),
            process_session_id: successor,
            report_sequence: 1,
            role: ReplicaRole::None,
            read_status: AccessStatus::NotPrimary,
            write_status: AccessStatus::NotPrimary,
            healthy: true,
            epoch: state.highest_epoch,
            reported_fault: None,
            public_lifecycle_report: Some(Box::new(lifecycle)),
            restart_action: Some(Box::new(result.record.clone())),
            ..Default::default()
        };
        Ok(kuberic_controller::cluster_api::PreviewRestartExecution {
            record: wire(result.record),
            report: wire(report),
        })
    }
}

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
    let process_session = ProcessSessionId::new(uuid::Uuid::new_v4().to_string());
    let replicator = Arc::new(PreviewChildReplicator {
        instance_id: format!("replicator-{process_session}"),
    });
    let application = Arc::new(PreviewChildApplication {
        instance_id: format!("application-{process_session}"),
        replicator: replicator.clone(),
    });
    let _application: Arc<dyn StatefulServiceReplica> = application.clone();
    let _replicator: Arc<dyn Replicator> = replicator.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        replicator.open().await.unwrap();
        replicator
            .change_role(Epoch::default(), ReplicaRole::None)
            .await
            .unwrap();
        application.change_role(ReplicaRole::None).await.unwrap();
    });
    let evidence = PreviewChildEvidence {
        process_session,
        child_pid: std::process::id(),
        data_root: data_root.clone(),
        pod_uid: PodUid::new(std::env::var("KUBERIC_PREVIEW_POD_UID").unwrap()),
        pvc_uid: PvcUid::new(std::env::var("KUBERIC_PREVIEW_PVC_UID").unwrap()),
        provider_sentinel: sentinel,
        application_instance_id: application.instance_id.clone(),
        replicator_instance_id: replicator.instance_id.clone(),
        callbacks: vec![
            "replicator.open".into(),
            "replicator.change_role.none".into(),
            "application.change_role.none".into(),
        ],
        launch_nonce: std::env::var("KUBERIC_PREVIEW_LAUNCH_NONCE")
            .unwrap_or_else(|_| "predecessor".into()),
    };
    if let Ok(delay) = std::env::var("KUBERIC_PREVIEW_READY_DELAY_MS") {
        std::thread::sleep(std::time::Duration::from_millis(delay.parse().unwrap()));
    }
    if std::env::var_os("KUBERIC_PREVIEW_EXIT_BEFORE_READY").is_some() {
        std::process::exit(42);
    }

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
    let shutdown = PathBuf::from(std::env::var("KUBERIC_PREVIEW_SHUTDOWN").unwrap());
    while !shutdown.exists() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn preview_supervisor_parent_entrypoint() {
    let Ok(root) = std::env::var(PARENT_MARKER_ROOT) else {
        return;
    };
    let root = PathBuf::from(root);
    ReplicaProcessSupervisor::record_parent_identity(&root).unwrap();
    let ready = PathBuf::from(std::env::var("KUBERIC_PREVIEW_PARENT_READY").unwrap());
    let release = PathBuf::from(std::env::var("KUBERIC_PREVIEW_PARENT_RELEASE").unwrap());
    std::fs::write(ready, b"ready").unwrap();
    while !release.exists() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[tokio::test]
async fn persisted_restart_uses_fresh_child_on_the_same_storage() {
    let fixture = Fixture::new();
    let mut predecessor = fixture.spawn_predecessor().await;
    let action = fixture
        .action(
            predecessor.evidence.process_session.clone(),
            predecessor.evidence.child_pid,
        )
        .await;

    let result = fixture
        .supervisor
        .restart_with_child(&action, &mut predecessor, None)
        .await
        .unwrap();
    assert_eq!(result.record.stage, RestartActionStage::SuccessorStarted);
    assert_ne!(
        result.successor.process_session,
        predecessor.evidence.process_session
    );
    assert_eq!(result.successor.data_root, fixture.data_root);
    assert_eq!(
        result.successor.provider_sentinel,
        predecessor.evidence.provider_sentinel
    );
    assert!(Path::new(&format!("/proc/{}", result.successor.child_pid)).exists());
    assert!(predecessor.child.try_wait().unwrap().is_some());
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
    fixture
        .supervisor
        .shutdown_successor(&action)
        .await
        .unwrap();
}

#[tokio::test]
async fn controller_executor_drives_the_real_supervisor_and_successor_report() {
    use kuberic_controller::cluster_api::{EffectRecord, InMemoryClusterApi};
    use kuberic_controller::evaluator::EvaluationConfig;
    use kuberic_controller::reconciler::{ReconcileKind, Reconciler};

    let fixture = Fixture::new();
    let predecessor = fixture.spawn_predecessor().await;
    let predecessor_session = predecessor.evidence.process_session.clone();
    let predecessor_process_id = predecessor.evidence.child_pid;
    fixture.seed_fault(&predecessor_session).await;
    let api = Arc::new(InMemoryClusterApi::new(
        fixture.controller_observation(&predecessor_session, predecessor_process_id),
    ));
    api.set_preview_fault_executor(Arc::new(ControllerSupervisorExecutor {
        supervisor: fixture.supervisor.clone(),
        predecessor: tokio::sync::Mutex::new(Some(predecessor)),
        store: fixture.store.clone(),
        binding: fixture.binding.clone(),
        identity: fixture.identity.clone(),
    }))
    .await;
    let reconciler = Reconciler::new(
        api.clone(),
        EvaluationConfig {
            public_operation_preview: Some(wire(fixture.binding.preview.clone())),
            stable_resync_seconds: 10,
            wait_requeue_seconds: 1,
            unsafe_requeue_seconds: 1,
            ..Default::default()
        },
    );
    let mut kinds = Vec::new();
    for _ in 0..10 {
        let kind = reconciler
            .reconcile("tests", "preview-db")
            .await
            .unwrap()
            .kind;
        kinds.push(kind);
        if kind == ReconcileKind::Stable {
            break;
        }
    }
    assert!(kinds.contains(&ReconcileKind::Executed), "{kinds:?}");
    assert_eq!(kinds.last(), Some(&ReconcileKind::Stable));
    let executed = api
        .effects()
        .await
        .into_iter()
        .find_map(|effect| match effect {
            EffectRecord::Execute(
                kuberic_controller::protocol::command::ProtocolCommand::RestartReplicaProcess(
                    command,
                ),
            ) => Some(command.action),
            _ => None,
        })
        .expect("controller dispatched restart");
    let action: PublicFaultAction = wire(&executed);
    let record = fixture.store.restart_action().await.unwrap().unwrap();
    assert_eq!(record.action, action);
    assert_eq!(record.stage, RestartActionStage::SuccessorStarted);
    assert_ne!(
        record.successor_session.as_ref(),
        Some(&predecessor_session)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.data_root.join("child-constructions")).unwrap(),
        "2"
    );
    fixture
        .supervisor
        .shutdown_successor(&action)
        .await
        .unwrap();
}

#[tokio::test]
async fn restart_crash_cuts_resume_exactly_once_and_uncontained_work_stays_closed() {
    let fixture = Fixture::new();
    let action = fixture.accepted_stopped_action().await;
    assert!(
        fixture
            .supervisor
            .resume_after_container_restart(&action, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("does not prove a parent/container restart")
    );
    let supervisor = fixture.restarted_supervisor().await;
    assert!(
        supervisor
            .resume_after_container_restart(&action, Some(PreviewRestartCut::Accepted))
            .await
            .unwrap_err()
            .to_string()
            .contains("after durable restart acceptance")
    );
    assert_eq!(
        fixture.store.restart_action().await.unwrap().unwrap().stage,
        RestartActionStage::Accepted
    );
    assert_eq!(
        std::fs::read_to_string(fixture.data_root.join("child-constructions")).unwrap(),
        "1"
    );

    assert!(
        supervisor
            .resume_after_container_restart(&action, Some(PreviewRestartCut::PredecessorContained),)
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
        supervisor
            .resume_after_container_restart(&action, Some(PreviewRestartCut::SuccessorLaunched))
            .await
            .unwrap_err()
            .to_string()
            .contains("after successor launch")
    );
    assert_eq!(
        fixture.store.restart_action().await.unwrap().unwrap().stage,
        RestartActionStage::SuccessorLaunching
    );
    drop(supervisor);
    let supervisor = fixture.recovery_supervisor(0);
    let completed = supervisor
        .resume_after_container_restart(&action, None)
        .await
        .unwrap();
    let started = fixture.store.restart_action().await.unwrap().unwrap();
    assert_eq!(started.stage, RestartActionStage::SuccessorStarted);
    let session = started.successor_session.clone().unwrap();
    assert_eq!(completed.record.successor_session, Some(session));
    assert_eq!(
        std::fs::read_to_string(fixture.data_root.join("child-constructions")).unwrap(),
        "2"
    );
    supervisor.shutdown_successor(&action).await.unwrap();

    let after_start = Fixture::new();
    let action = after_start.accepted_stopped_action().await;
    let supervisor = after_start.restarted_supervisor().await;
    assert!(
        supervisor
            .resume_after_container_restart(&action, Some(PreviewRestartCut::SuccessorStarted))
            .await
            .unwrap_err()
            .to_string()
            .contains("after successor start")
    );
    let constructions =
        std::fs::read_to_string(after_start.data_root.join("child-constructions")).unwrap();
    supervisor
        .resume_after_container_restart(&action, None)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(after_start.data_root.join("child-constructions")).unwrap(),
        constructions
    );
    supervisor.shutdown_successor(&action).await.unwrap();

    let cancelled = Fixture::new();
    let action = cancelled.accepted_stopped_action().await;
    let supervisor = cancelled.restarted_supervisor_with_delay(200).await;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            supervisor.resume_after_container_restart(&action, None),
        )
        .await
        .is_err()
    );
    drop(supervisor);
    let supervisor = cancelled.recovery_supervisor(200);
    let completed = supervisor
        .resume_after_container_restart(&action, None)
        .await
        .unwrap();
    assert_eq!(completed.record.stage, RestartActionStage::SuccessorStarted);
    assert_eq!(
        std::fs::read_to_string(cancelled.data_root.join("child-constructions")).unwrap(),
        "2"
    );
    supervisor.shutdown_successor(&action).await.unwrap();

    let failed = Fixture::new();
    let action = failed.accepted_stopped_action().await;
    let supervisor = failed.restarted_supervisor_with_exit().await;
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        supervisor.resume_after_container_restart(&action, None),
    )
    .await
    .expect("failed child must not deadlock")
    .unwrap_err();
    assert!(error.to_string().contains("exited before readiness"));

    let unaccepted = Fixture::new();
    let action = unaccepted.stopped_unaccepted_action().await;
    let supervisor = unaccepted.restarted_supervisor().await;
    assert!(
        supervisor
            .resume_after_container_restart(&action, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("cannot admit a new restart action")
    );
    assert!(unaccepted.store.restart_action().await.unwrap().is_none());
}

struct Fixture {
    _directory: tempfile::TempDir,
    data_root: PathBuf,
    release: PathBuf,
    store: Arc<SqliteStore>,
    binding: PreviewLifecycleBinding,
    identity: ReplicaIdentity,
    supervisor: Arc<ReplicaProcessSupervisor>,
}

impl Fixture {
    fn new() -> Self {
        let directory = super::tempdir().unwrap();
        let data_root = directory.path().join("data");
        let release = directory.path().join("release");
        let binding = PreviewLifecycleBinding {
            preview: PublicOperationPreviewIdentity::new(44),
            resource_uid: ResourceUid::new("resource-1"),
            spec_generation: 9,
            state_persistence: StatePersistence::Persisted,
        };
        let initialization_id = crate::protocol::types::derive_initialization_id(
            &binding.resource_uid,
            ReplicaId::new(1),
            &PodUid::new("pod-1"),
            &PvcUid::new("pvc-1"),
        );
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(1),
            instance_id: ReplicaInstanceId::new("pod-1"),
            agent_generation: crate::protocol::types::derive_agent_generation(&initialization_id),
        };
        let storage = StorageIdentity {
            schema_version: PUBLIC_OPERATION_PREVIEW_SCHEMA_VERSION,
            resource_uid: binding.resource_uid.clone(),
            pod_uid: PodUid::new("pod-1"),
            pvc_uid: PvcUid::new("pvc-1"),
            initialization_id,
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
        let supervisor = Arc::new(
            ReplicaProcessSupervisor::new(store.clone(), data_root.clone(), Self::child_command())
                .unwrap(),
        );
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

    fn child_command() -> PreviewChildCommand {
        PreviewChildCommand {
            executable: std::env::current_exe().unwrap(),
            arguments: vec![
                "preview_process_child_entrypoint".into(),
                "--nocapture".into(),
            ],
            environment: BTreeMap::new(),
        }
    }

    async fn restarted_supervisor(&self) -> Arc<ReplicaProcessSupervisor> {
        self.restarted_supervisor_with_delay(0).await
    }

    async fn restarted_supervisor_with_delay(
        &self,
        delay_millis: u64,
    ) -> Arc<ReplicaProcessSupervisor> {
        self.record_dead_parent_marker().await;
        let mut child = Self::child_command();
        if delay_millis > 0 {
            child.environment.insert(
                "KUBERIC_PREVIEW_READY_DELAY_MS".into(),
                delay_millis.to_string(),
            );
        }
        Arc::new(
            ReplicaProcessSupervisor::new(self.store.clone(), self.data_root.clone(), child)
                .unwrap(),
        )
    }

    async fn restarted_supervisor_with_exit(&self) -> Arc<ReplicaProcessSupervisor> {
        self.record_dead_parent_marker().await;
        let mut child = Self::child_command();
        child
            .environment
            .insert("KUBERIC_PREVIEW_EXIT_BEFORE_READY".into(), "1".into());
        Arc::new(
            ReplicaProcessSupervisor::new(self.store.clone(), self.data_root.clone(), child)
                .unwrap(),
        )
    }

    async fn record_dead_parent_marker(&self) {
        let ready = self.data_root.join("parent-marker-ready");
        let release = self.data_root.join("parent-marker-release");
        let _ = std::fs::remove_file(&ready);
        let _ = std::fs::remove_file(&release);
        let mut parent = tokio::process::Command::new(std::env::current_exe().unwrap());
        parent
            .arg("preview_supervisor_parent_entrypoint")
            .arg("--nocapture")
            .env(PARENT_MARKER_ROOT, &self.data_root)
            .env("KUBERIC_PREVIEW_PARENT_READY", &ready)
            .env("KUBERIC_PREVIEW_PARENT_RELEASE", &release)
            .kill_on_drop(true);
        let mut parent = parent.spawn().unwrap();
        for _ in 0..200 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(ready.exists(), "parent marker helper did not start");
        std::fs::write(&release, b"exit").unwrap();
        assert!(parent.wait().await.unwrap().success());
    }

    fn recovery_supervisor(&self, delay_millis: u64) -> Arc<ReplicaProcessSupervisor> {
        let mut child = Self::child_command();
        if delay_millis > 0 {
            child.environment.insert(
                "KUBERIC_PREVIEW_READY_DELAY_MS".into(),
                delay_millis.to_string(),
            );
        }
        Arc::new(
            ReplicaProcessSupervisor::new(self.store.clone(), self.data_root.clone(), child)
                .unwrap(),
        )
    }

    fn controller_observation(
        &self,
        predecessor_session: &ProcessSessionId,
        predecessor_process_id: u32,
    ) -> kuberic_controller::observation::RawObservation {
        use k8s_openapi::api::core::v1::{
            PersistentVolumeClaim, Pod, PodCondition, PodStatus, Service, ServicePort, ServiceSpec,
        };
        use kube::ResourceExt;
        use kuberic_controller::crd::{
            INSTANCE_LABEL, KubericSet, KubericSetSpec, KubericSetStatus, PreviewLifecycleSpec,
            REPLICA_ID_LABEL, SET_UID_LABEL,
        };
        use kuberic_controller::observation::{RawAgentObservation, RawObservation};
        use kuberic_controller::protocol::observation::{AgentReport, ReplicaObservationKey};

        let mut set = KubericSet::new(
            "preview-db",
            KubericSetSpec {
                replicas: 1,
                image: "preview:test".into(),
                failover_delay_seconds: 1,
                switchover: None,
                preview_lifecycle: Some(PreviewLifecycleSpec {
                    state_persistence: wire(StatePersistence::Persisted),
                }),
            },
        );
        set.metadata.namespace = Some("tests".into());
        set.metadata.uid = Some(self.binding.resource_uid.to_string());
        set.metadata.resource_version = Some("1".into());
        set.metadata.generation = Some(self.binding.spec_generation as i64);
        set.status = Some(KubericSetStatus::default());
        let labels = BTreeMap::from([
            (
                SET_UID_LABEL.to_string(),
                self.binding.resource_uid.to_string(),
            ),
            (REPLICA_ID_LABEL.to_string(), "1".into()),
            (INSTANCE_LABEL.to_string(), "pod-1".into()),
        ]);
        let pod = Pod {
            metadata: kube::core::ObjectMeta {
                name: Some("preview-db-1".into()),
                namespace: Some("tests".into()),
                uid: Some("pod-1".into()),
                resource_version: Some("2".into()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            status: Some(PodStatus {
                conditions: Some(vec![PodCondition {
                    type_: "Ready".into(),
                    status: "True".into(),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pvc = PersistentVolumeClaim {
            metadata: kube::core::ObjectMeta {
                name: Some("preview-db-1-data".into()),
                namespace: Some("tests".into()),
                uid: Some("pvc-1".into()),
                resource_version: Some("3".into()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let endpoint_name = crate::protocol::types::derive_replica_endpoint_name(
            &self.binding.resource_uid,
            &self.identity,
        );
        let endpoint = Service {
            metadata: kube::core::ObjectMeta {
                name: Some(endpoint_name),
                namespace: Some("tests".into()),
                uid: Some("endpoint-uid".into()),
                resource_version: Some("4".into()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(BTreeMap::from([(
                    INSTANCE_LABEL.to_string(),
                    "pod-1".into(),
                )])),
                ports: Some(vec![
                    ServicePort {
                        name: Some("control".into()),
                        port: 50051,
                        ..Default::default()
                    },
                    ServicePort {
                        name: Some("replication".into()),
                        port: 50052,
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let write = Service {
            metadata: kube::core::ObjectMeta {
                name: Some("preview-db-write".into()),
                namespace: Some("tests".into()),
                uid: Some("write-uid".into()),
                resource_version: Some("5".into()),
                labels: Some(BTreeMap::from([(
                    SET_UID_LABEL.to_string(),
                    self.binding.resource_uid.to_string(),
                )])),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(BTreeMap::from([(
                    INSTANCE_LABEL.to_string(),
                    "pod-1".into(),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        };
        let report: AgentReport = wire(crate::protocol::observation::AgentReport {
            protocol_version: crate::protocol::PROTOCOL_VERSION,
            resource_uid: self.binding.resource_uid.clone(),
            identity: self.identity.clone(),
            process_session_id: predecessor_session.clone(),
            report_sequence: 1,
            role: ReplicaRole::None,
            read_status: AccessStatus::NotPrimary,
            write_status: AccessStatus::NotPrimary,
            healthy: false,
            epoch: Epoch::default(),
            reported_fault: Some(FaultType::Transient),
            public_lifecycle_report: Some(Box::new(
                crate::protocol::public_operations::PublicLifecycleReport {
                    preview: self.binding.preview.clone(),
                    binding: Some(self.binding.clone()),
                    resource_uid: self.binding.resource_uid.clone(),
                    replica: self.identity.clone(),
                    process_session_id: predecessor_session.clone(),
                    process_id: predecessor_process_id,
                    revision: 3,
                    operation_id: Some(OperationId::new("fault-operation")),
                    role: ReplicaRole::None,
                    write_access: false,
                    service_location: None,
                },
            )),
            ..Default::default()
        });
        let key = ReplicaObservationKey::new(
            wire(self.identity.replica_id),
            wire(self.identity.instance_id.clone()),
        );
        assert_eq!(pod.uid().as_deref(), Some("pod-1"));
        RawObservation {
            set,
            pods: vec![pod],
            pvcs: vec![pvc],
            services: vec![endpoint, write],
            secrets: Vec::new(),
            agents: BTreeMap::from([(key, RawAgentObservation::PreviewReport(Box::new(report)))]),
            exact_resources: Vec::new(),
            failures: Vec::new(),
            now_unix_seconds: 100,
        }
    }

    async fn spawn_predecessor(&self) -> PreviewChildProcess {
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
                return PreviewChildProcess {
                    child,
                    evidence,
                    restart_signal: self.release.clone(),
                };
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("predecessor child did not report");
    }

    async fn accepted_stopped_action(&self) -> PublicFaultAction {
        let mut predecessor = self.spawn_predecessor().await;
        let action = self
            .action(
                predecessor.evidence.process_session.clone(),
                predecessor.evidence.child_pid,
            )
            .await;
        self.store.begin_restart_action(&action).await.unwrap();
        std::fs::write(&self.release, b"restart").unwrap();
        let status = predecessor.child.wait().await.unwrap();
        assert_eq!(status.code(), Some(PREVIEW_RESTART_DISPOSITION));
        action
    }

    async fn stopped_unaccepted_action(&self) -> PublicFaultAction {
        let mut predecessor = self.spawn_predecessor().await;
        let action = self
            .action(
                predecessor.evidence.process_session.clone(),
                predecessor.evidence.child_pid,
            )
            .await;
        std::fs::write(&self.release, b"restart").unwrap();
        let status = predecessor.child.wait().await.unwrap();
        assert_eq!(status.code(), Some(PREVIEW_RESTART_DISPOSITION));
        action
    }

    async fn seed_fault(&self, predecessor_session: &ProcessSessionId) {
        self.store
            .begin_public_operation(
                &crate::protocol::public_operations::PublicOperationIntent {
                    preview: self.binding.preview.clone(),
                    operation_id: OperationId::new("fault-operation"),
                    revision: 3,
                    process_session_id: predecessor_session.clone(),
                    class: crate::protocol::public_operations::PublicOperationClass::TransientFault,
                    input_digest: "fault-operation-digest".into(),
                    lifecycle: None,
                    program: None,
                },
                &[],
                &[],
            )
            .await
            .unwrap();
    }

    async fn action(
        &self,
        predecessor_session: ProcessSessionId,
        predecessor_process_id: u32,
    ) -> PublicFaultAction {
        self.seed_fault(&predecessor_session).await;
        let mut action = PublicFaultAction {
            action_id: OperationId::new("pending"),
            fault_operation_id: OperationId::new("fault-operation"),
            binding: self.binding.clone(),
            target: self.identity.clone(),
            resources: FrozenReplicaResources {
                pod_name: "replica-1".into(),
                pod_uid: PodUid::new("pod-1"),
                pvc_name: "replica-1-data".into(),
                pvc_uid: PvcUid::new("pvc-1"),
                endpoint_name: "replica-1-endpoint".into(),
                endpoint_uid: "endpoint-uid".into(),
                endpoint_resource_version: "1".into(),
            },
            predecessor_session,
            predecessor_process_id,
            fault_revision: 3,
            fault: FaultType::Transient,
            kind: PublicFaultActionKind::Restart,
        };
        action.action_id = action.expected_id();
        action
    }
}
