use std::collections::{HashMap, HashSet};

use tokio::sync::oneshot;

use crate::error::{KubelicateError, Result};
use crate::types::{Lsn, ReplicaId};

/// Tracks pending operations waiting for quorum acknowledgment.
///
/// Supports dual-config quorum: during reconfiguration, an operation must be
/// acknowledged by write quorum from BOTH current and previous configurations.
/// A replica present in both configs counts toward both quorums with a single ACK.
pub struct QuorumTracker {
    /// Pending operations: LSN → PendingOp
    pending: HashMap<Lsn, PendingOp>,
    /// Current configuration member IDs and write quorum
    current_members: HashSet<ReplicaId>,
    current_write_quorum: u32,
    /// Previous configuration (non-empty during reconfiguration)
    previous_members: HashSet<ReplicaId>,
    previous_write_quorum: u32,
    /// Highest committed LSN
    committed_lsn: Lsn,
}

struct PendingOp {
    /// Replicas that have acknowledged this operation
    acked_by: HashSet<ReplicaId>,
    /// Completion channel — sent when quorum is met
    reply: Option<oneshot::Sender<Result<Lsn>>>,
    lsn: Lsn,
}

impl QuorumTracker {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            current_members: HashSet::new(),
            current_write_quorum: 0,
            previous_members: HashSet::new(),
            previous_write_quorum: 0,
            committed_lsn: 0,
        }
    }

    pub fn committed_lsn(&self) -> Lsn {
        self.committed_lsn
    }

    /// Update to dual-config mode (during reconfiguration).
    pub fn set_catch_up_configuration(
        &mut self,
        current_members: HashSet<ReplicaId>,
        current_write_quorum: u32,
        previous_members: HashSet<ReplicaId>,
        previous_write_quorum: u32,
    ) {
        self.current_members = current_members;
        self.current_write_quorum = current_write_quorum;
        self.previous_members = previous_members;
        self.previous_write_quorum = previous_write_quorum;
    }

    /// Update to single-config mode (reconfiguration complete).
    pub fn set_current_configuration(
        &mut self,
        current_members: HashSet<ReplicaId>,
        current_write_quorum: u32,
    ) {
        self.current_members = current_members;
        self.current_write_quorum = current_write_quorum;
        self.previous_members.clear();
        self.previous_write_quorum = 0;
    }

    /// Register a new pending operation. The primary's own ACK is counted
    /// immediately (primary_id).
    pub fn register(
        &mut self,
        lsn: Lsn,
        primary_id: ReplicaId,
        reply: oneshot::Sender<Result<Lsn>>,
    ) {
        let mut acked_by = HashSet::new();
        acked_by.insert(primary_id);

        let mut op = PendingOp {
            acked_by,
            reply: Some(reply),
            lsn,
        };

        // Check if primary alone satisfies quorum (e.g., single replica)
        if self.is_quorum_met(&op.acked_by) {
            self.commit_op(&mut op);
        }

        // Only insert if not already committed
        if op.reply.is_some() {
            self.pending.insert(lsn, op);
        }
    }

    /// Record an ACK from a secondary. If this causes quorum to be met,
    /// the operation is committed and the reply is sent.
    pub fn ack(&mut self, lsn: Lsn, replica_id: ReplicaId) {
        if let Some(op) = self.pending.get_mut(&lsn) {
            op.acked_by.insert(replica_id);
        } else {
            return; // Already committed or unknown LSN
        }

        // Re-check quorum after inserting the ACK (avoids borrow conflict)
        let quorum_met = {
            let op = self.pending.get(&lsn).unwrap();
            self.is_quorum_met(&op.acked_by)
        };

        if quorum_met {
            let mut op = self.pending.remove(&lsn).unwrap();
            self.commit_op(&mut op);
            self.try_commit_pending();
        }
    }

    /// Fail all pending operations (e.g., on role change or close).
    pub fn fail_all(&mut self, error: KubelicateError) {
        for (_, mut op) in self.pending.drain() {
            if let Some(reply) = op.reply.take() {
                let _ = reply.send(Err(match &error {
                    KubelicateError::NotPrimary => KubelicateError::NotPrimary,
                    KubelicateError::Closed => KubelicateError::Closed,
                    _ => KubelicateError::Internal(error.to_string().into()),
                }));
            }
        }
    }

    /// Number of pending (uncommitted) operations.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    fn is_quorum_met(&self, acked_by: &HashSet<ReplicaId>) -> bool {
        let cc_met =
            self.count_acks_in_set(acked_by, &self.current_members) >= self.current_write_quorum;

        if self.previous_members.is_empty() {
            return cc_met;
        }

        let pc_met =
            self.count_acks_in_set(acked_by, &self.previous_members) >= self.previous_write_quorum;

        cc_met && pc_met
    }

    fn count_acks_in_set(
        &self,
        acked_by: &HashSet<ReplicaId>,
        members: &HashSet<ReplicaId>,
    ) -> u32 {
        acked_by.intersection(members).count() as u32
    }

    fn commit_op(&mut self, op: &mut PendingOp) {
        if op.lsn > self.committed_lsn {
            self.committed_lsn = op.lsn;
        }
        if let Some(reply) = op.reply.take() {
            let _ = reply.send(Ok(op.lsn));
        }
    }

    fn try_commit_pending(&mut self) {
        let mut to_remove = Vec::new();
        for (lsn, op) in &self.pending {
            if self.is_quorum_met(&op.acked_by) {
                to_remove.push(*lsn);
            }
        }
        for lsn in to_remove {
            if let Some(mut op) = self.pending.remove(&lsn) {
                self.commit_op(&mut op);
            }
        }
    }
}

impl Default for QuorumTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_single_replica_commits_immediately() {
        let mut tracker = QuorumTracker::new();
        let primary_id = 1;

        // Single replica: write quorum = 1, primary counts
        tracker.set_current_configuration(
            HashSet::from([primary_id]),
            1, // write_quorum
        );

        let (tx, rx) = oneshot::channel();
        tracker.register(1, primary_id, tx);

        // Should commit immediately — primary alone satisfies quorum
        let lsn = rx.await.unwrap().unwrap();
        assert_eq!(lsn, 1);
        assert_eq!(tracker.committed_lsn(), 1);
        assert_eq!(tracker.pending_count(), 0);
    }

    #[tokio::test]
    async fn test_three_replicas_quorum() {
        let mut tracker = QuorumTracker::new();
        let primary_id = 1;

        // 3 replicas: write quorum = 2
        tracker.set_current_configuration(
            HashSet::from([1, 2, 3]),
            2, // write_quorum
        );

        let (tx, rx) = oneshot::channel();
        tracker.register(1, primary_id, tx);

        // Primary ACK alone (quorum=2, have 1) — not committed yet
        assert_eq!(tracker.pending_count(), 1);

        // Secondary 2 ACKs — now quorum met (have 2)
        tracker.ack(1, 2);

        let lsn = rx.await.unwrap().unwrap();
        assert_eq!(lsn, 1);
        assert_eq!(tracker.committed_lsn(), 1);
        assert_eq!(tracker.pending_count(), 0);
    }

    #[tokio::test]
    async fn test_dual_config_quorum() {
        let mut tracker = QuorumTracker::new();
        let primary_id = 1;

        // During reconfiguration: CC = {1, 2, 3} quorum=2, PC = {1, 2} quorum=2
        tracker.set_catch_up_configuration(HashSet::from([1, 2, 3]), 2, HashSet::from([1, 2]), 2);

        let (tx, rx) = oneshot::channel();
        tracker.register(1, primary_id, tx);

        // Primary ACK (CC: 1/2, PC: 1/2) — not enough
        assert_eq!(tracker.pending_count(), 1);

        // Secondary 3 ACKs — CC met (2/2) but PC not (3 not in PC)
        tracker.ack(1, 3);
        assert_eq!(tracker.pending_count(), 1);

        // Secondary 2 ACKs — CC met (3/2), PC met (2/2)
        tracker.ack(1, 2);

        let lsn = rx.await.unwrap().unwrap();
        assert_eq!(lsn, 1);
        assert_eq!(tracker.pending_count(), 0);
    }

    #[tokio::test]
    async fn test_out_of_order_acks() {
        let mut tracker = QuorumTracker::new();
        let primary_id = 1;

        tracker.set_current_configuration(HashSet::from([1, 2, 3]), 2);

        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        tracker.register(1, primary_id, tx1);
        tracker.register(2, primary_id, tx2);

        // ACK LSN=2 first (out of order)
        tracker.ack(2, 2);
        let lsn2 = rx2.await.unwrap().unwrap();
        assert_eq!(lsn2, 2);

        // ACK LSN=1 second
        tracker.ack(1, 2);
        let lsn1 = rx1.await.unwrap().unwrap();
        assert_eq!(lsn1, 1);

        assert_eq!(tracker.committed_lsn(), 2);
    }

    #[tokio::test]
    async fn test_fail_all() {
        let mut tracker = QuorumTracker::new();

        tracker.set_current_configuration(HashSet::from([1, 2, 3]), 2);

        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        tracker.register(1, 1, tx1);
        tracker.register(2, 1, tx2);

        tracker.fail_all(KubelicateError::NotPrimary);

        let result1 = rx1.await.unwrap();
        assert!(matches!(result1, Err(KubelicateError::NotPrimary)));

        let result2 = rx2.await.unwrap();
        assert!(matches!(result2, Err(KubelicateError::NotPrimary)));

        assert_eq!(tracker.pending_count(), 0);
    }
}
