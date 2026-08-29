use crate::types::{Epoch, ReplicaId, ReplicaInstanceId, Role};

/// Validation failures returned while reconstructing a driver from a durable
/// stable snapshot and current live replica handles.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecoveryError {
    #[error("stable snapshot has no members")]
    EmptySnapshot,
    #[error("duplicate snapshot replica ID {0}")]
    DuplicateReplicaId(ReplicaId),
    #[error("duplicate snapshot incarnation {0}")]
    DuplicateInstanceId(ReplicaInstanceId),
    #[error("duplicate live handle replica ID {0}")]
    DuplicateHandleId(ReplicaId),
    #[error("duplicate live handle incarnation {0}")]
    DuplicateHandleInstanceId(ReplicaInstanceId),
    #[error("snapshot member {0} has no current live handle")]
    MissingHandle(ReplicaId),
    #[error("live handle {0} is absent from the stable snapshot")]
    ExtraHandle(ReplicaId),
    #[error("handle incarnation mismatch for replica {id}: expected {expected}, current {actual}")]
    HandleInstanceMismatch {
        id: ReplicaId,
        expected: ReplicaInstanceId,
        actual: ReplicaInstanceId,
    },
    #[error("snapshot primary {0} is not a member")]
    PrimaryMissing(ReplicaId),
    #[error("stable snapshot must contain exactly one primary, found {0}")]
    InvalidPrimaryCount(usize),
    #[error("snapshot primary ID {expected} conflicts with role primary {actual}")]
    ConflictingPrimary {
        expected: ReplicaId,
        actual: ReplicaId,
    },
    #[error("replica {id} has unsupported stable role {role:?}")]
    UnsupportedStableRole { id: ReplicaId, role: Role },
    #[error("invalid write quorum {actual}; expected {expected} for {members} members")]
    InvalidWriteQuorum {
        actual: u32,
        expected: u32,
        members: usize,
    },
    #[error("runtime epoch mismatch for replica {id}: expected {expected:?}, got {actual:?}")]
    EpochMismatch {
        id: ReplicaId,
        expected: Epoch,
        actual: Epoch,
    },
    #[error("runtime incarnation mismatch for replica {id}: expected {expected}, got {actual}")]
    RuntimeInstanceMismatch {
        id: ReplicaId,
        expected: ReplicaInstanceId,
        actual: ReplicaInstanceId,
    },
    #[error("runtime role mismatch for replica {id}: expected {expected:?}, got {actual:?}")]
    RuntimeRoleMismatch {
        id: ReplicaId,
        expected: Role,
        actual: Role,
    },
    #[error("driver stable configuration is inconsistent: {0}")]
    InvalidConfiguration(String),
}

#[derive(Debug, thiserror::Error)]
pub enum KubericError {
    /// This replica is not the primary. Redirect client.
    #[error("not primary")]
    NotPrimary,

    /// Primary does not have write quorum. Retry later.
    #[error("no write quorum")]
    NoWriteQuorum,

    /// Reconfiguration in progress. Retry later.
    #[error("reconfiguration pending")]
    ReconfigurationPending,

    /// Operation from a stale epoch was rejected.
    #[error("stale epoch: got {got:?}, current {current:?}")]
    StaleEpoch { got: Epoch, current: Epoch },

    /// Operation was cancelled via CancellationToken.
    #[error("cancelled")]
    Cancelled,

    /// The replica/partition is closed or shutting down.
    #[error("closed")]
    Closed,

    /// Internal error (IO, serialization, etc.)
    #[error(transparent)]
    Internal(#[from] Box<dyn std::error::Error + Send + Sync>),

    /// Stable operator-restart recovery validation failed.
    #[error(transparent)]
    Recovery(#[from] RecoveryError),
}

pub type Result<T> = std::result::Result<T, KubericError>;
