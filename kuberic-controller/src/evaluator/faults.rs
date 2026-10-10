use kuberic_runtime::protocol::command::{
    KubernetesChange, ProtocolCommand, RestartReplicaProcess,
};
use kuberic_runtime::protocol::observation::{AgentObservation, ObservationSnapshot};
use kuberic_runtime::protocol::public_operations::{
    FrozenReplicaResources, PreviewLifecycleBinding, PublicFaultAction, PublicFaultActionKind,
    RestartActionStage,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, ConditionStatus, FaultType, OperationId, ReplicaRole, StatusCondition,
};

use crate::plan::{Plan, UnsafeReason};

use super::EvaluationConfig;

pub(super) fn evaluate(snapshot: &ObservationSnapshot, config: &EvaluationConfig) -> Option<Plan> {
    let preview = config.public_operation_preview.as_ref()?;
    let persistence = match snapshot.desired.preview_lifecycle {
        Some(persistence) => persistence,
        None => {
            return Some(reject(
                snapshot,
                "PreviewClassificationRequired",
                "previewLifecycle.statePersistence must be explicit",
                config,
            ));
        }
    };
    let binding = PreviewLifecycleBinding {
        preview: preview.clone(),
        resource_uid: snapshot.resource_uid.clone(),
        spec_generation: snapshot.desired.generation,
        state_persistence: persistence,
    };
    if binding.validate().is_err() {
        return Some(reject(
            snapshot,
            "InvalidPreviewIdentity",
            "preview lifecycle binding is incomplete",
            config,
        ));
    }
    match &snapshot.status.preview_lifecycle {
        None => {
            let mut status = snapshot.status.clone();
            status.preview_lifecycle = Some(binding);
            status.observed_generation = snapshot.desired.generation;
            return Some(Plan::Apply {
                changes: vec![KubernetesChange::PersistStatus {
                    status: Box::new(status),
                }],
            });
        }
        Some(accepted) if accepted != &binding => {
            return Some(reject(
                snapshot,
                "PreviewIdentityChanged",
                "accepted preview identity, resource UID, generation, or persistence changed",
                config,
            ));
        }
        Some(_) => {}
    }

    let mut reports = Vec::new();
    for replica in snapshot.replicas.values() {
        let AgentObservation::Report(report) = &replica.agent else {
            continue;
        };
        let Some(kubernetes) = replica.kubernetes.as_ref() else {
            return Some(reject(
                snapshot,
                "PreviewScaffoldingRequired",
                "preview report has no exact Kubernetes scaffolding",
                config,
            ));
        };
        let Some(lifecycle) = report.public_lifecycle_report.as_deref() else {
            return Some(reject(
                snapshot,
                "MixedPreviewLegacyReports",
                "preview controller refuses a legacy report in the preview observation",
                config,
            ));
        };
        if lifecycle.preview != *preview
            || lifecycle.resource_uid != snapshot.resource_uid
            || lifecycle.replica != report.identity
            || lifecycle.process_session_id != report.process_session_id
        {
            return Some(reject(
                snapshot,
                "PreviewReportMismatch",
                "public lifecycle report does not match the selected preview incarnation",
                config,
            ));
        }
        reports.push((kubernetes, report.as_ref(), lifecycle));
    }
    if reports.is_empty() {
        return Some(reject(
            snapshot,
            "PreviewReportRequired",
            "preview controller requires exact public lifecycle reports",
            config,
        ));
    }
    let mut candidates = reports.into_iter().filter(|(_, report, _)| {
        snapshot.status.public_fault_action.as_ref().map_or_else(
            || report.reported_fault.is_some(),
            |action| {
                report.identity == action.target
                    && (report.reported_fault.is_some() || report.restart_action.is_some())
            },
        )
    });
    let Some((kubernetes, report, lifecycle)) = candidates.next() else {
        if snapshot.status.public_fault_action.is_some() {
            return Some(reject(
                snapshot,
                "FaultEvidenceDisappeared",
                "accepted fault action has no exact predecessor or successor report",
                config,
            ));
        }
        return Some(Plan::Stable {
            status: snapshot.status.clone(),
            requeue_after_seconds: config.stable_resync_seconds,
        });
    };
    if candidates.next().is_some() {
        return Some(reject(
            snapshot,
            "ConflictingFaultReports",
            "multiple preview reports claim the accepted fault action",
            config,
        ));
    }
    let Some(fault) = report.reported_fault else {
        if let (Some(accepted), Some(record)) = (
            snapshot.status.public_fault_action.as_ref(),
            report.restart_action.as_deref(),
        ) && record.action == *accepted
            && record.stage == RestartActionStage::SuccessorStarted
            && record.successor_session.as_ref() == Some(&report.process_session_id)
            && report.process_session_id != accepted.predecessor_session
        {
            return Some(Plan::Stable {
                status: unhealthy_status(snapshot.status.clone(), accepted.fault),
                requeue_after_seconds: config.stable_resync_seconds,
            });
        }
        if snapshot.status.public_fault_action.is_some() {
            return Some(reject(
                snapshot,
                "FaultEvidenceDisappeared",
                "accepted fault action cannot be cleared by a predecessor report",
                config,
            ));
        }
        return Some(Plan::Stable {
            status: snapshot.status.clone(),
            requeue_after_seconds: config.stable_resync_seconds,
        });
    };
    if report.healthy
        || report.read_status == AccessStatus::Granted
        || report.write_status == AccessStatus::Granted
        || lifecycle.write_access
        || lifecycle.role != ReplicaRole::None
        || lifecycle.service_location.is_some()
    {
        return Some(reject(
            snapshot,
            "FaultClosureIncomplete",
            "faulted incarnation still exposes readiness, access, role, or service location",
            config,
        ));
    }
    let Some(pod_uid) = &kubernetes.pod_uid else {
        return Some(reject(
            snapshot,
            "FaultPodIdentityMissing",
            "fault action requires the exact Pod UID",
            config,
        ));
    };
    let Some(pvc_uid) = &kubernetes.pvc_uid else {
        return Some(reject(
            snapshot,
            "FaultPvcIdentityMissing",
            "fault action requires the exact PVC UID",
            config,
        ));
    };
    let kind = PublicFaultAction::expected_kind(persistence, fault);
    let mut action = PublicFaultAction {
        action_id: OperationId::new("pending"),
        binding,
        target: report.identity.clone(),
        resources: FrozenReplicaResources {
            pod_name: kubernetes.pod_name.clone(),
            pod_uid: pod_uid.clone(),
            pvc_name: kubernetes.pvc_name.clone(),
            pvc_uid: pvc_uid.clone(),
        },
        predecessor_session: report.process_session_id.clone(),
        fault_revision: lifecycle.revision,
        fault,
        kind,
    };
    action.action_id = action.expected_id();
    if let Err(error) = kuberic_runtime::protocol::validation::validate_public_fault_action(
        &action,
        &action.binding,
        &report.identity,
        &report.process_session_id,
        fault,
    ) {
        return Some(reject(
            snapshot,
            "InvalidFaultAction",
            &error.to_string(),
            config,
        ));
    }

    match &snapshot.status.public_fault_action {
        None => {
            let mut status = snapshot.status.clone();
            status.public_fault_action = Some(action);
            status = unhealthy_status(status, fault);
            return Some(Plan::Apply {
                changes: vec![KubernetesChange::PersistStatus {
                    status: Box::new(status),
                }],
            });
        }
        Some(existing) if existing != &action => {
            let escalation = existing.target == action.target
                && existing.predecessor_session == action.predecessor_session
                && existing.fault == FaultType::Transient
                && action.fault == FaultType::Permanent;
            if escalation {
                let mut status = snapshot.status.clone();
                status.public_fault_action = Some(action);
                status = unhealthy_status(status, FaultType::Permanent);
                return Some(Plan::Apply {
                    changes: vec![KubernetesChange::PersistStatus {
                        status: Box::new(status),
                    }],
                });
            }
            return Some(reject(
                snapshot,
                "FaultActionChanged",
                "fault action cannot retarget another incarnation or session",
                config,
            ));
        }
        Some(_) => {}
    }

    if snapshot.routing.write_target.as_ref() == Some(&action.target)
        || snapshot.routing.unresolved_write_target
    {
        return Some(Plan::Apply {
            changes: vec![KubernetesChange::RemoveWriteRouting],
        });
    }

    Some(match action.kind {
        PublicFaultActionKind::Restart => {
            if let Some(record) = report.restart_action.as_deref() {
                if record.action != action {
                    return Some(reject(
                        snapshot,
                        "RestartHandshakeMismatch",
                        "agent restart record differs from the accepted controller action",
                        config,
                    ));
                }
                if record.stage == RestartActionStage::SuccessorStarted {
                    if record.successor_session.as_ref().is_none_or(|session| {
                        session.is_empty() || session == &action.predecessor_session
                    }) {
                        return Some(reject(
                            snapshot,
                            "RestartSuccessorInvalid",
                            "successor-started evidence does not name a fresh session",
                            config,
                        ));
                    }
                    return Some(Plan::Stable {
                        status: unhealthy_status(snapshot.status.clone(), fault),
                        requeue_after_seconds: config.stable_resync_seconds,
                    });
                }
            }
            Plan::Execute {
                command: ProtocolCommand::RestartReplicaProcess(Box::new(RestartReplicaProcess {
                    action,
                })),
            }
        }
        PublicFaultActionKind::DropReplacement => {
            let old_present = snapshot
                .replicas
                .values()
                .any(|replica| match &replica.agent {
                    AgentObservation::Report(candidate) => candidate.identity == action.target,
                    _ => {
                        replica
                            .kubernetes
                            .as_ref()
                            .and_then(|kubernetes| kubernetes.pod_uid.as_ref())
                            == Some(&action.resources.pod_uid)
                    }
                });
            if !old_present
                && snapshot.replicas.values().any(|replica| {
                    replica.kubernetes.as_ref().is_some_and(|kubernetes| {
                        kubernetes.replica_id == action.target.replica_id
                            && kubernetes.pod_uid.as_ref() != Some(&action.resources.pod_uid)
                    })
                })
            {
                Plan::Stable {
                    status: unhealthy_status(snapshot.status.clone(), fault),
                    requeue_after_seconds: config.stable_resync_seconds,
                }
            } else {
                Plan::Apply {
                    changes: vec![
                        KubernetesChange::DeleteReplicaEndpoint {
                            identity: action.target.clone(),
                        },
                        KubernetesChange::DeleteReplicaScaffolding {
                            pod_name: Some(action.resources.pod_name.clone()),
                            pod_uid: Some(action.resources.pod_uid.clone()),
                            pvc_name: Some(action.resources.pvc_name.clone()),
                            pvc_uid: Some(action.resources.pvc_uid.clone()),
                        },
                        KubernetesChange::EnsureReplacementScaffolding {
                            replica_id: action.target.replica_id,
                            replacing: action.target,
                        },
                    ],
                }
            }
        }
    })
}

fn unhealthy_status(
    mut status: kuberic_runtime::protocol::types::AcceptedStatus,
    fault: FaultType,
) -> kuberic_runtime::protocol::types::AcceptedStatus {
    status = status.with_condition(StatusCondition {
        type_: "Ready".into(),
        status: ConditionStatus::False,
        reason: "PublicOperationFault".into(),
        message: format!("{fault:?} fault excludes the exact incarnation"),
    });
    status.with_condition(StatusCondition {
        type_: "Progressing".into(),
        status: ConditionStatus::True,
        reason: "PublicOperationFaultContainment".into(),
        message: "Routing is removed before restart or replacement".into(),
    })
}

fn reject(
    snapshot: &ObservationSnapshot,
    reason: &str,
    message: &str,
    config: &EvaluationConfig,
) -> Plan {
    let status = snapshot.status.clone().with_condition(StatusCondition {
        type_: "Ready".into(),
        status: ConditionStatus::False,
        reason: reason.into(),
        message: message.into(),
    });
    Plan::Unsafe {
        reason: UnsafeReason::InvalidAcceptedAuthority(message.into()),
        status,
        safety_changes: if snapshot.routing.write_target.is_some()
            || snapshot.routing.unresolved_write_target
        {
            vec![kuberic_runtime::protocol::command::SafetyChange::RemoveWriteRouting]
        } else {
            Vec::new()
        },
        requeue_after_seconds: config.unsafe_requeue_seconds,
    }
}
