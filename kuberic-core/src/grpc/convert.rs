use crate::proto;
use crate::types::{
    AccessStatus, Epoch, OpenMode, ReplicaConfigurationMemberStatus, ReplicaConfigurationMode,
    ReplicaConfigurationStatus, ReplicaInfo, ReplicaInstanceId, ReplicaSetConfig,
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
}
