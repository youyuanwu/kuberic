use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use kuberic_protocol::types::ReplicaRole;

use crate::Result;

pub type Lsn = i64;
pub type OperationStream = Pin<Box<dyn Stream<Item = Result<Operation>> + Send>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub lsn: Lsn,
    pub committed_lsn: Lsn,
    pub data: Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DurableApplicationAck {
    pub applied_lsn: Lsn,
    pub committed_lsn: Lsn,
}

pub type DurableApplicationProgress = DurableApplicationAck;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientWrite {
    pub data: Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteReceipt {
    pub lsn: Lsn,
    pub committed_lsn: Lsn,
}

#[async_trait]
pub trait StatefulApplication: Send + Sync {
    async fn open(&self) -> Result<()>;

    async fn change_role(&self, role: ReplicaRole) -> Result<()>;

    async fn apply(&self, operation: Operation) -> Result<DurableApplicationAck>;

    async fn durable_progress(&self) -> Result<DurableApplicationProgress>;

    async fn verify_applied(&self, operation: &Operation) -> Result<bool>;

    async fn commit(&self, committed_lsn: Lsn) -> Result<DurableApplicationProgress>;

    async fn copy_operations(&self, from_lsn: Lsn) -> Result<OperationStream>;

    async fn close(&self) -> Result<()>;
}
