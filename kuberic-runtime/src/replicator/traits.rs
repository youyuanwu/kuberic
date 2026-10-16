use std::sync::Arc;

use async_trait::async_trait;

use super::stream::OperationStream;
use super::{
    ReplicaInformation, ReplicaSetConfiguration, ReplicaSetQuorumMode, ReplicatorFactoryContext,
    ReplicatorInterfaces, ReplicatorSettings,
};
use crate::Result;
use crate::application::{Lsn, OperationData, StateProvider};
use crate::protocol::types::{Epoch, ReplicaId, ReplicaRole};

/// Controls replication lifecycle, role and epoch changes, and reports progress
/// used for primary selection and catch-up.
#[async_trait]
pub trait Replicator: Send + Sync {
    /// Opens the replicator and returns its replication address, advertised to
    /// the primary in [`ReplicaInformation`].
    ///
    /// The replicator does not yet have an assigned role and should set up its
    /// listening endpoint.
    async fn open(&self) -> Result<String>;

    /// Changes the replicator's role in the supplied epoch.
    ///
    /// Called before the service replica's
    /// [`change_role`](crate::application::StatefulServiceReplica::change_role).
    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()>;

    /// Indicates a configuration change or attempted primary change. Epochs
    /// segment operations by primary configuration; replicas must fence
    /// operations from an older epoch.
    ///
    /// The runtime can replay explicit epoch barriers on the primary as well as
    /// secondaries, so implementations must not assume a secondary-only call.
    async fn update_epoch(&self, epoch: Epoch) -> Result<()>;

    /// Closes the replicator gracefully when it is being shut down.
    async fn close(&self) -> Result<()>;

    /// Ungracefully terminates the replicator and promptly releases resources.
    fn abort(&self);

    /// Returns the current LSN used for primary selection and supplied to the
    /// primary's catch-up configuration.
    ///
    /// Primary: locally durable quorum-committed progress. Secondary: highest
    /// contiguous durably accepted progress. Recovery: election-only progress
    /// while read/write access remains closed.
    async fn current_progress(&self) -> Result<Lsn>;

    /// Earliest retained public progress that can catch up an admitted peer.
    /// `INVALID_LSN` means the capability is not currently known or applicable.
    ///
    /// Together with the peer's current progress, this determines whether
    /// retained history can catch the peer up or a fresh copy is required.
    async fn catch_up_capability(&self) -> Result<Lsn>;
}

/// Manages replica-set configuration, catch-up, and replica building on the primary.
///
/// There is always a current voting configuration and there may also be a
/// previous configuration during reconfiguration. When both exist, writes must
/// be acknowledged by a write quorum of each configuration.
///
/// Adding a secondary starts with an idle replica, possibly containing old or
/// partial data. The primary builds it with [`Self::build_replica`], after which
/// it can become an active secondary. A catch-up configuration includes it and
/// [`Self::wait_for_catch_up_quorum`] performs final synchronization before read
/// access is granted. Building a replica alone does not admit it to a quorum.
#[async_trait]
pub trait PrimaryReplicator: Replicator {
    /// Called when data loss may have occurred, such as after write-quorum loss.
    /// Returns whether recovery changed state; `true` requires other replicas
    /// to be rebuilt.
    ///
    /// A replicator using a state provider can delegate recovery to
    /// [`StateProvider::on_data_loss`].
    async fn on_data_loss(&self) -> Result<bool>;

    /// Supplies the current and previous replica-set configurations to the
    /// primary so active secondaries can catch up. Idle build targets are not
    /// voting members.
    ///
    /// A secondary selected as the next primary is marked `must_catchup` in
    /// the current configuration and must catch up before promotion. Replica
    /// progress and catch-up capability describe the remote replica's retained
    /// history; [`INVALID_LSN`](crate::application::INVALID_LSN) denotes unknown
    /// or inapplicable progress.
    async fn update_catch_up_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
        previous: ReplicaSetConfiguration,
    ) -> Result<()>;

    /// Waits for replicas to catch up to the current committed LSN.
    ///
    /// [`ReplicaSetQuorumMode::All`] requires all participating replicas to
    /// catch up. [`ReplicaSetQuorumMode::WriteQuorum`] requires a write quorum
    /// of each applicable configuration and the replica marked `must_catchup`,
    /// if present. This lets a primary swap proceed without waiting for every
    /// slow replica when specific-quorum catch-up is supported.
    ///
    /// A primary swap can use double catch-up: once while writes are permitted,
    /// then again after write access is revoked. Other reconfigurations can
    /// catch up with writes still permitted. Implementations must follow the
    /// catch-up condition rather than assume a particular write-access status
    /// or a single invocation; concurrent writes can advance the target LSN.
    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()>;

    /// Supplies the current replica-set configuration and ends the previous
    /// configuration's participation.
    ///
    /// Replicas in the final configuration are no longer marked `must_catchup`.
    async fn update_current_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
    ) -> Result<()>;

    /// Transfers state through the current quorum-committed LSN to a new or
    /// existing idle replica outside the voting configuration.
    ///
    /// The target has role [`ReplicaRole::IdleSecondary`], both progress values
    /// set to [`INVALID_LSN`](crate::application::INVALID_LSN), and
    /// `must_catchup` set to `false`.
    /// A replica being built or already built does not count toward quorum;
    /// it must be admitted separately through a configuration update.
    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()>;

    /// Notifies the primary that an idle replica built by [`Self::build_replica`]
    /// has gone down. Stop sending operations to it and release its resources.
    ///
    /// Active replicas are removed through a configuration update that omits
    /// them, rather than this callback. This callback concerns an idle replica
    /// whose build has completed.
    async fn remove_replica(&self, replica_id: ReplicaId) -> Result<()>;
}

/// Application-facing replication and operation-stream interface.
///
/// Services use this interface to replicate state changes for high availability
/// and consume secondary copy and replication operations.
///
/// The stream methods describe service-owned operation delivery. The built-in
/// replicator currently applies incoming data directly through
/// [`DurableState`](crate::engine::DurableState) rather than delivering it
/// through these service streams.
#[async_trait]
pub trait StateReplicator: Send + Sync {
    /// Replicates `data`, representing a primary's state change, to secondaries
    /// and returns the operation's LSN after quorum acknowledgement.
    ///
    /// Completes only after durable local acceptance and the admitted PC/CC write
    /// quorums. Check
    /// [`StatefulServicePartition::get_write_status`](super::StatefulServicePartition::get_write_status)
    /// before servicing a client write; primary role alone does not grant write
    /// access.
    ///
    /// The LSN becomes available only when this async call completes. Dropping
    /// the future is not proof that no replica accepted the operation's bytes.
    async fn replicate(&self, data: OperationData) -> Result<Lsn>;

    /// Returns the stream of replicated operations for the service to apply
    /// and durably acknowledge on a secondary.
    ///
    /// Operations originate from the primary's [`Self::replicate`] calls.
    /// Normally drain the copy stream before applying replication operations.
    /// Consuming both concurrently requires the service to preserve correct
    /// copy/replication ordering and is intended for advanced implementations.
    /// [`OperationStream::get_operation`] returns `None` at the end of a stream.
    async fn get_replication_stream(&self) -> Result<OperationStream>;

    /// Returns the stream of copy operations used to initialize a replica's state.
    ///
    /// These payloads originate from the primary provider's
    /// [`StateProvider::get_copy_state`] stream. The secondary obtains the
    /// stream, applies its operations, and explicitly acknowledges them while
    /// participating in the copy-context exchange.
    /// [`OperationStream::get_operation`] returns `None` at the end of a stream.
    async fn get_copy_stream(&self) -> Result<OperationStream>;

    /// Updates replication settings without changing the registered replication
    /// address or replica identity.
    ///
    /// [`ReplicatorSettings`] currently exposes only the replication address.
    /// Runtime credential updates and settings retrieval are not exposed.
    async fn update_replicator_settings(&self, settings: ReplicatorSettings) -> Result<()>;
}

#[async_trait]
pub trait ReplicatorFactory: Send + Sync {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        state_provider: Option<Arc<dyn StateProvider>>,
        settings: ReplicatorSettings,
    ) -> Result<ReplicatorInterfaces>;
}
