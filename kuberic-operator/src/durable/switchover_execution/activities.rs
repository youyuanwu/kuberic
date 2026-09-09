#![allow(dead_code)]

use kuberic_durable_execution::DurableActivity;
use serde::{Deserialize, Serialize};

use crate::crd::{EpochStatus, StablePartitionSnapshotStatus};
use crate::durable::effects::{LabelEffectCommand, ReplicaEffectCommand};

pub const DIRECT_SWITCHOVER_CONTRACT_VERSION: u32 = 4;
pub const DIRECT_ACTIVITY_VERSION: u32 = 1;
pub const DIRECT_REPLICA_INPUT_BYTES: u64 = 8_192;
pub const DIRECT_LABEL_INPUT_BYTES: u64 = 4_096;
pub const DIRECT_OBSERVATION_INPUT_BYTES: u64 = 4_096;
pub const DIRECT_ATTESTATION_INPUT_BYTES: u64 = 8_192;
pub const DIRECT_EFFECT_RESULT_BYTES: u64 = 1_024;
pub const DIRECT_OBSERVATION_RESULT_BYTES: u64 = 2_048;

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
    fn observation(output: Self::Output) -> EffectObservation;
}

pub trait LabelDirectActivity: DurableActivity {
    fn input(request: LabelOperationRequest) -> Self::Input;
    fn observation(output: Self::Output) -> EffectObservation;
}

macro_rules! define_replica_activity {
    ($activity:ident, $input:ident, $output:ident, $name:literal) => {
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $input(pub ReplicaOperationRequest);

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
            const MAX_INPUT_BYTES: u64 = DIRECT_REPLICA_INPUT_BYTES;
            const MAX_RESULT_BYTES: u64 = DIRECT_EFFECT_RESULT_BYTES;
        }

        impl ReplicaDirectActivity for $activity {
            fn input(request: ReplicaOperationRequest) -> Self::Input {
                $input(request)
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
        }
    };
}

macro_rules! define_label_activity {
    ($activity:ident, $input:ident, $output:ident, $name:literal) => {
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $input(pub LabelOperationRequest);

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
            const MAX_INPUT_BYTES: u64 = DIRECT_LABEL_INPUT_BYTES;
            const MAX_RESULT_BYTES: u64 = DIRECT_EFFECT_RESULT_BYTES;
        }

        impl LabelDirectActivity for $activity {
            fn input(request: LabelOperationRequest) -> Self::Input {
                $input(request)
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
        }
    };
}

define_replica_activity!(
    RevokeWritesActivity,
    RevokeWritesInput,
    RevokeWritesOutput,
    "kuberic.switchover.revoke-writes"
);
define_replica_activity!(
    DemoteOldPrimaryActivity,
    DemoteOldPrimaryInput,
    DemoteOldPrimaryOutput,
    "kuberic.switchover.demote-old-primary"
);
define_replica_activity!(
    PromoteTargetActivity,
    PromoteTargetInput,
    PromoteTargetOutput,
    "kuberic.switchover.promote-target"
);
define_replica_activity!(
    DistributeReplicaEpochActivity,
    DistributeReplicaEpochInput,
    DistributeReplicaEpochOutput,
    "kuberic.switchover.distribute-replica-epoch"
);
define_replica_activity!(
    InstallTargetCatchUpConfigurationActivity,
    InstallTargetCatchUpConfigurationInput,
    InstallTargetCatchUpConfigurationOutput,
    "kuberic.switchover.install-target-catch-up-configuration"
);
define_replica_activity!(
    WaitTargetWriteQuorumActivity,
    WaitTargetWriteQuorumInput,
    WaitTargetWriteQuorumOutput,
    "kuberic.switchover.wait-target-write-quorum"
);
define_replica_activity!(
    InstallTargetCurrentConfigurationActivity,
    InstallTargetCurrentConfigurationInput,
    InstallTargetCurrentConfigurationOutput,
    "kuberic.switchover.install-target-current-configuration"
);
define_replica_activity!(
    RestorePreviousCurrentConfigurationActivity,
    RestorePreviousCurrentConfigurationInput,
    RestorePreviousCurrentConfigurationOutput,
    "kuberic.switchover.restore-previous-current-configuration"
);
define_replica_activity!(
    CompensatePromoteOldPrimaryActivity,
    CompensatePromoteOldPrimaryInput,
    CompensatePromoteOldPrimaryOutput,
    "kuberic.switchover.compensate-promote-old-primary"
);
define_replica_activity!(
    CompensateDistributeReplicaEpochActivity,
    CompensateDistributeReplicaEpochInput,
    CompensateDistributeReplicaEpochOutput,
    "kuberic.switchover.compensate-distribute-replica-epoch"
);
define_replica_activity!(
    InstallCompensationCatchUpConfigurationActivity,
    InstallCompensationCatchUpConfigurationInput,
    InstallCompensationCatchUpConfigurationOutput,
    "kuberic.switchover.install-compensation-catch-up-configuration"
);
define_replica_activity!(
    InstallCompensationCurrentConfigurationActivity,
    InstallCompensationCurrentConfigurationInput,
    InstallCompensationCurrentConfigurationOutput,
    "kuberic.switchover.install-compensation-current-configuration"
);

define_label_activity!(
    PublishTargetPrimaryLabelActivity,
    PublishTargetPrimaryLabelInput,
    PublishTargetPrimaryLabelOutput,
    "kuberic.switchover.publish-target-primary-label"
);
define_label_activity!(
    PublishOldPrimarySecondaryLabelActivity,
    PublishOldPrimarySecondaryLabelInput,
    PublishOldPrimarySecondaryLabelOutput,
    "kuberic.switchover.publish-old-primary-secondary-label"
);
define_label_activity!(
    RestoreOldPrimaryLabelActivity,
    RestoreOldPrimaryLabelInput,
    RestoreOldPrimaryLabelOutput,
    "kuberic.switchover.restore-old-primary-label"
);
define_label_activity!(
    RestoreTargetSecondaryLabelActivity,
    RestoreTargetSecondaryLabelInput,
    RestoreTargetSecondaryLabelOutput,
    "kuberic.switchover.restore-target-secondary-label"
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
    const MAX_INPUT_BYTES: u64 = DIRECT_OBSERVATION_INPUT_BYTES;
    const MAX_RESULT_BYTES: u64 = DIRECT_OBSERVATION_RESULT_BYTES;
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
    const MAX_INPUT_BYTES: u64 = DIRECT_OBSERVATION_INPUT_BYTES;
    const MAX_RESULT_BYTES: u64 = DIRECT_OBSERVATION_RESULT_BYTES;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttestTopologyInput {
    pub contract_version: u32,
    pub execution_id: String,
    pub expected_snapshot: StablePartitionSnapshotStatus,
    pub deadline_unix_seconds: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttestTopologyOutput {
    Attested {
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

macro_rules! define_attestation_activity {
    ($activity:ident, $name:literal) => {
        pub struct $activity;

        impl DurableActivity for $activity {
            type Input = AttestTopologyInput;
            type Output = AttestTopologyOutput;

            const NAME: &'static str = $name;
            const VERSION: u32 = DIRECT_ACTIVITY_VERSION;
            const MAX_INPUT_BYTES: u64 = DIRECT_ATTESTATION_INPUT_BYTES;
            const MAX_RESULT_BYTES: u64 = DIRECT_OBSERVATION_RESULT_BYTES;
        }
    };
}

define_attestation_activity!(
    AttestTargetTopologyActivity,
    "kuberic.switchover.attest-target-topology"
);
define_attestation_activity!(
    AttestCompensatedTopologyActivity,
    "kuberic.switchover.attest-compensated-topology"
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

    #[test]
    fn direct_activity_identities_are_unique_positive_and_operation_specific() {
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
    fn every_direct_activity_declares_independent_nonzero_bounds() {
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
    }
}
