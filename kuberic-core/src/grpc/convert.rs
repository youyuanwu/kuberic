use std::collections::HashSet;

use prost::Message;

use crate::proto;
use crate::types::{
    AccessStatus, AgentControlVersion, AgentGeneration, CorrelatedActionObservation,
    CorrelatedControlActionRequest, DataLossAction, DurableActionErrorClass,
    DurableActionObservation, DurableActionResult, DurableActionState, DurableReplicaAction, Epoch,
    FaultType, LocalFaultRecord, OpenMode, ReplicaConfigurationMemberStatus,
    ReplicaConfigurationMode, ReplicaConfigurationStatus, ReplicaDeactivationInfo,
    ReplicaElectionConfiguration, ReplicaInfo, ReplicaInstanceId, ReplicaSetConfig,
    ReplicaSetQuorumMode, ReplicaStatus, Role,
};

// --- Epoch ---

impl From<Epoch> for proto::EpochProto {
    fn from(e: Epoch) -> Self {
        proto::EpochProto {
            data_loss_number: e.data_loss_number,
            configuration_number: e.configuration_number,
        }
    }
}

impl From<proto::EpochProto> for Epoch {
    fn from(e: proto::EpochProto) -> Self {
        Epoch::new(e.data_loss_number, e.configuration_number)
    }
}

// --- Role ---

impl From<Role> for proto::RoleProto {
    fn from(r: Role) -> Self {
        match r {
            Role::Unknown => proto::RoleProto::RoleUnknown,
            Role::Primary => proto::RoleProto::RolePrimary,
            Role::ActiveSecondary => proto::RoleProto::RoleActiveSecondary,
            Role::IdleSecondary => proto::RoleProto::RoleIdleSecondary,
            Role::None => proto::RoleProto::RoleNone,
        }
    }
}

impl From<proto::RoleProto> for Role {
    fn from(r: proto::RoleProto) -> Self {
        match r {
            proto::RoleProto::RoleUnknown => Role::Unknown,
            proto::RoleProto::RolePrimary => Role::Primary,
            proto::RoleProto::RoleActiveSecondary => Role::ActiveSecondary,
            proto::RoleProto::RoleIdleSecondary => Role::IdleSecondary,
            proto::RoleProto::RoleNone => Role::None,
        }
    }
}

impl From<i32> for Role {
    fn from(v: i32) -> Self {
        proto::RoleProto::try_from(v)
            .unwrap_or(proto::RoleProto::RoleUnknown)
            .into()
    }
}

// --- OpenMode ---

impl From<OpenMode> for proto::OpenModeProto {
    fn from(m: OpenMode) -> Self {
        match m {
            OpenMode::New => proto::OpenModeProto::OpenNew,
            OpenMode::Existing => proto::OpenModeProto::OpenExisting,
        }
    }
}

impl From<proto::OpenModeProto> for OpenMode {
    fn from(m: proto::OpenModeProto) -> Self {
        match m {
            proto::OpenModeProto::OpenNew => OpenMode::New,
            proto::OpenModeProto::OpenExisting => OpenMode::Existing,
        }
    }
}

impl From<i32> for OpenMode {
    fn from(v: i32) -> Self {
        proto::OpenModeProto::try_from(v)
            .unwrap_or(proto::OpenModeProto::OpenNew)
            .into()
    }
}

// --- QuorumMode ---

impl From<ReplicaSetQuorumMode> for proto::QuorumModeProto {
    fn from(m: ReplicaSetQuorumMode) -> Self {
        match m {
            ReplicaSetQuorumMode::All => proto::QuorumModeProto::QuorumAll,
            ReplicaSetQuorumMode::Write => proto::QuorumModeProto::QuorumWrite,
        }
    }
}

impl From<proto::QuorumModeProto> for ReplicaSetQuorumMode {
    fn from(m: proto::QuorumModeProto) -> Self {
        match m {
            proto::QuorumModeProto::QuorumAll => ReplicaSetQuorumMode::All,
            proto::QuorumModeProto::QuorumWrite => ReplicaSetQuorumMode::Write,
        }
    }
}

impl From<i32> for ReplicaSetQuorumMode {
    fn from(v: i32) -> Self {
        proto::QuorumModeProto::try_from(v)
            .unwrap_or(proto::QuorumModeProto::QuorumAll)
            .into()
    }
}

impl From<AccessStatus> for proto::AccessStatusProto {
    fn from(status: AccessStatus) -> Self {
        match status {
            AccessStatus::Granted => Self::AccessGranted,
            AccessStatus::ReconfigurationPending => Self::AccessReconfigurationPending,
            AccessStatus::NotPrimary => Self::AccessNotPrimary,
            AccessStatus::NoWriteQuorum => Self::AccessNoWriteQuorum,
        }
    }
}

pub(crate) fn try_access_status(value: i32) -> Result<AccessStatus, String> {
    match proto::AccessStatusProto::try_from(value)
        .map_err(|_| format!("unknown runtime write status {value}"))?
    {
        proto::AccessStatusProto::AccessGranted => Ok(AccessStatus::Granted),
        proto::AccessStatusProto::AccessReconfigurationPending => {
            Ok(AccessStatus::ReconfigurationPending)
        }
        proto::AccessStatusProto::AccessNotPrimary => Ok(AccessStatus::NotPrimary),
        proto::AccessStatusProto::AccessNoWriteQuorum => Ok(AccessStatus::NoWriteQuorum),
    }
}

impl From<ReplicaConfigurationStatus> for proto::ReplicaConfigurationStatusProto {
    fn from(status: ReplicaConfigurationStatus) -> Self {
        Self {
            mode: match status.mode {
                ReplicaConfigurationMode::CatchUp => {
                    proto::ReplicaConfigurationModeProto::ConfigurationCatchUp as i32
                }
                ReplicaConfigurationMode::Current => {
                    proto::ReplicaConfigurationModeProto::ConfigurationCurrent as i32
                }
            },
            members: status
                .members
                .into_iter()
                .map(|member| proto::ReplicaConfigurationMemberStatusProto {
                    id: member.id,
                    instance_id: member.instance_id.to_string(),
                    role: proto::RoleProto::from(member.role) as i32,
                })
                .collect(),
            write_quorum: status.write_quorum,
        }
    }
}

pub(crate) fn try_runtime_configuration_status(
    status: proto::ReplicaConfigurationStatusProto,
) -> Result<ReplicaConfigurationStatus, String> {
    let mode = match proto::ReplicaConfigurationModeProto::try_from(status.mode)
        .map_err(|_| format!("unknown runtime configuration mode {}", status.mode))?
    {
        proto::ReplicaConfigurationModeProto::ConfigurationCatchUp => {
            ReplicaConfigurationMode::CatchUp
        }
        proto::ReplicaConfigurationModeProto::ConfigurationCurrent => {
            ReplicaConfigurationMode::Current
        }
        proto::ReplicaConfigurationModeProto::ConfigurationNone => {
            return Err("runtime configuration mode is none".to_string());
        }
    };
    let mut ids = HashSet::new();
    let mut instances = HashSet::new();
    let members = status
        .members
        .into_iter()
        .map(|member| {
            if member.id <= 0 || !ids.insert(member.id) {
                return Err(format!(
                    "runtime configuration has invalid or duplicate replica ID {}",
                    member.id
                ));
            }
            if member.instance_id.is_empty() || !instances.insert(member.instance_id.clone()) {
                return Err(format!(
                    "runtime configuration replica {} has missing or duplicate incarnation",
                    member.id
                ));
            }
            let role = proto::RoleProto::try_from(member.role)
                .map_err(|_| format!("unknown role {}", member.role))?;
            if role == proto::RoleProto::RoleUnknown {
                return Err(format!(
                    "runtime configuration replica {} has unknown role",
                    member.id
                ));
            }
            Ok(ReplicaConfigurationMemberStatus {
                id: member.id,
                instance_id: ReplicaInstanceId::new(member.instance_id),
                role: role.into(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if status.write_quorum == 0 || status.write_quorum as usize > members.len() + 1 {
        return Err(format!(
            "invalid runtime configuration write quorum {} for {} remote members",
            status.write_quorum,
            members.len()
        ));
    }
    Ok(ReplicaConfigurationStatus {
        mode,
        members,
        write_quorum: status.write_quorum,
    })
}

impl From<ReplicaElectionConfiguration> for proto::ReplicaElectionConfigurationProto {
    fn from(configuration: ReplicaElectionConfiguration) -> Self {
        Self {
            current: Some(configuration.current.into()),
            previous: configuration.previous.map(Into::into),
        }
    }
}

impl TryFrom<proto::ReplicaElectionConfigurationProto> for ReplicaElectionConfiguration {
    type Error = String;

    fn try_from(
        configuration: proto::ReplicaElectionConfigurationProto,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            current: try_configuration_status(
                configuration
                    .current
                    .ok_or_else(|| "missing current election configuration".to_string())?,
            )?,
            previous: configuration
                .previous
                .map(try_configuration_status)
                .transpose()?,
        })
    }
}

impl From<ReplicaDeactivationInfo> for proto::ReplicaDeactivationInfoProto {
    fn from(info: ReplicaDeactivationInfo) -> Self {
        Self {
            epoch: Some(info.epoch.into()),
            catch_up_lsn: info.catch_up_lsn,
        }
    }
}

impl TryFrom<proto::ReplicaDeactivationInfoProto> for ReplicaDeactivationInfo {
    type Error = String;

    fn try_from(info: proto::ReplicaDeactivationInfoProto) -> Result<Self, Self::Error> {
        Ok(Self {
            epoch: info
                .epoch
                .ok_or_else(|| "missing deactivation epoch".to_string())?
                .into(),
            catch_up_lsn: info.catch_up_lsn,
        })
    }
}

impl From<DurableActionResult> for proto::DurableActionResultProto {
    fn from(result: DurableActionResult) -> Self {
        match result {
            DurableActionResult::DataLoss(DataLossAction::None) => {
                Self::DurableActionResultDataLossNoStateChange
            }
            DurableActionResult::DataLoss(DataLossAction::StateChanged) => {
                Self::DurableActionResultDataLossStateChanged
            }
        }
    }
}

impl TryFrom<proto::DurableActionResultProto> for DurableActionResult {
    type Error = ();

    fn try_from(result: proto::DurableActionResultProto) -> Result<Self, Self::Error> {
        match result {
            proto::DurableActionResultProto::DurableActionResultDataLossNoStateChange => {
                Ok(Self::DataLoss(DataLossAction::None))
            }

            proto::DurableActionResultProto::DurableActionResultDataLossStateChanged => {
                Ok(Self::DataLoss(DataLossAction::StateChanged))
            }
            proto::DurableActionResultProto::DurableActionResultNone => Err(()),
        }
    }
}

impl From<DurableActionErrorClass> for proto::DurableActionErrorClassProto {
    fn from(class: DurableActionErrorClass) -> Self {
        match class {
            DurableActionErrorClass::Internal => Self::DurableActionErrorInternal,
            DurableActionErrorClass::NotPrimary => Self::DurableActionErrorNotPrimary,
            DurableActionErrorClass::NoWriteQuorum => Self::DurableActionErrorNoWriteQuorum,
            DurableActionErrorClass::ReconfigurationPending => {
                Self::DurableActionErrorReconfigurationPending
            }
            DurableActionErrorClass::StaleEpoch => Self::DurableActionErrorStaleEpoch,
            DurableActionErrorClass::Cancelled => Self::DurableActionErrorCancelled,
            DurableActionErrorClass::Closed => Self::DurableActionErrorClosed,
        }
    }
}

impl TryFrom<proto::DurableActionErrorClassProto> for DurableActionErrorClass {
    type Error = ();

    fn try_from(class: proto::DurableActionErrorClassProto) -> Result<Self, Self::Error> {
        match class {
            proto::DurableActionErrorClassProto::DurableActionErrorInternal => Ok(Self::Internal),
            proto::DurableActionErrorClassProto::DurableActionErrorNotPrimary => {
                Ok(Self::NotPrimary)
            }
            proto::DurableActionErrorClassProto::DurableActionErrorNoWriteQuorum => {
                Ok(Self::NoWriteQuorum)
            }
            proto::DurableActionErrorClassProto::DurableActionErrorReconfigurationPending => {
                Ok(Self::ReconfigurationPending)
            }
            proto::DurableActionErrorClassProto::DurableActionErrorStaleEpoch => {
                Ok(Self::StaleEpoch)
            }
            proto::DurableActionErrorClassProto::DurableActionErrorCancelled => Ok(Self::Cancelled),
            proto::DurableActionErrorClassProto::DurableActionErrorClosed => Ok(Self::Closed),
            proto::DurableActionErrorClassProto::DurableActionErrorNone => Err(()),
        }
    }
}

fn try_configuration_status(
    status: proto::ReplicaConfigurationStatusProto,
) -> Result<ReplicaConfigurationStatus, String> {
    let mode = match proto::ReplicaConfigurationModeProto::try_from(status.mode)
        .map_err(|_| format!("unknown configuration mode {}", status.mode))?
    {
        proto::ReplicaConfigurationModeProto::ConfigurationCatchUp => {
            ReplicaConfigurationMode::CatchUp
        }
        proto::ReplicaConfigurationModeProto::ConfigurationCurrent => {
            ReplicaConfigurationMode::Current
        }
        proto::ReplicaConfigurationModeProto::ConfigurationNone => {
            return Err("election configuration mode is none".to_string());
        }
    };
    let members = status
        .members
        .into_iter()
        .map(|member| {
            if member.instance_id.is_empty() {
                return Err(format!(
                    "election configuration member {} has empty incarnation",
                    member.id
                ));
            }
            let role = proto::RoleProto::try_from(member.role)
                .map_err(|_| format!("unknown role {}", member.role))?;
            if role == proto::RoleProto::RoleUnknown {
                return Err(format!(
                    "election configuration member {} has unknown role",
                    member.id
                ));
            }
            Ok(ReplicaConfigurationMemberStatus {
                id: member.id,
                instance_id: ReplicaInstanceId::new(member.instance_id),
                role: role.into(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if members.is_empty() {
        return Err("election configuration has no members".to_string());
    }
    if status.write_quorum == 0 || status.write_quorum as usize > members.len() {
        return Err(format!(
            "invalid election configuration write quorum {} for {} members",
            status.write_quorum,
            members.len()
        ));
    }
    Ok(ReplicaConfigurationStatus {
        mode,
        members,
        write_quorum: status.write_quorum,
    })
}

// --- ReplicaInfo ---

impl From<ReplicaInfo> for proto::ReplicaInfoProto {
    fn from(r: ReplicaInfo) -> Self {
        proto::ReplicaInfoProto {
            id: r.id,
            instance_id: r.instance_id.to_string(),
            role: proto::RoleProto::from(r.role) as i32,
            status: match r.status {
                ReplicaStatus::Up => proto::ReplicaStatusProto::StatusUp as i32,
                ReplicaStatus::Down => proto::ReplicaStatusProto::StatusDown as i32,
            },
            replicator_address: r.replicator_address,
            current_progress: r.current_progress,
            catch_up_capability: r.catch_up_capability,
            must_catch_up: r.must_catch_up,
        }
    }
}

impl From<proto::ReplicaInfoProto> for ReplicaInfo {
    fn from(r: proto::ReplicaInfoProto) -> Self {
        ReplicaInfo {
            id: r.id,
            instance_id: ReplicaInstanceId::new(r.instance_id),
            role: Role::from(r.role),
            status: if r.status == proto::ReplicaStatusProto::StatusUp as i32 {
                ReplicaStatus::Up
            } else {
                ReplicaStatus::Down
            },
            replicator_address: r.replicator_address,
            current_progress: r.current_progress,
            catch_up_capability: r.catch_up_capability,
            must_catch_up: r.must_catch_up,
        }
    }
}

// --- ReplicaSetConfig ---

impl From<ReplicaSetConfig> for proto::ReplicaSetConfigProto {
    fn from(c: ReplicaSetConfig) -> Self {
        proto::ReplicaSetConfigProto {
            members: c.members.into_iter().map(|r| r.into()).collect(),
            write_quorum: c.write_quorum,
        }
    }
}

impl From<proto::ReplicaSetConfigProto> for ReplicaSetConfig {
    fn from(c: proto::ReplicaSetConfigProto) -> Self {
        ReplicaSetConfig {
            members: c.members.into_iter().map(|r| r.into()).collect(),
            write_quorum: c.write_quorum,
        }
    }
}

/// Serialize the exact correlated action payload for durable operator
/// write-ahead intent. Envelope fences are persisted separately.
pub fn encode_correlated_action_payload(action: &DurableReplicaAction) -> String {
    proto::ExecuteCorrelatedControlActionRequest {
        protocol_version: 0,
        action_id: String::new(),
        input_signature: String::new(),
        target_replica_id: 0,
        target_instance_id: String::new(),
        expected_agent_generation: String::new(),
        expected_agent_control_version: None,
        observed_runtime_epoch: None,
        action: Some(correlated_action_proto(action.clone())),
    }
    .encode_to_vec()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

/// Decode a previously persisted exact correlated action payload.
pub fn decode_correlated_action_payload(encoded: &str) -> Result<DurableReplicaAction, String> {
    let encoded = encoded.as_bytes();
    if encoded.is_empty()
        || encoded.len() % 2 != 0
        || !encoded
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return Err("persisted correlated action payload is not valid hexadecimal".to_string());
    }
    let bytes = encoded
        .chunks_exact(2)
        .map(|pair| {
            let nibble = |byte: u8| {
                if byte.is_ascii_digit() {
                    byte - b'0'
                } else {
                    byte - b'a' + 10
                }
            };
            (nibble(pair[0]) << 4) | nibble(pair[1])
        })
        .collect::<Vec<_>>();
    let request = proto::ExecuteCorrelatedControlActionRequest::decode(bytes.as_slice())
        .map_err(|error| format!("invalid persisted correlated action payload: {error}"))?;
    strict_correlated_action(
        request
            .action
            .ok_or_else(|| "persisted correlated action payload is missing action".to_string())?,
    )
}

impl TryFrom<proto::ExecuteCorrelatedControlActionRequest> for CorrelatedControlActionRequest {
    type Error = String;

    fn try_from(
        request: proto::ExecuteCorrelatedControlActionRequest,
    ) -> Result<Self, Self::Error> {
        if request.action_id.is_empty() {
            return Err("missing correlated action ID".to_string());
        }
        if request.input_signature.is_empty() {
            return Err("missing correlated action input signature".to_string());
        }
        if request.target_replica_id <= 0 {
            return Err("target replica ID must be positive".to_string());
        }
        if request.target_instance_id.is_empty() {
            return Err("missing target replica incarnation".to_string());
        }
        let expected_agent_generation = AgentGeneration::parse(request.expected_agent_generation)?;
        let expected_control_version = AgentControlVersion::new(
            request
                .expected_agent_control_version
                .ok_or_else(|| "missing expected agent control version".to_string())?,
        );
        let observed_runtime_epoch = request
            .observed_runtime_epoch
            .ok_or_else(|| "missing observed runtime epoch".to_string())?
            .into();
        let action = strict_correlated_action(
            request
                .action
                .ok_or_else(|| "missing correlated control action".to_string())?,
        )?;
        Ok(Self {
            protocol_version: request.protocol_version,
            action_id: request.action_id,
            input_signature: request.input_signature,
            target_replica_id: request.target_replica_id,
            target_instance_id: ReplicaInstanceId::new(request.target_instance_id),
            expected_agent_generation,
            expected_control_version,
            observed_runtime_epoch,
            action,
        })
    }
}

impl From<CorrelatedControlActionRequest> for proto::ExecuteCorrelatedControlActionRequest {
    fn from(request: CorrelatedControlActionRequest) -> Self {
        Self {
            protocol_version: request.protocol_version,
            action_id: request.action_id,
            input_signature: request.input_signature,
            target_replica_id: request.target_replica_id,
            target_instance_id: request.target_instance_id.to_string(),
            expected_agent_generation: request.expected_agent_generation.to_string(),
            expected_agent_control_version: Some(request.expected_control_version.value()),
            observed_runtime_epoch: Some(request.observed_runtime_epoch.into()),
            action: Some(correlated_action_proto(request.action)),
        }
    }
}

impl From<CorrelatedActionObservation> for proto::CorrelatedActionObservationProto {
    fn from(observation: CorrelatedActionObservation) -> Self {
        Self {
            agent_generation: observation.generation.to_string(),
            agent_control_version: observation.control_version.value(),
            action_id: observation.action.action_id,
            input_signature: observation.action.signature,
            state: match observation.action.state {
                DurableActionState::Scheduled => {
                    proto::DurableActionStateProto::DurableActionScheduled as i32
                }
                DurableActionState::InProgress => {
                    proto::DurableActionStateProto::DurableActionInProgress as i32
                }
                DurableActionState::Completed => {
                    proto::DurableActionStateProto::DurableActionCompleted as i32
                }
                DurableActionState::Failed => {
                    proto::DurableActionStateProto::DurableActionFailed as i32
                }
            },
            error: observation.action.error.unwrap_or_default(),
            result: observation
                .action
                .result
                .map(proto::DurableActionResultProto::from)
                .unwrap_or(proto::DurableActionResultProto::DurableActionResultNone)
                as i32,
            error_class: observation
                .action
                .error_class
                .map(proto::DurableActionErrorClassProto::from)
                .unwrap_or(proto::DurableActionErrorClassProto::DurableActionErrorNone)
                as i32,
        }
    }
}

impl TryFrom<proto::CorrelatedActionObservationProto> for CorrelatedActionObservation {
    type Error = String;

    fn try_from(observation: proto::CorrelatedActionObservationProto) -> Result<Self, Self::Error> {
        if observation.action_id.is_empty() {
            return Err("missing correlated action observation ID".to_string());
        }
        if observation.input_signature.is_empty() {
            return Err("missing correlated action observation signature".to_string());
        }
        let state = match proto::DurableActionStateProto::try_from(observation.state)
            .map_err(|_| format!("unknown durable action state {}", observation.state))?
        {
            proto::DurableActionStateProto::DurableActionScheduled => DurableActionState::Scheduled,
            proto::DurableActionStateProto::DurableActionInProgress => {
                DurableActionState::InProgress
            }
            proto::DurableActionStateProto::DurableActionCompleted => DurableActionState::Completed,
            proto::DurableActionStateProto::DurableActionFailed => DurableActionState::Failed,
            proto::DurableActionStateProto::DurableActionNone => {
                return Err("correlated action state is none".to_string());
            }
        };
        let result = proto::DurableActionResultProto::try_from(observation.result)
            .map_err(|_| format!("unknown durable action result {}", observation.result))?;
        let error_class = proto::DurableActionErrorClassProto::try_from(observation.error_class)
            .map_err(|_| {
                format!(
                    "unknown durable action error class {}",
                    observation.error_class
                )
            })?;
        let error_class =
            if error_class == proto::DurableActionErrorClassProto::DurableActionErrorNone {
                None
            } else {
                Some(
                    DurableActionErrorClass::try_from(error_class)
                        .map_err(|_| "invalid durable action error class".to_string())?,
                )
            };
        let error = (!observation.error.is_empty()).then_some(observation.error);
        let result = if result == proto::DurableActionResultProto::DurableActionResultNone {
            None
        } else {
            Some(
                DurableActionResult::try_from(result)
                    .map_err(|_| "invalid durable action result".to_string())?,
            )
        };
        match state {
            DurableActionState::Scheduled | DurableActionState::InProgress
                if error_class.is_some() || error.is_some() || result.is_some() =>
            {
                return Err("non-terminal correlated action carries terminal data".to_string());
            }
            DurableActionState::Completed if error_class.is_some() || error.is_some() => {
                return Err("completed correlated action carries an error".to_string());
            }
            DurableActionState::Failed
                if error_class.is_none() || error.is_none() || result.is_some() =>
            {
                return Err("failed correlated action has malformed terminal data".to_string());
            }
            _ => {}
        }
        Ok(Self {
            generation: AgentGeneration::parse(observation.agent_generation)?,
            control_version: AgentControlVersion::new(observation.agent_control_version),
            action: DurableActionObservation {
                action_id: observation.action_id,
                signature: observation.input_signature,
                state,
                error_class,
                error,
                result,
            },
        })
    }
}

impl From<LocalFaultRecord> for proto::LocalFaultRecordProto {
    fn from(record: LocalFaultRecord) -> Self {
        Self {
            sequence: record.sequence,
            fault_type: match record.fault_type {
                FaultType::Transient => proto::LocalFaultTypeProto::LocalFaultTransient as i32,
                FaultType::Permanent => proto::LocalFaultTypeProto::LocalFaultPermanent as i32,
            },
        }
    }
}

impl TryFrom<proto::LocalFaultRecordProto> for LocalFaultRecord {
    type Error = String;

    fn try_from(record: proto::LocalFaultRecordProto) -> Result<Self, Self::Error> {
        let fault_type = match proto::LocalFaultTypeProto::try_from(record.fault_type)
            .map_err(|_| format!("unknown local fault type {}", record.fault_type))?
        {
            proto::LocalFaultTypeProto::LocalFaultTransient => FaultType::Transient,
            proto::LocalFaultTypeProto::LocalFaultPermanent => FaultType::Permanent,
            proto::LocalFaultTypeProto::LocalFaultUnknown => {
                return Err("local fault type is unknown".to_string());
            }
        };
        Ok(Self {
            sequence: record.sequence,
            fault_type,
        })
    }
}

fn strict_correlated_action(
    action: proto::execute_correlated_control_action_request::Action,
) -> Result<DurableReplicaAction, String> {
    use proto::execute_correlated_control_action_request::Action;
    match action {
        Action::RevokeWriteStatus(_) => Ok(DurableReplicaAction::RevokeWriteStatus),
        Action::ChangeRole(request) => {
            let role = proto::RoleProto::try_from(request.role)
                .map_err(|_| format!("unknown role {}", request.role))?;
            if role == proto::RoleProto::RoleUnknown {
                return Err("change-role target role is unknown".to_string());
            }
            Ok(DurableReplicaAction::ChangeRole {
                epoch: request
                    .epoch
                    .ok_or_else(|| "missing change-role target epoch".to_string())?
                    .into(),
                role: role.into(),
            })
        }
        Action::UpdateEpoch(request) => Ok(DurableReplicaAction::UpdateEpoch {
            epoch: request
                .epoch
                .ok_or_else(|| "missing update-epoch target epoch".to_string())?
                .into(),
        }),
        Action::UpdateCatchUpConfiguration(request) => {
            Ok(DurableReplicaAction::UpdateCatchUpConfiguration {
                current: try_replica_set_config(
                    request
                        .current
                        .ok_or_else(|| "missing current catch-up configuration".to_string())?,
                )?,
                previous: try_replica_set_config(
                    request
                        .previous
                        .ok_or_else(|| "missing previous catch-up configuration".to_string())?,
                )?,
            })
        }
        Action::WaitForCatchUpQuorum(request) => {
            let mode = proto::QuorumModeProto::try_from(request.mode)
                .map_err(|_| format!("unknown quorum mode {}", request.mode))?;
            Ok(DurableReplicaAction::WaitForCatchUpQuorum { mode: mode.into() })
        }
        Action::UpdateCurrentConfiguration(request) => {
            Ok(DurableReplicaAction::UpdateCurrentConfiguration {
                current: try_replica_set_config(
                    request
                        .current
                        .ok_or_else(|| "missing current configuration".to_string())?,
                )?,
            })
        }
        Action::Open(request) => {
            let mode = proto::OpenModeProto::try_from(request.mode)
                .map_err(|_| format!("unknown open mode {}", request.mode))?;
            Ok(DurableReplicaAction::Open { mode: mode.into() })
        }
        Action::Close(_) => Ok(DurableReplicaAction::Close),
        Action::BuildReplica(request) => Ok(DurableReplicaAction::BuildReplica {
            replica: try_replica_info(
                request
                    .replica
                    .ok_or_else(|| "missing build replica".to_string())?,
            )?,
        }),
        Action::RemoveReplica(request) => {
            if request.replica_id <= 0 || request.instance_id.is_empty() {
                return Err("remove-replica target identity is invalid".to_string());
            }
            Ok(DurableReplicaAction::RemoveReplica {
                replica_id: request.replica_id,
                instance_id: ReplicaInstanceId::new(request.instance_id),
            })
        }
        Action::OnDataLoss(request) => Ok(DurableReplicaAction::OnDataLoss {
            epoch: request
                .expected_epoch
                .ok_or_else(|| "missing data-loss epoch".to_string())?
                .into(),
        }),
        Action::RecordElectionConfiguration(request) => {
            Ok(DurableReplicaAction::RecordElectionConfiguration {
                configuration: ReplicaElectionConfiguration::try_from(
                    request
                        .configuration
                        .ok_or_else(|| "missing election configuration".to_string())?,
                )?,
            })
        }
    }
}

fn correlated_action_proto(
    action: DurableReplicaAction,
) -> proto::execute_correlated_control_action_request::Action {
    use proto::execute_correlated_control_action_request::Action;
    match action {
        DurableReplicaAction::Open { mode } => Action::Open(proto::OpenRequest {
            mode: proto::OpenModeProto::from(mode) as i32,
        }),
        DurableReplicaAction::Close => Action::Close(proto::CloseRequest {}),
        DurableReplicaAction::RevokeWriteStatus => {
            Action::RevokeWriteStatus(proto::RevokeWriteStatusRequest {})
        }
        DurableReplicaAction::ChangeRole { epoch, role } => {
            Action::ChangeRole(proto::ChangeRoleRequest {
                epoch: Some(epoch.into()),
                role: proto::RoleProto::from(role) as i32,
            })
        }
        DurableReplicaAction::UpdateEpoch { epoch } => {
            Action::UpdateEpoch(proto::UpdateEpochRequest {
                epoch: Some(epoch.into()),
            })
        }
        DurableReplicaAction::UpdateCatchUpConfiguration { current, previous } => {
            Action::UpdateCatchUpConfiguration(proto::UpdateCatchUpConfigRequest {
                current: Some(current.into()),
                previous: Some(previous.into()),
            })
        }
        DurableReplicaAction::WaitForCatchUpQuorum { mode } => {
            Action::WaitForCatchUpQuorum(proto::WaitForCatchUpQuorumRequest {
                mode: proto::QuorumModeProto::from(mode) as i32,
            })
        }
        DurableReplicaAction::UpdateCurrentConfiguration { current } => {
            Action::UpdateCurrentConfiguration(proto::UpdateCurrentConfigRequest {
                current: Some(current.into()),
            })
        }
        DurableReplicaAction::BuildReplica { replica } => {
            Action::BuildReplica(proto::BuildReplicaRequest {
                replica: Some(replica.into()),
            })
        }
        DurableReplicaAction::RemoveReplica {
            replica_id,
            instance_id,
        } => Action::RemoveReplica(proto::RemoveReplicaRequest {
            replica_id,
            instance_id: instance_id.to_string(),
        }),
        DurableReplicaAction::OnDataLoss { epoch } => {
            Action::OnDataLoss(proto::DurableOnDataLossRequest {
                expected_epoch: Some(epoch.into()),
            })
        }
        DurableReplicaAction::RecordElectionConfiguration { configuration } => {
            Action::RecordElectionConfiguration(proto::RecordElectionConfigurationRequest {
                configuration: Some(configuration.into()),
            })
        }
    }
}

fn try_replica_set_config(
    configuration: proto::ReplicaSetConfigProto,
) -> Result<ReplicaSetConfig, String> {
    Ok(ReplicaSetConfig {
        members: configuration
            .members
            .into_iter()
            .map(try_replica_info)
            .collect::<Result<Vec<_>, _>>()?,
        write_quorum: configuration.write_quorum,
    })
}

fn try_replica_info(replica: proto::ReplicaInfoProto) -> Result<ReplicaInfo, String> {
    if replica.id <= 0 || replica.instance_id.is_empty() {
        return Err("replica identity is invalid".to_string());
    }
    let role = proto::RoleProto::try_from(replica.role)
        .map_err(|_| format!("unknown replica role {}", replica.role))?;
    if role == proto::RoleProto::RoleUnknown {
        return Err(format!("replica {} has unknown role", replica.id));
    }
    let status = match proto::ReplicaStatusProto::try_from(replica.status)
        .map_err(|_| format!("unknown replica status {}", replica.status))?
    {
        proto::ReplicaStatusProto::StatusUp => ReplicaStatus::Up,
        proto::ReplicaStatusProto::StatusDown => ReplicaStatus::Down,
    };
    Ok(ReplicaInfo {
        id: replica.id,
        instance_id: ReplicaInstanceId::new(replica.instance_id),
        role: role.into(),
        status,
        replicator_address: replica.replicator_address,
        current_progress: replica.current_progress,
        catch_up_capability: replica.catch_up_capability,
        must_catch_up: replica.must_catch_up,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replica_info_round_trip_preserves_incarnation() {
        let original = ReplicaInfo {
            id: 7,
            instance_id: ReplicaInstanceId::new("pod-uid-7"),
            role: Role::ActiveSecondary,
            status: ReplicaStatus::Up,
            replicator_address: "http://127.0.0.1:7007".into(),
            current_progress: 42,
            catch_up_capability: 40,
            must_catch_up: true,
        };

        let round_trip = ReplicaInfo::from(proto::ReplicaInfoProto::from(original.clone()));

        assert_eq!(round_trip.id, original.id);
        assert_eq!(round_trip.instance_id, original.instance_id);
        assert_eq!(round_trip.role, original.role);
        assert_eq!(round_trip.status, original.status);
        assert_eq!(round_trip.replicator_address, original.replicator_address);
        assert_eq!(round_trip.current_progress, original.current_progress);
        assert_eq!(round_trip.catch_up_capability, original.catch_up_capability);
        assert_eq!(round_trip.must_catch_up, original.must_catch_up);
    }

    #[test]
    fn election_evidence_and_data_loss_result_round_trip() {
        let current = ReplicaConfigurationStatus {
            mode: ReplicaConfigurationMode::Current,
            members: vec![ReplicaConfigurationMemberStatus {
                id: 2,
                instance_id: ReplicaInstanceId::new("two"),
                role: Role::ActiveSecondary,
            }],
            write_quorum: 1,
        };
        let configuration = ReplicaElectionConfiguration {
            previous: Some(current.clone()),
            current,
        };
        let round_trip = ReplicaElectionConfiguration::try_from(
            proto::ReplicaElectionConfigurationProto::from(configuration.clone()),
        )
        .unwrap();
        assert_eq!(round_trip, configuration);

        let deactivation = ReplicaDeactivationInfo {
            epoch: Epoch::new(4, 8),
            catch_up_lsn: 17,
        };
        assert_eq!(
            ReplicaDeactivationInfo::try_from(proto::ReplicaDeactivationInfoProto::from(
                deactivation,
            ))
            .unwrap(),
            deactivation
        );

        for result in [
            DurableActionResult::DataLoss(DataLossAction::None),
            DurableActionResult::DataLoss(DataLossAction::StateChanged),
        ] {
            let encoded = proto::DurableActionResultProto::from(result);
            assert_eq!(DurableActionResult::try_from(encoded), Ok(result));
        }
    }

    #[test]
    fn malformed_election_evidence_is_rejected() {
        let missing_current = proto::ReplicaElectionConfigurationProto::default();
        assert!(ReplicaElectionConfiguration::try_from(missing_current).is_err());

        let missing_epoch = proto::ReplicaDeactivationInfoProto {
            epoch: None,
            catch_up_lsn: 1,
        };
        assert!(ReplicaDeactivationInfo::try_from(missing_epoch).is_err());

        let malformed = proto::ReplicaElectionConfigurationProto {
            current: Some(proto::ReplicaConfigurationStatusProto {
                mode: 999,
                members: Vec::new(),
                write_quorum: 0,
            }),
            previous: None,
        };
        assert!(ReplicaElectionConfiguration::try_from(malformed).is_err());
    }

    fn valid_correlated_request() -> CorrelatedControlActionRequest {
        let action = DurableReplicaAction::RevokeWriteStatus;
        CorrelatedControlActionRequest {
            protocol_version: 1,
            action_id: "operation:1".to_string(),
            input_signature: action.signature(),
            target_replica_id: 1,
            target_instance_id: ReplicaInstanceId::new("pod-uid"),
            expected_agent_generation: AgentGeneration::parse("0123456789abcdef0123456789abcdef")
                .unwrap(),
            expected_control_version: AgentControlVersion::new(7),
            observed_runtime_epoch: Epoch::new(2, 3),
            action,
        }
    }

    #[test]
    fn correlated_request_and_observation_round_trip_strictly() {
        let request = valid_correlated_request();
        let round_trip = CorrelatedControlActionRequest::try_from(
            proto::ExecuteCorrelatedControlActionRequest::from(request.clone()),
        )
        .unwrap();
        assert_eq!(round_trip.protocol_version, request.protocol_version);
        assert_eq!(round_trip.action_id, request.action_id);
        assert_eq!(round_trip.input_signature, request.input_signature);
        assert_eq!(round_trip.target_replica_id, request.target_replica_id);
        assert_eq!(round_trip.target_instance_id, request.target_instance_id);
        assert_eq!(
            round_trip.expected_agent_generation,
            request.expected_agent_generation
        );
        assert_eq!(
            round_trip.expected_control_version,
            request.expected_control_version
        );
        assert_eq!(
            round_trip.observed_runtime_epoch,
            request.observed_runtime_epoch
        );
        assert_eq!(round_trip.action.signature(), request.action.signature());

        let observation = CorrelatedActionObservation {
            generation: request.expected_agent_generation,
            control_version: AgentControlVersion::new(8),
            action: DurableActionObservation {
                action_id: request.action_id,
                signature: request.input_signature,
                state: DurableActionState::Completed,
                error_class: None,
                error: None,
                result: Some(DurableActionResult::DataLoss(DataLossAction::StateChanged)),
            },
        };
        let round_trip = CorrelatedActionObservation::try_from(
            proto::CorrelatedActionObservationProto::from(observation.clone()),
        )
        .unwrap();
        assert_eq!(round_trip, observation);

        let failed = CorrelatedActionObservation {
            generation: AgentGeneration::parse("0123456789abcdef0123456789abcdef").unwrap(),
            control_version: AgentControlVersion::new(9),
            action: DurableActionObservation {
                action_id: "operation:failed".to_string(),
                signature: "wait-for-catch-up:Write".to_string(),
                state: DurableActionState::Failed,
                error_class: Some(DurableActionErrorClass::NoWriteQuorum),
                error: Some("no write quorum".to_string()),
                result: None,
            },
        };
        let failed_round_trip = CorrelatedActionObservation::try_from(
            proto::CorrelatedActionObservationProto::from(failed.clone()),
        )
        .unwrap();
        assert_eq!(failed_round_trip, failed);
    }

    #[test]
    fn persisted_correlated_action_payload_round_trips_exact_progress() {
        assert!(decode_correlated_action_payload("éé").is_err());
        assert!(decode_correlated_action_payload("0g").is_err());

        let action = DurableReplicaAction::UpdateCurrentConfiguration {
            current: ReplicaSetConfig {
                members: vec![ReplicaInfo {
                    id: 2,
                    instance_id: ReplicaInstanceId::new("secondary"),
                    role: Role::ActiveSecondary,
                    status: ReplicaStatus::Up,
                    replicator_address: "http://secondary".to_string(),
                    current_progress: 42,
                    catch_up_capability: 17,
                    must_catch_up: true,
                }],
                write_quorum: 2,
            },
        };
        let signature = action.signature();
        let decoded =
            decode_correlated_action_payload(&encode_correlated_action_payload(&action)).unwrap();

        assert_eq!(decoded.signature(), signature);
        let DurableReplicaAction::UpdateCurrentConfiguration { current } = decoded else {
            panic!("wrong decoded action");
        };
        assert_eq!(current.members[0].current_progress, 42);
        assert_eq!(current.members[0].catch_up_capability, 17);
    }

    #[test]
    fn malformed_correlated_safety_fields_are_rejected() {
        let encoded =
            proto::ExecuteCorrelatedControlActionRequest::from(valid_correlated_request());

        let mut missing_version = encoded.clone();
        missing_version.expected_agent_control_version = None;
        assert!(
            CorrelatedControlActionRequest::try_from(missing_version)
                .unwrap_err()
                .contains("control version")
        );

        let mut missing_epoch = encoded.clone();
        missing_epoch.observed_runtime_epoch = None;
        assert!(
            CorrelatedControlActionRequest::try_from(missing_epoch)
                .unwrap_err()
                .contains("runtime epoch")
        );

        let mut malformed_generation = encoded.clone();
        malformed_generation.expected_agent_generation = "not-a-generation".to_string();
        assert!(
            CorrelatedControlActionRequest::try_from(malformed_generation)
                .unwrap_err()
                .contains("lowercase hexadecimal")
        );

        let mut missing_action = encoded;
        missing_action.action = None;
        assert!(
            CorrelatedControlActionRequest::try_from(missing_action)
                .unwrap_err()
                .contains("missing correlated control action")
        );
    }
}
