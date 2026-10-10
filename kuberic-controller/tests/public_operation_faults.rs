#![cfg(feature = "runtime-test-bridge")]

use std::collections::BTreeMap;
use std::sync::Arc;

use k8s_openapi::api::core::v1::{
    PersistentVolumeClaim, Pod, PodCondition, PodStatus, Secret, Service, ServiceSpec,
};
use kuberic_controller::cluster_api::{EffectRecord, InMemoryClusterApi};
use kuberic_controller::crd::{
    INSTANCE_LABEL, KubericSet, KubericSetSpec, KubericSetStatus, PreviewLifecycleSpec,
    REPLICA_ID_LABEL, SET_UID_LABEL,
};
use kuberic_controller::evaluator::EvaluationConfig;
use kuberic_controller::observation::{RawAgentObservation, RawObservation};
use kuberic_controller::reconciler::{ReconcileKind, Reconciler};
use kuberic_runtime::protocol::command::{KubernetesChange, ProtocolCommand};
use kuberic_runtime::protocol::observation::{AgentReport, ReplicaObservationKey};
use kuberic_runtime::protocol::public_operations::{
    PublicLifecycleReport, PublicOperationPreviewIdentity, RestartActionRecord, RestartActionStage,
    StatePersistence,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, AgentGeneration, Epoch, FaultType, PodUid, ProcessSessionId, PvcUid, ReplicaId,
    ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ResourceUid, derive_replica_endpoint_name,
};

const RESOURCE_UID: &str = "set-fault-uid";
const POD_UID: &str = "fault-pod-uid";
const PVC_UID: &str = "fault-pvc-uid";
const PREDECESSOR_SESSION: &str = "fault-session-1";

fn preview() -> PublicOperationPreviewIdentity {
    PublicOperationPreviewIdentity::new(44)
}

fn config() -> EvaluationConfig {
    EvaluationConfig {
        public_operation_preview: Some(preview()),
        stable_resync_seconds: 11,
        wait_requeue_seconds: 1,
        unsafe_requeue_seconds: 2,
        ..Default::default()
    }
}

fn identity(replica_id: i64) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(replica_id),
        instance_id: ReplicaInstanceId::new(POD_UID),
        agent_generation: AgentGeneration::new("fault-generation"),
    }
}

fn labels(replica_id: ReplicaId) -> BTreeMap<String, String> {
    BTreeMap::from([
        (SET_UID_LABEL.into(), RESOURCE_UID.into()),
        (REPLICA_ID_LABEL.into(), replica_id.to_string()),
        (INSTANCE_LABEL.into(), POD_UID.into()),
    ])
}

fn raw_fault(
    persistence: Option<StatePersistence>,
    fault: FaultType,
    replica_id: i64,
) -> RawObservation {
    let target = identity(replica_id);
    let mut set = KubericSet::new(
        "fault-db",
        KubericSetSpec {
            replicas: replica_id as u32,
            image: "example/fault:latest".into(),
            failover_delay_seconds: 1,
            switchover: None,
            preview_lifecycle: persistence
                .map(|state_persistence| PreviewLifecycleSpec { state_persistence }),
        },
    );
    set.metadata.namespace = Some("tests".into());
    set.metadata.uid = Some(RESOURCE_UID.into());
    set.metadata.resource_version = Some("1".into());
    set.metadata.generation = Some(7);
    set.status = Some(KubericSetStatus::default());

    let pod = Pod {
        metadata: kube::core::ObjectMeta {
            name: Some(format!("fault-db-{replica_id}")),
            namespace: Some("tests".into()),
            uid: Some(POD_UID.into()),
            resource_version: Some("2".into()),
            labels: Some(labels(target.replica_id)),
            ..Default::default()
        },
        status: Some(PodStatus {
            conditions: Some(vec![PodCondition {
                last_probe_time: None,
                last_transition_time: None,
                message: None,
                reason: None,
                status: "True".into(),
                type_: "Ready".into(),
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let pvc = PersistentVolumeClaim {
        metadata: kube::core::ObjectMeta {
            name: Some(format!("fault-db-{replica_id}-data")),
            namespace: Some("tests".into()),
            uid: Some(PVC_UID.into()),
            resource_version: Some("3".into()),
            labels: Some(labels(target.replica_id)),
            ..Default::default()
        },
        ..Default::default()
    };
    let report = AgentReport {
        protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
        resource_uid: ResourceUid::new(RESOURCE_UID),
        identity: target.clone(),
        process_session_id: ProcessSessionId::new(PREDECESSOR_SESSION),
        report_sequence: 3,
        role: ReplicaRole::None,
        read_status: AccessStatus::NotPrimary,
        write_status: AccessStatus::NotPrimary,
        healthy: false,
        epoch: Epoch::new(3, 9),
        reported_fault: Some(fault),
        public_lifecycle_report: Some(Box::new(PublicLifecycleReport {
            preview: preview(),
            resource_uid: ResourceUid::new(RESOURCE_UID),
            replica: target.clone(),
            process_session_id: ProcessSessionId::new(PREDECESSOR_SESSION),
            revision: 3,
            operation_id: None,
            role: ReplicaRole::None,
            write_access: false,
            service_location: None,
        })),
        ..Default::default()
    };
    let write_service = Service {
        metadata: kube::core::ObjectMeta {
            name: Some("fault-db-write".into()),
            namespace: Some("tests".into()),
            uid: Some("write-service-uid".into()),
            resource_version: Some("4".into()),
            labels: Some(BTreeMap::from([(
                SET_UID_LABEL.into(),
                RESOURCE_UID.into(),
            )])),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(BTreeMap::from([(INSTANCE_LABEL.into(), POD_UID.into())])),
            ..Default::default()
        }),
        ..Default::default()
    };
    let endpoint = Service {
        metadata: kube::core::ObjectMeta {
            name: Some(derive_replica_endpoint_name(
                &ResourceUid::new(RESOURCE_UID),
                &target,
            )),
            namespace: Some("tests".into()),
            uid: Some("endpoint-uid".into()),
            resource_version: Some("5".into()),
            labels: Some(BTreeMap::from([(
                SET_UID_LABEL.into(),
                RESOURCE_UID.into(),
            )])),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(BTreeMap::from([(INSTANCE_LABEL.into(), POD_UID.into())])),
            ..Default::default()
        }),
        ..Default::default()
    };
    RawObservation {
        set,
        pods: vec![pod],
        pvcs: vec![pvc],
        services: vec![write_service, endpoint],
        secrets: Vec::<Secret>::new(),
        agents: BTreeMap::from([(
            ReplicaObservationKey::new(target.replica_id, target.instance_id.clone()),
            RawAgentObservation::PreviewReport(Box::new(report)),
        )]),
        exact_resources: Vec::new(),
        failures: Vec::new(),
        now_unix_seconds: 100,
    }
}

#[tokio::test]
async fn persisted_primary_and_secondary_faults_use_one_restart_action_across_redelivery() {
    for replica_id in [1, 2] {
        let api = Arc::new(InMemoryClusterApi::new(raw_fault(
            Some(StatePersistence::Persisted),
            FaultType::Transient,
            replica_id,
        )));
        let reconciler = Reconciler::new(api.clone(), config());
        assert_eq!(
            reconciler
                .reconcile("tests", "fault-db")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );
        assert_eq!(
            reconciler
                .reconcile("tests", "fault-db")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );
        let accepted = api
            .observation()
            .await
            .set
            .status
            .unwrap()
            .authority
            .public_fault_action
            .unwrap();
        assert_eq!(accepted.target.replica_id, ReplicaId::new(replica_id));
        assert_eq!(
            accepted.kind,
            kuberic_runtime::protocol::public_operations::PublicFaultActionKind::Restart
        );
        assert!(
            kuberic_runtime::protocol::validation::validate_public_fault_action(
                &accepted,
                &accepted.binding,
                &accepted.target,
                &ProcessSessionId::new("stale-session"),
                FaultType::Transient,
            )
            .is_err()
        );
        assert_eq!(
            reconciler
                .reconcile("tests", "fault-db")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );

        api.unavailable_next_execute().await;
        assert_eq!(
            reconciler
                .reconcile("tests", "fault-db")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Waiting
        );
        let restarted_controller = Reconciler::new(api.clone(), config());
        assert_eq!(
            restarted_controller
                .reconcile("tests", "fault-db")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Executed
        );
        let executed = api
            .effects()
            .await
            .into_iter()
            .filter_map(|effect| match effect {
                EffectRecord::Execute(ProtocolCommand::RestartReplicaProcess(command)) => {
                    Some(command.action.action_id)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            executed,
            vec![accepted.action_id.clone(), accepted.action_id.clone()]
        );

        let mut observation = api.observation().await;
        let RawAgentObservation::PreviewReport(report) = observation
            .agents
            .values_mut()
            .next()
            .expect("preview report")
        else {
            panic!("expected preview report");
        };
        let successor = ProcessSessionId::new(format!("successor-{replica_id}"));
        report.process_session_id = successor.clone();
        report.report_sequence += 1;
        report.reported_fault = None;
        report.healthy = true;
        report.restart_action = Some(Box::new(RestartActionRecord {
            action: accepted.clone(),
            stage: RestartActionStage::SuccessorStarted,
            successor_session: Some(successor.clone()),
        }));
        report
            .public_lifecycle_report
            .as_mut()
            .unwrap()
            .process_session_id = successor;
        api.set_observation(observation).await;
        assert_eq!(
            restarted_controller
                .reconcile("tests", "fault-db")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Stable
        );
    }
}

#[tokio::test]
async fn volatile_and_permanent_faults_freeze_exact_drop_and_replacement() {
    for (persistence, fault) in [
        (StatePersistence::Volatile, FaultType::Transient),
        (StatePersistence::Persisted, FaultType::Permanent),
    ] {
        let api = Arc::new(InMemoryClusterApi::new(raw_fault(
            Some(persistence),
            fault,
            1,
        )));
        let reconciler = Reconciler::new(api.clone(), config());
        for _ in 0..4 {
            reconciler.reconcile("tests", "fault-db").await.unwrap();
        }

        let effects = api.effects().await;
        assert!(effects.iter().any(|effect| {
            matches!(effect, EffectRecord::DeleteScaffolding {
                pod_name: Some(name),
                pvc_name: Some(pvc),
            } if name == "fault-db-1" && pvc == "fault-db-1-data")
        }));
        assert!(effects.iter().any(|effect| {
            matches!(effect, EffectRecord::EnsureReplacement(replacing)
                if replacing == &identity(1))
        }));
        let action = api
            .observation()
            .await
            .set
            .status
            .unwrap()
            .authority
            .public_fault_action
            .unwrap();
        assert_eq!(
            action.kind,
            kuberic_runtime::protocol::public_operations::PublicFaultActionKind::DropReplacement
        );
        assert_eq!(action.resources.pod_uid, PodUid::new(POD_UID));
        assert_eq!(action.resources.pvc_uid, PvcUid::new(PVC_UID));
    }
}

#[tokio::test]
async fn permanent_fault_supersedes_transient_action_without_retargeting() {
    let api = Arc::new(InMemoryClusterApi::new(raw_fault(
        Some(StatePersistence::Persisted),
        FaultType::Transient,
        1,
    )));
    let reconciler = Reconciler::new(api.clone(), config());
    reconciler.reconcile("tests", "fault-db").await.unwrap();
    reconciler.reconcile("tests", "fault-db").await.unwrap();
    let transient = api
        .observation()
        .await
        .set
        .status
        .unwrap()
        .authority
        .public_fault_action
        .unwrap();

    let mut observation = api.observation().await;
    let RawAgentObservation::PreviewReport(report) =
        observation.agents.values_mut().next().unwrap()
    else {
        panic!("expected preview report");
    };
    report.reported_fault = Some(FaultType::Permanent);
    report.report_sequence += 1;
    api.set_observation(observation).await;
    assert_eq!(
        reconciler
            .reconcile("tests", "fault-db")
            .await
            .unwrap()
            .kind,
        ReconcileKind::Applied
    );
    let permanent = api
        .observation()
        .await
        .set
        .status
        .unwrap()
        .authority
        .public_fault_action
        .unwrap();
    assert_eq!(permanent.target, transient.target);
    assert_eq!(permanent.predecessor_session, transient.predecessor_session);
    assert_ne!(permanent.action_id, transient.action_id);
    assert_eq!(permanent.fault, FaultType::Permanent);
    assert_eq!(
        permanent.kind,
        kuberic_runtime::protocol::public_operations::PublicFaultActionKind::DropReplacement
    );
}

#[tokio::test]
async fn missing_mutated_and_production_preview_selection_fail_closed() {
    let missing = Arc::new(InMemoryClusterApi::new(raw_fault(
        None,
        FaultType::Transient,
        1,
    )));
    let reconciler = Reconciler::new(missing.clone(), config());
    assert_eq!(
        reconciler
            .reconcile("tests", "fault-db")
            .await
            .unwrap()
            .kind,
        ReconcileKind::Unsafe
    );
    assert!(!missing.effects().await.iter().any(|effect| matches!(
        effect,
        EffectRecord::Execute(_)
            | EffectRecord::DeleteScaffolding { .. }
            | EffectRecord::EnsureReplacement(_)
    )));

    let api = Arc::new(InMemoryClusterApi::new(raw_fault(
        Some(StatePersistence::Persisted),
        FaultType::Transient,
        1,
    )));
    let preview_reconciler = Reconciler::new(api.clone(), config());
    preview_reconciler
        .reconcile("tests", "fault-db")
        .await
        .unwrap();
    let mut changed = api.observation().await;
    changed.set.spec.preview_lifecycle = Some(PreviewLifecycleSpec {
        state_persistence: StatePersistence::Volatile,
    });
    api.set_observation(changed).await;
    assert_eq!(
        preview_reconciler
            .reconcile("tests", "fault-db")
            .await
            .unwrap()
            .kind,
        ReconcileKind::Unsafe
    );

    let production = Arc::new(InMemoryClusterApi::new(raw_fault(
        Some(StatePersistence::Persisted),
        FaultType::Transient,
        1,
    )));
    let production_reconciler = Reconciler::new(
        production.clone(),
        kuberic_controller::production_evaluation_config(11, 1, 2),
    );
    assert_eq!(
        production_reconciler
            .reconcile("tests", "fault-db")
            .await
            .unwrap()
            .kind,
        ReconcileKind::Unsafe
    );
    assert!(!production.effects().await.iter().any(|effect| matches!(
        effect,
        EffectRecord::Execute(_)
            | EffectRecord::DeleteScaffolding { .. }
            | EffectRecord::EnsureReplacement(_)
            | EffectRecord::DeleteExactPod { .. }
    )));
}

#[test]
fn preview_fault_changes_are_destructive_only_after_persisted_action() {
    let raw = raw_fault(Some(StatePersistence::Volatile), FaultType::Transient, 1);
    let snapshot = kuberic_controller::normalize::normalize(raw, BTreeMap::new()).unwrap();
    let plan = kuberic_controller::evaluate(&snapshot, &config());
    assert!(matches!(
        plan,
        kuberic_controller::Plan::Apply { changes }
            if matches!(
                changes.as_slice(),
                [KubernetesChange::PersistStatus { .. }]
            )
    ));
}
