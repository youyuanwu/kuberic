use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use crate::error::Result;
use crate::handles::{PartitionHandle, StateReplicatorHandle};
use crate::types::{
    CancellationToken, DataLossAction, Epoch, Lsn, OpenMode, OperationStream, ReplicaId,
    ReplicaInfo, ReplicaSetConfig, ReplicaSetQuorumMode, Role,
};

// ---------------------------------------------------------------------------
// Replicator control events (system-internal, operator → replicator actor)
// ---------------------------------------------------------------------------

/// Control events delivered to the replicator actor by the runtime.
/// Processed sequentially on the control channel.
pub enum ReplicatorControlEvent {
    Open {
        mode: OpenMode,
        reply: oneshot::Sender<Result<()>>,
    },
    Close {
        reply: oneshot::Sender<Result<()>>,
    },
    Abort,

    ChangeRole {
        epoch: Epoch,
        role: Role,
        reply: oneshot::Sender<Result<()>>,
    },
    UpdateEpoch {
        epoch: Epoch,
        reply: oneshot::Sender<Result<()>>,
    },

    // Primary-only reconfiguration commands
    OnDataLoss {
        reply: oneshot::Sender<Result<DataLossAction>>,
    },
    UpdateCatchUpConfiguration {
        current: ReplicaSetConfig,
        previous: ReplicaSetConfig,
        reply: oneshot::Sender<Result<()>>,
    },
    UpdateCurrentConfiguration {
        current: ReplicaSetConfig,
        reply: oneshot::Sender<Result<()>>,
    },
    WaitForCatchUpQuorum {
        mode: ReplicaSetQuorumMode,
        reply: oneshot::Sender<Result<()>>,
    },
    BuildReplica {
        replica: ReplicaInfo,
        reply: oneshot::Sender<Result<()>>,
    },
    RemoveReplica {
        replica_id: ReplicaId,
        reply: oneshot::Sender<Result<()>>,
    },
}

// ---------------------------------------------------------------------------
// Replicator data events (user → replicator actor, high-throughput path)
// ---------------------------------------------------------------------------

/// Request to replicate data to quorum. Sent by StateReplicatorHandle.
pub struct ReplicateRequest {
    pub data: Bytes,
    pub reply: oneshot::Sender<Result<Lsn>>,
}

// ---------------------------------------------------------------------------
// Lifecycle events (runtime → user, SF's IStatefulServiceReplica)
// ---------------------------------------------------------------------------

/// Lifecycle events delivered on the lifecycle channel.
/// Rare but high-priority — handle immediately (blocks reconfiguration).
pub enum LifecycleEvent {
    /// Replica opened. Initialize state, store handles.
    Open {
        ctx: ServiceContext,
        reply: oneshot::Sender<Result<()>>,
    },

    /// Role changed. Start/stop background work accordingly.
    ///
    /// Reply with the new listening address (e.g., "http://0.0.0.0:8080")
    /// or empty string if not listening in this role.
    ///
    /// Ordering guarantee: the runtime calls the replicator's ChangeRole
    /// BEFORE delivering this event on promotion (S→P), and AFTER on
    /// demotion (P→S).
    ChangeRole {
        new_role: Role,
        reply: oneshot::Sender<Result<String>>,
    },

    /// Graceful shutdown. Drain in-flight work, flush state, release
    /// resources. The runtime calls the replicator's Close after this returns.
    Close { reply: oneshot::Sender<Result<()>> },

    /// Ungraceful termination. Release resources best-effort and return
    /// immediately. No reply channel — the runtime doesn't wait.
    Abort,
}

// ---------------------------------------------------------------------------
// State provider events (runtime/replicator → user, SF's IStateProvider)
// ---------------------------------------------------------------------------

/// State provider callbacks delivered on the state_provider channel.
/// Role-specific. May involve heavy work (e.g., copy state production).
pub enum StateProviderEvent {
    /// Epoch changed (secondaries only).
    /// Primary gets epoch via ChangeRole, not UpdateEpoch.
    /// Operations with LSN above `previous_epoch_last_lsn` from the old
    /// epoch may be stale (uncommitted zombie primary writes).
    UpdateEpoch {
        epoch: Epoch,
        previous_epoch_last_lsn: Lsn,
        reply: oneshot::Sender<Result<()>>,
    },

    /// "What's your last applied LSN?" (secondary, during build/catchup)
    GetLastCommittedLsn { reply: oneshot::Sender<Result<Lsn>> },

    /// "What state do you already have?" (new idle secondary, during build)
    GetCopyContext {
        reply: oneshot::Sender<Result<OperationStream>>,
    },

    /// "Produce state for this secondary" (primary, during build_replica)
    GetCopyState {
        up_to_lsn: Lsn,
        copy_context: OperationStream,
        reply: oneshot::Sender<Result<OperationStream>>,
    },

    /// "Quorum was lost, data loss possible" (new primary after quorum loss)
    OnDataLoss {
        reply: oneshot::Sender<Result<bool>>,
    },
}

// ---------------------------------------------------------------------------
// ServiceEvent — kept as a backward-compatible union for transition
// (will be removed once all consumers switch to dual-channel)
// ---------------------------------------------------------------------------

/// Legacy combined event type. Prefer LifecycleEvent + StateProviderEvent.
pub enum ServiceEvent {
    Open {
        ctx: ServiceContext,
        reply: oneshot::Sender<Result<()>>,
    },
    ChangeRole {
        new_role: Role,
        reply: oneshot::Sender<Result<String>>,
    },
    Close {
        reply: oneshot::Sender<Result<()>>,
    },
    Abort,
}

/// Handles provided to the user at Open time.
pub struct ServiceContext {
    /// Query read/write access status, report faults.
    pub partition: Arc<PartitionHandle>,
    /// Replicate writes to quorum (usable on primary only).
    pub replicator: StateReplicatorHandle,
    /// Copy stream — secondary pulls full-state operations during build.
    /// None on primary.
    pub copy_stream: Option<OperationStream>,
    /// Replication stream — secondary pulls incremental operations.
    /// None on primary.
    pub replication_stream: Option<OperationStream>,
    /// Cancellation token for the replica's lifetime.
    /// Cancelled when close or abort is triggered.
    pub token: CancellationToken,
}

// ---------------------------------------------------------------------------
// Channel bundle for wiring
// ---------------------------------------------------------------------------

/// All channels needed to wire the replicator actor, runtime, and user service.
pub struct ReplicatorChannels {
    pub control_tx: mpsc::Sender<ReplicatorControlEvent>,
    pub control_rx: mpsc::Receiver<ReplicatorControlEvent>,
    pub data_tx: mpsc::Sender<ReplicateRequest>,
    pub data_rx: mpsc::Receiver<ReplicateRequest>,
}

impl ReplicatorChannels {
    pub fn new(control_buffer: usize, data_buffer: usize) -> Self {
        let (control_tx, control_rx) = mpsc::channel(control_buffer);
        let (data_tx, data_rx) = mpsc::channel(data_buffer);
        Self {
            control_tx,
            control_rx,
            data_tx,
            data_rx,
        }
    }
}
