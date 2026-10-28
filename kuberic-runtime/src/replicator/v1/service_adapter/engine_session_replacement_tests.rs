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

fn live_target_fixture() -> (Binding, Binding) {
    let seed = fixture();
    let old = seed.replacement().unwrap();
    let mut source = seed.source.clone();
    let mut target = seed.target.clone();
    target.scope.storage = old.storage_incarnation.clone();
    target.scope.process_session_id = old.process_session_id.clone();
    target.scope.creation_id = ReplicatorCreationId::new("original-target-creation");
    target.replication_address = old.replication_address.clone();
    target.role = ReplicaRole::ActiveSecondary;
    target.scope.configuration.replicas = vec![description(&source, ReplicaRole::Primary)];
    source.scope.configuration.replicas = vec![description(&target, ReplicaRole::ActiveSecondary)];
    let predecessor = Binding::new(source.clone(), target.clone()).unwrap();
    let previous = source.scope.configuration.current.clone();
    let mut current = previous.clone();
    current.epoch.configuration_number += 1;
    current.configuration_id = current.expected_id();
    for publication in [&mut source, &mut target] {
        publication.scope.epoch = current.epoch;
        publication.scope.configuration.current = current.clone();
        publication.scope.configuration.previous = Some(previous.clone());
    }
    target.scope.process_session_id = ProcessSessionId::new("reconstructed-process");
    target.scope.creation_id = ReplicatorCreationId::new("reconstructed-creation");
    target.replication_address = "http://reconstructed:1".into();
    source.scope.configuration.replicas = vec![description(&target, ReplicaRole::ActiveSecondary)];
    target.scope.configuration.replicas = vec![description(&source, ReplicaRole::Primary)];
    let mut next = Binding::new(source, target).unwrap();
    next.reformation = true;
    next.live_target = Some(LiveTarget {
        predecessor: predecessor.nonce.clone(),
        previous,
        source: predecessor.primary(),
        target: predecessor.secondary(),
    });
    next.validate().unwrap();
    (predecessor, next)
}

#[test]
fn live_target_relation_is_not_continuation_idle_credit_or_a_competing_proposal() {
    let (old, next) = live_target_fixture();
    let sessions = Sessions::default();
    sessions.propose(&old).unwrap();
    sessions.activate_source(&old).unwrap();
    sessions.complete(&old).unwrap();
    assert!(sessions.proposal(next.clone()).is_err());
    let reserved = sessions.live_target_proposal(next.clone()).unwrap();
    assert_eq!(sessions.binding_for(2), Some(old.clone()));
    assert!(sessions.live_target_joined(&reserved).is_err());
    assert!(sessions.active(&reserved).is_err());
    let mut retry = next.clone();
    retry.nonce = uuid::Uuid::new_v4().to_string();
    assert_eq!(sessions.live_target_proposal(retry).unwrap(), reserved);
    let mut competitor = next.clone();
    competitor.nonce = uuid::Uuid::new_v4().to_string();
    competitor.target.scope.creation_id = ReplicatorCreationId::new("competing-creation");
    assert!(sessions.live_target_proposal(competitor).is_err());
    sessions.retire(&old).unwrap();
    sessions.live_target_joined(&reserved).unwrap();
    assert_eq!(sessions.binding_for(2), Some(reserved.clone()));
    assert!(sessions.active(&old).is_err());
    assert!(sessions.active(&reserved).is_err());
    assert!(!sessions.completed(&reserved));
    let mut continued = next;
    continued.live_target = None;
    continued.reformation = false;
    continued.continuation = Some(old.nonce);
    assert!(sessions.continuation_predecessor(&continued).is_err());
}

#[test]
fn live_target_relation_rejects_source_storage_history_epoch_and_original_quorum_changes() {
    let (_, binding) = live_target_fixture();
    assert_eq!(
        serde_json::from_slice::<Binding>(&serde_json::to_vec(&binding).unwrap()).unwrap(),
        binding
    );
    for fault in 0..9 {
        let mut crossed = binding.clone();
        match fault {
            0 => {
                crossed.source.scope.creation_id =
                    ReplicatorCreationId::new("different-source-creation")
            }
            1 => {
                crossed.source.scope.process_session_id =
                    ProcessSessionId::new("different-source-process")
            }
            2 => {
                crossed.target.scope.storage.storage_generation =
                    StorageGeneration::new("different-storage")
            }
            3 => crossed.target.scope.history.history_id = HistoryId::new("different-history"),
            4 => crossed.live_target.as_mut().unwrap().previous.write_quorum = 1,
            5 => {
                crossed.target.scope.process_session_id = crossed
                    .live_target
                    .as_ref()
                    .unwrap()
                    .target
                    .process_session_id
                    .clone()
            }
            6 => {
                crossed.target.scope.creation_id = crossed
                    .live_target
                    .as_ref()
                    .unwrap()
                    .target
                    .creation_id
                    .clone()
            }
            7 => crossed.source.scope.configuration.previous = None,
            _ => crossed.live_target.as_mut().unwrap().previous.epoch = binding.epoch(),
        }
        assert!(crossed.validate().is_err(), "fault {fault}");
    }
}
