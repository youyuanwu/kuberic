use std::collections::BTreeMap;

use kuberic_protocol::command::{KubernetesChange, ProtocolCommand};
use kuberic_protocol::evaluator::{EvaluationConfig, evaluate};
use kuberic_protocol::observation::{
    AgentObservation, AgentReport, DesiredState, KubernetesReplicaObservation, ObservationFailure,
    ObservationSnapshot, ReplicaObservation, ReportWatermark, RoutingObservation,
    UninitializedAgentObservation,
};
use kuberic_protocol::plan::{Plan, UnsafeReason, WaitReason};
use kuberic_protocol::types::{
    AcceptedStatus, AcceptedTopology, AccessStatus, AgentGeneration, ConfigurationDescriptor,
    ConfigurationMember, EffectivePolicy, Epoch, PodUid, ProcessSessionId, PvcUid, ReplicaId,
    ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ResourceUid, TransitionIntent, TransitionKind,
    derive_agent_generation, derive_initialization_id, derive_transition_id,
};
use kuberic_protocol::validation::{ValidationError, validate_configuration};

fn identity(id: i64, instance: &str, generation: &str) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(instance),
        agent_generation: AgentGeneration::new(generation),
    }
}

fn member(id: i64, role: ReplicaRole) -> ConfigurationMember {
    ConfigurationMember {
        identity: identity(id, &format!("pod-{id}"), &format!("generation-{id}")),
        role,
    }
}

fn configuration() -> ConfigurationDescriptor {
    ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        ReplicaId::new(1),
        vec![
            member(1, ReplicaRole::Primary),
            member(2, ReplicaRole::ActiveSecondary),
            member(3, ReplicaRole::ActiveSecondary),
        ],
        2,
    )
}

fn desired(replicas: u32) -> DesiredState {
    DesiredState {
        generation: 1,
        replicas,
        image: "example:v1".to_string(),
        failover_delay_seconds: 10,
    }
}

fn empty_snapshot(replicas: u32) -> ObservationSnapshot {
    ObservationSnapshot {
        resource_uid: ResourceUid::new("resource-uid"),
        resource_version: "1".to_string(),
        desired: desired(replicas),
        status: AcceptedStatus::default(),
        replicas: BTreeMap::new(),
        previous_report_watermarks: BTreeMap::new(),
        durable_storage_evidence: false,
        routing: RoutingObservation::default(),
        observation_failures: Vec::new(),
        now_unix_seconds: 100,
    }
}

fn scaffolded_snapshot() -> ObservationSnapshot {
    let mut snapshot = empty_snapshot(3);
    for id in 1..=3 {
        let replica_id = ReplicaId::new(id);
        snapshot.replicas.insert(
            replica_id,
            ReplicaObservation {
                kubernetes: Some(KubernetesReplicaObservation {
                    replica_id,
                    pod_name: format!("example-{id}"),
                    pod_uid: Some(PodUid::new(format!("pod-uid-{id}"))),
                    pvc_name: format!("example-{id}"),
                    pvc_uid: Some(PvcUid::new(format!("pvc-uid-{id}"))),
                    pod_ready: true,
                }),
                agent: AgentObservation::Absent,
            },
        );
    }
    snapshot
}

#[test]
fn configuration_id_is_canonical_across_member_order() {
    let first = configuration();
    let second = ConfigurationDescriptor::new(
        first.epoch,
        first.primary_id,
        vec![
            member(3, ReplicaRole::ActiveSecondary),
            member(1, ReplicaRole::Primary),
            member(2, ReplicaRole::ActiveSecondary),
        ],
        first.write_quorum,
    );

    assert_eq!(first.configuration_id, second.configuration_id);
    assert_eq!(first.members, second.members);
}

#[test]
fn duplicate_logical_replica_id_is_rejected() {
    let configuration = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        ReplicaId::new(1),
        vec![
            member(1, ReplicaRole::Primary),
            ConfigurationMember {
                identity: identity(1, "other-pod", "other-generation"),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );

    assert!(matches!(
        validate_configuration(&configuration, None),
        Err(ValidationError::DuplicateReplicaIds(ids)) if ids == vec![1]
    ));
}

#[test]
fn missing_scaffolding_produces_apply() {
    let plan = evaluate(&empty_snapshot(3), &EvaluationConfig::default());

    assert_eq!(
        plan,
        Plan::Apply {
            changes: vec![KubernetesChange::EnsureReplicaScaffolding {
                replica_ids: vec![ReplicaId::new(1), ReplicaId::new(2), ReplicaId::new(3)],
            }],
        }
    );
}

#[test]
fn durable_evidence_without_authority_is_unsafe() {
    let mut snapshot = empty_snapshot(3);
    snapshot.durable_storage_evidence = true;

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Unsafe {
            reason: UnsafeReason::DurableEvidenceWithoutAuthority,
            ..
        }
    ));
}

#[test]
fn zero_desired_replicas_is_invalid_desired_state() {
    assert!(matches!(
        evaluate(&empty_snapshot(0), &EvaluationConfig::default()),
        Plan::Unsafe {
            reason: UnsafeReason::InvalidDesiredState(_),
            ..
        }
    ));
}

#[test]
fn observation_failure_waits_instead_of_inventing_defaults() {
    let mut snapshot = empty_snapshot(3);
    snapshot.observation_failures.push(ObservationFailure {
        source: "agent-list".to_string(),
        message: "timeout".to_string(),
    });

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Wait {
            reason: WaitReason::AgentUnavailable,
            ..
        }
    ));
}

#[test]
fn normalized_invalid_agent_evidence_is_unsafe() {
    let mut snapshot = scaffolded_snapshot();
    snapshot.replicas.get_mut(&ReplicaId::new(1)).unwrap().agent = AgentObservation::Invalid {
        message: "report sequence regressed".to_string(),
    };

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Unsafe {
            reason: UnsafeReason::ContradictoryReplicaEvidence(_),
            ..
        }
    ));
}

#[test]
fn stale_report_sequence_is_unsafe_within_the_same_session() {
    let mut snapshot = scaffolded_snapshot();
    snapshot.previous_report_watermarks.insert(
        ReplicaId::new(1),
        ReportWatermark {
            process_session_id: ProcessSessionId::new("session-1"),
            report_sequence: 4,
        },
    );
    snapshot.replicas.get_mut(&ReplicaId::new(1)).unwrap().agent =
        AgentObservation::Uninitialized(UninitializedAgentObservation {
            protocol_version: kuberic_protocol::PROTOCOL_VERSION,
            resource_uid: snapshot.resource_uid.clone(),
            replica_id: ReplicaId::new(1),
            pod_uid: PodUid::new("pod-uid-1"),
            pvc_uid: PvcUid::new("pvc-uid-1"),
            process_session_id: ProcessSessionId::new("session-1"),
            report_sequence: 4,
        });

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Unsafe {
            reason: UnsafeReason::InvalidAcceptedAuthority(_),
            ..
        }
    ));
}

#[test]
fn bootstrap_intent_is_deterministic() {
    let snapshot = scaffolded_snapshot();
    let first = evaluate(&snapshot, &EvaluationConfig::default());
    let second = evaluate(&snapshot, &EvaluationConfig::default());

    assert_eq!(first, second);
    let Plan::Apply { changes } = first else {
        panic!("expected bootstrap intent persistence");
    };
    let KubernetesChange::PersistStatus { status } = &changes[0] else {
        panic!("expected status persistence");
    };
    let transition = status.transition.as_ref().expect("bootstrap transition");
    assert_eq!(transition.kind, TransitionKind::Bootstrap);
    assert_eq!(transition.current_configuration.members.len(), 3);
    assert_eq!(transition.effective_policy.write_quorum, 2);
}

#[test]
fn persisted_bootstrap_initializes_exact_first_store() {
    let mut snapshot = scaffolded_snapshot();
    let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("expected bootstrap persistence");
    };
    let KubernetesChange::PersistStatus { status } = changes[0].clone() else {
        panic!("expected persisted status");
    };
    snapshot.status = *status;

    let first_member = &snapshot
        .status
        .transition
        .as_ref()
        .unwrap()
        .current_configuration
        .members[0];
    let observation = snapshot.replicas.get_mut(&ReplicaId::new(1)).unwrap();
    observation.agent = AgentObservation::Uninitialized(UninitializedAgentObservation {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        resource_uid: snapshot.resource_uid.clone(),
        replica_id: ReplicaId::new(1),
        pod_uid: PodUid::new("pod-uid-1"),
        pvc_uid: PvcUid::new("pvc-uid-1"),
        process_session_id: ProcessSessionId::new("session-1"),
        report_sequence: 1,
    });

    let Plan::Execute {
        command: ProtocolCommand::InitializeAgentStore(command),
    } = evaluate(&snapshot, &EvaluationConfig::default())
    else {
        panic!("expected store initialization");
    };
    assert_eq!(
        command.assigned_agent_generation,
        first_member.identity.agent_generation
    );
    assert_eq!(
        command.expected_instance_id,
        first_member.identity.instance_id
    );
    assert_eq!(
        command.assigned_agent_generation,
        derive_agent_generation(&derive_initialization_id(
            &snapshot.resource_uid,
            ReplicaId::new(1),
            &PodUid::new("pod-uid-1"),
            &PvcUid::new("pvc-uid-1"),
        ))
    );
}

#[test]
fn unsupported_protocol_version_is_unsafe() {
    let mut snapshot = scaffolded_snapshot();
    let observation = snapshot.replicas.get_mut(&ReplicaId::new(1)).unwrap();
    observation.agent = AgentObservation::Uninitialized(UninitializedAgentObservation {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION + 1,
        resource_uid: snapshot.resource_uid.clone(),
        replica_id: ReplicaId::new(1),
        pod_uid: PodUid::new("pod-uid-1"),
        pvc_uid: PvcUid::new("pvc-uid-1"),
        process_session_id: ProcessSessionId::new("session-1"),
        report_sequence: 1,
    });

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Unsafe {
            reason: UnsafeReason::IncompatibleProtocolVersion { .. },
            ..
        }
    ));
}

#[test]
fn active_transition_keeps_frozen_policy_after_spec_change() {
    let current_configuration = configuration();
    let policy = EffectivePolicy::fixed(3, 10).unwrap();
    let transition = TransitionIntent {
        transition_id: derive_transition_id(
            &ResourceUid::new("resource-uid"),
            TransitionKind::Bootstrap,
            &current_configuration.configuration_id,
        ),
        kind: TransitionKind::Bootstrap,
        spec_generation: 1,
        effective_policy: policy.clone(),
        previous_configuration_id: None,
        current_configuration,
        started_at_unix_seconds: 100,
    };
    let mut snapshot = empty_snapshot(5);
    snapshot.status.transition = Some(transition);

    let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("expected unsupported-spec condition");
    };
    let KubernetesChange::PersistStatus { status } = changes[0].clone() else {
        panic!("expected status update");
    };
    assert_eq!(status.transition.as_ref().unwrap().effective_policy, policy);

    snapshot.status = *status;
    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Wait {
            reason: WaitReason::AgentUnavailable,
            ..
        }
    ));
}

#[test]
fn stable_topology_does_not_apply_unsupported_scale_request() {
    let configuration = configuration();
    let mut snapshot = empty_snapshot(5);
    snapshot.status = AcceptedStatus {
        initialized: true,
        topology: Some(AcceptedTopology { configuration }),
        ..AcceptedStatus::default()
    };

    let Plan::Stable { status, .. } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("expected stable unsupported projection");
    };
    assert_eq!(
        status
            .conditions
            .iter()
            .find(|condition| condition.type_ == "UnsupportedSpec")
            .unwrap()
            .reason,
        "ReplicaCountImmutable"
    );
}

#[test]
fn stale_replica_epoch_is_unsafe() {
    let configuration = configuration();
    let mut snapshot = empty_snapshot(3);
    snapshot.status = AcceptedStatus {
        initialized: true,
        topology: Some(AcceptedTopology {
            configuration: configuration.clone(),
        }),
        ..AcceptedStatus::default()
    };
    snapshot.replicas.insert(
        ReplicaId::new(1),
        ReplicaObservation {
            kubernetes: None,
            agent: AgentObservation::Report(Box::new(AgentReport {
                protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                resource_uid: snapshot.resource_uid.clone(),
                identity: configuration.members[0].identity.clone(),
                process_session_id: ProcessSessionId::new("session"),
                report_sequence: 1,
                role: ReplicaRole::Primary,
                write_status: AccessStatus::Granted,
                epoch: Epoch::new(0, 0),
                previous_configuration: None,
                current_configuration: Some(configuration),
                current_progress: 1,
                committed_lsn: 1,
                catch_up_capability: Some(1),
            })),
        },
    );

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Unsafe {
            reason: UnsafeReason::InvalidAcceptedAuthority(_),
            ..
        }
    ));
}

#[test]
fn conflicting_primary_claims_are_unsafe() {
    let configuration = configuration();
    let mut snapshot = empty_snapshot(3);
    snapshot.status = AcceptedStatus {
        initialized: true,
        topology: Some(AcceptedTopology {
            configuration: configuration.clone(),
        }),
        ..AcceptedStatus::default()
    };
    for replica_id in [ReplicaId::new(1), ReplicaId::new(2)] {
        let expected_identity = configuration
            .members
            .iter()
            .find(|member| member.identity.replica_id == replica_id)
            .unwrap()
            .identity
            .clone();
        snapshot.replicas.insert(
            replica_id,
            ReplicaObservation {
                kubernetes: None,
                agent: AgentObservation::Report(Box::new(AgentReport {
                    protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                    resource_uid: snapshot.resource_uid.clone(),
                    identity: expected_identity,
                    process_session_id: ProcessSessionId::new(format!("session-{replica_id}")),
                    report_sequence: 1,
                    role: ReplicaRole::Primary,
                    write_status: AccessStatus::Granted,
                    epoch: configuration.epoch,
                    previous_configuration: None,
                    current_configuration: Some(configuration.clone()),
                    current_progress: 1,
                    committed_lsn: 1,
                    catch_up_capability: Some(1),
                })),
            },
        );
    }

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Unsafe {
            reason: UnsafeReason::InvalidAcceptedAuthority(_),
            ..
        }
    ));
}

#[test]
fn accepted_replica_cannot_report_another_incarnation() {
    let configuration = configuration();
    let mut snapshot = empty_snapshot(3);
    snapshot.status = AcceptedStatus {
        initialized: true,
        topology: Some(AcceptedTopology {
            configuration: configuration.clone(),
        }),
        ..AcceptedStatus::default()
    };
    snapshot.replicas.insert(
        ReplicaId::new(1),
        ReplicaObservation {
            kubernetes: None,
            agent: AgentObservation::Report(Box::new(AgentReport {
                protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                resource_uid: snapshot.resource_uid.clone(),
                identity: identity(1, "replacement-pod", "replacement-generation"),
                process_session_id: ProcessSessionId::new("session"),
                report_sequence: 1,
                role: ReplicaRole::Primary,
                write_status: AccessStatus::Granted,
                epoch: configuration.epoch,
                previous_configuration: None,
                current_configuration: Some(configuration),
                current_progress: 1,
                committed_lsn: 1,
                catch_up_capability: Some(1),
            })),
        },
    );

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Unsafe {
            reason: UnsafeReason::InvalidAcceptedAuthority(_),
            ..
        }
    ));
}
