use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use kuberic_protocol::types::{Epoch, OperationId, ReplicaIdentity, ReplicaRole};

use crate::Result;

pub type Lsn = i64;
pub type OperationStream = Pin<Box<dyn Stream<Item = Result<Operation>> + Send>>;
pub type CopyStream = Pin<Box<dyn Stream<Item = Result<CopyChunk>> + Send>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    New,
    Existing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenContext {
    pub identity: ReplicaIdentity,
    pub mode: OpenMode,
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
    async fn open(&self, context: OpenContext) -> Result<()>;

    async fn change_role(&self, role: ReplicaRole) -> Result<RoleChange>;

    async fn close(&self) -> Result<()>;

    fn abort(&self);
}

#[async_trait]
pub trait StateProvider: Send + Sync {
    async fn update_epoch(&self, epoch: Epoch, previous_epoch_last_lsn: Lsn) -> Result<()>;

    async fn last_committed_lsn(&self) -> Result<Lsn>;

    async fn get_copy_context(&self) -> Result<Bytes>;

    async fn get_copy_state(&self, up_to_lsn: Lsn, copy_context: Bytes) -> Result<CopyStream>;

    async fn get_replication_operations(
        &self,
        from_lsn: Lsn,
        to_lsn: Lsn,
    ) -> Result<OperationStream>;

    async fn apply_copy_chunk(
        &self,
        build_id: &OperationId,
        sequence: u64,
        chunk: CopyChunk,
    ) -> Result<()>;

    async fn finish_copy(
        &self,
        build_id: &OperationId,
        up_to_lsn: Lsn,
        committed_lsn: Lsn,
    ) -> Result<DurableApplicationProgress>;

    async fn on_data_loss(&self) -> Result<bool>;

    async fn apply(&self, operation: Operation) -> Result<DurableApplicationAck>;

    async fn durable_progress(&self) -> Result<DurableApplicationProgress>;

    async fn verify_applied(&self, operation: &Operation) -> Result<bool>;

    async fn commit(&self, committed_lsn: Lsn) -> Result<DurableApplicationProgress>;
}

pub trait StatefulApplication: StatefulServiceReplica + StateProvider {}

impl<T> StatefulApplication for T where T: StatefulServiceReplica + StateProvider {}
