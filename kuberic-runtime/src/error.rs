use crate::protocol::types::AccessStatus;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("runtime is closed")]
    Closed,
    #[error("runtime is not open")]
    NotOpen,
    #[error("runtime is not primary")]
    NotPrimary,
    #[error("operation was cancelled")]
    OperationCancelled,
    #[error("replica {0} was removed")]
    ReplicaRemoved(i64),
    #[error("runtime reconfiguration is still pending")]
    ReconfigurationPending,
    #[error("bounded operation queue is full")]
    QueueFull,
    #[error("writes are closed with status {0:?}")]
    WriteClosed(AccessStatus),
    #[error("reads are closed with status {0:?}")]
    ReadClosed(AccessStatus),
    #[error("durable authority has not been admitted")]
    AuthorityNotAdmitted,
    #[error("authority mismatch: {0}")]
    AuthorityMismatch(String),
    #[error("invalid replication evidence: {0}")]
    InvalidReplication(String),
    #[error("application error: {0}")]
    Application(String),
    #[error("control-plane error: {0}")]
    ControlPlane(String),
    #[error("effect sequence {observed} is out of order; expected {expected}")]
    EffectOutOfOrder { expected: u64, observed: u64 },
    #[error("effect sequence {sequence} was reused with a different operation ID")]
    EffectConflict { sequence: u64 },
    #[error("write completion channel closed")]
    WriteCompletionClosed,
    #[error("local write {0} must be resolved before another write can be assigned")]
    LocalWritePending(String),
    #[error("local write was fenced by data-loss recovery")]
    DataLossFenced,
    #[cfg(feature = "host")]
    #[error("agent store is uninitialized")]
    Uninitialized,
    #[cfg(feature = "host")]
    #[error("established agent metadata is missing")]
    MissingEstablishedStore,
    #[cfg(feature = "host")]
    #[error("agent store identity mismatch: {0}")]
    IdentityMismatch(String),
    #[cfg(feature = "host")]
    #[error("agent store schema {observed} is incompatible; expected {expected}")]
    SchemaMismatch { expected: u32, observed: u32 },
    #[cfg(feature = "host")]
    #[error("agent store is corrupt: {0}")]
    Corrupt(String),
    #[cfg(feature = "host")]
    #[error("initialization is not authorized: {0}")]
    InitializationNotAuthorized(String),
    #[cfg(feature = "host")]
    #[error("durable effect conflict: {0}")]
    DurableEffectConflict(String),
    #[cfg(feature = "host")]
    #[error("command rejected: {0}")]
    CommandRejected(String),
    #[cfg(feature = "host")]
    #[error("transport session rejected: {0}")]
    SessionRejected(String),
    #[cfg(feature = "host")]
    #[error("transport backpressure: {0}")]
    Backpressure(String),
    #[cfg(feature = "host")]
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[cfg(feature = "host")]
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[cfg(feature = "host")]
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, RuntimeError>;

#[derive(Debug, Error)]
pub(crate) enum ContractError {
    #[error("authority mismatch: {0}")]
    AuthorityMismatch(String),
    #[error("persistence error: {0}")]
    Persistence(String),
}

pub(crate) type ContractResult<T> = std::result::Result<T, ContractError>;

impl From<ContractError> for RuntimeError {
    fn from(error: ContractError) -> Self {
        match error {
            ContractError::AuthorityMismatch(message) => Self::AuthorityMismatch(message),
            ContractError::Persistence(message) => Self::Application(message),
        }
    }
}

impl From<crate::protocol::validation::ValidationError> for RuntimeError {
    fn from(error: crate::protocol::validation::ValidationError) -> Self {
        Self::AuthorityMismatch(error.to_string())
    }
}
