//! Composition of the delivered classic owners with the production host carrier.
//! All access/admission decisions remain in the host capability.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;

use crate::application::{Lsn, OperationData, StateProvider};
use crate::host::native_transport::{NativeBinding, NativeEndpoint};
use crate::protocol::public_operations::StatePersistence;
use crate::protocol::types::{Epoch, ReplicaId, ReplicaRole};
use crate::replicator::stream::{OperationKind, OperationStream, ReceivedOperation};
use crate::replicator::{
    NativeRuntimeControl, PrimaryReplicator, ReplicaInformation, ReplicaSetConfiguration,
    ReplicaSetQuorumMode, Replication, Replicator, ReplicatorCapabilities,
    ReplicatorFactoryContext, ReplicatorInterfaces, ReplicatorSettings, StateReplicator,
};
use crate::{Result, RuntimeError};

use super::super::copy_async_operation::CopyConfig;
use super::super::copy_context_receiver::CopyContextConfig;
use super::super::operation_queue::QueueConfig;
use super::super::primary::PrimaryConfig;
use super::super::replica_manager::QueuePolicyConfig;
use super::super::secondary::{SecondaryCallbacks, SecondaryConfig};
use super::super::secondary_copy_receiver::SecondaryCopyConfig;
use super::super::secondary_receiver::ReceiverConfig;
use super::super::sender::SenderConfig;
use super::facade::{Facade, FacadeConfig, FacadeError, PeerBinding, empty_previous};
use super::production_transport::{Transport, buffer_limits, route};
use super::*;

fn error(error: AdapterError) -> RuntimeError {
    FacadeError::Adapter(error).into()
}

fn queue() -> QueueConfig {
    QueueConfig {
        initial_capacity: 64,
        max_capacity: 8192,
        max_bytes: 256 * 1024 * 1024,
        max_completed_count: 0,
        max_completed_bytes: 0,
        clean_on_complete: false,
    }
}

fn sender() -> SenderConfig {
    SenderConfig {
        initial_window: 64,
        max_window: 8192,
        retry: Duration::from_secs(5),
        decay_factor: 0.5,
        decay_interval: Duration::from_secs(10),
    }
}

fn copy_config() -> CopyConfig {
    CopyConfig {
        sender: sender(),
        buffer_policy: BufferPolicy::RawSingle,
        buffer_limits: buffer_limits(),
    }
}

pub(crate) async fn create(
    context: ReplicatorFactoryContext,
    provider: Option<Arc<dyn StateProvider>>,
    settings: ReplicatorSettings,
) -> Result<ReplicatorInterfaces> {
    context.claim_native_creation()?;
    let provider = provider
        .ok_or_else(|| RuntimeError::Application("classic V1 requires a state provider".into()))?;
    if !context.public_v1_binding().identity.is_public_v1() {
        return Err(RuntimeError::AuthorityNotAdmitted);
    }
    let registry = MessageRegistry::default();
    let transport = Transport::start(
        context.clone(),
        settings.replication_address.clone(),
        registry.clone(),
    )
    .await?;
    match compose(context, provider, settings, registry, transport.clone()).await {
        Ok(interfaces) => Ok(interfaces),
        Err(failure) => {
            transport.abort();
            match transport.drain().await {
                Ok(()) => Err(failure),
                Err(cleanup) => Err(RuntimeError::Application(format!(
                    "{failure}; native setup cleanup failed: {cleanup}"
                ))),
            }
        }
    }
}

async fn compose(
    context: ReplicatorFactoryContext,
    provider: Arc<dyn StateProvider>,
    settings: ReplicatorSettings,
    registry: MessageRegistry,
    transport: Arc<Transport>,
) -> Result<ReplicatorInterfaces> {
    let local = transport.local().clone();
    let (history, _) = context.native_local_endpoint(local.address.clone()).await?;
    // The initial route only identifies the local endpoint. An authenticated
    // StartCopy supplies the native secondary's actual reply route.
    let unbound = NativeBinding {
        history,
        epoch: Epoch::default(),
        current_configuration_id: Default::default(),
        previous_configuration_id: None,
        primary: local.clone(),
        secondary: local.clone(),
        build: None,
    };
    let local_route = route(&unbound, &local)?;
    let persisted = context.public_v1_binding().state_persistence == StatePersistence::Persisted;
    let sends = transport.clone();
    let acks = transport.clone();
    let contexts = transport.clone();
    let faults = context.clone();
    let fault_tasks = transport.clone();
    let adapter = ServiceReplicator::new(
        RoleConfig {
            primary: PrimaryConfig {
                queue: queue(),
                initial_progress: 0,
                endpoint: local_route.source.clone(),
                address: local.address.clone(),
                epoch: Epoch::default(),
                allow_multiple_quorum_set: true,
                sender: sender(),
                queue_policy: QueuePolicyConfig::for_persisted(persisted),
                send: Arc::new(move |intent| sends.replication(intent)),
            },
            secondary: SecondaryConfig {
                replication: ReceiverConfig {
                    queue: queue(),
                    has_persisted_state: persisted,
                    require_service_ack: false,
                    enable_eos_ack: true,
                },
                copy: SecondaryCopyConfig {
                    initial_capacity: 64,
                    max_capacity: 1024,
                    require_service_ack: persisted,
                    enable_eos_ack: true,
                    copy_done: false,
                },
                context: copy_config(),
                batch_ack: Duration::from_millis(15),
                retry_ack: Duration::from_secs(5),
                max_pending_acks: 1024,
                trace_interval: Duration::from_secs(30),
            },
            secondary_route: local_route,
            secondary_callbacks: SecondaryCallbacks {
                ack: Arc::new(move |route, ack, _| acks.ack(&route, ack)),
                context: Arc::new(move |route, action| {
                    contexts.copy(&route, action, true, Epoch::default())
                }),
                fault: Arc::new(move |fault| {
                    tracing::error!(%fault, "native secondary reported fault");
                    let context = faults.clone();
                    fault_tasks.spawn(async move {
                        context
                            .report_fault(crate::protocol::types::FaultType::Transient)
                            .await
                    });
                }),
            },
            cached_progress: None,
        },
        provider,
        Clock::new(),
        registry,
        Arc::new(|| WriteAccess::NotPrimary),
    );
    let adapter = adapter.map_err(error)?;
    let peers: Arc<Mutex<BTreeMap<ReplicaId, PeerBinding>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let resolve = peers.clone();
    let config = FacadeConfig {
        local: local.identity.clone(),
        peers: Vec::new(),
        close_quorum_wait: Duration::from_secs(5),
        resolve_peer: Some(Arc::new(move |information| {
            resolve
                .lock()
                .unwrap()
                .get(&information.identity.replica_id)
                .filter(|bound| matches_binding(&bound.information, information))
                .cloned()
                .ok_or_else(|| FacadeError::Input("no live production peer binding".into()))
        })),
    };
    let access = context.clone();
    let observations = context.clone();
    let facade = FacadeSetup::new(adapter, config).start(
        Arc::new(move || access.native_write_status()),
        Arc::new(move |observation| observations.observe_native_lifecycle(observation)),
    )?;
    let owner = Arc::new(Production {
        facade,
        transport,
        context,
        peers,
        inbound: tokio::sync::Mutex::new(None),
        contained: tokio::sync::watch::channel(false).0,
        epoch: Mutex::new(None),
        settings_address: settings.replication_address,
    });
    Ok(ReplicatorInterfaces::primary(
        owner.clone(),
        Some(owner.clone()),
        ReplicatorCapabilities {
            catch_up_specific_quorum: true,
            data_loss_replay:
                crate::protocol::public_operations::DataLossReplay::ReplaceOnAmbiguity,
        },
    )
    .with_native_runtime_control(owner))
}

struct Production {
    facade: Arc<Facade>,
    transport: Arc<Transport>,
    context: ReplicatorFactoryContext,
    peers: Arc<Mutex<BTreeMap<ReplicaId, PeerBinding>>>,
    inbound:
        tokio::sync::Mutex<Option<crate::protocol::public_operations::PublicInboundBuildAuthority>>,
    contained: tokio::sync::watch::Sender<bool>,
    epoch: Mutex<Option<Epoch>>,
    settings_address: String,
}

impl Production {
    async fn prepare(
        &self,
        information: &ReplicaInformation,
        epoch: Epoch,
        current: crate::protocol::types::ConfigurationId,
        previous: Option<crate::protocol::types::ConfigurationId>,
        build: Option<crate::protocol::public_operations::PublicInboundBuildAuthority>,
    ) -> Result<()> {
        information.validate_public_progress()?;
        let (history, local) = self
            .context
            .native_local_endpoint(self.transport.local().address.clone())
            .await?;
        let remote = NativeEndpoint {
            identity: information.identity.clone(),
            process_session_id: information.process_session_id.clone(),
            storage: information
                .storage_incarnation
                .clone()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?,
            address: information.replication_address.clone(),
        };
        remote.validate()?;
        let binding = NativeBinding {
            history,
            epoch,
            current_configuration_id: current,
            previous_configuration_id: previous,
            primary: local.clone(),
            secondary: remote,
            build,
        };
        let native_route = route(&binding, &local)?;
        let incarnation = self.transport.admit(binding.clone(), true).await?;
        let replication = self.transport.clone();
        let copy = self.transport.clone();
        let copy_route = native_route.clone();
        let context = self.transport.clone();
        let context_route = native_route.clone();
        let context_binding = binding.clone();
        self.peers.lock().unwrap().insert(
            information.identity.replica_id,
            PeerBinding {
                information: information.clone(),
                build: BuildConfig {
                    route: native_route,
                    remote_incarnation: incarnation,
                    copy: copy_config(),
                    context: (self.context.public_v1_binding().state_persistence
                        == StatePersistence::Persisted)
                        .then_some(CopyContextConfig {
                            initial_queue_size: 64,
                            max_queue_size: 1024,
                            batch_ack_interval: Duration::from_millis(15),
                            retry_interval: Duration::from_secs(5),
                            max_pending_acknowledgements: 1024,
                            trace_interval: Duration::from_secs(30),
                        }),
                },
                callbacks: BuildCallbacks {
                    replication: Arc::new(move |intent| replication.replication(intent)),
                    copy: Arc::new(move |action| {
                        copy.bound_copy(&binding, &copy_route, action, epoch)
                    }),
                    context_ack: Arc::new(move |ack, _| {
                        context.bound_context_ack(&context_binding, &context_route, ack)
                    }),
                },
            },
        );
        Ok(())
    }

    async fn configuration(
        &self,
        current: &ReplicaSetConfiguration,
        previous: Option<&ReplicaSetConfiguration>,
    ) -> Result<()> {
        if current.configuration.primary_id != self.transport.local().identity.replica_id {
            return Err(RuntimeError::NotPrimary);
        }
        for information in current.replicas.iter().chain(
            previous
                .into_iter()
                .flat_map(|configuration| &configuration.replicas),
        ) {
            self.prepare(
                information,
                current.configuration.epoch,
                current.configuration.configuration_id.clone(),
                previous.map(|configuration| configuration.configuration.configuration_id.clone()),
                None,
            )
            .await?;
        }
        Ok(())
    }

    fn stream(&self, copy: bool) -> Result<OperationStream> {
        let service = self.facade.service();
        let stream = if copy {
            service.copy_stream()
        } else {
            service.replication_stream()
        }
        .map_err(error)?;
        let faults = service.clone();
        let kind = if copy {
            OperationKind::Copy
        } else {
            OperationKind::Replication
        };
        Ok(OperationStream::new(
            Box::pin(stream.map(move |operation| {
                let operation = operation.map_err(error)?;
                let acknowledge = operation.clone();
                Ok(ReceivedOperation::new(
                    kind,
                    operation.lsn(),
                    operation.epoch(),
                    operation.data().clone(),
                    operation.is_end_of_stream(),
                    Arc::new(move || acknowledge.acknowledge().map_err(error)),
                ))
            })),
            Arc::new(move |reason| {
                // Obtain the same native receiver, not a fresh delivery channel.
                let stream = if copy {
                    faults.copy_stream()
                } else {
                    faults.replication_stream()
                }
                .map_err(error)?;
                stream.report_fault(reason).map_err(error)
            }),
        ))
    }
}

impl Drop for Production {
    fn drop(&mut self) {
        self.facade.abort();
        self.transport.abort();
    }
}

#[async_trait]
impl Replicator for Production {
    async fn open(&self) -> Result<String> {
        self.transport.check_health()?;
        self.facade.open().await
    }
    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()> {
        self.facade.change_role(epoch, role).await?;
        *self.epoch.lock().unwrap() = Some(epoch);
        Ok(())
    }
    async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        self.facade.update_epoch(epoch).await?;
        *self.epoch.lock().unwrap() = Some(epoch);
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        self.facade.close().await
    }
    fn abort(&self) {
        self.context
            .observe_native_lifecycle(LifecycleObservation::Closed);
        self.facade.abort();
        self.transport.abort();
    }
    async fn current_progress(&self) -> Result<Lsn> {
        self.facade.current_progress().await
    }
    async fn catch_up_capability(&self) -> Result<Lsn> {
        self.facade.catch_up_capability().await
    }
}

#[async_trait]
impl PrimaryReplicator for Production {
    async fn on_data_loss(&self) -> Result<bool> {
        self.facade.on_data_loss().await
    }
    async fn update_catch_up_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
        previous: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.configuration(&current, (!empty_previous(&previous)).then_some(&previous))
            .await?;
        self.facade
            .update_catch_up_replica_set_configuration(current, previous)
            .await
    }
    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()> {
        self.facade.wait_for_catch_up_quorum(mode).await
    }
    async fn update_current_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.configuration(&current, None).await?;
        self.facade
            .update_current_replica_set_configuration(current)
            .await
    }
    async fn build_replica(&self, information: ReplicaInformation) -> Result<()> {
        let authority = self
            .context
            .native_outbound_build_authority(&information.build_id)
            .await?;
        self.prepare(
            &information,
            authority.attempt.epoch,
            authority.current_configuration_id.clone(),
            None,
            Some(authority),
        )
        .await?;
        self.facade.build_replica(information).await
    }
    async fn remove_replica(&self, replica: ReplicaId) -> Result<()> {
        self.facade.remove_replica(replica).await?;
        self.peers.lock().unwrap().remove(&replica);
        Ok(())
    }
}

#[async_trait]
impl StateReplicator for Production {
    fn begin_replicate(&self, data: OperationData) -> Result<Replication> {
        self.transport.check_health()?;
        let mut admitted = self.facade.service().begin_replicate(data).map_err(error)?;
        Ok(Replication {
            lsn: admitted.lsn,
            completion: Box::pin(async move { admitted.completion.wait().await.map_err(error) }),
        })
    }
    fn replication_operations(&self) -> Result<OperationStream> {
        self.stream(false)
    }
    fn copy_operations(&self) -> Result<OperationStream> {
        self.stream(true)
    }
    async fn replicate(&self, data: OperationData) -> Result<Lsn> {
        let admitted = self.begin_replicate(data)?;
        admitted.completion.await?;
        Ok(admitted.lsn)
    }
    async fn update_replicator_settings(&self, settings: ReplicatorSettings) -> Result<()> {
        if settings.replication_address != self.settings_address {
            return Err(RuntimeError::AuthorityMismatch(
                "native endpoint is immutable".into(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl NativeRuntimeControl for Production {
    fn containment(&self) -> tokio::sync::watch::Receiver<bool> {
        self.contained.subscribe()
    }
    fn selected_epoch(&self) -> Option<Epoch> {
        *self.epoch.lock().unwrap()
    }
    fn idle_replicas(&self) -> Result<Vec<ReplicaId>> {
        self.facade
            .service()
            .idle_replica_ids()
            .map(|ids| ids.into_iter().map(ReplicaId::new).collect())
            .map_err(error)
    }
    fn progress_query_readiness(&self) -> Result<bool> {
        self.facade
            .service()
            .progress_query_readiness()
            .map_err(error)
    }
    #[cfg(feature = "testing")]
    fn testing_tcp_binding(
        &self,
    ) -> Option<(
        crate::protocol::types::ReplicaIdentity,
        crate::protocol::types::ProcessSessionId,
        crate::replicator::ReplicatorCreationReservation,
    )> {
        self.context.reservation.map(|reservation| {
            (
                self.transport.local().identity.clone(),
                self.transport.local().process_session_id.clone(),
                reservation,
            )
        })
    }
    async fn shutdown_and_drain(&self) -> Result<()> {
        self.abort();
        let (facade, transport) = tokio::join!(self.facade.drain(), self.transport.drain());
        self.context.release_provider_writer();
        self.contained.send_replace(true);
        let failures = [facade, transport]
            .into_iter()
            .filter_map(|result| result.err().map(|error| error.to_string()))
            .collect::<Vec<_>>();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(RuntimeError::ControlPlane(failures.join("; ")))
        }
    }
    async fn install_inbound_build_authority(
        &self,
        authority: &crate::protocol::public_operations::PublicInboundBuildAuthority,
    ) -> Result<()> {
        let mut inbound = self.inbound.lock().await;
        self.context
            .classify_replacement_inbound_build_authority(authority)
            .await?;
        if inbound.as_ref().is_some_and(|current| current != authority) {
            // Native streams belong to a receiver lifetime. A new authority
            // cannot reuse dequeued application work from the old receiver.
            self.context
                .report_fault(crate::protocol::types::FaultType::Transient)
                .await?;
            self.abort();
            return Err(RuntimeError::InvalidReplication(
                "successor copy requires process replacement; prior receiver was retired".into(),
            ));
        }
        self.context
            .install_replacement_inbound_build_authority(authority)
            .await?;
        *inbound = Some(authority.clone());
        Ok(())
    }
    async fn revoke_inbound_build_authority(
        &self,
        authority: &crate::protocol::public_operations::PublicInboundBuildAuthority,
    ) -> Result<()> {
        let inbound = self.inbound.lock().await;
        if inbound.as_ref() != Some(authority) {
            return Err(RuntimeError::AuthorityMismatch(
                "inbound copy revocation does not match the current receiver".into(),
            ));
        }
        self.context
            .revoke_replacement_inbound_build_authority()
            .await
    }
    fn raw_retained_task_cardinality(&self) -> (usize, usize) {
        (
            self.transport.cardinality(),
            self.facade.retained_task_count(),
        )
    }
}
