pub mod copy;
pub mod queue;
pub mod quorum;
pub mod stream;

pub(crate) mod log;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use kuberic_protocol::types::{
    AccessStatus, ConfigurationDescriptor, Epoch, ReplicaId, ReplicaIdentity, ReplicaRole,
};
use tokio::sync::{Mutex, RwLock};

use crate::application::{ClientWrite, Lsn, OperationData, StateProvider};
use crate::authority::{
    BuildAuthorityStore, BuildProgressStore, LocalWriteJournal, ReplicaAuthorityStore,
    ReplicationProgressStore,
};
use crate::effects::RuntimeEffectAction;
use crate::engine::DurableState;
use crate::replicator::copy::{PrepareCopyRequest, PreparedCopy};
use crate::runtime::{
    DefaultReplicatorInner, OutboundReplication, PendingReplication, PendingWrite, RuntimeHost,
    RuntimeSnapshot,
};
use crate::{Result, RuntimeError};
use kuberic_wire::proto;
use stream::{OperationStream, ServiceStreams};

/// IFabricReplicator, with COM Begin/End pairs collapsed to async calls.
#[async_trait]
pub trait Replicator: Send + Sync {
    async fn open(&self) -> Result<String>;
    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()>;
    async fn update_epoch(&self, epoch: Epoch) -> Result<()>;
    async fn close(&self) -> Result<()>;
    fn abort(&self);
    async fn current_progress(&self) -> Result<Lsn>;
    async fn catch_up_capability(&self) -> Result<Lsn>;
}

/// IFabricPrimaryReplicator; engine bookkeeping is deliberately not part of this API.
#[async_trait]
pub trait PrimaryReplicator: Replicator {
    async fn on_data_loss(&self) -> Result<bool>;
    async fn update_catch_up_replica_set_configuration(
        &self,
        current: ConfigurationDescriptor,
        previous: ConfigurationDescriptor,
    ) -> Result<()>;
    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()>;
    async fn update_current_replica_set_configuration(
        &self,
        current: ConfigurationDescriptor,
    ) -> Result<()>;
    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()>;
    async fn remove_replica(&self, replica_id: ReplicaId) -> Result<()>;
}

#[async_trait]
pub trait StateReplicator: Send + Sync {
    /// Completes only after durable local acceptance and the admitted PC/CC write quorums.
    async fn replicate(&self, data: OperationData) -> Result<Lsn>;
    async fn get_replication_stream(&self) -> Result<OperationStream>;
    async fn get_copy_stream(&self) -> Result<OperationStream>;
    async fn update_replicator_settings(&self, settings: ReplicatorSettings) -> Result<()>;
}

#[async_trait]
pub(crate) trait ManagedReplicator: Send + Sync {
    async fn attach_interfaces(
        &self,
        control: Arc<dyn Replicator>,
        primary: Option<Arc<dyn PrimaryReplicator>>,
    ) -> Result<()>;
    async fn complete_open(&self, replication_address: String) -> Result<()>;
    async fn restore_authority(&self) -> Result<()>;
    async fn execute_action(&self, action: RuntimeEffectAction) -> Result<()>;
    async fn snapshot(&self) -> RuntimeSnapshot;
    async fn begin_write(&self, write: ClientWrite) -> Result<PendingWrite>;
    async fn accept_acknowledgement(&self, acknowledgement: proto::ReplicationAck) -> Result<()>;
    async fn prepare_copy(&self, request: PrepareCopyRequest) -> Result<PreparedCopy>;
    async fn accept_copy_acknowledgement(&self, acknowledgement: proto::CopyAck) -> Result<()>;
    async fn receive_copy_item(&self, item: proto::CopyItem) -> Result<proto::CopyAck>;
    async fn receive_replication(&self, item: proto::ReplicationItem)
    -> Result<PendingReplication>;
    async fn next_outbound(&self) -> Option<OutboundReplication>;
    fn abort(&self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaSetQuorumMode {
    WriteQuorum,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaInformation {
    pub identity: ReplicaIdentity,
    pub replication_address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplicatorSettings {
    pub replication_address: String,
}

/// Rust's explicit counterpart of obtaining both interfaces from CreateReplicator.
pub struct ReplicatorInterfaces {
    pub replicator: Arc<dyn Replicator>,
    pub state_replicator: Arc<dyn StateReplicator>,
    pub primary_replicator: Option<Arc<dyn PrimaryReplicator>>,
    managed_replicator: Option<Arc<dyn ManagedReplicator>>,
}

impl ReplicatorInterfaces {
    pub fn new(
        replicator: Arc<dyn Replicator>,
        state_replicator: Arc<dyn StateReplicator>,
        primary_replicator: Option<Arc<dyn PrimaryReplicator>>,
    ) -> Self {
        Self {
            replicator,
            state_replicator,
            primary_replicator,
            managed_replicator: None,
        }
    }

    pub(crate) fn with_managed(
        replicator: Arc<dyn Replicator>,
        state_replicator: Arc<dyn StateReplicator>,
        primary_replicator: Option<Arc<dyn PrimaryReplicator>>,
        managed_replicator: Arc<dyn ManagedReplicator>,
    ) -> Self {
        Self {
            replicator,
            state_replicator,
            primary_replicator,
            managed_replicator: Some(managed_replicator),
        }
    }

    pub(crate) fn managed_replicator(&self) -> Option<Arc<dyn ManagedReplicator>> {
        self.managed_replicator.clone()
    }
}

#[derive(Clone)]
pub struct ReplicatorFactoryContext {
    identity: ReplicaIdentity,
    access: Arc<dyn PartitionAccessView>,
    pub(crate) default_dependencies: Option<DefaultReplicatorDependencies>,
}

impl ReplicatorFactoryContext {
    pub(crate) fn new(
        identity: ReplicaIdentity,
        access: Arc<dyn PartitionAccessView>,
        default_dependencies: DefaultReplicatorDependencies,
    ) -> Self {
        Self {
            identity,
            access,
            default_dependencies: Some(default_dependencies),
        }
    }

    pub fn identity(&self) -> Result<ReplicaIdentity> {
        Ok(self.identity.clone())
    }

    pub async fn write_status(&self) -> Result<AccessStatus> {
        self.access.write_status().await
    }
}

#[async_trait]
pub(crate) trait PartitionAccessView: Send + Sync {
    async fn write_status(&self) -> Result<AccessStatus>;
}

#[derive(Clone)]
pub(crate) struct DefaultReplicatorDependencies {
    pub application: Arc<dyn crate::application::StatefulServiceReplica>,
    pub replica_authority_store: Arc<dyn ReplicaAuthorityStore>,
    pub replication_progress_store: Arc<dyn ReplicationProgressStore>,
    pub local_write_journal: Arc<dyn LocalWriteJournal>,
    pub build_authority_store: Arc<dyn BuildAuthorityStore>,
    pub build_progress_store: Arc<dyn BuildProgressStore>,
}

#[async_trait]
pub trait ReplicatorFactory: Send + Sync {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        state_provider: Arc<dyn StateProvider>,
        settings: ReplicatorSettings,
    ) -> Result<ReplicatorInterfaces>;
}

#[derive(Clone)]
pub struct StatefulServicePartition {
    context: ReplicatorFactoryContext,
    host: Weak<RuntimeHost>,
    factory: Option<Arc<dyn ReplicatorFactory>>,
}

impl StatefulServicePartition {
    pub(crate) fn new(host: Weak<RuntimeHost>, context: ReplicatorFactoryContext) -> Self {
        Self {
            context,
            host,
            factory: None,
        }
    }

    /// Select an implementation during service Open, not in the PodRuntime constructor.
    pub fn with_factory(&self, factory: Arc<dyn ReplicatorFactory>) -> Self {
        Self {
            context: self.context.clone(),
            host: self.host.clone(),
            factory: Some(factory),
        }
    }

    pub async fn get_write_status(&self) -> Result<AccessStatus> {
        self.context.write_status().await
    }

    pub async fn create_replicator(
        &self,
        state_provider: Arc<dyn StateProvider>,
        settings: Option<ReplicatorSettings>,
    ) -> Result<ReplicatorInterfaces> {
        let factory = self.factory.as_ref().ok_or_else(|| {
            RuntimeError::Application("select a replicator factory during Open".into())
        })?;
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        let reservation = host.reserve_replicator_creation()?;
        let interfaces = factory
            .create_replicator(
                self.context.clone(),
                state_provider.clone(),
                settings.unwrap_or_default(),
            )
            .await?;
        if let Err(error) = host
            .register_interfaces(&interfaces, state_provider, reservation)
            .await
        {
            interfaces.replicator.abort();
            return Err(error);
        }
        Ok(interfaces)
    }
}

pub struct DefaultReplicatorFactory {
    storage: Arc<dyn DurableState>,
}

impl DefaultReplicatorFactory {
    pub fn new(storage: Arc<dyn DurableState>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl ReplicatorFactory for DefaultReplicatorFactory {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        state_provider: Arc<dyn StateProvider>,
        settings: ReplicatorSettings,
    ) -> Result<ReplicatorInterfaces> {
        let streams = Arc::new(ServiceStreams::new());
        let dependencies = context.default_dependencies.clone().ok_or_else(|| {
            RuntimeError::Application(
                "default replicator dependencies are unavailable for this partition".into(),
            )
        })?;
        let engine = DefaultReplicatorInner::new(
            context.identity.clone(),
            dependencies.application,
            dependencies.replica_authority_store,
            dependencies.replication_progress_store,
            dependencies.local_write_journal,
            dependencies.build_authority_store,
            dependencies.build_progress_store,
        );
        engine
            .install_provider(
                state_provider.clone(),
                self.storage.clone(),
                streams.clone(),
            )
            .await?;
        let pending = Arc::new(Mutex::new(None));
        let next_operation = Arc::new(AtomicU64::new(0));
        let replicator = Arc::new(DefaultReplicator {
            engine: engine.clone(),
            provider: state_provider,
            settings: Arc::new(RwLock::new(settings)),
            pending: pending.clone(),
            next_operation: next_operation.clone(),
        });
        let state_replicator = Arc::new(DefaultStateReplicator {
            engine: engine.clone(),
            streams,
            settings: replicator.settings.clone(),
            next_operation,
            pending,
        });
        Ok(ReplicatorInterfaces::with_managed(
            replicator.clone(),
            state_replicator,
            Some(replicator),
            engine,
        ))
    }
}

pub struct DefaultReplicator {
    engine: Arc<DefaultReplicatorInner>,
    provider: Arc<dyn StateProvider>,
    settings: Arc<RwLock<ReplicatorSettings>>,
    pending: Arc<Mutex<Option<ClientWrite>>>,
    next_operation: Arc<AtomicU64>,
}

#[async_trait]
impl Replicator for DefaultReplicator {
    async fn open(&self) -> Result<String> {
        let committed_lsn = self.provider.last_committed_lsn().await?;
        self.engine.control_open(committed_lsn).await?;
        Ok(self.settings.read().await.replication_address.clone())
    }

    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()> {
        self.engine.control_change_role(epoch, role).await
    }

    async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        self.engine
            .control_update_epoch(epoch, self.provider.as_ref())
            .await
    }

    async fn close(&self) -> Result<()> {
        self.engine.control_close().await
    }

    fn abort(&self) {
        self.engine.control_abort();
    }

    async fn current_progress(&self) -> Result<Lsn> {
        self.engine.control_progress().await
    }

    async fn catch_up_capability(&self) -> Result<Lsn> {
        self.engine.control_catch_up_capability().await
    }
}

#[async_trait]
impl PrimaryReplicator for DefaultReplicator {
    async fn on_data_loss(&self) -> Result<bool> {
        if let Some(progress) = self
            .engine
            .control_on_data_loss(self.provider.as_ref())
            .await?
        {
            *self.pending.lock().await = None;
            self.next_operation
                .store(progress.max(0) as u64, Ordering::Release);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn update_catch_up_replica_set_configuration(
        &self,
        current: ConfigurationDescriptor,
        previous: ConfigurationDescriptor,
    ) -> Result<()> {
        self.engine
            .configure_replicas(current, Some(previous))
            .await
    }

    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()> {
        self.engine.wait_for_quorum(mode).await
    }

    async fn update_current_replica_set_configuration(
        &self,
        current: ConfigurationDescriptor,
    ) -> Result<()> {
        self.engine.configure_replicas(current, None).await
    }

    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()> {
        self.engine.wait_for_build(replica).await
    }

    async fn remove_replica(&self, replica_id: ReplicaId) -> Result<()> {
        self.engine.remove_replica(replica_id).await
    }
}

struct DefaultStateReplicator {
    engine: Arc<DefaultReplicatorInner>,
    streams: Arc<ServiceStreams>,
    settings: Arc<RwLock<ReplicatorSettings>>,
    next_operation: Arc<AtomicU64>,
    pending: Arc<Mutex<Option<ClientWrite>>>,
}

#[async_trait]
impl StateReplicator for DefaultStateReplicator {
    async fn replicate(&self, data: OperationData) -> Result<Lsn> {
        let engine = &self.engine;
        engine.require_write_access().await?;
        let mut reservation = self.pending.lock().await;
        engine.require_write_access().await?;
        if let Some(write) = reservation.as_ref() {
            if write.data != data {
                return Err(RuntimeError::LocalWritePending(
                    write.operation_id.to_string(),
                ));
            }
        } else {
            let id = self.next_operation.fetch_add(1, Ordering::Relaxed);
            *reservation = Some(engine.recover_replicate_write(data, id).await);
        }
        let pending = match engine
            .begin_write(reservation.as_ref().expect("write reserved").clone())
            .await
        {
            Ok(pending) => pending,
            Err(
                error @ (RuntimeError::DataLossFenced
                | RuntimeError::WriteClosed(_)
                | RuntimeError::AuthorityMismatch(_)
                | RuntimeError::NotPrimary),
            ) => {
                *reservation = None;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = engine.publish_replication(&pending).await {
            if matches!(
                error,
                RuntimeError::DataLossFenced
                    | RuntimeError::WriteClosed(_)
                    | RuntimeError::AuthorityMismatch(_)
                    | RuntimeError::NotPrimary
            ) {
                *reservation = None;
            }
            return Err(error);
        }
        let lsn = match pending.committed().await {
            Ok(receipt) => receipt.lsn,
            Err(error) => {
                if matches!(
                    error,
                    RuntimeError::DataLossFenced
                        | RuntimeError::WriteClosed(_)
                        | RuntimeError::AuthorityMismatch(_)
                        | RuntimeError::NotPrimary
                ) {
                    *reservation = None;
                }
                return Err(error);
            }
        };
        *reservation = None;
        Ok(lsn)
    }

    async fn get_replication_stream(&self) -> Result<OperationStream> {
        self.streams.take_replication().await
    }

    async fn get_copy_stream(&self) -> Result<OperationStream> {
        self.streams.take_copy().await
    }

    async fn update_replicator_settings(&self, settings: ReplicatorSettings) -> Result<()> {
        *self.settings.write().await = settings;
        Ok(())
    }
}
