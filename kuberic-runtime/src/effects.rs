use async_trait::async_trait;
use kuberic_protocol::types::{AccessStatus, OperationId, ReplicaRole};

use crate::Result;
use crate::application::OpenMode;
use crate::authority::{AdmittedAuthority, BuildAuthority};
use crate::runtime::RoleTransition;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEffect {
    pub operation_id: OperationId,
    pub sequence: u64,
    pub action: RuntimeEffectAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPostcondition {
    pub authority: BuildAuthority,
    pub last_sequence: u64,
    pub durable_lsn: i64,
    pub completed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEffectResult {
    pub operation_id: OperationId,
    pub sequence: u64,
    pub postcondition: RuntimePostcondition,
}

#[async_trait]
pub trait RuntimeControlPlane: Send {
    async fn next_effect(&mut self) -> Result<Option<RuntimeEffect>>;

    async fn publish(&mut self, result: RuntimeEffectResult) -> Result<()>;
}
