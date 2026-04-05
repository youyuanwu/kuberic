use std::fmt::Debug;

use async_trait::async_trait;
use bytes::Bytes;

use crate::types::{
    CancelToken, DataLossAction, Epoch, Lsn, ReplData, ReplicaId, ReplicaInfo, ReplicaSetConfig,
    Role, TokioReceiverStream,
};

#[async_trait]
pub trait Replicator: 'static + Send + Sync {
    async fn open(&self, token: CancelToken) -> crate::Result<()>;
    async fn close(&self, token: CancelToken) -> crate::Result<()>;
    async fn change_role(&self, role: Role, token: CancelToken) -> crate::Result<()>;
    async fn update_epoch(&self, epoch: Epoch, token: CancelToken) -> crate::Result<()>;
    /// Returns the address of the replicator.
    async fn get_address(&self, token: CancelToken) -> crate::Result<String>;

    /// the latest lsn that this replica has.
    async fn get_current_progress(&self) -> crate::Result<Lsn>;
    /// The oldest lsn that this replica has.
    async fn get_catch_up_capability(&self) -> crate::Result<Lsn>;

    // primary apis

    async fn on_data_loss(&self, token: CancelToken) -> crate::Result<DataLossAction>;

    async fn update_catch_up_replica_set_configuration(
        &mut self,
        currentconfiguration: ReplicaSetConfig,
        previousconfiguration: ReplicaSetConfig,
    ) -> crate::Result<()>;

    async fn update_current_replica_set_configuration(
        &mut self,
        currentconfiguration: ReplicaSetConfig,
    ) -> crate::Result<()>;

    async fn wait_for_catch_up_quorum(
        &self,
        mode: crate::types::ReplicaSetQuorumMode,
        token: CancelToken,
    ) -> crate::Result<()>;

    async fn build_replica(&self, replica: ReplicaInfo, token: CancelToken) -> crate::Result<()>;

    async fn remove_replica(&self, replicaid: ReplicaId) -> crate::Result<()>;
}

/// State provider is managed by the system.
#[async_trait]
pub trait StateProvider: 'static + Send + Sync + std::fmt::Debug {
    /// set the epoch of the state provider.
    // TODO change epoch type.
    async fn update_epoch(&self, epoch: Epoch, token: CancelToken) -> crate::Result<()>;

    /// Obtains the last sequence number that the service has committed,
    /// also known as Logical Sequence Number (LSN).
    /// TODO: this is what replicator will start the lsn from.
    /// consume_replication_data should update this number.
    fn get_last_committed_sequence_number(&self) -> crate::Result<Lsn>;

    /// The oldest lsn that this replica has.
    /// lsn data might be cleaned up before this lsn.
    fn get_oldest_sequence_number(&self) -> crate::Result<Lsn>;

    async fn on_data_loss(&self, token: CancelToken) -> crate::Result<DataLossAction>;

    /// Called on primary to copy data to secondary.
    /// up_to_lsn is the lsn up to which the copy is needed, after this lsn data should be replicated.
    /// Remarks:
    /// up_to_lsn is determined at the start of a replica build by the replicator.
    /// It is typically the last committed lsn tracked by the replicator.
    /// Replicator should ensure that up-to-lsn is what the state provider should have applied,
    /// and the state provider can take some time to replay up to that lsn.
    /// Replicator should keep track of the wal after up_to_lsn and ship to the in build replica after copy is done.
    fn produce_copy_state(
        &mut self,
        up_to_lsn: Lsn,
        copy_context_stream: TokioReceiverStream<Bytes>,
    ) -> crate::Result<TokioReceiverStream<Bytes>>;
}

/// User uses this to drive replication.
/// Never called by the system.
#[async_trait]
pub trait StateReplicator: 'static + Send + Sync {
    /// Returns lsn and error.
    /// Replicates data to peers and local and returns the lsn.
    /// This should be called on primary.
    async fn replicate(&mut self, data: Bytes, token: CancelToken) -> crate::Result<Lsn>;
}

#[async_trait]
pub trait BackgroundStateReplicator: 'static + Send + Sync {
    /// Gets the stream of committed wal data.
    fn replication_stream(
        &mut self,
        ack: TokioReceiverStream<Lsn>,
    ) -> TokioReceiverStream<ReplData>;
}

#[async_trait]
pub trait BackgroundCopyStateProvider: 'static + Send + Sync {
    /// Gets the stream of copy state data.
    fn copy_state_stream(&mut self, ack: TokioReceiverStream<Lsn>)
    -> TokioReceiverStream<ReplData>;
}

/// Wal storage.
#[async_trait]
pub trait WalStorage: 'static + Send + Sync {
    /// Store the replication data for the given lsn.
    async fn store_replication_data(&mut self, lsn: Lsn, data: Bytes) -> crate::Result<()>;
    async fn remove_replication_data(&mut self, lsn: Lsn) -> crate::Result<()>;

    /// Store the copy data for the given lsn.
    /// lsn is generated and does not mean much.
    async fn store_copy_data(&mut self, id: Lsn, data: Bytes) -> crate::Result<()>;
    async fn remove_copy_data(&mut self, id: Lsn) -> crate::Result<()>;
}

/// Server
#[async_trait]
pub trait OperationReceiver: 'static + Send + Sync {
    /// Receives a temporary replication stream for consumption.
    fn replication_data(&mut self, data: ReplData) -> crate::Result<()>;

    /// Receives a copy data stream for consumption on secondary.
    /// Returns ok if data is saved in storage.
    async fn copy_data(&mut self, data: Bytes) -> crate::Result<()>;

    /// Receives a copy context on primary.
    async fn copy_context(&mut self, context: Bytes) -> crate::Result<()>;
}

/// Client
#[async_trait]
pub trait OperationSender: 'static + Send + Sync + Debug {
    /// Requests a copy context stream from secondary.
    async fn request_copy_context_stream(&self) -> crate::Result<TokioReceiverStream<Bytes>>;

    /// Sends replication item for consumption.
    async fn send_replication_data_item(&mut self, data_item: ReplData) -> crate::Result<()>;
    /// Sends a copy data stream for consumption.
    async fn send_copy_data_item(&mut self, copy_item: Bytes) -> crate::Result<()>;

    fn clone_box(&self) -> Box<dyn OperationSender>;
}
