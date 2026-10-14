use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kuberic_controller::cluster_api::InMemoryClusterApi;
use kuberic_controller::crd::{KubericSet, KubericSetSpec};
use kuberic_controller::observation::{RawAgentObservation, RawObservation};
use kuberic_controller::reconciler::{ReconcileKind, Reconciler};
use kuberic_runtime::protocol::observation::ReplicaObservationKey;
use kuberic_runtime::protocol::public_operations::{
    PublicOperationPreviewIdentity, PublicProgress, PublicProgressRole, PublicV1AgentReport,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, AgentGeneration, PodUid, PublicProtocolGeneration, PvcUid, ReplicaId,
    ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity,
    ReplicaStorageIncarnationContext, ResourceUid, StorageGeneration, derive_bootstrap_history,
    derive_history_initialization_id,
};

fn observation() -> RawObservation {
    let resource_uid = ResourceUid::new("resource-1");
    let generation = PublicProtocolGeneration::new(1);
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
    let identity = ReplicaIdentity {
        replica_id: ReplicaId::new(1),
        instance_id: ReplicaInstanceId::new("pod-1"),
        agent_generation: AgentGeneration::new("generation-1"),
    };
    let report = PublicV1AgentReport {
        identity: PublicOperationPreviewIdentity::public_v1(1),
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
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new("pod-1"),
                pvc_uid: PvcUid::new("pvc-1"),
            },
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
        agents: BTreeMap::from([(
            ReplicaObservationKey::new(ReplicaId::new(1), ReplicaInstanceId::new("pod-1")),
            RawAgentObservation::PublicV1Report(Box::new(report)),
        )]),
        #[cfg(feature = "runtime-test-bridge")]
        exact_resources: Vec::new(),
        failures: Vec::new(),
        now_unix_seconds: 0,
    }
}

#[tokio::test]
async fn production_reconciler_bootstraps_then_stabilizes_on_public_v1_reports() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let api = Arc::new(InMemoryClusterApi::new(observation()));
        let reconciler = Reconciler::new(
            api,
            kuberic_controller::production_evaluation_config(30, 5, 30),
        );
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
