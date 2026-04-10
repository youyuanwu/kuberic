use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use crate::error::{KubelicateError, Result};
use crate::events::ReplicateRequest;
use crate::types::{AccessStatus, CancellationToken, FaultType, Lsn, ReplicaId};

// ---------------------------------------------------------------------------
// PartitionState — shared atomics written by replicator, read by handles
// ---------------------------------------------------------------------------

/// Shared partition state. Written by the replicator actor on the control
/// channel, read lock-free by PartitionHandle and StateReplicatorHandle.
///
/// `copy_lsn_map` stores per-replica copy boundary LSNs. Written by
/// `run_build_replica_copy` after snapshotting state, read by the actor
/// at `UpdateCatchUpConfiguration` to replay only ops beyond the copy
/// boundary (matching SF's gap-free build approach).
pub struct PartitionState {
    read_status: AtomicU8,
    write_status: AtomicU8,
    current_progress: AtomicI64,
    catch_up_capability: AtomicI64,
    committed_lsn: AtomicI64,
    copy_lsn_map: Mutex<HashMap<ReplicaId, Lsn>>,
}

impl PartitionState {
    pub fn new() -> Self {
        Self {
            read_status: AtomicU8::new(AccessStatus::NotPrimary as u8),
            write_status: AtomicU8::new(AccessStatus::NotPrimary as u8),
            current_progress: AtomicI64::new(0),
            catch_up_capability: AtomicI64::new(0),
            committed_lsn: AtomicI64::new(0),
            copy_lsn_map: Mutex::new(HashMap::new()),
        }
    }

    // --- Reads (lock-free, used by handles and runtime) ---

    pub fn read_status(&self) -> AccessStatus {
        AccessStatus::from_u8(self.read_status.load(Ordering::Acquire))
    }

    pub fn write_status(&self) -> AccessStatus {
        AccessStatus::from_u8(self.write_status.load(Ordering::Acquire))
    }

    pub fn current_progress(&self) -> Lsn {
        self.current_progress.load(Ordering::Acquire)
    }

    pub fn catch_up_capability(&self) -> Lsn {
        self.catch_up_capability.load(Ordering::Acquire)
    }

    pub fn committed_lsn(&self) -> Lsn {
        self.committed_lsn.load(Ordering::Acquire)
    }

    // --- Writes (called by replicator actor only) ---

    pub fn set_read_status(&self, status: AccessStatus) {
        self.read_status.store(status as u8, Ordering::Release);
    }

    pub fn set_write_status(&self, status: AccessStatus) {
        self.write_status.store(status as u8, Ordering::Release);
    }

    pub fn set_current_progress(&self, lsn: Lsn) {
        self.current_progress.store(lsn, Ordering::Release);
    }

    pub fn set_catch_up_capability(&self, lsn: Lsn) {
        self.catch_up_capability.store(lsn, Ordering::Release);
    }

    pub fn set_committed_lsn(&self, lsn: Lsn) {
        self.committed_lsn.store(lsn, Ordering::Release);
    }

    /// Record the copy snapshot LSN for a replica being built.
    /// Called by `run_build_replica_copy` after collecting state.
    pub fn set_copy_lsn(&self, replica_id: ReplicaId, lsn: Lsn) {
        self.copy_lsn_map.lock().unwrap().insert(replica_id, lsn);
    }

    /// Take (read and remove) the copy snapshot LSN for a replica.
    /// Called by the actor at UpdateCatchUpConfiguration to determine
    /// the precise replay boundary.
    pub fn take_copy_lsn(&self, replica_id: &ReplicaId) -> Option<Lsn> {
        self.copy_lsn_map.lock().unwrap().remove(replica_id)
    }
}

impl Default for PartitionState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// PartitionHandle — user-facing, reads from PartitionState atomics
// ---------------------------------------------------------------------------

/// User-facing partition handle. Reads access status from shared atomics.
pub struct PartitionHandle {
    state: Arc<PartitionState>,
    fault_tx: mpsc::Sender<FaultType>,
}

impl PartitionHandle {
    pub fn new(state: Arc<PartitionState>, fault_tx: mpsc::Sender<FaultType>) -> Self {
        Self { state, fault_tx }
    }

    /// Current read access status. Check before serving read requests.
    pub fn read_status(&self) -> AccessStatus {
        self.state.read_status()
    }

    /// Current write access status. Check before serving write requests.
    pub fn write_status(&self) -> AccessStatus {
        self.state.write_status()
    }

    /// Report a fault to trigger failover or restart.
    pub fn report_fault(&self, fault_type: FaultType) {
        let _ = self.fault_tx.try_send(fault_type);
    }
}

// ---------------------------------------------------------------------------
// StateReplicatorHandle — user-facing, sends to replicator data channel
// ---------------------------------------------------------------------------

/// User-facing write handle. Backed by the replicator's data channel.
/// Includes fast-path access status check to avoid channel round-trip
/// when writes are not allowed.
#[derive(Clone)]
pub struct StateReplicatorHandle {
    data_tx: mpsc::Sender<ReplicateRequest>,
    state: Arc<PartitionState>,
}

impl StateReplicatorHandle {
    pub fn new(data_tx: mpsc::Sender<ReplicateRequest>, state: Arc<PartitionState>) -> Self {
        Self { data_tx, state }
    }

    /// Replicate data to quorum. Returns the assigned LSN.
    ///
    /// Fast-path: checks write status before sending to the replicator.
    /// The replicator also checks internally (authoritative gate).
    pub async fn replicate(&self, data: Bytes, token: CancellationToken) -> Result<Lsn> {
        // Fast-path access status check
        match self.state.write_status() {
            AccessStatus::Granted => {}
            AccessStatus::NotPrimary => return Err(KubelicateError::NotPrimary),
            AccessStatus::NoWriteQuorum => return Err(KubelicateError::NoWriteQuorum),
            AccessStatus::ReconfigurationPending => {
                return Err(KubelicateError::ReconfigurationPending);
            }
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        self.data_tx
            .send(ReplicateRequest {
                data,
                reply: reply_tx,
            })
            .await
            .map_err(|_| KubelicateError::Closed)?;

        tokio::select! {
            result = reply_rx => result.map_err(|_| KubelicateError::Closed)?,
            _ = token.cancelled() => Err(KubelicateError::Cancelled),
        }
    }
}
