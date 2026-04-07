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
    /// Replicas marked as must_catch_up (from current config)
    must_catch_up_ids: HashSet<ReplicaId>,
    /// Per-replica highest ACKed LSN (for must_catch_up enforcement)
    replica_acked_lsn: HashMap<ReplicaId, Lsn>,
    /// Highest committed LSN
    committed_lsn: Lsn,
    /// Highest registered LSN (current progress)
    highest_lsn: Lsn,
    /// Waiters for catch-up quorum
    catch_up_waiters: Vec<CatchUpWaiter>,
}

struct CatchUpWaiter {
    mode: crate::types::ReplicaSetQuorumMode,
    reply: oneshot::Sender<Result<()>>,
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
            must_catch_up_ids: HashSet::new(),
            replica_acked_lsn: HashMap::new(),
            committed_lsn: 0,
            highest_lsn: 0,
            catch_up_waiters: Vec::new(),
        }
    }

    pub fn committed_lsn(&self) -> Lsn {
        self.committed_lsn
    }

    /// Update to dual-config mode (during reconfiguration).
    /// Extracts `must_catch_up` replica IDs from the current config members.
    pub fn set_catch_up_configuration(
        &mut self,
        current_members: HashSet<ReplicaId>,
        current_write_quorum: u32,
        previous_members: HashSet<ReplicaId>,
        previous_write_quorum: u32,
        must_catch_up_ids: HashSet<ReplicaId>,
    ) {
        self.current_members = current_members;
        self.current_write_quorum = current_write_quorum;
        self.previous_members = previous_members;
        self.previous_write_quorum = previous_write_quorum;
        self.must_catch_up_ids = must_catch_up_ids;
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
        self.must_catch_up_ids.clear();
    }

    /// Register a new pending operation. The primary's own ACK is counted
    /// immediately (primary_id).
    pub fn register(
        &mut self,
        lsn: Lsn,
        primary_id: ReplicaId,
        reply: oneshot::Sender<Result<Lsn>>,
    ) {
        if lsn > self.highest_lsn {
            self.highest_lsn = lsn;
        }

        let mut acked_by = HashSet::new();
        acked_by.insert(primary_id);

        // Track primary's own ACK progress
        self.replica_acked_lsn
            .entry(primary_id)
            .and_modify(|v| {
                if lsn > *v {
                    *v = lsn;
                }
            })
            .or_insert(lsn);

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
        } else {
            self.notify_catch_up_waiters();
        }
    }

    /// Record an ACK from a secondary. If this causes quorum to be met,
    /// the operation is committed and the reply is sent.
    pub fn ack(&mut self, lsn: Lsn, replica_id: ReplicaId) {
        // Track per-replica progress
        self.replica_acked_lsn
            .entry(replica_id)
            .and_modify(|v| {
                if lsn > *v {
                    *v = lsn;
                }
            })
            .or_insert(lsn);

        if let Some(op) = self.pending.get_mut(&lsn) {
            op.acked_by.insert(replica_id);
        } else {
            // Already committed or unknown LSN — still notify waiters
            // since the must_catch_up replica might have just caught up
            self.notify_catch_up_waiters();
            return;
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
            self.notify_catch_up_waiters();
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
        // Also fail any catch-up waiters
        for waiter in self.catch_up_waiters.drain(..) {
            let _ = waiter.reply.send(Err(match &error {
                KubelicateError::NotPrimary => KubelicateError::NotPrimary,
                KubelicateError::Closed => KubelicateError::Closed,
                _ => KubelicateError::Internal(error.to_string().into()),
            }));
        }
    }

    /// Number of pending (uncommitted) operations.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Register a waiter that will be notified when catch-up is complete.
    ///
    /// - `All` mode: all pending ops committed (quorum met for each).
    /// - `Write` mode: all pending ops committed AND every `must_catch_up`
    ///   replica has individually ACKed all ops.
    ///
    /// If already caught up, replies immediately.
    pub fn wait_for_catch_up(
        &mut self,
        mode: crate::types::ReplicaSetQuorumMode,
        reply: oneshot::Sender<Result<()>>,
    ) {
        if self.is_caught_up(mode) {
            let _ = reply.send(Ok(()));
        } else {
            self.catch_up_waiters.push(CatchUpWaiter { mode, reply });
        }
    }

    fn is_caught_up(&self, mode: crate::types::ReplicaSetQuorumMode) -> bool {
        if !self.pending.is_empty() {
            return false;
        }
        match mode {
            crate::types::ReplicaSetQuorumMode::Write => {
                // Write mode: every must_catch_up replica must have individually
                // ACKed up to highest_lsn
                for &id in &self.must_catch_up_ids {
                    let acked = self.replica_acked_lsn.get(&id).copied().unwrap_or(0);
                    if acked < self.highest_lsn {
                        return false;
                    }
                }
            }
            crate::types::ReplicaSetQuorumMode::All => {
                // All mode: every member in current config must have ACKed
                // up to highest_lsn (brute-force fallback)
                for &id in &self.current_members {
                    let acked = self.replica_acked_lsn.get(&id).copied().unwrap_or(0);
                    if acked < self.highest_lsn {
                        return false;
                    }
                }
            }
        }
        true
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

    fn notify_catch_up_waiters(&mut self) {
        if self.catch_up_waiters.is_empty() {
            return;
        }
        let waiters = std::mem::take(&mut self.catch_up_waiters);
        for waiter in waiters {
            if self.is_caught_up(waiter.mode) {
                let _ = waiter.reply.send(Ok(()));
            } else {
                self.catch_up_waiters.push(waiter);
            }
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
        tracker.set_catch_up_configuration(
            HashSet::from([1, 2, 3]),
            2,
            HashSet::from([1, 2]),
            2,
            HashSet::new(), // no must_catch_up in this test
        );

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

    #[tokio::test]
    async fn test_must_catch_up_enforcement() {
        use crate::types::ReplicaSetQuorumMode;

        let mut tracker = QuorumTracker::new();

        // 3 replicas: write quorum = 2, replica 2 is must_catch_up
        tracker.set_catch_up_configuration(
            HashSet::from([1, 2, 3]),
            2,
            HashSet::new(),
            0,
            HashSet::from([2]), // replica 2 must catch up
        );

        let (tx, rx) = oneshot::channel();
        tracker.register(1, 1, tx);

        // Secondary 3 ACKs → quorum met (1+3=2), op committed
        tracker.ack(1, 3);
        let lsn = rx.await.unwrap().unwrap();
        assert_eq!(lsn, 1);
        assert_eq!(tracker.pending_count(), 0);

        // Now wait_for_catch_up(Write): pending==0, but replica 2
        // hasn't ACKed LSN 1 yet
        let (wait_tx, mut wait_rx) = oneshot::channel();
        tracker.wait_for_catch_up(ReplicaSetQuorumMode::Write, wait_tx);

        // Should NOT have fired yet — must_catch_up replica 2 is behind
        assert!(wait_rx.try_recv().is_err());

        // Replica 2 finally ACKs LSN 1 (late ACK for already-committed op)
        tracker.ack(1, 2);

        // Now the waiter should fire
        let result = wait_rx.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_wait_catch_up_all_mode() {
        use crate::types::ReplicaSetQuorumMode;

        let mut tracker = QuorumTracker::new();
        tracker.set_current_configuration(HashSet::from([1, 2, 3]), 2);

        let (tx, _rx) = oneshot::channel();
        tracker.register(1, 1, tx);

        // Register a wait_for_catch_up(All) — pending > 0
        let (wait_tx, mut wait_rx) = oneshot::channel();
        tracker.wait_for_catch_up(ReplicaSetQuorumMode::All, wait_tx);
        assert!(wait_rx.try_recv().is_err());

        // Replica 2 ACKs → quorum met, op committed, but All requires ALL members
        tracker.ack(1, 2);
        // pending==0, but replica 3 hasn't ACKed yet
        assert!(wait_rx.try_recv().is_err());

        // Replica 3 ACKs → all members caught up
        tracker.ack(1, 3);
        let result = wait_rx.await.unwrap();
        assert!(result.is_ok());
    }
}
