use super::*;
use crate::protocol::public_operations::*;
use crate::protocol::types::*;

pub(in crate::replicator::v1::service_adapter) fn description(
    publication: &ReplicaPublication,
    role: ReplicaRole,
) -> ReplicaPeerDescription {
    ReplicaPeerDescription {
        identity: publication.scope.storage.replica.clone(),
        replication_address: publication.replication_address.clone(),
        process_session_id: publication.scope.process_session_id.clone(),
        storage_incarnation: publication.scope.storage.clone(),
        role,
        current_progress: 1,
        catch_up_capability: 1,
        is_up: true,
        is_ready: true,
        must_catchup: false,
    }
}

pub(in crate::replicator::v1::service_adapter) fn replacement_from(
    mut source: ReplicaPublication,
    mut target: ReplicaPublication,
) -> Binding {
    let old_identity = target.scope.storage.replica.clone();
    let mut old = description(&target, ReplicaRole::ActiveSecondary);
    old.is_up = false;
    old.is_ready = false;
    old.current_progress = crate::application::INVALID_LSN;
    old.catch_up_capability = crate::application::INVALID_LSN;
    let epoch = Epoch::new(
        source.scope.epoch.data_loss_number,
        source.scope.epoch.configuration_number + 1,
    );
    let current = ConfigurationDescriptor::new(
        epoch,
        source.scope.storage.replica.replica_id,
        vec![
            ConfigurationMember {
                identity: source.scope.storage.replica.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: old_identity,
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    source.scope.epoch = epoch;
    source.scope.configuration = ReplicaConfigurationScope {
        current: current.clone(),
        previous: None,
        replicas: vec![old.clone()],
    };
    source.stage = ReplicaPublicationStage::AchievedScope;
    source.role = ReplicaRole::Primary;
    target.scope.storage.replica.instance_id = ReplicaInstanceId::new("fresh-instance");
    target.scope.storage.replica.agent_generation = AgentGeneration::new("fresh-agent");
    target.scope.storage.storage = ReplicaStorageIdentity::Persistent {
        pod_uid: PodUid::new("fresh-pod"),
        pvc_uid: PvcUid::new("fresh-pvc"),
    };
    target.scope.storage.storage_generation = StorageGeneration::new("fresh-storage");
    target.scope.process_session_id = ProcessSessionId::new("fresh-process");
    target.scope.creation_id = ReplicatorCreationId::new("fresh-creation");
    target.replication_address = "http://fresh:1".into();
    target.scope.epoch = epoch;
    target.scope.configuration = ReplicaConfigurationScope {
        current,
        previous: None,
        replicas: vec![description(&source, ReplicaRole::Primary), old],
    };
    target.stage = ReplicaPublicationStage::AchievedScope;
    target.role = ReplicaRole::IdleSecondary;
    Binding::new(source, target).unwrap()
}

pub(in crate::replicator::v1::service_adapter) fn fixture() -> Binding {
    let resource = ResourceUid::new("outer-replacement");
    let generation = PublicProtocolGeneration::new(1);
    let initialization = derive_history_initialization_id(&resource, generation);
    let history = derive_bootstrap_history(&resource, generation, &initialization);
    let publication = |id, role| {
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(id),
            instance_id: ReplicaInstanceId::new(format!("instance-{id}")),
            agent_generation: AgentGeneration::new(format!("agent-{id}")),
        };
        ReplicaPublication {
            format: ORDINARY_ACTION_FORMAT,
            scope: ReplicaScope {
                history: history.clone(),
                storage: ReplicaStorageIncarnationContext {
                    replica: identity.clone(),
                    storage: ReplicaStorageIdentity::Persistent {
                        pod_uid: PodUid::new(format!("pod-{id}")),
                        pvc_uid: PvcUid::new(format!("pvc-{id}")),
                    },
                    storage_generation: StorageGeneration::new(format!("storage-{id}")),
                },
                process_session_id: ProcessSessionId::new(format!("process-{id}")),
                creation_id: ReplicatorCreationId::new(format!("creation-{id}")),
                epoch: Epoch::new(history.data_loss_number, 1),
                configuration: ReplicaConfigurationScope {
                    current: ConfigurationDescriptor::new(
                        Epoch::new(history.data_loss_number, 1),
                        ReplicaId::new(1),
                        vec![ConfigurationMember {
                            identity: ReplicaIdentity {
                                replica_id: ReplicaId::new(1),
                                instance_id: ReplicaInstanceId::new("instance-1"),
                                agent_generation: AgentGeneration::new("agent-1"),
                            },
                            role: ReplicaRole::Primary,
                        }],
                        1,
                    ),
                    previous: None,
                    replicas: Vec::new(),
                },
            },
            replication_address: format!("http://replica-{id}:1"),
            generation: 1,
            stage: ReplicaPublicationStage::AchievedScope,
            role,
        }
    };
    replacement_from(
        publication(1, ReplicaRole::Primary),
        publication(2, ReplicaRole::IdleSecondary),
    )
}

#[test]
fn m3_replacement_binding_serializes_exact_predecessor_and_rejects_untagged_or_live_overlap() {
    let binding = fixture();
    let bytes = serde_json::to_vec(&binding).unwrap();
    let restored: Binding = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(restored, binding);
    restored.validate().unwrap();
    let mut untagged = WireBinding::from(binding.clone());
    untagged.replacement = None;
    assert!(Binding::try_from(untagged).is_err());
    for mutation in 0..4 {
        let mut changed = WireBinding::from(binding.clone());
        let selected = changed.replacement.as_mut().unwrap();
        match mutation {
            0 => selected.is_up = true,
            1 => selected.current_progress = 1,
            2 => selected.process_session_id = changed.target.scope.process_session_id.clone(),
            _ => selected.storage_incarnation = changed.target.scope.storage.clone(),
        }
        assert!(Binding::try_from(changed).is_err());
    }
}

#[test]
fn m3_cold_replacement_reservation_has_no_invented_completed_predecessor_or_ready() {
    let sessions = Sessions::default();
    let binding = sessions.proposal(fixture()).unwrap();
    assert!(
        sessions
            .replacement_predecessor(&binding)
            .unwrap()
            .is_none()
    );
    let mut retry = binding.clone();
    retry.nonce = uuid::Uuid::new_v4().to_string();
    assert_eq!(sessions.proposal(retry).unwrap(), binding);
    assert!(sessions.binding_for(2).is_none());
    assert!(!sessions.completed(&binding));
    sessions.replacement_source_settled(&binding).unwrap();
    assert_eq!(sessions.binding_for(2), Some(binding.clone()));
    assert!(sessions.active(&binding).is_err());
    assert!(!sessions.completed(&binding));
    let mut competing = binding.clone();
    competing.nonce = uuid::Uuid::new_v4().to_string();
    competing.target.scope.creation_id = ReplicatorCreationId::new("another-creation");
    assert!(sessions.proposal(competing).is_err());
}
