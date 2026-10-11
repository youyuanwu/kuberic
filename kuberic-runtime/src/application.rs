use std::pin::Pin;
use std::sync::Arc;

use crate::protocol::types::{Epoch, OperationId, ReplicaIdentity, ReplicaRole};
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::replicator::{Replicator, StatefulServicePartition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenMode {
    New,
    Existing,
}

pub type Lsn = i64;
/// Public sentinel for progress that is not yet known or does not apply.
///
/// Zero is a valid empty-history boundary. Values below `INVALID_LSN` are
/// invalid and `INVALID_LSN` must never be reported as completed progress.
pub const INVALID_LSN: Lsn = crate::protocol::public_operations::INVALID_LSN;
pub type OperationData = Bytes;
pub type OperationDataStream = Pin<Box<dyn Stream<Item = Result<OperationData>> + Send>>;

#[derive(Clone)]
pub struct OpenContext {
    pub identity: ReplicaIdentity,
    pub mode: OpenMode,
    pub partition: StatefulServicePartition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleChange {
    pub service_address: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub lsn: Lsn,
    pub committed_lsn: Lsn,
    pub data: Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyChunk {
    pub data: Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DurableApplicationProgress {
    pub applied_lsn: Lsn,
    pub committed_lsn: Lsn,
}

pub type DurableApplicationAck = DurableApplicationProgress;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientWrite {
    pub operation_id: OperationId,
    pub data: Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteReceipt {
    pub lsn: Lsn,
    pub committed_lsn: Lsn,
}

#[async_trait]
pub trait StatefulServiceReplica: Send + Sync {
    async fn open(self: Arc<Self>, context: OpenContext) -> Result<Arc<dyn Replicator>>;

    async fn change_role(&self, role: ReplicaRole) -> Result<RoleChange>;

    async fn close(&self) -> Result<()>;

    fn abort(&self);
}

#[async_trait]
pub trait StateProvider: Send + Sync {
    /// Advances the provider epoch with the exact durable tail of the previous
    /// epoch. Recovery callers must not substitute an observed or applied-only
    /// position for `previous_epoch_last_lsn`.
    async fn update_epoch(&self, epoch: Epoch, previous_epoch_last_lsn: Lsn) -> Result<()>;

    /// Returns provider-owned committed progress. Applied and committed
    /// progress remain distinct even when an implementation usually advances
    /// them together.
    async fn last_committed_lsn(&self) -> Result<Lsn>;

    async fn get_copy_context(&self) -> Result<OperationDataStream>;

    /// Returns copy bytes frozen at the committed `up_to_lsn`. Repeating an
    /// authorized build, including after restart, must reproduce identical
    /// bytes from the same frozen boundary while later replication remains
    /// ordered as retained catch-up.
    async fn get_copy_state(
        &self,
        up_to_lsn: Lsn,
        copy_context: OperationDataStream,
    ) -> Result<OperationDataStream>;

    async fn on_data_loss(&self) -> Result<bool>;
}
