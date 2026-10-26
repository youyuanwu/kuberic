use std::pin::Pin;

use crate::protocol::types::{OperationId, ReplicaIdentity};
use bytes::Bytes;
use futures::Stream;

use crate::Result;
use crate::replicator::StatefulServicePartition;

#[doc(inline)]
pub use crate::protocol::types::OpenMode;
pub use crate::traits::{StateProvider, StatefulServiceReplica};

/// Logical sequence number identifying an operation's position in replicated history.
pub type Lsn = i64;
/// Public sentinel for omitted, unknown, or inapplicable progress.
///
/// In SF configuration input, progress may be omitted for a replica already
/// configured or built: use the replicator's known progress for that exact
/// replica, not an empty-history boundary. This does not authorize reuse of
/// progress across different process sessions or storage incarnations.
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

impl OpenContext {
    /// One-way Existing-mode startup instructions, not native initialization,
    /// membership, readiness or access authority.
    pub fn restoring_retained_member(&self) -> bool {
        self.mode == OpenMode::Existing && self.partition.restoring_retained_member()
    }
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
