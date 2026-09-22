use kuberic_protocol::types::{AccessStatus, OperationId, ReplicaIdentity, ReplicaRole};
use serde::{Deserialize, Serialize};

use crate::authority::{AdmittedAuthority, BuildAuthority};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenMode {
    New,
    Existing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleTransition {
    pub completed_role: ReplicaRole,
    pub target_role: ReplicaRole,
    pub replicator_completed: bool,
    pub application_completed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEffect {
    pub operation_id: OperationId,
    pub sequence: u64,
    pub action: RuntimeEffectAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeEffectAction {
    Open(OpenMode),
    AdmitAuthority(Box<AdmittedAuthority>),
    AdmitBuildAuthority(Box<BuildAuthority>),
    ChangeRole(ReplicaRole),
    SetWriteStatus(AccessStatus),
    RefreshApplicationProgress,
    RetireBuild(OperationId),
    Close,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildPostcondition {
    pub authority: BuildAuthority,
    pub last_sequence: u64,
    pub durable_lsn: i64,
    pub completed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub identity: ReplicaIdentity,
    pub open: bool,
    pub replication_address: Option<String>,
    pub role: ReplicaRole,
    pub role_transition: Option<RoleTransition>,
    pub write_status: AccessStatus,
    pub authority: Option<AdmittedAuthority>,
    pub current_progress: i64,
    pub verified_replication_lsn: Option<i64>,
    pub committed_lsn: i64,
    pub current_configuration_quorum_progress: i64,
    pub catch_up_boundary: Option<i64>,
    pub catch_up_complete: bool,
    pub builds: Vec<BuildPostcondition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimePostcondition {
    pub open: bool,
    pub role: ReplicaRole,
    pub role_transition: Option<RoleTransition>,
    pub write_status: AccessStatus,
    pub authority: Option<AdmittedAuthority>,
    pub current_progress: i64,
    pub verified_replication_lsn: Option<i64>,
    pub committed_lsn: i64,
    pub current_configuration_quorum_progress: i64,
    pub catch_up_boundary: Option<i64>,
    pub catch_up_complete: bool,
    pub builds: Vec<BuildPostcondition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEffectResult {
    pub operation_id: OperationId,
    pub sequence: u64,
    pub postcondition: RuntimePostcondition,
}
