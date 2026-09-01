use crate::types::{
    AgentControlVersion, AgentGeneration, DurableActionErrorClass, Epoch, ReplicaId,
    ReplicaInstanceId, Role,
};

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

    /// A locally serialized mutation is already active.
    #[error("replica agent is busy")]
    AgentBusy,

    /// One retained action identifier was reused with different input.
    #[error("correlated action ID {action_id} was reused with different input")]
    ActionIdConflict { action_id: String },

    /// Correlated actions require a stable non-empty identifier.
    #[error("correlated action ID must not be empty")]
    InvalidCorrelatedActionId,

    /// The caller-provided signature does not describe the action payload.
    #[error("correlated action {action_id} input signature does not match its payload")]
    ActionSignatureMismatch { action_id: String },

    /// The versioned correlated command uses an unsupported protocol version.
    #[error("unsupported correlated control protocol version {got}")]
    UnsupportedControlProtocolVersion { got: u32 },

    #[error("unsupported replica add/build peer protocol version {got}")]
    UnsupportedPeerProtocolVersion { got: u32 },

    #[error("peer stage message ID {message_id} was reused with different input")]
    PeerStageIdConflict { message_id: String },

    #[error("peer stage target mismatch: {0}")]
    PeerStageTargetMismatch(String),

    #[error("peer stage is stale: {0}")]
    PeerStageStale(String),

    /// The command addresses another replica or replica incarnation.
    #[error(
        "correlated action target mismatch: expected replica {expected_id}@{expected_instance}, got {actual_id}@{actual_instance}"
    )]
    CorrelatedTargetMismatch {
        expected_id: ReplicaId,
        expected_instance: ReplicaInstanceId,
        actual_id: ReplicaId,
        actual_instance: ReplicaInstanceId,
    },

    /// The command was fenced to another pod-local process.
    #[error("stale agent generation: expected {expected}, current {current}")]
    StaleAgentGeneration {
        expected: AgentGeneration,
        current: AgentGeneration,
    },

    /// Local mutation admission advanced after the caller observed status.
    #[error("stale agent control version: expected {expected:?}, current {current:?}")]
    StaleAgentControlVersion {
        expected: AgentControlVersion,
        current: AgentControlVersion,
    },

    /// An unretained action cannot be safely classified from an old snapshot.
    #[error("correlated action {action_id} continuity is unavailable")]
    CorrelatedContinuityUnavailable { action_id: String },

    /// A remote agent rejected a request before execution.
    #[error("remote replica-agent request rejected: {0}")]
    RemoteAgentRequestRejected(String),

    /// A remote agent rejected stale target/generation/version/epoch evidence.
    #[error("remote replica-agent precondition rejected: {0}")]
    RemoteAgentPreconditionRejected(String),

    /// A remote agent reported a retained action-ID conflict.
    #[error("remote replica-agent action conflict: {0}")]
    RemoteAgentConflict(String),

    /// A remote agent could not prove the requested local continuity.
    #[error("remote replica-agent continuity unavailable: {0}")]
    RemoteAgentContinuityUnavailable(String),

    /// The remote endpoint does not support the requested control protocol.
    #[error("remote replica-agent protocol unsupported: {0}")]
    RemoteControlProtocolUnsupported(String),

    #[error("remote replica add/build peer request rejected: {0}")]
    RemotePeerRequestRejected(String),

    #[error("remote replica add/build peer is unavailable: {0}")]
    RemotePeerUnavailable(String),

    /// The remote agent retained a terminal action failure with its stable
    /// classification.
    #[error("remote replica-agent terminal failure ({class:?}): {message}")]
    RemoteAgentTerminalFailure {
        class: DurableActionErrorClass,
        message: String,
    },

    /// Internal error (IO, serialization, etc.)
    #[error(transparent)]
    Internal(#[from] Box<dyn std::error::Error + Send + Sync>),

    /// Stable operator-restart recovery validation failed.
    #[error(transparent)]
    Recovery(#[from] RecoveryError),
}

pub type Result<T> = std::result::Result<T, KubericError>;
