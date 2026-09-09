use std::collections::BTreeMap;

use kuberic_core::types::{
    AccessStatus, DurableActionState, DurableReplicaAction, Epoch,
    ReplicaConfigurationMemberStatus, ReplicaConfigurationMode, ReplicaConfigurationStatus,
    ReplicaInfo, ReplicaInstanceId, ReplicaSetConfig, ReplicaSetQuorumMode, ReplicaStatus, Role,
};
use kuberic_durable_execution::{
    ActivityName, ActivitySpec, DurableActivity, ExactBytes, PreparedActivityError,
    decode_activity_input, encode_activity_input, encode_activity_result,
};

use crate::crd::{
    DurableActionKind, DurablePostconditionKind, DurablePostconditionStatus, EpochStatus,
    PendingActionStatus, StablePartitionSnapshotStatus, StableReplicaRoleStatus,
};
use crate::durable::{
    OperationObservations, correlated_action_observation,
    effects::{
        DurableEffectPreparationError, LabelEffectCommand, ReplicaEffectCommand,
        prepare_replica_effect_command,
    },
};

use super::{
    activities::{
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
    },
    model::DirectSwitchoverDefinition,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectReplicaOperation {
    RevokeWrites,
    DemoteOldPrimary,
    PromoteTarget,
    DistributeReplicaEpoch,
    InstallTargetCatchUpConfiguration,
    WaitTargetWriteQuorum,
    InstallTargetCurrentConfiguration,
    RestorePreviousCurrentConfiguration,
    CompensatePromoteOldPrimary,
    CompensateDistributeReplicaEpoch,
    InstallCompensationCatchUpConfiguration,
    InstallCompensationCurrentConfiguration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectLabelOperation {
    PublishTargetPrimary,
    PublishOldPrimarySecondary,
    RestoreOldPrimary,
    RestoreTargetSecondary,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DirectActivity {
    Replica {
        operation: DirectReplicaOperation,
        request: ReplicaOperationRequest,
    },
    Label {
        operation: DirectLabelOperation,
        request: LabelOperationRequest,
    },
    CaptureFrozenLsn(CaptureFrozenLsnInput),
    WaitTargetCaughtUp(WaitTargetCaughtUpInput),
    AttestTargetTopology(AttestTargetTopologyInput),
    AttestCompensatedTopology(AttestCompensatedTopologyInput),
}

pub enum DirectEvaluation {
    Observe(ExactBytes),
    AwaitEvidence,
    DispatchReplica {
        action: DurableReplicaAction,
        pending: Box<PendingActionStatus>,
    },
    DispatchLabel,
}

impl DirectActivity {
    pub fn decode(spec: &ActivitySpec) -> Result<Self, String> {
        let name = spec.name().name();
        Ok(match name {
            RevokeWritesActivity::NAME => Self::Replica {
                operation: DirectReplicaOperation::RevokeWrites,
                request: decode_replica::<RevokeWritesActivity>(spec)?,
            },
            DemoteOldPrimaryActivity::NAME => Self::Replica {
                operation: DirectReplicaOperation::DemoteOldPrimary,
                request: decode_replica::<DemoteOldPrimaryActivity>(spec)?,
            },
            PromoteTargetActivity::NAME => Self::Replica {
                operation: DirectReplicaOperation::PromoteTarget,
                request: decode_replica::<PromoteTargetActivity>(spec)?,
            },
            DistributeReplicaEpochActivity::NAME => Self::Replica {
                operation: DirectReplicaOperation::DistributeReplicaEpoch,
                request: decode_replica::<DistributeReplicaEpochActivity>(spec)?,
            },
            InstallTargetCatchUpConfigurationActivity::NAME => Self::Replica {
                operation: DirectReplicaOperation::InstallTargetCatchUpConfiguration,
                request: decode_replica::<InstallTargetCatchUpConfigurationActivity>(spec)?,
            },
            WaitTargetWriteQuorumActivity::NAME => Self::Replica {
                operation: DirectReplicaOperation::WaitTargetWriteQuorum,
                request: decode_replica::<WaitTargetWriteQuorumActivity>(spec)?,
            },
            InstallTargetCurrentConfigurationActivity::NAME => Self::Replica {
                operation: DirectReplicaOperation::InstallTargetCurrentConfiguration,
                request: decode_replica::<InstallTargetCurrentConfigurationActivity>(spec)?,
            },
            RestorePreviousCurrentConfigurationActivity::NAME => Self::Replica {
                operation: DirectReplicaOperation::RestorePreviousCurrentConfiguration,
                request: decode_replica::<RestorePreviousCurrentConfigurationActivity>(spec)?,
            },
            CompensatePromoteOldPrimaryActivity::NAME => Self::Replica {
                operation: DirectReplicaOperation::CompensatePromoteOldPrimary,
                request: decode_replica::<CompensatePromoteOldPrimaryActivity>(spec)?,
            },
            CompensateDistributeReplicaEpochActivity::NAME => Self::Replica {
                operation: DirectReplicaOperation::CompensateDistributeReplicaEpoch,
                request: decode_replica::<CompensateDistributeReplicaEpochActivity>(spec)?,
            },
            InstallCompensationCatchUpConfigurationActivity::NAME => Self::Replica {
                operation: DirectReplicaOperation::InstallCompensationCatchUpConfiguration,
                request: decode_replica::<InstallCompensationCatchUpConfigurationActivity>(spec)?,
            },
            InstallCompensationCurrentConfigurationActivity::NAME => Self::Replica {
                operation: DirectReplicaOperation::InstallCompensationCurrentConfiguration,
                request: decode_replica::<InstallCompensationCurrentConfigurationActivity>(spec)?,
            },
            PublishTargetPrimaryLabelActivity::NAME => Self::Label {
                operation: DirectLabelOperation::PublishTargetPrimary,
                request: decode_label::<PublishTargetPrimaryLabelActivity>(spec)?,
            },
            PublishOldPrimarySecondaryLabelActivity::NAME => Self::Label {
                operation: DirectLabelOperation::PublishOldPrimarySecondary,
                request: decode_label::<PublishOldPrimarySecondaryLabelActivity>(spec)?,
            },
            RestoreOldPrimaryLabelActivity::NAME => Self::Label {
                operation: DirectLabelOperation::RestoreOldPrimary,
                request: decode_label::<RestoreOldPrimaryLabelActivity>(spec)?,
            },
            RestoreTargetSecondaryLabelActivity::NAME => Self::Label {
                operation: DirectLabelOperation::RestoreTargetSecondary,
                request: decode_label::<RestoreTargetSecondaryLabelActivity>(spec)?,
            },
            CaptureFrozenLsnActivity::NAME => {
                Self::CaptureFrozenLsn(decode::<CaptureFrozenLsnActivity>(spec)?)
            }
            WaitTargetCaughtUpActivity::NAME => {
                Self::WaitTargetCaughtUp(decode::<WaitTargetCaughtUpActivity>(spec)?)
            }
            AttestTargetTopologyActivity::NAME => {
                Self::AttestTargetTopology(decode::<AttestTargetTopologyActivity>(spec)?)
            }
            AttestCompensatedTopologyActivity::NAME => {
                Self::AttestCompensatedTopology(decode::<AttestCompensatedTopologyActivity>(spec)?)
            }
            _ => return Err(format!("unknown direct switchover activity {name}")),
        })
    }

    pub fn spec(&self) -> Result<ActivitySpec, String> {
        match self {
            Self::Replica { operation, request } => operation.spec(request.clone()),
            Self::Label { operation, request } => operation.spec(request.clone()),
            Self::CaptureFrozenLsn(input) => spec::<CaptureFrozenLsnActivity>(input),
            Self::WaitTargetCaughtUp(input) => spec::<WaitTargetCaughtUpActivity>(input),
            Self::AttestTargetTopology(input) => spec::<AttestTargetTopologyActivity>(input),
            Self::AttestCompensatedTopology(input) => {
                spec::<AttestCompensatedTopologyActivity>(input)
            }
        }
    }

    pub fn deadline_unix_seconds(&self) -> i64 {
        match self {
            Self::Replica { request, .. } => request.deadline_unix_seconds,
            Self::Label { request, .. } => request.deadline_unix_seconds,
            Self::CaptureFrozenLsn(input) => input.deadline_unix_seconds,
            Self::WaitTargetCaughtUp(input) => input.deadline_unix_seconds,
            Self::AttestTargetTopology(input) => input.deadline_unix_seconds,
            Self::AttestCompensatedTopology(input) => input.deadline_unix_seconds,
        }
    }

    pub fn logical_predecessor(&self) -> Self {
        let mut predecessor = self.clone();
        match &mut predecessor {
            Self::Replica { request, .. } => request.prepared_command = None,
            Self::Label { request, .. } => request.prepared_command = None,
            Self::CaptureFrozenLsn(_)
            | Self::WaitTargetCaughtUp(_)
            | Self::AttestTargetTopology(_)
            | Self::AttestCompensatedTopology(_) => {}
        }
        predecessor
    }

    pub fn is_logical(&self) -> bool {
        match self {
            Self::Replica { request, .. } => request.prepared_command.is_none(),
            Self::Label { request, .. } => request.prepared_command.is_none(),
            Self::CaptureFrozenLsn(_)
            | Self::WaitTargetCaughtUp(_)
            | Self::AttestTargetTopology(_)
            | Self::AttestCompensatedTopology(_) => true,
        }
    }

    pub fn validate(&self, definition: &DirectSwitchoverDefinition) -> Result<(), String> {
        match self {
            Self::Replica { operation, request } => {
                operation.validate_request(request, definition)?;
                if request.prepared_command.is_some() {
                    operation.validate_prepared(request, definition)?;
                }
            }
            Self::Label { operation, request } => {
                operation.validate_request(request, definition)?;
                if request.prepared_command.is_some() {
                    operation.validate_prepared(request)?;
                }
            }
            Self::CaptureFrozenLsn(input) => {
                validate_common(
                    input.contract_version,
                    &input.execution_id,
                    input.deadline_unix_seconds,
                    definition,
                )?;
                let old = definition.member(definition.old_primary_id)?;
                if input.old_primary_id != definition.old_primary_id
                    || input.old_primary_instance_id != old.instance_id
                    || input.expected_epoch != definition.previous_snapshot.epoch
                {
                    return Err("capture-frozen-lsn input conflicts with admission".to_string());
                }
            }
            Self::WaitTargetCaughtUp(input) => {
                validate_common(
                    input.contract_version,
                    &input.execution_id,
                    input.deadline_unix_seconds,
                    definition,
                )?;
                let target = definition.member(definition.target_primary_id)?;
                if input.target_id != definition.target_primary_id
                    || input.target_instance_id != target.instance_id
                    || input.expected_epoch != definition.previous_snapshot.epoch
                    || input.frozen_lsn < 0
                {
                    return Err("wait-target-caught-up input conflicts with admission".to_string());
                }
            }
            Self::AttestTargetTopology(input) => {
                validate_common(
                    input.contract_version,
                    &input.execution_id,
                    input.deadline_unix_seconds,
                    definition,
                )?;
                if input.expected_snapshot != definition.target_snapshot {
                    return Err("target attestation snapshot conflicts with admission".to_string());
                }
            }
            Self::AttestCompensatedTopology(input) => {
                validate_common(
                    input.contract_version,
                    &input.execution_id,
                    input.deadline_unix_seconds,
                    definition,
                )?;
                let compensation = definition.compensation_snapshot();
                if input.expected_snapshot != definition.previous_snapshot
                    && input.expected_snapshot != compensation
                {
                    return Err(
                        "compensated attestation snapshot conflicts with admission".to_string()
                    );
                }
            }
        }
        Ok(())
    }

    pub fn evaluate(
        &self,
        definition: &DirectSwitchoverDefinition,
        observations: &OperationObservations,
        now: i64,
    ) -> Result<DirectEvaluation, String> {
        self.validate(definition)?;
        match self {
            Self::Replica { operation, request } => {
                operation.evaluate(request, definition, observations, now)
            }
            Self::Label { operation, request } => {
                operation.evaluate(request, definition, observations, now)
            }
            Self::CaptureFrozenLsn(input) => evaluate_capture(input, observations, now),
            Self::WaitTargetCaughtUp(input) => evaluate_target_catch_up(input, observations, now),
            Self::AttestTargetTopology(input) => {
                evaluate_attestation::<AttestTargetTopologyActivity, _, _, _>(
                    &input.expected_snapshot,
                    input.deadline_unix_seconds,
                    observations,
                    now,
                    |observed_at, snapshot| AttestTargetTopologyOutput::Attested {
                        observed_at_unix_seconds: observed_at,
                        snapshot,
                        accounting: None,
                    },
                    |observed_at, message| AttestTargetTopologyOutput::DeadlineExceeded {
                        observed_at_unix_seconds: observed_at,
                        message,
                    },
                    |observed_at, message| AttestTargetTopologyOutput::Conflicting {
                        observed_at_unix_seconds: observed_at,
                        message,
                    },
                )
            }
            Self::AttestCompensatedTopology(input) => {
                evaluate_attestation::<AttestCompensatedTopologyActivity, _, _, _>(
                    &input.expected_snapshot,
                    input.deadline_unix_seconds,
                    observations,
                    now,
                    |observed_at, snapshot| AttestCompensatedTopologyOutput::Attested {
                        observed_at_unix_seconds: observed_at,
                        snapshot,
                        accounting: None,
                    },
                    |observed_at, message| AttestCompensatedTopologyOutput::DeadlineExceeded {
                        observed_at_unix_seconds: observed_at,
                        message,
                    },
                    |observed_at, message| AttestCompensatedTopologyOutput::Conflicting {
                        observed_at_unix_seconds: observed_at,
                        message,
                    },
                )
            }
        }
    }

    pub fn prepare(
        &self,
        definition: &DirectSwitchoverDefinition,
        observations: &OperationObservations,
        addressed_instances: &BTreeMap<i64, ReplicaInstanceId>,
        now: i64,
    ) -> Result<Self, PreparedActivityError> {
        if !self.is_logical() {
            return Err(PreparedActivityError::Validation);
        }
        match self
            .evaluate(definition, observations, now)
            .map_err(|_| PreparedActivityError::Validation)?
        {
            DirectEvaluation::Observe(_) => Ok(self.clone()),
            DirectEvaluation::AwaitEvidence => match self {
                Self::Replica { .. } | Self::Label { .. } => Err(PreparedActivityError::Derivation),
                _ => Ok(self.clone()),
            },
            DirectEvaluation::DispatchReplica { action, pending } => {
                let observed = observations
                    .get(&pending.target_id)
                    .ok_or(PreparedActivityError::Derivation)?;
                let addressed = addressed_instances
                    .get(&pending.target_id)
                    .ok_or(PreparedActivityError::Derivation)?;
                let (_, command) =
                    prepare_replica_effect_command(&pending, &observed.status, addressed, &action)
                        .map_err(preparation_error)?;
                let mut prepared = self.clone();
                let Self::Replica { request, .. } = &mut prepared else {
                    return Err(PreparedActivityError::Validation);
                };
                request.prepared_command = Some(command);
                prepared
                    .validate(definition)
                    .map_err(|_| PreparedActivityError::Validation)?;
                Ok(prepared)
            }
            DirectEvaluation::DispatchLabel => {
                let mut prepared = self.clone();
                let Self::Label { request, .. } = &mut prepared else {
                    return Err(PreparedActivityError::Validation);
                };
                let observed = observations
                    .get(&request.target_id)
                    .ok_or(PreparedActivityError::Derivation)?;
                if observed.status.instance_id.as_str() != request.target_instance_id {
                    return Err(PreparedActivityError::Validation);
                }
                request.prepared_command = Some(LabelEffectCommand::new(
                    request.target_id,
                    observed.pod_name.clone(),
                    request.target_instance_id.clone(),
                    request.desired_role.clone(),
                ));
                prepared
                    .validate(definition)
                    .map_err(|_| PreparedActivityError::Validation)?;
                Ok(prepared)
            }
        }
    }

    pub fn encode_effect_observation(
        &self,
        observation: EffectObservation,
    ) -> Result<ExactBytes, String> {
        match self {
            Self::Replica { operation, .. } => operation.encode_observation(observation),
            Self::Label { operation, .. } => operation.encode_observation(observation),
            _ => Err("passive direct activity cannot encode an effect observation".to_string()),
        }
    }

    pub fn prepared_replica_command(&self) -> Option<&ReplicaEffectCommand> {
        match self {
            Self::Replica { request, .. } => request.prepared_command.as_ref(),
            _ => None,
        }
    }

    pub fn prepared_label_command(&self) -> Option<&LabelEffectCommand> {
        match self {
            Self::Label { request, .. } => request.prepared_command.as_ref(),
            _ => None,
        }
    }
}

impl DirectReplicaOperation {
    fn spec(self, request: ReplicaOperationRequest) -> Result<ActivitySpec, String> {
        match self {
            Self::RevokeWrites => replica_spec::<RevokeWritesActivity>(request),
            Self::DemoteOldPrimary => replica_spec::<DemoteOldPrimaryActivity>(request),
            Self::PromoteTarget => replica_spec::<PromoteTargetActivity>(request),
            Self::DistributeReplicaEpoch => replica_spec::<DistributeReplicaEpochActivity>(request),
            Self::InstallTargetCatchUpConfiguration => {
                replica_spec::<InstallTargetCatchUpConfigurationActivity>(request)
            }
            Self::WaitTargetWriteQuorum => replica_spec::<WaitTargetWriteQuorumActivity>(request),
            Self::InstallTargetCurrentConfiguration => {
                replica_spec::<InstallTargetCurrentConfigurationActivity>(request)
            }
            Self::RestorePreviousCurrentConfiguration => {
                replica_spec::<RestorePreviousCurrentConfigurationActivity>(request)
            }
            Self::CompensatePromoteOldPrimary => {
                replica_spec::<CompensatePromoteOldPrimaryActivity>(request)
            }
            Self::CompensateDistributeReplicaEpoch => {
                replica_spec::<CompensateDistributeReplicaEpochActivity>(request)
            }
            Self::InstallCompensationCatchUpConfiguration => {
                replica_spec::<InstallCompensationCatchUpConfigurationActivity>(request)
            }
            Self::InstallCompensationCurrentConfiguration => {
                replica_spec::<InstallCompensationCurrentConfigurationActivity>(request)
            }
        }
    }

    fn validate_request(
        self,
        request: &ReplicaOperationRequest,
        definition: &DirectSwitchoverDefinition,
    ) -> Result<(), String> {
        validate_common(
            request.contract_version,
            &request.execution_id,
            request.deadline_unix_seconds,
            definition,
        )?;
        if request.redelivery > 1
            || request.action_id != format!("{}:{}", definition.execution_id, request.sequence)
        {
            return Err(
                "direct replica request has invalid redelivery or action identity".to_string(),
            );
        }
        let (sequence, target_id, expected_epoch, desired_snapshot) =
            self.expected_request(definition, request.sequence)?;
        let member = definition.member(target_id)?;
        if request.sequence != sequence
            || request.target_id != target_id
            || request.target_instance_id != member.instance_id
            || request.expected_epoch != expected_epoch
            || request.desired_snapshot != desired_snapshot
        {
            return Err("direct replica request conflicts with operation semantics".to_string());
        }
        Ok(())
    }

    fn expected_request(
        self,
        definition: &DirectSwitchoverDefinition,
        actual_sequence: u32,
    ) -> Result<(u32, i64, EpochStatus, StablePartitionSnapshotStatus), String> {
        let previous = definition.previous_snapshot.clone();
        let target = definition.target_snapshot.clone();
        let compensation = definition.compensation_snapshot();
        Ok(match self {
            Self::RevokeWrites => (
                1,
                definition.old_primary_id,
                previous.epoch.clone(),
                previous,
            ),
            Self::DemoteOldPrimary => (2, definition.old_primary_id, target.epoch.clone(), target),
            Self::PromoteTarget => (
                3,
                definition.target_primary_id,
                previous.epoch.clone(),
                target,
            ),
            Self::DistributeReplicaEpoch => {
                let index = actual_sequence
                    .checked_sub(100)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| "normal epoch sequence is invalid".to_string())?;
                let target_id = *definition
                    .normal_epoch_distribution_ids()
                    .get(index)
                    .ok_or_else(|| "normal epoch sequence is outside membership".to_string())?;
                (actual_sequence, target_id, target.epoch.clone(), target)
            }
            Self::InstallTargetCatchUpConfiguration => (
                1000,
                definition.target_primary_id,
                target.epoch.clone(),
                target,
            ),
            Self::WaitTargetWriteQuorum => (
                1001,
                definition.target_primary_id,
                target.epoch.clone(),
                target,
            ),
            Self::InstallTargetCurrentConfiguration => (
                1002,
                definition.target_primary_id,
                target.epoch.clone(),
                target,
            ),
            Self::RestorePreviousCurrentConfiguration => (
                1500,
                definition.old_primary_id,
                previous.epoch.clone(),
                previous,
            ),
            Self::CompensatePromoteOldPrimary => (
                2000,
                definition.old_primary_id,
                target.epoch.clone(),
                compensation,
            ),
            Self::CompensateDistributeReplicaEpoch => {
                let index = actual_sequence
                    .checked_sub(2100)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| "compensation epoch sequence is invalid".to_string())?;
                let target_id = *definition
                    .compensation_epoch_distribution_ids()
                    .get(index)
                    .ok_or_else(|| {
                        "compensation epoch sequence is outside membership".to_string()
                    })?;
                (
                    actual_sequence,
                    target_id,
                    target.epoch.clone(),
                    compensation,
                )
            }
            Self::InstallCompensationCatchUpConfiguration => (
                2001,
                definition.old_primary_id,
                target.epoch.clone(),
                compensation,
            ),
            Self::InstallCompensationCurrentConfiguration => (
                2002,
                definition.old_primary_id,
                target.epoch.clone(),
                compensation,
            ),
        })
    }

    fn action_kind(self) -> DurableActionKind {
        match self {
            Self::RevokeWrites => DurableActionKind::RevokeWrite,
            Self::DemoteOldPrimary => DurableActionKind::DemoteOldPrimary,
            Self::PromoteTarget => DurableActionKind::PromoteTarget,
            Self::DistributeReplicaEpoch => DurableActionKind::UpdateSecondaryEpoch,
            Self::InstallTargetCatchUpConfiguration => {
                DurableActionKind::UpdateCatchUpConfiguration
            }
            Self::WaitTargetWriteQuorum => DurableActionKind::WaitForCatchUpQuorum,
            Self::InstallTargetCurrentConfiguration => {
                DurableActionKind::UpdateCurrentConfiguration
            }
            Self::RestorePreviousCurrentConfiguration => {
                DurableActionKind::RestorePreviousConfiguration
            }
            Self::CompensatePromoteOldPrimary => DurableActionKind::CompensatePromoteOldPrimary,
            Self::CompensateDistributeReplicaEpoch => {
                DurableActionKind::CompensateUpdateSecondaryEpoch
            }
            Self::InstallCompensationCatchUpConfiguration => {
                DurableActionKind::CompensateCatchUpConfiguration
            }
            Self::InstallCompensationCurrentConfiguration => {
                DurableActionKind::CompensateCurrentConfiguration
            }
        }
    }

    fn postcondition(self) -> DurablePostconditionStatus {
        let (kind, role) = match self {
            Self::RevokeWrites => (DurablePostconditionKind::WriteRevoked, None),
            Self::DemoteOldPrimary => (
                DurablePostconditionKind::Role,
                Some(StableReplicaRoleStatus::ActiveSecondary),
            ),
            Self::PromoteTarget | Self::CompensatePromoteOldPrimary => (
                DurablePostconditionKind::Role,
                Some(StableReplicaRoleStatus::Primary),
            ),
            Self::DistributeReplicaEpoch | Self::CompensateDistributeReplicaEpoch => (
                DurablePostconditionKind::Epoch,
                Some(StableReplicaRoleStatus::ActiveSecondary),
            ),
            Self::InstallTargetCatchUpConfiguration
            | Self::InstallCompensationCatchUpConfiguration => {
                (DurablePostconditionKind::CatchUpConfiguration, None)
            }
            Self::WaitTargetWriteQuorum => (DurablePostconditionKind::CatchUpQuorum, None),
            Self::InstallTargetCurrentConfiguration
            | Self::RestorePreviousCurrentConfiguration
            | Self::InstallCompensationCurrentConfiguration => {
                (DurablePostconditionKind::CurrentConfiguration, None)
            }
        };
        DurablePostconditionStatus { kind, role }
    }

    fn pending(self, request: &ReplicaOperationRequest) -> PendingActionStatus {
        let mut pending = PendingActionStatus {
            action_id: request.action_id.clone(),
            sequence: request.sequence,
            kind: self.action_kind(),
            target_id: request.target_id,
            target_instance_id: request.target_instance_id.clone(),
            expected_epoch: request.expected_epoch.clone(),
            desired_postcondition: self.postcondition(),
            attempts: u32::from(request.redelivery),
            deadline_unix_seconds: request.deadline_unix_seconds,
            last_error: None,
            dispatch_authorized: request.prepared_command.is_some(),
            dispatch_agent_generation: None,
            dispatch_agent_control_version: None,
            dispatch_observed_runtime_epoch: None,
            dispatch_action_payload: String::new(),
        };
        if let Some(command) = request.prepared_command.as_ref() {
            pending.dispatch_agent_generation = Some(command.expected_agent_generation.clone());
            pending.dispatch_agent_control_version = Some(command.expected_control_version);
            pending.dispatch_observed_runtime_epoch = Some(command.observed_runtime_epoch.clone());
            pending.dispatch_action_payload = command.action_payload.clone();
        }
        pending
    }

    fn action(
        self,
        request: &ReplicaOperationRequest,
        definition: &DirectSwitchoverDefinition,
        observations: &OperationObservations,
    ) -> Result<DurableReplicaAction, String> {
        let target_epoch = epoch(&definition.target_snapshot.epoch);
        Ok(match self {
            Self::RevokeWrites => DurableReplicaAction::RevokeWriteStatus,
            Self::DemoteOldPrimary => DurableReplicaAction::ChangeRole {
                epoch: target_epoch,
                role: Role::ActiveSecondary,
            },
            Self::PromoteTarget | Self::CompensatePromoteOldPrimary => {
                DurableReplicaAction::ChangeRole {
                    epoch: target_epoch,
                    role: Role::Primary,
                }
            }
            Self::DistributeReplicaEpoch | Self::CompensateDistributeReplicaEpoch => {
                DurableReplicaAction::UpdateEpoch {
                    epoch: target_epoch,
                }
            }
            Self::InstallTargetCatchUpConfiguration => {
                DurableReplicaAction::UpdateCatchUpConfiguration {
                    current: config_for_snapshot(&request.desired_snapshot, observations)?,
                    previous: config_for_snapshot(&definition.previous_snapshot, observations)?,
                }
            }
            Self::WaitTargetWriteQuorum => DurableReplicaAction::WaitForCatchUpQuorum {
                mode: ReplicaSetQuorumMode::Write,
            },
            Self::InstallTargetCurrentConfiguration
            | Self::RestorePreviousCurrentConfiguration
            | Self::InstallCompensationCurrentConfiguration => {
                DurableReplicaAction::UpdateCurrentConfiguration {
                    current: config_for_snapshot(&request.desired_snapshot, observations)?,
                }
            }
            Self::InstallCompensationCatchUpConfiguration => {
                DurableReplicaAction::UpdateCatchUpConfiguration {
                    current: config_for_snapshot(&request.desired_snapshot, observations)?,
                    previous: config_for_snapshot(&definition.previous_snapshot, observations)?,
                }
            }
        })
    }

    fn evaluate(
        self,
        request: &ReplicaOperationRequest,
        definition: &DirectSwitchoverDefinition,
        observations: &OperationObservations,
        now: i64,
    ) -> Result<DirectEvaluation, String> {
        let Some(observed) = observations.get(&request.target_id) else {
            return deadline_or_wait(request.deadline_unix_seconds, now, || {
                self.encode_observation(EffectObservation::UnavailableAtDeadline {
                    observed_at_unix_seconds: now,
                    message: format!(
                        "direct switchover replica {} is unavailable at deadline",
                        request.target_id
                    ),
                })
            });
        };
        if observed.status.instance_id.as_str() != request.target_instance_id {
            return self
                .encode_observation(EffectObservation::Conflicting {
                    observed_at_unix_seconds: now,
                    message: format!(
                        "direct switchover replica {} incarnation changed",
                        request.target_id
                    ),
                })
                .map(DirectEvaluation::Observe);
        }
        if let Some(recorded) = correlated_action_observation(&observed.status, &request.action_id)
        {
            let action = match self.action(request, definition, observations) {
                Ok(action) => action,
                Err(error) => {
                    return deadline_or_wait(request.deadline_unix_seconds, now, || {
                        self.encode_observation(EffectObservation::DeadlineExceeded {
                            observed_at_unix_seconds: now,
                            message: error,
                        })
                    });
                }
            };
            if recorded.signature != action.signature() {
                return self
                    .encode_observation(EffectObservation::Conflicting {
                        observed_at_unix_seconds: now,
                        message: "correlated action signature conflicts with direct request"
                            .to_string(),
                    })
                    .map(DirectEvaluation::Observe);
            }
            return match recorded.state {
                DurableActionState::Completed => self
                    .encode_observation(EffectObservation::Applied {
                        observed_at_unix_seconds: now,
                    })
                    .map(DirectEvaluation::Observe),
                DurableActionState::Failed => self
                    .encode_observation(EffectObservation::Failed {
                        observed_at_unix_seconds: now,
                        message: recorded.error.clone().unwrap_or_else(|| {
                            "correlated direct switchover action failed".to_string()
                        }),
                    })
                    .map(DirectEvaluation::Observe),
                DurableActionState::Scheduled | DurableActionState::InProgress => {
                    deadline_or_wait(request.deadline_unix_seconds, now, || {
                        self.encode_observation(EffectObservation::DeadlineExceeded {
                            observed_at_unix_seconds: now,
                            message: "correlated direct switchover action reached its deadline"
                                .to_string(),
                        })
                    })
                }
            };
        }
        match self.status_relation(request, definition, &observed.status)? {
            StatusRelation::Postcondition => self
                .encode_observation(EffectObservation::Applied {
                    observed_at_unix_seconds: now,
                })
                .map(DirectEvaluation::Observe),
            StatusRelation::Precondition if now >= request.deadline_unix_seconds => self
                .encode_observation(EffectObservation::DeadlineExceeded {
                    observed_at_unix_seconds: now,
                    message: format!(
                        "direct switchover action {:?} reached its deadline",
                        self.action_kind()
                    ),
                })
                .map(DirectEvaluation::Observe),
            StatusRelation::Precondition => {
                let action = match self.action(request, definition, observations) {
                    Ok(action) => action,
                    Err(_) => return Ok(DirectEvaluation::AwaitEvidence),
                };
                Ok(DirectEvaluation::DispatchReplica {
                    action,
                    pending: Box::new(self.pending(request)),
                })
            }
            StatusRelation::Conflicting(message) => self
                .encode_observation(EffectObservation::Conflicting {
                    observed_at_unix_seconds: now,
                    message,
                })
                .map(DirectEvaluation::Observe),
        }
    }

    fn status_relation(
        self,
        request: &ReplicaOperationRequest,
        definition: &DirectSwitchoverDefinition,
        status: &kuberic_core::types::ReplicaStatusInfo,
    ) -> Result<StatusRelation, String> {
        let previous_epoch = epoch(&definition.previous_snapshot.epoch);
        let target_epoch = epoch(&definition.target_snapshot.epoch);
        let desired_current =
            configuration_status(ReplicaConfigurationMode::Current, &request.desired_snapshot);
        let desired_catch_up =
            configuration_status(ReplicaConfigurationMode::CatchUp, &request.desired_snapshot);
        Ok(match self {
            Self::RevokeWrites => {
                if status.role == Role::Primary
                    && status.epoch == previous_epoch
                    && status.write_status == AccessStatus::ReconfigurationPending
                {
                    StatusRelation::Postcondition
                } else if status.role == Role::Primary
                    && status.epoch == previous_epoch
                    && status.write_status == AccessStatus::Granted
                {
                    StatusRelation::Precondition
                } else {
                    StatusRelation::conflicting("revoke-writes pre/postcondition is not exact")
                }
            }
            Self::DemoteOldPrimary => {
                if status.role == Role::ActiveSecondary && status.epoch == target_epoch {
                    StatusRelation::Postcondition
                } else if status.role == Role::Primary
                    && status.epoch == previous_epoch
                    && status.write_status == AccessStatus::ReconfigurationPending
                {
                    StatusRelation::Precondition
                } else {
                    StatusRelation::conflicting("demote-old-primary pre/postcondition is not exact")
                }
            }
            Self::PromoteTarget => {
                if status.role == Role::Primary && status.epoch == target_epoch {
                    StatusRelation::Postcondition
                } else if status.role == Role::ActiveSecondary && status.epoch == previous_epoch {
                    StatusRelation::Precondition
                } else {
                    StatusRelation::conflicting("promote-target pre/postcondition is not exact")
                }
            }
            Self::DistributeReplicaEpoch | Self::CompensateDistributeReplicaEpoch => {
                if status.role == Role::ActiveSecondary && status.epoch == target_epoch {
                    StatusRelation::Postcondition
                } else if status.role == Role::ActiveSecondary && status.epoch == previous_epoch {
                    StatusRelation::Precondition
                } else {
                    StatusRelation::conflicting(
                        "replica-epoch distribution pre/postcondition is not exact",
                    )
                }
            }
            Self::InstallTargetCatchUpConfiguration => {
                configuration_relation(status, &desired_catch_up, None, true, target_epoch)
            }
            Self::WaitTargetWriteQuorum => {
                if status.role == Role::Primary && status.epoch == target_epoch {
                    StatusRelation::Precondition
                } else {
                    StatusRelation::conflicting("write-quorum wait target is not exact primary")
                }
            }
            Self::InstallTargetCurrentConfiguration => configuration_relation(
                status,
                &desired_current,
                Some(&desired_catch_up),
                false,
                target_epoch,
            ),
            Self::RestorePreviousCurrentConfiguration => {
                if status.role != Role::Primary || status.epoch != previous_epoch {
                    StatusRelation::conflicting(
                        "previous-configuration restore target is not exact primary",
                    )
                } else if status.configuration.as_ref() == Some(&desired_current)
                    && status.write_status == AccessStatus::Granted
                {
                    StatusRelation::Postcondition
                } else if status.configuration.as_ref() == Some(&desired_current)
                    && status.write_status == AccessStatus::ReconfigurationPending
                {
                    StatusRelation::Precondition
                } else {
                    StatusRelation::conflicting(
                        "previous-configuration restore pre/postcondition is not exact",
                    )
                }
            }
            Self::CompensatePromoteOldPrimary => {
                if status.role == Role::Primary && status.epoch == target_epoch {
                    StatusRelation::Postcondition
                } else if status.role == Role::ActiveSecondary && status.epoch == target_epoch {
                    StatusRelation::Precondition
                } else {
                    StatusRelation::conflicting(
                        "compensating promotion pre/postcondition is not exact",
                    )
                }
            }
            Self::InstallCompensationCatchUpConfiguration => {
                let previous_current = configuration_status(
                    ReplicaConfigurationMode::Current,
                    &definition.previous_snapshot,
                );
                configuration_relation(
                    status,
                    &desired_catch_up,
                    Some(&previous_current),
                    false,
                    target_epoch,
                )
            }
            Self::InstallCompensationCurrentConfiguration => configuration_relation(
                status,
                &desired_current,
                Some(&desired_catch_up),
                false,
                target_epoch,
            ),
        })
    }

    fn validate_prepared(
        self,
        request: &ReplicaOperationRequest,
        definition: &DirectSwitchoverDefinition,
    ) -> Result<(), String> {
        let command = request
            .prepared_command
            .as_ref()
            .ok_or_else(|| "direct replica activity is not prepared".to_string())?;
        let pending = self.pending(request);
        if ReplicaEffectCommand::from_pending(&pending)? != *command
            || command.action_id != request.action_id
            || command.target_id != request.target_id
            || command.target_instance_id != request.target_instance_id
            || command.expected_epoch != request.expected_epoch
            || command.desired_postcondition != self.postcondition()
            || command.expected_agent_generation.is_empty()
            || command.expected_control_version == 0
            || command.action_payload.is_empty()
        {
            return Err("prepared direct replica command has invalid exact identity".to_string());
        }
        let action = kuberic_core::grpc::convert::decode_direct_correlated_action_payload(
            &command.action_payload,
        )
        .map_err(|error| format!("decode direct prepared action: {error}"))?;
        if action.signature() != command.action_signature
            || !self.action_has_fixed_semantics(&action, &request.desired_snapshot, definition)
        {
            return Err("prepared direct replica command changed fixed semantics".to_string());
        }
        Ok(())
    }

    fn action_has_fixed_semantics(
        self,
        action: &DurableReplicaAction,
        desired_snapshot: &StablePartitionSnapshotStatus,
        definition: &DirectSwitchoverDefinition,
    ) -> bool {
        let target_epoch = epoch(&definition.target_snapshot.epoch);
        match (self, action) {
            (Self::RevokeWrites, DurableReplicaAction::RevokeWriteStatus) => true,
            (
                Self::DemoteOldPrimary,
                DurableReplicaAction::ChangeRole {
                    epoch,
                    role: Role::ActiveSecondary,
                },
            )
            | (
                Self::PromoteTarget | Self::CompensatePromoteOldPrimary,
                DurableReplicaAction::ChangeRole {
                    epoch,
                    role: Role::Primary,
                },
            )
            | (
                Self::DistributeReplicaEpoch | Self::CompensateDistributeReplicaEpoch,
                DurableReplicaAction::UpdateEpoch { epoch },
            ) => *epoch == target_epoch,
            (
                Self::InstallTargetCatchUpConfiguration
                | Self::InstallCompensationCatchUpConfiguration,
                DurableReplicaAction::UpdateCatchUpConfiguration { current, previous },
            ) => {
                config_matches_snapshot(current, desired_snapshot)
                    && config_matches_snapshot(previous, &definition.previous_snapshot)
            }
            (
                Self::WaitTargetWriteQuorum,
                DurableReplicaAction::WaitForCatchUpQuorum {
                    mode: ReplicaSetQuorumMode::Write,
                },
            ) => true,
            (
                Self::InstallTargetCurrentConfiguration
                | Self::RestorePreviousCurrentConfiguration
                | Self::InstallCompensationCurrentConfiguration,
                DurableReplicaAction::UpdateCurrentConfiguration { current },
            ) => config_matches_snapshot(current, desired_snapshot),
            _ => false,
        }
    }

    fn encode_observation(self, observation: EffectObservation) -> Result<ExactBytes, String> {
        match self {
            Self::RevokeWrites => encode_replica_observation::<RevokeWritesActivity>(observation),
            Self::DemoteOldPrimary => {
                encode_replica_observation::<DemoteOldPrimaryActivity>(observation)
            }
            Self::PromoteTarget => encode_replica_observation::<PromoteTargetActivity>(observation),
            Self::DistributeReplicaEpoch => {
                encode_replica_observation::<DistributeReplicaEpochActivity>(observation)
            }
            Self::InstallTargetCatchUpConfiguration => {
                encode_replica_observation::<InstallTargetCatchUpConfigurationActivity>(observation)
            }
            Self::WaitTargetWriteQuorum => {
                encode_replica_observation::<WaitTargetWriteQuorumActivity>(observation)
            }
            Self::InstallTargetCurrentConfiguration => {
                encode_replica_observation::<InstallTargetCurrentConfigurationActivity>(observation)
            }
            Self::RestorePreviousCurrentConfiguration => encode_replica_observation::<
                RestorePreviousCurrentConfigurationActivity,
            >(observation),
            Self::CompensatePromoteOldPrimary => {
                encode_replica_observation::<CompensatePromoteOldPrimaryActivity>(observation)
            }
            Self::CompensateDistributeReplicaEpoch => {
                encode_replica_observation::<CompensateDistributeReplicaEpochActivity>(observation)
            }
            Self::InstallCompensationCatchUpConfiguration => encode_replica_observation::<
                InstallCompensationCatchUpConfigurationActivity,
            >(observation),
            Self::InstallCompensationCurrentConfiguration => encode_replica_observation::<
                InstallCompensationCurrentConfigurationActivity,
            >(observation),
        }
    }
}

impl DirectLabelOperation {
    fn spec(self, request: LabelOperationRequest) -> Result<ActivitySpec, String> {
        match self {
            Self::PublishTargetPrimary => label_spec::<PublishTargetPrimaryLabelActivity>(request),
            Self::PublishOldPrimarySecondary => {
                label_spec::<PublishOldPrimarySecondaryLabelActivity>(request)
            }
            Self::RestoreOldPrimary => label_spec::<RestoreOldPrimaryLabelActivity>(request),
            Self::RestoreTargetSecondary => {
                label_spec::<RestoreTargetSecondaryLabelActivity>(request)
            }
        }
    }

    fn validate_request(
        self,
        request: &LabelOperationRequest,
        definition: &DirectSwitchoverDefinition,
    ) -> Result<(), String> {
        validate_common(
            request.contract_version,
            &request.execution_id,
            request.deadline_unix_seconds,
            definition,
        )?;
        let (sequence, target_id, role) = match self {
            Self::PublishTargetPrimary => (1003, definition.target_primary_id, "primary"),
            Self::PublishOldPrimarySecondary => (1004, definition.old_primary_id, "secondary"),
            Self::RestoreOldPrimary => (2003, definition.old_primary_id, "primary"),
            Self::RestoreTargetSecondary => (2004, definition.target_primary_id, "secondary"),
        };
        let member = definition.member(target_id)?;
        if request.sequence != sequence
            || request.action_id != format!("{}:{sequence}", definition.execution_id)
            || request.target_id != target_id
            || request.target_instance_id != member.instance_id
            || request.desired_role != role
        {
            return Err("direct label request conflicts with operation semantics".to_string());
        }
        Ok(())
    }

    fn validate_prepared(self, request: &LabelOperationRequest) -> Result<(), String> {
        let command = request
            .prepared_command
            .as_ref()
            .ok_or_else(|| "direct label activity is not prepared".to_string())?;
        if command.target_id != request.target_id
            || command.expected_uid != request.target_instance_id
            || command.role != request.desired_role
            || command.pod_name.is_empty()
            || !command.has_valid_identity_signature()
        {
            return Err("prepared direct label command has invalid exact identity".to_string());
        }
        Ok(())
    }

    fn evaluate(
        self,
        request: &LabelOperationRequest,
        definition: &DirectSwitchoverDefinition,
        observations: &OperationObservations,
        now: i64,
    ) -> Result<DirectEvaluation, String> {
        let Some(observed) = observations.get(&request.target_id) else {
            return deadline_or_wait(request.deadline_unix_seconds, now, || {
                self.encode_observation(EffectObservation::UnavailableAtDeadline {
                    observed_at_unix_seconds: now,
                    message: format!(
                        "direct switchover label target {} is unavailable at deadline",
                        request.target_id
                    ),
                })
            });
        };
        if observed.status.instance_id.as_str() != request.target_instance_id {
            return self
                .encode_observation(EffectObservation::Conflicting {
                    observed_at_unix_seconds: now,
                    message: "direct switchover label target incarnation changed".to_string(),
                })
                .map(DirectEvaluation::Observe);
        }
        let expected_role = if request.desired_role == "primary" {
            Role::Primary
        } else {
            Role::ActiveSecondary
        };
        if observed.status.epoch != epoch(&definition.target_snapshot.epoch)
            || observed.status.role != expected_role
        {
            return self
                .encode_observation(EffectObservation::Conflicting {
                    observed_at_unix_seconds: now,
                    message: "direct switchover label target runtime role is not exact".to_string(),
                })
                .map(DirectEvaluation::Observe);
        }
        if observed.pod_role_label.as_deref() == Some(request.desired_role.as_str()) {
            self.encode_observation(EffectObservation::Applied {
                observed_at_unix_seconds: now,
            })
            .map(DirectEvaluation::Observe)
        } else if now >= request.deadline_unix_seconds {
            self.encode_observation(EffectObservation::DeadlineExceeded {
                observed_at_unix_seconds: now,
                message: format!(
                    "direct switchover label action {:?} reached its deadline",
                    self
                ),
            })
            .map(DirectEvaluation::Observe)
        } else {
            Ok(DirectEvaluation::DispatchLabel)
        }
    }

    fn encode_observation(self, observation: EffectObservation) -> Result<ExactBytes, String> {
        match self {
            Self::PublishTargetPrimary => {
                encode_label_observation::<PublishTargetPrimaryLabelActivity>(observation)
            }
            Self::PublishOldPrimarySecondary => {
                encode_label_observation::<PublishOldPrimarySecondaryLabelActivity>(observation)
            }
            Self::RestoreOldPrimary => {
                encode_label_observation::<RestoreOldPrimaryLabelActivity>(observation)
            }
            Self::RestoreTargetSecondary => {
                encode_label_observation::<RestoreTargetSecondaryLabelActivity>(observation)
            }
        }
    }
}

enum StatusRelation {
    Precondition,
    Postcondition,
    Conflicting(String),
}

impl StatusRelation {
    fn conflicting(message: &str) -> Self {
        Self::Conflicting(message.to_string())
    }
}

fn validate_common(
    contract_version: u32,
    execution_id: &str,
    deadline_unix_seconds: i64,
    definition: &DirectSwitchoverDefinition,
) -> Result<(), String> {
    if contract_version != DIRECT_SWITCHOVER_CONTRACT_VERSION
        || execution_id != definition.execution_id
        || deadline_unix_seconds <= 0
    {
        return Err("direct activity identity, version, or deadline is invalid".to_string());
    }
    Ok(())
}

fn decode<A: DurableActivity>(spec: &ActivitySpec) -> Result<A::Input, String> {
    validate_spec_identity::<A>(spec)?;
    decode_activity_input::<A>(spec.input())
        .map_err(|error| format!("decode {} activity input: {error}", A::NAME))
}

fn decode_replica<A: ReplicaDirectActivity>(
    spec: &ActivitySpec,
) -> Result<ReplicaOperationRequest, String> {
    decode::<A>(spec).map(A::request)
}

fn decode_label<A: LabelDirectActivity>(
    spec: &ActivitySpec,
) -> Result<LabelOperationRequest, String> {
    decode::<A>(spec).map(A::request)
}

fn validate_spec_identity<A: DurableActivity>(spec: &ActivitySpec) -> Result<(), String> {
    if spec.name().name() != A::NAME
        || spec.name().version() != A::VERSION
        || spec.max_result_bytes() != A::MAX_RESULT_BYTES
    {
        return Err(format!(
            "{} activity identity or result bound changed",
            A::NAME
        ));
    }
    Ok(())
}

fn spec<A: DurableActivity>(input: &A::Input) -> Result<ActivitySpec, String> {
    Ok(ActivitySpec::new(
        ActivityName::new(A::NAME, A::VERSION)
            .map_err(|error| format!("construct {} activity identity: {error}", A::NAME))?,
        encode_activity_input::<A>(input)
            .map_err(|error| format!("encode {} activity input: {error}", A::NAME))?,
        A::MAX_RESULT_BYTES,
    ))
}

fn replica_spec<A: ReplicaDirectActivity>(
    request: ReplicaOperationRequest,
) -> Result<ActivitySpec, String> {
    spec::<A>(&A::input(request))
}

fn label_spec<A: LabelDirectActivity>(
    request: LabelOperationRequest,
) -> Result<ActivitySpec, String> {
    spec::<A>(&A::input(request))
}

fn encode_replica_observation<A: ReplicaDirectActivity>(
    observation: EffectObservation,
) -> Result<ExactBytes, String> {
    let output = A::output(observation)?;
    encode_activity_result::<A>(&output)
        .map_err(|error| format!("encode {} result: {error}", A::NAME))
}

fn encode_label_observation<A: LabelDirectActivity>(
    observation: EffectObservation,
) -> Result<ExactBytes, String> {
    let output = A::output(observation)?;
    encode_activity_result::<A>(&output)
        .map_err(|error| format!("encode {} result: {error}", A::NAME))
}

fn preparation_error(error: DurableEffectPreparationError) -> PreparedActivityError {
    match error {
        DurableEffectPreparationError::WaitForExactIncarnation
        | DurableEffectPreparationError::WaitForSupportedProtocol => {
            PreparedActivityError::Derivation
        }
        DurableEffectPreparationError::InvalidCommand => PreparedActivityError::Validation,
    }
}

fn evaluate_capture(
    input: &CaptureFrozenLsnInput,
    observations: &OperationObservations,
    now: i64,
) -> Result<DirectEvaluation, String> {
    let output = match observations.get(&input.old_primary_id) {
        None if now < input.deadline_unix_seconds => return Ok(DirectEvaluation::AwaitEvidence),
        None => CaptureFrozenLsnOutput::DeadlineExceeded {
            observed_at_unix_seconds: now,
            message: "old primary was unavailable at frozen-LSN deadline".to_string(),
        },
        Some(observed)
            if observed.status.instance_id.as_str() != input.old_primary_instance_id
                || observed.status.epoch != epoch(&input.expected_epoch)
                || observed.status.role != Role::Primary
                || observed.status.write_status != AccessStatus::ReconfigurationPending =>
        {
            CaptureFrozenLsnOutput::Conflicting {
                observed_at_unix_seconds: now,
                message: "old primary frozen-LSN observation is not exact".to_string(),
            }
        }
        Some(observed) => CaptureFrozenLsnOutput::Captured {
            frozen_lsn: observed.status.current_progress,
            observed_at_unix_seconds: now,
        },
    };
    encode_activity_result::<CaptureFrozenLsnActivity>(&output)
        .map(DirectEvaluation::Observe)
        .map_err(|error| format!("encode capture-frozen-lsn result: {error}"))
}

fn evaluate_target_catch_up(
    input: &WaitTargetCaughtUpInput,
    observations: &OperationObservations,
    now: i64,
) -> Result<DirectEvaluation, String> {
    let output = match observations.get(&input.target_id) {
        None if now < input.deadline_unix_seconds => return Ok(DirectEvaluation::AwaitEvidence),
        None => WaitTargetCaughtUpOutput::DeadlineExceeded {
            observed_at_unix_seconds: now,
            message: "target was unavailable at catch-up deadline".to_string(),
        },
        Some(observed)
            if observed.status.instance_id.as_str() != input.target_instance_id
                || observed.status.epoch != epoch(&input.expected_epoch)
                || observed.status.role != Role::ActiveSecondary =>
        {
            WaitTargetCaughtUpOutput::Conflicting {
                observed_at_unix_seconds: now,
                message: "target catch-up observation is not exact".to_string(),
            }
        }
        Some(observed) if observed.status.current_progress >= input.frozen_lsn => {
            WaitTargetCaughtUpOutput::CaughtUp {
                observed_at_unix_seconds: now,
            }
        }
        Some(_) if now < input.deadline_unix_seconds => {
            return Ok(DirectEvaluation::AwaitEvidence);
        }
        Some(_) => WaitTargetCaughtUpOutput::DeadlineExceeded {
            observed_at_unix_seconds: now,
            message: "target did not reach the frozen LSN before deadline".to_string(),
        },
    };
    encode_activity_result::<WaitTargetCaughtUpActivity>(&output)
        .map(DirectEvaluation::Observe)
        .map_err(|error| format!("encode wait-target-caught-up result: {error}"))
}

#[allow(clippy::too_many_arguments)]
fn evaluate_attestation<A, FAttested, FDeadline, FConflicting>(
    snapshot: &StablePartitionSnapshotStatus,
    deadline_unix_seconds: i64,
    observations: &OperationObservations,
    now: i64,
    attested: FAttested,
    deadline: FDeadline,
    conflicting: FConflicting,
) -> Result<DirectEvaluation, String>
where
    A: DurableActivity,
    FAttested: FnOnce(i64, StablePartitionSnapshotStatus) -> A::Output,
    FDeadline: FnOnce(i64, String) -> A::Output,
    FConflicting: FnOnce(i64, String) -> A::Output,
{
    let result = match attestation_error(snapshot, observations) {
        Ok(()) => attested(
            now,
            crate::reconciler::snapshot_with_observed_metadata(snapshot.clone(), observations),
        ),
        Err(AttestationError::Unavailable(_)) if now < deadline_unix_seconds => {
            return Ok(DirectEvaluation::AwaitEvidence);
        }
        Err(AttestationError::Unavailable(message)) => deadline(now, message),
        Err(AttestationError::Conflicting(message)) => conflicting(now, message),
    };
    encode_activity_result::<A>(&result)
        .map(DirectEvaluation::Observe)
        .map_err(|error| format!("encode {} result: {error}", A::NAME))
}

enum AttestationError {
    Unavailable(String),
    Conflicting(String),
}

fn attestation_error(
    snapshot: &StablePartitionSnapshotStatus,
    observations: &OperationObservations,
) -> Result<(), AttestationError> {
    let expected_epoch = epoch(&snapshot.epoch);
    for member in &snapshot.members {
        let observed = observations.get(&member.id).ok_or_else(|| {
            AttestationError::Unavailable(format!(
                "replica {} is unavailable for topology attestation",
                member.id
            ))
        })?;
        let expected_role = role(member.role);
        let expected_label = if member.id == snapshot.primary_id {
            "primary"
        } else {
            "secondary"
        };
        if observed.status.instance_id.as_str() != member.instance_id
            || observed.status.epoch != expected_epoch
            || observed.status.role != expected_role
            || observed.pod_role_label.as_deref() != Some(expected_label)
        {
            return Err(AttestationError::Conflicting(format!(
                "replica {} conflicts with exact topology attestation",
                member.id
            )));
        }
    }
    let primary = observations.get(&snapshot.primary_id).ok_or_else(|| {
        AttestationError::Unavailable("topology primary is unavailable".to_string())
    })?;
    let expected = configuration_status(ReplicaConfigurationMode::Current, snapshot);
    if primary.status.configuration.as_ref() != Some(&expected) {
        return Err(AttestationError::Conflicting(
            "topology primary current configuration is not exact".to_string(),
        ));
    }
    Ok(())
}

fn deadline_or_wait(
    deadline_unix_seconds: i64,
    now: i64,
    result: impl FnOnce() -> Result<ExactBytes, String>,
) -> Result<DirectEvaluation, String> {
    if now < deadline_unix_seconds {
        Ok(DirectEvaluation::AwaitEvidence)
    } else {
        result().map(DirectEvaluation::Observe)
    }
}

fn configuration_relation(
    status: &kuberic_core::types::ReplicaStatusInfo,
    expected: &ReplicaConfigurationStatus,
    precondition: Option<&ReplicaConfigurationStatus>,
    allow_none_precondition: bool,
    expected_epoch: Epoch,
) -> StatusRelation {
    if status.role != Role::Primary || status.epoch != expected_epoch {
        return StatusRelation::conflicting("configuration target is not exact primary");
    }
    if status.configuration.as_ref() == Some(expected) {
        StatusRelation::Postcondition
    } else if status.configuration.as_ref() == precondition
        || (allow_none_precondition && status.configuration.is_none())
    {
        StatusRelation::Precondition
    } else {
        StatusRelation::conflicting("configuration pre/postcondition is not exact")
    }
}

fn config_for_snapshot(
    snapshot: &StablePartitionSnapshotStatus,
    observations: &OperationObservations,
) -> Result<ReplicaSetConfig, String> {
    let mut members = Vec::new();
    for member in &snapshot.members {
        if member.id == snapshot.primary_id {
            continue;
        }
        let observed = observations
            .get(&member.id)
            .ok_or_else(|| format!("replica {} is unavailable for configuration", member.id))?;
        if observed.status.instance_id.as_str() != member.instance_id {
            return Err(format!(
                "replica {} incarnation changed before configuration preparation",
                member.id
            ));
        }
        members.push(ReplicaInfo {
            id: member.id,
            instance_id: ReplicaInstanceId::new(member.instance_id.clone()),
            role: Role::ActiveSecondary,
            status: ReplicaStatus::Up,
            replicator_address: observed.replicator_address.clone(),
            current_progress: 0,
            catch_up_capability: 0,
            must_catch_up: false,
        });
    }
    members.sort_by_key(|member| member.id);
    Ok(ReplicaSetConfig {
        members,
        write_quorum: snapshot.write_quorum,
    })
}

fn configuration_status(
    mode: ReplicaConfigurationMode,
    snapshot: &StablePartitionSnapshotStatus,
) -> ReplicaConfigurationStatus {
    let mut members = snapshot
        .members
        .iter()
        .filter(|member| member.id != snapshot.primary_id)
        .map(|member| ReplicaConfigurationMemberStatus {
            id: member.id,
            instance_id: ReplicaInstanceId::new(member.instance_id.clone()),
            role: Role::ActiveSecondary,
        })
        .collect::<Vec<_>>();
    members.sort_by_key(|member| member.id);
    ReplicaConfigurationStatus {
        mode,
        members,
        write_quorum: snapshot.write_quorum,
    }
}

fn config_matches_snapshot(
    config: &ReplicaSetConfig,
    snapshot: &StablePartitionSnapshotStatus,
) -> bool {
    if config.write_quorum != snapshot.write_quorum
        || config.members.len() != snapshot.members.len().saturating_sub(1)
    {
        return false;
    }
    let mut expected = snapshot
        .members
        .iter()
        .filter(|member| member.id != snapshot.primary_id)
        .map(|member| (member.id, member.instance_id.as_str()))
        .collect::<Vec<_>>();
    expected.sort_unstable_by_key(|(id, _)| *id);
    let mut actual = config
        .members
        .iter()
        .map(|member| (member.id, member.instance_id.as_str(), member.role))
        .collect::<Vec<_>>();
    actual.sort_unstable_by_key(|(id, _, _)| *id);
    actual.len() == expected.len()
        && actual.iter().zip(expected).all(
            |((id, instance, role), (expected_id, expected_instance))| {
                *id == expected_id
                    && *instance == expected_instance
                    && *role == Role::ActiveSecondary
            },
        )
}

fn epoch(value: &EpochStatus) -> Epoch {
    Epoch::new(value.data_loss_number, value.configuration_number)
}

fn role(value: StableReplicaRoleStatus) -> Role {
    match value {
        StableReplicaRoleStatus::Primary => Role::Primary,
        StableReplicaRoleStatus::ActiveSecondary => Role::ActiveSecondary,
    }
}
