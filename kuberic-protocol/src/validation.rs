use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::observation::{AgentObservation, ObservationSnapshot};
use crate::types::{
    AcceptedStatus, ConfigurationDescriptor, EffectivePolicy, ReplicaId, ReplicaRole,
    TransitionKind, derive_agent_generation, duplicate_replica_ids,
};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    #[error("desired replica count must be greater than zero")]
    DesiredReplicasZero,
    #[error("initialized status has no accepted topology")]
    InitializedWithoutTopology,
    #[error("never-initialized status contains an accepted topology")]
    TopologyBeforeInitialization,
    #[error("status cannot contain provisioning and a PC/CC transition simultaneously")]
    ProvisioningAndTransition,
    #[error("provisioning intent requires initialized accepted topology")]
    ProvisioningWithoutTopology,
    #[error("provisioning intent has inconsistent assigned durable generation")]
    ProvisioningGenerationMismatch,
    #[error("provisioning instance ID must equal the exact Pod UID")]
    ProvisioningInstanceMismatch,
    #[error("configuration ID {actual} does not match canonical ID {expected}")]
    ConfigurationIdMismatch { actual: String, expected: String },
    #[error("configuration must contain at least one member")]
    EmptyConfiguration,
    #[error("configuration has duplicate logical replica IDs: {0:?}")]
    DuplicateReplicaIds(Vec<i64>),
    #[error("configuration contains invalid replica ID {0}; IDs must be positive")]
    InvalidReplicaId(i64),
    #[error("configuration has duplicate exact replica identity {0}")]
    DuplicateReplicaIdentity(String),
    #[error("configuration primary {0} is not a Primary member")]
    MissingPrimary(i64),
    #[error("configuration has {0} Primary members; expected exactly one")]
    InvalidPrimaryCount(usize),
    #[error("configuration member count {actual} does not match frozen size {expected}")]
    ReplicaSetSizeMismatch { actual: u32, expected: u32 },
    #[error("configuration write quorum {actual} does not match fixed quorum {expected}")]
    WriteQuorumMismatch { actual: u32, expected: u32 },
    #[error("effective policy quorum values do not match fixed policy for size {0}")]
    InvalidEffectivePolicy(u32),
    #[error("bootstrap transition must not reference a Previous Configuration")]
    BootstrapHasPreviousConfiguration,
    #[error("bootstrap transition cannot coexist with accepted topology")]
    BootstrapHasTopology,
    #[error("non-bootstrap transition has no accepted Previous Configuration")]
    TransitionWithoutTopology,
    #[error("transition Previous Configuration {actual:?} does not match topology {expected}")]
    PreviousConfigurationMismatch {
        actual: Option<String>,
        expected: String,
    },
    #[error("provisioning target does not match resource UID")]
    ProvisioningResourceMismatch,
    #[error("provisioning target reuses the accepted exact incarnation")]
    ProvisioningReusesAcceptedIncarnation,
    #[error("replica map key {key} does not match reported replica ID {reported}")]
    ReplicaObservationKeyMismatch { key: i64, reported: i64 },
    #[error("replica {0} reports a different resource UID")]
    ReplicaResourceMismatch(i64),
    #[error("replica {replica_id} report sequence {observed} did not advance past {previous}")]
    StaleReportSequence {
        replica_id: i64,
        observed: u64,
        previous: u64,
    },
    #[error("replica {replica_id} reports stale epoch {observed:?} below accepted {accepted:?}")]
    StaleReplicaEpoch {
        replica_id: i64,
        observed: crate::types::Epoch,
        accepted: crate::types::Epoch,
    },
    #[error("replica {replica_id} contradicts accepted configuration at epoch {epoch:?}")]
    ConflictingReplicaConfiguration {
        replica_id: i64,
        epoch: crate::types::Epoch,
    },
    #[error("replica {replica_id} identity contradicts accepted member identity")]
    ConflictingReplicaIdentity { replica_id: i64 },
    #[error("multiple replicas claim Primary for the same accepted authority: {0:?}")]
    ConflictingPrimaryClaims(Vec<i64>),
    #[error("uninitialized agent identity does not match observed Pod/PVC scaffolding")]
    UninitializedScaffoldingMismatch,
}

pub fn validate_snapshot(snapshot: &ObservationSnapshot) -> Result<(), ValidationError> {
    if snapshot.desired.replicas == 0 {
        return Err(ValidationError::DesiredReplicasZero);
    }
    validate_status(&snapshot.status)?;

    if let Some(provisioning) = &snapshot.status.provisioning {
        if provisioning.resource_uid != snapshot.resource_uid {
            return Err(ValidationError::ProvisioningResourceMismatch);
        }
        if let Some(topology) = &snapshot.status.topology
            && topology.configuration.members.iter().any(|member| {
                member.identity.replica_id == provisioning.replica_id
                    && member.identity.instance_id == provisioning.instance_id
            })
        {
            return Err(ValidationError::ProvisioningReusesAcceptedIncarnation);
        }
        if provisioning.assigned_agent_generation
            != derive_agent_generation(&provisioning.initialization_id)
        {
            return Err(ValidationError::ProvisioningGenerationMismatch);
        }
        if provisioning.instance_id.as_str() != provisioning.pod_uid.as_str() {
            return Err(ValidationError::ProvisioningInstanceMismatch);
        }
    }

    let mut primary_claims: BTreeMap<_, Vec<ReplicaId>> = BTreeMap::new();
    for (replica_id, observation) in &snapshot.replicas {
        match &observation.agent {
            AgentObservation::Uninitialized(report) => {
                if report.replica_id != *replica_id {
                    return Err(ValidationError::ReplicaObservationKeyMismatch {
                        key: replica_id.value(),
                        reported: report.replica_id.value(),
                    });
                }
                if report.resource_uid != snapshot.resource_uid {
                    return Err(ValidationError::ReplicaResourceMismatch(replica_id.value()));
                }
                validate_report_sequence(
                    snapshot,
                    *replica_id,
                    &report.process_session_id,
                    report.report_sequence,
                )?;
                let matches_scaffolding =
                    observation.kubernetes.as_ref().is_some_and(|kubernetes| {
                        kubernetes.replica_id == *replica_id
                            && kubernetes.pod_uid.as_ref() == Some(&report.pod_uid)
                            && kubernetes.pvc_uid.as_ref() == Some(&report.pvc_uid)
                    });
                if !matches_scaffolding {
                    return Err(ValidationError::UninitializedScaffoldingMismatch);
                }
            }
            AgentObservation::Report(report) => {
                if report.identity.replica_id != *replica_id {
                    return Err(ValidationError::ReplicaObservationKeyMismatch {
                        key: replica_id.value(),
                        reported: report.identity.replica_id.value(),
                    });
                }
                if report.resource_uid != snapshot.resource_uid {
                    return Err(ValidationError::ReplicaResourceMismatch(replica_id.value()));
                }
                validate_report_sequence(
                    snapshot,
                    *replica_id,
                    &report.process_session_id,
                    report.report_sequence,
                )?;
                if let Some(previous) = &report.previous_configuration {
                    validate_configuration(previous, None)?;
                }

                if let Some(current) = &report.current_configuration {
                    validate_configuration(current, None)?;
                }
                if let Some(topology) = &snapshot.status.topology {
                    let accepted = &topology.configuration;
                    if report.epoch < accepted.epoch {
                        return Err(ValidationError::StaleReplicaEpoch {
                            replica_id: replica_id.value(),
                            observed: report.epoch,
                            accepted: accepted.epoch,
                        });
                    }
                    if report.epoch == accepted.epoch
                        && report
                            .current_configuration
                            .as_ref()
                            .is_some_and(|current| {
                                current.configuration_id != accepted.configuration_id
                            })
                    {
                        return Err(ValidationError::ConflictingReplicaConfiguration {
                            replica_id: replica_id.value(),
                            epoch: report.epoch,
                        });
                    }
                    if report.epoch == accepted.epoch {
                        let accepted_member = accepted
                            .members
                            .iter()
                            .find(|member| member.identity.replica_id == *replica_id);
                        if accepted_member.is_some_and(|member| member.identity != report.identity)
                        {
                            return Err(ValidationError::ConflictingReplicaIdentity {
                                replica_id: replica_id.value(),
                            });
                        }
                    }
                }
                if report.role == ReplicaRole::Primary
                    && let Some(current) = &report.current_configuration
                {
                    primary_claims
                        .entry((report.epoch, current.configuration_id.clone()))
                        .or_default()
                        .push(*replica_id);
                }
            }
            AgentObservation::Absent
            | AgentObservation::Unreachable { .. }
            | AgentObservation::Invalid { .. } => {}
        }
    }

    if let Some((_, claims)) = primary_claims
        .into_iter()
        .find(|(_, claims)| claims.len() > 1)
    {
        return Err(ValidationError::ConflictingPrimaryClaims(
            claims.into_iter().map(ReplicaId::value).collect(),
        ));
    }

    Ok(())
}

fn validate_report_sequence(
    snapshot: &ObservationSnapshot,
    replica_id: ReplicaId,
    session_id: &crate::types::ProcessSessionId,
    sequence: u64,
) -> Result<(), ValidationError> {
    if let Some(previous) = snapshot.previous_report_watermarks.get(&replica_id)
        && previous.process_session_id == *session_id
        && sequence <= previous.report_sequence
    {
        return Err(ValidationError::StaleReportSequence {
            replica_id: replica_id.value(),
            observed: sequence,
            previous: previous.report_sequence,
        });
    }
    Ok(())
}

pub fn validate_status(status: &AcceptedStatus) -> Result<(), ValidationError> {
    match (status.initialized, status.topology.as_ref()) {
        (true, None) => return Err(ValidationError::InitializedWithoutTopology),
        (false, Some(_)) => return Err(ValidationError::TopologyBeforeInitialization),
        _ => {}
    }
    if status.provisioning.is_some() && status.transition.is_some() {
        return Err(ValidationError::ProvisioningAndTransition);
    }
    if status.provisioning.is_some() && (!status.initialized || status.topology.is_none()) {
        return Err(ValidationError::ProvisioningWithoutTopology);
    }
    if let Some(topology) = &status.topology {
        validate_configuration(&topology.configuration, None)?;
    }
    if let Some(transition) = &status.transition {
        validate_policy(&transition.effective_policy)?;
        validate_configuration(
            &transition.current_configuration,
            Some(&transition.effective_policy),
        )?;
        match transition.kind {
            TransitionKind::Bootstrap => {
                if transition.previous_configuration_id.is_some() {
                    return Err(ValidationError::BootstrapHasPreviousConfiguration);
                }
                if status.topology.is_some() {
                    return Err(ValidationError::BootstrapHasTopology);
                }
            }
            TransitionKind::Replacement | TransitionKind::Failover => {
                let topology = status
                    .topology
                    .as_ref()
                    .ok_or(ValidationError::TransitionWithoutTopology)?;
                if transition.previous_configuration_id.as_ref()
                    != Some(&topology.configuration.configuration_id)
                {
                    return Err(ValidationError::PreviousConfigurationMismatch {
                        actual: transition
                            .previous_configuration_id
                            .as_ref()
                            .map(ToString::to_string),
                        expected: topology.configuration.configuration_id.to_string(),
                    });
                }
                validate_configuration(
                    &topology.configuration,
                    Some(&transition.effective_policy),
                )?;
            }
        }
    }
    Ok(())
}

pub fn validate_configuration(
    configuration: &ConfigurationDescriptor,
    policy: Option<&EffectivePolicy>,
) -> Result<(), ValidationError> {
    if configuration.members.is_empty() {
        return Err(ValidationError::EmptyConfiguration);
    }
    let expected_id = configuration.expected_id();
    if configuration.configuration_id != expected_id {
        return Err(ValidationError::ConfigurationIdMismatch {
            actual: configuration.configuration_id.to_string(),
            expected: expected_id.to_string(),
        });
    }

    let duplicate_ids = duplicate_replica_ids(&configuration.members);
    if !duplicate_ids.is_empty() {
        return Err(ValidationError::DuplicateReplicaIds(
            duplicate_ids.into_iter().map(ReplicaId::value).collect(),
        ));
    }
    if let Some(invalid) = configuration
        .members
        .iter()
        .map(|member| member.identity.replica_id)
        .find(|replica_id| replica_id.value() <= 0)
    {
        return Err(ValidationError::InvalidReplicaId(invalid.value()));
    }

    let mut identities = BTreeSet::new();
    for member in &configuration.members {
        let key = (
            member.identity.instance_id.clone(),
            member.identity.agent_generation.clone(),
        );
        if !identities.insert(key) {
            return Err(ValidationError::DuplicateReplicaIdentity(format!(
                "{}@{}",
                member.identity.instance_id, member.identity.agent_generation
            )));
        }
    }

    let primaries = configuration
        .members
        .iter()
        .filter(|member| member.role == ReplicaRole::Primary)
        .collect::<Vec<_>>();
    if primaries.len() != 1 {
        return Err(ValidationError::InvalidPrimaryCount(primaries.len()));
    }
    if primaries[0].identity.replica_id != configuration.primary_id {
        return Err(ValidationError::MissingPrimary(
            configuration.primary_id.value(),
        ));
    }

    let expected_policy = policy
        .cloned()
        .or_else(|| EffectivePolicy::fixed(configuration.members.len() as u32, 0))
        .expect("empty configurations returned before deriving policy");
    let actual_size = configuration.members.len() as u32;
    if actual_size != expected_policy.replica_set_size {
        return Err(ValidationError::ReplicaSetSizeMismatch {
            actual: actual_size,
            expected: expected_policy.replica_set_size,
        });
    }
    if configuration.write_quorum != expected_policy.write_quorum {
        return Err(ValidationError::WriteQuorumMismatch {
            actual: configuration.write_quorum,
            expected: expected_policy.write_quorum,
        });
    }
    Ok(())
}

fn validate_policy(policy: &EffectivePolicy) -> Result<(), ValidationError> {
    let expected = EffectivePolicy::fixed(policy.replica_set_size, policy.failover_delay_seconds)
        .ok_or(ValidationError::InvalidEffectivePolicy(
        policy.replica_set_size,
    ))?;
    if policy.write_quorum != expected.write_quorum || policy.read_quorum != expected.read_quorum {
        return Err(ValidationError::InvalidEffectivePolicy(
            policy.replica_set_size,
        ));
    }
    Ok(())
}
