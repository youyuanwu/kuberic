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
    /// Primary progress is native queue/quorum progress, not a primary-local
    /// service commit receipt. Secondary progress follows its native receiver's
    /// ACK mode and is unavailable before copy completes. An explicit premature
    /// secondary query retains native fault behavior. Opened recovery obtains
    /// the protected provider's last committed LSN, normalizing successful
    /// negative values as native empty progress. No scalar grants serving access.
    async fn current_progress(&self) -> Result<Lsn>;

    /// Native catch-up capability, distinct from current progress and application
    /// durability. The built-in primary reports its first committed operation
    /// boundary, or zero when that queue observation has no committed range.
    /// As a query result, `INVALID_LSN` means the capability is not currently
    /// known or applicable, unlike omitted known-replica configuration input.
    ///
    /// Native queue/session validation still determines whether an exact peer
    /// can replay retained history or requires copy; this scalar alone is not a
    /// durability proof or an ordered range paired with current progress.
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
    /// history. In SF configuration input,
    /// [`INVALID_LSN`](crate::application::INVALID_LSN) can omit progress for a
    /// replica already configured or built: use the replicator's known progress,
    /// not zero or a new history. Unknown or inapplicable progress also uses the
    /// sentinel; reuse must preserve exact identity, session, and storage checks.
    async fn update_catch_up_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
        previous: ReplicaSetConfiguration,
    ) -> Result<()>;

    /// Waits for replicas to satisfy the requested catch-up condition.
    ///
    /// For the built-in classic V1 replicator:
    ///
    /// - [`ReplicaSetQuorumMode::All`] requires all current replicas' apply
    ///   progress to reach the latest issued LSN. The target follows new writes
    ///   while the wait is pending.
    /// - [`ReplicaSetQuorumMode::WriteQuorum`] requires current-configuration
    ///   quorum apply progress to reach the boundary captured by configuration
    ///   updates. If replicas are marked `must_catchup`, their minimum apply
    ///   progress must also reach the live current-quorum apply progress.
    ///
    /// Unlike ordinary replication completion, this is not an independent
    /// previous/current-configuration quorum check. Catch-up completion alone
    /// does not certify provider-owned commitment or client success.
    ///
    /// A primary swap can use double catch-up: once while writes are permitted,
    /// then again after write access is revoked. Other reconfigurations can
    /// catch up with writes still permitted. Implementations must follow the
    /// catch-up condition rather than assume a particular write-access status
    /// or a single invocation.
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
/// Native queue assignment and replication completion do not persist or commit
/// primary-local service state. The service owns preparation, ordered local
/// commitment, visibility and client completion. Persisted receivers require a
/// parameterless service ACK after durable application acceptance; volatile
/// receivers retain their configured native dispatch/ACK behavior.
#[async_trait]
pub trait StateReplicator: Send + Sync {
    /// Admits work and returns its sequence separately from native completion.
    /// Service-local commitment and client success remain application-owned.
    fn begin_replicate(&self, data: OperationData) -> Result<super::Replication>;

    /// Obtain a handle to the current secondary's shared replication receiver.
    /// Obtain streams after the secondary role exists; repeated handles do not
    /// rewind delivery or create independent copies of the receiver.
    fn replication_operations(&self) -> Result<OperationStream>;

    /// Obtain a handle to the current secondary's shared copy receiver. Copy
    /// enumeration EOF, a native EOS operation, native Build completion and
    /// configuration admission are distinct events.
    fn copy_operations(&self) -> Result<OperationStream>;

    /// Replicates `data`, representing a primary's state change, to secondaries
    /// and returns the operation's LSN after quorum acknowledgement.
    ///
    /// This convenience wait observes native completion only; it is not a
    /// service-local commit or a successful client transaction. Check
    /// [`StatefulServicePartition::get_write_status`](super::StatefulServicePartition::get_write_status)
    /// before servicing a client write; primary role alone does not grant write
    /// access.
    ///
    /// The LSN becomes available only when this async call completes. Dropping
    /// the future is not proof that no replica accepted the operation's bytes.
    async fn replicate(&self, data: OperationData) -> Result<Lsn> {
        let admitted = self.begin_replicate(data)?;
        admitted.completion.await?;
        Ok(admitted.lsn)
    }

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
