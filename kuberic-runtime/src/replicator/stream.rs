//! Shared native receiver operations. Service persistence precedes an explicit,
//! parameterless ACK when the configured receiver requires service ACKs.

use std::pin::Pin;
use std::sync::Arc;

use futures::{Stream, StreamExt};

use crate::Result;
use crate::application::{Lsn, OperationData};
use crate::protocol::types::Epoch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Replication,
    Copy,
}

#[derive(Clone)]
pub struct ReceivedOperation {
    pub kind: OperationKind,
    pub lsn: Lsn,
    pub epoch: Epoch,
    pub data: OperationData,
    pub end_of_stream: bool,
    acknowledge: Arc<dyn Fn() -> Result<()> + Send + Sync>,
}

impl ReceivedOperation {
    pub(crate) fn new(
        kind: OperationKind,
        lsn: Lsn,
        epoch: Epoch,
        data: OperationData,
        end_of_stream: bool,
        acknowledge: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    ) -> Self {
        Self {
            kind,
            lsn,
            epoch,
            data,
            end_of_stream,
            acknowledge,
        }
    }

    /// Does not carry service progress or manufacture an application commit.
    /// Dropping this operation is not an ACK or a rollback.
    pub fn acknowledge(&self) -> Result<()> {
        (self.acknowledge)()
    }
}

pub struct OperationStream {
    stream: Pin<Box<dyn Stream<Item = Result<ReceivedOperation>> + Send>>,
    fault: Arc<dyn Fn(String) -> Result<()> + Send + Sync>,
}

impl OperationStream {
    pub(crate) fn new(
        stream: Pin<Box<dyn Stream<Item = Result<ReceivedOperation>> + Send>>,
        fault: Arc<dyn Fn(String) -> Result<()> + Send + Sync>,
    ) -> Self {
        Self { stream, fault }
    }

    /// Receiver sharing, copy termination and native EOF operations retain
    /// their owner semantics. A new stream handle never rewinds the receiver.
    pub async fn get_operation(&mut self) -> Result<Option<ReceivedOperation>> {
        self.stream.next().await.transpose()
    }

    /// Report failed service application instead of acknowledging success.
    pub fn report_fault(&self, reason: String) -> Result<()> {
        (self.fault)(reason)
    }
}
