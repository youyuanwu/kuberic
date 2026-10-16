use std::pin::Pin;

use crate::protocol::types::{OperationId, ReplicaIdentity};
use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::replicator::StatefulServicePartition;

pub use crate::traits::{StateProvider, StatefulServiceReplica};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenMode {
    New,
    Existing,
}

/// Logical sequence number identifying an operation's position in replicated history.
pub type Lsn = i64;
/// Public sentinel for progress that is not yet known or does not apply.
///
/// Zero is a valid empty-history boundary. Values below `INVALID_LSN` are
/// invalid and `INVALID_LSN` must never be reported as completed progress.
pub const INVALID_LSN: Lsn = crate::protocol::public_operations::INVALID_LSN;
/// Opaque bytes representing a state change, copy state, or copy context.
pub type OperationData = Bytes;
/// Asynchronous sequence of provider-defined operation data.
///
/// Stream completion (`None`) marks the end of the transfer. An error item
/// reports a failed transfer rather than successful completion.
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
