use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kuberic_controller::cluster_api::InMemoryClusterApi;
use kuberic_controller::crd::{
    KubericSet, KubericSetSpec, KubericSetStatus, PublicV1AuthorityStatus, PublicV1OperationStatus,
    PublicV1ReplacementState, PublicV1TopologyState,
};
use kuberic_controller::observation::{RawObservation, RawObservationFailure};
use kuberic_controller::reconciler::Reconciler;
use kuberic_runtime::control::{normalize_public_v1_agent_status_report, proto};
use kuberic_runtime::protocol::public_operations::{PublicV1AcceptedStatus, PublicV1Identity};
use kuberic_runtime::protocol::types::{
    AgentGeneration, ConfigurationDescriptor, ConfigurationMember, Epoch, HistoryContext,
    HistoryId, HistoryInitializationId, OperationId, PodUid, ProcessSessionEndpoint,
    ProcessSessionId, PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity, ReplicaStorageIncarnationContext,
    ReplicatorCreationId, ResourceUid, StorageGeneration, derive_bootstrap_history,
    derive_history_initialization_id,
};

fn valid_status_parts() -> (
    PublicV1AuthorityStatus,
    PublicV1TopologyState,
    ReplicaStorageIncarnationContext,
) {
    let resource_uid = ResourceUid::new("resource-1");
    let generation = PublicProtocolGeneration::new(1);
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
    let identity = ReplicaIdentity {
        replica_id: ReplicaId::new(1),
        instance_id: ReplicaInstanceId::new("pod-1"),
        agent_generation: AgentGeneration::new("generation-1"),
    };
    let storage = ReplicaStorageIncarnationContext {
        replica: identity.clone(),
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new("pod-1"),
            pvc_uid: PvcUid::new("pvc-1"),
        },
        storage_generation: StorageGeneration::new("storage-1"),
    };
    let configuration = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        identity.replica_id,
        vec![ConfigurationMember {
            identity: identity.clone(),
            role: ReplicaRole::Primary,
        }],
        1,
    );
    let endpoint = ProcessSessionEndpoint {
        identity,
        storage_incarnation: storage.clone(),
        process_session_id: ProcessSessionId::new("session-1"),
        replicator_creation_id: ReplicatorCreationId::new("creation-1"),
        role: ReplicaRole::Primary,
    };
    let accepted = PublicV1AcceptedStatus {
        identity: PublicV1Identity::new(1),
        history_initialization_id: initialization.clone(),
        history,
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        extensions: Default::default(),
    };
    (
        PublicV1AuthorityStatus {
            identity: accepted.identity.clone(),
            history_initialization_id: initialization,
            history: accepted.history.clone(),
            completed_operation_count: 0,
            canonical_authority_json: serde_json::to_string(&accepted).unwrap(),
        },
        PublicV1TopologyState::from_configuration(&configuration, &[endpoint]).unwrap(),
        storage,
    )
}

#[test]
fn old_and_partial_controller_inputs_reject_without_mutation() {
    let old = proto::AgentStatusReport {
        protocol_version: 9,
        public_v1: Some(proto::PublicV1AgentStatus::default()),
        ..Default::default()
    };
    let old_before = old.clone();
    assert!(normalize_public_v1_agent_status_report(old.clone()).is_err());
    assert_eq!(old, old_before);

    let partial = proto::AgentStatusReport {
        protocol_version: 11,
        public_v1: Some(proto::PublicV1AgentStatus {
            protocol_version: 11,
            generation: 1,
            resource_uid: "resource".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let partial_before = partial.clone();
    assert!(normalize_public_v1_agent_status_report(partial.clone()).is_err());
    assert_eq!(partial, partial_before);
}

#[test]
fn old_and_partial_controller_status_reject_without_mutation() {
    assert!(
        serde_json::from_value::<KubericSetStatus>(serde_json::json!({
            "initialized": true,
            "observedGeneration": 1
        }))
        .is_err()
    );

    let partial = KubericSetStatus {
        public_v1_authority: Some(PublicV1AuthorityStatus {
            identity: PublicV1Identity::new(1),
            history_initialization_id: HistoryInitializationId::new(""),
            history: HistoryContext {
                resource_uid: ResourceUid::new("resource-1"),
                protocol_generation: PublicProtocolGeneration::new(1),
                history_id: HistoryId::new(""),
                data_loss_number: 0,
            },
            completed_operation_count: 0,
            canonical_authority_json: serde_json::to_string(&PublicV1AcceptedStatus {
                identity: PublicV1Identity::new(1),
                history_initialization_id: HistoryInitializationId::new("different"),
                history: HistoryContext {
                    resource_uid: ResourceUid::new("resource-1"),
                    protocol_generation: PublicProtocolGeneration::new(1),
                    history_id: HistoryId::new("different"),
                    data_loss_number: 0,
                },
                data_loss_recovery: None,
                completed_operations: Vec::new(),
                extensions: Default::default(),
            })
            .unwrap(),
        }),
        ..Default::default()
    };
    let partial_before = partial.clone();
    assert!(partial.validate_public_v1().is_err());
    assert_eq!(partial, partial_before);
}

#[tokio::test]
async fn partial_controller_status_rejects_before_reconciliation_effects() {
    tokio::time::timeout(Duration::from_secs(270), async {
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
        set.status = Some(KubericSetStatus {
            public_v1_operation: Some(PublicV1OperationStatus {
                target: ReplicaIdentity {
                    replica_id: ReplicaId::new(1),
                    instance_id: ReplicaInstanceId::new("pod-1"),
                    agent_generation: AgentGeneration::new("generation-1"),
                },
                operation_id: OperationId::new("orphan-operation"),
                revision: 1,
                canonical_command_json: "{}".into(),
            }),
            ..Default::default()
        });
        let observation = RawObservation {
            set,
            pods: Vec::new(),
            pvcs: Vec::new(),
            services: Vec::new(),
            secrets: Vec::new(),
            agents: BTreeMap::new(),
            failures: Vec::new(),
            now_unix_seconds: 0,
        };
        let before = observation.clone();
        let api = Arc::new(InMemoryClusterApi::new(observation));
        let reconciler = Reconciler::new(
            api.clone(),
            kuberic_controller::production_evaluation_config(30, 5, 30),
        );
        assert!(reconciler.reconcile("default", "sample").await.is_err());
        assert_eq!(api.observation().await, before);
        assert!(api.effects().await.is_empty());
    })
    .await
    .expect("controller rejection ordering exceeded 270 seconds");
}

#[tokio::test]
async fn incomplete_kubernetes_observation_rejects_before_effects() {
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
    let observation = RawObservation {
        set,
        pods: Vec::new(),
        pvcs: Vec::new(),
        services: Vec::new(),
        secrets: Vec::new(),
        agents: BTreeMap::new(),
        failures: vec![RawObservationFailure {
            source: "pods".into(),
            message: "injected list failure".into(),
        }],
        now_unix_seconds: 0,
    };
    let before = observation.clone();
    let api = Arc::new(InMemoryClusterApi::new(observation));
    let reconciler = Reconciler::new(
        api.clone(),
        kuberic_controller::production_evaluation_config(30, 5, 30),
    );

    assert!(reconciler.reconcile("default", "sample").await.is_err());
    assert_eq!(api.observation().await, before);
    assert!(api.effects().await.is_empty());
}

#[tokio::test]
async fn cross_field_partial_status_rejects_before_effects() {
    let (authority, topology, predecessor) = valid_status_parts();
    let crossed = ReplicaStorageIncarnationContext {
        replica: ReplicaIdentity {
            replica_id: predecessor.replica.replica_id,
            instance_id: ReplicaInstanceId::new("other-pod"),
            agent_generation: AgentGeneration::new("other-generation"),
        },
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new("other-pod"),
            pvc_uid: PvcUid::new("other-pvc"),
        },
        storage_generation: StorageGeneration::new("other-storage"),
    };
    let statuses = [
        KubericSetStatus {
            public_v1_topology: Some(topology.clone()),
            ..Default::default()
        },
        KubericSetStatus {
            public_v1_replacement: Some(
                PublicV1ReplacementState::new(&topology.configuration_id, predecessor.clone())
                    .unwrap(),
            ),
            ..Default::default()
        },
        KubericSetStatus {
            public_v1_authority: Some(authority.clone()),
            public_v1_replacement: Some(
                PublicV1ReplacementState::new(&topology.configuration_id, predecessor.clone())
                    .unwrap(),
            ),
            ..Default::default()
        },
        KubericSetStatus {
            public_v1_authority: Some(authority),
            public_v1_topology: Some(topology.clone()),
            public_v1_replacement: Some(
                PublicV1ReplacementState::new(&topology.configuration_id, crossed).unwrap(),
            ),
            ..Default::default()
        },
    ];

    for (index, status) in statuses.into_iter().enumerate() {
        let mut set = KubericSet::new(
            format!("sample-{index}").as_str(),
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
        set.status = Some(status);
        let observation = RawObservation {
            set,
            pods: Vec::new(),
            pvcs: Vec::new(),
            services: Vec::new(),
            secrets: Vec::new(),
            agents: BTreeMap::new(),
            failures: Vec::new(),
            now_unix_seconds: 0,
        };
        let before = observation.clone();
        let api = Arc::new(InMemoryClusterApi::new(observation));
        let reconciler = Reconciler::new(
            api.clone(),
            kuberic_controller::production_evaluation_config(30, 5, 30),
        );
        assert!(
            reconciler
                .reconcile("default", format!("sample-{index}").as_str())
                .await
                .is_err()
        );
        assert_eq!(api.observation().await, before);
        assert!(api.effects().await.is_empty());
    }
}
