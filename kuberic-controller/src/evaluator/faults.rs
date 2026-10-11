use kuberic_runtime::protocol::command::{
    KubernetesChange, ProtocolCommand, RestartReplicaProcess,
};
use kuberic_runtime::protocol::observation::{AgentObservation, ObservationSnapshot};
use kuberic_runtime::protocol::public_operations::{
    FrozenReplicaResources, PreviewLifecycleBinding, PublicFaultAction, PublicFaultActionKind,
    PublicServiceClear, PublicServiceClearStage, RestartActionStage,
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
    if !snapshot.observation_failures.is_empty() {
        return Some(Plan::Wait {
            reason: crate::plan::WaitReason::AgentUnavailable,
            status: snapshot.status.clone().with_condition(StatusCondition {
                type_: "Ready".into(),
                status: ConditionStatus::False,
                reason: "FaultReobservationRequired".into(),
                message: "fault cleanup waits for complete resource observation".into(),
            }),
            requeue_after_seconds: config.wait_requeue_seconds,
        });
    }
    if let Err(message) = validate_present_preview_reports(snapshot, preview, &binding) {
        return Some(reject(
            snapshot,
            "MixedPreviewLegacyReports",
            message,
            config,
        ));
    }
    if let Some(action) = snapshot
        .status
        .public_fault_action
        .as_ref()
        .filter(|action| action.kind == PublicFaultActionKind::DropReplacement)
    {
        if action.binding != binding || action.validate().is_err() {
            return Some(reject(
                snapshot,
                "FaultActionChanged",
                "accepted drop action differs from the current preview binding",
                config,
            ));
        }
        if snapshot.replicas.values().any(|replica| {
            matches!(
                &replica.agent,
                AgentObservation::Report(report)
                    if report.identity == action.target
                        && (report.process_session_id != action.predecessor_session
                            || report
                                .public_lifecycle_report
                                .as_deref()
                                .is_some_and(|lifecycle| {
                                    lifecycle.process_id != action.predecessor_process_id
                                }))
            )
        }) {
            return Some(reject(
                snapshot,
                "FaultCleanupSuccessorPresent",
                "predecessor-bound cleanup cannot target a successor process",
                config,
            ));
        }
        if let Some(plan) = service_clear_plan(snapshot, action) {
            return Some(plan);
        }
        return Some(evaluate_drop(snapshot, action));
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
            || lifecycle.binding.as_ref() != Some(&binding)
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
        if snapshot.status.public_fault_action.is_none()
            && snapshot.status.last_public_fault_action.is_some()
        {
            return Some(Plan::Stable {
                status: snapshot.status.clone(),
                requeue_after_seconds: config.stable_resync_seconds,
            });
        }
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
    let access_closed = report.read_status != AccessStatus::Granted
        && report.write_status != AccessStatus::Granted
        && report.role == ReplicaRole::None
        && !lifecycle.write_access
        && lifecycle.role == ReplicaRole::None
        && lifecycle.service_location.is_none();
    if !access_closed {
        return Some(reject(
            snapshot,
            "FaultClosureIncomplete",
            "fault or restart report still exposes access, role, or service location",
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
            && record.successor_process_id == Some(lifecycle.process_id)
            && report.process_session_id != accepted.predecessor_session
            && kubernetes.pod_uid.as_ref() == Some(&accepted.resources.pod_uid)
            && kubernetes.pvc_uid.as_ref() == Some(&accepted.resources.pvc_uid)
        {
            return Some(complete_fault_plan(snapshot, accepted));
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
    if report.healthy {
        return Some(reject(
            snapshot,
            "FaultClosureIncomplete",
            "faulted incarnation still reports healthy",
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
    let Some(endpoint_name) = &kubernetes.endpoint_name else {
        return Some(reject(
            snapshot,
            "FaultEndpointIdentityMissing",
            "fault action requires the exact endpoint name",
            config,
        ));
    };
    let Some(endpoint_uid) = &kubernetes.endpoint_uid else {
        return Some(reject(
            snapshot,
            "FaultEndpointIdentityMissing",
            "fault action requires the exact endpoint UID",
            config,
        ));
    };
    let Some(endpoint_resource_version) = &kubernetes.endpoint_resource_version else {
        return Some(reject(
            snapshot,
            "FaultEndpointIdentityMissing",
            "fault action requires the exact endpoint resource version",
            config,
        ));
    };
    let Some(fault_operation_id) = &lifecycle.operation_id else {
        return Some(reject(
            snapshot,
            "FaultOperationIdentityMissing",
            "fault report requires the exact durable operation ID",
            config,
        ));
    };
    let kind = PublicFaultAction::expected_kind(persistence, fault);
    let mut action = PublicFaultAction {
        action_id: OperationId::new("pending"),
        fault_operation_id: fault_operation_id.clone(),
        binding,
        target: report.identity.clone(),
        resources: FrozenReplicaResources {
            pod_name: kubernetes.pod_name.clone(),
            pod_uid: pod_uid.clone(),
            pvc_name: kubernetes.pvc_name.clone(),
            pvc_uid: pvc_uid.clone(),
            endpoint_name: endpoint_name.clone(),
            endpoint_uid: endpoint_uid.clone(),
            endpoint_resource_version: endpoint_resource_version.clone(),
        },
        predecessor_session: report.process_session_id.clone(),
        predecessor_process_id: lifecycle.process_id,
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

    if let Some(plan) = service_clear_plan(snapshot, &action) {
        return Some(plan);
    }

    Some(match action.kind {
        PublicFaultActionKind::Restart => {
            if let Some(record) = report.restart_action.as_deref() {
                let historical = snapshot.status.last_public_fault_action.as_ref()
                    == Some(&record.action)
                    && record.stage == RestartActionStage::SuccessorStarted
                    && record.successor_session.as_ref() == Some(&action.predecessor_session)
                    && record.successor_process_id == Some(action.predecessor_process_id);
                if record.action != action && !historical {
                    return Some(reject(
                        snapshot,
                        "RestartHandshakeMismatch",
                        "agent restart record differs from the accepted controller action",
                        config,
                    ));
                }
                if record.action == action && record.stage == RestartActionStage::SuccessorStarted {
                    return Some(reject(
                        snapshot,
                        "RestartSuccessorNotObserved",
                        "faulted predecessor report cannot complete successor startup",
                        config,
                    ));
                }
            }
            Plan::Execute {
                command: ProtocolCommand::RestartReplicaProcess(Box::new(RestartReplicaProcess {
                    action,
                })),
            }
        }
        PublicFaultActionKind::DropReplacement => evaluate_drop(snapshot, &action),
    })
}

fn validate_present_preview_reports(
    snapshot: &ObservationSnapshot,
    preview: &kuberic_runtime::protocol::public_operations::PublicOperationPreviewIdentity,
    binding: &PreviewLifecycleBinding,
) -> Result<(), &'static str> {
    for replica in snapshot.replicas.values() {
        let report = match &replica.agent {
            AgentObservation::Report(report) => report,
            AgentObservation::Invalid { .. } => {
                return Err("preview cleanup refuses invalid normalized evidence");
            }
            AgentObservation::Absent
            | AgentObservation::Unreachable { .. }
            | AgentObservation::Uninitialized(_) => continue,
        };
        if replica.kubernetes.is_none() {
            return Err("preview report has no exact Kubernetes scaffolding");
        }
        let Some(lifecycle) = report.public_lifecycle_report.as_deref() else {
            return Err("preview cleanup refuses a legacy report");
        };
        if lifecycle.preview != *preview
            || lifecycle.binding.as_ref() != Some(binding)
            || lifecycle.resource_uid != snapshot.resource_uid
            || lifecycle.replica != report.identity
            || lifecycle.process_session_id != report.process_session_id
        {
            return Err("preview cleanup report identity or binding changed");
        }
    }
    Ok(())
}

fn service_clear_plan(snapshot: &ObservationSnapshot, action: &PublicFaultAction) -> Option<Plan> {
    let clear = match &snapshot.status.public_service_clear {
        Some(clear) if clear.action_id == action.action_id => clear,
        _ => {
            let mut status = snapshot.status.clone();
            status.public_service_clear = Some(PublicServiceClear {
                action_id: action.action_id.clone(),
                stage: PublicServiceClearStage::Pending,
                service_uid: None,
                service_resource_version: None,
            });
            return Some(Plan::Apply {
                changes: vec![KubernetesChange::PersistStatus {
                    status: Box::new(status),
                }],
            });
        }
    };
    if snapshot.routing.write_target.as_ref() == Some(&action.target)
        || snapshot.routing.unresolved_write_target
        || snapshot.routing.preview_service_location_present
    {
        return Some(Plan::Apply {
            changes: vec![KubernetesChange::RemoveWriteRouting],
        });
    }
    if clear.stage == PublicServiceClearStage::Pending {
        let mut status = snapshot.status.clone();
        status.public_service_clear = Some(PublicServiceClear {
            action_id: action.action_id.clone(),
            stage: PublicServiceClearStage::PublishedAbsent,
            service_uid: snapshot.routing.write_service_uid.clone(),
            service_resource_version: snapshot.routing.write_service_resource_version.clone(),
        });
        return Some(Plan::Apply {
            changes: vec![KubernetesChange::PersistStatus {
                status: Box::new(status),
            }],
        });
    }
    if clear.service_uid != snapshot.routing.write_service_uid
        || clear.service_resource_version != snapshot.routing.write_service_resource_version
    {
        let mut status = snapshot.status.clone();
        status.public_service_clear = Some(PublicServiceClear {
            action_id: action.action_id.clone(),
            stage: PublicServiceClearStage::Pending,
            service_uid: None,
            service_resource_version: None,
        });
        return Some(Plan::Apply {
            changes: vec![KubernetesChange::PersistStatus {
                status: Box::new(status),
            }],
        });
    }
    None
}

fn evaluate_drop(snapshot: &ObservationSnapshot, action: &PublicFaultAction) -> Plan {
    if snapshot.routing.write_target.as_ref() == Some(&action.target)
        || snapshot.routing.unresolved_write_target
    {
        return Plan::Apply {
            changes: vec![KubernetesChange::RemoveWriteRouting],
        };
    }
    let old_pod_or_pvc_present = snapshot.replicas.values().any(|replica| {
        replica.kubernetes.as_ref().is_some_and(|kubernetes| {
            kubernetes.pod_uid.as_ref() == Some(&action.resources.pod_uid)
                || kubernetes.pvc_uid.as_ref() == Some(&action.resources.pvc_uid)
        })
    });
    let old_endpoint_present = snapshot
        .routing
        .service_identities
        .iter()
        .any(|(name, uid)| {
            name == &action.resources.endpoint_name && uid == &action.resources.endpoint_uid
        });
    if old_pod_or_pvc_present || old_endpoint_present {
        return Plan::Apply {
            changes: vec![
                KubernetesChange::DeleteExactService {
                    name: action.resources.endpoint_name.clone(),
                    uid: action.resources.endpoint_uid.clone(),
                    resource_version: action.resources.endpoint_resource_version.clone(),
                },
                KubernetesChange::DeleteReplicaScaffolding {
                    pod_name: Some(action.resources.pod_name.clone()),
                    pod_uid: Some(action.resources.pod_uid.clone()),
                    pvc_name: Some(action.resources.pvc_name.clone()),
                    pvc_uid: Some(action.resources.pvc_uid.clone()),
                },
            ],
        };
    }
    let replacement_ready = snapshot.replicas.values().any(|replica| {
        replica.kubernetes.as_ref().is_some_and(|kubernetes| {
            kubernetes.replica_id == action.target.replica_id
                && kubernetes
                    .pod_uid
                    .as_ref()
                    .is_some_and(|uid| uid != &action.resources.pod_uid)
                && kubernetes
                    .pvc_uid
                    .as_ref()
                    .is_some_and(|uid| uid != &action.resources.pvc_uid)
                && kubernetes.endpoint_uid.is_some()
        })
    });
    if replacement_ready {
        return complete_fault_plan(snapshot, action);
    }
    Plan::Apply {
        changes: vec![KubernetesChange::EnsureReplacementScaffolding {
            replica_id: action.target.replica_id,
            replacing: action.target.clone(),
        }],
    }
}

fn complete_fault_plan(snapshot: &ObservationSnapshot, action: &PublicFaultAction) -> Plan {
    let mut status = unhealthy_status(snapshot.status.clone(), action.fault);
    status.last_public_fault_action = Some(action.clone());
    status.public_fault_action = None;
    status.public_service_clear = None;
    Plan::Apply {
        changes: vec![KubernetesChange::PersistStatus {
            status: Box::new(status),
        }],
    }
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
