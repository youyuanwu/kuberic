use std::num::NonZeroU64;

use crate::{CheckpointEnvelope, ExecutionId};

/// Opaque compare-and-swap revision for one execution's checkpoint.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct StorageRevision(NonZeroU64);

impl StorageRevision {
    /// Construct a store-owned revision token without exposing its representation
    /// to host logic.
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    pub(crate) const fn initial() -> Self {
        Self(NonZeroU64::MIN)
    }

    pub(crate) fn next(self) -> Self {
        Self(
            NonZeroU64::new(
                self.0
                    .get()
                    .checked_add(1)
                    .expect("in-memory checkpoint revision exhausted"),
            )
            .expect("incrementing a nonzero revision cannot produce zero"),
        )
    }
}

/// A checkpoint and the storage revision at which it was loaded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredCheckpoint {
    revision: StorageRevision,
    checkpoint: CheckpointEnvelope,
}

impl StoredCheckpoint {
    pub const fn new(revision: StorageRevision, checkpoint: CheckpointEnvelope) -> Self {
        Self {
            revision,
            checkpoint,
        }
    }

    pub const fn revision(&self) -> StorageRevision {
        self.revision
    }

    pub const fn checkpoint(&self) -> &CheckpointEnvelope {
        &self.checkpoint
    }
}

/// Result of one atomic checkpoint compare-and-swap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompareAndSwap {
    Accepted(StorageRevision),
    Conflict,
    RejectedBeforeApply,
    ResponseLostAfterApply,
}

/// Synchronous persistence boundary used by the experimental host.
pub trait CheckpointStore {
    fn load(&self, execution_id: ExecutionId) -> Option<StoredCheckpoint>;

    fn compare_and_swap(
        &self,
        execution_id: ExecutionId,
        expected: Option<StorageRevision>,
        checkpoint: CheckpointEnvelope,
    ) -> CompareAndSwap;
}
