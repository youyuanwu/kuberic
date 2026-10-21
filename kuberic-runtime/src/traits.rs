use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::application::{Lsn, OpenContext, OperationDataStream, RoleChange};
use crate::protocol::types::{Epoch, ReplicaRole};
use crate::replicator::Replicator;

/// Defines behavior that governs the lifecycle of a stateful service replica,
/// such as startup, initialization, role changes, and shutdown.
///
/// Stateful service types implement this interface for behavior invoked on
/// both primary and secondary replicas.
#[async_trait]
pub trait StatefulServiceReplica: Send + Sync {
    /// Opens an initialized service replica so that additional actions can be taken.
    /// Returns the replicator used by the stateful service.
    ///
    /// The open context supplies the open mode and partition. Services can use
    /// [`StatefulServicePartition::create_replicator`](crate::replicator::StatefulServicePartition::create_replicator)
    /// to create and register coherent replication interfaces rather than
    /// implementing a replicator.
    async fn open(self: Arc<Self>, context: OpenContext) -> Result<Arc<dyn Replicator>>;

    /// Changes the service replica's role and returns its new client connection
    /// address, if any.
    ///
    /// [`Replicator::change_role`] is called before this callback. The service
    /// can update its listening address and claim resources such as ports only
    /// when its role requires client communication.
    async fn change_role(&self, role: ReplicaRole) -> Result<RoleChange>;

    /// Closes the service replica gracefully when it is being shut down.
    async fn close(&self) -> Result<()>;

    /// Ungracefully terminates the service replica.
    ///
    /// Immediately release resources and clean up references without waiting
    /// for graceful shutdown.
    fn abort(&self);
}

/// Defines the behavior a service implements to interact with its replicator:
/// application-owned state, progress, copy, and data-loss callbacks.
#[async_trait]
pub trait StateProvider: Send + Sync {
    /// Notifies the provider of a new replica-set epoch after a change or
    /// attempted change of primary, such as failure or load balancing. Epochs
    /// separate operations sent by different primary configurations and provide
    /// a barrier against operations from an older primary.
    ///
    /// Advances the provider epoch using the replicator-supplied
    /// `previous_epoch_last_lsn` boundary. The built-in classic V1 primary
    /// supplies its latest issued LSN, including on duplicate-epoch callbacks;
    /// this does not certify quorum commitment or provider durability.
    /// Secondary stream callbacks use their captured prior-epoch boundary.
    /// Preserve the supplied boundary rather than substituting observed,
    /// applied or committed progress.
    async fn update_epoch(&self, epoch: Epoch, previous_epoch_last_lsn: Lsn) -> Result<()>;

    /// Returns the last logical sequence number committed by the service.
    ///
    /// This is provider-owned committed progress. Applied and committed progress
    /// remain distinct even when an implementation usually advances them together.
    async fn last_committed_lsn(&self) -> Result<Lsn>;

    /// Returns provider-defined context used by [`Self::get_copy_state`] to
    /// determine the state needed to build a secondary.
    ///
    /// Copy context lets a newly created and opened idle secondary describe
    /// state it already has. The primary analyzes that context and responds
    /// with a copy-state stream. An empty secondary that requires all state
    /// does not need to supply context.
    ///
    /// Providers that do not need context can return an empty stream.
    /// The built-in classic V1 replicator obtains context from the persisted
    /// target secondary and streams it to the source primary's provider.
    async fn get_copy_context(&self) -> Result<OperationDataStream>;

    /// Obtains state on the primary that is needed to build a secondary,
    /// returning a stream of copy bytes frozen at the committed `up_to_lsn`.
    ///
    /// `up_to_lsn` is the maximum operation LSN represented by this copy.
    /// Operations beyond that boundary belong to subsequent replication.
    /// `copy_context` contains provider-defined context for selecting the
    /// required state. The built-in classic V1 replicator passes the target's
    /// incoming context at the replication start boundary minus one; see
    /// [`Self::get_copy_context`] for the target-to-source exchange.
    ///
    /// With service-stream delivery, the output goes through the secondary's
    /// [`copy_operations`](crate::replicator::StateReplicator::copy_operations).
    /// Repeating an authorized build, including after restart, must reproduce
    /// identical bytes from the same frozen boundary while later replication
    /// remains ordered as retained catch-up.
    async fn get_copy_state(
        &self,
        up_to_lsn: Lsn,
        copy_context: OperationDataStream,
    ) -> Result<OperationDataStream>;

    /// Invoked when data loss may have occurred, such as after write-quorum loss.
    /// Returns whether recovery changed the provider's state.
    ///
    /// Return `false` when recovery made no state correction. A `true` result
    /// tells the runtime that other replicas must be rebuilt from the recovered
    /// state; it does not merely indicate that possible data loss was reported.
    async fn on_data_loss(&self) -> Result<bool>;
}
