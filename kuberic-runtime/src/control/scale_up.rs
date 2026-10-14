use crate::protocol::types::*;
use crate::protocol::validation::{
    ValidationError, validate_scale_up_cleanup, validate_scale_up_failover_evidence,
    validate_scale_up_provisioning_request, validate_scale_up_receipt,
};

use crate::control::convert::{
    WireError, access_status_from_proto, policy_from_proto, role_from_proto,
};
use crate::control::proto;

fn required<T>(value: Option<T>, name: &'static str) -> Result<T, WireError> {
    value.ok_or(WireError::MissingField(name))
}

fn authority(error: ValidationError) -> WireError {
    WireError::InvalidAuthority(error.to_string())
}

impl From<ScaleUpProvisioning> for proto::ScaleUpProvisioning {
    fn from(value: ScaleUpProvisioning) -> Self {
        Self {
            resource_uid: value.resource_uid.to_string(),
            spec_generation: value.spec_generation,
            desired_replicas: value.desired_replicas,
            previous_configuration: Some(value.previous_configuration.into()),
            previous_policy: Some(value.previous_policy.into()),
            current_policy: Some(value.current_policy.into()),
            target_replica_id: value.target_replica_id.value(),
        }
    }
}

impl TryFrom<proto::ScaleUpProvisioning> for ScaleUpProvisioning {
    type Error = WireError;

    fn try_from(value: proto::ScaleUpProvisioning) -> Result<Self, Self::Error> {
        let provisioning = Self {
            resource_uid: ResourceUid::new(value.resource_uid),
            spec_generation: value.spec_generation,
            desired_replicas: value.desired_replicas,
            previous_configuration: required(
                value.previous_configuration,
                "scale_up_provisioning.previous_configuration",
            )?
            .try_into()?,
            previous_policy: policy_from_proto(required(
                value.previous_policy,
                "scale_up_provisioning.previous_policy",
            )?)?,
            current_policy: policy_from_proto(required(
                value.current_policy,
                "scale_up_provisioning.current_policy",
            )?)?,
            target_replica_id: ReplicaId::new(value.target_replica_id),
        };
        validate_scale_up_provisioning_request(&provisioning).map_err(authority)?;
        Ok(provisioning)
    }
}

impl From<ScaleUpIntent> for proto::ScaleUpIntent {
    fn from(value: ScaleUpIntent) -> Self {
        Self {
            operation_id: value.operation_id.to_string(),
            resource_uid: value.resource_uid.to_string(),
            spec_generation: value.spec_generation,
            desired_replicas: value.desired_replicas,
            previous_configuration: Some(value.previous_configuration.into()),
            current_configuration: Some(value.current_configuration.into()),
            previous_policy: Some(value.previous_policy.into()),
            current_policy: Some(value.current_policy.into()),
            primary: Some(value.primary.into()),
            target: Some(value.target.into()),
            build_id: value.build_id.to_string(),
            snapshot_boundary_lsn: value.snapshot_boundary_lsn,
            catch_up_boundary_lsn: value.catch_up_boundary_lsn,
        }
    }
}

impl TryFrom<proto::ScaleUpIntent> for ScaleUpIntent {
    type Error = WireError;

    fn try_from(value: proto::ScaleUpIntent) -> Result<Self, Self::Error> {
        let intent = Self {
            operation_id: OperationId::new(value.operation_id),
            resource_uid: ResourceUid::new(value.resource_uid),
            spec_generation: value.spec_generation,
            desired_replicas: value.desired_replicas,
            previous_configuration: required(
                value.previous_configuration,
                "scale_up.previous_configuration",
            )?
            .try_into()?,
            current_configuration: required(
                value.current_configuration,
                "scale_up.current_configuration",
            )?
            .try_into()?,
            previous_policy: policy_from_proto(required(
                value.previous_policy,
                "scale_up.previous_policy",
            )?)?,
            current_policy: policy_from_proto(required(
                value.current_policy,
                "scale_up.current_policy",
            )?)?,
            primary: required(value.primary, "scale_up.primary")?.try_into()?,
            target: required(value.target, "scale_up.target")?.try_into()?,
            build_id: OperationId::new(value.build_id),
            snapshot_boundary_lsn: value.snapshot_boundary_lsn,
            catch_up_boundary_lsn: value.catch_up_boundary_lsn,
        };
        crate::protocol::validation::validate_scale_up(&intent).map_err(authority)?;
        Ok(intent)
    }
}

impl From<ScaleUpWitness> for proto::ScaleUpWitness {
    fn from(value: ScaleUpWitness) -> Self {
        Self {
            resource_uid: value.resource_uid.to_string(),
            identity: Some(value.identity.into()),
            process_session_id: value.process_session_id.to_string(),
            report_sequence: value.report_sequence,
            epoch: Some(value.epoch.into()),
            previous_configuration_id: value
                .previous_configuration_id
                .map_or_else(String::new, |id| id.to_string()),
            current_configuration_id: value.current_configuration_id.to_string(),
            verified_replication_lsn: value.verified_replication_lsn,
            write_status: crate::control::convert::access_status_to_proto(value.write_status)
                as i32,
            pending_operation_id: value
                .pending_operation_id
                .map_or_else(String::new, |id| id.to_string()),
            retained_operation_id: value
                .retained_operation_id
                .map_or_else(String::new, |id| id.to_string()),
            role: crate::control::convert::role_to_proto(value.role) as i32,
        }
    }
}

impl TryFrom<proto::ScaleUpWitness> for ScaleUpWitness {
    type Error = WireError;

    fn try_from(value: proto::ScaleUpWitness) -> Result<Self, Self::Error> {
        let witness = Self {
            resource_uid: ResourceUid::new(value.resource_uid),
            identity: required(value.identity, "scale_up_witness.identity")?.try_into()?,
            role: role_from_proto(proto::ReplicaRole::try_from(value.role).map_err(|_| {
                WireError::InvalidEnum {
                    field: "scale_up_witness.role",
                    value: value.role,
                }
            })?)?,
            process_session_id: ProcessSessionId::new(value.process_session_id),
            report_sequence: value.report_sequence,
            epoch: required(value.epoch, "scale_up_witness.epoch")?.into(),
            previous_configuration_id: (!value.previous_configuration_id.is_empty())
                .then(|| ConfigurationId::new(value.previous_configuration_id)),
            current_configuration_id: ConfigurationId::new(value.current_configuration_id),
            verified_replication_lsn: value.verified_replication_lsn,
            write_status: access_status_from_proto(
                proto::AccessStatus::try_from(value.write_status).map_err(|_| {
                    WireError::InvalidEnum {
                        field: "scale_up_witness.write_status",
                        value: value.write_status,
                    }
                })?,
            )?,
            pending_operation_id: (!value.pending_operation_id.is_empty())
                .then(|| OperationId::new(value.pending_operation_id)),
            retained_operation_id: (!value.retained_operation_id.is_empty())
                .then(|| OperationId::new(value.retained_operation_id)),
        };
        if witness.resource_uid.is_empty()
            || witness.identity.replica_id.value() <= 0
            || witness.identity.instance_id.is_empty()
            || witness.identity.agent_generation.is_empty()
            || witness.process_session_id.is_empty()
            || witness.report_sequence == 0
            || witness.epoch.data_loss_number < 0
            || witness.epoch.configuration_number < 0
            || witness.current_configuration_id.is_empty()
            || witness.verified_replication_lsn < 0
        {
            return Err(WireError::InvalidAuthority(
                "scale-up witness contains invalid identity, epoch, sequence, or progress".into(),
            ));
        }
        Ok(witness)
    }
}

impl From<ScaleUpFinalWitness> for proto::ScaleUpFinalWitness {
    fn from(value: ScaleUpFinalWitness) -> Self {
        Self {
            replica_id: value.replica_id.value(),
            process_session_id: value.process_session_id.to_string(),
            report_sequence: value.report_sequence,
            current_progress: value.current_progress,
            committed_lsn: value.committed_lsn,
            deactivated_lsn: value.deactivated_lsn,
            fence_operation_id: value.fence_operation_id.to_string(),
        }
    }
}

impl TryFrom<proto::ScaleUpFinalWitness> for ScaleUpFinalWitness {
    type Error = WireError;

    fn try_from(value: proto::ScaleUpFinalWitness) -> Result<Self, Self::Error> {
        let witness = Self {
            replica_id: ReplicaId::new(value.replica_id),
            process_session_id: ProcessSessionId::new(value.process_session_id),
            report_sequence: value.report_sequence,
            current_progress: value.current_progress,
            committed_lsn: value.committed_lsn,
            deactivated_lsn: value.deactivated_lsn,
            fence_operation_id: OperationId::new(value.fence_operation_id),
        };
        if witness.replica_id.value() <= 0
            || witness.process_session_id.is_empty()
            || witness.report_sequence == 0
            || witness.current_progress < 0
            || witness.committed_lsn < 0
            || witness.deactivated_lsn < 0
            || witness.fence_operation_id.is_empty()
        {
            return Err(WireError::InvalidAuthority(
                "final scale-up witness contains invalid replica, sequence, or progress".into(),
            ));
        }
        Ok(witness)
    }
}

impl From<ScaleUpFinalElectionEvidence> for proto::ScaleUpFinalElectionEvidence {
    fn from(value: ScaleUpFinalElectionEvidence) -> Self {
        Self {
            selected_primary_replica_id: value.selected_primary_replica_id.value(),
            witnesses: value.witnesses.into_iter().map(Into::into).collect(),
            previous_read_quorum: value
                .previous_read_quorum
                .into_iter()
                .map(ReplicaId::value)
                .collect(),
            current_read_quorum: value
                .current_read_quorum
                .into_iter()
                .map(ReplicaId::value)
                .collect(),
        }
    }
}

impl TryFrom<proto::ScaleUpFinalElectionEvidence> for ScaleUpFinalElectionEvidence {
    type Error = WireError;

    fn try_from(value: proto::ScaleUpFinalElectionEvidence) -> Result<Self, Self::Error> {
        let evidence = Self {
            selected_primary_replica_id: ReplicaId::new(value.selected_primary_replica_id),
            witnesses: value
                .witnesses
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            previous_read_quorum: value
                .previous_read_quorum
                .into_iter()
                .map(ReplicaId::new)
                .collect(),
            current_read_quorum: value
                .current_read_quorum
                .into_iter()
                .map(ReplicaId::new)
                .collect(),
        };
        if evidence.selected_primary_replica_id.value() <= 0
            || evidence
                .previous_read_quorum
                .iter()
                .chain(&evidence.current_read_quorum)
                .any(|replica_id| replica_id.value() <= 0)
        {
            return Err(WireError::InvalidAuthority(
                "final scale-up election contains invalid authority or progress".into(),
            ));
        }
        Ok(evidence)
    }
}

impl From<ScaleUpFailoverEvidence> for proto::ScaleUpFailoverEvidence {
    fn from(value: ScaleUpFailoverEvidence) -> Self {
        Self {
            intent: Some(value.intent.into()),
            provisional_configuration: Some(value.provisional_configuration.into()),
            previous_read_quorum: value
                .previous_read_quorum
                .into_iter()
                .map(Into::into)
                .collect(),
            current_read_quorum: value
                .current_read_quorum
                .into_iter()
                .map(Into::into)
                .collect(),
            final_election: value.final_election.map(|evidence| (*evidence).into()),
        }
    }
}

impl TryFrom<proto::ScaleUpFailoverEvidence> for ScaleUpFailoverEvidence {
    type Error = WireError;

    fn try_from(value: proto::ScaleUpFailoverEvidence) -> Result<Self, Self::Error> {
        let evidence = Self {
            intent: required(value.intent, "scale_up_failover.intent")?.try_into()?,
            provisional_configuration: required(
                value.provisional_configuration,
                "scale_up_failover.provisional_configuration",
            )?
            .try_into()?,
            previous_read_quorum: value
                .previous_read_quorum
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            current_read_quorum: value
                .current_read_quorum
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            final_election: value
                .final_election
                .map(TryInto::try_into)
                .transpose()?
                .map(Box::new),
        };
        validate_scale_up_failover_evidence(&evidence).map_err(authority)?;
        Ok(evidence)
    }
}

impl From<ScaleUpConfigurationEvidence> for proto::ScaleUpConfigurationEvidence {
    fn from(value: ScaleUpConfigurationEvidence) -> Self {
        use proto::scale_up_configuration_evidence::Evidence;
        Self {
            evidence: Some(match value {
                ScaleUpConfigurationEvidence::Admission { intent } => {
                    Evidence::Admission(Box::new(intent.into()))
                }
                ScaleUpConfigurationEvidence::Failover { evidence } => {
                    Evidence::Failover(Box::new(evidence.into()))
                }
            }),
        }
    }
}

impl TryFrom<proto::ScaleUpConfigurationEvidence> for ScaleUpConfigurationEvidence {
    type Error = WireError;

    fn try_from(value: proto::ScaleUpConfigurationEvidence) -> Result<Self, Self::Error> {
        use proto::scale_up_configuration_evidence::Evidence;
        match required(value.evidence, "scale_up_evidence.evidence")? {
            Evidence::Admission(intent) => Ok(Self::Admission {
                intent: (*intent).try_into()?,
            }),
            Evidence::Failover(evidence) => Ok(Self::Failover {
                evidence: (*evidence).try_into()?,
            }),
        }
    }
}

impl From<ScaleUpCleanup> for proto::ScaleUpCleanup {
    fn from(value: ScaleUpCleanup) -> Self {
        Self {
            provisioning: Some(super::convert::provisioning_to_proto(value.provisioning)),
            target: Some(value.target.into()),
            resources: Some(value.resources.into()),
        }
    }
}

impl TryFrom<proto::ScaleUpCleanup> for ScaleUpCleanup {
    type Error = WireError;

    fn try_from(value: proto::ScaleUpCleanup) -> Result<Self, Self::Error> {
        let cleanup = Self {
            provisioning: super::convert::provisioning_from_proto(required(
                value.provisioning,
                "scale_up_cleanup.provisioning",
            )?)?,
            target: required(value.target, "scale_up_cleanup.target")?.try_into()?,
            resources: required(value.resources, "scale_up_cleanup.resources")?.try_into()?,
        };
        validate_scale_up_cleanup(&cleanup).map_err(authority)?;
        Ok(cleanup)
    }
}

impl From<ScaleUpFailoverReceiptEvidence> for proto::ScaleUpFailoverReceiptEvidence {
    fn from(value: ScaleUpFailoverReceiptEvidence) -> Self {
        Self {
            provisional_primary_replica_id: value.provisional_primary_replica_id.value(),
            previous_read_quorum: value
                .previous_read_quorum
                .into_iter()
                .map(Into::into)
                .collect(),
            current_read_quorum: value
                .current_read_quorum
                .into_iter()
                .map(Into::into)
                .collect(),
            final_election: Some((*value.final_election).into()),
        }
    }
}

impl TryFrom<proto::ScaleUpFailoverReceiptEvidence> for ScaleUpFailoverReceiptEvidence {
    type Error = WireError;

    fn try_from(value: proto::ScaleUpFailoverReceiptEvidence) -> Result<Self, Self::Error> {
        let evidence = Self {
            provisional_primary_replica_id: ReplicaId::new(value.provisional_primary_replica_id),
            previous_read_quorum: value
                .previous_read_quorum
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            current_read_quorum: value
                .current_read_quorum
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            final_election: Box::new(
                required(
                    value.final_election,
                    "scale_up_failover_receipt.final_election",
                )?
                .try_into()?,
            ),
        };
        if evidence.provisional_primary_replica_id.value() <= 0 {
            return Err(WireError::InvalidAuthority(
                "scale-up failover receipt contains invalid provisional authority".into(),
            ));
        }
        Ok(evidence)
    }
}

impl From<ScaleUpReceipt> for proto::ScaleUpReceipt {
    fn from(value: ScaleUpReceipt) -> Self {
        Self {
            intent: Some(value.intent.into()),
            accepted_configuration: Some(value.accepted_configuration.into()),
            failover_evidence: value.failover_evidence.map(Into::into),
            failover_safe_lsn: value.failover_safe_lsn,
            current_only_write_quorum: value
                .current_only_write_quorum
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

impl TryFrom<proto::ScaleUpReceipt> for ScaleUpReceipt {
    type Error = WireError;

    fn try_from(value: proto::ScaleUpReceipt) -> Result<Self, Self::Error> {
        let receipt = Self {
            intent: required(value.intent, "scale_up_receipt.intent")?.try_into()?,
            accepted_configuration: required(
                value.accepted_configuration,
                "scale_up_receipt.accepted_configuration",
            )?
            .try_into()?,
            failover_evidence: value.failover_evidence.map(TryInto::try_into).transpose()?,
            failover_safe_lsn: value.failover_safe_lsn,
            current_only_write_quorum: value
                .current_only_write_quorum
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        };
        validate_scale_up_receipt(&receipt).map_err(authority)?;
        Ok(receipt)
    }
}
