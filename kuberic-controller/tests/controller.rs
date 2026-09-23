use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::{
    PersistentVolumeClaim, Pod, PodCondition, PodStatus, Service, ServiceSpec,
};
use kuberic_controller::cluster_api::{EffectRecord, InMemoryClusterApi};
use kuberic_controller::crd::{
    INSTANCE_LABEL, KubericSet, KubericSetSpec, KubericSetStatus, REPLICA_ID_LABEL, SET_UID_LABEL,
};
use kuberic_controller::normalize::normalize;
use kuberic_controller::observation::{RawAgentObservation, RawObservation};
use kuberic_controller::reconciler::{ReconcileKind, Reconciler};
use kuberic_protocol::evaluator::{EvaluationConfig, evaluate};
use kuberic_protocol::observation::AgentObservation;
use kuberic_protocol::plan::Plan;
use kuberic_protocol::types::{AcceptedStatus, AcceptedTopology, ReplicaId, ReplicaRole};
use kuberic_wire::proto;

const UID: &str = "set-uid";
const POD_UID: &str = "pod-uid-1";
const PVC_UID: &str = "pvc-uid-1";

fn config() -> EvaluationConfig {
    EvaluationConfig {
        supported_protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        stable_resync_seconds: 11,
        wait_requeue_seconds: 3,
        unsafe_requeue_seconds: 7,
    }
}

fn raw(replicas: u32) -> RawObservation {
    let mut set = KubericSet::new(
        "db",
        KubericSetSpec {
            replicas,
            image: "example/db:latest".to_string(),
            failover_delay_seconds: 9,
        },
    );
    set.metadata.namespace = Some("tests".to_string());
    set.metadata.uid = Some(UID.to_string());
    set.metadata.resource_version = Some("1".to_string());
    set.metadata.generation = Some(1);
    RawObservation {
        set,
        pods: Vec::new(),
        pvcs: Vec::new(),
        services: Vec::new(),
        agents: BTreeMap::new(),
        failures: Vec::new(),
        now_unix_seconds: 100,
    }
}

fn labels(replica_id: ReplicaId) -> BTreeMap<String, String> {
    BTreeMap::from([
        (SET_UID_LABEL.to_string(), UID.to_string()),
        (REPLICA_ID_LABEL.to_string(), replica_id.to_string()),
    ])
}

fn with_scaffolding(mut raw: RawObservation) -> RawObservation {
    raw.pods.push(Pod {
        metadata: kube::core::ObjectMeta {
            name: Some("db-1".to_string()),
            namespace: Some("tests".to_string()),
            uid: Some(POD_UID.to_string()),
            resource_version: Some("2".to_string()),
            labels: Some(labels(ReplicaId::new(1))),
            ..Default::default()
        },
        status: Some(PodStatus {
            conditions: Some(vec![PodCondition {
                last_probe_time: None,
                last_transition_time: None,
                message: None,
                reason: None,
                status: "True".to_string(),
                type_: "Ready".to_string(),
            }]),
            pod_ip: Some("127.0.0.1".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    raw.pvcs.push(PersistentVolumeClaim {
        metadata: kube::core::ObjectMeta {
            name: Some("db-1-data".to_string()),
            namespace: Some("tests".to_string()),
            uid: Some(PVC_UID.to_string()),
            resource_version: Some("3".to_string()),
            labels: Some(labels(ReplicaId::new(1))),
            ..Default::default()
        },
        ..Default::default()
    });
    raw
}

fn bootstrap_status(raw: &RawObservation) -> AcceptedStatus {
    let snapshot = normalize(raw.clone(), BTreeMap::new()).unwrap();
    let Plan::Apply { changes } = evaluate(&snapshot, &config()) else {
        panic!("complete scaffolding must persist bootstrap intent");
    };
    changes
        .into_iter()
        .find_map(|change| match change {
            kuberic_protocol::command::KubernetesChange::PersistStatus { status } => Some(*status),
            _ => None,
        })
        .unwrap()
}

fn uninitialized_report() -> proto::AgentStatusReport {
    proto::AgentStatusReport {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        resource_uid: UID.to_string(),
        process_session_id: "session-1".to_string(),
        report_sequence: 1,
        storage_state: proto::AgentStorageState::Uninitialized as i32,
        pod_uid: POD_UID.to_string(),
        pvc_uid: PVC_UID.to_string(),
        replica_id: 1,
        ..Default::default()
    }
}

fn initialized_report(
    identity: kuberic_protocol::types::ReplicaIdentity,
    configuration: kuberic_protocol::types::ConfigurationDescriptor,
) -> proto::AgentStatusReport {
    proto::AgentStatusReport {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        resource_uid: UID.to_string(),
        identity: Some(identity.into()),
        process_session_id: "session-2".to_string(),
        report_sequence: 2,
        role: proto::ReplicaRole::Primary as i32,
        read_status: proto::AccessStatus::Granted as i32,
        write_status: proto::AccessStatus::Granted as i32,
        epoch: Some(configuration.epoch.into()),
        current_configuration: Some(configuration.into()),
        current_progress: 5,
        committed_lsn: 5,
        catch_up_capability: Some(5),
        current_configuration_quorum_progress: 5,
        catch_up_complete: true,
        storage_state: proto::AgentStorageState::Initialized as i32,
        healthy: true,
        replica_id: 1,
        ..Default::default()
    }
}

#[test]
fn normalization_distinguishes_missing_and_unreachable_agents() {
    let missing = normalize(raw(1), BTreeMap::new()).unwrap();
    assert!(matches!(
        missing.replicas.values().next().unwrap().agent,
        AgentObservation::Absent
    ));

    let mut unreachable = with_scaffolding(raw(1));
    unreachable.agents.insert(
        ReplicaId::new(1),
        RawAgentObservation::Unavailable {
            message: "reconstructing".to_string(),
        },
    );
    let unreachable = normalize(unreachable, BTreeMap::new()).unwrap();
    assert!(matches!(
        unreachable.replicas.values().next().unwrap().agent,
        AgentObservation::Unreachable { .. }
    ));
}

#[tokio::test]
async fn status_conflict_reobserves_without_dispatching_authority() {
    let api = Arc::new(InMemoryClusterApi::new(with_scaffolding(raw(1))));
    api.conflict_next_status().await;
    let reconciler = Reconciler::new(api.clone(), config());
    let action = reconciler.reconcile("tests", "db").await.unwrap();
    assert_eq!(action.kind, ReconcileKind::ObservationStale);
    assert_eq!(action.requeue_after, Duration::ZERO);
    assert!(api.effects().await.is_empty());
}

#[tokio::test]
async fn overlapping_reconciles_are_serialized_per_resource() {
    let api = Arc::new(InMemoryClusterApi::new(raw(1)));
    api.set_observation_delay(Duration::from_millis(30)).await;
    let reconciler = Arc::new(Reconciler::new(api.clone(), config()));
    let first = {
        let reconciler = reconciler.clone();
        tokio::spawn(async move { reconciler.reconcile("tests", "db").await })
    };
    let second = {
        let reconciler = reconciler.clone();
        tokio::spawn(async move { reconciler.reconcile("tests", "db").await })
    };
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(api.max_active_observations().await, 1);
}

#[tokio::test]
async fn each_execute_requires_a_fresh_full_observation() {
    let mut observation = with_scaffolding(raw(1));
    observation.set.status = Some(KubericSetStatus {
        authority: bootstrap_status(&observation),
    });
    observation.agents.insert(
        ReplicaId::new(1),
        RawAgentObservation::Report(Box::new(uninitialized_report())),
    );
    let api = Arc::new(InMemoryClusterApi::new(observation));
    let reconciler = Reconciler::new(api.clone(), config());

    let status_update = reconciler.reconcile("tests", "db").await.unwrap();
    assert_eq!(status_update.kind, ReconcileKind::Applied);
    let mut refreshed = api.observation().await;
    let RawAgentObservation::Report(report) = refreshed.agents.get_mut(&ReplicaId::new(1)).unwrap()
    else {
        panic!("uninitialized report");
    };
    report.report_sequence += 1;
    api.set_observation(refreshed).await;
    let first = reconciler.reconcile("tests", "db").await.unwrap();
    assert_eq!(first.kind, ReconcileKind::Executed);
    assert_eq!(first.requeue_after, Duration::ZERO);
    assert_eq!(api.observation_count().await, 2);
    assert_eq!(
        api.effects()
            .await
            .iter()
            .filter(|effect| matches!(effect, EffectRecord::Execute(_)))
            .count(),
        1
    );

    let mut refreshed = api.observation().await;
    let RawAgentObservation::Report(report) = refreshed.agents.get_mut(&ReplicaId::new(1)).unwrap()
    else {
        panic!("uninitialized report");
    };
    report.report_sequence += 1;
    api.set_observation(refreshed).await;
    reconciler.reconcile("tests", "db").await.unwrap();
    assert_eq!(api.observation_count().await, 3);
    assert_eq!(
        api.effects()
            .await
            .iter()
            .filter(|effect| matches!(effect, EffectRecord::Execute(_)))
            .count(),
        2
    );
}

#[tokio::test]
async fn startup_unavailable_is_a_bounded_wait() {
    let mut observation = with_scaffolding(raw(1));
    observation.set.status = Some(KubericSetStatus {
        authority: bootstrap_status(&observation),
    });
    observation.agents.insert(
        ReplicaId::new(1),
        RawAgentObservation::Unavailable {
            message: "runtime reconstruction in progress".to_string(),
        },
    );
    let api = Arc::new(InMemoryClusterApi::new(observation));
    let reconciler = Reconciler::new(api, config());
    let status_update = reconciler.reconcile("tests", "db").await.unwrap();
    assert_eq!(status_update.kind, ReconcileKind::Applied);
    let action = reconciler.reconcile("tests", "db").await.unwrap();
    assert_eq!(action.kind, ReconcileKind::Waiting);
    assert_eq!(action.requeue_after, Duration::from_secs(3));
}

#[tokio::test]
async fn stable_and_unsafe_states_use_bounded_reobservation() {
    let scaffold = with_scaffolding(raw(1));
    let transition = bootstrap_status(&scaffold)
        .transition
        .expect("bootstrap transition");
    let configuration = transition.current_configuration;
    let identity = configuration.members[0].identity.clone();
    assert_eq!(configuration.members[0].role, ReplicaRole::Primary);

    let mut stable = scaffold;
    stable.set.status = Some(KubericSetStatus {
        authority: AcceptedStatus {
            initialized: true,
            observed_generation: 1,
            topology: Some(AcceptedTopology {
                configuration: configuration.clone(),
            }),
            ..Default::default()
        },
    });
    stable.agents.insert(
        ReplicaId::new(1),
        RawAgentObservation::Report(Box::new(initialized_report(
            identity.clone(),
            configuration,
        ))),
    );
    stable.services.push(Service {
        metadata: kube::core::ObjectMeta {
            name: Some("db-write".to_string()),
            uid: Some("service-uid".to_string()),
            resource_version: Some("4".to_string()),
            labels: Some(BTreeMap::from([(
                SET_UID_LABEL.to_string(),
                UID.to_string(),
            )])),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(BTreeMap::from([(
                INSTANCE_LABEL.to_string(),
                identity.instance_id.to_string(),
            )])),
            ..Default::default()
        }),
        ..Default::default()
    });
    let stable_api = Arc::new(InMemoryClusterApi::new(stable));
    let stable_action = Reconciler::new(stable_api, config())
        .reconcile("tests", "db")
        .await
        .unwrap();
    assert_eq!(stable_action.kind, ReconcileKind::Stable);
    assert_eq!(stable_action.requeue_after, Duration::from_secs(11));

    let unsafe_api = Arc::new(InMemoryClusterApi::new(raw(0)));
    let unsafe_reconciler = Reconciler::new(unsafe_api.clone(), config());
    let unsafe_action = unsafe_reconciler.reconcile("tests", "db").await.unwrap();
    assert_eq!(unsafe_action.kind, ReconcileKind::Unsafe);
    assert_eq!(unsafe_action.requeue_after, Duration::from_secs(7));

    let recovered = raw(1);
    unsafe_api.set_observation(recovered).await;
    let recovered_action = unsafe_reconciler.reconcile("tests", "db").await.unwrap();
    assert_eq!(recovered_action.kind, ReconcileKind::Applied);
    assert_eq!(recovered_action.requeue_after, Duration::ZERO);
}

#[test]
fn exact_pod_uid_is_preserved_as_the_replica_instance() {
    let snapshot = normalize(with_scaffolding(raw(1)), BTreeMap::new()).unwrap();
    let observation = snapshot.replicas.values().next().unwrap();
    let kubernetes = observation.kubernetes.as_ref().unwrap();
    assert_eq!(kubernetes.pod_uid.as_ref().unwrap().as_str(), POD_UID);
    assert_eq!(kubernetes.pvc_uid.as_ref().unwrap().as_str(), PVC_UID);
    assert_eq!(kubernetes.pod_name, "db-1");
    assert_eq!(kubernetes.pvc_name, "db-1-data");
    assert!(kubernetes.pod_ready);
    assert_eq!(snapshot.resource_uid.as_str(), UID);
    assert_eq!(snapshot.resource_version, "1");
    assert_eq!(snapshot.intended_replica_ids(), vec![ReplicaId::new(1)]);
    assert_eq!(snapshot.replicas.len(), 1);
    let key = snapshot.replicas.keys().next().unwrap();
    assert_eq!(key.instance_id.as_str(), POD_UID);
    assert_eq!(key.replica_id, ReplicaId::new(1));
    assert!(!snapshot.durable_storage_evidence);
    assert!(snapshot.observation_failures.is_empty());
    assert!(snapshot.routing.write_target.is_none());
    assert!(snapshot.has_complete_scaffolding());
    assert_eq!(observation.agent, AgentObservation::Absent);
}
