use bytes::Bytes;

pub type CancellationToken = tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Epoch
// ---------------------------------------------------------------------------

/// Configuration version for a partition. Incremented on every reconfiguration.
/// Provides total ordering (data_loss_number first) for epoch-based fencing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Epoch {
    /// Incremented when write quorum is lost (data loss event).
    pub data_loss_number: i64,
    /// Incremented on every reconfiguration.
    pub configuration_number: i64,
}

impl Epoch {
    pub fn new(data_loss_number: i64, configuration_number: i64) -> Self {
        Self {
            data_loss_number,
            configuration_number,
        }
    }
}

impl PartialOrd for Epoch {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Epoch {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.data_loss_number
            .cmp(&other.data_loss_number)
            .then(self.configuration_number.cmp(&other.configuration_number))
    }
}

// ---------------------------------------------------------------------------
// Replica types
// ---------------------------------------------------------------------------

pub type Lsn = i64;
pub type ReplicaId = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Primary,
    ActiveSecondary,
    IdleSecondary,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaStatus {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// New replica — no previous state. Initialize fresh.
    New,
    /// Existing replica — has persisted state from a previous run.
    Existing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataLossAction {
    /// State accepted as-is, no changes.
    None,
    /// State was modified (e.g., restored from backup).
    /// Triggers full rebuild of other replicas.
    StateChanged,
}

// ---------------------------------------------------------------------------
// Replica set configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ReplicaInfo {
    pub id: ReplicaId,
    pub role: Role,
    pub status: ReplicaStatus,
    /// Data plane address (replication streams + copy protocol).
    pub replicator_address: String,
    pub current_progress: Lsn,
    pub catch_up_capability: Lsn,
    /// Whether this replica must catch up as part of WaitForQuorumCatchup.
    /// Exactly one replica (the primary candidate) is marked true during failover.
    pub must_catch_up: bool,
}

#[derive(Debug, Clone)]
pub struct ReplicaSetConfig {
    pub members: Vec<ReplicaInfo>,
    pub write_quorum: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaSetQuorumMode {
    /// Full quorum: all replicas in config must catch up.
    All,
    /// Write quorum: a subset forming write quorum must catch up,
    /// and the subset must include the must_catch_up replica.
    Write,
}

// ---------------------------------------------------------------------------
// Access status (ReadStatus / WriteStatus)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AccessStatus {
    /// Operation is allowed.
    Granted = 0,
    /// Reconfiguration in progress. Retry later.
    ReconfigurationPending = 1,
    /// This replica is not primary. Redirect client.
    NotPrimary = 2,
    /// Primary but no write quorum. Reads may still work.
    NoWriteQuorum = 3,
}

impl AccessStatus {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Granted,
            1 => Self::ReconfigurationPending,
            2 => Self::NotPrimary,
            3 => Self::NoWriteQuorum,
            _ => Self::NotPrimary,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultType {
    /// Restart replica, preserve persisted state.
    Transient,
    /// Drop replica and rebuild from scratch.
    Permanent,
}

// ---------------------------------------------------------------------------
// Replication data
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ReplData {
    pub data: Bytes,
    pub lsn: Lsn,
}

// ---------------------------------------------------------------------------
// Operation + OperationStream (pull-based secondary delivery)
// ---------------------------------------------------------------------------

/// A single replicated operation delivered to the user via an OperationStream.
/// The user must call `acknowledge()` after applying the operation.
///
/// In persisted mode, `acknowledge()` gates quorum — the primary waits for
/// the secondary to acknowledge before completing `replicate()`.
/// In volatile mode, `acknowledge()` only releases memory.
pub struct Operation {
    pub lsn: Lsn,
    pub data: Bytes,
    ack_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Operation {
    pub fn new(lsn: Lsn, data: Bytes, ack_tx: Option<tokio::sync::oneshot::Sender<()>>) -> Self {
        Self { lsn, data, ack_tx }
    }

    /// Acknowledge this operation. Mandatory for persisted replicators —
    /// blocks subsequent ops until called.
    pub fn acknowledge(mut self) {
        if let Some(tx) = self.ack_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl std::fmt::Debug for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Operation")
            .field("lsn", &self.lsn)
            .field("data_len", &self.data.len())
            .field("has_ack", &self.ack_tx.is_some())
            .finish()
    }
}

/// Stream of operations delivered to the user on secondaries.
/// Wraps an mpsc receiver — user calls `get_operation()` in a loop.
pub struct OperationStream {
    rx: tokio::sync::mpsc::Receiver<Operation>,
}

impl OperationStream {
    pub fn new(rx: tokio::sync::mpsc::Receiver<Operation>) -> Self {
        Self { rx }
    }

    /// Returns the next operation, or None when the stream ends.
    pub async fn get_operation(&mut self) -> Option<Operation> {
        self.rx.recv().await
    }

    /// Create a pair (sender, stream) for wiring.
    pub fn channel(buffer: usize) -> (tokio::sync::mpsc::Sender<Operation>, Self) {
        let (tx, rx) = tokio::sync::mpsc::channel(buffer);
        (tx, Self { rx })
    }
}
