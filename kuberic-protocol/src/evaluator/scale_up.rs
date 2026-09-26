use super::*;
use crate::command::ScaleDownResource;
use crate::observation::{AgentBuildReport, AgentReport, ExactResourceObservation};
use crate::types::{
    CleanupResourceIdentity, ProvisioningPurpose, ReplicaId, ScaleUpCleanup,
    ScaleUpConfigurationEvidence, ScaleUpFailoverEvidence, ScaleUpIntent, ScaleUpProvisioning,
    ScaleUpReceipt, ScaleUpStage, ScaleUpWitness,
};
use crate::validation::{
    validate_scale_up, validate_scale_up_cleanup, validate_scale_up_failover_evidence,
    validate_scale_up_receipt,
};

fn persist(status: AcceptedStatus) -> Plan {
    Plan::Apply {
        changes: vec![KubernetesChange::PersistStatus {
            status: Box::new(status),
        }],
    }
}

fn counts(snapshot: &ObservationSnapshot, status: &AcceptedStatus) -> (u32, u32) {
    (
        status
            .effective_policy
            .as_ref()
            .map_or(0, |policy| policy.replica_set_size),
        snapshot.desired.replicas,
    )
}

fn progress_status(
    snapshot: &ObservationSnapshot,
    status: AcceptedStatus,
    reason: &str,
    phase: &str,
    target: Option<&ReplicaIdentity>,
    attempt: Option<&OperationId>,
    blocking: &str,
) -> AcceptedStatus {
    let (accepted, desired) = counts(snapshot, &status);
    let target = target.map_or_else(
        || "none".to_string(),
        |identity| format!("{}@{}", identity.replica_id, identity.instance_id),
    );
    let attempt = attempt.map_or("none", OperationId::as_str);
    waiting_status(
        status,
        reason,
        &format!(
            "accepted={accepted} desired={desired} target={target} attempt={attempt} phase={phase} blocking={blocking}"
        ),
    )
}

#[allow(clippy::too_many_arguments)]
fn publish_phase_if_changed(
    snapshot: &ObservationSnapshot,
    status: &AcceptedStatus,
    reason: &str,
    phase: &str,
    target: Option<&ReplicaIdentity>,
    attempt: Option<&OperationId>,
    blocking: &str,
) -> Option<Plan> {
    let next = progress_status(
        snapshot,
        status.clone(),
        reason,
        phase,
        target,
        attempt,
        blocking,
    );
    if &next == status {
        return None;
    }
    Some(persist(next))
}

pub(super) fn stable_condition(
    snapshot: &ObservationSnapshot,
    receipt: &ScaleUpReceipt,
) -> StatusCondition {
    let (accepted, desired) = counts(snapshot, &snapshot.status);
    StatusCondition {
        type_: "Ready".into(),
        status: ConditionStatus::True,
        reason: "ScaleUpStable".into(),
        message: format!(
            "accepted={accepted} desired={desired} target={}@{} attempt={} phase=stable blocking=none",
            receipt.intent.target.replica_id,
            receipt.intent.target.instance_id,
            receipt.intent.operation_id
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn wait(
    snapshot: &ObservationSnapshot,
    status: AcceptedStatus,
    reason: &str,
    phase: &str,
    target: Option<&ReplicaIdentity>,
    attempt: Option<&OperationId>,
    blocking: &str,
    config: &EvaluationConfig,
) -> Plan {
    Plan::Wait {
        reason: WaitReason::ActiveTransition,
        status: progress_status(snapshot, status, reason, phase, target, attempt, blocking),
        requeue_after_seconds: config.wait_requeue_seconds,
    }
}

#[allow(clippy::too_many_arguments)]
fn quorum_wait(
    snapshot: &ObservationSnapshot,
    status: AcceptedStatus,
    reason: &str,
    phase: &str,
    target: &ReplicaIdentity,
    attempt: &OperationId,
    blocking: &str,
    config: &EvaluationConfig,
) -> Plan {
    Plan::Wait {
        reason: WaitReason::QuorumLoss,
        status: progress_status(
            snapshot,
            status,
            reason,
            phase,
            Some(target),
            Some(attempt),
            blocking,
        ),
        requeue_after_seconds: config.wait_requeue_seconds,
    }
}

fn report<'a>(
    snapshot: &'a ObservationSnapshot,
    identity: &ReplicaIdentity,
) -> Option<&'a AgentReport> {
    healthy_report(snapshot, identity)
}

fn build<'a>(
    report: &'a AgentReport,
    build_id: &OperationId,
    target: &ReplicaIdentity,
) -> Option<&'a AgentBuildReport> {
    report
        .builds
        .iter()
        .find(|build| &build.build_id == build_id && &build.target == target)
}

enum BuildPair<'a> {
    Missing,
    Propagating,
    Exact(&'a AgentBuildReport, &'a AgentBuildReport),
}

fn exact_build_pair<'a>(
    source: &'a AgentReport,
    target: &'a AgentReport,
    build_id: &OperationId,
    target_identity: &ReplicaIdentity,
) -> Result<BuildPair<'a>, &'static str> {
    let source_build = build(source, build_id, target_identity);
    let target_build = build(target, build_id, target_identity);
    let (Some(source_build), Some(target_build)) = (source_build, target_build) else {
        return Ok(if source_build.is_some() || target_build.is_some() {
            BuildPair::Propagating
        } else {
            BuildPair::Missing
        });
    };
    if source_build.replication_boundary_lsn != target_build.replication_boundary_lsn {
        return Err("source and receiver report conflicting immutable snapshot boundaries");
    }
    match (
        source_build.catch_up_boundary_lsn,
        target_build.catch_up_boundary_lsn,
    ) {
        (Some(source), Some(target)) if source != target => {
            return Err("source and receiver report conflicting frozen catch-up boundaries");
        }
        (Some(_), None) | (None, Some(_)) => return Ok(BuildPair::Propagating),
        _ => {}
    }
    if source_build
        .catch_up_boundary_lsn
        .is_some_and(|boundary| boundary < source_build.replication_boundary_lsn)
    {
        return Err("source and receiver report conflicting immutable build boundaries");
    }
    Ok(BuildPair::Exact(source_build, target_build))
}

fn cleanup_observation<'a>(
    snapshot: &'a ObservationSnapshot,
    target: &ReplicaIdentity,
) -> Option<&'a crate::observation::SecondaryScaleDownResourceObservation> {
    secondary_scale_down::resources(snapshot, target)
}

fn restore_accepted_service_before_cleanup(
    snapshot: &ObservationSnapshot,
    accepted: &ConfigurationDescriptor,
    policy: &EffectivePolicy,
    config: &EvaluationConfig,
) -> Option<Plan> {
    let primary = configuration_primary(accepted);
    let reports = accepted
        .members
        .iter()
        .filter_map(|member| {
            let report = report(snapshot, &member.identity)?;
            stable_member_report(report, member, accepted).then_some(report)
        })
        .collect::<Vec<_>>();
    let primary_report = reports
        .iter()
        .copied()
        .find(|report| report.identity == primary.identity);
    let Some(primary_report) = primary_report else {
        return Some(Plan::Wait {
            reason: WaitReason::AwaitingStableEvidence,
            status: waiting_status(
                snapshot.status.clone(),
                "ScaleUpFailoverPrimaryPending",
                "Accepted failover primary must attest exact authority before cleanup",
            ),
            requeue_after_seconds: config.wait_requeue_seconds,
        });
    };
    if reports.len() < accepted.write_quorum as usize {
        return Some(Plan::Wait {
            reason: WaitReason::QuorumLoss,
            status: waiting_status(
                snapshot.status.clone(),
                "ScaleUpFailoverWriteQuorumPending",
                "Accepted failover write quorum must recover before candidate cleanup",
            ),
            requeue_after_seconds: config.wait_requeue_seconds,
        });
    }
    if primary_report.write_status != AccessStatus::Granted {
        return Some(Plan::Execute {
            command: ProtocolCommand::EnsureConfiguration(Box::new(ensure_configuration_command(
                accepted,
                primary,
                policy,
                OperationId::new(format!(
                    "scale-up-failover:{}:grant-write",
                    accepted.configuration_id
                )),
                AccessStatus::Granted,
                false,
            ))),
        });
    }
    if !snapshot.routing.service_present {
        return Some(Plan::Apply {
            changes: vec![KubernetesChange::EnsureWriteRoutingService],
        });
    }
    if snapshot.routing.write_target.as_ref() != Some(&primary.identity)
        || snapshot.routing.unresolved_write_target
    {
        let mut changes = Vec::new();
        if snapshot.routing.write_target.is_some() || snapshot.routing.unresolved_write_target {
            changes.push(KubernetesChange::RemoveWriteRouting);
        } else {
            changes.push(KubernetesChange::PublishWriteRouting {
                primary: primary.identity.clone(),
            });
        }
        return Some(Plan::Apply { changes });
    }
    None
}

fn freeze_cleanup(
    snapshot: &ObservationSnapshot,
    provisioning: &ProvisioningIntent,
    mut status: AcceptedStatus,
    reason: &str,
    primary_failed: bool,
    config: &EvaluationConfig,
) -> Plan {
    let target = provisioning.target_identity(&snapshot.resource_uid);
    let Some(exact) = cleanup_observation(snapshot, &target).filter(|observation| {
        secondary_scale_down::observed(&observation.identity.pod, &observation.pod)
            && secondary_scale_down::observed(&observation.identity.pvc, &observation.pvc)
            && secondary_scale_down::observed(&observation.identity.endpoint, &observation.endpoint)
    }) else {
        return wait(
            snapshot,
            status,
            "ScaleUpExactCleanupEvidencePending",
            "cancellation",
            Some(&target),
            Some(&provisioning.operation_id),
            "authoritative exact Pod, PVC, and endpoint observations are required",
            config,
        );
    };
    let cleanup = ScaleUpCleanup {
        provisioning: provisioning.clone(),
        target: target.clone(),
        resources: exact.identity.clone(),
    };
    if let Err(error) = validate_scale_up_cleanup(&cleanup) {
        return unsafe_plan(
            status,
            UnsafeReason::ContradictoryReplicaEvidence(error.to_string()),
            config,
        );
    }
    status.provisioning = None;
    status.transition = None;
    status.scale_up_cleanup = Some(Box::new(cleanup));
    status.scale_up_admission_started = None;
    status = progress_status(
        snapshot,
        status,
        reason,
        if primary_failed {
            "failover-recovery"
        } else {
            "cleanup"
        },
        Some(&target),
        Some(&provisioning.operation_id),
        "frozen exact uncommitted candidate cleanup",
    );
    let mut changes = Vec::new();
    if primary_failed
        && (snapshot.routing.write_target.is_some() || snapshot.routing.unresolved_write_target)
    {
        changes.push(KubernetesChange::RemoveWriteRouting);
    }
    changes.push(KubernetesChange::PersistStatus {
        status: Box::new(status),
    });
    Plan::Apply { changes }
}

fn cleanup_delete(
    identity: &CleanupResourceIdentity,
    observed: &ExactResourceObservation,
    resource: ScaleDownResource,
) -> Option<KubernetesChange> {
    let CleanupResourceIdentity::Present { name, uid } = identity else {
        return None;
    };
    let ExactResourceObservation::FrozenUidPresent { resource_version } = observed else {
        return None;
    };
    Some(KubernetesChange::DeleteScaleDownResource {
        resource,
        name: name.clone(),
        uid: uid.clone(),
        resource_version: resource_version.clone(),
    })
}

pub(super) fn cleanup(
    snapshot: &ObservationSnapshot,
    cleanup: &ScaleUpCleanup,
    config: &EvaluationConfig,
) -> Plan {
    if snapshot.status.transition.is_none()
        && let Some(plan) = maybe_begin_stable_failover(snapshot, snapshot.status.clone(), config)
    {
        return plan;
    }
    let target = &cleanup.target;
    let Some(exact) = cleanup_observation(snapshot, target) else {
        return wait(
            snapshot,
            snapshot.status.clone(),
            "ScaleUpCleanupObservationPending",
            "cleanup",
            Some(target),
            Some(&cleanup.provisioning.operation_id),
            "fresh exact cleanup observation is unavailable",
            config,
        );
    };
    if !secondary_scale_down::absent(&cleanup.resources.endpoint, &exact.endpoint) {
        if let Some(change) = cleanup_delete(
            &cleanup.resources.endpoint,
            &exact.endpoint,
            ScaleDownResource::Endpoint,
        ) {
            return Plan::Apply {
                changes: vec![change],
            };
        }
        return wait(
            snapshot,
            snapshot.status.clone(),
            "ScaleUpEndpointCleanupPending",
            "cleanup",
            Some(target),
            Some(&cleanup.provisioning.operation_id),
            "endpoint lookup is unresolved",
            config,
        );
    }
    if !secondary_scale_down::absent(&cleanup.resources.pod, &exact.pod) {
        if let Some(change) =
            cleanup_delete(&cleanup.resources.pod, &exact.pod, ScaleDownResource::Pod)
        {
            return Plan::Apply {
                changes: vec![change],
            };
        }
        return wait(
            snapshot,
            snapshot.status.clone(),
            "ScaleUpPodCleanupPending",
            "cleanup",
            Some(target),
            Some(&cleanup.provisioning.operation_id),
            "Pod lookup is unresolved",
            config,
        );
    }
    if !secondary_scale_down::absent(&cleanup.resources.pvc, &exact.pvc) {
        if let Some(change) =
            cleanup_delete(&cleanup.resources.pvc, &exact.pvc, ScaleDownResource::Pvc)
        {
            return Plan::Apply {
                changes: vec![change],
            };
        }
        return wait(
            snapshot,
            snapshot.status.clone(),
            "ScaleUpPvcCleanupPending",
            "cleanup",
            Some(target),
            Some(&cleanup.provisioning.operation_id),
            "PVC lookup is unresolved",
            config,
        );
    }
    let mut status = snapshot.status.clone();
    status.scale_up_cleanup = None;
    persist(progress_status(
        snapshot,
        status,
        "ScaleUpCleanupComplete",
        "cleanup",
        Some(target),
        Some(&cleanup.provisioning.operation_id),
        "exact uncommitted candidate is absent",
    ))
}

fn prior_receipt_settled(snapshot: &ObservationSnapshot, receipt: &ScaleUpReceipt) -> bool {
    let accepted = snapshot
        .status
        .topology
        .as_ref()
        .map(|topology| &topology.configuration);
    receipt.accepted_configuration.members.iter().all(|member| {
        let Some(report) = report(snapshot, &member.identity) else {
            return accepted.is_some_and(|configuration| {
                configuration
                    .members
                    .iter()
                    .all(|accepted_member| accepted_member.identity != member.identity)
            });
        };
        let accepted_newer = accepted.is_some_and(|configuration| {
            report.previous_configuration.is_none()
                && report.current_configuration.as_ref() == Some(configuration)
                && report.epoch >= configuration.epoch
        });
        accepted_newer
            || (report.role == member.role
                && report.previous_configuration.is_none()
                && report.current_configuration.as_ref() == Some(&receipt.accepted_configuration)
                && report
                    .verified_replication_lsn
                    .is_some_and(|lsn| lsn >= receipt.intent.catch_up_boundary_lsn)
                && report.scale_up_intent.as_deref() == Some(&receipt.intent)
                && report.pending_operation_id.is_none())
    })
}

fn configuration_command(
    evidence: ScaleUpConfigurationEvidence,
    current: &ConfigurationDescriptor,
    member: &ConfigurationMember,
    current_only: bool,
    failover_safe_lsn: Option<i64>,
) -> EnsureConfiguration {
    let intent = evidence.intent();
    let retire_build_id = intent.build_id.clone();
    let stage = if current_only {
        ScaleUpStage::CurrentOnly
    } else {
        ScaleUpStage::PreviousCurrent
    };
    EnsureConfiguration {
        operation_id: intent.command_operation_id(stage, &member.identity, current),
        previous_configuration: (!current_only).then(|| intent.previous_configuration.clone()),
        current_configuration: current.clone(),
        previous_epoch: (!current_only).then_some(intent.previous_configuration.epoch),
        current_epoch: current.epoch,
        effective_policy: intent.current_policy.clone(),
        previous_policy: Some(intent.previous_policy.clone()),
        secondary_removal_evidence: None,
        scale_up_evidence: Some(Box::new(evidence)),
        local_replica_id: member.identity.replica_id,
        expected_instance_id: member.identity.instance_id.clone(),
        expected_agent_generation: member.identity.agent_generation.clone(),
        transition_kind: if failover_safe_lsn.is_some() {
            TransitionKind::Failover
        } else {
            TransitionKind::ScaleUp
        },
        failover_safe_lsn,
        primary_write_status: AccessStatus::Granted,
        current_only,
        retire_build_ids: if current_only {
            vec![retire_build_id]
        } else {
            Vec::new()
        },
        switchover_handoff: None,
        retire_switchover_preparation_ids: Vec::new(),
    }
}

pub(super) fn recover_local_acceptance(
    snapshot: &ObservationSnapshot,
    config: &EvaluationConfig,
) -> Option<Plan> {
    let receipt = snapshot.status.last_scale_up.as_deref()?;
    let accepted = &snapshot.status.topology.as_ref()?.configuration;
    if replica_failed(snapshot, &configuration_primary(accepted).identity) {
        return None;
    }
    let primary = configuration_primary(accepted);
    let service_needs_restoration = !snapshot.routing.service_present
        || snapshot.routing.unresolved_write_target
        || snapshot.routing.write_target.as_ref() != Some(&primary.identity)
        || report(snapshot, &primary.identity)
            .is_none_or(|report| report.write_status != AccessStatus::Granted);
    if snapshot.status.transition.is_none()
        && service_needs_restoration
        && let Some(plan) = restore_accepted_service_before_cleanup(
            snapshot,
            accepted,
            snapshot.status.effective_policy.as_ref()?,
            config,
        )
    {
        return Some(plan);
    }
    if prior_receipt_settled(snapshot, receipt) {
        return None;
    }
    let evidence = receipt.failover_evidence.as_ref().map_or_else(
        || ScaleUpConfigurationEvidence::Admission {
            intent: receipt.intent.clone(),
        },
        |evidence| ScaleUpConfigurationEvidence::Failover {
            evidence: evidence.clone(),
        },
    );
    let failover_safe_lsn = receipt
        .failover_evidence
        .as_ref()
        .and_then(|_| {
            receipt
                .current_only_write_quorum
                .iter()
                .find_map(|witness| {
                    (witness.identity.replica_id == receipt.accepted_configuration.primary_id)
                        .then_some(witness.verified_replication_lsn)
                })
        })
        .or_else(|| {
            receipt.failover_evidence.as_ref().and_then(|evidence| {
                evidence.current_read_quorum.iter().find_map(|witness| {
                    (witness.identity.replica_id == receipt.accepted_configuration.primary_id)
                        .then_some(witness.verified_replication_lsn)
                })
            })
        });
    for member in &receipt.accepted_configuration.members {
        let Some(report) = report(snapshot, &member.identity) else {
            return Some(wait(
                snapshot,
                snapshot.status.clone(),
                "ScaleUpCommittedDegraded",
                "local-convergence",
                Some(&receipt.intent.target),
                Some(&receipt.intent.operation_id),
                "an exact accepted member is unavailable",
                config,
            ));
        };
        let advanced_to_newer_scale_up = report.epoch > receipt.accepted_configuration.epoch
            && report.scale_up_intent.as_deref() != Some(&receipt.intent)
            && snapshot
                .status
                .transition
                .as_ref()
                .is_some_and(|transition| {
                    report.current_configuration.as_ref() == Some(&transition.current_configuration)
                });
        if advanced_to_newer_scale_up {
            continue;
        }
        let stale_original_failover_authority = receipt.failover_evidence.is_some()
            && report.scale_up_intent.as_deref() == Some(&receipt.intent)
            && report.current_configuration.as_ref() == Some(&receipt.intent.current_configuration)
            && (report.previous_configuration.as_ref()
                == Some(&receipt.intent.previous_configuration)
                || report.previous_configuration.is_none());
        if stale_original_failover_authority {
            return Some(Plan::Execute {
                command: ProtocolCommand::EnsureConfiguration(Box::new(configuration_command(
                    evidence.clone(),
                    &receipt.accepted_configuration,
                    member,
                    false,
                    failover_safe_lsn,
                ))),
            });
        }
        let historical_pc_cc = report.previous_configuration.as_ref()
            == Some(&receipt.intent.previous_configuration)
            && report.current_configuration.as_ref() == Some(&receipt.accepted_configuration)
            && report.scale_up_intent.as_deref() == Some(&receipt.intent);
        if historical_pc_cc {
            return Some(Plan::Execute {
                command: ProtocolCommand::EnsureConfiguration(Box::new(configuration_command(
                    evidence.clone(),
                    &receipt.accepted_configuration,
                    member,
                    true,
                    failover_safe_lsn,
                ))),
            });
        }
        let accepted_member = accepted
            .members
            .iter()
            .find(|accepted_member| accepted_member.identity == member.identity);
        if accepted_member.is_some()
            && (report.previous_configuration.is_some()
                || report.current_configuration.as_ref() != Some(accepted))
        {
            return Some(wait(
                snapshot,
                snapshot.status.clone(),
                "ScaleUpHistoricalAcceptancePending",
                "local-convergence",
                Some(&receipt.intent.target),
                Some(&receipt.intent.operation_id),
                "historical completion must settle before ordinary accepted-authority correction",
                config,
            ));
        }
    }
    None
}

pub(super) fn begin(
    snapshot: &ObservationSnapshot,
    status: AcceptedStatus,
    config: &EvaluationConfig,
) -> Option<Plan> {
    let topology = &status.topology.as_ref()?.configuration;
    let previous_policy = status.effective_policy.as_ref()?;
    if snapshot.desired.replicas <= previous_policy.replica_set_size {
        return None;
    }
    if status
        .conditions
        .iter()
        .any(|condition| condition.type_ == "UnsupportedSpec")
        || status.primary_failure.is_some()
        || status.quorum_loss.is_some()
        || status.pending_replacement_cleanup.is_some()
        || status.last_replacement.is_some()
        || status.secondary_scale_down_cleanup.is_some()
        || status.scale_up_cleanup.is_some()
    {
        return Some(wait(
            snapshot,
            status,
            "ScaleUpBlockedByHigherPriorityWork",
            "admission",
            None,
            None,
            "repair, cleanup, quorum recovery, or unsupported drift remains active",
            config,
        ));
    }
    if let Some(receipt) = status.last_scale_up.as_deref().cloned()
        && !prior_receipt_settled(snapshot, &receipt)
    {
        return Some(wait(
            snapshot,
            status,
            "ScaleUpPriorReceiptPending",
            "local-convergence",
            Some(&receipt.intent.target),
            Some(&receipt.intent.operation_id),
            "late members still require the previous completion receipt",
            config,
        ));
    }
    if snapshot.desired.failover_delay_seconds != previous_policy.failover_delay_seconds
        || topology.members.iter().any(|member| {
            snapshot
                .observation_for_identity(&member.identity)
                .and_then(|observation| observation.kubernetes.as_ref())
                .and_then(|kubernetes| kubernetes.image.as_deref())
                != Some(snapshot.desired.image.as_str())
        })
    {
        return Some(wait(
            snapshot,
            status,
            "ScaleUpUnsupportedDrift",
            "admission",
            None,
            None,
            "scale-up requires count-only desired-state drift",
            config,
        ));
    }
    if topology.members.iter().any(|member| {
        report(snapshot, &member.identity)
            .is_none_or(|report| !stable_member_report(report, member, topology))
    }) {
        return Some(wait(
            snapshot,
            status,
            "ScaleUpAwaitingStableAuthority",
            "admission",
            None,
            None,
            "every exact accepted member must attest stable current-only authority",
            config,
        ));
    }
    let primary = configuration_primary(topology);
    if report(snapshot, &primary.identity)
        .is_none_or(|report| report.write_status != AccessStatus::Granted)
        || snapshot.routing.write_target.as_ref() != Some(&primary.identity)
    {
        return Some(wait(
            snapshot,
            status,
            "ScaleUpAwaitingWritablePrimary",
            "admission",
            None,
            None,
            "accepted write quorum and exact primary routing must be stable",
            config,
        ));
    }
    let Some(target_value) = previous_policy.replica_set_size.checked_add(1) else {
        return Some(unsafe_plan(
            status,
            UnsafeReason::InvalidDesiredState("scale-up target ordinal overflowed".into()),
            config,
        ));
    };
    let target_id = ReplicaId::new(i64::from(target_value));
    let unrelated_extra = snapshot.replicas.iter().any(|(key, observation)| {
        let accepted = topology.members.iter().any(|member| {
            member.identity.replica_id == key.replica_id
                && member.identity.instance_id == key.instance_id
        });
        !accepted && key.replica_id != target_id && observation.kubernetes.is_some()
    });
    if unrelated_extra {
        return Some(wait(
            snapshot,
            status,
            "ScaleUpCleanupBlocking",
            "admission",
            None,
            None,
            "unresolved exact lifecycle resources block a fresh scale-up attempt",
            config,
        ));
    }
    let candidates = snapshot
        .replicas
        .iter()
        .filter(|(key, observation)| {
            key.replica_id == target_id
                && observation
                    .kubernetes
                    .as_ref()
                    .is_some_and(|kubernetes| kubernetes.has_exact_scaffolding())
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Some(Plan::Apply {
            changes: vec![KubernetesChange::EnsureReplicaScaffolding {
                replica_ids: vec![target_id],
            }],
        });
    }
    if candidates.len() != 1 {
        return Some(unsafe_plan(
            status,
            UnsafeReason::ContradictoryReplicaEvidence(
                "multiple exact scale-up candidate incarnations are present".into(),
            ),
            config,
        ));
    }
    let (_, observation) = candidates[0];
    let kubernetes = observation
        .kubernetes
        .as_ref()
        .expect("candidate filter requires Kubernetes evidence");
    let Some(pod_uid) = kubernetes.pod_uid.clone() else {
        unreachable!("candidate filter requires Pod UID")
    };
    let Some(pvc_uid) = kubernetes.pvc_uid.clone() else {
        unreachable!("candidate filter requires PVC UID")
    };
    if kubernetes.image.as_deref() != Some(snapshot.desired.image.as_str()) {
        return Some(wait(
            snapshot,
            status,
            "ScaleUpCandidateImagePending",
            "provisioning",
            None,
            None,
            "candidate image differs from accepted desired image",
            config,
        ));
    }
    match &observation.agent {
        AgentObservation::Uninitialized(_) => {}
        AgentObservation::Absent | AgentObservation::Unreachable { .. } => {
            return Some(wait(
                snapshot,
                status,
                "ScaleUpFreshStorePending",
                "provisioning",
                None,
                None,
                "candidate has not reported fresh uninitialized storage",
                config,
            ));
        }
        AgentObservation::Report(_) | AgentObservation::Invalid { .. } => {
            return Some(unsafe_plan(
                status,
                UnsafeReason::ContradictoryReplicaEvidence(
                    "scale-up candidate storage is already initialized or invalid".into(),
                ),
                config,
            ));
        }
    }
    let Some(current_policy) =
        EffectivePolicy::fixed(target_value, previous_policy.failover_delay_seconds)
    else {
        unreachable!("positive scale-up target has a policy")
    };
    let scale_up = ScaleUpProvisioning {
        resource_uid: snapshot.resource_uid.clone(),
        spec_generation: snapshot.desired.generation,
        desired_replicas: snapshot.desired.replicas,
        previous_configuration: topology.clone(),
        previous_policy: previous_policy.clone(),
        current_policy,
        target_replica_id: target_id,
    };
    let mut provisioning = ProvisioningIntent {
        purpose: ProvisioningPurpose::scale_up(scale_up),
        pod_uid,
        pvc_uid,
        operation_id: OperationId::default(),
    };
    provisioning.operation_id = provisioning.expected_operation_id();
    let target = provisioning.target_identity(&snapshot.resource_uid);
    let attempt = provisioning.operation_id.clone();
    let mut next = status;
    next.provisioning = Some(provisioning);
    Some(persist(progress_status(
        snapshot,
        next,
        "ScaleUpProvisioningAccepted",
        "provisioning",
        Some(&target),
        Some(&attempt),
        "frozen one exact next-ordinal candidate outside accepted authority",
    )))
}

pub(super) fn provisioning(
    snapshot: &ObservationSnapshot,
    provisioning: &ProvisioningIntent,
    config: &EvaluationConfig,
) -> Plan {
    let scale_up = provisioning
        .scale_up()
        .expect("scale-up provisioning branch");
    let target = provisioning.target_identity(&snapshot.resource_uid);
    let build_id = provisioning
        .scale_up_build_id(&snapshot.resource_uid)
        .expect("scale-up provisioning has build ID");
    let previous = &scale_up.previous_configuration;
    let primary = configuration_primary(previous);
    let status = progress_status(
        snapshot,
        snapshot.status.clone(),
        "ScaleUpProvisioningInProgress",
        "provisioning",
        Some(&target),
        Some(&provisioning.operation_id),
        "candidate initialization, endpoint, or build evidence is incomplete",
    );
    let accepted = &snapshot
        .status
        .topology
        .as_ref()
        .expect("validated scale-up provisioning has accepted topology")
        .configuration;
    if accepted != previous {
        if let Some(plan) = restore_accepted_service_before_cleanup(
            snapshot,
            accepted,
            &scale_up.previous_policy,
            config,
        ) {
            return plan;
        }
        return freeze_cleanup(
            snapshot,
            provisioning,
            status,
            "ScaleUpCleanupPendingAfterFailover",
            false,
            config,
        );
    }
    if snapshot.desired.replicas <= scale_up.previous_policy.replica_set_size {
        return freeze_cleanup(
            snapshot,
            provisioning,
            status,
            "ScaleUpCancelledBeforeAdmission",
            false,
            config,
        );
    }
    if replica_failed(snapshot, &primary.identity) {
        return maybe_begin_stable_failover(snapshot, status, config).unwrap_or_else(|| {
            wait(
                snapshot,
                snapshot.status.clone(),
                "ScaleUpFailoverArbitrationPending",
                "failover-recovery",
                Some(&target),
                Some(&provisioning.operation_id),
                "accepted primary failure arbitration is pending",
                config,
            )
        });
    }
    let Some(observation) = snapshot.observation_for_identity(&target) else {
        return freeze_cleanup(
            snapshot,
            provisioning,
            status,
            "ScaleUpCandidateDisappeared",
            false,
            config,
        );
    };
    let candidate_failed = matches!(observation.agent, AgentObservation::Invalid { .. })
        || matches!(
            &observation.agent,
            AgentObservation::Report(report)
                if report.reported_fault == Some(crate::types::FaultType::Permanent)
        );
    if candidate_failed {
        return freeze_cleanup(
            snapshot,
            provisioning,
            status,
            "ScaleUpCandidateFailed",
            false,
            config,
        );
    }
    match &observation.agent {
        AgentObservation::Uninitialized(report) => Plan::Execute {
            command: ProtocolCommand::InitializeAgentStore(Box::new(InitializeAgentStore {
                initialization_id: provisioning.initialization_id(&snapshot.resource_uid),
                resource_uid: snapshot.resource_uid.clone(),
                local_replica_id: target.replica_id,
                expected_instance_id: target.instance_id.clone(),
                expected_pod_uid: report.pod_uid.clone(),
                expected_pvc_uid: report.pvc_uid.clone(),
                assigned_agent_generation: target.agent_generation.clone(),
                effective_policy: scale_up.current_policy.clone(),
                bootstrap_configuration: previous.clone(),
                provisioning: Some(provisioning.clone()),
            })),
        },
        AgentObservation::Absent | AgentObservation::Unreachable { .. } => wait(
            snapshot,
            status,
            "ScaleUpCandidateAgentPending",
            "provisioning",
            Some(&target),
            Some(&provisioning.operation_id),
            "candidate agent is unavailable",
            config,
        ),
        AgentObservation::Invalid { message } => unsafe_plan(
            status,
            UnsafeReason::ContradictoryReplicaEvidence(message.clone()),
            config,
        ),
        AgentObservation::Report(target_report) => {
            if target_report.identity != target
                || target_report.resource_uid != snapshot.resource_uid
                || target_report.retired_replica.is_some()
                || target_report.role == ReplicaRole::ActiveSecondary
                || target_report.current_configuration.is_some()
            {
                return unsafe_plan(
                    status,
                    UnsafeReason::ContradictoryReplicaEvidence(
                        "candidate report differs from fresh scale-up initialization authority"
                            .into(),
                    ),
                    config,
                );
            }
            if observation
                .kubernetes
                .as_ref()
                .is_none_or(|kubernetes| !kubernetes.peer_endpoint_ready)
            {
                return Plan::Apply {
                    changes: vec![KubernetesChange::EnsureReplicaScaffolding {
                        replica_ids: vec![target.replica_id],
                    }],
                };
            }
            let Some(source_report) = report(snapshot, &primary.identity) else {
                return wait(
                    snapshot,
                    status,
                    "ScaleUpSourceUnavailable",
                    "copying",
                    Some(&target),
                    Some(&build_id),
                    "accepted primary is unavailable",
                    config,
                );
            };
            let pair = match exact_build_pair(source_report, target_report, &build_id, &target) {
                Ok(pair) => pair,
                Err(message) => {
                    return unsafe_plan(
                        status,
                        UnsafeReason::ContradictoryReplicaEvidence(message.into()),
                        config,
                    );
                }
            };
            let (source_build, target_build) = match pair {
                BuildPair::Missing => {
                    if let Some(plan) = publish_phase_if_changed(
                        snapshot,
                        &snapshot.status,
                        "ScaleUpCopying",
                        "copying",
                        Some(&target),
                        Some(&build_id),
                        "source and receiver build progress has not been observed",
                    ) {
                        return plan;
                    }
                    return Plan::Execute {
                        command: ProtocolCommand::EnsureReplicaBuild(Box::new(
                            EnsureReplicaBuild {
                                operation_id: build_id,
                                local_replica_id: primary.identity.replica_id,
                                expected_instance_id: primary.identity.instance_id.clone(),
                                expected_agent_generation: primary
                                    .identity
                                    .agent_generation
                                    .clone(),
                                target,
                                authority: None,
                                source_session_id: None,
                            },
                        )),
                    };
                }
                BuildPair::Propagating => {
                    return wait(
                        snapshot,
                        status,
                        "ScaleUpBoundaryPropagationPending",
                        "catch-up",
                        Some(&target),
                        Some(&build_id),
                        "source and receiver have not both observed the frozen boundary",
                        config,
                    );
                }
                BuildPair::Exact(source_build, target_build) => (source_build, target_build),
            };
            let Some(catch_up_boundary) = source_build.catch_up_boundary_lsn else {
                return wait(
                    snapshot,
                    status,
                    "ScaleUpBoundaryPending",
                    "catch-up",
                    Some(&target),
                    Some(&build_id),
                    "post-enumeration catch-up boundary is not frozen",
                    config,
                );
            };
            if !source_build.completed
                || !target_build.completed
                || target_build.catch_up_boundary_lsn != Some(catch_up_boundary)
                || source_build.durable_lsn < catch_up_boundary
                || target_build.durable_lsn < catch_up_boundary
            {
                if let Some(plan) = publish_phase_if_changed(
                    snapshot,
                    &snapshot.status,
                    "ScaleUpCatchUpPending",
                    "catch-up",
                    Some(&target),
                    Some(&build_id),
                    "receiver durable progress has not reached the frozen catch-up boundary",
                ) {
                    return plan;
                }
                return Plan::Execute {
                    command: ProtocolCommand::EnsureReplicaBuild(Box::new(EnsureReplicaBuild {
                        operation_id: build_id,
                        local_replica_id: primary.identity.replica_id,
                        expected_instance_id: primary.identity.instance_id.clone(),
                        expected_agent_generation: primary.identity.agent_generation.clone(),
                        target,
                        authority: None,
                        source_session_id: None,
                    })),
                };
            }
            let Some(configuration_number) = previous.epoch.configuration_number.checked_add(1)
            else {
                return unsafe_plan(
                    status,
                    UnsafeReason::InvalidAcceptedAuthority(
                        "scale-up configuration epoch exhausted".into(),
                    ),
                    config,
                );
            };
            let mut members = previous.members.clone();
            members.push(ConfigurationMember {
                identity: target.clone(),
                role: ReplicaRole::ActiveSecondary,
            });
            let current = ConfigurationDescriptor::new(
                Epoch::new(previous.epoch.data_loss_number, configuration_number),
                previous.primary_id,
                members,
                scale_up.current_policy.write_quorum,
            );
            let mut intent = ScaleUpIntent {
                operation_id: OperationId::default(),
                resource_uid: snapshot.resource_uid.clone(),
                spec_generation: scale_up.spec_generation,
                desired_replicas: scale_up.desired_replicas,
                previous_configuration: previous.clone(),
                current_configuration: current.clone(),
                previous_policy: scale_up.previous_policy.clone(),
                current_policy: scale_up.current_policy.clone(),
                primary: primary.identity.clone(),
                target: target.clone(),
                build_id: provisioning
                    .scale_up_build_id(&snapshot.resource_uid)
                    .expect("scale-up build ID"),
                snapshot_boundary_lsn: source_build.replication_boundary_lsn,
                catch_up_boundary_lsn: catch_up_boundary,
            };
            intent.operation_id = intent.expected_operation_id();
            if let Err(error) = validate_scale_up(&intent) {
                return unsafe_plan(
                    status,
                    UnsafeReason::InvalidAcceptedAuthority(error.to_string()),
                    config,
                );
            }
            let mut next = snapshot.status.clone();
            next.transition = Some(TransitionIntent {
                transition_id: intent.transition_id(TransitionKind::ScaleUp, &current),
                kind: TransitionKind::ScaleUp,
                spec_generation: intent.spec_generation,
                effective_policy: intent.current_policy.clone(),
                previous_configuration_id: Some(previous.configuration_id.clone()),
                current_configuration: current,
                election_lsn: None,
                build_id: Some(intent.build_id.clone()),
                repair: None,
                switchover: None,
                secondary_scale_down: None,
                secondary_removal_evidence: None,
                scale_up: Some(Box::new(intent.clone())),
                scale_up_failover: None,
            });
            persist(progress_status(
                snapshot,
                next,
                "ScaleUpPreviousCurrentPersisted",
                "pc-cc",
                Some(&target),
                Some(&intent.operation_id),
                "exact candidate build completed through the frozen boundary",
            ))
        }
    }
}

fn witness(report: &AgentReport) -> Option<ScaleUpWitness> {
    Some(ScaleUpWitness {
        resource_uid: report.resource_uid.clone(),
        identity: report.identity.clone(),
        role: report.role,
        process_session_id: report.process_session_id.clone(),
        report_sequence: report.report_sequence,
        epoch: report.epoch,
        previous_configuration_id: report
            .previous_configuration
            .as_ref()
            .map(|configuration| configuration.configuration_id.clone()),
        current_configuration_id: report
            .current_configuration
            .as_ref()?
            .configuration_id
            .clone(),
        verified_replication_lsn: report.verified_replication_lsn?,
        write_status: report.write_status,
        pending_operation_id: report.pending_operation_id.clone(),
        retained_operation_id: report.retained_operation_id.clone(),
    })
}

fn exact_pc_cc_report(
    report: &AgentReport,
    intent: &ScaleUpIntent,
    current: &ConfigurationDescriptor,
) -> bool {
    report.epoch == current.epoch
        && report.previous_configuration.as_ref() == Some(&intent.previous_configuration)
        && report.current_configuration.as_ref() == Some(current)
        && report.scale_up_intent.as_deref() == Some(intent)
        && report
            .verified_replication_lsn
            .is_some_and(|lsn| lsn >= intent.catch_up_boundary_lsn)
}

fn exact_current_only_report(
    report: &AgentReport,
    intent: &ScaleUpIntent,
    current: &ConfigurationDescriptor,
) -> bool {
    report.epoch == current.epoch
        && report.previous_configuration.is_none()
        && report.current_configuration.as_ref() == Some(current)
        && report.scale_up_intent.as_deref() == Some(intent)
        && report
            .verified_replication_lsn
            .is_some_and(|lsn| lsn >= intent.catch_up_boundary_lsn)
}

fn quorum_witnesses(
    snapshot: &ObservationSnapshot,
    intent: &ScaleUpIntent,
    current: &ConfigurationDescriptor,
    eligible: &ConfigurationDescriptor,
    stage: ScaleUpStage,
) -> Vec<ScaleUpWitness> {
    eligible
        .members
        .iter()
        .filter_map(|member| {
            let report = report(snapshot, &member.identity)?;
            let exact = if stage == ScaleUpStage::CurrentOnly {
                exact_current_only_report(report, intent, current)
            } else {
                exact_pc_cc_report(report, intent, current)
            };
            let operation_id = intent.command_operation_id(stage, &member.identity, current);
            (exact
                && report.pending_operation_id.is_none()
                && report.retained_operation_id.as_ref() == Some(&operation_id))
            .then(|| witness(report))
            .flatten()
        })
        .collect()
}

fn recovery_witnesses(
    snapshot: &ObservationSnapshot,
    intent: &ScaleUpIntent,
    eligible: &ConfigurationDescriptor,
) -> Vec<ScaleUpWitness> {
    eligible
        .members
        .iter()
        .filter_map(|member| {
            let report = report(snapshot, &member.identity)?;
            let stage = if exact_pc_cc_report(report, intent, &intent.current_configuration) {
                ScaleUpStage::PreviousCurrent
            } else if exact_current_only_report(report, intent, &intent.current_configuration) {
                ScaleUpStage::CurrentOnly
            } else {
                return None;
            };
            let operation_id =
                intent.command_operation_id(stage, &member.identity, &intent.current_configuration);
            (report.pending_operation_id.is_none()
                && report.retained_operation_id.as_ref() == Some(&operation_id))
            .then(|| witness(report))
            .flatten()
        })
        .collect()
}

fn begin_failover(
    snapshot: &ObservationSnapshot,
    transition: &TransitionIntent,
    intent: &ScaleUpIntent,
    config: &EvaluationConfig,
) -> Plan {
    let current = &intent.current_configuration;
    let previous_witnesses = recovery_witnesses(snapshot, intent, &intent.previous_configuration);
    let current_witnesses = recovery_witnesses(snapshot, intent, current);
    if previous_witnesses.len() < intent.previous_policy.read_quorum as usize
        || current_witnesses.len() < intent.current_policy.read_quorum as usize
    {
        return wait(
            snapshot,
            snapshot.status.clone(),
            "ScaleUpFailoverEvidencePending",
            "failover-recovery",
            Some(&intent.target),
            Some(&intent.operation_id),
            "independent previous and current read-quorum witnesses are required",
            config,
        );
    }
    let candidate = current_witnesses
        .iter()
        .filter(|witness| witness.identity != intent.primary)
        .max_by(|left, right| {
            left.verified_replication_lsn
                .cmp(&right.verified_replication_lsn)
                .then_with(|| right.identity.replica_id.cmp(&left.identity.replica_id))
        })
        .cloned();
    let Some(candidate) = candidate else {
        return wait(
            snapshot,
            snapshot.status.clone(),
            "ScaleUpFailoverCandidatePending",
            "failover-recovery",
            Some(&intent.target),
            Some(&intent.operation_id),
            "no exact surviving current member can become primary",
            config,
        );
    };
    let Some(configuration_number) = current.epoch.configuration_number.checked_add(1) else {
        return unsafe_plan(
            snapshot.status.clone(),
            UnsafeReason::InvalidAcceptedAuthority("scale-up failover epoch exhausted".into()),
            config,
        );
    };
    let failover = configuration_with_primary(
        current,
        &candidate.identity,
        Epoch::new(current.epoch.data_loss_number, configuration_number),
    );
    let evidence = ScaleUpFailoverEvidence {
        intent: intent.clone(),
        previous_read_quorum: previous_witnesses,
        current_read_quorum: current_witnesses,
    };
    if let Err(error) = validate_scale_up_failover_evidence(&evidence) {
        return unsafe_plan(
            snapshot.status.clone(),
            UnsafeReason::ContradictoryReplicaEvidence(error.to_string()),
            config,
        );
    }
    let mut status = snapshot.status.clone();
    status.transition = Some(TransitionIntent {
        transition_id: intent.transition_id(TransitionKind::Failover, &failover),
        kind: TransitionKind::Failover,
        spec_generation: transition.spec_generation,
        effective_policy: intent.current_policy.clone(),
        previous_configuration_id: Some(intent.previous_configuration.configuration_id.clone()),
        current_configuration: failover,
        election_lsn: Some(candidate.verified_replication_lsn),
        build_id: Some(intent.build_id.clone()),
        repair: None,
        switchover: None,
        secondary_scale_down: None,
        secondary_removal_evidence: None,
        scale_up: None,
        scale_up_failover: Some(Box::new(evidence)),
    });
    status.primary_failure = None;
    let status = progress_status(
        snapshot,
        status,
        "ScaleUpFailoverPersisted",
        "failover-recovery",
        Some(&intent.target),
        Some(&intent.operation_id),
        "preserved original PC and expanded CC with independent recovery evidence",
    );
    let mut changes = Vec::new();
    if snapshot.routing.write_target.is_some() || snapshot.routing.unresolved_write_target {
        changes.push(KubernetesChange::RemoveWriteRouting);
    }
    changes.push(KubernetesChange::PersistStatus {
        status: Box::new(status),
    });
    Plan::Apply { changes }
}

pub(super) fn transition(
    snapshot: &ObservationSnapshot,
    transition: &TransitionIntent,
    config: &EvaluationConfig,
) -> Plan {
    let evidence = transition
        .scale_up
        .as_deref()
        .map(|intent| ScaleUpConfigurationEvidence::Admission {
            intent: intent.clone(),
        })
        .or_else(|| {
            transition.scale_up_failover.as_deref().map(|evidence| {
                ScaleUpConfigurationEvidence::Failover {
                    evidence: evidence.clone(),
                }
            })
        })
        .expect("scale-up transition branch");
    let intent = evidence.intent();
    let current = &transition.current_configuration;
    let Some(provisioning) = snapshot.status.provisioning.as_ref() else {
        return unsafe_plan(
            snapshot.status.clone(),
            UnsafeReason::InvalidAcceptedAuthority(
                "active scale-up transition lacks exact provisioning provenance".into(),
            ),
            config,
        );
    };
    let admission_started =
        snapshot.status.scale_up_admission_started.as_ref() == Some(&intent.operation_id);
    if transition.kind == TransitionKind::ScaleUp && replica_failed(snapshot, &intent.primary) {
        if !admission_started {
            return maybe_begin_stable_failover(snapshot, snapshot.status.clone(), config)
                .unwrap_or_else(|| {
                    wait(
                        snapshot,
                        snapshot.status.clone(),
                        "ScaleUpFailoverArbitrationPending",
                        "failover-recovery",
                        Some(&intent.target),
                        Some(&intent.operation_id),
                        "accepted primary recovery is pending before scale-up admission",
                        config,
                    )
                });
        }
        return begin_failover(snapshot, transition, intent, config);
    }
    if transition.kind == TransitionKind::ScaleUp
        && !admission_started
        && snapshot.desired.replicas <= intent.previous_policy.replica_set_size
    {
        return freeze_cleanup(
            snapshot,
            provisioning,
            snapshot.status.clone(),
            "ScaleUpCancelledBeforePreviousCurrent",
            false,
            config,
        );
    }
    if transition.kind == TransitionKind::ScaleUp && !admission_started {
        let mut status = snapshot.status.clone();
        status.scale_up_admission_started = Some(intent.operation_id.clone());
        return persist(progress_status(
            snapshot,
            status,
            "ScaleUpAdmissionStarted",
            "pc-cc",
            Some(&intent.target),
            Some(&intent.operation_id),
            "persisted monotonic admission fence before the first PC/CC command",
        ));
    }

    let primary = configuration_primary(current);
    let current_only_started = current.members.iter().any(|member| {
        report(snapshot, &member.identity)
            .is_some_and(|report| exact_current_only_report(report, intent, current))
    });
    if !current_only_started {
        for member in current
            .members
            .iter()
            .filter(|member| member.identity != primary.identity)
            .chain(std::iter::once(primary))
        {
            let Some(report) = report(snapshot, &member.identity) else {
                if transition.kind == TransitionKind::Failover {
                    continue;
                }
                if member.identity == intent.target && !admission_started {
                    return freeze_cleanup(
                        snapshot,
                        provisioning,
                        snapshot.status.clone(),
                        "ScaleUpCandidateDisappearedBeforeAdmission",
                        false,
                        config,
                    );
                }
                if member.identity != primary.identity {
                    continue;
                }
                return wait(
                    snapshot,
                    snapshot.status.clone(),
                    "ScaleUpParticipantPending",
                    "pc-cc",
                    Some(&intent.target),
                    Some(&intent.operation_id),
                    "an exact participant is unavailable",
                    config,
                );
            };
            let operation_id = intent.command_operation_id(
                ScaleUpStage::PreviousCurrent,
                &member.identity,
                current,
            );
            if !exact_pc_cc_report(report, intent, current)
                || report.pending_operation_id.is_some()
                || report.retained_operation_id.as_ref() != Some(&operation_id)
            {
                if report.pending_operation_id.is_some() {
                    return wait(
                        snapshot,
                        snapshot.status.clone(),
                        "ScaleUpCommandPending",
                        "pc-cc",
                        Some(&intent.target),
                        Some(&intent.operation_id),
                        "one exact authority command remains pending",
                        config,
                    );
                }
                return Plan::Execute {
                    command: ProtocolCommand::EnsureConfiguration(Box::new(configuration_command(
                        evidence.clone(),
                        current,
                        member,
                        false,
                        transition.election_lsn,
                    ))),
                };
            }
        }
        let previous_witnesses = quorum_witnesses(
            snapshot,
            intent,
            current,
            &intent.previous_configuration,
            ScaleUpStage::PreviousCurrent,
        );
        let current_witnesses = quorum_witnesses(
            snapshot,
            intent,
            current,
            current,
            ScaleUpStage::PreviousCurrent,
        );
        let candidate_ready = current_witnesses.iter().any(|witness| {
            witness.identity == intent.target
                && witness.verified_replication_lsn >= intent.catch_up_boundary_lsn
        });
        let primary_ready = current_witnesses.iter().any(|witness| {
            witness.identity == primary.identity
                && (transition.kind == TransitionKind::Failover
                    || witness.write_status == AccessStatus::Granted)
        });
        let previous_quorum = if transition.kind == TransitionKind::Failover {
            intent.previous_policy.read_quorum
        } else {
            intent.previous_policy.write_quorum
        };
        let current_quorum = if transition.kind == TransitionKind::Failover {
            intent.current_policy.read_quorum
        } else {
            intent.current_policy.write_quorum
        };
        if previous_witnesses.len() < previous_quorum as usize
            || current_witnesses.len() < current_quorum as usize
        {
            return quorum_wait(
                snapshot,
                snapshot.status.clone(),
                "ScaleUpDualQuorumUnavailable",
                "pc-cc",
                &intent.target,
                &intent.operation_id,
                "previous and current configuration quorum evidence is insufficient",
                config,
            );
        }
        if !candidate_ready || !primary_ready {
            return wait(
                snapshot,
                snapshot.status.clone(),
                "ScaleUpDualQuorumPending",
                "catch-up",
                Some(&intent.target),
                Some(&intent.operation_id),
                "both configurations, the exact candidate boundary, and writable primary are required",
                config,
            );
        }
    }

    if let Some(plan) = publish_phase_if_changed(
        snapshot,
        &snapshot.status,
        "ScaleUpCurrentOnlyInstalling",
        "current-only",
        Some(&intent.target),
        Some(&intent.operation_id),
        "installing expanded current-only authority one exact member at a time",
    ) {
        return plan;
    }

    for member in current
        .members
        .iter()
        .filter(|member| member.identity != primary.identity)
        .chain(std::iter::once(primary))
    {
        let Some(report) = report(snapshot, &member.identity) else {
            continue;
        };
        let operation_id =
            intent.command_operation_id(ScaleUpStage::CurrentOnly, &member.identity, current);
        if !exact_current_only_report(report, intent, current)
            || report.pending_operation_id.is_some()
            || report.retained_operation_id.as_ref() != Some(&operation_id)
        {
            if report.pending_operation_id.is_some() {
                continue;
            }
            return Plan::Execute {
                command: ProtocolCommand::EnsureConfiguration(Box::new(configuration_command(
                    evidence.clone(),
                    current,
                    member,
                    true,
                    transition.election_lsn,
                ))),
            };
        }
    }

    let current_only_witnesses = quorum_witnesses(
        snapshot,
        intent,
        current,
        current,
        ScaleUpStage::CurrentOnly,
    );
    let primary_ready = current_only_witnesses.iter().any(|witness| {
        witness.identity == primary.identity && witness.write_status == AccessStatus::Granted
    });
    if current_only_witnesses.len() >= intent.current_policy.write_quorum as usize && primary_ready
    {
        let receipt = ScaleUpReceipt {
            intent: intent.clone(),
            accepted_configuration: current.clone(),
            failover_evidence: match &evidence {
                ScaleUpConfigurationEvidence::Admission { .. } => None,
                ScaleUpConfigurationEvidence::Failover { evidence } => Some(evidence.clone()),
            },
            current_only_write_quorum: current_only_witnesses,
        };
        if let Err(error) = validate_scale_up_receipt(&receipt) {
            return unsafe_plan(
                snapshot.status.clone(),
                UnsafeReason::ContradictoryReplicaEvidence(error.to_string()),
                config,
            );
        }
        let candidate_ready = report(snapshot, &intent.target).is_some_and(|report| {
            exact_current_only_report(report, intent, current)
                && report.role == ReplicaRole::ActiveSecondary
        });
        let mut accepted = clear_evaluator_conditions(snapshot.status.clone());
        accepted.observed_generation = transition.spec_generation;
        accepted.effective_policy = Some(intent.current_policy.clone());
        accepted.topology = Some(AcceptedTopology {
            configuration: current.clone(),
        });
        accepted.provisioning = None;
        accepted.transition = None;
        accepted.scale_up_cleanup = None;
        accepted.last_scale_up = Some(Box::new(receipt));
        accepted.scale_up_admission_started = None;
        accepted.primary_failure = None;
        accepted.quorum_loss = None;
        return persist(progress_status(
            snapshot,
            accepted,
            if candidate_ready {
                "ScaleUpTopologyAccepted"
            } else {
                "ScaleUpCommittedDegraded"
            },
            if candidate_ready {
                "current-only"
            } else {
                "local-convergence"
            },
            Some(&intent.target),
            Some(&intent.operation_id),
            if candidate_ready {
                "expanded topology committed; stable routing and fresh local evidence remain"
            } else {
                "expanded topology committed without exact candidate local acceptance"
            },
        ));
    }
    wait(
        snapshot,
        snapshot.status.clone(),
        "ScaleUpCurrentOnlyQuorumPending",
        "current-only",
        Some(&intent.target),
        Some(&intent.operation_id),
        "expanded current-only write quorum including the primary is required",
        config,
    )
}
