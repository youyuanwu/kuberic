#![allow(dead_code)]

use kuberic_durable_execution::DurableActivity;
use serde::{Deserialize, Serialize};

use crate::crd::{EpochStatus, StablePartitionSnapshotStatus};
use crate::durable::effects::{LabelEffectCommand, ReplicaEffectCommand};

pub const DIRECT_SWITCHOVER_CONTRACT_VERSION: u32 = 4;
pub const DIRECT_ACTIVITY_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectActivityAccounting {
    pub external_effect_count: u64,
    pub passive_observation_count: u64,
}

impl DirectActivityAccounting {
    pub const fn new(external_effect_count: u64, passive_observation_count: u64) -> Self {
        Self {
            external_effect_count,
            passive_observation_count,
        }
    }

    pub fn total(self) -> Option<u64> {
        self.external_effect_count
            .checked_add(self.passive_observation_count)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplicaOperationRequest {
    pub contract_version: u32,
    pub execution_id: String,
    pub action_id: String,
    pub sequence: u32,
    pub target_id: i64,
    pub target_instance_id: String,
    pub expected_epoch: EpochStatus,
    pub desired_snapshot: StablePartitionSnapshotStatus,
    pub deadline_unix_seconds: i64,
    pub redelivery: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_command: Option<ReplicaEffectCommand>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LabelOperationRequest {
    pub contract_version: u32,
    pub execution_id: String,
    pub action_id: String,
    pub sequence: u32,
    pub target_id: i64,
    pub target_instance_id: String,
    pub desired_role: String,
    pub deadline_unix_seconds: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_command: Option<LabelEffectCommand>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EffectObservation {
    Applied {
        observed_at_unix_seconds: i64,
    },
    ProvenNoAdmission {
        observed_at_unix_seconds: i64,
    },
    Rejected {
        observed_at_unix_seconds: i64,
        message: String,
    },
    Failed {
        observed_at_unix_seconds: i64,
        message: String,
    },
    Conflicting {
        observed_at_unix_seconds: i64,
        message: String,
    },
}

impl EffectObservation {
    pub const fn observed_at_unix_seconds(&self) -> i64 {
        match self {
            Self::Applied {
                observed_at_unix_seconds,
            }
            | Self::ProvenNoAdmission {
                observed_at_unix_seconds,
            }
            | Self::Rejected {
                observed_at_unix_seconds,
                ..
            }
            | Self::Failed {
                observed_at_unix_seconds,
                ..
            }
            | Self::Conflicting {
                observed_at_unix_seconds,
                ..
            } => *observed_at_unix_seconds,
        }
    }
}

pub trait ReplicaDirectActivity: DurableActivity {
    fn input(request: ReplicaOperationRequest) -> Self::Input;
    fn request(input: Self::Input) -> ReplicaOperationRequest;
    fn observation(output: Self::Output) -> EffectObservation;
    fn output(observation: EffectObservation) -> Result<Self::Output, String>;
}

pub trait LabelDirectActivity: DurableActivity {
    fn input(request: LabelOperationRequest) -> Self::Input;
    fn request(input: Self::Input) -> LabelOperationRequest;
    fn observation(output: Self::Output) -> EffectObservation;
    fn output(observation: EffectObservation) -> Result<Self::Output, String>;
}

macro_rules! define_replica_activity {
    ($activity:ident, $input:ident, $output:ident, $name:literal, $max_input:literal, $max_result:literal) => {
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub struct $input {
            pub contract_version: u32,
            pub execution_id: String,
            pub action_id: String,
            pub sequence: u32,
            pub target_id: i64,
            pub target_instance_id: String,
            pub expected_epoch: EpochStatus,
            pub desired_snapshot: StablePartitionSnapshotStatus,
            pub deadline_unix_seconds: i64,
            pub redelivery: u8,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub prepared_command: Option<ReplicaEffectCommand>,
        }

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
        pub enum $output {
            Applied {
                observed_at_unix_seconds: i64,
            },
            ProvenNoAdmission {
                observed_at_unix_seconds: i64,
            },
            Rejected {
                observed_at_unix_seconds: i64,
                message: String,
            },
            Failed {
                observed_at_unix_seconds: i64,
                message: String,
            },
            Conflicting {
                observed_at_unix_seconds: i64,
                message: String,
            },
        }

        pub struct $activity;

        impl DurableActivity for $activity {
            type Input = $input;
            type Output = $output;

            const NAME: &'static str = $name;
            const VERSION: u32 = DIRECT_ACTIVITY_VERSION;
            const MAX_INPUT_BYTES: u64 = $max_input;
            const MAX_RESULT_BYTES: u64 = $max_result;
        }

        impl ReplicaDirectActivity for $activity {
            fn input(request: ReplicaOperationRequest) -> Self::Input {
                $input {
                    contract_version: request.contract_version,
                    execution_id: request.execution_id,
                    action_id: request.action_id,
                    sequence: request.sequence,
                    target_id: request.target_id,
                    target_instance_id: request.target_instance_id,
                    expected_epoch: request.expected_epoch,
                    desired_snapshot: request.desired_snapshot,
                    deadline_unix_seconds: request.deadline_unix_seconds,
                    redelivery: request.redelivery,
                    prepared_command: request.prepared_command,
                }
            }

            fn request(input: Self::Input) -> ReplicaOperationRequest {
                ReplicaOperationRequest {
                    contract_version: input.contract_version,
                    execution_id: input.execution_id,
                    action_id: input.action_id,
                    sequence: input.sequence,
                    target_id: input.target_id,
                    target_instance_id: input.target_instance_id,
                    expected_epoch: input.expected_epoch,
                    desired_snapshot: input.desired_snapshot,
                    deadline_unix_seconds: input.deadline_unix_seconds,
                    redelivery: input.redelivery,
                    prepared_command: input.prepared_command,
                }
            }

            fn observation(output: Self::Output) -> EffectObservation {
                match output {
                    $output::Applied {
                        observed_at_unix_seconds,
                    } => EffectObservation::Applied {
                        observed_at_unix_seconds,
                    },
                    $output::ProvenNoAdmission {
                        observed_at_unix_seconds,
                    } => EffectObservation::ProvenNoAdmission {
                        observed_at_unix_seconds,
                    },
                    $output::Rejected {
                        observed_at_unix_seconds,
                        message,
                    } => EffectObservation::Rejected {
                        observed_at_unix_seconds,
                        message,
                    },
                    $output::Failed {
                        observed_at_unix_seconds,
                        message,
                    } => EffectObservation::Failed {
                        observed_at_unix_seconds,
                        message,
                    },
                    $output::Conflicting {
                        observed_at_unix_seconds,
                        message,
                    } => EffectObservation::Conflicting {
                        observed_at_unix_seconds,
                        message,
                    },
                }
            }

            fn output(observation: EffectObservation) -> Result<Self::Output, String> {
                Ok(match observation {
                    EffectObservation::Applied {
                        observed_at_unix_seconds,
                    } => $output::Applied {
                        observed_at_unix_seconds,
                    },
                    EffectObservation::ProvenNoAdmission {
                        observed_at_unix_seconds,
                    } => $output::ProvenNoAdmission {
                        observed_at_unix_seconds,
                    },
                    EffectObservation::Rejected {
                        observed_at_unix_seconds,
                        message,
                    } => $output::Rejected {
                        observed_at_unix_seconds,
                        message,
                    },
                    EffectObservation::Failed {
                        observed_at_unix_seconds,
                        message,
                    } => $output::Failed {
                        observed_at_unix_seconds,
                        message,
                    },
                    EffectObservation::Conflicting {
                        observed_at_unix_seconds,
                        message,
                    } => $output::Conflicting {
                        observed_at_unix_seconds,
                        message,
                    },
                })
            }
        }
    };
}

macro_rules! define_label_activity {
    ($activity:ident, $input:ident, $output:ident, $name:literal, $max_input:literal, $max_result:literal) => {
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub struct $input {
            pub contract_version: u32,
            pub execution_id: String,
            pub action_id: String,
            pub sequence: u32,
            pub target_id: i64,
            pub target_instance_id: String,
            pub desired_role: String,
            pub deadline_unix_seconds: i64,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub prepared_command: Option<LabelEffectCommand>,
        }

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
        pub enum $output {
            Applied {
                observed_at_unix_seconds: i64,
            },
            Failed {
                observed_at_unix_seconds: i64,
                message: String,
            },
            Conflicting {
                observed_at_unix_seconds: i64,
                message: String,
            },
        }

        pub struct $activity;

        impl DurableActivity for $activity {
            type Input = $input;
            type Output = $output;

            const NAME: &'static str = $name;
            const VERSION: u32 = DIRECT_ACTIVITY_VERSION;
            const MAX_INPUT_BYTES: u64 = $max_input;
            const MAX_RESULT_BYTES: u64 = $max_result;
        }

        impl LabelDirectActivity for $activity {
            fn input(request: LabelOperationRequest) -> Self::Input {
                $input {
                    contract_version: request.contract_version,
                    execution_id: request.execution_id,
                    action_id: request.action_id,
                    sequence: request.sequence,
                    target_id: request.target_id,
                    target_instance_id: request.target_instance_id,
                    desired_role: request.desired_role,
                    deadline_unix_seconds: request.deadline_unix_seconds,
                    prepared_command: request.prepared_command,
                }
            }

            fn request(input: Self::Input) -> LabelOperationRequest {
                LabelOperationRequest {
                    contract_version: input.contract_version,
                    execution_id: input.execution_id,
                    action_id: input.action_id,
                    sequence: input.sequence,
                    target_id: input.target_id,
                    target_instance_id: input.target_instance_id,
                    desired_role: input.desired_role,
                    deadline_unix_seconds: input.deadline_unix_seconds,
                    prepared_command: input.prepared_command,
                }
            }

            fn observation(output: Self::Output) -> EffectObservation {
                match output {
                    $output::Applied {
                        observed_at_unix_seconds,
                    } => EffectObservation::Applied {
                        observed_at_unix_seconds,
                    },
                    $output::Failed {
                        observed_at_unix_seconds,
                        message,
                    } => EffectObservation::Failed {
                        observed_at_unix_seconds,
                        message,
                    },
                    $output::Conflicting {
                        observed_at_unix_seconds,
                        message,
                    } => EffectObservation::Conflicting {
                        observed_at_unix_seconds,
                        message,
                    },
                }
            }

            fn output(observation: EffectObservation) -> Result<Self::Output, String> {
                Ok(match observation {
                    EffectObservation::Applied {
                        observed_at_unix_seconds,
                    } => $output::Applied {
                        observed_at_unix_seconds,
                    },
                    EffectObservation::Failed {
                        observed_at_unix_seconds,
                        message,
                    } => $output::Failed {
                        observed_at_unix_seconds,
                        message,
                    },
                    EffectObservation::Conflicting {
                        observed_at_unix_seconds,
                        message,
                    } => $output::Conflicting {
                        observed_at_unix_seconds,
                        message,
                    },
                    EffectObservation::ProvenNoAdmission { .. }
                    | EffectObservation::Rejected { .. } => {
                        return Err(
                            "label activities cannot report replica-only outcomes".to_string()
                        );
                    }
                })
            }
        }
    };
}

define_replica_activity!(
    RevokeWritesActivity,
    RevokeWritesInput,
    RevokeWritesOutput,
    "kuberic.switchover.revoke-writes",
    6_144,
    1_024
);
define_replica_activity!(
    DemoteOldPrimaryActivity,
    DemoteOldPrimaryInput,
    DemoteOldPrimaryOutput,
    "kuberic.switchover.demote-old-primary",
    6_144,
    1_024
);
define_replica_activity!(
    PromoteTargetActivity,
    PromoteTargetInput,
    PromoteTargetOutput,
    "kuberic.switchover.promote-target",
    6_144,
    1_024
);
define_replica_activity!(
    DistributeReplicaEpochActivity,
    DistributeReplicaEpochInput,
    DistributeReplicaEpochOutput,
    "kuberic.switchover.distribute-replica-epoch",
    6_144,
    1_024
);
define_replica_activity!(
    InstallTargetCatchUpConfigurationActivity,
    InstallTargetCatchUpConfigurationInput,
    InstallTargetCatchUpConfigurationOutput,
    "kuberic.switchover.install-target-catch-up-configuration",
    8_192,
    1_024
);
define_replica_activity!(
    WaitTargetWriteQuorumActivity,
    WaitTargetWriteQuorumInput,
    WaitTargetWriteQuorumOutput,
    "kuberic.switchover.wait-target-write-quorum",
    6_144,
    1_024
);
define_replica_activity!(
    InstallTargetCurrentConfigurationActivity,
    InstallTargetCurrentConfigurationInput,
    InstallTargetCurrentConfigurationOutput,
    "kuberic.switchover.install-target-current-configuration",
    8_192,
    1_024
);
define_replica_activity!(
    RestorePreviousCurrentConfigurationActivity,
    RestorePreviousCurrentConfigurationInput,
    RestorePreviousCurrentConfigurationOutput,
    "kuberic.switchover.restore-previous-current-configuration",
    8_192,
    1_024
);
define_replica_activity!(
    CompensatePromoteOldPrimaryActivity,
    CompensatePromoteOldPrimaryInput,
    CompensatePromoteOldPrimaryOutput,
    "kuberic.switchover.compensate-promote-old-primary",
    6_144,
    1_024
);
define_replica_activity!(
    CompensateDistributeReplicaEpochActivity,
    CompensateDistributeReplicaEpochInput,
    CompensateDistributeReplicaEpochOutput,
    "kuberic.switchover.compensate-distribute-replica-epoch",
    6_144,
    1_024
);
define_replica_activity!(
    InstallCompensationCatchUpConfigurationActivity,
    InstallCompensationCatchUpConfigurationInput,
    InstallCompensationCatchUpConfigurationOutput,
    "kuberic.switchover.install-compensation-catch-up-configuration",
    8_192,
    1_024
);
define_replica_activity!(
    InstallCompensationCurrentConfigurationActivity,
    InstallCompensationCurrentConfigurationInput,
    InstallCompensationCurrentConfigurationOutput,
    "kuberic.switchover.install-compensation-current-configuration",
    8_192,
    1_024
);

define_label_activity!(
    PublishTargetPrimaryLabelActivity,
    PublishTargetPrimaryLabelInput,
    PublishTargetPrimaryLabelOutput,
    "kuberic.switchover.publish-target-primary-label",
    4_096,
    768
);
define_label_activity!(
    PublishOldPrimarySecondaryLabelActivity,
    PublishOldPrimarySecondaryLabelInput,
    PublishOldPrimarySecondaryLabelOutput,
    "kuberic.switchover.publish-old-primary-secondary-label",
    4_096,
    768
);
define_label_activity!(
    RestoreOldPrimaryLabelActivity,
    RestoreOldPrimaryLabelInput,
    RestoreOldPrimaryLabelOutput,
    "kuberic.switchover.restore-old-primary-label",
    4_096,
    768
);
define_label_activity!(
    RestoreTargetSecondaryLabelActivity,
    RestoreTargetSecondaryLabelInput,
    RestoreTargetSecondaryLabelOutput,
    "kuberic.switchover.restore-target-secondary-label",
    4_096,
    768
);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CaptureFrozenLsnInput {
    pub contract_version: u32,
    pub execution_id: String,
    pub old_primary_id: i64,
    pub old_primary_instance_id: String,
    pub expected_epoch: EpochStatus,
    pub deadline_unix_seconds: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureFrozenLsnOutput {
    Captured {
        frozen_lsn: i64,
        observed_at_unix_seconds: i64,
    },
    DeadlineExceeded {
        observed_at_unix_seconds: i64,
        message: String,
    },
    Conflicting {
        observed_at_unix_seconds: i64,
        message: String,
    },
}

pub struct CaptureFrozenLsnActivity;

impl DurableActivity for CaptureFrozenLsnActivity {
    type Input = CaptureFrozenLsnInput;
    type Output = CaptureFrozenLsnOutput;

    const NAME: &'static str = "kuberic.switchover.capture-frozen-lsn";
    const VERSION: u32 = DIRECT_ACTIVITY_VERSION;
    const MAX_INPUT_BYTES: u64 = 2_048;
    const MAX_RESULT_BYTES: u64 = 1_024;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WaitTargetCaughtUpInput {
    pub contract_version: u32,
    pub execution_id: String,
    pub target_id: i64,
    pub target_instance_id: String,
    pub expected_epoch: EpochStatus,
    pub frozen_lsn: i64,
    pub deadline_unix_seconds: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitTargetCaughtUpOutput {
    CaughtUp {
        observed_at_unix_seconds: i64,
    },
    DeadlineExceeded {
        observed_at_unix_seconds: i64,
        message: String,
    },
    Conflicting {
        observed_at_unix_seconds: i64,
        message: String,
    },
}

pub struct WaitTargetCaughtUpActivity;

impl DurableActivity for WaitTargetCaughtUpActivity {
    type Input = WaitTargetCaughtUpInput;
    type Output = WaitTargetCaughtUpOutput;

    const NAME: &'static str = "kuberic.switchover.wait-target-caught-up";
    const VERSION: u32 = DIRECT_ACTIVITY_VERSION;
    const MAX_INPUT_BYTES: u64 = 2_048;
    const MAX_RESULT_BYTES: u64 = 1_024;
}

macro_rules! define_attestation_activity {
    ($activity:ident, $input:ident, $output:ident, $name:literal, $max_input:literal, $max_result:literal) => {
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub struct $input {
            pub contract_version: u32,
            pub execution_id: String,
            pub expected_snapshot: StablePartitionSnapshotStatus,
            pub deadline_unix_seconds: i64,
        }

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
        pub enum $output {
            Attested {
                observed_at_unix_seconds: i64,
                #[serde(default, skip_serializing_if = "Option::is_none")]
                accounting: Option<DirectActivityAccounting>,
            },
            DeadlineExceeded {
                observed_at_unix_seconds: i64,
                message: String,
            },
            Conflicting {
                observed_at_unix_seconds: i64,
                message: String,
            },
        }

        pub struct $activity;

        impl DurableActivity for $activity {
            type Input = $input;
            type Output = $output;

            const NAME: &'static str = $name;
            const VERSION: u32 = DIRECT_ACTIVITY_VERSION;
            const MAX_INPUT_BYTES: u64 = $max_input;
            const MAX_RESULT_BYTES: u64 = $max_result;
        }
    };
}

define_attestation_activity!(
    AttestTargetTopologyActivity,
    AttestTargetTopologyInput,
    AttestTargetTopologyOutput,
    "kuberic.switchover.attest-target-topology",
    8_192,
    1_024
);
define_attestation_activity!(
    AttestCompensatedTopologyActivity,
    AttestCompensatedTopologyInput,
    AttestCompensatedTopologyOutput,
    "kuberic.switchover.attest-compensated-topology",
    8_192,
    1_024
);

pub const ALL_DIRECT_ACTIVITY_IDENTITIES: &[(&str, u32)] = &[
    (RevokeWritesActivity::NAME, RevokeWritesActivity::VERSION),
    (
        CaptureFrozenLsnActivity::NAME,
        CaptureFrozenLsnActivity::VERSION,
    ),
    (
        WaitTargetCaughtUpActivity::NAME,
        WaitTargetCaughtUpActivity::VERSION,
    ),
    (
        DemoteOldPrimaryActivity::NAME,
        DemoteOldPrimaryActivity::VERSION,
    ),
    (PromoteTargetActivity::NAME, PromoteTargetActivity::VERSION),
    (
        DistributeReplicaEpochActivity::NAME,
        DistributeReplicaEpochActivity::VERSION,
    ),
    (
        InstallTargetCatchUpConfigurationActivity::NAME,
        InstallTargetCatchUpConfigurationActivity::VERSION,
    ),
    (
        WaitTargetWriteQuorumActivity::NAME,
        WaitTargetWriteQuorumActivity::VERSION,
    ),
    (
        InstallTargetCurrentConfigurationActivity::NAME,
        InstallTargetCurrentConfigurationActivity::VERSION,
    ),
    (
        PublishTargetPrimaryLabelActivity::NAME,
        PublishTargetPrimaryLabelActivity::VERSION,
    ),
    (
        PublishOldPrimarySecondaryLabelActivity::NAME,
        PublishOldPrimarySecondaryLabelActivity::VERSION,
    ),
    (
        AttestTargetTopologyActivity::NAME,
        AttestTargetTopologyActivity::VERSION,
    ),
    (
        RestorePreviousCurrentConfigurationActivity::NAME,
        RestorePreviousCurrentConfigurationActivity::VERSION,
    ),
    (
        CompensatePromoteOldPrimaryActivity::NAME,
        CompensatePromoteOldPrimaryActivity::VERSION,
    ),
    (
        CompensateDistributeReplicaEpochActivity::NAME,
        CompensateDistributeReplicaEpochActivity::VERSION,
    ),
    (
        InstallCompensationCatchUpConfigurationActivity::NAME,
        InstallCompensationCatchUpConfigurationActivity::VERSION,
    ),
    (
        InstallCompensationCurrentConfigurationActivity::NAME,
        InstallCompensationCurrentConfigurationActivity::VERSION,
    ),
    (
        RestoreOldPrimaryLabelActivity::NAME,
        RestoreOldPrimaryLabelActivity::VERSION,
    ),
    (
        RestoreTargetSecondaryLabelActivity::NAME,
        RestoreTargetSecondaryLabelActivity::VERSION,
    ),
    (
        AttestCompensatedTopologyActivity::NAME,
        AttestCompensatedTopologyActivity::VERSION,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{StableReplicaRoleStatus, StableReplicaSnapshotStatus};
    use kuberic_durable_execution::{
        ActivityCallError, decode_activity_input, decode_activity_result, encode_activity_input,
        encode_activity_result,
    };

    fn snapshot() -> StablePartitionSnapshotStatus {
        StablePartitionSnapshotStatus {
            epoch: EpochStatus {
                data_loss_number: 1,
                configuration_number: 2,
            },
            primary_id: 1,
            members: vec![
                StableReplicaSnapshotStatus {
                    id: 1,
                    instance_id: "instance-1".to_string(),
                    role: StableReplicaRoleStatus::Primary,
                    election_metadata: None,
                },
                StableReplicaSnapshotStatus {
                    id: 2,
                    instance_id: "instance-2".to_string(),
                    role: StableReplicaRoleStatus::ActiveSecondary,
                    election_metadata: None,
                },
            ],
            write_quorum: 2,
        }
    }

    fn replica_request() -> ReplicaOperationRequest {
        ReplicaOperationRequest {
            contract_version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
            execution_id: "execution".to_string(),
            action_id: "execution:1".to_string(),
            sequence: 1,
            target_id: 1,
            target_instance_id: "instance-1".to_string(),
            expected_epoch: snapshot().epoch,
            desired_snapshot: snapshot(),
            deadline_unix_seconds: 10,
            redelivery: 0,
            prepared_command: None,
        }
    }

    fn label_request() -> LabelOperationRequest {
        LabelOperationRequest {
            contract_version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
            execution_id: "execution".to_string(),
            action_id: "execution:1003".to_string(),
            sequence: 1003,
            target_id: 2,
            target_instance_id: "instance-2".to_string(),
            desired_role: "primary".to_string(),
            deadline_unix_seconds: 10,
            prepared_command: None,
        }
    }

    fn assert_exact_input_bound<A, F>(make: F)
    where
        A: DurableActivity,
        F: Fn(usize) -> A::Input,
    {
        let base = encode_activity_input::<A>(&make(0)).unwrap();
        let maximum = usize::try_from(A::MAX_INPUT_BYTES).unwrap();
        assert!(base.as_slice().len() <= maximum, "{}", A::NAME);
        let padding = maximum - base.as_slice().len();
        let exact = encode_activity_input::<A>(&make(padding)).unwrap();
        assert_eq!(exact.as_slice().len(), maximum, "{}", A::NAME);
        assert!(decode_activity_input::<A>(&exact).is_ok(), "{}", A::NAME);
        assert!(
            matches!(
                encode_activity_input::<A>(&make(padding + 1)),
                Err(ActivityCallError::InputTooLarge {
                    actual_bytes,
                    max_bytes,
                }) if actual_bytes == max_bytes + 1 && max_bytes == A::MAX_INPUT_BYTES
            ),
            "{}",
            A::NAME
        );
    }

    fn assert_exact_result_bound<A, F>(make: F)
    where
        A: DurableActivity,
        F: Fn(usize) -> A::Output,
    {
        let base = encode_activity_result::<A>(&make(0)).unwrap();
        let maximum = usize::try_from(A::MAX_RESULT_BYTES).unwrap();
        assert!(base.as_slice().len() <= maximum, "{}", A::NAME);
        let padding = maximum - base.as_slice().len();
        let exact = encode_activity_result::<A>(&make(padding)).unwrap();
        assert_eq!(exact.as_slice().len(), maximum, "{}", A::NAME);
        assert!(decode_activity_result::<A>(&exact).is_ok(), "{}", A::NAME);
        assert!(
            matches!(
                encode_activity_result::<A>(&make(padding + 1)),
                Err(ActivityCallError::ResultTooLarge {
                    actual_bytes,
                    max_bytes,
                }) if actual_bytes == max_bytes + 1 && max_bytes == A::MAX_RESULT_BYTES
            ),
            "{}",
            A::NAME
        );
    }

    fn assert_replica_activity_exact_bounds<A: ReplicaDirectActivity>() {
        assert_exact_input_bound::<A, _>(|padding| {
            let mut request = replica_request();
            request.execution_id = "x".repeat(padding);
            A::input(request)
        });
        assert_exact_result_bound::<A, _>(|padding| {
            A::output(EffectObservation::Failed {
                observed_at_unix_seconds: 1,
                message: "x".repeat(padding),
            })
            .unwrap()
        });
    }

    fn assert_label_activity_exact_bounds<A: LabelDirectActivity>() {
        assert_exact_input_bound::<A, _>(|padding| {
            let mut request = label_request();
            request.execution_id = "x".repeat(padding);
            A::input(request)
        });
        assert_exact_result_bound::<A, _>(|padding| {
            A::output(EffectObservation::Failed {
                observed_at_unix_seconds: 1,
                message: "x".repeat(padding),
            })
            .unwrap()
        });
    }

    #[test]
    fn direct_switchover_activity_identities_are_unique_positive_and_operation_specific() {
        let identities = ALL_DIRECT_ACTIVITY_IDENTITIES
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(identities.len(), ALL_DIRECT_ACTIVITY_IDENTITIES.len());
        assert_eq!(identities.len(), 20);
        for (name, version) in identities {
            assert!(name.starts_with("kuberic.switchover."));
            assert_ne!(name, "kuberic.switchover.native-boundary");
            assert_eq!(version, 1);
        }
    }

    #[test]
    fn every_direct_switchover_activity_declares_independent_nonzero_bounds() {
        macro_rules! assert_bounds {
            ($($activity:ty),+ $(,)?) => {
                $(
                    assert!(<$activity>::MAX_INPUT_BYTES > 0);
                    assert!(<$activity>::MAX_RESULT_BYTES > 0);
                )+
            };
        }
        assert_bounds!(
            RevokeWritesActivity,
            CaptureFrozenLsnActivity,
            WaitTargetCaughtUpActivity,
            DemoteOldPrimaryActivity,
            PromoteTargetActivity,
            DistributeReplicaEpochActivity,
            InstallTargetCatchUpConfigurationActivity,
            WaitTargetWriteQuorumActivity,
            InstallTargetCurrentConfigurationActivity,
            PublishTargetPrimaryLabelActivity,
            PublishOldPrimarySecondaryLabelActivity,
            AttestTargetTopologyActivity,
            RestorePreviousCurrentConfigurationActivity,
            CompensatePromoteOldPrimaryActivity,
            CompensateDistributeReplicaEpochActivity,
            InstallCompensationCatchUpConfigurationActivity,
            InstallCompensationCurrentConfigurationActivity,
            RestoreOldPrimaryLabelActivity,
            RestoreTargetSecondaryLabelActivity,
            AttestCompensatedTopologyActivity,
        );
        assert_ne!(
            RevokeWritesActivity::MAX_INPUT_BYTES,
            InstallTargetCurrentConfigurationActivity::MAX_INPUT_BYTES
        );
        assert_ne!(
            PublishTargetPrimaryLabelActivity::MAX_RESULT_BYTES,
            RevokeWritesActivity::MAX_RESULT_BYTES
        );
    }

    #[test]
    fn direct_switchover_contracts_reject_unknown_fields() {
        let mut value =
            serde_json::to_value(RevokeWritesActivity::input(replica_request())).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unknown".to_string(), serde_json::Value::Bool(true));
        assert!(serde_json::from_value::<RevokeWritesInput>(value).is_err());

        let malformed = serde_json::json!({
            "result": "applied",
            "observed_at_unix_seconds": 1,
            "unknown": true,
        });
        assert!(serde_json::from_value::<RevokeWritesOutput>(malformed).is_err());
    }

    #[test]
    fn direct_switchover_effect_outputs_preserve_domain_outcomes() {
        let outcomes = [
            RevokeWritesOutput::Applied {
                observed_at_unix_seconds: 1,
            },
            RevokeWritesOutput::ProvenNoAdmission {
                observed_at_unix_seconds: 2,
            },
            RevokeWritesOutput::Rejected {
                observed_at_unix_seconds: 3,
                message: "rejected".to_string(),
            },
            RevokeWritesOutput::Failed {
                observed_at_unix_seconds: 4,
                message: "failed".to_string(),
            },
            RevokeWritesOutput::Conflicting {
                observed_at_unix_seconds: 5,
                message: "conflicting".to_string(),
            },
        ];
        for outcome in outcomes {
            let encoded = serde_json::to_vec(&outcome).unwrap();
            let decoded = serde_json::from_slice::<RevokeWritesOutput>(&encoded).unwrap();
            assert_eq!(decoded, outcome);
        }
    }

    #[test]
    fn direct_switchover_every_activity_accepts_exact_bounds_and_rejects_one_over() {
        assert_replica_activity_exact_bounds::<RevokeWritesActivity>();
        assert_replica_activity_exact_bounds::<DemoteOldPrimaryActivity>();
        assert_replica_activity_exact_bounds::<PromoteTargetActivity>();
        assert_replica_activity_exact_bounds::<DistributeReplicaEpochActivity>();
        assert_replica_activity_exact_bounds::<InstallTargetCatchUpConfigurationActivity>();
        assert_replica_activity_exact_bounds::<WaitTargetWriteQuorumActivity>();
        assert_replica_activity_exact_bounds::<InstallTargetCurrentConfigurationActivity>();
        assert_replica_activity_exact_bounds::<RestorePreviousCurrentConfigurationActivity>();
        assert_replica_activity_exact_bounds::<CompensatePromoteOldPrimaryActivity>();
        assert_replica_activity_exact_bounds::<CompensateDistributeReplicaEpochActivity>();
        assert_replica_activity_exact_bounds::<InstallCompensationCatchUpConfigurationActivity>();
        assert_replica_activity_exact_bounds::<InstallCompensationCurrentConfigurationActivity>();

        assert_label_activity_exact_bounds::<PublishTargetPrimaryLabelActivity>();
        assert_label_activity_exact_bounds::<PublishOldPrimarySecondaryLabelActivity>();
        assert_label_activity_exact_bounds::<RestoreOldPrimaryLabelActivity>();
        assert_label_activity_exact_bounds::<RestoreTargetSecondaryLabelActivity>();

        assert_exact_input_bound::<CaptureFrozenLsnActivity, _>(|padding| CaptureFrozenLsnInput {
            contract_version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
            execution_id: "x".repeat(padding),
            old_primary_id: 1,
            old_primary_instance_id: "instance-1".to_string(),
            expected_epoch: snapshot().epoch,
            deadline_unix_seconds: 10,
        });
        assert_exact_result_bound::<CaptureFrozenLsnActivity, _>(|padding| {
            CaptureFrozenLsnOutput::Conflicting {
                observed_at_unix_seconds: 1,
                message: "x".repeat(padding),
            }
        });

        assert_exact_input_bound::<WaitTargetCaughtUpActivity, _>(|padding| {
            WaitTargetCaughtUpInput {
                contract_version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
                execution_id: "x".repeat(padding),
                target_id: 2,
                target_instance_id: "instance-2".to_string(),
                expected_epoch: snapshot().epoch,
                frozen_lsn: 10,
                deadline_unix_seconds: 10,
            }
        });
        assert_exact_result_bound::<WaitTargetCaughtUpActivity, _>(|padding| {
            WaitTargetCaughtUpOutput::Conflicting {
                observed_at_unix_seconds: 1,
                message: "x".repeat(padding),
            }
        });

        assert_exact_input_bound::<AttestTargetTopologyActivity, _>(|padding| {
            AttestTargetTopologyInput {
                contract_version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
                execution_id: "x".repeat(padding),
                expected_snapshot: snapshot(),
                deadline_unix_seconds: 10,
            }
        });
        assert_exact_result_bound::<AttestTargetTopologyActivity, _>(|padding| {
            AttestTargetTopologyOutput::Conflicting {
                observed_at_unix_seconds: 1,
                message: "x".repeat(padding),
            }
        });

        assert_exact_input_bound::<AttestCompensatedTopologyActivity, _>(|padding| {
            AttestCompensatedTopologyInput {
                contract_version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
                execution_id: "x".repeat(padding),
                expected_snapshot: snapshot(),
                deadline_unix_seconds: 10,
            }
        });
        assert_exact_result_bound::<AttestCompensatedTopologyActivity, _>(|padding| {
            AttestCompensatedTopologyOutput::Conflicting {
                observed_at_unix_seconds: 1,
                message: "x".repeat(padding),
            }
        });
    }
}
