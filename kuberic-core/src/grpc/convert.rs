use crate::proto;
use crate::types::{
    AccessStatus, DataLossAction, DurableActionResult, Epoch, OpenMode,
    ReplicaConfigurationMemberStatus, ReplicaConfigurationMode, ReplicaConfigurationStatus,
    ReplicaDeactivationInfo, ReplicaElectionConfiguration, ReplicaInfo, ReplicaInstanceId,
    ReplicaSetConfig, ReplicaSetQuorumMode, ReplicaStatus, Role,
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

impl From<i32> for AccessStatus {
    fn from(value: i32) -> Self {
        match proto::AccessStatusProto::try_from(value)
            .unwrap_or(proto::AccessStatusProto::AccessNotPrimary)
        {
            proto::AccessStatusProto::AccessGranted => Self::Granted,
            proto::AccessStatusProto::AccessReconfigurationPending => Self::ReconfigurationPending,
            proto::AccessStatusProto::AccessNotPrimary => Self::NotPrimary,
            proto::AccessStatusProto::AccessNoWriteQuorum => Self::NoWriteQuorum,
        }
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

impl From<proto::ReplicaConfigurationStatusProto> for ReplicaConfigurationStatus {
    fn from(status: proto::ReplicaConfigurationStatusProto) -> Self {
        Self {
            mode: match proto::ReplicaConfigurationModeProto::try_from(status.mode)
                .unwrap_or(proto::ReplicaConfigurationModeProto::ConfigurationNone)
            {
                proto::ReplicaConfigurationModeProto::ConfigurationCatchUp => {
                    ReplicaConfigurationMode::CatchUp
                }
                proto::ReplicaConfigurationModeProto::ConfigurationCurrent
                | proto::ReplicaConfigurationModeProto::ConfigurationNone => {
                    ReplicaConfigurationMode::Current
                }
            },
            members: status
                .members
                .into_iter()
                .map(|member| ReplicaConfigurationMemberStatus {
                    id: member.id,
                    instance_id: ReplicaInstanceId::new(member.instance_id),
                    role: Role::from(member.role),
                })
                .collect(),
            write_quorum: status.write_quorum,
        }
    }
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
}
