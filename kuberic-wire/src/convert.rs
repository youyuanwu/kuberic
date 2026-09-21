use kuberic_protocol::types::{
    AgentGeneration, ConfigurationDescriptor, ConfigurationId, ConfigurationMember,
    EffectivePolicy, Epoch, InitializationId, ReplicaId, ReplicaIdentity, ReplicaInstanceId,
    ReplicaRole, derive_agent_generation,
};
use kuberic_protocol::validation::validate_configuration;
use thiserror::Error;

use crate::proto;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WireError {
    #[error("unsupported protocol version {observed}; expected {expected}")]
    UnsupportedProtocolVersion { expected: u32, observed: u32 },
    #[error("missing required field {0}")]
    MissingField(&'static str),
    #[error("invalid enum value {value} for {field}")]
    InvalidEnum { field: &'static str, value: i32 },
    #[error("invalid wire authority: {0}")]
    InvalidAuthority(String),
}

pub fn ensure_supported_version(observed: u32) -> Result<(), WireError> {
    if observed == kuberic_protocol::PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(WireError::UnsupportedProtocolVersion {
            expected: kuberic_protocol::PROTOCOL_VERSION,
            observed,
        })
    }
}

pub fn validate_agent_status_report(report: &proto::AgentStatusReport) -> Result<(), WireError> {
    ensure_supported_version(report.protocol_version)?;
    if report.resource_uid.is_empty() {
        return Err(WireError::MissingField("agent_status.resource_uid"));
    }
    if report.process_session_id.is_empty() {
        return Err(WireError::MissingField("agent_status.process_session_id"));
    }
    let storage_state = proto::AgentStorageState::try_from(report.storage_state).map_err(|_| {
        WireError::InvalidEnum {
            field: "agent_status.storage_state",
            value: report.storage_state,
        }
    })?;
    match storage_state {
        proto::AgentStorageState::Unknown => Err(WireError::InvalidEnum {
            field: "agent_status.storage_state",
            value: report.storage_state,
        }),
        proto::AgentStorageState::Uninitialized => {
            if report.pod_uid.is_empty() {
                return Err(WireError::MissingField("agent_status.pod_uid"));
            }
            if report.pvc_uid.is_empty() {
                return Err(WireError::MissingField("agent_status.pvc_uid"));
            }
            if report.identity.is_some() {
                return Err(WireError::InvalidAuthority(
                    "uninitialized agent status must not claim durable identity".to_string(),
                ));
            }
            Ok(())
        }
        proto::AgentStorageState::Initialized => {
            let _: ReplicaIdentity = report
                .identity
                .clone()
                .ok_or(WireError::MissingField("agent_status.identity"))?
                .try_into()?;
            if report.epoch.is_none() {
                return Err(WireError::MissingField("agent_status.epoch"));
            }
            if report.role == proto::ReplicaRole::Unknown as i32 {
                return Err(WireError::InvalidEnum {
                    field: "agent_status.role",
                    value: report.role,
                });
            }
            if report.write_status == proto::AccessStatus::Unknown as i32 {
                return Err(WireError::InvalidEnum {
                    field: "agent_status.write_status",
                    value: report.write_status,
                });
            }
            Ok(())
        }
        proto::AgentStorageState::Unsafe => {
            if report.storage_error.is_empty() {
                return Err(WireError::MissingField("agent_status.storage_error"));
            }
            Ok(())
        }
    }
}

pub fn validate_execute_request(request: &proto::ExecuteCommandRequest) -> Result<(), WireError> {
    ensure_supported_version(request.protocol_version)?;
    if request.resource_uid.is_empty() {
        return Err(WireError::MissingField("execute.resource_uid"));
    }
    let command = request
        .command
        .as_ref()
        .ok_or(WireError::MissingField("execute.command"))?;
    match command {
        proto::execute_command_request::Command::InitializeAgentStore(command) => {
            for (field, value) in [
                (
                    "initialize.initialization_id",
                    command.initialization_id.as_str(),
                ),
                ("initialize.resource_uid", command.resource_uid.as_str()),
                (
                    "initialize.expected_instance_id",
                    command.expected_instance_id.as_str(),
                ),
                (
                    "initialize.expected_pod_uid",
                    command.expected_pod_uid.as_str(),
                ),
                (
                    "initialize.expected_pvc_uid",
                    command.expected_pvc_uid.as_str(),
                ),
                (
                    "initialize.assigned_agent_generation",
                    command.assigned_agent_generation.as_str(),
                ),
            ] {
                if value.is_empty() {
                    return Err(WireError::MissingField(field));
                }
            }
            if command.resource_uid != request.resource_uid {
                return Err(WireError::InvalidAuthority(
                    "initialize resource UID differs from request fence".to_string(),
                ));
            }
            if command.local_replica_id <= 0 {
                return Err(WireError::InvalidAuthority(
                    "initialize replica ID must be positive".to_string(),
                ));
            }
            if command.expected_instance_id != command.expected_pod_uid {
                return Err(WireError::InvalidAuthority(
                    "initialize instance ID must equal exact Pod UID".to_string(),
                ));
            }
            if derive_agent_generation(&InitializationId::new(command.initialization_id.as_str()))
                .as_str()
                != command.assigned_agent_generation
            {
                return Err(WireError::InvalidAuthority(
                    "assigned generation does not match initialization identity".to_string(),
                ));
            }
            let policy = command
                .effective_policy
                .as_ref()
                .ok_or(WireError::MissingField("initialize.effective_policy"))?;
            validate_policy(policy)?;
            Ok(())
        }
        proto::execute_command_request::Command::EnsureConfiguration(command) => {
            let target: ReplicaIdentity = request
                .target
                .clone()
                .ok_or(WireError::MissingField("execute.target"))?
                .try_into()?;
            if command.operation_id.is_empty() {
                return Err(WireError::MissingField("ensure_configuration.operation_id"));
            }
            let current = command
                .current_configuration
                .clone()
                .ok_or(WireError::MissingField(
                    "ensure_configuration.current_configuration",
                ))?;
            let current = ConfigurationDescriptor::try_from(current)?;
            let current_epoch: Epoch = command
                .current_epoch
                .ok_or(WireError::MissingField(
                    "ensure_configuration.current_epoch",
                ))?
                .into();
            let policy = command
                .effective_policy
                .as_ref()
                .ok_or(WireError::MissingField(
                    "ensure_configuration.effective_policy",
                ))?;
            validate_policy(policy)?;
            if command.expected_instance_id.is_empty() {
                return Err(WireError::MissingField(
                    "ensure_configuration.expected_instance_id",
                ));
            }
            if command.expected_agent_generation.is_empty() {
                return Err(WireError::MissingField(
                    "ensure_configuration.expected_agent_generation",
                ));
            }
            if target.replica_id != ReplicaId::new(command.local_replica_id)
                || target.instance_id.as_str() != command.expected_instance_id
                || target.agent_generation.as_str() != command.expected_agent_generation
            {
                return Err(WireError::InvalidAuthority(
                    "ensure target differs from command fence".to_string(),
                ));
            }
            if current.epoch != current_epoch {
                return Err(WireError::InvalidAuthority(
                    "ensure current epoch differs from Current Configuration".to_string(),
                ));
            }
            if current.members.len() as u32 != policy.replica_set_size
                || current.write_quorum != policy.write_quorum
            {
                return Err(WireError::InvalidAuthority(
                    "ensure policy differs from Current Configuration".to_string(),
                ));
            }
            if let Some(previous) = command.previous_configuration.clone() {
                let previous = ConfigurationDescriptor::try_from(previous)?;
                let previous_epoch: Epoch = command
                    .previous_epoch
                    .ok_or(WireError::MissingField(
                        "ensure_configuration.previous_epoch",
                    ))?
                    .into();
                if previous.epoch != previous_epoch {
                    return Err(WireError::InvalidAuthority(
                        "ensure previous epoch differs from Previous Configuration".to_string(),
                    ));
                }
            } else if command.previous_epoch.is_some() {
                return Err(WireError::InvalidAuthority(
                    "ensure previous epoch exists without Previous Configuration".to_string(),
                ));
            }
            if !current
                .members
                .iter()
                .any(|member| member.identity == target)
            {
                return Err(WireError::InvalidAuthority(
                    "ensure target is not an exact Current Configuration member".to_string(),
                ));
            }
            Ok(())
        }
    }
}

pub fn validate_replication_item(item: &proto::ReplicationItem) -> Result<(), WireError> {
    ensure_supported_version(item.protocol_version)?;
    item.sender
        .clone()
        .ok_or(WireError::MissingField("replication_item.sender"))?
        .try_into()
        .map(|_: ReplicaIdentity| ())?;
    if item.epoch.is_none() {
        return Err(WireError::MissingField("replication_item.epoch"));
    }
    if item.current_configuration_id.is_empty() {
        return Err(WireError::MissingField(
            "replication_item.current_configuration_id",
        ));
    }
    if item.lsn <= 0 {
        return Err(WireError::InvalidAuthority(
            "replication item LSN must be positive".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_replication_ack(ack: &proto::ReplicationAck) -> Result<(), WireError> {
    ensure_supported_version(ack.protocol_version)?;
    ack.sender
        .clone()
        .ok_or(WireError::MissingField("replication_ack.sender"))?
        .try_into()
        .map(|_: ReplicaIdentity| ())?;
    ack.receiver
        .clone()
        .ok_or(WireError::MissingField("replication_ack.receiver"))?
        .try_into()
        .map(|_: ReplicaIdentity| ())?;
    if ack.epoch.is_none() {
        return Err(WireError::MissingField("replication_ack.epoch"));
    }
    if ack.current_configuration_id.is_empty() {
        return Err(WireError::MissingField(
            "replication_ack.current_configuration_id",
        ));
    }
    if ack.received_lsn <= 0
        || ack.applied_lsn > ack.received_lsn
        || ack.committed_lsn > ack.applied_lsn
    {
        return Err(WireError::InvalidAuthority(
            "replication ACK progress is inconsistent".to_string(),
        ));
    }
    Ok(())
}

impl From<Epoch> for proto::Epoch {
    fn from(value: Epoch) -> Self {
        Self {
            data_loss_number: value.data_loss_number,
            configuration_number: value.configuration_number,
        }
    }
}

impl From<proto::Epoch> for Epoch {
    fn from(value: proto::Epoch) -> Self {
        Self::new(value.data_loss_number, value.configuration_number)
    }
}

impl From<ReplicaIdentity> for proto::ReplicaIdentity {
    fn from(value: ReplicaIdentity) -> Self {
        Self {
            replica_id: value.replica_id.value(),
            instance_id: value.instance_id.to_string(),
            agent_generation: value.agent_generation.to_string(),
        }
    }
}

impl TryFrom<proto::ReplicaIdentity> for ReplicaIdentity {
    type Error = WireError;

    fn try_from(value: proto::ReplicaIdentity) -> Result<Self, Self::Error> {
        if value.replica_id <= 0 {
            return Err(WireError::InvalidAuthority(
                "replica ID must be positive".to_string(),
            ));
        }
        if value.instance_id.is_empty() {
            return Err(WireError::MissingField("replica_identity.instance_id"));
        }
        if value.agent_generation.is_empty() {
            return Err(WireError::MissingField("replica_identity.agent_generation"));
        }
        Ok(Self {
            replica_id: ReplicaId::new(value.replica_id),
            instance_id: ReplicaInstanceId::new(value.instance_id),
            agent_generation: AgentGeneration::new(value.agent_generation),
        })
    }
}

impl From<ConfigurationDescriptor> for proto::Configuration {
    fn from(value: ConfigurationDescriptor) -> Self {
        Self {
            configuration_id: value.configuration_id.to_string(),
            epoch: Some(value.epoch.into()),
            primary_id: value.primary_id.value(),
            members: value
                .members
                .into_iter()
                .map(|member| proto::ConfigurationMember {
                    identity: Some(member.identity.into()),
                    role: role_to_proto(member.role) as i32,
                })
                .collect(),
            write_quorum: value.write_quorum,
        }
    }
}

impl TryFrom<proto::Configuration> for ConfigurationDescriptor {
    type Error = WireError;

    fn try_from(value: proto::Configuration) -> Result<Self, Self::Error> {
        if value.configuration_id.is_empty() {
            return Err(WireError::MissingField("configuration.configuration_id"));
        }
        let epoch = value
            .epoch
            .ok_or(WireError::MissingField("configuration.epoch"))?
            .into();
        let mut members = value
            .members
            .into_iter()
            .map(|member| {
                let identity = member
                    .identity
                    .ok_or(WireError::MissingField("configuration.member.identity"))?
                    .try_into()?;
                let role = proto::ReplicaRole::try_from(member.role).map_err(|_| {
                    WireError::InvalidEnum {
                        field: "configuration.member.role",
                        value: member.role,
                    }
                })?;
                Ok(ConfigurationMember {
                    identity,
                    role: role_from_proto(role)?,
                })
            })
            .collect::<Result<Vec<_>, WireError>>()?;
        members.sort_by_key(|member| member.identity.replica_id);
        let configuration = ConfigurationDescriptor {
            configuration_id: ConfigurationId::new(value.configuration_id),
            epoch,
            primary_id: ReplicaId::new(value.primary_id),
            members,
            write_quorum: value.write_quorum,
        };
        validate_configuration(&configuration, None)
            .map_err(|error| WireError::InvalidAuthority(error.to_string()))?;
        Ok(configuration)
    }
}

fn role_to_proto(role: ReplicaRole) -> proto::ReplicaRole {
    match role {
        ReplicaRole::Primary => proto::ReplicaRole::Primary,
        ReplicaRole::ActiveSecondary => proto::ReplicaRole::ActiveSecondary,
        ReplicaRole::IdleSecondary => proto::ReplicaRole::IdleSecondary,
        ReplicaRole::None => proto::ReplicaRole::None,
    }
}

fn role_from_proto(role: proto::ReplicaRole) -> Result<ReplicaRole, WireError> {
    match role {
        proto::ReplicaRole::Unknown => Err(WireError::InvalidEnum {
            field: "configuration.member.role",
            value: role as i32,
        }),
        proto::ReplicaRole::Primary => Ok(ReplicaRole::Primary),
        proto::ReplicaRole::ActiveSecondary => Ok(ReplicaRole::ActiveSecondary),
        proto::ReplicaRole::IdleSecondary => Ok(ReplicaRole::IdleSecondary),
        proto::ReplicaRole::None => Ok(ReplicaRole::None),
    }
}

fn validate_policy(policy: &proto::EffectivePolicy) -> Result<(), WireError> {
    let expected = EffectivePolicy::fixed(policy.replica_set_size, policy.failover_delay_seconds)
        .ok_or_else(|| {
        WireError::InvalidAuthority("replica-set size must be positive".to_string())
    })?;
    if policy.write_quorum != expected.write_quorum || policy.read_quorum != expected.read_quorum {
        return Err(WireError::InvalidAuthority(
            "effective policy quorum values are not fixed-majority values".to_string(),
        ));
    }
    Ok(())
}
