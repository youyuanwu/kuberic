use std::collections::BTreeMap;

use bytes::Bytes;

use crate::types::Lsn;

/// In-memory replication queue. Retains ops from first_lsn to highest_lsn.
/// Ops are GC'd when all configured secondaries have ACKed them.
///
/// This serves the same role as SF's ReplicationQueueManager — it bridges
/// the gap between the copy stream (committed state) and live replication
/// (new ops) by retaining in-flight ops that can be replayed to new replicas.
pub struct ReplicationQueue {
    ops: BTreeMap<Lsn, Bytes>,
}

impl ReplicationQueue {
    pub fn new() -> Self {
        Self {
            ops: BTreeMap::new(),
        }
    }

    /// Append a new op. Called from the data path on every replicate.
    pub fn push(&mut self, lsn: Lsn, data: Bytes) {
        self.ops.insert(lsn, data);
    }

    /// Get all ops from `from_lsn` onward (inclusive).
    /// Used at add_secondary time to replay pending ops to a new replica.
    pub fn ops_from(&self, from_lsn: Lsn) -> Vec<(Lsn, Bytes)> {
        self.ops
            .range(from_lsn..)
            .map(|(&lsn, data)| (lsn, data.clone()))
            .collect()
    }

    /// Remove all ops with LSN <= `acked_lsn`.
    /// Called when the minimum ACKed LSN across all secondaries advances.
    pub fn gc(&mut self, acked_lsn: Lsn) {
        // split_off returns everything >= acked_lsn + 1, we keep that
        let keep = self.ops.split_off(&(acked_lsn + 1));
        self.ops = keep;
    }

    /// Number of ops retained in the queue.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Clear all ops (on role change / close).
    pub fn clear(&mut self) {
        self.ops.clear();
    }
}

impl Default for ReplicationQueue {
    fn default() -> Self {
        Self::new()
    }
}
