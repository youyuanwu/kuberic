#![cfg(feature = "runtime-test-bridge")]

use std::collections::BTreeMap;
use std::sync::Arc;

use k8s_openapi::api::core::v1::{
    PersistentVolumeClaim, Pod, PodCondition, PodStatus, Secret, Service, ServiceSpec,
};
use kube::ResourceExt;
use kuberic_controller::cluster_api::{ClusterApi, EffectRecord, InMemoryClusterApi};
use kuberic_controller::crd::{
    INSTANCE_LABEL, KubericSet, KubericSetSpec, KubericSetStatus, PreviewLifecycleSpec,
    REPLICA_ID_LABEL, SET_UID_LABEL,
};
use kuberic_controller::evaluator::EvaluationConfig;
use kuberic_controller::observation::{RawAgentObservation, RawObservation, RawObservationFailure};
use kuberic_controller::reconciler::{ReconcileKind, Reconciler};
use kuberic_runtime::protocol::command::{KubernetesChange, ProtocolCommand};
use kuberic_runtime::protocol::observation::{AgentReport, ReplicaObservationKey};
use kuberic_runtime::protocol::public_operations::{
    PreviewLifecycleBinding, PublicLifecycleReport, PublicOperationPreviewIdentity,
    RestartActionRecord, RestartActionStage, StatePersistence,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, Epoch, FaultType, OperationId, PodUid, ProcessSessionId, PvcUid, ReplicaId,
    ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ResourceUid, derive_agent_generation,
    derive_initialization_id, derive_replica_endpoint_name,
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
    let replica_id = ReplicaId::new(replica_id);
    let initialization = derive_initialization_id(
        &ResourceUid::new(RESOURCE_UID),
        replica_id,
        &PodUid::new(POD_UID),
        &PvcUid::new(PVC_UID),
    );
    ReplicaIdentity {
        replica_id,
        instance_id: ReplicaInstanceId::new(POD_UID),
        agent_generation: derive_agent_generation(&initialization),
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
            binding: persistence.map(|state_persistence| PreviewLifecycleBinding {
                preview: preview(),
                resource_uid: ResourceUid::new(RESOURCE_UID),
                spec_generation: 7,
                state_persistence,
            }),
            resource_uid: ResourceUid::new(RESOURCE_UID),
            replica: target.clone(),
            process_session_id: ProcessSessionId::new(PREDECESSOR_SESSION),
            process_id: std::process::id(),
            revision: 3,
            operation_id: Some(OperationId::new("fault-operation")),
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
            annotations: Some(BTreeMap::from([(
                "operator.kuberic.io/preview-service-location".into(),
                "opaque://published-before-fault".into(),
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
        for _ in 0..2 {
            assert_eq!(
                reconciler
                    .reconcile("tests", "fault-db")
                    .await
                    .unwrap()
                    .kind,
                ReconcileKind::Applied
            );
        }
        assert!(
            api.observation()
                .await
                .services
                .iter()
                .find(|service| service.name_any().ends_with("-write"))
                .unwrap()
                .annotations()
                .get("operator.kuberic.io/preview-service-location")
                .is_none()
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
            successor_process_id: Some(std::process::id()),
            launch_nonce: Some("controller-test-launch".into()),
        }));
        report
            .public_lifecycle_report
            .as_mut()
            .unwrap()
            .process_session_id = successor;
        report.public_lifecycle_report.as_mut().unwrap().process_id = std::process::id();
        let mut invalid_successor = observation.clone();
        invalid_successor.pvcs[0].metadata.uid = Some("changed-pvc".into());
        let RawAgentObservation::PreviewReport(invalid_report) =
            invalid_successor.agents.values_mut().next().unwrap()
        else {
            panic!("expected preview report");
        };
        invalid_report.role = ReplicaRole::Primary;
        invalid_report.read_status = AccessStatus::Granted;
        invalid_report.write_status = AccessStatus::Granted;
        api.set_observation(invalid_successor).await;
        assert_eq!(
            restarted_controller
                .reconcile("tests", "fault-db")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Unsafe
        );
        api.set_observation(observation).await;
        assert_eq!(
            restarted_controller
                .reconcile("tests", "fault-db")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );
        assert_eq!(
            restarted_controller
                .reconcile("tests", "fault-db")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Stable
        );
        let historical = api
            .observation()
            .await
            .set
            .status
            .unwrap()
            .authority
            .last_public_fault_action
            .unwrap();
        let mut next_fault = api.observation().await;
        let RawAgentObservation::PreviewReport(report) =
            next_fault.agents.values_mut().next().unwrap()
        else {
            panic!("expected preview report");
        };
        report.reported_fault = Some(FaultType::Transient);
        report.healthy = false;
        report.report_sequence += 1;
        let lifecycle = report.public_lifecycle_report.as_mut().unwrap();
        lifecycle.revision += 1;
        lifecycle.operation_id = Some(OperationId::new(format!("fault-next-{replica_id}")));
        api.set_observation(next_fault).await;
        assert_eq!(
            restarted_controller
                .reconcile("tests", "fault-db")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );
        let next = api
            .observation()
            .await
            .set
            .status
            .unwrap()
            .authority
            .public_fault_action
            .unwrap();
        assert_ne!(next.action_id, historical.action_id);
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
        let mut last = ReconcileKind::Waiting;
        for _ in 0..15 {
            last = reconciler
                .reconcile("tests", "fault-db")
                .await
                .unwrap()
                .kind;
        }
        assert_eq!(last, ReconcileKind::Stable);

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
        let deleted = effects
            .iter()
            .position(|effect| matches!(effect, EffectRecord::DeleteScaffolding { .. }))
            .unwrap();
        let replaced = effects
            .iter()
            .position(|effect| matches!(effect, EffectRecord::EnsureReplacement(_)))
            .unwrap();
        assert!(deleted < replaced);
        let action = api
            .observation()
            .await
            .set
            .status
            .unwrap()
            .authority
            .last_public_fault_action
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
    for _ in 0..3 {
        assert_eq!(
            reconciler
                .reconcile("tests", "fault-db")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );
    }

    let original = api.observation().await;
    let stale = ProtocolCommand::RestartReplicaProcess(Box::new(
        kuberic_runtime::protocol::command::RestartReplicaProcess {
            action: transient.clone(),
        },
    ));
    let mut changed_pvc = original.clone();
    changed_pvc.pvcs[0].metadata.uid = Some("changed-before-dispatch".into());
    api.set_observation(changed_pvc).await;
    assert!(matches!(
        api.execute_command(&api.observation().await, &stale).await,
        Err(kuberic_controller::ControllerError::ObservationStale)
    ));

    let mut changed_revision = original.clone();
    let RawAgentObservation::PreviewReport(report) =
        changed_revision.agents.values_mut().next().unwrap()
    else {
        panic!("expected preview report");
    };
    report.public_lifecycle_report.as_mut().unwrap().revision += 1;
    api.set_observation(changed_revision).await;
    assert!(matches!(
        api.execute_command(&api.observation().await, &stale).await,
        Err(kuberic_controller::ControllerError::ObservationStale)
    ));

    let mut rerouted = original.clone();
    let service = rerouted
        .services
        .iter_mut()
        .find(|service| service.name_any().ends_with("-write"))
        .unwrap();
    service.spec.get_or_insert_default().selector =
        Some(BTreeMap::from([(INSTANCE_LABEL.into(), POD_UID.into())]));
    service.metadata.annotations.get_or_insert_default().insert(
        "operator.kuberic.io/preview-service-location".into(),
        "opaque://resurrected".into(),
    );
    api.set_observation(rerouted).await;
    assert!(matches!(
        api.execute_command(&api.observation().await, &stale).await,
        Err(kuberic_controller::ControllerError::ObservationStale)
    ));

    let mut observation = original;
    let RawAgentObservation::PreviewReport(report) =
        observation.agents.values_mut().next().unwrap()
    else {
        panic!("expected preview report");
    };
    report.reported_fault = Some(FaultType::Permanent);
    report.report_sequence += 1;
    api.set_observation(observation).await;
    assert!(matches!(
        api.execute_command(&api.observation().await, &stale).await,
        Err(kuberic_controller::ControllerError::ObservationStale)
    ));
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

    let unbound = Arc::new(InMemoryClusterApi::new(raw_fault(
        Some(StatePersistence::Persisted),
        FaultType::Transient,
        1,
    )));
    let mut unbound_observation = unbound.observation().await;
    let RawAgentObservation::PreviewReport(report) =
        unbound_observation.agents.values_mut().next().unwrap()
    else {
        panic!("expected preview report");
    };
    report.public_lifecycle_report.as_mut().unwrap().binding = None;
    unbound.set_observation(unbound_observation).await;
    let unbound_reconciler = Reconciler::new(unbound.clone(), config());
    unbound_reconciler
        .reconcile("tests", "fault-db")
        .await
        .unwrap();
    assert_eq!(
        unbound_reconciler
            .reconcile("tests", "fault-db")
            .await
            .unwrap()
            .kind,
        ReconcileKind::Unsafe
    );

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

#[tokio::test]
async fn volatile_cleanup_never_deletes_a_recreated_endpoint() {
    let api = Arc::new(InMemoryClusterApi::new(raw_fault(
        Some(StatePersistence::Volatile),
        FaultType::Transient,
        1,
    )));
    let reconciler = Reconciler::new(api.clone(), config());
    for _ in 0..5 {
        reconciler.reconcile("tests", "fault-db").await.unwrap();
    }

    let mut observation = api.observation().await;
    let endpoint = observation
        .services
        .iter_mut()
        .find(|service| !service.name_any().ends_with("-write"))
        .unwrap();
    endpoint.metadata.uid = Some("replacement-endpoint-uid".into());
    endpoint.metadata.resource_version = Some("replacement-version".into());
    api.set_observation(observation).await;
    assert_eq!(
        reconciler
            .reconcile("tests", "fault-db")
            .await
            .unwrap()
            .kind,
        ReconcileKind::ObservationStale
    );
    assert!(
        api.observation()
            .await
            .services
            .iter()
            .any(|service| service.uid().as_deref() == Some("replacement-endpoint-uid"))
    );
}

#[tokio::test]
async fn volatile_cleanup_never_deletes_recreated_pod_or_pvc_names() {
    let api = Arc::new(InMemoryClusterApi::new(raw_fault(
        Some(StatePersistence::Volatile),
        FaultType::Transient,
        1,
    )));
    let reconciler = Reconciler::new(api.clone(), config());
    for _ in 0..5 {
        reconciler.reconcile("tests", "fault-db").await.unwrap();
    }
    let mut observation = api.observation().await;
    observation.pods[0].metadata.uid = Some("replacement-pod-uid".into());
    observation.pvcs[0].metadata.uid = Some("replacement-pvc-uid".into());
    api.set_observation(observation).await;
    assert_eq!(
        reconciler
            .reconcile("tests", "fault-db")
            .await
            .unwrap()
            .kind,
        ReconcileKind::ObservationStale
    );
    let observation = api.observation().await;
    assert!(
        observation
            .pods
            .iter()
            .any(|pod| pod.uid().as_deref() == Some("replacement-pod-uid"))
    );
    assert!(
        observation
            .pvcs
            .iter()
            .any(|pvc| pvc.uid().as_deref() == Some("replacement-pvc-uid"))
    );
}

#[tokio::test]
async fn accepted_drop_rejects_invalid_normalized_predecessor_evidence() {
    let api = Arc::new(InMemoryClusterApi::new(raw_fault(
        Some(StatePersistence::Volatile),
        FaultType::Transient,
        1,
    )));
    let reconciler = Reconciler::new(api.clone(), config());
    for _ in 0..3 {
        reconciler.reconcile("tests", "fault-db").await.unwrap();
    }
    let effects_before = api.effects().await.len();
    let mut observation = api.observation().await;
    let RawAgentObservation::PreviewReport(report) =
        observation.agents.values_mut().next().unwrap()
    else {
        panic!("expected preview report");
    };
    report.resource_uid = ResourceUid::new("mismatched-resource");
    api.set_observation(observation).await;
    assert_eq!(
        reconciler
            .reconcile("tests", "fault-db")
            .await
            .unwrap()
            .kind,
        ReconcileKind::Unsafe
    );
    assert!(!api.effects().await[effects_before..].iter().any(|effect| {
        matches!(
            effect,
            EffectRecord::Execute(_)
                | EffectRecord::DeleteExactService { .. }
                | EffectRecord::DeleteScaffolding { .. }
                | EffectRecord::EnsureReplacement(_)
        )
    }));
}

#[tokio::test]
async fn accepted_drop_waits_for_failed_resource_observation() {
    let api = Arc::new(InMemoryClusterApi::new(raw_fault(
        Some(StatePersistence::Volatile),
        FaultType::Transient,
        1,
    )));
    let reconciler = Reconciler::new(api.clone(), config());
    for _ in 0..5 {
        reconciler.reconcile("tests", "fault-db").await.unwrap();
    }
    let effects_before = api.effects().await.len();
    let mut observation = api.observation().await;
    observation.failures.push(RawObservationFailure {
        source: "services".into(),
        message: "injected observation failure".into(),
    });
    api.set_observation(observation).await;
    assert_eq!(
        reconciler
            .reconcile("tests", "fault-db")
            .await
            .unwrap()
            .kind,
        ReconcileKind::Waiting
    );
    assert!(!api.effects().await[effects_before..].iter().any(|effect| {
        matches!(
            effect,
            EffectRecord::DeleteExactService { .. }
                | EffectRecord::DeleteScaffolding { .. }
                | EffectRecord::EnsureReplacement(_)
        )
    }));
}

#[tokio::test]
async fn accepted_drop_never_targets_a_successor_on_the_same_storage() {
    let api = Arc::new(InMemoryClusterApi::new(raw_fault(
        Some(StatePersistence::Persisted),
        FaultType::Permanent,
        1,
    )));
    let reconciler = Reconciler::new(api.clone(), config());
    for _ in 0..5 {
        reconciler.reconcile("tests", "fault-db").await.unwrap();
    }
    let effects_before = api.effects().await.len();
    let mut observation = api.observation().await;
    let RawAgentObservation::PreviewReport(report) =
        observation.agents.values_mut().next().unwrap()
    else {
        panic!("expected preview report");
    };
    report.process_session_id = ProcessSessionId::new("successor-session");
    report.report_sequence += 1;
    report.reported_fault = None;
    report.healthy = true;
    let lifecycle = report.public_lifecycle_report.as_mut().unwrap();
    lifecycle.process_session_id = ProcessSessionId::new("successor-session");
    lifecycle.process_id = std::process::id().saturating_add(1);
    api.set_observation(observation).await;
    assert_eq!(
        reconciler
            .reconcile("tests", "fault-db")
            .await
            .unwrap()
            .kind,
        ReconcileKind::Unsafe
    );
    assert!(!api.effects().await[effects_before..].iter().any(|effect| {
        matches!(
            effect,
            EffectRecord::DeleteExactService { .. }
                | EffectRecord::DeleteScaffolding { .. }
                | EffectRecord::EnsureReplacement(_)
        )
    }));
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
