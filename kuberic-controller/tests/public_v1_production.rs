use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kube::ResourceExt;
use kuberic_controller::cluster_api::InMemoryClusterApi;
use kuberic_controller::crd::{INSTANCE_LABEL, KubericSet, KubericSetSpec};
use kuberic_controller::observation::{RawAgentObservation, RawObservation};
use kuberic_controller::reconciler::{ReconcileKind, Reconciler};
use kuberic_runtime::protocol::observation::ReplicaObservationKey;
use kuberic_runtime::protocol::public_operations::{
    PublicProgress, PublicProgressRole, PublicV1AgentReport, PublicV1Identity,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, PodUid, PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity, ReplicaStorageIncarnationContext,
    ResourceUid, StorageGeneration, derive_agent_generation, derive_bootstrap_history,
    derive_history_initialization_id, derive_initialization_id,
};

fn empty_observation() -> RawObservation {
    let mut set = KubericSet::new(
        "sample",
        KubericSetSpec {
            replicas: 1,
            image: "example".into(),
            failover_delay_seconds: 30,
            switchover: None,
        },
    );
    set.metadata.namespace = Some("default".into());
    set.metadata.uid = Some("resource-1".into());
    set.metadata.resource_version = Some("1".into());
    set.metadata.generation = Some(1);
    RawObservation {
        set,
        pods: Vec::new(),
        pvcs: Vec::new(),
        services: Vec::new(),
        secrets: Vec::new(),
        agents: BTreeMap::new(),
        failures: Vec::new(),
        now_unix_seconds: 0,
    }
}

fn report_for(observation: &RawObservation) -> PublicV1AgentReport {
    let resource_uid = ResourceUid::new(observation.set.uid().expect("set UID"));
    let generation = PublicProtocolGeneration::new(1);
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
    let pod = observation
        .pods
        .iter()
        .find(|pod| pod.name_any() == "sample-1")
        .expect("bootstrap Pod");
    let pvc = observation
        .pvcs
        .iter()
        .find(|pvc| pvc.name_any() == "sample-1-data")
        .expect("bootstrap PVC");
    let pod_uid = PodUid::new(pod.uid().expect("bootstrap Pod UID"));
    let pvc_uid = PvcUid::new(pvc.uid().expect("bootstrap PVC UID"));
    let replica_id = ReplicaId::new(1);
    let initialization_id = derive_initialization_id(&resource_uid, replica_id, &pod_uid, &pvc_uid);
    let identity = ReplicaIdentity {
        replica_id,
        instance_id: ReplicaInstanceId::new(pod_uid.as_str()),
        agent_generation: derive_agent_generation(&initialization_id),
    };
    let report = PublicV1AgentReport {
        identity: PublicV1Identity::new(1),
        resource_uid,
        replica: identity.clone(),
        process_session_id: kuberic_runtime::protocol::types::ProcessSessionId::new("session-1"),
        report_sequence: 1,
        role: ReplicaRole::None,
        read_status: AccessStatus::NotPrimary,
        write_status: AccessStatus::NotPrimary,
        history,
        storage_incarnation: ReplicaStorageIncarnationContext {
            replica: identity.clone(),
            storage: ReplicaStorageIdentity::Persistent { pod_uid, pvc_uid },
            storage_generation: StorageGeneration::new("storage-1"),
        },
        progress: PublicProgress {
            role: PublicProgressRole::Recovery,
            first: 0,
            last: 0,
        },
        service_location: None,
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        extensions: Default::default(),
    };
    report.validate().unwrap();
    report
}

#[tokio::test]
async fn production_reconciler_bootstraps_then_stabilizes_on_public_v1_reports() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let api = Arc::new(InMemoryClusterApi::new(empty_observation()));
        let reconciler = Reconciler::new(
            api.clone(),
            kuberic_controller::production_evaluation_config(30, 5, 30),
        );
        for _ in 0..8 {
            let action = reconciler.reconcile("default", "sample").await.unwrap();
            assert_eq!(action.kind, ReconcileKind::Executed);
            let observed = api.observation().await;
            if observed.pods.len() == 1
                && observed.pvcs.len() == 1
                && observed.services.len() == 3
                && observed.secrets.len() == 1
            {
                break;
            }
        }
        let mut observed = api.observation().await;
        assert_eq!(observed.pods.len(), 1);
        assert_eq!(observed.pvcs.len(), 1);
        assert_eq!(observed.services.len(), 3);
        assert_eq!(observed.secrets.len(), 1);
        let pod_uid = observed.pods[0].uid().expect("bootstrap Pod UID");
        assert_eq!(
            observed.pods[0].labels().get(INSTANCE_LABEL),
            Some(&pod_uid)
        );
        let report = report_for(&observed);
        observed.agents.insert(
            ReplicaObservationKey::new(
                report.replica.replica_id,
                report.replica.instance_id.clone(),
            ),
            RawAgentObservation::PublicV1Report(Box::new(report)),
        );
        api.set_observation(observed).await;
        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );
        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Stable
        );
    })
    .await
    .expect("public-v1 production reconciliation exceeded 270 seconds");
}
