#![allow(dead_code)]

use async_trait::async_trait;
use kuberic_durable_execution::{ExactBytes, TerminalOutcome, Workflow, WorkflowContext};
use serde::{Deserialize, Serialize};

use crate::crd::{EpochStatus, StablePartitionSnapshotStatus};

use super::SwitchoverWorkflowInput;
use super::activities::{
    AttestCompensatedTopologyActivity, AttestCompensatedTopologyInput,
    AttestCompensatedTopologyOutput, AttestTargetTopologyActivity, AttestTargetTopologyInput,
    AttestTargetTopologyOutput, CaptureFrozenLsnActivity, CaptureFrozenLsnInput,
    CaptureFrozenLsnOutput, CompensateDistributeReplicaEpochActivity,
    CompensatePromoteOldPrimaryActivity, DIRECT_SWITCHOVER_CONTRACT_VERSION,
    DemoteOldPrimaryActivity, DistributeReplicaEpochActivity, EffectObservation,
    InstallCompensationCatchUpConfigurationActivity,
    InstallCompensationCurrentConfigurationActivity, InstallTargetCatchUpConfigurationActivity,
    InstallTargetCurrentConfigurationActivity, LabelDirectActivity, LabelOperationRequest,
    PromoteTargetActivity, PublishOldPrimarySecondaryLabelActivity,
    PublishTargetPrimaryLabelActivity, ReplicaDirectActivity, ReplicaOperationRequest,
    RestoreOldPrimaryLabelActivity, RestorePreviousCurrentConfigurationActivity,
    RestoreTargetSecondaryLabelActivity, RevokeWritesActivity, WaitTargetCaughtUpActivity,
    WaitTargetCaughtUpInput, WaitTargetCaughtUpOutput, WaitTargetWriteQuorumActivity,
};
use super::model::{DirectSwitchoverDefinition, next_deadline};

const DIRECT_WORKFLOW_TRANSITION_FUEL: usize = 64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum DirectSwitchoverTerminalRecord {
    Complete {
        snapshot: StablePartitionSnapshotStatus,
        compensated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Stopped {
        message: String,
    },
}

pub struct DirectSwitchoverWorkflow;

#[async_trait]
impl Workflow for DirectSwitchoverWorkflow {
    async fn run(&self, context: &mut WorkflowContext<'_>, input: ExactBytes) -> TerminalOutcome {
        let input: SwitchoverWorkflowInput = match serde_json::from_slice(input.as_slice()) {
            Ok(input) => input,
            Err(error) => return stopped(format!("decode direct switchover input: {error}")),
        };
        if input.version != DIRECT_SWITCHOVER_CONTRACT_VERSION {
            return stopped(format!(
                "unsupported direct switchover workflow version {}",
                input.version
            ));
        }
        if input.execution_id != super::encode_execution_id(context.execution_id()) {
            return stopped("direct switchover execution identity mismatch".to_string());
        }
        let definition = match DirectSwitchoverDefinition::from_initial(&input.initial_operation) {
            Ok(definition) => definition,
            Err(error) => return stopped(error),
        };
        let mut transition_count = 0usize;
        let mut deadline = definition.initial_deadline_unix_seconds;

        let revoke = match call_replica::<RevokeWritesActivity>(
            context,
            replica_request(
                &definition,
                1,
                definition.old_primary_id,
                definition.previous_snapshot.epoch.clone(),
                definition.previous_snapshot.clone(),
                deadline,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(error) => return stopped(error),
        };
        transition_count += 1;
        deadline = match effect_applied(revoke) {
            Ok(observed_at) => match next_deadline(observed_at) {
                Ok(deadline) => deadline,
                Err(error) => return stopped(error),
            },
            Err(EffectBranch::DomainFailure {
                observed_at,
                message,
            }) => {
                return attest_compensated(
                    context,
                    &definition,
                    definition.previous_snapshot.clone(),
                    observed_at,
                    message,
                )
                .await;
            }
            Err(EffectBranch::Stopped(message)) => return stopped(message),
        };

        let old_primary = match definition.member(definition.old_primary_id) {
            Ok(member) => member,
            Err(error) => return stopped(error),
        };
        let captured = match context
            .call::<CaptureFrozenLsnActivity>(CaptureFrozenLsnInput {
                contract_version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
                execution_id: definition.execution_id.clone(),
                old_primary_id: definition.old_primary_id,
                old_primary_instance_id: old_primary.instance_id.clone(),
                expected_epoch: definition.previous_snapshot.epoch.clone(),
                deadline_unix_seconds: deadline,
            })
            .await
        {
            Ok(result) => result,
            Err(error) => return stopped(format!("capture frozen LSN activity failed: {error}")),
        };
        transition_count += 1;
        let (frozen_lsn, observed_at) = match captured {
            CaptureFrozenLsnOutput::Captured {
                frozen_lsn,
                observed_at_unix_seconds,
            } => (frozen_lsn, observed_at_unix_seconds),
            CaptureFrozenLsnOutput::DeadlineExceeded { message, .. }
            | CaptureFrozenLsnOutput::Conflicting { message, .. } => return stopped(message),
        };
        deadline = match next_deadline(observed_at) {
            Ok(deadline) => deadline,
            Err(error) => return stopped(error),
        };

        let target = match definition.member(definition.target_primary_id) {
            Ok(member) => member,
            Err(error) => return stopped(error),
        };
        let caught_up = match context
            .call::<WaitTargetCaughtUpActivity>(WaitTargetCaughtUpInput {
                contract_version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
                execution_id: definition.execution_id.clone(),
                target_id: definition.target_primary_id,
                target_instance_id: target.instance_id.clone(),
                expected_epoch: definition.previous_snapshot.epoch.clone(),
                frozen_lsn,
                deadline_unix_seconds: deadline,
            })
            .await
        {
            Ok(result) => result,
            Err(error) => return stopped(format!("target catch-up activity failed: {error}")),
        };
        transition_count += 1;
        deadline = match caught_up {
            WaitTargetCaughtUpOutput::CaughtUp {
                observed_at_unix_seconds,
            } => match next_deadline(observed_at_unix_seconds) {
                Ok(deadline) => deadline,
                Err(error) => return stopped(error),
            },
            WaitTargetCaughtUpOutput::DeadlineExceeded {
                observed_at_unix_seconds,
                message,
            } => {
                return restore_previous_configuration(
                    context,
                    &definition,
                    observed_at_unix_seconds,
                    message,
                )
                .await;
            }
            WaitTargetCaughtUpOutput::Conflicting { message, .. } => return stopped(message),
        };

        let demote = match call_replica::<DemoteOldPrimaryActivity>(
            context,
            replica_request(
                &definition,
                2,
                definition.old_primary_id,
                definition.target_snapshot.epoch.clone(),
                definition.target_snapshot.clone(),
                deadline,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(error) => return stopped(error),
        };
        transition_count += 1;
        deadline = match effect_applied(demote) {
            Ok(observed_at) => match next_deadline(observed_at) {
                Ok(deadline) => deadline,
                Err(error) => return stopped(error),
            },
            Err(EffectBranch::DomainFailure {
                observed_at,
                message,
            }) => {
                return restore_previous_configuration(context, &definition, observed_at, message)
                    .await;
            }
            Err(EffectBranch::Stopped(message)) => return stopped(message),
        };

        let promote = match call_replica::<PromoteTargetActivity>(
            context,
            replica_request(
                &definition,
                3,
                definition.target_primary_id,
                definition.previous_snapshot.epoch.clone(),
                definition.target_snapshot.clone(),
                deadline,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(error) => return stopped(error),
        };
        transition_count += 1;
        deadline = match effect_applied(promote) {
            Ok(observed_at) => match next_deadline(observed_at) {
                Ok(deadline) => deadline,
                Err(error) => return stopped(error),
            },
            Err(EffectBranch::DomainFailure {
                observed_at,
                message,
            }) => {
                return compensate_after_promotion_failure(
                    context,
                    &definition,
                    observed_at,
                    message,
                )
                .await;
            }
            Err(EffectBranch::Stopped(message)) => return stopped(message),
        };

        for (index, replica_id) in definition
            .normal_epoch_distribution_ids()
            .into_iter()
            .enumerate()
        {
            let sequence = match u32::try_from(index) {
                Ok(index) => 100 + index,
                Err(_) => return stopped("normal epoch distribution index overflow".to_string()),
            };
            let result = match call_replica::<DistributeReplicaEpochActivity>(
                context,
                replica_request(
                    &definition,
                    sequence,
                    replica_id,
                    definition.target_snapshot.epoch.clone(),
                    definition.target_snapshot.clone(),
                    deadline,
                ),
            )
            .await
            {
                Ok(result) => result,
                Err(error) => return stopped(error),
            };
            transition_count += 1;
            deadline = match late_effect_deadline(result) {
                Ok(deadline) => deadline,
                Err(message) => return stopped(message),
            };
        }

        macro_rules! run_target_replica_step {
            ($activity:ty, $sequence:expr) => {{
                let result = match call_replica::<$activity>(
                    context,
                    replica_request(
                        &definition,
                        $sequence,
                        definition.target_primary_id,
                        definition.target_snapshot.epoch.clone(),
                        definition.target_snapshot.clone(),
                        deadline,
                    ),
                )
                .await
                {
                    Ok(result) => result,
                    Err(error) => return stopped(error),
                };
                transition_count += 1;
                deadline = match late_effect_deadline(result) {
                    Ok(deadline) => deadline,
                    Err(message) => return stopped(message),
                };
            }};
        }

        run_target_replica_step!(InstallTargetCatchUpConfigurationActivity, 1000);
        run_target_replica_step!(WaitTargetWriteQuorumActivity, 1001);
        run_target_replica_step!(InstallTargetCurrentConfigurationActivity, 1002);

        let target_label = match call_label::<PublishTargetPrimaryLabelActivity>(
            context,
            label_request(
                &definition,
                1003,
                definition.target_primary_id,
                "primary",
                deadline,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(error) => return stopped(error),
        };
        transition_count += 1;
        deadline = match late_effect_deadline(target_label) {
            Ok(deadline) => deadline,
            Err(message) => return stopped(message),
        };

        let old_label = match call_label::<PublishOldPrimarySecondaryLabelActivity>(
            context,
            label_request(
                &definition,
                1004,
                definition.old_primary_id,
                "secondary",
                deadline,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(error) => return stopped(error),
        };
        transition_count += 1;
        deadline = match late_effect_deadline(old_label) {
            Ok(deadline) => deadline,
            Err(message) => return stopped(message),
        };

        if transition_count >= DIRECT_WORKFLOW_TRANSITION_FUEL {
            return stopped("direct switchover exhausted transition fuel".to_string());
        }
        match context
            .call::<AttestTargetTopologyActivity>(AttestTargetTopologyInput {
                contract_version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
                execution_id: definition.execution_id.clone(),
                expected_snapshot: definition.target_snapshot.clone(),
                deadline_unix_seconds: deadline,
            })
            .await
        {
            Ok(AttestTargetTopologyOutput::Attested { .. }) => {
                complete(definition.target_snapshot, false, None)
            }
            Ok(AttestTargetTopologyOutput::DeadlineExceeded { message, .. })
            | Ok(AttestTargetTopologyOutput::Conflicting { message, .. }) => stopped(message),
            Err(error) => stopped(format!("target topology attestation failed: {error}")),
        }
    }
}

async fn restore_previous_configuration(
    context: &mut WorkflowContext<'_>,
    definition: &DirectSwitchoverDefinition,
    observed_at_unix_seconds: i64,
    reason: String,
) -> TerminalOutcome {
    let deadline = match next_deadline(observed_at_unix_seconds) {
        Ok(deadline) => deadline,
        Err(error) => return stopped(error),
    };
    let restore = match call_replica::<RestorePreviousCurrentConfigurationActivity>(
        context,
        replica_request(
            definition,
            1500,
            definition.old_primary_id,
            definition.previous_snapshot.epoch.clone(),
            definition.previous_snapshot.clone(),
            deadline,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return stopped(error),
    };
    let observed_at = match effect_applied(restore) {
        Ok(observed_at) => observed_at,
        Err(EffectBranch::DomainFailure { message, .. }) | Err(EffectBranch::Stopped(message)) => {
            return stopped(message);
        }
    };
    attest_compensated(
        context,
        definition,
        definition.previous_snapshot.clone(),
        observed_at,
        reason,
    )
    .await
}

async fn compensate_after_promotion_failure(
    context: &mut WorkflowContext<'_>,
    definition: &DirectSwitchoverDefinition,
    observed_at_unix_seconds: i64,
    reason: String,
) -> TerminalOutcome {
    let snapshot = definition.compensation_snapshot();
    let mut deadline = match next_deadline(observed_at_unix_seconds) {
        Ok(deadline) => deadline,
        Err(error) => return stopped(error),
    };

    let promote_old = match call_replica::<CompensatePromoteOldPrimaryActivity>(
        context,
        replica_request(
            definition,
            2000,
            definition.old_primary_id,
            definition.target_snapshot.epoch.clone(),
            snapshot.clone(),
            deadline,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return stopped(error),
    };
    deadline = match late_effect_deadline(promote_old) {
        Ok(deadline) => deadline,
        Err(message) => return stopped(message),
    };

    for (index, replica_id) in definition
        .compensation_epoch_distribution_ids()
        .into_iter()
        .enumerate()
    {
        let sequence = match u32::try_from(index) {
            Ok(index) => 2100 + index,
            Err(_) => return stopped("compensation epoch distribution index overflow".to_string()),
        };
        let result = match call_replica::<CompensateDistributeReplicaEpochActivity>(
            context,
            replica_request(
                definition,
                sequence,
                replica_id,
                definition.target_snapshot.epoch.clone(),
                snapshot.clone(),
                deadline,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(error) => return stopped(error),
        };
        deadline = match late_effect_deadline(result) {
            Ok(deadline) => deadline,
            Err(message) => return stopped(message),
        };
    }

    for (sequence, step) in [
        (2001, CompensationStep::CatchUpConfiguration),
        (2002, CompensationStep::CurrentConfiguration),
    ] {
        let result = match step {
            CompensationStep::CatchUpConfiguration => {
                call_replica::<InstallCompensationCatchUpConfigurationActivity>(
                    context,
                    replica_request(
                        definition,
                        sequence,
                        definition.old_primary_id,
                        definition.target_snapshot.epoch.clone(),
                        snapshot.clone(),
                        deadline,
                    ),
                )
                .await
            }
            CompensationStep::CurrentConfiguration => {
                call_replica::<InstallCompensationCurrentConfigurationActivity>(
                    context,
                    replica_request(
                        definition,
                        sequence,
                        definition.old_primary_id,
                        definition.target_snapshot.epoch.clone(),
                        snapshot.clone(),
                        deadline,
                    ),
                )
                .await
            }
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => return stopped(error),
        };
        deadline = match late_effect_deadline(result) {
            Ok(deadline) => deadline,
            Err(message) => return stopped(message),
        };
    }

    let restore_old_label = match call_label::<RestoreOldPrimaryLabelActivity>(
        context,
        label_request(
            definition,
            2003,
            definition.old_primary_id,
            "primary",
            deadline,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return stopped(error),
    };
    deadline = match late_effect_deadline(restore_old_label) {
        Ok(deadline) => deadline,
        Err(message) => return stopped(message),
    };

    let restore_target_label = match call_label::<RestoreTargetSecondaryLabelActivity>(
        context,
        label_request(
            definition,
            2004,
            definition.target_primary_id,
            "secondary",
            deadline,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return stopped(error),
    };
    let observed_at = match effect_applied(restore_target_label) {
        Ok(observed_at) => observed_at,
        Err(EffectBranch::DomainFailure { message, .. }) | Err(EffectBranch::Stopped(message)) => {
            return stopped(message);
        }
    };

    attest_compensated(context, definition, snapshot, observed_at, reason).await
}

async fn attest_compensated(
    context: &mut WorkflowContext<'_>,
    definition: &DirectSwitchoverDefinition,
    snapshot: StablePartitionSnapshotStatus,
    observed_at_unix_seconds: i64,
    reason: String,
) -> TerminalOutcome {
    let deadline = match next_deadline(observed_at_unix_seconds) {
        Ok(deadline) => deadline,
        Err(error) => return stopped(error),
    };
    match context
        .call::<AttestCompensatedTopologyActivity>(AttestCompensatedTopologyInput {
            contract_version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
            execution_id: definition.execution_id.clone(),
            expected_snapshot: snapshot.clone(),
            deadline_unix_seconds: deadline,
        })
        .await
    {
        Ok(AttestCompensatedTopologyOutput::Attested { .. }) => {
            complete(snapshot, true, Some(reason))
        }
        Ok(AttestCompensatedTopologyOutput::DeadlineExceeded { message, .. })
        | Ok(AttestCompensatedTopologyOutput::Conflicting { message, .. }) => stopped(message),
        Err(error) => stopped(format!("compensated topology attestation failed: {error}")),
    }
}

async fn call_replica<A: ReplicaDirectActivity>(
    context: &mut WorkflowContext<'_>,
    mut request: ReplicaOperationRequest,
) -> Result<EffectObservation, String> {
    let first = context
        .call::<A>(A::input(request.clone()))
        .await
        .map(A::observation)
        .map_err(|error| format!("{} activity failed: {error}", A::NAME))?;
    if !matches!(first, EffectObservation::ProvenNoAdmission { .. }) {
        return Ok(first);
    }
    request.redelivery = 1;
    let second = context
        .call::<A>(A::input(request))
        .await
        .map(A::observation)
        .map_err(|error| format!("{} redelivery failed: {error}", A::NAME))?;
    if matches!(second, EffectObservation::ProvenNoAdmission { .. }) {
        return Err(format!(
            "{} exceeded one proven-no-admission redelivery",
            A::NAME
        ));
    }
    Ok(second)
}

async fn call_label<A: LabelDirectActivity>(
    context: &mut WorkflowContext<'_>,
    request: LabelOperationRequest,
) -> Result<EffectObservation, String> {
    context
        .call::<A>(A::input(request))
        .await
        .map(A::observation)
        .map_err(|error| format!("{} activity failed: {error}", A::NAME))
}

fn replica_request(
    definition: &DirectSwitchoverDefinition,
    sequence: u32,
    target_id: i64,
    expected_epoch: EpochStatus,
    desired_snapshot: StablePartitionSnapshotStatus,
    deadline_unix_seconds: i64,
) -> ReplicaOperationRequest {
    let target_instance_id = definition
        .member(target_id)
        .map(|member| member.instance_id.clone())
        .unwrap_or_default();
    ReplicaOperationRequest {
        contract_version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
        execution_id: definition.execution_id.clone(),
        action_id: format!("{}:{sequence}", definition.execution_id),
        sequence,
        target_id,
        target_instance_id,
        expected_epoch,
        desired_snapshot,
        deadline_unix_seconds,
        redelivery: 0,
        prepared_command: None,
    }
}

fn label_request(
    definition: &DirectSwitchoverDefinition,
    sequence: u32,
    target_id: i64,
    desired_role: &str,
    deadline_unix_seconds: i64,
) -> LabelOperationRequest {
    let target_instance_id = definition
        .member(target_id)
        .map(|member| member.instance_id.clone())
        .unwrap_or_default();
    LabelOperationRequest {
        contract_version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
        execution_id: definition.execution_id.clone(),
        action_id: format!("{}:{sequence}", definition.execution_id),
        sequence,
        target_id,
        target_instance_id,
        desired_role: desired_role.to_string(),
        deadline_unix_seconds,
        prepared_command: None,
    }
}

enum EffectBranch {
    DomainFailure { observed_at: i64, message: String },
    Stopped(String),
}

fn effect_applied(observation: EffectObservation) -> Result<i64, EffectBranch> {
    match observation {
        EffectObservation::Applied {
            observed_at_unix_seconds,
        } => Ok(observed_at_unix_seconds),
        EffectObservation::Rejected {
            observed_at_unix_seconds,
            message,
        }
        | EffectObservation::Failed {
            observed_at_unix_seconds,
            message,
        } => Err(EffectBranch::DomainFailure {
            observed_at: observed_at_unix_seconds,
            message,
        }),
        EffectObservation::Conflicting { message, .. } => Err(EffectBranch::Stopped(message)),
        EffectObservation::ProvenNoAdmission { .. } => Err(EffectBranch::Stopped(
            "unresolved proven-no-admission result".to_string(),
        )),
    }
}

fn late_effect_deadline(observation: EffectObservation) -> Result<i64, String> {
    let observed_at = observation.observed_at_unix_seconds();
    match observation {
        EffectObservation::Applied { .. } => next_deadline(observed_at),
        EffectObservation::Rejected { message, .. }
        | EffectObservation::Failed { message, .. }
        | EffectObservation::Conflicting { message, .. } => Err(message),
        EffectObservation::ProvenNoAdmission { .. } => {
            Err("unresolved proven-no-admission result".to_string())
        }
    }
}

enum CompensationStep {
    CatchUpConfiguration,
    CurrentConfiguration,
}

fn complete(
    snapshot: StablePartitionSnapshotStatus,
    compensated: bool,
    reason: Option<String>,
) -> TerminalOutcome {
    terminal(DirectSwitchoverTerminalRecord::Complete {
        snapshot,
        compensated,
        reason: reason
            .map(|reason| super::bounded_utf8(&reason, super::SWITCHOVER_MAX_ERROR_BYTES)),
    })
}

fn stopped(message: String) -> TerminalOutcome {
    let message = super::bounded_utf8(&message, super::SWITCHOVER_MAX_ERROR_BYTES);
    let payload = serde_json::to_vec(&DirectSwitchoverTerminalRecord::Stopped { message })
        .unwrap_or_else(|_| br#"{"status":"stopped","message":"encode failure"}"#.to_vec());
    TerminalOutcome::failed(ExactBytes::new(payload))
}

fn terminal(record: DirectSwitchoverTerminalRecord) -> TerminalOutcome {
    match serde_json::to_vec(&record) {
        Ok(payload) => TerminalOutcome::succeeded(ExactBytes::new(payload)),
        Err(error) => stopped(format!("encode direct switchover terminal: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuberic_durable_execution::{
        ActivityObservation, CheckpointLimits, DurableActivity, DurableHost, ExecutionId,
        ExecutionSpec, HostEpoch, HostOutcome, InMemoryCheckpointStore,
    };

    use crate::crd::{EpochStatus, StableReplicaRoleStatus, StableReplicaSnapshotStatus};
    use crate::durable::start_switchover;

    fn snapshot(count: i64) -> StablePartitionSnapshotStatus {
        StablePartitionSnapshotStatus {
            epoch: EpochStatus {
                data_loss_number: 1,
                configuration_number: 7,
            },
            primary_id: 1,
            members: (1..=count)
                .rev()
                .map(|id| StableReplicaSnapshotStatus {
                    id,
                    instance_id: format!("instance-{id}"),
                    role: if id == 1 {
                        StableReplicaRoleStatus::Primary
                    } else {
                        StableReplicaRoleStatus::ActiveSecondary
                    },
                    election_metadata: None,
                })
                .collect(),
            write_quorum: u32::try_from(count / 2 + 1).unwrap(),
        }
    }

    async fn run_script(
        replica_count: i64,
        failure: Option<(&str, &str)>,
        proven_no_admission: Option<&str>,
    ) -> (
        Vec<String>,
        Vec<Option<i64>>,
        DirectSwitchoverTerminalRecord,
    ) {
        let execution_id = ExecutionId::from_bytes([41; 16]);
        let operation = start_switchover("set-uid", snapshot(replica_count), 2, 100).unwrap();
        let input = SwitchoverWorkflowInput {
            version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
            execution_id: super::super::encode_execution_id(execution_id),
            initial_operation: operation,
        };
        let execution = ExecutionSpec::new(
            execution_id,
            ExactBytes::new(serde_json::to_vec(&input).unwrap()),
            4_096,
        );
        let limits = CheckpointLimits::new(64, 1024 * 1024, 16 * 1024).unwrap();
        let mut host = DurableHost::new(
            InMemoryCheckpointStore::new(),
            HostEpoch::from_bytes([42; 16]),
            limits,
        );
        let workflow = DirectSwitchoverWorkflow;
        let mut names = Vec::new();
        let mut targets = Vec::new();
        let mut observed_at = 101i64;
        let mut occurrences = std::collections::BTreeMap::<String, usize>::new();

        loop {
            match host.turn(&workflow, execution.clone()).await {
                HostOutcome::ScheduleAccepted { .. } => {}
                HostOutcome::DispatchPermitted { permit, .. } => {
                    let name = permit.activity().name().name().to_string();
                    names.push(name.clone());
                    let target_id = serde_json::from_slice::<serde_json::Value>(
                        permit.activity().input().as_slice(),
                    )
                    .ok()
                    .and_then(|value| value.get("targetId").and_then(serde_json::Value::as_i64));
                    targets.push(target_id);
                    let occurrence = occurrences.entry(name.clone()).or_default();
                    let result = scripted_result(
                        &name,
                        observed_at,
                        failure.filter(|(failed_name, _)| *failed_name == name),
                        proven_no_admission == Some(name.as_str()) && *occurrence == 0,
                    );
                    *occurrence += 1;
                    observed_at += 1;
                    let outcome = host
                        .observe(
                            &execution,
                            ActivityObservation::new(permit.activity().clone(), result),
                        )
                        .await;
                    assert!(matches!(outcome, HostOutcome::ObservationAccepted { .. }));
                }
                HostOutcome::WorkflowCompleted { outcome, .. } => {
                    let record = serde_json::from_slice::<DirectSwitchoverTerminalRecord>(
                        outcome.payload().as_slice(),
                    )
                    .unwrap();
                    return (names, targets, record);
                }
                other => panic!("unexpected direct workflow host outcome: {other:?}"),
            }
        }
    }

    fn scripted_result(
        name: &str,
        observed_at: i64,
        failure: Option<(&str, &str)>,
        proven_no_admission: bool,
    ) -> ExactBytes {
        let value = if proven_no_admission {
            serde_json::json!({
                "result": "proven_no_admission",
                "observed_at_unix_seconds": observed_at,
            })
        } else if let Some((_, message)) = failure {
            if name == WaitTargetCaughtUpActivity::NAME {
                serde_json::json!({
                    "result": "deadline_exceeded",
                    "observed_at_unix_seconds": observed_at,
                    "message": message,
                })
            } else {
                serde_json::json!({
                    "result": "failed",
                    "observed_at_unix_seconds": observed_at,
                    "message": message,
                })
            }
        } else if name == CaptureFrozenLsnActivity::NAME {
            serde_json::json!({
                "result": "captured",
                "frozen_lsn": 55,
                "observed_at_unix_seconds": observed_at,
            })
        } else if name == WaitTargetCaughtUpActivity::NAME {
            serde_json::json!({
                "result": "caught_up",
                "observed_at_unix_seconds": observed_at,
            })
        } else if name == AttestTargetTopologyActivity::NAME
            || name == AttestCompensatedTopologyActivity::NAME
        {
            serde_json::json!({
                "result": "attested",
                "observed_at_unix_seconds": observed_at,
            })
        } else {
            serde_json::json!({
                "result": "applied",
                "observed_at_unix_seconds": observed_at,
            })
        };
        ExactBytes::new(serde_json::to_vec(&value).unwrap())
    }

    #[tokio::test]
    async fn direct_switchover_workflow_spells_out_successful_protocol() {
        for replica_count in [2, 4, 9] {
            let (names, _targets, terminal) = run_script(replica_count, None, None).await;
            let mut expected = vec![
                RevokeWritesActivity::NAME,
                CaptureFrozenLsnActivity::NAME,
                WaitTargetCaughtUpActivity::NAME,
                DemoteOldPrimaryActivity::NAME,
                PromoteTargetActivity::NAME,
            ];
            expected.extend(std::iter::repeat_n(
                DistributeReplicaEpochActivity::NAME,
                usize::try_from(replica_count - 2).unwrap(),
            ));
            expected.extend([
                InstallTargetCatchUpConfigurationActivity::NAME,
                WaitTargetWriteQuorumActivity::NAME,
                InstallTargetCurrentConfigurationActivity::NAME,
                PublishTargetPrimaryLabelActivity::NAME,
                PublishOldPrimarySecondaryLabelActivity::NAME,
                AttestTargetTopologyActivity::NAME,
            ]);
            assert_eq!(names, expected);
            assert!(matches!(
                terminal,
                DirectSwitchoverTerminalRecord::Complete {
                    compensated: false,
                    ..
                }
            ));
        }
    }

    #[tokio::test]
    async fn direct_switchover_workflow_spells_out_post_promotion_compensation() {
        let (names, _targets, terminal) = run_script(
            3,
            Some((PromoteTargetActivity::NAME, "promotion failed")),
            None,
        )
        .await;
        assert_eq!(
            names,
            vec![
                RevokeWritesActivity::NAME,
                CaptureFrozenLsnActivity::NAME,
                WaitTargetCaughtUpActivity::NAME,
                DemoteOldPrimaryActivity::NAME,
                PromoteTargetActivity::NAME,
                CompensatePromoteOldPrimaryActivity::NAME,
                CompensateDistributeReplicaEpochActivity::NAME,
                CompensateDistributeReplicaEpochActivity::NAME,
                InstallCompensationCatchUpConfigurationActivity::NAME,
                InstallCompensationCurrentConfigurationActivity::NAME,
                RestoreOldPrimaryLabelActivity::NAME,
                RestoreTargetSecondaryLabelActivity::NAME,
                AttestCompensatedTopologyActivity::NAME,
            ]
        );
        assert!(matches!(
            terminal,
            DirectSwitchoverTerminalRecord::Complete {
                compensated: true,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn direct_switchover_workflow_spells_out_pre_promotion_compensation() {
        let (names, _targets, terminal) = run_script(
            3,
            Some((
                WaitTargetCaughtUpActivity::NAME,
                "target catch-up timed out",
            )),
            None,
        )
        .await;
        assert_eq!(
            names,
            vec![
                RevokeWritesActivity::NAME,
                CaptureFrozenLsnActivity::NAME,
                WaitTargetCaughtUpActivity::NAME,
                RestorePreviousCurrentConfigurationActivity::NAME,
                AttestCompensatedTopologyActivity::NAME,
            ]
        );
        assert!(matches!(
            terminal,
            DirectSwitchoverTerminalRecord::Complete {
                compensated: true,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn direct_switchover_records_one_same_operation_redelivery() {
        let (names, _targets, terminal) =
            run_script(3, None, Some(DemoteOldPrimaryActivity::NAME)).await;
        assert_eq!(
            names
                .iter()
                .filter(|name| name.as_str() == DemoteOldPrimaryActivity::NAME)
                .count(),
            2
        );
        assert!(matches!(
            terminal,
            DirectSwitchoverTerminalRecord::Complete {
                compensated: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn direct_switchover_late_failure_stops_without_compensation() {
        let (names, _targets, terminal) = run_script(
            3,
            Some((
                InstallTargetCurrentConfigurationActivity::NAME,
                "current configuration failed",
            )),
            None,
        )
        .await;
        assert!(names.contains(&InstallTargetCurrentConfigurationActivity::NAME.to_string()));
        assert!(!names.contains(&RestorePreviousCurrentConfigurationActivity::NAME.to_string()));
        assert!(!names.contains(&CompensatePromoteOldPrimaryActivity::NAME.to_string()));
        assert!(matches!(
            terminal,
            DirectSwitchoverTerminalRecord::Stopped { .. }
        ));
    }

    #[tokio::test]
    async fn direct_switchover_replay_is_identical_one_hundred_times() {
        let baseline = run_script(2, None, None).await;
        for _ in 0..100 {
            assert_eq!(run_script(2, None, None).await, baseline);
        }
    }

    #[tokio::test]
    async fn direct_switchover_matches_reducer_oracle_actions_targets_and_terminals() {
        use crate::crd::{DurableActionKind, DurableOperationPhase};
        use crate::durable::switchover::{
            TestSwitchoverOracleScenario, test_switchover_oracle_trace,
        };

        for replica_count in [2, 4, 9] {
            for scenario in [
                TestSwitchoverOracleScenario::Success,
                TestSwitchoverOracleScenario::PrePromotionCompensation,
                TestSwitchoverOracleScenario::PostPromotionCompensation,
                TestSwitchoverOracleScenario::LateFailure,
            ] {
                let failure = match scenario {
                    TestSwitchoverOracleScenario::Success => None,
                    TestSwitchoverOracleScenario::PrePromotionCompensation => Some((
                        WaitTargetCaughtUpActivity::NAME,
                        "target catch-up timed out",
                    )),
                    TestSwitchoverOracleScenario::PostPromotionCompensation => {
                        Some((PromoteTargetActivity::NAME, "promotion failed"))
                    }
                    TestSwitchoverOracleScenario::LateFailure => Some((
                        InstallTargetCurrentConfigurationActivity::NAME,
                        "current configuration failed",
                    )),
                };
                let initial = start_switchover("set-uid", snapshot(replica_count), 2, 100).unwrap();
                let oracle = test_switchover_oracle_trace(&initial, scenario).unwrap();
                let (names, targets, terminal) = run_script(replica_count, failure, None).await;
                let direct_actions = names
                    .iter()
                    .zip(targets)
                    .filter_map(|(name, target)| {
                        direct_action_kind(name).map(|kind| {
                            (
                                kind,
                                target.expect("effect activity input must carry targetId"),
                            )
                        })
                    })
                    .collect::<Vec<(DurableActionKind, i64)>>();
                assert_eq!(direct_actions, oracle.actions, "{scenario:?}");
                let direct_terminal = match terminal {
                    DirectSwitchoverTerminalRecord::Complete {
                        compensated: false, ..
                    } => DurableOperationPhase::Completed,
                    DirectSwitchoverTerminalRecord::Complete {
                        compensated: true, ..
                    } => DurableOperationPhase::Failed,
                    DirectSwitchoverTerminalRecord::Stopped { .. } => {
                        DurableOperationPhase::Poisoned
                    }
                };
                assert_eq!(direct_terminal, oracle.terminal_phase, "{scenario:?}");
            }
        }
    }

    fn direct_action_kind(name: &str) -> Option<crate::crd::DurableActionKind> {
        use crate::crd::DurableActionKind as Action;

        Some(match name {
            RevokeWritesActivity::NAME => Action::RevokeWrite,
            DemoteOldPrimaryActivity::NAME => Action::DemoteOldPrimary,
            PromoteTargetActivity::NAME => Action::PromoteTarget,
            DistributeReplicaEpochActivity::NAME => Action::UpdateSecondaryEpoch,
            InstallTargetCatchUpConfigurationActivity::NAME => Action::UpdateCatchUpConfiguration,
            WaitTargetWriteQuorumActivity::NAME => Action::WaitForCatchUpQuorum,
            InstallTargetCurrentConfigurationActivity::NAME => Action::UpdateCurrentConfiguration,
            PublishTargetPrimaryLabelActivity::NAME => Action::LabelTargetPrimary,
            PublishOldPrimarySecondaryLabelActivity::NAME => Action::LabelOldSecondary,
            RestorePreviousCurrentConfigurationActivity::NAME => {
                Action::RestorePreviousConfiguration
            }
            CompensatePromoteOldPrimaryActivity::NAME => Action::CompensatePromoteOldPrimary,
            CompensateDistributeReplicaEpochActivity::NAME => {
                Action::CompensateUpdateSecondaryEpoch
            }
            InstallCompensationCatchUpConfigurationActivity::NAME => {
                Action::CompensateCatchUpConfiguration
            }
            InstallCompensationCurrentConfigurationActivity::NAME => {
                Action::CompensateCurrentConfiguration
            }
            RestoreOldPrimaryLabelActivity::NAME => Action::CompensateLabelOldPrimary,
            RestoreTargetSecondaryLabelActivity::NAME => Action::CompensateLabelTargetSecondary,
            CaptureFrozenLsnActivity::NAME
            | WaitTargetCaughtUpActivity::NAME
            | AttestTargetTopologyActivity::NAME
            | AttestCompensatedTopologyActivity::NAME => return None,
            _ => return None,
        })
    }

    #[test]
    fn direct_switchover_terminal_messages_are_utf8_bounded() {
        let TerminalOutcome::Failed(payload) = stopped("é".repeat(1_000)) else {
            panic!("stopped terminal must fail");
        };
        let terminal =
            serde_json::from_slice::<DirectSwitchoverTerminalRecord>(payload.as_slice()).unwrap();
        let DirectSwitchoverTerminalRecord::Stopped { message } = terminal else {
            panic!("expected stopped terminal");
        };
        assert!(message.len() <= super::super::SWITCHOVER_MAX_ERROR_BYTES);
        assert!(message.is_char_boundary(message.len()));
    }
}
