use std::pin::Pin;

use futures::Stream;
use kuberic_protocol::types::{ConfigurationDescriptor, OperationId, ReplicaIdentity};
use kuberic_wire::proto;

use crate::Result;
use crate::application::OperationDataStream;
use crate::authority::{BuildAuthority, DurableBuildProgress};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildConfiguration {
    Current,
    Bootstrap(ConfigurationDescriptor),
}

pub struct PrepareCopyRequest {
    pub build_id: OperationId,
    pub target: ReplicaIdentity,
    pub configuration: BuildConfiguration,
    pub copy_context: OperationDataStream,
}

pub struct PreparedCopy {
    pub authority: BuildAuthority,
    pub items: Pin<Box<dyn Stream<Item = Result<proto::CopyItem>> + Send>>,
}

pub type BuildProgress = DurableBuildProgress;
