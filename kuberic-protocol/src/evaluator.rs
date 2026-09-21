use crate::command::{InitializeAgentStore, KubernetesChange, ProtocolCommand, SafetyChange};
use crate::observation::{AgentObservation, ObservationSnapshot};
use crate::plan::{Plan, UnsafeReason, WaitReason};
use crate::types::{
    AcceptedStatus, ConditionStatus, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy,
    Epoch, ReplicaIdentity, ReplicaInstanceId, ReplicaRole, StatusCondition, TransitionIntent,
    TransitionKind, derive_agent_generation, derive_initialization_id, derive_transition_id,
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
            status: snapshot.status.clone(),
            requeue_after_seconds: config.wait_requeue_seconds,
        };
    }

    if let Some(transition) = &snapshot.status.transition {
        return evaluate_transition(snapshot, transition, config);
    }

    if snapshot.status.provisioning.is_some() {
        return Plan::Wait {
            reason: WaitReason::ProvisioningInProgress,
            status: snapshot.status.clone(),
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
    let frozen_size = topology.configuration.members.len() as u32;
    let mut status = snapshot.status.clone();
    if snapshot.desired.replicas != frozen_size {
        status = status.with_condition(unsupported_replica_count_condition(
            snapshot.desired.replicas,
            frozen_size,
        ));
    } else {
        status.observed_generation = snapshot.desired.generation;
        status = status.with_condition(ready_condition());
    }
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
            .replicas
            .get(&replica_id)
            .and_then(|observation| observation.kubernetes.as_ref())
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
    let mut status = snapshot.status.clone();
    if snapshot.desired.replicas != transition.effective_policy.replica_set_size {
        let condition = unsupported_replica_count_condition(
            snapshot.desired.replicas,
            transition.effective_policy.replica_set_size,
        );
        if !has_condition(&status, &condition) {
            status = status.with_condition(condition);
            return Plan::Apply {
                changes: vec![KubernetesChange::PersistStatus {
                    status: Box::new(status),
                }],
            };
        }
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
        let Some(observation) = snapshot.replicas.get(&member.identity.replica_id) else {
            return Plan::Wait {
                reason: WaitReason::AgentUnavailable,
                status,
                requeue_after_seconds: config.wait_requeue_seconds,
            };
        };
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
                    return unsafe_plan(
                        status,
                        UnsafeReason::ContradictoryReplicaEvidence(format!(
                            "bootstrap member {} does not match persisted identity",
                            member.identity.replica_id
                        )),
                        config,
                    );
                }
                return Plan::Execute {
                    command: ProtocolCommand::InitializeAgentStore(InitializeAgentStore {
                        initialization_id,
                        resource_uid: snapshot.resource_uid.clone(),
                        local_replica_id: member.identity.replica_id,
                        expected_instance_id: member.identity.instance_id.clone(),
                        expected_pod_uid: report.pod_uid.clone(),
                        expected_pvc_uid: report.pvc_uid.clone(),
                        assigned_agent_generation: member.identity.agent_generation.clone(),
                        effective_policy: transition.effective_policy.clone(),
                    }),
                };
            }
            AgentObservation::Report(report) => {
                if report.identity != member.identity {
                    return unsafe_plan(
                        status,
                        UnsafeReason::ContradictoryReplicaEvidence(format!(
                            "bootstrap member {} reports identity {}@{} instead of {}@{}",
                            member.identity.replica_id,
                            report.identity.instance_id,
                            report.identity.agent_generation,
                            member.identity.instance_id,
                            member.identity.agent_generation,
                        )),
                        config,
                    );
                }
            }
            AgentObservation::Absent
            | AgentObservation::Unreachable { .. }
            | AgentObservation::Invalid { .. } => {
                return Plan::Wait {
                    reason: WaitReason::AgentUnavailable,
                    status,
                    requeue_after_seconds: config.wait_requeue_seconds,
                };
            }
        }
    }

    Plan::Wait {
        reason: WaitReason::ActiveTransition,
        status,
        requeue_after_seconds: config.wait_requeue_seconds,
    }
}

fn incompatible_protocol_plan(
    snapshot: &ObservationSnapshot,
    config: &EvaluationConfig,
) -> Option<Plan> {
    snapshot
        .replicas
        .iter()
        .find_map(|(replica_id, observation)| {
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
                        replica_id: replica_id.value(),
                        expected: config.supported_protocol_version,
                        observed,
                    },
                    config,
                )
            })
        })
}

fn invalid_agent_plan(snapshot: &ObservationSnapshot, config: &EvaluationConfig) -> Option<Plan> {
    snapshot
        .replicas
        .iter()
        .find_map(|(replica_id, observation)| {
            let AgentObservation::Invalid { message } = &observation.agent else {
                return None;
            };
            Some(unsafe_plan(
                snapshot.status.clone(),
                UnsafeReason::ContradictoryReplicaEvidence(format!(
                    "replica {replica_id} returned invalid evidence: {message}"
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
    let status = status.with_condition(StatusCondition {
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

fn has_condition(status: &AcceptedStatus, condition: &StatusCondition) -> bool {
    status.conditions.iter().any(|existing| {
        existing.type_ == condition.type_
            && existing.status == condition.status
            && existing.reason == condition.reason
            && existing.message == condition.message
    })
}
