use kuberic_protocol::types::AccessStatus;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("runtime is closed")]
    Closed,
    #[error("runtime is not open")]
    NotOpen,
    #[error("runtime is not primary")]
    NotPrimary,
    #[error("writes are closed with status {0:?}")]
    WriteClosed(AccessStatus),
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
}

pub type Result<T> = std::result::Result<T, RuntimeError>;
