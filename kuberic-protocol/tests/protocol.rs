use std::collections::BTreeMap;

use kuberic_protocol::command::{KubernetesChange, ProtocolCommand, SafetyChange};
use kuberic_protocol::evaluator::{EvaluationConfig, evaluate};
use kuberic_protocol::observation::{
    AgentBuildReport, AgentObservation, AgentReport, DesiredState, KubernetesReplicaObservation,
    ObservationFailure, ObservationSnapshot, ReplicaObservation, ReplicaObservationKey,
    ReportWatermark, RoutingObservation, UninitializedAgentObservation,
};
use kuberic_protocol::plan::{Plan, UnsafeReason, WaitReason};
use kuberic_protocol::types::{
    AcceptedStatus, AcceptedTopology, AccessStatus, AgentGeneration, ConfigurationDescriptor,
    ConfigurationMember, EffectivePolicy, Epoch, OperationId, PodUid, ProcessSessionId,
    ProvisioningId, ProvisioningIntent, ProvisioningKind, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaRole, ResourceUid, TransitionIntent, TransitionKind,
    derive_agent_generation, derive_initialization_id, derive_transition_id,
};
use kuberic_protocol::validation::{
    ValidationError, validate_configuration, validate_transition_relationship,
};

fn identity(id: i64, instance: &str, generation: &str) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(instance),
        agent_generation: AgentGeneration::new(generation),
    }
}

fn observation_key(id: i64, instance: &str) -> ReplicaObservationKey {
    ReplicaObservationKey::new(ReplicaId::new(id), ReplicaInstanceId::new(instance))
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
        supporting_resources_ready: true,
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
            observation_key(id, &format!("pod-uid-{id}")),
            ReplicaObservation {
                kubernetes: Some(KubernetesReplicaObservation {
                    replica_id,
                    pod_name: format!("example-{id}"),
                    pod_uid: Some(PodUid::new(format!("pod-uid-{id}"))),
                    pvc_name: format!("example-{id}"),
                    pvc_uid: Some(PvcUid::new(format!("pvc-uid-{id}"))),
                    pod_ready: true,
                    peer_endpoint_ready: true,
                }),
                agent: AgentObservation::Uninitialized(UninitializedAgentObservation {
                    protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                    resource_uid: snapshot.resource_uid.clone(),
                    replica_id,
                    pod_uid: PodUid::new(format!("pod-uid-{id}")),
                    pvc_uid: PvcUid::new(format!("pvc-uid-{id}")),
                    process_session_id: ProcessSessionId::new(format!("session-{id}")),
                    report_sequence: 1,
                }),
            },
        );
    }
    snapshot.routing.service_present = true;
    snapshot
}

fn attest_stable_topology(
    snapshot: &mut ObservationSnapshot,
    configuration: &ConfigurationDescriptor,
) {
    for member in &configuration.members {
        snapshot.replicas.insert(
            ReplicaObservationKey::new(
                member.identity.replica_id,
                member.identity.instance_id.clone(),
            ),
            ReplicaObservation {
                kubernetes: None,
                agent: AgentObservation::Report(Box::new(AgentReport {
                    protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                    resource_uid: snapshot.resource_uid.clone(),
                    identity: member.identity.clone(),
                    process_session_id: ProcessSessionId::new(format!(
                        "session-{}",
                        member.identity.replica_id
                    )),
                    report_sequence: 1,
                    role: member.role,
                    write_status: if member.role == ReplicaRole::Primary {
                        AccessStatus::Granted
                    } else {
                        AccessStatus::NotPrimary
                    },
                    healthy: true,
                    epoch: configuration.epoch,
                    previous_configuration: None,
                    current_configuration: Some(configuration.clone()),
                    current_progress: 10,
                    committed_lsn: 10,
                    catch_up_capability: Some(10),
                    ..AgentReport::default()
                })),
            },
        );
    }
    snapshot.routing.write_target = configuration
        .members
        .iter()
        .find(|member| member.identity.replica_id == configuration.primary_id)
        .map(|member| member.identity.clone());
    snapshot.routing.service_present = true;
    snapshot.routing.unresolved_write_target = false;
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
fn transition_relationship_rejects_epoch_regression_and_data_loss_change() {
    let previous = ConfigurationDescriptor::new(
        Epoch::new(0, 5),
        ReplicaId::new(1),
        configuration().members,
        2,
    );
    let regressed = ConfigurationDescriptor::new(
        Epoch::new(0, 4),
        ReplicaId::new(1),
        previous.members.clone(),
        2,
    );
    let data_loss_changed = ConfigurationDescriptor::new(
        Epoch::new(1, 6),
        ReplicaId::new(1),
        previous.members.clone(),
        2,
    );
    let policy = EffectivePolicy::fixed(3, 10).unwrap();

    assert!(matches!(
        validate_transition_relationship(
            TransitionKind::Failover,
            Some(&previous),
            &regressed,
            &policy
        ),
        Err(ValidationError::TransitionEpochNotNewer)
    ));
    assert!(matches!(
        validate_transition_relationship(
            TransitionKind::Failover,
            Some(&previous),
            &data_loss_changed,
            &policy
        ),
        Err(ValidationError::TransitionDataLossChanged)
    ));
}

#[test]
fn replacement_relationship_requires_one_non_primary_incarnation_change() {
    let previous = ConfigurationDescriptor::new(
        Epoch::new(0, 5),
        ReplicaId::new(1),
        configuration().members,
        2,
    );
    let mut replacement_members = previous.members.clone();
    replacement_members
        .iter_mut()
        .find(|member| member.identity.replica_id == ReplicaId::new(3))
        .unwrap()
        .identity = identity(3, "replacement-pod", "replacement-generation");
    let current =
        ConfigurationDescriptor::new(Epoch::new(0, 6), ReplicaId::new(1), replacement_members, 2);
    let policy = EffectivePolicy::fixed(3, 10).unwrap();

    assert!(
        validate_transition_relationship(
            TransitionKind::Replacement,
            Some(&previous),
            &current,
            &policy
        )
        .is_ok()
    );
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
fn missing_replica_support_converges_before_authority_changes() {
    let mut snapshot = scaffolded_snapshot();
    snapshot.supporting_resources_ready = false;
    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Apply { changes }
            if changes == vec![KubernetesChange::EnsureReplicaSupport]
    ));
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
    snapshot
        .replicas
        .get_mut(&observation_key(1, "pod-uid-1"))
        .unwrap()
        .agent = AgentObservation::Invalid {
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
        observation_key(1, "pod-uid-1"),
        ReportWatermark {
            process_session_id: ProcessSessionId::new("session-1"),
            report_sequence: 4,
        },
    );
    snapshot
        .replicas
        .get_mut(&observation_key(1, "pod-uid-1"))
        .unwrap()
        .agent = AgentObservation::Uninitialized(UninitializedAgentObservation {
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
fn bootstrap_installs_full_genesis_accepts_topology_then_grants_writes() {
    let mut snapshot = scaffolded_snapshot();
    let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("expected bootstrap persistence");
    };
    let KubernetesChange::PersistStatus { status } = changes[0].clone() else {
        panic!("expected persisted bootstrap status");
    };
    snapshot.status = *status;
    let transition = snapshot.status.transition.clone().unwrap();

    for member in &transition.current_configuration.members {
        let observation = snapshot
            .replicas
            .get_mut(&ReplicaObservationKey::new(
                member.identity.replica_id,
                member.identity.instance_id.clone(),
            ))
            .unwrap();
        observation.agent = AgentObservation::Report(Box::new(AgentReport {
            protocol_version: kuberic_protocol::PROTOCOL_VERSION,
            resource_uid: snapshot.resource_uid.clone(),
            identity: member.identity.clone(),
            process_session_id: ProcessSessionId::new(format!(
                "initialized-{}",
                member.identity.replica_id
            )),
            report_sequence: 1,
            role: ReplicaRole::None,
            read_status: AccessStatus::NotPrimary,
            write_status: AccessStatus::NotPrimary,
            healthy: true,
            ..AgentReport::default()
        }));
    }

    let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("expected transition condition persistence");
    };
    let KubernetesChange::PersistStatus { status } = changes[0].clone() else {
        panic!("expected transition status");
    };
    snapshot.status = *status;

    for member in &transition.current_configuration.members {
        let Plan::Execute {
            command: ProtocolCommand::EnsureConfiguration(command),
        } = evaluate(&snapshot, &EvaluationConfig::default())
        else {
            panic!("expected write-closed genesis installation");
        };
        assert!(!command.grant_write);
        assert_eq!(command.local_replica_id, member.identity.replica_id);
        let observation = snapshot
            .observation_for_identity(&member.identity)
            .unwrap()
            .clone();
        let AgentObservation::Report(mut report) = observation.agent else {
            panic!("initialized report");
        };
        report.role = member.role;
        report.read_status = AccessStatus::Granted;
        report.write_status = if member.role == ReplicaRole::Primary {
            AccessStatus::ReconfigurationPending
        } else {
            AccessStatus::NotPrimary
        };
        report.epoch = transition.current_configuration.epoch;
        report.current_configuration = Some(transition.current_configuration.clone());
        report.retained_operation_id = Some(OperationId::new(format!(
            "{}:install:{}",
            transition.transition_id, member.identity.replica_id
        )));
        snapshot
            .replicas
            .get_mut(&ReplicaObservationKey::new(
                member.identity.replica_id,
                member.identity.instance_id.clone(),
            ))
            .unwrap()
            .agent = AgentObservation::Report(report);
    }

    let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("expected atomic topology acceptance");
    };
    let KubernetesChange::PersistStatus { status } = changes[0].clone() else {
        panic!("expected accepted topology status");
    };
    assert!(status.initialized);
    assert!(status.transition.is_none());
    assert_eq!(
        status
            .topology
            .as_ref()
            .unwrap()
            .configuration
            .members
            .len(),
        3
    );
    snapshot.status = *status;

    let Plan::Execute {
        command: ProtocolCommand::EnsureConfiguration(command),
    } = evaluate(&snapshot, &EvaluationConfig::default())
    else {
        panic!("expected separate write grant");
    };
    assert!(command.grant_write);
    assert_eq!(command.local_replica_id, ReplicaId::new(1));
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

    let first_identity = snapshot
        .status
        .transition
        .as_ref()
        .unwrap()
        .current_configuration
        .members[0]
        .identity
        .clone();
    let observation = snapshot
        .replicas
        .get_mut(&observation_key(1, "pod-uid-1"))
        .unwrap();
    observation.agent = AgentObservation::Uninitialized(UninitializedAgentObservation {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        resource_uid: snapshot.resource_uid.clone(),
        replica_id: ReplicaId::new(1),
        pod_uid: PodUid::new("pod-uid-1"),
        pvc_uid: PvcUid::new("pvc-uid-1"),
        process_session_id: ProcessSessionId::new("session-1"),
        report_sequence: 1,
    });

    let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("expected transition condition persistence");
    };
    let KubernetesChange::PersistStatus { status } = changes[0].clone() else {
        panic!("expected persisted transition status");
    };
    snapshot.status = *status;

    let Plan::Execute {
        command: ProtocolCommand::InitializeAgentStore(command),
    } = evaluate(&snapshot, &EvaluationConfig::default())
    else {
        panic!("expected store initialization");
    };
    assert_eq!(
        command.assigned_agent_generation,
        first_identity.agent_generation
    );
    assert_eq!(command.expected_instance_id, first_identity.instance_id);
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
fn bootstrap_validates_all_members_before_initializing_any_store() {
    let mut snapshot = scaffolded_snapshot();
    let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("expected bootstrap persistence");
    };
    let KubernetesChange::PersistStatus { status } = changes[0].clone() else {
        panic!("expected persisted status");
    };
    snapshot.status = *status;

    snapshot
        .replicas
        .get_mut(&observation_key(1, "pod-uid-1"))
        .unwrap()
        .agent = AgentObservation::Uninitialized(UninitializedAgentObservation {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        resource_uid: snapshot.resource_uid.clone(),
        replica_id: ReplicaId::new(1),
        pod_uid: PodUid::new("pod-uid-1"),
        pvc_uid: PvcUid::new("pvc-uid-1"),
        process_session_id: ProcessSessionId::new("session-1"),
        report_sequence: 1,
    });
    snapshot
        .replicas
        .get_mut(&observation_key(2, "pod-uid-2"))
        .unwrap()
        .agent = AgentObservation::Report(Box::new(AgentReport {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        resource_uid: snapshot.resource_uid.clone(),
        identity: identity(2, "pod-uid-2", "conflicting-generation"),
        process_session_id: ProcessSessionId::new("session-2"),
        report_sequence: 1,
        role: ReplicaRole::None,
        write_status: AccessStatus::NotPrimary,
        healthy: true,
        epoch: Epoch::default(),
        previous_configuration: None,
        current_configuration: None,
        current_progress: 0,
        committed_lsn: 0,
        catch_up_capability: None,
        ..AgentReport::default()
    }));

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Unsafe {
            reason: UnsafeReason::InvalidAcceptedAuthority(_),
            ..
        }
    ));
}

#[test]
fn provisioning_observation_can_coexist_with_accepted_incarnation() {
    let accepted = configuration();
    let mut snapshot = empty_snapshot(3);
    let old_identity = accepted
        .members
        .iter()
        .find(|member| member.identity.replica_id == ReplicaId::new(3))
        .unwrap()
        .identity
        .clone();
    let initialization_id = kuberic_protocol::types::derive_initialization_id(
        &snapshot.resource_uid,
        ReplicaId::new(3),
        &PodUid::new("replacement-pod"),
        &PvcUid::new("replacement-pvc"),
    );
    let replacement_identity = ReplicaIdentity {
        replica_id: ReplicaId::new(3),
        instance_id: ReplicaInstanceId::new("replacement-pod"),
        agent_generation: derive_agent_generation(&initialization_id),
    };
    snapshot.status = AcceptedStatus {
        initialized: true,
        effective_policy: Some(EffectivePolicy::fixed(3, 10).unwrap()),
        topology: Some(AcceptedTopology {
            configuration: accepted.clone(),
        }),
        provisioning: Some(ProvisioningIntent {
            provisioning_id: ProvisioningId::new("provisioning"),
            kind: ProvisioningKind::Replacement,
            resource_uid: snapshot.resource_uid.clone(),
            replaces: old_identity.clone(),
            replica_id: ReplicaId::new(3),
            instance_id: replacement_identity.instance_id.clone(),
            pod_uid: PodUid::new("replacement-pod"),
            pvc_uid: PvcUid::new("replacement-pvc"),
            initialization_id,
            assigned_agent_generation: replacement_identity.agent_generation.clone(),
            operation_id: OperationId::new("replacement-operation"),
            started_at_unix_seconds: 100,
        }),
        ..AcceptedStatus::default()
    };
    snapshot.replicas.insert(
        ReplicaObservationKey::new(old_identity.replica_id, old_identity.instance_id.clone()),
        ReplicaObservation {
            kubernetes: None,
            agent: AgentObservation::Report(Box::new(AgentReport {
                protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                resource_uid: snapshot.resource_uid.clone(),
                identity: old_identity,
                process_session_id: ProcessSessionId::new("old-session"),
                report_sequence: 1,
                role: ReplicaRole::ActiveSecondary,
                write_status: AccessStatus::NotPrimary,
                healthy: true,
                epoch: accepted.epoch,
                previous_configuration: None,
                current_configuration: Some(accepted.clone()),
                current_progress: 10,
                committed_lsn: 10,
                catch_up_capability: Some(10),
                ..AgentReport::default()
            })),
        },
    );
    snapshot.replicas.insert(
        ReplicaObservationKey::new(
            replacement_identity.replica_id,
            replacement_identity.instance_id.clone(),
        ),
        ReplicaObservation {
            kubernetes: None,
            agent: AgentObservation::Report(Box::new(AgentReport {
                protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                resource_uid: snapshot.resource_uid.clone(),
                identity: replacement_identity,
                process_session_id: ProcessSessionId::new("replacement-session"),
                report_sequence: 1,
                role: ReplicaRole::IdleSecondary,
                write_status: AccessStatus::NotPrimary,
                healthy: true,
                epoch: Epoch::default(),
                previous_configuration: None,
                current_configuration: None,
                current_progress: 0,
                committed_lsn: 0,
                catch_up_capability: None,
                ..AgentReport::default()
            })),
        },
    );

    assert_eq!(snapshot.replicas.len(), 2);
    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Wait {
            reason: WaitReason::ProvisioningInProgress,
            ..
        }
    ));
}

#[test]
fn unsupported_protocol_version_is_unsafe() {
    let mut snapshot = scaffolded_snapshot();
    let observation = snapshot
        .replicas
        .get_mut(&observation_key(1, "pod-uid-1"))
        .unwrap();
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
        build_id: None,
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
        effective_policy: Some(EffectivePolicy::fixed(3, 10).unwrap()),
        topology: Some(AcceptedTopology {
            configuration: configuration.clone(),
        }),
        ..AcceptedStatus::default()
    };
    attest_stable_topology(&mut snapshot, &configuration);

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
fn stable_topology_waits_without_attested_replica_evidence() {
    let configuration = configuration();
    let mut snapshot = empty_snapshot(3);
    snapshot.status = AcceptedStatus {
        initialized: true,
        effective_policy: Some(EffectivePolicy::fixed(3, 10).unwrap()),
        topology: Some(AcceptedTopology { configuration }),
        ..AcceptedStatus::default()
    };

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Wait {
            reason: WaitReason::AwaitingStableEvidence,
            ..
        }
    ));
}

#[test]
fn stable_topology_reconciles_routing_and_clears_resolved_conditions() {
    let configuration = configuration();
    let mut snapshot = empty_snapshot(3);
    snapshot.status = AcceptedStatus {
        initialized: true,
        effective_policy: Some(EffectivePolicy::fixed(3, 10).unwrap()),
        topology: Some(AcceptedTopology {
            configuration: configuration.clone(),
        }),
        conditions: vec![
            kuberic_protocol::types::StatusCondition {
                type_: "Unsafe".to_string(),
                status: kuberic_protocol::types::ConditionStatus::True,
                reason: "OldConflict".to_string(),
                message: "resolved".to_string(),
            },
            kuberic_protocol::types::StatusCondition {
                type_: "UnsupportedSpec".to_string(),
                status: kuberic_protocol::types::ConditionStatus::True,
                reason: "ReplicaCountImmutable".to_string(),
                message: "resolved".to_string(),
            },
        ],
        ..AcceptedStatus::default()
    };
    attest_stable_topology(&mut snapshot, &configuration);

    let Plan::Stable { status, .. } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("expected stable attested topology");
    };
    assert!(status.conditions.iter().any(|condition| {
        condition.type_ == "Ready"
            && condition.status == kuberic_protocol::types::ConditionStatus::True
    }));
    assert!(
        status
            .conditions
            .iter()
            .all(|condition| condition.type_ != "Unsafe" && condition.type_ != "UnsupportedSpec")
    );

    snapshot.routing.write_target = None;
    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Apply { changes }
            if matches!(
                changes.first(),
                Some(KubernetesChange::PublishWriteRouting { .. })
            )
    ));
}

#[test]
fn stale_replica_epoch_is_unsafe() {
    let configuration = configuration();
    let mut snapshot = empty_snapshot(3);
    snapshot.status = AcceptedStatus {
        initialized: true,
        effective_policy: Some(EffectivePolicy::fixed(3, 10).unwrap()),
        topology: Some(AcceptedTopology {
            configuration: configuration.clone(),
        }),
        ..AcceptedStatus::default()
    };
    snapshot.replicas.insert(
        observation_key(1, "pod-1"),
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
                healthy: true,
                epoch: Epoch::new(0, 0),
                previous_configuration: None,
                current_configuration: Some(configuration),
                current_progress: 1,
                committed_lsn: 1,
                catch_up_capability: Some(1),
                ..AgentReport::default()
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
        effective_policy: Some(EffectivePolicy::fixed(3, 10).unwrap()),
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
            ReplicaObservationKey::new(replica_id, expected_identity.instance_id.clone()),
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
                    healthy: true,
                    epoch: configuration.epoch,
                    previous_configuration: None,
                    current_configuration: Some(configuration.clone()),
                    current_progress: 1,
                    committed_lsn: 1,
                    catch_up_capability: Some(1),
                    ..AgentReport::default()
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
        effective_policy: Some(EffectivePolicy::fixed(3, 10).unwrap()),
        topology: Some(AcceptedTopology {
            configuration: configuration.clone(),
        }),
        ..AcceptedStatus::default()
    };
    snapshot.replicas.insert(
        observation_key(1, "replacement-pod"),
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
                healthy: true,
                epoch: configuration.epoch,
                previous_configuration: None,
                current_configuration: Some(configuration),
                current_progress: 1,
                committed_lsn: 1,
                catch_up_capability: Some(1),
                ..AgentReport::default()
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
fn wrong_generation_cannot_prove_ready_or_publish_routing() {
    let configuration = configuration();
    for routing_present in [false, true] {
        let mut snapshot = empty_snapshot(3);
        snapshot.status = AcceptedStatus {
            initialized: true,
            effective_policy: Some(EffectivePolicy::fixed(3, 10).unwrap()),
            topology: Some(AcceptedTopology {
                configuration: configuration.clone(),
            }),
            ..AcceptedStatus::default()
        };
        attest_stable_topology(&mut snapshot, &configuration);
        if !routing_present {
            snapshot.routing.write_target = None;
        }
        for replica_id in [ReplicaId::new(2), ReplicaId::new(3)] {
            let member = configuration
                .members
                .iter()
                .find(|member| member.identity.replica_id == replica_id)
                .unwrap();
            let observation = snapshot
                .replicas
                .get_mut(&ReplicaObservationKey::new(
                    replica_id,
                    member.identity.instance_id.clone(),
                ))
                .unwrap();
            let AgentObservation::Report(report) = &mut observation.agent else {
                unreachable!();
            };
            report.identity.agent_generation =
                AgentGeneration::new(format!("wrong-generation-{}", replica_id.value()));
        }

        assert!(matches!(
            evaluate(&snapshot, &EvaluationConfig::default()),
            Plan::Unsafe {
                reason: UnsafeReason::InvalidAcceptedAuthority(_),
                safety_changes,
                ..
            } if safety_changes == vec![SafetyChange::RemoveWriteRouting]
        ));
    }
}

#[test]
fn accepted_incarnation_missing_its_store_is_unsafe() {
    let configuration = configuration();
    let mut snapshot = empty_snapshot(3);
    snapshot.status = AcceptedStatus {
        initialized: true,
        effective_policy: Some(EffectivePolicy::fixed(3, 10).unwrap()),
        topology: Some(AcceptedTopology {
            configuration: configuration.clone(),
        }),
        ..AcceptedStatus::default()
    };
    let primary = &configuration.members[0].identity;
    snapshot.replicas.insert(
        ReplicaObservationKey::new(primary.replica_id, primary.instance_id.clone()),
        ReplicaObservation {
            kubernetes: Some(KubernetesReplicaObservation {
                replica_id: primary.replica_id,
                pod_name: "primary".to_string(),
                pod_uid: Some(PodUid::new(primary.instance_id.as_str())),
                pvc_name: "primary".to_string(),
                pvc_uid: Some(PvcUid::new("established-pvc")),
                pod_ready: true,
                peer_endpoint_ready: true,
            }),
            agent: AgentObservation::Uninitialized(UninitializedAgentObservation {
                protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                resource_uid: snapshot.resource_uid.clone(),
                replica_id: primary.replica_id,
                pod_uid: PodUid::new(primary.instance_id.as_str()),
                pvc_uid: PvcUid::new("established-pvc"),
                process_session_id: ProcessSessionId::new("lost-store-session"),
                report_sequence: 1,
            }),
        },
    );
    snapshot.routing.write_target = Some(primary.clone());

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Unsafe {
            reason: UnsafeReason::InvalidAcceptedAuthority(_),
            safety_changes,
            ..
        } if safety_changes == vec![SafetyChange::RemoveWriteRouting]
    ));
}

#[test]
fn bootstrap_prevalidates_later_uninitialized_fences() {
    let mut snapshot = scaffolded_snapshot();
    let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("expected bootstrap persistence");
    };
    let KubernetesChange::PersistStatus { status } = changes[0].clone() else {
        panic!("expected persisted status");
    };
    snapshot.status = *status;

    for id in [1, 2] {
        let observation = snapshot
            .replicas
            .get_mut(&observation_key(id, &format!("pod-uid-{id}")))
            .unwrap();
        let pvc_uid = if id == 2 {
            PvcUid::new("different-pvc")
        } else {
            PvcUid::new("pvc-uid-1")
        };
        observation.kubernetes.as_mut().unwrap().pvc_uid = Some(pvc_uid.clone());
        observation.agent = AgentObservation::Uninitialized(UninitializedAgentObservation {
            protocol_version: kuberic_protocol::PROTOCOL_VERSION,
            resource_uid: snapshot.resource_uid.clone(),
            replica_id: ReplicaId::new(id),
            pod_uid: PodUid::new(format!("pod-uid-{id}")),
            pvc_uid,
            process_session_id: ProcessSessionId::new(format!("session-{id}")),
            report_sequence: 1,
        });
    }

    fn verify_replacement_provisions_exact_target_builds_then_freezes_pc_cc() {
        let accepted = configuration();
        let replacing = accepted
            .members
            .iter()
            .find(|member| member.identity.replica_id != accepted.primary_id)
            .unwrap()
            .identity
            .clone();
        let mut snapshot = empty_snapshot(3);
        snapshot.status = AcceptedStatus {
            initialized: true,
            effective_policy: Some(EffectivePolicy::fixed(3, 10).unwrap()),
            topology: Some(AcceptedTopology {
                configuration: accepted.clone(),
            }),
            ..AcceptedStatus::default()
        };
        snapshot.replicas.insert(
            ReplicaObservationKey::new(
                replacing.replica_id,
                ReplicaInstanceId::new("orphan-old-pvc"),
            ),
            ReplicaObservation {
                kubernetes: Some(KubernetesReplicaObservation {
                    replica_id: replacing.replica_id,
                    pod_name: String::new(),
                    pod_uid: None,
                    pvc_name: "old-data".to_string(),
                    pvc_uid: Some(PvcUid::new("old-pvc")),
                    pod_ready: false,
                    peer_endpoint_ready: false,
                }),
                agent: AgentObservation::Absent,
            },
        );
        assert!(matches!(
            evaluate(&snapshot, &EvaluationConfig::default()),
            Plan::Apply { changes }
                if matches!(
                    changes.as_slice(),
                    [KubernetesChange::EnsureReplacementScaffolding { replica_id, replacing: old }]
                        if *replica_id == replacing.replica_id && old == &replacing
                )
        ));

        let pod_uid = PodUid::new("replacement-pod");
        let pvc_uid = PvcUid::new("replacement-pvc");
        let initialization_id = kuberic_protocol::types::derive_initialization_id(
            &snapshot.resource_uid,
            replacing.replica_id,
            &pod_uid,
            &pvc_uid,
        );
        let target = ReplicaIdentity {
            replica_id: replacing.replica_id,
            instance_id: ReplicaInstanceId::new(pod_uid.as_str()),
            agent_generation: derive_agent_generation(&initialization_id),
        };
        snapshot.replicas.insert(
            ReplicaObservationKey::new(target.replica_id, target.instance_id.clone()),
            ReplicaObservation {
                kubernetes: Some(KubernetesReplicaObservation {
                    replica_id: target.replica_id,
                    pod_name: "replacement".to_string(),
                    pod_uid: Some(pod_uid.clone()),
                    pvc_name: "replacement-data".to_string(),
                    pvc_uid: Some(pvc_uid.clone()),
                    pod_ready: true,
                    peer_endpoint_ready: true,
                }),
                agent: AgentObservation::Uninitialized(UninitializedAgentObservation {
                    protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                    resource_uid: snapshot.resource_uid.clone(),
                    replica_id: target.replica_id,
                    pod_uid,
                    pvc_uid,
                    process_session_id: ProcessSessionId::new("replacement-session"),
                    report_sequence: 1,
                }),
            },
        );
        let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
            panic!("replacement target must be persisted outside authority");
        };
        let provisioning_status = changes
            .into_iter()
            .find_map(|change| match change {
                KubernetesChange::PersistStatus { status } => Some(*status),
                _ => None,
            })
            .expect("replacement provisioning status");
        let provisioning = provisioning_status.provisioning.clone().unwrap();
        assert_eq!(provisioning.replaces, replacing);
        assert_eq!(provisioning.instance_id, target.instance_id);

        snapshot.status = provisioning_status;
        let primary = accepted
            .members
            .iter()
            .find(|member| member.identity.replica_id == accepted.primary_id)
            .unwrap()
            .identity
            .clone();
        let build = AgentBuildReport {
            build_id: provisioning.operation_id.clone(),
            target: target.clone(),
            last_sequence: 1,
            durable_lsn: 5,
            completed: true,
        };
        snapshot.replicas.insert(
            ReplicaObservationKey::new(primary.replica_id, primary.instance_id.clone()),
            ReplicaObservation {
                kubernetes: None,
                agent: AgentObservation::Report(Box::new(AgentReport {
                    protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                    resource_uid: snapshot.resource_uid.clone(),
                    identity: primary,
                    process_session_id: ProcessSessionId::new("primary-session"),
                    report_sequence: 1,
                    role: ReplicaRole::Primary,
                    read_status: AccessStatus::Granted,
                    write_status: AccessStatus::Granted,
                    healthy: true,
                    epoch: accepted.epoch,
                    current_configuration: Some(accepted.clone()),
                    current_progress: 5,
                    committed_lsn: 5,
                    builds: vec![build.clone()],
                    ..AgentReport::default()
                })),
            },
        );
        snapshot.replicas.insert(
            ReplicaObservationKey::new(target.replica_id, target.instance_id.clone()),
            ReplicaObservation {
                kubernetes: None,
                agent: AgentObservation::Report(Box::new(AgentReport {
                    protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                    resource_uid: snapshot.resource_uid.clone(),
                    identity: target.clone(),
                    process_session_id: ProcessSessionId::new("target-session"),
                    report_sequence: 2,
                    role: ReplicaRole::IdleSecondary,
                    write_status: AccessStatus::NotPrimary,
                    healthy: true,
                    builds: vec![build],
                    ..AgentReport::default()
                })),
            },
        );
        let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
            panic!("completed build must freeze replacement PC/CC");
        };
        let transition = changes.into_iter().find_map(|change| match change {
            KubernetesChange::PersistStatus { status } => status.transition,
            _ => None,
        });
        let transition = transition.expect("replacement transition");
        assert_eq!(transition.kind, TransitionKind::Replacement);
        assert_eq!(transition.current_configuration.members.len(), 3);
        assert!(
            transition
                .current_configuration
                .members
                .iter()
                .any(|member| member.identity == target)
        );
    }
    verify_replacement_provisions_exact_target_builds_then_freezes_pc_cc();

    let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("expected transition condition persistence");
    };
    let KubernetesChange::PersistStatus { status } = changes[0].clone() else {
        panic!("expected persisted transition status");
    };
    snapshot.status = *status;

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Unsafe {
            reason: UnsafeReason::ContradictoryReplicaEvidence(_),
            ..
        }
    ));
}

#[test]
fn transition_report_previous_configuration_must_match_frozen_topology() {
    let previous = ConfigurationDescriptor::new(
        Epoch::new(0, 5),
        ReplicaId::new(1),
        configuration().members,
        2,
    );
    let mut current_members = previous.members.clone();
    current_members[2].identity = identity(3, "replacement-pod", "replacement-generation");
    let current =
        ConfigurationDescriptor::new(Epoch::new(0, 6), ReplicaId::new(1), current_members, 2);
    let mut unauthorized_previous_members = previous.members.clone();
    unauthorized_previous_members[1].identity =
        identity(2, "unauthorized-pod", "unauthorized-generation");
    let unauthorized_previous = ConfigurationDescriptor::new(
        previous.epoch,
        previous.primary_id,
        unauthorized_previous_members,
        previous.write_quorum,
    );
    let policy = EffectivePolicy::fixed(3, 10).unwrap();
    let mut snapshot = empty_snapshot(3);
    snapshot.status = AcceptedStatus {
        initialized: true,
        effective_policy: Some(EffectivePolicy::fixed(3, 10).unwrap()),
        topology: Some(AcceptedTopology {
            configuration: previous.clone(),
        }),
        transition: Some(TransitionIntent {
            transition_id: derive_transition_id(
                &snapshot.resource_uid,
                TransitionKind::Replacement,
                &current.configuration_id,
            ),
            kind: TransitionKind::Replacement,
            spec_generation: 1,
            effective_policy: policy,
            previous_configuration_id: Some(previous.configuration_id.clone()),
            current_configuration: current.clone(),
            build_id: Some(OperationId::new("replacement-build")),
            started_at_unix_seconds: 100,
        }),
        ..AcceptedStatus::default()
    };
    let replacement = current.members[2].clone();
    snapshot.replicas.insert(
        ReplicaObservationKey::new(
            replacement.identity.replica_id,
            replacement.identity.instance_id.clone(),
        ),
        ReplicaObservation {
            kubernetes: None,
            agent: AgentObservation::Report(Box::new(AgentReport {
                protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                resource_uid: snapshot.resource_uid.clone(),
                identity: replacement.identity,
                process_session_id: ProcessSessionId::new("replacement-session"),
                report_sequence: 1,
                role: replacement.role,
                write_status: AccessStatus::NotPrimary,
                healthy: true,
                epoch: current.epoch,
                previous_configuration: Some(unauthorized_previous),
                current_configuration: Some(current),
                current_progress: 1,
                committed_lsn: 1,
                catch_up_capability: Some(1),
                ..AgentReport::default()
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
fn bootstrap_report_cannot_claim_previous_configuration() {
    let mut snapshot = scaffolded_snapshot();
    let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("expected bootstrap persistence");
    };
    let KubernetesChange::PersistStatus { status } = changes[0].clone() else {
        panic!("expected persisted status");
    };
    snapshot.status = *status;
    let transition = snapshot.status.transition.as_ref().unwrap();
    let member = transition.current_configuration.members[0].clone();
    snapshot
        .replicas
        .get_mut(&observation_key(1, "pod-uid-1"))
        .unwrap()
        .agent = AgentObservation::Report(Box::new(AgentReport {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        resource_uid: snapshot.resource_uid.clone(),
        identity: member.identity,
        process_session_id: ProcessSessionId::new("bootstrap-session"),
        report_sequence: 1,
        role: member.role,
        write_status: AccessStatus::ReconfigurationPending,
        healthy: true,
        epoch: transition.current_configuration.epoch,
        previous_configuration: Some(configuration()),
        current_configuration: Some(transition.current_configuration.clone()),
        current_progress: 0,
        committed_lsn: 0,
        catch_up_capability: Some(0),
        ..AgentReport::default()
    }));

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Unsafe {
            reason: UnsafeReason::InvalidAcceptedAuthority(_),
            ..
        }
    ));
}

#[test]
fn replacement_cleanup_does_not_replace_a_healthy_accepted_incarnation() {
    let accepted = configuration();
    let member = accepted
        .members
        .iter()
        .find(|member| member.identity.replica_id != accepted.primary_id)
        .unwrap()
        .clone();
    let mut snapshot = empty_snapshot(3);
    snapshot.status = AcceptedStatus {
        initialized: true,
        effective_policy: Some(EffectivePolicy::fixed(3, 10).unwrap()),
        topology: Some(AcceptedTopology {
            configuration: accepted.clone(),
        }),
        ..AcceptedStatus::default()
    };
    snapshot.replicas.insert(
        ReplicaObservationKey::new(
            member.identity.replica_id,
            member.identity.instance_id.clone(),
        ),
        ReplicaObservation {
            kubernetes: Some(KubernetesReplicaObservation {
                replica_id: member.identity.replica_id,
                pod_name: "replacement".to_string(),
                pod_uid: Some(PodUid::new(member.identity.instance_id.as_str())),
                pvc_name: "replacement-data".to_string(),
                pvc_uid: Some(PvcUid::new("replacement-pvc")),
                pod_ready: true,
                peer_endpoint_ready: true,
            }),
            agent: AgentObservation::Report(Box::new(AgentReport {
                protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                resource_uid: snapshot.resource_uid.clone(),
                identity: member.identity.clone(),
                process_session_id: ProcessSessionId::new("replacement-session"),
                report_sequence: 1,
                role: member.role,
                write_status: AccessStatus::NotPrimary,
                healthy: true,
                epoch: accepted.epoch,
                current_configuration: Some(accepted),
                ..AgentReport::default()
            })),
        },
    );
    snapshot.replicas.insert(
        ReplicaObservationKey::new(
            member.identity.replica_id,
            ReplicaInstanceId::new("orphan-old-pvc"),
        ),
        ReplicaObservation {
            kubernetes: Some(KubernetesReplicaObservation {
                replica_id: member.identity.replica_id,
                pod_name: String::new(),
                pod_uid: None,
                pvc_name: "old-data".to_string(),
                pvc_uid: Some(PvcUid::new("old-pvc")),
                pod_ready: false,
                peer_endpoint_ready: false,
            }),
            agent: AgentObservation::Absent,
        },
    );

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Apply { changes }
            if matches!(
                changes.as_slice(),
                [KubernetesChange::DeleteReplicaScaffolding { pvc_name: Some(name), .. }]
                    if name == "old-data"
            )
    ));
}

#[test]
fn replacement_target_loss_before_cc_clears_provisioning() {
    let accepted = configuration();
    let replacing = accepted
        .members
        .iter()
        .find(|member| member.identity.replica_id != accepted.primary_id)
        .unwrap()
        .identity
        .clone();
    let pod_uid = PodUid::new("lost-target");
    let pvc_uid = PvcUid::new("lost-target-pvc");
    let initialization_id = derive_initialization_id(
        &ResourceUid::new("resource-uid"),
        replacing.replica_id,
        &pod_uid,
        &pvc_uid,
    );
    let mut snapshot = empty_snapshot(3);
    snapshot.status = AcceptedStatus {
        initialized: true,
        effective_policy: Some(EffectivePolicy::fixed(3, 10).unwrap()),
        topology: Some(AcceptedTopology {
            configuration: accepted,
        }),
        provisioning: Some(ProvisioningIntent {
            provisioning_id: ProvisioningId::new("lost-provisioning"),
            kind: ProvisioningKind::Replacement,
            resource_uid: snapshot.resource_uid.clone(),
            replaces: replacing.clone(),
            replica_id: replacing.replica_id,
            instance_id: ReplicaInstanceId::new(pod_uid.as_str()),
            pod_uid,
            pvc_uid,
            initialization_id: initialization_id.clone(),
            assigned_agent_generation: derive_agent_generation(&initialization_id),
            operation_id: OperationId::new("lost-build"),
            started_at_unix_seconds: 100,
        }),
        ..AcceptedStatus::default()
    };

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Apply { changes }
            if matches!(
                changes.as_slice(),
                [KubernetesChange::PersistStatus { status }]
                    if status.provisioning.is_none() && status.transition.is_none()
            )
    ));
}

#[test]
fn replacement_accepts_current_only_quorum_with_missing_target() {
    let previous = configuration();
    let replacing = previous
        .members
        .iter()
        .find(|member| member.identity.replica_id != previous.primary_id)
        .unwrap()
        .clone();
    let target = ConfigurationMember {
        identity: identity(
            replacing.identity.replica_id.value(),
            "missing-target",
            "missing-target-generation",
        ),
        role: ReplicaRole::ActiveSecondary,
    };
    let current = ConfigurationDescriptor::new(
        Epoch::new(
            previous.epoch.data_loss_number,
            previous.epoch.configuration_number + 1,
        ),
        previous.primary_id,
        previous
            .members
            .iter()
            .map(|member| {
                if member.identity == replacing.identity {
                    target.clone()
                } else {
                    member.clone()
                }
            })
            .collect(),
        previous.write_quorum,
    );
    let transition_id = derive_transition_id(
        &ResourceUid::new("resource"),
        TransitionKind::Replacement,
        &current.configuration_id,
    );
    let mut snapshot = empty_snapshot(3);
    snapshot.status = AcceptedStatus {
        initialized: true,
        effective_policy: Some(EffectivePolicy::fixed(3, 10).unwrap()),
        topology: Some(AcceptedTopology {
            configuration: previous.clone(),
        }),
        transition: Some(TransitionIntent {
            transition_id: transition_id.clone(),
            kind: TransitionKind::Replacement,
            spec_generation: 1,
            effective_policy: EffectivePolicy::fixed(3, 10).unwrap(),
            previous_configuration_id: Some(previous.configuration_id.clone()),
            current_configuration: current.clone(),
            build_id: Some(OperationId::new("replacement-build")),
            started_at_unix_seconds: 100,
        }),
        ..AcceptedStatus::default()
    };
    for member in current
        .members
        .iter()
        .filter(|member| member.identity != target.identity)
    {
        snapshot.replicas.insert(
            ReplicaObservationKey::new(
                member.identity.replica_id,
                member.identity.instance_id.clone(),
            ),
            ReplicaObservation {
                kubernetes: None,
                agent: AgentObservation::Report(Box::new(AgentReport {
                    protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                    resource_uid: snapshot.resource_uid.clone(),
                    identity: member.identity.clone(),
                    process_session_id: ProcessSessionId::new(format!(
                        "session-{}",
                        member.identity.replica_id
                    )),
                    report_sequence: 1,
                    role: member.role,
                    read_status: AccessStatus::Granted,
                    write_status: if member.identity.replica_id == current.primary_id {
                        AccessStatus::Granted
                    } else {
                        AccessStatus::NotPrimary
                    },
                    healthy: true,
                    epoch: current.epoch,
                    current_configuration: Some(current.clone()),
                    retained_operation_id: Some(OperationId::new(format!(
                        "{}:current-only:{}",
                        transition_id, member.identity.replica_id
                    ))),
                    ..AgentReport::default()
                })),
            },
        );
    }
    let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("transition condition must be persisted");
    };
    let KubernetesChange::PersistStatus { status } = changes[0].clone() else {
        panic!("transition status");
    };
    snapshot.status = *status;

    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Apply { changes }
            if changes.iter().any(|change| matches!(
                change,
                KubernetesChange::PersistStatus { status }
                    if status.transition.is_none()
                        && status.topology.as_ref().is_some_and(|topology| {
                            topology.configuration == current
                        })
            ))
    ));
}

#[test]
fn bootstrap_replacement_supersedes_only_a_never_installed_incarnation() {
    let mut snapshot = scaffolded_snapshot();
    let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("bootstrap transition");
    };
    let KubernetesChange::PersistStatus { status } = changes[0].clone() else {
        panic!("bootstrap status");
    };
    snapshot.status = *status;
    let old = snapshot
        .status
        .transition
        .as_ref()
        .unwrap()
        .current_configuration
        .members[1]
        .identity
        .clone();
    snapshot.replicas.remove(&ReplicaObservationKey::new(
        old.replica_id,
        old.instance_id.clone(),
    ));
    snapshot.replicas.insert(
        ReplicaObservationKey::new(old.replica_id, ReplicaInstanceId::new("orphan-genesis")),
        ReplicaObservation {
            kubernetes: Some(KubernetesReplicaObservation {
                replica_id: old.replica_id,
                pod_name: String::new(),
                pod_uid: None,
                pvc_name: "old-genesis-data".to_string(),
                pvc_uid: Some(PvcUid::new("old-genesis-pvc")),
                pod_ready: false,
                peer_endpoint_ready: false,
            }),
            agent: AgentObservation::Absent,
        },
    );
    let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("transition condition persistence");
    };
    let KubernetesChange::PersistStatus { status } = changes[0].clone() else {
        panic!("transition status");
    };
    snapshot.status = *status;
    assert!(matches!(
        evaluate(&snapshot, &EvaluationConfig::default()),
        Plan::Apply { changes }
            if matches!(
                changes.as_slice(),
                [KubernetesChange::EnsureReplacementScaffolding { replacing, .. }]
                    if replacing == &old
            )
    ));

    let pod_uid = PodUid::new("new-genesis-pod");
    let pvc_uid = PvcUid::new("new-genesis-pvc");
    snapshot.replicas.insert(
        ReplicaObservationKey::new(old.replica_id, ReplicaInstanceId::new(pod_uid.as_str())),
        ReplicaObservation {
            kubernetes: Some(KubernetesReplicaObservation {
                replica_id: old.replica_id,
                pod_name: "new-genesis".to_string(),
                pod_uid: Some(pod_uid.clone()),
                pvc_name: "new-genesis-data".to_string(),
                pvc_uid: Some(pvc_uid.clone()),
                pod_ready: true,
                peer_endpoint_ready: true,
            }),
            agent: AgentObservation::Uninitialized(UninitializedAgentObservation {
                protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                resource_uid: snapshot.resource_uid.clone(),
                replica_id: old.replica_id,
                pod_uid,
                pvc_uid,
                process_session_id: ProcessSessionId::new("new-genesis-session"),
                report_sequence: 1,
            }),
        },
    );
    let Plan::Apply { changes } = evaluate(&snapshot, &EvaluationConfig::default()) else {
        panic!("superseded bootstrap transition");
    };
    let transition = changes.into_iter().find_map(|change| match change {
        KubernetesChange::PersistStatus { status } => status.transition,
        _ => None,
    });
    let transition = transition.expect("superseded transition");
    assert_ne!(transition.current_configuration.members[1].identity, old);
}
