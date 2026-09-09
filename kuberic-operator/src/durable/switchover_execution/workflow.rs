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
    DemoteOldPrimaryActivity, DirectActivityAccounting, DistributeReplicaEpochActivity,
    EffectObservation, InstallCompensationCatchUpConfigurationActivity,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accounting: Option<DirectActivityAccounting>,
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
            Ok(AttestTargetTopologyOutput::Attested {
                snapshot,
                accounting,
                ..
            }) => complete(snapshot, false, None, accounting),
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
        Ok(AttestCompensatedTopologyOutput::Attested {
            snapshot,
            accounting,
            ..
        }) => complete(snapshot, true, Some(reason), accounting),
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
        }
        | EffectObservation::DeadlineExceeded {
            observed_at_unix_seconds,
            message,
        } => Err(EffectBranch::DomainFailure {
            observed_at: observed_at_unix_seconds,
            message,
        }),
        EffectObservation::UnavailableAtDeadline { message, .. }
        | EffectObservation::Conflicting { message, .. } => Err(EffectBranch::Stopped(message)),
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
        | EffectObservation::DeadlineExceeded { message, .. }
        | EffectObservation::UnavailableAtDeadline { message, .. }
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
    accounting: Option<DirectActivityAccounting>,
) -> TerminalOutcome {
    terminal(DirectSwitchoverTerminalRecord::Complete {
        snapshot,
        compensated,
        reason: reason
            .map(|reason| super::bounded_utf8(&reason, super::SWITCHOVER_MAX_ERROR_BYTES)),
        accounting,
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
    use crate::durable::switchover_execution::direct_initial_operation;

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

    #[derive(Clone, Copy)]
    enum ScriptedFailureKind {
        Failed,
        DeadlineExceeded,
        UnavailableAtDeadline,
    }

    #[derive(Clone, Copy)]
    struct ScriptedFailure {
        name: &'static str,
        kind: ScriptedFailureKind,
        message: &'static str,
    }

    async fn run_script(
        replica_count: i64,
        failures: &[ScriptedFailure],
        proven_no_admission: Option<&str>,
    ) -> (
        Vec<String>,
        Vec<Option<i64>>,
        DirectSwitchoverTerminalRecord,
    ) {
        let execution_id = ExecutionId::from_bytes([41; 16]);
        let operation =
            direct_initial_operation("set-uid", snapshot(replica_count), 2, 100).unwrap();
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
                    let activity_input = serde_json::from_slice::<serde_json::Value>(
                        permit.activity().input().as_slice(),
                    )
                    .unwrap();
                    let target_id = activity_input
                        .get("targetId")
                        .and_then(serde_json::Value::as_i64);
                    targets.push(target_id);
                    let occurrence = occurrences.entry(name.clone()).or_default();
                    let failure = failures
                        .iter()
                        .copied()
                        .find(|failure| failure.name == name);
                    let result = scripted_result(
                        &name,
                        observed_at,
                        failure,
                        proven_no_admission == Some(name.as_str()) && *occurrence == 0,
                        &activity_input,
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
        failure: Option<ScriptedFailure>,
        proven_no_admission: bool,
        activity_input: &serde_json::Value,
    ) -> ExactBytes {
        let value = if proven_no_admission {
            serde_json::json!({
                "result": "proven_no_admission",
                "observed_at_unix_seconds": observed_at,
            })
        } else if let Some(failure) = failure {
            let result = match failure.kind {
                ScriptedFailureKind::Failed => "failed",
                ScriptedFailureKind::DeadlineExceeded => "deadline_exceeded",
                ScriptedFailureKind::UnavailableAtDeadline => "unavailable_at_deadline",
            };
            serde_json::json!({
                "result": result,
                "observed_at_unix_seconds": observed_at,
                "message": failure.message,
            })
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
                "snapshot": activity_input.get("expectedSnapshot").unwrap(),
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
            let (names, _targets, terminal) = run_script(replica_count, &[], None).await;
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
            &[ScriptedFailure {
                name: PromoteTargetActivity::NAME,
                kind: ScriptedFailureKind::Failed,
                message: "promotion failed",
            }],
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
            &[ScriptedFailure {
                name: WaitTargetCaughtUpActivity::NAME,
                kind: ScriptedFailureKind::DeadlineExceeded,
                message: "target catch-up timed out",
            }],
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
            run_script(3, &[], Some(DemoteOldPrimaryActivity::NAME)).await;
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
            &[ScriptedFailure {
                name: InstallTargetCurrentConfigurationActivity::NAME,
                kind: ScriptedFailureKind::Failed,
                message: "current configuration failed",
            }],
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
    async fn direct_switchover_expired_actions_preserve_prior_compensation_boundaries() {
        for activity_name in [
            RevokeWritesActivity::NAME,
            DemoteOldPrimaryActivity::NAME,
            PromoteTargetActivity::NAME,
        ] {
            let (names, _targets, terminal) = run_script(
                3,
                &[ScriptedFailure {
                    name: activity_name,
                    kind: ScriptedFailureKind::DeadlineExceeded,
                    message: "action reached its exact deadline",
                }],
                None,
            )
            .await;
            assert!(matches!(
                terminal,
                DirectSwitchoverTerminalRecord::Complete {
                    compensated: true,
                    ..
                }
            ));
            assert!(
                names.contains(&AttestCompensatedTopologyActivity::NAME.to_string()),
                "{activity_name}"
            );
            match activity_name {
                RevokeWritesActivity::NAME => {
                    assert!(
                        !names.contains(
                            &RestorePreviousCurrentConfigurationActivity::NAME.to_string()
                        )
                    );
                    assert!(
                        !names.contains(&CompensatePromoteOldPrimaryActivity::NAME.to_string())
                    );
                }
                DemoteOldPrimaryActivity::NAME => {
                    assert!(
                        names.contains(
                            &RestorePreviousCurrentConfigurationActivity::NAME.to_string()
                        )
                    );
                    assert!(
                        !names.contains(&CompensatePromoteOldPrimaryActivity::NAME.to_string())
                    );
                }
                PromoteTargetActivity::NAME => {
                    assert!(names.contains(&CompensatePromoteOldPrimaryActivity::NAME.to_string()));
                }
                _ => unreachable!(),
            }
        }
    }

    #[tokio::test]
    async fn direct_switchover_unavailable_at_action_deadline_stops_without_compensation() {
        for activity_name in [
            RevokeWritesActivity::NAME,
            DemoteOldPrimaryActivity::NAME,
            PromoteTargetActivity::NAME,
        ] {
            let (names, _targets, terminal) = run_script(
                3,
                &[ScriptedFailure {
                    name: activity_name,
                    kind: ScriptedFailureKind::UnavailableAtDeadline,
                    message: "action target unavailable at its exact deadline",
                }],
                None,
            )
            .await;
            assert!(matches!(
                terminal,
                DirectSwitchoverTerminalRecord::Stopped { .. }
            ));
            assert!(
                !names.contains(&RestorePreviousCurrentConfigurationActivity::NAME.to_string())
            );
            assert!(!names.contains(&CompensatePromoteOldPrimaryActivity::NAME.to_string()));
            assert!(!names.contains(&AttestCompensatedTopologyActivity::NAME.to_string()));
        }
    }

    #[tokio::test]
    async fn direct_switchover_late_and_compensation_deadlines_stop() {
        for activity_name in [
            DistributeReplicaEpochActivity::NAME,
            InstallTargetCatchUpConfigurationActivity::NAME,
            PublishTargetPrimaryLabelActivity::NAME,
        ] {
            let (names, _targets, terminal) = run_script(
                3,
                &[ScriptedFailure {
                    name: activity_name,
                    kind: ScriptedFailureKind::DeadlineExceeded,
                    message: "late action reached its exact deadline",
                }],
                None,
            )
            .await;
            assert!(matches!(
                terminal,
                DirectSwitchoverTerminalRecord::Stopped { .. }
            ));
            assert!(
                !names.contains(&RestorePreviousCurrentConfigurationActivity::NAME.to_string())
            );
            assert!(!names.contains(&CompensatePromoteOldPrimaryActivity::NAME.to_string()));
        }

        let (names, _targets, terminal) = run_script(
            3,
            &[
                ScriptedFailure {
                    name: PromoteTargetActivity::NAME,
                    kind: ScriptedFailureKind::Failed,
                    message: "promotion failed",
                },
                ScriptedFailure {
                    name: CompensatePromoteOldPrimaryActivity::NAME,
                    kind: ScriptedFailureKind::DeadlineExceeded,
                    message: "compensation action reached its exact deadline",
                },
            ],
            None,
        )
        .await;
        assert!(matches!(
            terminal,
            DirectSwitchoverTerminalRecord::Stopped { .. }
        ));
        assert!(names.contains(&CompensatePromoteOldPrimaryActivity::NAME.to_string()));
        assert!(!names.contains(&CompensateDistributeReplicaEpochActivity::NAME.to_string()));
        assert!(!names.contains(&AttestCompensatedTopologyActivity::NAME.to_string()));
    }

    #[tokio::test]
    async fn direct_switchover_replay_is_identical_one_hundred_times() {
        let baseline = run_script(2, &[], None).await;
        for _ in 0..100 {
            assert_eq!(run_script(2, &[], None).await, baseline);
        }
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
