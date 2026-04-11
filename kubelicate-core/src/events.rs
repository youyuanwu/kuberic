use bytes::Bytes;
use tokio::sync::oneshot;

use crate::error::Result;
use crate::replicator::{OpenContext, ReplicatorHandle};
use crate::types::{
    DataLossAction, Epoch, Lsn, OpenMode, OperationStream, ReplicaId, ReplicaInfo,
    ReplicaSetConfig, ReplicaSetQuorumMode, Role,
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
    /// Replica opened. Create replicator, return handle to runtime.
    ///
    /// The user creates the replicator (e.g., `WalReplicator::create()`),
    /// keeps the user-facing handles, and returns the `ReplicatorHandle`
    /// to the runtime.
    Open {
        ctx: OpenContext,
        reply: oneshot::Sender<Result<ReplicatorHandle>>,
    },

    /// Role changed. Start/stop background work accordingly.
    ///
    /// Reply with the new listening address (e.g., "http://0.0.0.0:8080")
    /// or empty string if not listening in this role.
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
/// Role-specific.
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
// ServiceContext — user-facing handles, defined in replicator::ServiceContext
// ---------------------------------------------------------------------------

// Users receive OpenContext in the Open event, create a replicator,
// and keep ServiceContext (partition, replicator, streams, state_provider_rx).
// The token comes from OpenContext.token.

// ---------------------------------------------------------------------------
// Channel bundle — REMOVED (internal to WalReplicator)
// ---------------------------------------------------------------------------

// ReplicatorChannels is no longer needed in the public API.
// Channel creation is internal to WalReplicator::create().
