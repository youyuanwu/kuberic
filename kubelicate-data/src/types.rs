use bytes::Bytes;

pub type CancelToken = tokio_util::sync::CancellationToken;

pub type TokioReceiverStream<T> = tokio_stream::wrappers::ReceiverStream<T>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataLossAction {
    None,
    StateChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Primary,
    ActiveSecondary,
    IdleSecondary,
    None,
}

pub type Epoch = i64;
pub type Lsn = i64;
pub type ReplicaId = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaStatus {
    Up,
    Down,
}

#[derive(Debug)]
pub struct ReplicaInfo {
    pub id: ReplicaId,
    pub role: Role,
    pub status: ReplicaStatus,
    pub client: Box<dyn crate::traits2::OperationSender>,
    // pub replicator_address: WString,
    pub current_progress: i64,
    pub catch_up_capability: i64,
    /// indicating whether the replica must be caught up as part of a WaitForQuorumCatchup
    pub must_catch_up: bool,
}

#[derive(Debug)]
pub struct ReplicaSetConfig {
    pub members: Vec<ReplicaInfo>,
    pub write_quorum: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaSetQuorumMode {
    All,
    WriteQuorum,
}

#[derive(Debug)]
pub struct ReplData {
    pub data: Bytes,
    pub lsn: Lsn,
}

pub struct ReplOperation {
    pub data: ReplData,
    pub ack: tokio::sync::Notify, // ack that the data is persisted.
}
