use bytes::Bytes;
use kuberic_protocol::types::{ConfigurationDescriptor, OperationId, ReplicaIdentity};
use kuberic_wire::proto;

use crate::authority::{BuildAuthority, DurableBuildProgress};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildConfiguration {
    Current,
    Bootstrap(ConfigurationDescriptor),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareCopyRequest {
    pub build_id: OperationId,
    pub target: ReplicaIdentity,
    pub configuration: BuildConfiguration,
    pub copy_context: Bytes,
}

#[derive(Debug, Clone)]
pub struct PreparedCopy {
    pub authority: BuildAuthority,
    pub items: Vec<proto::CopyItem>,
}

pub type BuildProgress = DurableBuildProgress;
