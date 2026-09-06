//! In-process operator effect preparation, dispatch, and recovery policy.
//!
//! This module does not own a worker or scheduler. It freezes commands from
//! authoritative observations and delegates the only correlated mutation to
//! `ReplicaHandle`; `ReplicaAgent` remains the admission, fencing, and replay
//! owner.

#[cfg(feature = "durable-switchover-pilot")]
use std::collections::BTreeMap;

use kuberic_core::driver::ReplicaHandle;
use kuberic_core::error::KubericError;
use kuberic_core::types::{
    AgentControlVersion, AgentGeneration, CorrelatedControlActionAcknowledgement,
    CorrelatedControlActionRequest, DurableActionState, DurableReplicaAction, Epoch, ReplicaId,
    ReplicaInstanceId, ReplicaStatusInfo,
};
use serde::{Deserialize, Serialize};

#[cfg(feature = "durable-switchover-pilot")]
use crate::cluster_api::ClusterApi;
#[cfg(feature = "durable-switchover-pilot")]
use crate::crd::DurableOperationPhase;
use crate::crd::{DurableOperationStatus, EpochStatus, PendingActionStatus};

#[cfg(feature = "durable-switchover-pilot")]
use super::pilot::{
    DurableSwitchoverState, DurableSwitchoverStepResult, PilotAdapterDecision, PilotPermitGuard,
};
#[cfg(feature = "durable-switchover-pilot")]
use super::{Decision, OperationObservations, switchover::is_switchover_postcondition_transition};
use super::{correlated_action_observation, fail_closed, record_activity_error};

const MAX_EFFECT_DIAGNOSTIC_BYTES: usize = 512;

/// Exact compact command persisted before one correlated replica dispatch.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplicaEffectCommand {
    pub action_id: String,
    pub target_id: ReplicaId,
    pub target_instance_id: String,
    pub expected_agent_generation: String,
    pub expected_control_version: u64,
    pub observed_runtime_epoch: EpochStatus,
    pub action_payload: String,
}

impl ReplicaEffectCommand {
    pub fn from_pending(pending: &PendingActionStatus) -> Result<Self, String> {
        kuberic_core::grpc::convert::decode_direct_correlated_action_payload(
            &pending.dispatch_action_payload,
        )
        .map_err(|error| format!("decode frozen correlated action: {error}"))?;
        Ok(Self {
            action_id: pending.action_id.clone(),
            target_id: pending.target_id,
            target_instance_id: pending.target_instance_id.clone(),
            expected_agent_generation: pending
                .dispatch_agent_generation
                .clone()
                .ok_or_else(|| "correlated command is missing agent generation".to_string())?,
            expected_control_version: pending
                .dispatch_agent_control_version
                .ok_or_else(|| "correlated command is missing control version".to_string())?,
            observed_runtime_epoch: pending
                .dispatch_observed_runtime_epoch
                .clone()
                .ok_or_else(|| "correlated command is missing runtime epoch".to_string())?,
            action_payload: pending.dispatch_action_payload.clone(),
        })
    }

    fn from_action(
        pending: &PendingActionStatus,
        action: &DurableReplicaAction,
    ) -> Result<Self, String> {
        let action_payload =
            kuberic_core::grpc::convert::encode_direct_correlated_action_payload(action)
                .map_err(|error| format!("encode frozen correlated action: {error}"))?;
        let mut pending = pending.clone();
        pending.dispatch_action_payload = action_payload;
        Self::from_pending(&pending)
    }
}

/// Exact UID-fenced pod-label command persisted before patch dispatch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LabelEffectCommand {
    pub target_id: ReplicaId,
    pub pod_name: String,
    pub expected_uid: String,
    pub role: String,
}

/// Portable outcome vocabulary exposed by operator effect adapters.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "status",
    content = "detail",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DurableEffectOutcome<T> {
    Applied(T),
    DefiniteFailure { class: String, message: String },
    ProvenNoAdmission { reason: String },
    Pending,
    Unknown { reason: String },
}

impl<T> DurableEffectOutcome<T> {
    pub fn definite_failure(class: &str, message: &str) -> Self {
        Self::DefiniteFailure {
            class: bounded(class),
            message: bounded(message),
        }
    }

    pub fn proven_no_admission(reason: &str) -> Self {
        Self::ProvenNoAdmission {
            reason: bounded(reason),
        }
    }

    pub fn unknown(reason: &str) -> Self {
        Self::Unknown {
            reason: bounded(reason),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchFailureDisposition {
    ProvenNoAdmission,
    DefiniteFailure,
    Unknown,
}

pub(crate) enum DispatchEvidencePlan {
    Ready,
    Persist(Box<PendingActionStatus>),
    WaitForExactIncarnation,
    WaitForSupportedProtocol,
}

pub(crate) fn plan_dispatch_evidence(
    pending: &PendingActionStatus,
    observed: &ReplicaStatusInfo,
    addressed_instance: &ReplicaInstanceId,
    action: &DurableReplicaAction,
    persist_action_payload: bool,
) -> DispatchEvidencePlan {
    let mut planned = pending.clone();
    let exact_incarnation = addressed_instance.as_str() == pending.target_instance_id
        && observed.instance_id.as_str() == pending.target_instance_id;
    if !exact_incarnation {
        return DispatchEvidencePlan::WaitForExactIncarnation;
    }
    let agent = &observed.agent;
    if agent.protocol_version != kuberic_core::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION {
        return DispatchEvidencePlan::WaitForSupportedProtocol;
    }
    let generation = agent.generation.to_string();
    let control_version = agent.control_version.value();
    let runtime_epoch = EpochStatus {
        data_loss_number: observed.epoch.data_loss_number,
        configuration_number: observed.epoch.configuration_number,
    };
    let evidence_matches = pending.dispatch_agent_generation.as_deref() == Some(&generation)
        && pending.dispatch_agent_control_version == Some(control_version)
        && pending.dispatch_observed_runtime_epoch.as_ref() == Some(&runtime_epoch);
    planned.dispatch_agent_generation = Some(generation);
    planned.dispatch_agent_control_version = Some(control_version);
    planned.dispatch_observed_runtime_epoch = Some(runtime_epoch);
    let local_record_exists = correlated_action_observation(observed, &pending.action_id).is_some();
    if persist_action_payload {
        if planned.dispatch_action_payload.is_empty() || (!evidence_matches && !local_record_exists)
        {
            let Ok(payload) =
                kuberic_core::grpc::convert::encode_direct_correlated_action_payload(action)
            else {
                return DispatchEvidencePlan::WaitForSupportedProtocol;
            };
            planned.dispatch_action_payload = payload;
        }
    } else {
        planned.dispatch_action_payload.clear();
    }

    if planned == *pending {
        DispatchEvidencePlan::Ready
    } else {
        DispatchEvidencePlan::Persist(Box::new(planned))
    }
}

pub(crate) async fn execute_planned_control_action(
    handle: &dyn ReplicaHandle,
    pending: &PendingActionStatus,
    authoritative_action: Option<DurableReplicaAction>,
) -> kuberic_core::Result<()> {
    let command = match authoritative_action {
        Some(action) => ReplicaEffectCommand::from_action(pending, &action)
            .map_err(|error| KubericError::Internal(error.into()))?,
        None => ReplicaEffectCommand::from_pending(pending)
            .map_err(|error| KubericError::Internal(error.into()))?,
    };
    execute_replica_command(handle, &command).await
}

pub async fn execute_replica_command(
    handle: &dyn ReplicaHandle,
    command: &ReplicaEffectCommand,
) -> kuberic_core::Result<()> {
    let generation = AgentGeneration::parse(&command.expected_agent_generation)
        .map_err(|error| KubericError::Internal(error.into()))?;
    let action = kuberic_core::grpc::convert::decode_direct_correlated_action_payload(
        &command.action_payload,
    )
    .map_err(|error| KubericError::Internal(error.into()))?;
    let input_signature = action.signature();
    handle
        .execute_correlated_control_action(CorrelatedControlActionRequest {
            protocol_version: kuberic_core::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION,
            action_id: command.action_id.clone(),
            input_signature: input_signature.clone(),
            target_replica_id: command.target_id,
            target_instance_id: ReplicaInstanceId::new(command.target_instance_id.clone()),
            expected_agent_generation: generation.clone(),
            expected_control_version: AgentControlVersion::new(command.expected_control_version),
            observed_runtime_epoch: Epoch::new(
                command.observed_runtime_epoch.data_loss_number,
                command.observed_runtime_epoch.configuration_number,
            ),
            action,
        })
        .await
        .and_then(|acknowledgement| {
            correlated_acknowledgement_result(
                acknowledgement,
                &command.action_id,
                &input_signature,
                &generation,
                AgentControlVersion::new(command.expected_control_version),
            )
        })
}

pub(crate) fn correlated_acknowledgement_result(
    acknowledgement: CorrelatedControlActionAcknowledgement,
    expected_action_id: &str,
    expected_signature: &str,
    expected_generation: &AgentGeneration,
    expected_control_version: AgentControlVersion,
) -> kuberic_core::Result<()> {
    let observation = &acknowledgement.observation;
    if observation.generation != *expected_generation
        || observation.control_version.value() == 0
        || observation.control_version.value() > expected_control_version.value().saturating_add(1)
        || observation.action.action_id != expected_action_id
        || observation.action.signature != expected_signature
    {
        return Err(KubericError::RemoteAgentRequestRejected(
            "correlated acknowledgement does not match the dispatched action".to_string(),
        ));
    }
    if acknowledgement.observation.action.state != DurableActionState::Failed {
        return Ok(());
    }
    let action = acknowledgement.observation.action;
    let class = action.error_class.ok_or_else(|| {
        KubericError::RemoteAgentRequestRejected(
            "failed correlated acknowledgement has no error class".to_string(),
        )
    })?;
    Err(KubericError::RemoteAgentTerminalFailure {
        class,
        message: action
            .error
            .unwrap_or_else(|| "correlated control action failed".to_string()),
    })
}

pub fn classify_dispatch_failure(error: &KubericError) -> DispatchFailureDisposition {
    if dispatch_rejection_requires_refresh(error)
        || dispatch_rejection_is_retryable_without_execution(error)
    {
        DispatchFailureDisposition::ProvenNoAdmission
    } else if matches!(
        error,
        KubericError::RemoteAgentConflict(_) | KubericError::RemoteAgentTerminalFailure { .. }
    ) {
        DispatchFailureDisposition::DefiniteFailure
    } else {
        DispatchFailureDisposition::Unknown
    }
}

pub(crate) fn dispatch_rejection_requires_refresh(error: &KubericError) -> bool {
    matches!(
        error,
        KubericError::RemoteAgentPreconditionRejected(_)
            | KubericError::RemoteAgentContinuityUnavailable(_)
    )
}

pub(crate) fn dispatch_rejection_is_retryable_without_execution(error: &KubericError) -> bool {
    matches!(error, KubericError::AgentBusy)
}

pub(crate) fn clear_dispatch_evidence(pending: &mut PendingActionStatus) {
    pending.dispatch_agent_generation = None;
    pending.dispatch_agent_control_version = None;
    pending.dispatch_observed_runtime_epoch = None;
    pending.dispatch_action_payload.clear();
}

pub(crate) fn operation_after_dispatch_error(
    operation: &DurableOperationStatus,
    error: &KubericError,
) -> DurableOperationStatus {
    if matches!(error, KubericError::RemoteAgentConflict(_)) {
        fail_closed(operation, &error.to_string())
    } else if dispatch_rejection_requires_refresh(error) {
        let mut next = operation.clone();
        if let Some(pending) = next.pending_action.as_mut() {
            clear_dispatch_evidence(pending);
            pending.last_error = Some(bounded(&error.to_string()));
        }
        next
    } else if dispatch_rejection_is_retryable_without_execution(error) {
        let mut next = operation.clone();
        if let Some(pending) = next.pending_action.as_mut() {
            pending.last_error = Some(bounded(&error.to_string()));
        }
        next
    } else {
        record_activity_error(operation, &error.to_string())
    }
}

#[cfg(feature = "durable-switchover-pilot")]
pub enum PilotEffectBridgeOutcome {
    Observe(Box<DurableSwitchoverStepResult>),
    Exposed,
    AwaitEvidence,
}

#[cfg(feature = "durable-switchover-pilot")]
pub async fn bridge_pilot_permitted_step(
    guard: &mut PilotPermitGuard,
    operation: &DurableOperationStatus,
    decision: PilotAdapterDecision,
    observations: &OperationObservations,
    handles: &BTreeMap<ReplicaId, Box<dyn ReplicaHandle>>,
    api: &dyn ClusterApi,
    namespace: &str,
) -> Result<PilotEffectBridgeOutcome, String> {
    let _permit = guard.consume_for(operation)?;
    let decision = match decision {
        PilotAdapterDecision::Observe(result) => {
            return Ok(PilotEffectBridgeOutcome::Observe(result));
        }
        PilotAdapterDecision::AwaitEvidence => {
            return Ok(PilotEffectBridgeOutcome::AwaitEvidence);
        }
        PilotAdapterDecision::External(decision) => *decision,
    };
    match decision {
        Decision::Execute {
            target_id,
            action_id,
            action,
        } => {
            let pending = operation.pending_action.as_ref().ok_or_else(|| {
                "pilot execution requested a replica effect without pending intent".to_string()
            })?;
            if pending.action_id != action_id {
                return Err(
                    "pilot replica effect does not match pending correlation identity".to_string(),
                );
            }
            let Some(observed) = observations.get(&target_id) else {
                return Ok(PilotEffectBridgeOutcome::AwaitEvidence);
            };
            let Some(handle) = handles.get(&target_id) else {
                return Ok(PilotEffectBridgeOutcome::AwaitEvidence);
            };
            match plan_dispatch_evidence(
                pending,
                &observed.status,
                &handle.instance_id(),
                &action,
                true,
            ) {
                DispatchEvidencePlan::Persist(planned) => {
                    let mut next = operation.clone();
                    next.pending_action = Some(*planned);
                    return Ok(PilotEffectBridgeOutcome::Observe(Box::new(
                        DurableSwitchoverStepResult::Advance {
                            operation: DurableSwitchoverState::from_operation(&next),
                        },
                    )));
                }
                DispatchEvidencePlan::WaitForExactIncarnation
                | DispatchEvidencePlan::WaitForSupportedProtocol => {
                    return Ok(PilotEffectBridgeOutcome::AwaitEvidence);
                }
                DispatchEvidencePlan::Ready => {}
            }
            match execute_planned_control_action(handle.as_ref(), pending, None).await {
                Ok(()) => Ok(PilotEffectBridgeOutcome::Exposed),
                Err(error) => match pilot_result_after_dispatch_error(operation, action_id, &error)
                {
                    Some(result) => Ok(PilotEffectBridgeOutcome::Observe(Box::new(result))),
                    None => Ok(PilotEffectBridgeOutcome::Exposed),
                },
            }
        }
        Decision::PatchPodRole { target_id, role } => {
            let command = exact_label_command(operation, target_id, &role, observations)?;
            let mut labels = BTreeMap::new();
            labels.insert("kuberic.io/role".to_string(), command.role);
            let _ = api
                .patch_pod_labels_if_uid(
                    namespace,
                    &command.pod_name,
                    &command.expected_uid,
                    labels,
                )
                .await;
            Ok(PilotEffectBridgeOutcome::Exposed)
        }
        Decision::PatchPodRoleExactUid {
            target_id,
            expected_uid,
            role,
        } => {
            let command = exact_label_command(operation, target_id, &role, observations)?;
            if command.expected_uid != expected_uid {
                return Err(
                    "pilot label command does not match shared switchover UID intent".to_string(),
                );
            }
            let mut labels = BTreeMap::new();
            labels.insert("kuberic.io/role".to_string(), command.role);
            let _ = api
                .patch_pod_labels_if_uid(
                    namespace,
                    &command.pod_name,
                    &command.expected_uid,
                    labels,
                )
                .await;
            Ok(PilotEffectBridgeOutcome::Exposed)
        }
        other => Err(format!(
            "unsupported external decision reached durable switchover pilot: {other:?}"
        )),
    }
}

#[cfg(feature = "durable-switchover-pilot")]
fn pilot_result_after_dispatch_error(
    operation: &DurableOperationStatus,
    action_id: String,
    error: &KubericError,
) -> Option<DurableSwitchoverStepResult> {
    if classify_dispatch_failure(error) == DispatchFailureDisposition::ProvenNoAdmission {
        return Some(DurableSwitchoverStepResult::ProvenNoAdmission {
            operation: DurableSwitchoverState::from_operation(&operation_after_dispatch_error(
                operation, error,
            )),
            action_id,
            redelivery: 1,
        });
    }
    matches!(error, KubericError::RemoteAgentConflict(_)).then(|| {
        DurableSwitchoverStepResult::Stopped {
            operation: DurableSwitchoverState::from_operation(&fail_closed(
                operation,
                &error.to_string(),
            )),
            message: error.to_string(),
        }
    })
}

#[cfg(feature = "durable-switchover-pilot")]
pub fn resolve_pilot_quarantine(
    operation: &DurableOperationStatus,
    decision: PilotAdapterDecision,
    observations: &OperationObservations,
) -> Result<PilotEffectBridgeOutcome, String> {
    let decision = match decision {
        PilotAdapterDecision::Observe(result) => {
            let effect_free_before_dispatch =
                operation.pending_action.as_ref().is_some_and(|pending| {
                    !matches!(
                        pending.kind,
                        crate::crd::DurableActionKind::LabelTargetPrimary
                            | crate::crd::DurableActionKind::LabelOldSecondary
                            | crate::crd::DurableActionKind::CompensateLabelOldPrimary
                            | crate::crd::DurableActionKind::CompensateLabelTargetSecondary
                    ) && pending.dispatch_agent_generation.is_none()
                        && pending.dispatch_agent_control_version.is_none()
                        && pending.dispatch_observed_runtime_epoch.is_none()
                        && pending.dispatch_action_payload.is_empty()
                });
            if effect_free_before_dispatch
                || quarantine_result_is_authoritative(operation, &result, observations)
            {
                return Ok(PilotEffectBridgeOutcome::Observe(result));
            }
            return Ok(PilotEffectBridgeOutcome::AwaitEvidence);
        }
        PilotAdapterDecision::AwaitEvidence => {
            return Ok(PilotEffectBridgeOutcome::AwaitEvidence);
        }
        PilotAdapterDecision::External(decision) => *decision,
    };
    match decision {
        Decision::Execute {
            target_id,
            action_id,
            action,
        } if operation
            .pending_action
            .as_ref()
            .is_some_and(|pending| pending.dispatch_agent_generation.is_none()) =>
        {
            let pending = operation.pending_action.as_ref().ok_or_else(|| {
                "quarantined pilot execution has no pending correlated action".to_string()
            })?;
            if pending.action_id != action_id {
                return Err(
                    "quarantined pilot action identity changed before fence persistence"
                        .to_string(),
                );
            }
            let observed = observations
                .get(&target_id)
                .ok_or_else(|| format!("quarantined pilot replica {target_id} is unavailable"))?;
            match plan_dispatch_evidence(
                pending,
                &observed.status,
                &ReplicaInstanceId::new(pending.target_instance_id.clone()),
                &action,
                true,
            ) {
                DispatchEvidencePlan::Persist(planned) => {
                    let mut next = operation.clone();
                    next.pending_action = Some(*planned);
                    Ok(PilotEffectBridgeOutcome::Observe(Box::new(
                        DurableSwitchoverStepResult::Advance {
                            operation: DurableSwitchoverState::from_operation(&next),
                        },
                    )))
                }
                DispatchEvidencePlan::Ready => Err(
                    "quarantined effect-free activity unexpectedly already had dispatch evidence"
                        .to_string(),
                ),
                DispatchEvidencePlan::WaitForExactIncarnation
                | DispatchEvidencePlan::WaitForSupportedProtocol => {
                    Ok(PilotEffectBridgeOutcome::AwaitEvidence)
                }
            }
        }
        Decision::Execute { target_id, .. }
            if generation_change_proves_no_admission(operation, target_id, observations) =>
        {
            let pending = operation.pending_action.as_ref().ok_or_else(|| {
                "quarantined pilot execution has no pending correlated action".to_string()
            })?;
            let mut next = operation.clone();
            if let Some(next_pending) = next.pending_action.as_mut() {
                clear_dispatch_evidence(next_pending);
            }
            Ok(PilotEffectBridgeOutcome::Observe(Box::new(
                DurableSwitchoverStepResult::ProvenNoAdmission {
                    operation: DurableSwitchoverState::from_operation(&next),
                    action_id: pending.action_id.clone(),
                    redelivery: 1,
                },
            )))
        }
        Decision::PatchPodRole { .. } | Decision::PatchPodRoleExactUid { .. } => {
            Ok(PilotEffectBridgeOutcome::AwaitEvidence)
        }
        _ => Ok(PilotEffectBridgeOutcome::AwaitEvidence),
    }
}

#[cfg(feature = "durable-switchover-pilot")]
fn quarantine_result_is_authoritative(
    operation: &DurableOperationStatus,
    result: &DurableSwitchoverStepResult,
    observations: &OperationObservations,
) -> bool {
    match result {
        DurableSwitchoverStepResult::Complete { .. } => operation.pending_action.is_none(),
        DurableSwitchoverStepResult::Stopped { .. } => operation.pending_action.is_none(),
        DurableSwitchoverStepResult::ProvenNoAdmission { .. } => true,
        DurableSwitchoverStepResult::Advance {
            operation: next_state,
        } => {
            let Ok(next) = next_state.apply_to(operation) else {
                return false;
            };
            if next.phase == DurableOperationPhase::Poisoned {
                return false;
            }
            let Some(pending) = operation.pending_action.as_ref() else {
                return true;
            };
            if let Some(observed) = observations.get(&pending.target_id)
                && let Some(action) =
                    correlated_action_observation(&observed.status, &pending.action_id)
            {
                return matches!(
                    action.state,
                    DurableActionState::Completed | DurableActionState::Failed
                ) && next.pending_action.is_none();
            }
            is_switchover_postcondition_transition(operation, &next, pending)
        }
    }
}

#[cfg(feature = "durable-switchover-pilot")]
pub(crate) fn generation_change_proves_no_admission(
    operation: &DurableOperationStatus,
    target_id: ReplicaId,
    observations: &OperationObservations,
) -> bool {
    let Some(pending) = operation.pending_action.as_ref() else {
        return false;
    };
    let Some(dispatched_generation) = pending.dispatch_agent_generation.as_deref() else {
        return false;
    };
    let Some(observed) = observations.get(&target_id) else {
        return false;
    };
    observed.status.agent.generation.as_str() != dispatched_generation
        && correlated_action_observation(&observed.status, &pending.action_id).is_none()
}

#[cfg(feature = "durable-switchover-pilot")]
pub(crate) fn exact_label_command(
    operation: &DurableOperationStatus,
    target_id: ReplicaId,
    role: &str,
    observations: &OperationObservations,
) -> Result<LabelEffectCommand, String> {
    let expected_uid = operation
        .previous_snapshot
        .members
        .iter()
        .find(|member| member.id == target_id)
        .or_else(|| {
            operation
                .target_snapshot
                .members
                .iter()
                .find(|member| member.id == target_id)
        })
        .map(|member| member.instance_id.clone())
        .ok_or_else(|| {
            format!("pilot label target {target_id} is not in the operation snapshot")
        })?;
    let observed = observations
        .get(&target_id)
        .ok_or_else(|| format!("pilot label target {target_id} is unavailable"))?;
    if observed.status.instance_id.as_str() != expected_uid {
        return Err(format!(
            "pilot label target {target_id} incarnation changed before patch"
        ));
    }
    Ok(LabelEffectCommand {
        target_id,
        pod_name: observed.pod_name.clone(),
        expected_uid,
        role: role.to_string(),
    })
}

fn bounded(value: &str) -> String {
    value.chars().take(MAX_EFFECT_DIAGNOSTIC_BYTES).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_outcome_kinds_are_distinct_and_bounded() {
        let applied = DurableEffectOutcome::Applied(7_u8);
        let rejected = DurableEffectOutcome::<u8>::definite_failure("rejected", &"x".repeat(700));
        let no_admission = DurableEffectOutcome::<u8>::proven_no_admission(&"n".repeat(700));
        let pending = DurableEffectOutcome::<u8>::Pending;
        let unknown = DurableEffectOutcome::<u8>::unknown(&"u".repeat(700));

        for outcome in [applied, rejected, no_admission, pending, unknown] {
            let encoded = serde_json::to_vec(&outcome).unwrap();
            assert_eq!(
                serde_json::from_slice::<DurableEffectOutcome<u8>>(&encoded).unwrap(),
                outcome
            );
            assert!(encoded.len() < 640);
        }
    }

    #[test]
    fn dispatch_errors_do_not_alias_unknown_with_non_admission() {
        assert_eq!(
            classify_dispatch_failure(&KubericError::AgentBusy),
            DispatchFailureDisposition::ProvenNoAdmission
        );
        assert_eq!(
            classify_dispatch_failure(&KubericError::RemoteAgentConflict("conflict".into())),
            DispatchFailureDisposition::DefiniteFailure
        );
        assert_eq!(
            classify_dispatch_failure(&KubericError::Closed),
            DispatchFailureDisposition::Unknown
        );
    }
}
