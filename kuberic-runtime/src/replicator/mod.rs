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
use crate::engine::DurableState;
use crate::runtime::ReplicationEngine;
use crate::{Result, RuntimeError};
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
}

#[derive(Clone)]
pub struct ReplicatorFactoryContext {
    pub(crate) engine: Weak<ReplicationEngine>,
}

impl ReplicatorFactoryContext {
    pub fn identity(&self) -> Result<ReplicaIdentity> {
        Ok(self.engine()?.identity.clone())
    }

    pub async fn write_status(&self) -> Result<AccessStatus> {
        Ok(self.engine()?.write_status().await)
    }

    fn engine(&self) -> Result<Arc<ReplicationEngine>> {
        self.engine.upgrade().ok_or(RuntimeError::Closed)
    }
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
    factory: Option<Arc<dyn ReplicatorFactory>>,
}

impl StatefulServicePartition {
    pub(crate) fn new(engine: Weak<ReplicationEngine>) -> Self {
        Self {
            context: ReplicatorFactoryContext { engine },
            factory: None,
        }
    }

    /// Select an implementation during service Open, not in the PodRuntime constructor.
    pub fn with_factory(&self, factory: Arc<dyn ReplicatorFactory>) -> Self {
        Self {
            context: self.context.clone(),
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
        let engine = self.context.engine()?;
        let reservation = engine.reserve_replicator_creation()?;
        let interfaces = factory
            .create_replicator(
                self.context.clone(),
                state_provider.clone(),
                settings.unwrap_or_default(),
            )
            .await?;
        if let Err(error) = engine
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
        let engine = context.engine()?;
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
            context: context.clone(),
            provider: state_provider,
            settings: Arc::new(RwLock::new(settings)),
            pending: pending.clone(),
            next_operation: next_operation.clone(),
        });
        let state_replicator = Arc::new(DefaultStateReplicator {
            context,
            streams,
            settings: replicator.settings.clone(),
            next_operation,
            pending,
        });
        Ok(ReplicatorInterfaces {
            replicator: replicator.clone(),
            state_replicator,
            primary_replicator: Some(replicator),
        })
    }
}

pub struct DefaultReplicator {
    context: ReplicatorFactoryContext,
    provider: Arc<dyn StateProvider>,
    settings: Arc<RwLock<ReplicatorSettings>>,
    pending: Arc<Mutex<Option<ClientWrite>>>,
    next_operation: Arc<AtomicU64>,
}

#[async_trait]
impl Replicator for DefaultReplicator {
    async fn open(&self) -> Result<String> {
        let committed_lsn = self.provider.last_committed_lsn().await?;
        self.context.engine()?.control_open(committed_lsn).await?;
        Ok(self.settings.read().await.replication_address.clone())
    }

    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()> {
        self.context
            .engine()?
            .control_change_role(epoch, role)
            .await
    }

    async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        let engine = self.context.engine()?;
        engine
            .control_update_epoch(epoch, self.provider.as_ref())
            .await
    }

    async fn close(&self) -> Result<()> {
        self.context.engine()?.control_close().await
    }

    fn abort(&self) {
        if let Ok(engine) = self.context.engine() {
            engine.control_abort();
        }
    }

    async fn current_progress(&self) -> Result<Lsn> {
        self.context.engine()?.control_progress().await
    }

    async fn catch_up_capability(&self) -> Result<Lsn> {
        self.context.engine()?.control_catch_up_capability().await
    }
}

#[async_trait]
impl PrimaryReplicator for DefaultReplicator {
    async fn on_data_loss(&self) -> Result<bool> {
        let engine = self.context.engine()?;
        if let Some(progress) = engine.control_on_data_loss(self.provider.as_ref()).await? {
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
        self.context
            .engine()?
            .configure_replicas(current, Some(previous))
            .await
    }

    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()> {
        self.context.engine()?.wait_for_quorum(mode).await
    }

    async fn update_current_replica_set_configuration(
        &self,
        current: ConfigurationDescriptor,
    ) -> Result<()> {
        self.context
            .engine()?
            .configure_replicas(current, None)
            .await
    }

    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()> {
        self.context.engine()?.wait_for_build(replica).await
    }

    async fn remove_replica(&self, replica_id: ReplicaId) -> Result<()> {
        self.context.engine()?.remove_replica(replica_id).await
    }
}

struct DefaultStateReplicator {
    context: ReplicatorFactoryContext,
    streams: Arc<ServiceStreams>,
    settings: Arc<RwLock<ReplicatorSettings>>,
    next_operation: Arc<AtomicU64>,
    pending: Arc<Mutex<Option<ClientWrite>>>,
}

#[async_trait]
impl StateReplicator for DefaultStateReplicator {
    async fn replicate(&self, data: OperationData) -> Result<Lsn> {
        let engine = self.context.engine()?;
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
