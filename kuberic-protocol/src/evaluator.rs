//! Deterministic reconciliation decisions over a normalized observation.

use crate::command::{InitializeAgentStore, KubernetesChange, ProtocolCommand, SafetyChange};
use crate::observation::{AgentObservation, ObservationSnapshot};
use crate::plan::{Plan, UnsafeReason, WaitReason};
use crate::types::{
    AcceptedStatus, AccessStatus, ConditionStatus, ConfigurationDescriptor, ConfigurationMember,
    EffectivePolicy, Epoch, ReplicaIdentity, ReplicaInstanceId, ReplicaRole, StatusCondition,
    TransitionIntent, TransitionKind, derive_agent_generation, derive_initialization_id,
    derive_transition_id,
};
use crate::validation::ValidationError;
use crate::validation::validate_snapshot;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationConfig {
    pub supported_protocol_version: u32,
    pub stable_resync_seconds: u64,
    pub wait_requeue_seconds: u64,
    pub unsafe_requeue_seconds: u64,
}

impl Default for EvaluationConfig {
    fn default() -> Self {
        Self {
            supported_protocol_version: crate::PROTOCOL_VERSION,
            stable_resync_seconds: 30,
            wait_requeue_seconds: 5,
            unsafe_requeue_seconds: 30,
        }
    }
}

/// Validates one snapshot and returns the next safe reconciliation outcome.
pub fn evaluate(snapshot: &ObservationSnapshot, config: &EvaluationConfig) -> Plan {
    if let Err(error) = validate_snapshot(snapshot) {
        let reason = if error == ValidationError::DesiredReplicasZero {
            UnsafeReason::InvalidDesiredState(error.to_string())
        } else {
            UnsafeReason::InvalidAcceptedAuthority(error.to_string())
        };
        return unsafe_plan(snapshot.status.clone(), reason, config);
    }

    if let Some(plan) = incompatible_protocol_plan(snapshot, config) {
        return plan;
    }
    if let Some(plan) = invalid_agent_plan(snapshot, config) {
        return plan;
    }
    if !snapshot.observation_failures.is_empty() {
        return Plan::Wait {
            reason: WaitReason::AgentUnavailable,
            status: waiting_status(
                snapshot.status.clone(),
                "ObservationFailed",
                "A required observation failed",
            ),
            requeue_after_seconds: config.wait_requeue_seconds,
        };
    }

    if let Some(transition) = &snapshot.status.transition {
        return evaluate_transition(snapshot, transition, config);
    }

    if snapshot.status.provisioning.is_some() {
        return Plan::Wait {
            reason: WaitReason::ProvisioningInProgress,
            status: waiting_status(
                snapshot.status.clone(),
                "ProvisioningInProgress",
                "An exact replacement remains outside authority",
            ),
            requeue_after_seconds: config.wait_requeue_seconds,
        };
    }

    if snapshot.status.initialized {
        return evaluate_stable(snapshot, config);
    }

    evaluate_never_initialized(snapshot, config)
}

fn evaluate_stable(snapshot: &ObservationSnapshot, config: &EvaluationConfig) -> Plan {
    let topology = snapshot
        .status
        .topology
        .as_ref()
        .expect("validated initialized status has topology");
    let configuration = &topology.configuration;
    let frozen_size = configuration.members.len() as u32;
    let mut status = clear_evaluator_conditions(snapshot.status.clone());
    let unsupported = snapshot.desired.replicas != frozen_size;
    if unsupported {
        status = status.with_condition(unsupported_replica_count_condition(
            snapshot.desired.replicas,
            frozen_size,
        ));
    } else {
        status.observed_generation = snapshot.desired.generation;
    }

    let primary = configuration
        .members
        .iter()
        .find(|member| member.identity.replica_id == configuration.primary_id)
        .expect("validated configuration has primary member");
    let mut attested_members = 0_u32;
    let mut primary_attested = false;
    for member in &configuration.members {
        let Some(observation) = snapshot.observation_for_identity(&member.identity) else {
            return Plan::Wait {
                reason: WaitReason::AwaitingStableEvidence,
                status: waiting_status(
                    status,
                    "ReplicaEvidenceMissing",
                    "Accepted topology is not fully observed",
                ),
                requeue_after_seconds: config.wait_requeue_seconds,
            };
        };
        let AgentObservation::Report(report) = &observation.agent else {
            return Plan::Wait {
                reason: WaitReason::AwaitingStableEvidence,
                status: waiting_status(
                    status,
                    "ReplicaEvidenceMissing",
                    "Accepted member has no initialized report",
                ),
                requeue_after_seconds: config.wait_requeue_seconds,
            };
        };
        let authority_matches = report.identity == member.identity
            && report.healthy
            && report.role == member.role
            && report.epoch == configuration.epoch
            && report
                .current_configuration
                .as_ref()
                .is_some_and(|current| current.configuration_id == configuration.configuration_id);
        if authority_matches {
            attested_members += 1;
        }
        if member.identity == primary.identity {
            primary_attested = authority_matches
                && report.role == ReplicaRole::Primary
                && report.write_status == AccessStatus::Granted;
        }
    }

    if !primary_attested || attested_members < configuration.write_quorum {
        return Plan::Wait {
            reason: WaitReason::AwaitingStableEvidence,
            status: waiting_status(
                status,
                "WriteAuthorityUnproven",
                "Primary WriteStatus or Current Configuration quorum is not proven",
            ),
            requeue_after_seconds: config.wait_requeue_seconds,
        };
    }

    match &snapshot.routing.write_target {
        Some(target) if target == &primary.identity => {}
        Some(_) => {
            return Plan::Apply {
                changes: vec![
                    KubernetesChange::RemoveWriteRouting,
                    KubernetesChange::PersistStatus {
                        status: Box::new(waiting_status(
                            status,
                            "RoutingFencePending",
                            "Removing routing to a non-authoritative target",
                        )),
                    },
                ],
            };
        }
        None => {
            return Plan::Apply {
                changes: vec![
                    KubernetesChange::PublishWriteRouting {
                        primary: primary.identity.clone(),
                    },
                    KubernetesChange::PersistStatus {
                        status: Box::new(waiting_status(
                            status,
                            "RoutingPublicationPending",
                            "Publishing routing to the attested primary",
                        )),
                    },
                ],
            };
        }
    }
    status = status.with_condition(ready_condition());
    Plan::Stable {
        status,
        requeue_after_seconds: config.stable_resync_seconds,
    }
}

fn evaluate_never_initialized(snapshot: &ObservationSnapshot, config: &EvaluationConfig) -> Plan {
    if has_durable_replica_evidence(snapshot) {
        return unsafe_plan(
            snapshot.status.clone(),
            UnsafeReason::DurableEvidenceWithoutAuthority,
            config,
        );
    }

    let Some(policy) = EffectivePolicy::fixed(
        snapshot.desired.replicas,
        snapshot.desired.failover_delay_seconds,
    ) else {
        return unsafe_plan(
            snapshot.status.clone(),
            UnsafeReason::InvalidDesiredState(
                "desired replica count must be greater than zero".to_string(),
            ),
            config,
        );
    };

    if !snapshot.has_complete_scaffolding() {
        return Plan::Apply {
            changes: vec![KubernetesChange::EnsureReplicaScaffolding {
                replica_ids: snapshot.intended_replica_ids(),
            }],
        };
    }

    let primary_id = snapshot
        .intended_replica_ids()
        .into_iter()
        .min()
        .expect("positive replica count has a first member");
    let mut members = Vec::with_capacity(policy.replica_set_size as usize);
    for replica_id in snapshot.intended_replica_ids() {
        let kubernetes = snapshot
            .scaffolding_for(replica_id)
            .expect("complete scaffolding has every intended member");
        let pod_uid = kubernetes
            .pod_uid
            .as_ref()
            .expect("complete scaffolding has Pod UID");
        let pvc_uid = kubernetes
            .pvc_uid
            .as_ref()
            .expect("complete scaffolding has PVC UID");
        let initialization_id =
            derive_initialization_id(&snapshot.resource_uid, replica_id, pod_uid, pvc_uid);
        members.push(ConfigurationMember {
            identity: ReplicaIdentity {
                replica_id,
                instance_id: ReplicaInstanceId::new(pod_uid.as_str()),
                agent_generation: derive_agent_generation(&initialization_id),
            },
            role: if replica_id == primary_id {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
        });
    }

    let current_configuration =
        ConfigurationDescriptor::new(Epoch::new(0, 1), primary_id, members, policy.write_quorum);
    let transition = TransitionIntent {
        transition_id: derive_transition_id(
            &snapshot.resource_uid,
            TransitionKind::Bootstrap,
            &current_configuration.configuration_id,
        ),
        kind: TransitionKind::Bootstrap,
        spec_generation: snapshot.desired.generation,
        effective_policy: policy,
        previous_configuration_id: None,
        current_configuration,
        started_at_unix_seconds: snapshot.now_unix_seconds,
    };
    let mut status = snapshot.status.clone();
    status.transition = Some(transition);
    status = status.with_condition(progressing_condition(
        "BootstrapIntentPersisted",
        "Persisted deterministic write-closed genesis authority",
    ));
    Plan::Apply {
        changes: vec![KubernetesChange::PersistStatus {
            status: Box::new(status),
        }],
    }
}

fn evaluate_transition(
    snapshot: &ObservationSnapshot,
    transition: &TransitionIntent,
    config: &EvaluationConfig,
) -> Plan {
    let mut status = transition_status(snapshot.status.clone());
    if snapshot.desired.replicas != transition.effective_policy.replica_set_size {
        status = status.with_condition(unsupported_replica_count_condition(
            snapshot.desired.replicas,
            transition.effective_policy.replica_set_size,
        ));
    }
    if status != snapshot.status {
        return Plan::Apply {
            changes: vec![KubernetesChange::PersistStatus {
                status: Box::new(status),
            }],
        };
    }

    if transition.kind != TransitionKind::Bootstrap {
        return Plan::Wait {
            reason: if snapshot.desired.replicas != transition.effective_policy.replica_set_size {
                WaitReason::UnsupportedSpecDuringTransition
            } else {
                WaitReason::ActiveTransition
            },
            status,
            requeue_after_seconds: config.wait_requeue_seconds,
        };
    }

    for member in &transition.current_configuration.members {
        let Some(observation) = snapshot.observation_for_identity(&member.identity) else {
            continue;
        };
        if let Err(message) = bootstrap_initialization_command(snapshot, member, observation) {
            return unsafe_plan(
                status,
                UnsafeReason::ContradictoryReplicaEvidence(message),
                config,
            );
        }
    }

    for member in &transition.current_configuration.members {
        let Some(observation) = snapshot.observation_for_identity(&member.identity) else {
            return Plan::Wait {
                reason: WaitReason::AgentUnavailable,
                status,
                requeue_after_seconds: config.wait_requeue_seconds,
            };
        };
        match bootstrap_initialization_command(snapshot, member, observation)
            .expect("bootstrap observations were prevalidated")
        {
            Some(command) => {
                return Plan::Execute {
                    command: ProtocolCommand::InitializeAgentStore(command),
                };
            }
            None if !matches!(observation.agent, AgentObservation::Report(_)) => {
                return Plan::Wait {
                    reason: WaitReason::AgentUnavailable,
                    status,
                    requeue_after_seconds: config.wait_requeue_seconds,
                };
            }
            None => {}
        }
    }

    Plan::Wait {
        reason: WaitReason::ActiveTransition,
        status,
        requeue_after_seconds: config.wait_requeue_seconds,
    }
}

fn bootstrap_initialization_command(
    snapshot: &ObservationSnapshot,
    member: &ConfigurationMember,
    observation: &crate::observation::ReplicaObservation,
) -> Result<Option<InitializeAgentStore>, String> {
    match &observation.agent {
        AgentObservation::Uninitialized(report) => {
            let kubernetes = observation
                .kubernetes
                .as_ref()
                .expect("validated uninitialized report matches scaffolding");
            let pod_uid = kubernetes
                .pod_uid
                .clone()
                .expect("validated uninitialized report has Pod UID");
            let pvc_uid = kubernetes
                .pvc_uid
                .clone()
                .expect("validated uninitialized report has PVC UID");
            let initialization_id = derive_initialization_id(
                &snapshot.resource_uid,
                member.identity.replica_id,
                &pod_uid,
                &pvc_uid,
            );
            if derive_agent_generation(&initialization_id) != member.identity.agent_generation
                || ReplicaInstanceId::new(pod_uid.as_str()) != member.identity.instance_id
            {
                return Err(format!(
                    "bootstrap member {} does not match persisted identity",
                    member.identity.replica_id
                ));
            }
            let transition = snapshot
                .status
                .transition
                .as_ref()
                .expect("bootstrap command requires persisted transition");
            Ok(Some(InitializeAgentStore {
                initialization_id,
                resource_uid: snapshot.resource_uid.clone(),
                local_replica_id: member.identity.replica_id,
                expected_instance_id: member.identity.instance_id.clone(),
                expected_pod_uid: report.pod_uid.clone(),
                expected_pvc_uid: report.pvc_uid.clone(),
                assigned_agent_generation: member.identity.agent_generation.clone(),
                effective_policy: transition.effective_policy.clone(),
            }))
        }
        AgentObservation::Report(report) => {
            if report.identity != member.identity {
                return Err(format!(
                    "bootstrap member {} reports identity {}@{} instead of {}@{}",
                    member.identity.replica_id,
                    report.identity.instance_id,
                    report.identity.agent_generation,
                    member.identity.instance_id,
                    member.identity.agent_generation,
                ));
            }
            Ok(None)
        }
        AgentObservation::Absent
        | AgentObservation::Unreachable { .. }
        | AgentObservation::Invalid { .. } => Ok(None),
    }
}

fn incompatible_protocol_plan(
    snapshot: &ObservationSnapshot,
    config: &EvaluationConfig,
) -> Option<Plan> {
    snapshot.replicas.iter().find_map(|(key, observation)| {
        let observed = match &observation.agent {
            AgentObservation::Uninitialized(report) => report.protocol_version,
            AgentObservation::Report(report) => report.protocol_version,
            AgentObservation::Absent
            | AgentObservation::Unreachable { .. }
            | AgentObservation::Invalid { .. } => return None,
        };
        (observed != config.supported_protocol_version).then(|| {
            unsafe_plan(
                snapshot.status.clone(),
                UnsafeReason::IncompatibleProtocolVersion {
                    replica_id: key.replica_id.value(),
                    expected: config.supported_protocol_version,
                    observed,
                },
                config,
            )
        })
    })
}

fn invalid_agent_plan(snapshot: &ObservationSnapshot, config: &EvaluationConfig) -> Option<Plan> {
    snapshot.replicas.iter().find_map(|(key, observation)| {
        let AgentObservation::Invalid { message } = &observation.agent else {
            return None;
        };
        Some(unsafe_plan(
            snapshot.status.clone(),
            UnsafeReason::ContradictoryReplicaEvidence(format!(
                "replica {}@{} returned invalid evidence: {message}",
                key.replica_id, key.instance_id
            )),
            config,
        ))
    })
}

fn has_durable_replica_evidence(snapshot: &ObservationSnapshot) -> bool {
    snapshot.durable_storage_evidence
        || snapshot
            .replicas
            .values()
            .any(|observation| matches!(observation.agent, AgentObservation::Report(_)))
}

fn unsafe_plan(status: AcceptedStatus, reason: UnsafeReason, config: &EvaluationConfig) -> Plan {
    let status = clear_runtime_conditions(status)
        .with_condition(StatusCondition {
            type_: "Ready".to_string(),
            status: ConditionStatus::False,
            reason: "Unsafe".to_string(),
            message: "Replica authority is unsafe".to_string(),
        })
        .with_condition(StatusCondition {
            type_: "Unsafe".to_string(),
            status: ConditionStatus::True,
            reason: "UnsafeAuthority".to_string(),
            message: format!("{reason:?}"),
        });
    Plan::Unsafe {
        reason,
        status,
        safety_changes: vec![SafetyChange::RemoveWriteRouting],
        requeue_after_seconds: config.unsafe_requeue_seconds,
    }
}

fn ready_condition() -> StatusCondition {
    StatusCondition {
        type_: "Ready".to_string(),
        status: ConditionStatus::True,
        reason: "Stable".to_string(),
        message: "Accepted topology is stable".to_string(),
    }
}

fn progressing_condition(reason: &str, message: &str) -> StatusCondition {
    StatusCondition {
        type_: "Progressing".to_string(),
        status: ConditionStatus::True,
        reason: reason.to_string(),
        message: message.to_string(),
    }
}

fn unsupported_replica_count_condition(requested: u32, frozen: u32) -> StatusCondition {
    StatusCondition {
        type_: "UnsupportedSpec".to_string(),
        status: ConditionStatus::True,
        reason: "ReplicaCountImmutable".to_string(),
        message: format!(
            "requested replica count {requested} differs from frozen replica-set size {frozen}"
        ),
    }
}

fn waiting_status(status: AcceptedStatus, reason: &str, message: &str) -> AcceptedStatus {
    clear_runtime_conditions(status)
        .with_condition(StatusCondition {
            type_: "Ready".to_string(),
            status: ConditionStatus::Unknown,
            reason: reason.to_string(),
            message: message.to_string(),
        })
        .with_condition(progressing_condition(reason, message))
}

fn transition_status(status: AcceptedStatus) -> AcceptedStatus {
    waiting_status(
        status.without_condition("UnsupportedSpec"),
        "TransitionActive",
        "Persisted transition remains in progress",
    )
}

fn clear_evaluator_conditions(status: AcceptedStatus) -> AcceptedStatus {
    clear_runtime_conditions(status).without_condition("UnsupportedSpec")
}

fn clear_runtime_conditions(status: AcceptedStatus) -> AcceptedStatus {
    status
        .without_condition("Ready")
        .without_condition("Unsafe")
        .without_condition("Progressing")
}
