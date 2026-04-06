use crate::proto;
use crate::types::{
    Epoch, OpenMode, ReplicaInfo, ReplicaSetConfig, ReplicaSetQuorumMode, ReplicaStatus, Role,
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
            Role::None => proto::RoleProto::RoleNone,
            Role::Primary => proto::RoleProto::RolePrimary,
            Role::ActiveSecondary => proto::RoleProto::RoleActiveSecondary,
            Role::IdleSecondary => proto::RoleProto::RoleIdleSecondary,
        }
    }
}

impl From<proto::RoleProto> for Role {
    fn from(r: proto::RoleProto) -> Self {
        match r {
            proto::RoleProto::RoleNone => Role::None,
            proto::RoleProto::RolePrimary => Role::Primary,
            proto::RoleProto::RoleActiveSecondary => Role::ActiveSecondary,
            proto::RoleProto::RoleIdleSecondary => Role::IdleSecondary,
        }
    }
}

impl From<i32> for Role {
    fn from(v: i32) -> Self {
        proto::RoleProto::try_from(v)
            .unwrap_or(proto::RoleProto::RoleNone)
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

// --- ReplicaInfo ---

impl From<ReplicaInfo> for proto::ReplicaInfoProto {
    fn from(r: ReplicaInfo) -> Self {
        proto::ReplicaInfoProto {
            id: r.id,
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
