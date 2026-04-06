use crate::types::Epoch;

#[derive(Debug, thiserror::Error)]
pub enum KubelicateError {
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
}

pub type Result<T> = std::result::Result<T, KubelicateError>;
