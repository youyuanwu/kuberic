//! Composition of the delivered classic owners with the production host carrier.
//! All access/admission decisions remain in the host capability.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::{FutureExt, Stream, StreamExt};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::application::{Lsn, OperationData, OperationDataStream, StateProvider};
use crate::protocol::public_operations::StatePersistence;
use crate::protocol::types::{Epoch, ReplicaId, ReplicaRole};
use crate::replicator::stream::{OperationKind, OperationStream, ReceivedOperation};
use crate::replicator::{
    PrimaryReplicator, ReplicaInformation, ReplicaSetConfiguration, ReplicaSetQuorumMode,
    Replication, Replicator, ReplicatorCapabilities, ReplicatorFactoryContext,
    ReplicatorInterfaces, ReplicatorSettings, StateReplicator,
};
use crate::{Result, RuntimeError};

use super::super::copy_async_operation::CopyConfig;
use super::super::copy_context_receiver::CopyContextConfig;
use super::super::operation_queue::QueueConfig;
use super::super::primary::{FROZEN_PREVIOUS_INCARNATION, PrimaryConfig};
use super::super::replica_manager::QueuePolicyConfig;
use super::super::secondary::{SecondaryCallbacks, SecondaryConfig};
use super::super::secondary_copy_receiver::SecondaryCopyConfig;
use super::super::secondary_receiver::ReceiverConfig;
use super::super::sender::SenderConfig;
use super::engine_session::{Binding, Control, Fence};
use super::facade::{Facade, FacadeConfig, FacadeError, PeerBinding, empty_previous};
use super::production_transport::{Transport, buffer_limits, local_route, route};
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
    match compose(context, provider, registry, transport.clone()).await {
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
    registry: MessageRegistry,
    transport: Arc<Transport>,
) -> Result<ReplicatorInterfaces> {
    let provider: Arc<dyn StateProvider> = Arc::new(SessionProvider {
        provider,
        transport: transport.clone(),
    });
    let local = transport.local().clone();
    let local_route = local_route(transport.history(), &local)?;
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
    let mut adapter = adapter.map_err(error)?;
    adapter.service.write_admission = Some(context.native_write_admission()?);
    let peers: Arc<Mutex<BTreeMap<ReplicaId, PeerBinding>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let resolve = peers.clone();
    let config = FacadeConfig {
        local: local.identity.clone(),
        peers: Vec::new(),
        close_quorum_wait: Duration::from_secs(5),
        resolve_peer: Some(Arc::new(move |information, configuration_only| {
            resolve
                .lock()
                .unwrap()
                .get(&information.identity.replica_id)
                .filter(|bound| {
                    matches_binding(&bound.information, information)
                        && (configuration_only
                            || bound
                                .callbacks
                                .fence
                                .as_ref()
                                .is_some_and(|fence| fence.check().is_ok()))
                })
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
    transport.attach(&facade);
    #[cfg(feature = "testing")]
    {
        let driver = Arc::downgrade(&facade);
        crate::replicator::testing_transport::retain_driver_loss(
            local.identity.instance_id.as_str(),
            move || {
                let driver = driver.upgrade();
                Box::pin(async move {
                    driver
                        .ok_or_else(|| std::io::Error::other("native V1 driver released"))?
                        .lose_driver()
                        .await
                })
            },
        );
    }
    let owner = Arc::new(Production {
        facade,
        transport,
        context,
        peers,
        epoch: Mutex::new(None),
        settings_address: local.address.clone(),
        close: Mutex::new(None),
        builds: Mutex::new(BTreeMap::new()),
        recovery_tasks: tokio_util::task::TaskTracker::new(),
        recover_on_epoch: std::sync::atomic::AtomicBool::new(false),
        role_transition_pending: std::sync::atomic::AtomicBool::new(false),
        role_changing: Arc::new(AtomicBool::new(false)),
        #[cfg(feature = "testing")]
        recovery_spawned: std::sync::atomic::AtomicUsize::new(0),
    });
    owner.transport.attach_production(&owner);
    Ok(ReplicatorInterfaces::primary(
        owner.clone(),
        Some(owner.clone()),
        ReplicatorCapabilities {
            catch_up_specific_quorum: true,
            data_loss_replay:
                crate::protocol::public_operations::DataLossReplay::ReplaceOnAmbiguity,
        },
    )
    .with_effective_replication_address(local.address))
}

type CloseResult = std::result::Result<(), String>;

struct RoleChangeGuard(Arc<AtomicBool>);

impl Drop for RoleChangeGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(super) fn role_drainage_operation(
    transport: Arc<Transport>,
    role_changing: Arc<AtomicBool>,
    binding: Binding,
    operation: super::ServiceOperation,
    kind: OperationKind,
) -> Option<ReceivedOperation> {
    let admitted =
        |transport: &Transport, changing: &AtomicBool, operation: &super::ServiceOperation| {
            changing.load(Ordering::Acquire)
                && operation.is_end_of_stream()
                && operation.lsn() == 0
                && operation.epoch() == Epoch::new(0, 0)
                && operation.data().is_empty()
                && transport.selected_lifecycle().is_none()
                && transport.sessions.retiring_receiver(&binding)
        };
    if !admitted(&transport, &role_changing, &operation) {
        return None;
    }
    let receiver = binding.clone();
    Some(ReceivedOperation::new(
        kind,
        operation.lsn(),
        operation.epoch(),
        operation.data().clone(),
        true,
        Arc::new(move || {
            if !role_changing.load(Ordering::Acquire)
                || transport.selected_lifecycle().is_some()
                || !transport.sessions.retiring_receiver(&receiver)
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            operation.acknowledge().map_err(error)
        }),
    ))
}

struct SessionProvider {
    provider: Arc<dyn StateProvider>,
    transport: Arc<Transport>,
}

#[async_trait]
impl StateProvider for SessionProvider {
    async fn update_epoch(&self, epoch: Epoch, previous_epoch_last_lsn: Lsn) -> Result<()> {
        self.transport.check_health()?;
        self.provider
            .update_epoch(epoch, previous_epoch_last_lsn)
            .await
    }

    async fn last_committed_lsn(&self) -> Result<Lsn> {
        self.transport.check_health()?;
        self.provider.last_committed_lsn().await
    }

    async fn get_copy_context(&self) -> Result<OperationDataStream> {
        let binding = self
            .transport
            .sessions
            .inbound()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if binding.retained_receiver() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.transport.confirm_effect(&binding).await?;
        let fence = self.transport.fence(&binding);
        fence.check()?;
        let stream = self
            .transport
            .effect_future(&binding, self.provider.get_copy_context())
            .await??;
        #[cfg(feature = "testing")]
        self.transport
            .test_cut(
                crate::replicator::TestingTcpRouteKind::AfterContext,
                &binding,
            )
            .await?;
        self.transport.confirm_effect(&binding).await?;
        fence
            .check()
            .map_err(|_| RuntimeError::OperationCancelled)?;
        Ok(self.transport.confirmed_stream(binding, stream))
    }

    async fn get_copy_state(
        &self,
        up_to_lsn: Lsn,
        copy_context: OperationDataStream,
    ) -> Result<OperationDataStream> {
        self.transport.check_health()?;
        self.provider.get_copy_state(up_to_lsn, copy_context).await
    }

    async fn on_data_loss(&self) -> Result<bool> {
        self.transport.check_health()?;
        self.provider.on_data_loss().await
    }
}

pub(super) struct Production {
    facade: Arc<Facade>,
    transport: Arc<Transport>,
    context: ReplicatorFactoryContext,
    peers: Arc<Mutex<BTreeMap<ReplicaId, PeerBinding>>>,
    epoch: Mutex<Option<Epoch>>,
    settings_address: String,
    close: Mutex<Option<watch::Receiver<Option<CloseResult>>>>,
    builds: Mutex<BTreeMap<String, watch::Receiver<Option<CloseResult>>>>,
    recovery_tasks: tokio_util::task::TaskTracker,
    recover_on_epoch: std::sync::atomic::AtomicBool,
    role_transition_pending: std::sync::atomic::AtomicBool,
    role_changing: Arc<AtomicBool>,
    #[cfg(feature = "testing")]
    recovery_spawned: std::sync::atomic::AtomicUsize,
}

impl Production {
    async fn settle_changed_role(&self) -> Result<()> {
        if self
            .role_transition_pending
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.transport.settle_role_sessions().await?;
            self.role_transition_pending
                .store(false, std::sync::atomic::Ordering::Release);
        }
        Ok(())
    }

    #[cfg(feature = "testing")]
    async fn configuration_cut(
        &self,
        kind: crate::replicator::TestingTcpRouteKind,
        configuration: &crate::protocol::types::ConfigurationDescriptor,
    ) -> Result<()> {
        for member in &configuration.members {
            if let Some(binding) = self
                .transport
                .sessions
                .binding_for(member.identity.replica_id.value())
            {
                self.transport.test_cut(kind, &binding).await?;
                break;
            }
        }
        Ok(())
    }
    #[cfg(feature = "testing")]
    pub(super) fn inspect(&self) -> serde_json::Value {
        let builds = self.builds.lock().unwrap();
        let native_configuration = match self.facade.service().target() {
            Ok(Target::Primary(primary)) => primary.inspect_configuration(),
            _ => serde_json::Value::Null,
        };
        serde_json::json!({
            "nativeConfiguration": native_configuration,
            "retainedResults": builds.len(),
            "pendingResults": builds.values().filter(|result| result.borrow().is_none()).count(),
            "recoveryWork": self.recovery_tasks.len(),
            "recoverySpawned": self.recovery_spawned.load(std::sync::atomic::Ordering::Acquire),
            "peers": self.peers.lock().unwrap().values().map(|peer| serde_json::json!({
                "identity": peer.information.identity,
                "process": peer.information.process_session_id,
                "address": peer.information.replication_address,
            })).collect::<Vec<_>>(),
        })
    }

    pub(super) async fn recover_source(&self, binding: &Binding, completed: bool) -> Result<()> {
        if !binding.recovery || self.transport.local() != &binding.primary() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if completed {
            self.transport.sessions.retained_recovery(binding)?;
            let mut receiver = self
                .builds
                .lock()
                .unwrap()
                .get(&binding.nonce)
                .cloned()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            let outcome = loop {
                let outcome = receiver.borrow().clone();
                if let Some(outcome) = outcome {
                    break outcome;
                }
                receiver.changed().await.map_err(|_| {
                    RuntimeError::ControlPlane("recovery result owner was lost".into())
                })?;
            };
            outcome.map_err(RuntimeError::ControlPlane)?;
            self.transport.authorize(binding).await?;
            return self
                .transport
                .sessions
                .completed(binding)
                .then_some(())
                .ok_or(RuntimeError::AuthorityNotAdmitted);
        }
        binding.validate()?;
        let mut builds = self.builds.lock().unwrap();
        let old = self.transport.sessions.reserve_recovery(binding)?;
        if let Some(result) = builds.get(&binding.nonce) {
            return result
                .borrow()
                .clone()
                .unwrap_or(Ok(()))
                .map_err(RuntimeError::ControlPlane);
        }
        // Insert before launching: retries attach to this exact retained work.
        let (result, receiver) = watch::channel(None);
        builds.insert(binding.nonce.clone(), receiver);
        drop(builds);
        let transport = self.transport.clone();
        let facade = self.facade.clone();
        let peers = self.peers.clone();
        let binding = binding.clone();
        let persisted =
            self.context.public_v1_binding().state_persistence == StatePersistence::Persisted;
        #[cfg(feature = "testing")]
        self.recovery_spawned
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        transport
            .clone()
            .spawn(self.recovery_tasks.track_future(async move {
                let outcome = async {
                    let source = transport.publication().await?;
                    transport.sessions.recovery_pending(&old, &binding)?;
                    if source != binding.source {
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    }
                    #[cfg(feature = "testing")]
                    transport
                        .test_cut(
                            crate::replicator::TestingTcpRouteKind::RecoveryDiscovery,
                            &binding,
                        )
                        .await?;
                    let target = transport
                        .recovery_peer(
                            &binding.secondary().identity,
                            &binding.secondary().process_session_id,
                            binding.history(),
                        )
                        .await?;
                    transport.sessions.recovery_pending(&old, &binding)?;
                    if target != binding.target {
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    }
                    if transport.publication().await? != binding.source {
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    }
                    transport.sessions.recovery_pending(&old, &binding)?;
                    transport.join_predecessor(&old).await?;
                    let binding = transport
                        .sessions
                        .recovery_proposal(&old, binding.clone())?;
                    let incarnation = transport.establish(&binding).await?;
                    let target = binding.secondary();
                    let information = ReplicaInformation {
                        identity: target.identity,
                        replication_address: target.address,
                        process_session_id: target.process_session_id,
                        storage_incarnation: Some(target.storage),
                        role: ReplicaRole::ActiveSecondary,
                        current_progress: crate::replicator::INVALID_LSN,
                        catch_up_capability: crate::replicator::INVALID_LSN,
                        must_catchup: false,
                    };
                    let fence = transport.fence(&binding);
                    transport.sessions.effect(&binding, || {
                        Self::prepare(
                            &transport,
                            &peers,
                            persisted,
                            &information,
                            binding.clone(),
                            fence,
                            incarnation,
                        )
                    })??;
                    facade.recover(information).await?;
                    transport.authorize_completion(&binding).await?;
                    transport.control(&binding, Control::Complete).await?;
                    transport.authorize_completion(&binding).await?;
                    transport.sessions.complete(&binding)
                }
                .await;
                let failed = outcome.is_err();
                result.send_replace(Some(outcome.map_err(|e| e.to_string())));
                if failed {
                    transport.sessions.retire(&binding)?;
                }
                Ok(())
            }));
        Ok(())
    }

    async fn recover_secondary(&self, epoch: Epoch) -> Result<()> {
        let target = self.transport.publication().await?;
        if target.scope.epoch != epoch
            || target.stage
                != crate::protocol::public_operations::ReplicaPublicationStage::DiscoveryPending
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let primary = target
            .scope
            .configuration
            .replicas
            .iter()
            .find(|peer| {
                peer.identity.replica_id == target.scope.configuration.current.primary_id
                    && peer.role == ReplicaRole::Primary
            })
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let source = self
            .transport
            .recovery_peer(
                &primary.identity,
                &primary.process_session_id,
                &target.scope.history,
            )
            .await?;
        let binding = Binding {
            nonce: uuid::Uuid::new_v4().to_string(),
            source,
            target,
            recovery: true,
            continuation: None,
            reformation: false,
        };
        binding.validate()?;
        self.transport.sessions.reserve(&binding)?;
        self.transport.control(&binding, Control::Recover).await?;
        self.transport.control(&binding, Control::Recovered).await?;
        self.transport.confirm_effect(&binding).await
    }

    fn prepare(
        transport: &Arc<Transport>,
        peers: &Arc<Mutex<BTreeMap<ReplicaId, PeerBinding>>>,
        persisted: bool,
        information: &ReplicaInformation,
        binding: Binding,
        fence: Fence,
        incarnation: i64,
    ) -> Result<()> {
        information.validate_public_progress()?;
        let native_route = route(&binding, transport.local())?;
        let epoch = binding.epoch();
        let replication = transport.clone();
        let copy = transport.clone();
        let copy_route = native_route.clone();
        let context = transport.clone();
        let context_route = native_route.clone();
        let context_binding = binding.clone();
        peers.lock().unwrap().insert(
            information.identity.replica_id,
            PeerBinding {
                information: information.clone(),
                build: BuildConfig {
                    route: native_route,
                    remote_incarnation: incarnation,
                    copy: copy_config(),
                    context: persisted.then_some(CopyContextConfig {
                        initial_queue_size: 64,
                        max_queue_size: 1024,
                        batch_ack_interval: Duration::from_millis(15),
                        retry_interval: Duration::from_secs(5),
                        max_pending_acknowledgements: 1024,
                        trace_interval: Duration::from_secs(30),
                    }),
                },
                callbacks: BuildCallbacks {
                    fence: Some(fence),
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
        self.settle_changed_role().await?;
        if let Some(previous) = previous {
            for information in &previous.replicas {
                self.retained_previous(information, current, Some(previous))
                    .await?;
            }
        }
        let mut staged = BTreeMap::new();
        for information in current.replicas.iter().chain(
            previous
                .into_iter()
                .flat_map(|configuration| &configuration.replicas),
        ) {
            if staged.contains_key(&information.identity.replica_id)
                || self
                    .retained_previous(information, current, previous)
                    .await?
                || self
                    .transport
                    .sessions
                    .binding_for(information.identity.replica_id.value())
                    .is_some()
            {
                continue;
            }
            staged.insert(
                information.identity.replica_id,
                self.admitted_proposal(information).await?,
            );
        }
        let mut recoveries = Vec::new();
        for information in current.replicas.iter().chain(
            previous
                .into_iter()
                .flat_map(|configuration| &configuration.replicas),
        ) {
            if self
                .retained_previous(information, current, previous)
                .await
                .map_err(|error| {
                    RuntimeError::ControlPlane(format!(
                        "retained PC descriptor {}: {error}",
                        information.identity.replica_id
                    ))
                })?
            {
                continue;
            }
            let planned = staged.remove(&information.identity.replica_id);
            if let Some(old) = self
                .transport
                .sessions
                .binding_for(information.identity.replica_id.value())
                .filter(|old| {
                    self.transport.sessions.active(old).is_err()
                        && old.epoch() < current.configuration.epoch
                })
            {
                self.continue_session(&old, information, current, previous)
                    .await
                    .map_err(|error| {
                        RuntimeError::ControlPlane(format!(
                            "epoch continuation {}: {error}",
                            information.identity.replica_id
                        ))
                    })?;
            }
            if let Some(binding) = self
                .transport
                .sessions
                .binding_for(information.identity.replica_id.value())
                .filter(|binding| {
                    planned.is_none()
                        && binding.reformation
                        && !self.transport.sessions.completed(binding)
                })
            {
                let mut result = self
                    .builds
                    .lock()
                    .unwrap()
                    .get(&binding.nonce)
                    .cloned()
                    .ok_or(RuntimeError::AuthorityNotAdmitted)?;
                loop {
                    if let Some(outcome) = result.borrow().clone() {
                        outcome.map_err(RuntimeError::ControlPlane)?;
                        break;
                    }
                    result.changed().await.map_err(|_| {
                        RuntimeError::ControlPlane("native reformation owner was lost".into())
                    })?;
                }
                self.transport.validate_publications(&binding).await?;
                if !recoveries
                    .iter()
                    .any(|known: &Binding| known.nonce == binding.nonce)
                {
                    recoveries.push(binding);
                }
                continue;
            }
            if planned.is_some()
                || self
                    .transport
                    .sessions
                    .binding_for(information.identity.replica_id.value())
                    .is_none()
            {
                let binding = if let Some(binding) = planned {
                    binding
                } else {
                    self.admitted_proposal(information).await?
                };
                if binding.reformation {
                    self.reform_session(binding.clone(), information.clone())
                        .await?;
                    recoveries.push(binding);
                    continue;
                }
                let binding = self.transport.sessions.proposal(binding)?;
                let incarnation = self.transport.establish(&binding).await?;
                let fence = self.transport.fence(&binding);
                let persisted = self.context.public_v1_binding().state_persistence
                    == StatePersistence::Persisted;
                self.transport.sessions.effect(&binding, || {
                    Self::prepare(
                        &self.transport,
                        &self.peers,
                        persisted,
                        information,
                        binding.clone(),
                        fence,
                        incarnation,
                    )
                })??;
                recoveries.push(binding);
            }
        }
        if !recoveries.is_empty() {
            if let Some(previous) = previous {
                self.facade
                    .update_catch_up_replica_set_configuration(current.clone(), previous.clone())
                    .await?;
            } else {
                self.facade
                    .update_catch_up_replica_set_configuration(
                        current.clone(),
                        ReplicaSetConfiguration {
                            configuration: crate::protocol::types::ConfigurationDescriptor::new(
                                current.configuration.epoch,
                                current.configuration.primary_id,
                                Vec::new(),
                                0,
                            ),
                            replicas: Vec::new(),
                        },
                    )
                    .await?;
            }
            for binding in recoveries {
                self.transport.authorize_completion(&binding).await?;
                self.transport.control(&binding, Control::Complete).await?;
                self.transport.authorize_completion(&binding).await?;
                self.transport.sessions.complete(&binding)?;
            }
        }
        for information in current.replicas.iter().chain(
            previous
                .into_iter()
                .flat_map(|configuration| &configuration.replicas),
        ) {
            if self
                .retained_previous(information, current, previous)
                .await
                .map_err(|error| {
                    RuntimeError::ControlPlane(format!(
                        "retained PC validation {}: {error}",
                        information.identity.replica_id
                    ))
                })?
            {
                continue;
            }
            if !self
                .peers
                .lock()
                .unwrap()
                .get(&information.identity.replica_id)
                .is_some_and(|peer| matches_binding(&peer.information, information))
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            let binding = self
                .transport
                .sessions
                .binding_for(information.identity.replica_id.value())
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            if !self.transport.sessions.completed(&binding) {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            self.transport.authorize(&binding).await.map_err(|error| {
                RuntimeError::ControlPlane(format!(
                    "configured session {}: {error}",
                    information.identity.replica_id
                ))
            })?;
            if binding.recovery {
                let target = self
                    .transport
                    .recovery_peer(
                        &binding.secondary().identity,
                        &binding.secondary().process_session_id,
                        binding.history(),
                    )
                    .await?;
                if target.stage
                    != crate::protocol::public_operations::ReplicaPublicationStage::AchievedScope
                    || target.role != ReplicaRole::ActiveSecondary
                {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
            }
        }
        Ok(())
    }

    async fn admitted_proposal(&self, information: &ReplicaInformation) -> Result<Binding> {
        let source = self.transport.publication().await?;
        let target = self
            .transport
            .discover(
                &information.identity,
                &information.process_session_id,
                &source.scope.history,
            )
            .await?;
        let binding = Binding::new(source, target)?;
        if !binding.admitted_reconfiguration() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let mut candidate = binding.clone();
        candidate.reformation = true;
        let binding = if self
            .transport
            .control(&candidate, Control::CheckReformation)
            .await
            .is_ok()
        {
            candidate
        } else {
            binding
        };
        self.transport.sessions.proposal(binding)
    }

    async fn retained_previous(
        &self,
        information: &ReplicaInformation,
        current: &ReplicaSetConfiguration,
        previous: Option<&ReplicaSetConfiguration>,
    ) -> Result<bool> {
        if current
            .configuration
            .members
            .iter()
            .any(|member| member.identity == information.identity)
            || !previous.is_some_and(|previous| {
                previous
                    .configuration
                    .members
                    .iter()
                    .any(|member| member.identity == information.identity)
            })
        {
            return Ok(false);
        }
        let publication = self.transport.publication().await?;
        let Some(description) = publication
            .scope
            .configuration
            .replicas
            .iter()
            .find(|peer| peer.identity == information.identity)
        else {
            return Ok(false);
        };
        if description.is_up {
            return Ok(false);
        }
        let existing = self
            .transport
            .sessions
            .binding_for(information.identity.replica_id.value())
            .filter(|binding| self.transport.sessions.retained_completed(binding));
        let Some(binding) = existing else {
            // No prior outbound session exists on a newly promoted/reopened
            // Primary. Only the exact selected unavailable PC-only descriptor
            // may populate native quorum metadata; it never gets a carrier.
            let source = self.transport.publication().await?;
            if source.scope.epoch != current.configuration.epoch
                || source.scope.configuration.current != current.configuration
                || source.scope.configuration.previous
                    != previous.map(|previous| previous.configuration.clone())
                || self.transport.selected_lifecycle()
                    != Some((source.scope.epoch, ReplicaRole::Primary))
                || self
                    .transport
                    .sessions
                    .binding_for(information.identity.replica_id.value())
                    .is_some()
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            let descriptor = source
                .scope
                .configuration
                .replicas
                .iter()
                .find(|peer| {
                    peer.identity == information.identity
                        && Some(&peer.storage_incarnation)
                            == information.storage_incarnation.as_ref()
                        && peer.process_session_id == information.process_session_id
                        && peer.replication_address == information.replication_address
                        && peer.role == information.role
                        && !peer.is_up
                        && !peer.is_ready
                        && peer.current_progress == crate::application::INVALID_LSN
                        && peer.catch_up_capability == crate::application::INVALID_LSN
                        && information.current_progress == crate::application::INVALID_LSN
                        && information.catch_up_capability == crate::application::INVALID_LSN
                })
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            let mut native_route = local_route(&source.scope.history, self.transport.local())?;
            let mut digest = Sha256::new();
            digest.update(b"kuberic-v1-frozen-previous-only");
            digest.update(
                serde_json::to_vec(descriptor)
                    .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))?,
            );
            native_route.destination.replica = information.identity.replica_id.value();
            native_route.destination.incarnation =
                bytes::Bytes::copy_from_slice(&digest.finalize()[..16]);
            native_route.remote_address = information.replication_address.clone();
            self.peers.lock().unwrap().insert(
                information.identity.replica_id,
                PeerBinding {
                    information: information.clone(),
                    build: BuildConfig {
                        route: native_route,
                        remote_incarnation: FROZEN_PREVIOUS_INCARNATION,
                        copy: copy_config(),
                        context: None,
                    },
                    callbacks: BuildCallbacks {
                        fence: Some(Fence::denied()),
                        replication: Arc::new(|_| false),
                        copy: Arc::new(|_| false),
                        context_ack: Arc::new(|_, _| false),
                    },
                },
            );
            return Ok(true);
        };
        if !self
            .peers
            .lock()
            .unwrap()
            .get(&information.identity.replica_id)
            .is_some_and(|peer| matches_binding(&peer.information, information))
            || binding.primary() != *self.transport.local()
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let native_route = route(&binding, self.transport.local())?;
        let identity = super::super::session::SessionIdentity {
            primary_endpoint: native_route.sender_address.clone(),
            remote_endpoint: native_route.remote_address.clone(),
            remote_incarnation: self.transport.native_incarnation(&binding)?,
        };
        match self.facade.service().target().map_err(error)? {
            Target::Primary(primary) => {
                primary
                    .continuation_session(&native_route, &identity, current.configuration.epoch)
                    .map_err(|failure| error(failure.into()))?;
            }
            _ => return Err(RuntimeError::NotPrimary),
        }
        Ok(true)
    }

    pub(super) fn frozen_previous_peer(
        &self,
        publication: &crate::protocol::public_operations::ReplicaPublication,
        peer: &crate::protocol::public_operations::ReplicaPeerDescription,
    ) -> bool {
        !peer.is_up
            && !peer.is_ready
            && !peer.must_catchup
            && peer.current_progress == crate::application::INVALID_LSN
            && peer.catch_up_capability == crate::application::INVALID_LSN
            && !publication
                .scope
                .configuration
                .current
                .members
                .iter()
                .any(|member| member.identity == peer.identity)
            && publication
                .scope
                .configuration
                .previous
                .as_ref()
                .is_some_and(|previous| {
                    previous
                        .members
                        .iter()
                        .any(|member| member.identity == peer.identity && member.role == peer.role)
                })
            && self.transport.selected_lifecycle()
                == Some((publication.scope.epoch, ReplicaRole::Primary))
            && self
                .peers
                .lock()
                .unwrap()
                .get(&peer.identity.replica_id)
                .is_some_and(|bound| {
                    bound.build.remote_incarnation == FROZEN_PREVIOUS_INCARNATION
                        && bound.information.identity == peer.identity
                        && bound.information.process_session_id == peer.process_session_id
                        && bound.information.storage_incarnation.as_ref()
                            == Some(&peer.storage_incarnation)
                        && bound.information.replication_address == peer.replication_address
                        && bound.information.role == peer.role
                        && bound
                            .callbacks
                            .fence
                            .as_ref()
                            .is_some_and(|fence| fence.check().is_err())
                })
    }

    async fn reform_session(
        &self,
        binding: Binding,
        information: ReplicaInformation,
    ) -> Result<()> {
        let source = &binding.source;
        if self.transport.selected_lifecycle() != Some((source.scope.epoch, ReplicaRole::Primary)) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let binding = self.transport.sessions.proposal(binding)?;
        let persisted =
            self.context.public_v1_binding().state_persistence == StatePersistence::Persisted;
        let transport = self.transport.clone();
        let peers = self.peers.clone();
        let (mut receiver, work) = {
            let mut builds = self.builds.lock().unwrap();
            if let Some(existing) = builds.get(&binding.nonce) {
                (existing.clone(), None)
            } else {
                let (result, receiver) = watch::channel(None);
                builds.insert(binding.nonce.clone(), receiver.clone());
                (receiver, Some(result))
            }
        };
        if let Some(result) = work {
            transport.clone().spawn(async move {
                let outcome: Result<()> = async {
                    let _continuation = transport.continuations.lock().await;
                    transport.validate_publications(&binding).await?;
                    transport.control(&binding, Control::Reform).await?;
                    let incarnation = transport.admit(binding.clone(), true).await?;
                    let fence = transport.fence(&binding);
                    transport.sessions.effect(&binding, || {
                        Self::prepare(
                            &transport,
                            &peers,
                            persisted,
                            &information,
                            binding.clone(),
                            fence,
                            incarnation,
                        )
                    })??;
                    // Configuration, not Build/copy, creates the new sender.
                    Ok(())
                }
                .await;
                if outcome.is_err() {
                    transport.sessions.retire(&binding)?;
                }
                result.send_replace(Some(outcome.map_err(|failure| failure.to_string())));
                Ok(())
            });
        }
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result.map_err(RuntimeError::ControlPlane);
            }
            receiver.changed().await.map_err(|_| {
                RuntimeError::ControlPlane("native reformation owner was lost".into())
            })?;
        }
    }

    async fn continue_session(
        &self,
        old: &Binding,
        information: &ReplicaInformation,
        current: &ReplicaSetConfiguration,
        previous: Option<&ReplicaSetConfiguration>,
    ) -> Result<()> {
        let source = self.transport.publication().await?;
        if source.scope.epoch != current.configuration.epoch
            || source.scope.configuration.current != current.configuration
            || source.scope.configuration.previous
                != previous.map(|previous| previous.configuration.clone())
            || self.transport.selected_lifecycle()
                != Some((source.scope.epoch, ReplicaRole::Primary))
            || !self
                .peers
                .lock()
                .unwrap()
                .get(&information.identity.replica_id)
                .is_some_and(|peer| matches_binding(&peer.information, information))
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }

        let target = self
            .transport
            .discover(
                &information.identity,
                &information.process_session_id,
                &source.scope.history,
            )
            .await?;
        let mut binding = Binding::new(source, target)?;
        binding.continuation = Some(old.nonce.clone());
        if self.transport.sessions.continuation_predecessor(&binding)? != *old {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let native_route = route(old, self.transport.local())?;
        let identity = super::super::session::SessionIdentity {
            primary_endpoint: native_route.sender_address.clone(),
            remote_endpoint: native_route.remote_address.clone(),
            remote_incarnation: self.transport.native_incarnation(old)?,
        };
        let native = match self.facade.service().target().map_err(error)? {
            Target::Primary(primary) => primary
                .continuation_session(&native_route, &identity, binding.epoch())
                .map_err(|failure| error(failure.into()))?,
            _ => return Err(RuntimeError::NotPrimary),
        };
        self.transport
            .control(&binding, Control::CheckContinuation)
            .await?;
        let persisted =
            self.context.public_v1_binding().state_persistence == StatePersistence::Persisted;
        let transport = self.transport.clone();
        let peers = self.peers.clone();
        let old = old.clone();
        let information = information.clone();
        let (result, mut receiver) = watch::channel(None);
        self.builds
            .lock()
            .unwrap()
            .insert(binding.nonce.clone(), receiver.clone());
        transport.clone().spawn(async move {
            let outcome = async {
                let _continuation = transport.continuations.lock().await;
                transport.sessions.continuation_predecessor(&binding)?;
                transport.join_predecessor(&old).await?;
                transport.validate_publications(&binding).await?;
                transport
                    .sessions
                    .continue_session(&binding, false, || Ok(()))?;
                transport.control(&binding, Control::Continue).await?;
                let incarnation = transport.admit(binding.clone(), true).await?;
                let fence = transport.fence(&binding);
                transport.sessions.effect(&binding, || {
                    if !native.continue_transport(&identity, binding.epoch(), incarnation) {
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    }
                    Self::prepare(
                        &transport,
                        &peers,
                        persisted,
                        &information,
                        binding.clone(),
                        fence,
                        incarnation,
                    )
                })??;
                transport.authorize_completion(&binding).await?;
                transport.control(&binding, Control::Complete).await?;
                transport.authorize_completion(&binding).await?;
                transport.sessions.complete(&binding)
            }
            .await;
            if outcome.is_err() {
                transport.sessions.retire(&binding)?;
            }
            result.send_replace(Some(outcome.map_err(|failure| failure.to_string())));
            Ok(())
        });
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result.map_err(RuntimeError::ControlPlane);
            }
            receiver.changed().await.map_err(|_| {
                RuntimeError::ControlPlane("native continuation owner was lost".into())
            })?;
        }
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
        let transport = self.transport.clone();
        let acknowledgements = transport.acknowledgements();
        let role_changing = self.role_changing.clone();
        let stream_binding = Arc::new(Mutex::new(transport.sessions.inbound()));
        let selection = transport.clone();
        let selected_binding = stream_binding.clone();
        let mut stream = Box::pin(stream);
        let stream = futures::stream::poll_fn(move |cx| {
            let mut origin = selected_binding.lock().unwrap();
            selection
                .sessions
                .poll_delivery(
                    &mut origin,
                    |value: &Option<std::result::Result<super::ServiceOperation, AdapterError>>| {
                        !copy
                            && matches!(value, Some(Ok(operation)) if !operation.is_end_of_stream())
                    },
                    || stream.as_mut().poll_next(cx),
                )
                .map(|(value, binding)| {
                    value.map(|operation| {
                        let operation = operation.map_err(error)?;
                        Ok::<_, RuntimeError>((
                            operation,
                            binding.ok_or(RuntimeError::AuthorityNotAdmitted)?,
                        ))
                    })
                })
        });
        Ok(OperationStream::new(
            Box::pin(stream.then(move |selected| {
                let transport = transport.clone();
                let acknowledgements = acknowledgements.clone();
                let role_changing = role_changing.clone();
                async move {
                    let (operation, binding) = selected?;
                    if let Some(drainage) = role_drainage_operation(
                        transport.clone(),
                        role_changing,
                        binding.clone(),
                        operation.clone(),
                        kind,
                    ) {
                        return Ok(drainage);
                    }
                    // Reserve before delivery: held operations backpressure the native
                    // stream, not an unbounded set of retained confirmation tasks.
                    let permit = acknowledgements
                        .clone()
                        .reserve_owned()
                        .await
                        .map_err(|_| RuntimeError::Closed)?;
                    let permit = Mutex::new(Some(permit));
                    #[cfg(feature = "testing")]
                    transport
                        .test_cut(
                            crate::replicator::TestingTcpRouteKind::BeforeDelivery,
                            &binding,
                        )
                        .await?;
                    transport.confirm_effect(&binding).await?;
                    let acknowledge = operation.clone();
                    let ack_transport = transport.clone();
                    transport.sessions.effect(&binding.clone(), || {
                        ReceivedOperation::new(
                            kind,
                            operation.lsn(),
                            operation.epoch(),
                            operation.data().clone(),
                            operation.is_end_of_stream(),
                            Arc::new(move || {
                                ack_transport.sessions.effect(&binding, || {
                                    let permit =
                                        permit.lock().unwrap().take().ok_or_else(|| {
                                            RuntimeError::InvalidReplication(
                                                "operation already acknowledged".into(),
                                            )
                                        })?;
                                    if acknowledgements.is_closed() {
                                        return Err(RuntimeError::Closed);
                                    }
                                    let acknowledge = acknowledge.clone();
                                    ack_transport.queue_ack(
                                        binding.clone(),
                                        permit,
                                        Box::new(move || acknowledge.acknowledge().map_err(error)),
                                    )
                                })?
                            }),
                        )
                    })
                }
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

    async fn settle(&self) -> Result<()> {
        self.abort();
        let mut result = {
            let mut close = self.close.lock().unwrap();
            close
                .get_or_insert_with(|| {
                    let (result, receiver) = watch::channel(None);
                    let facade = self.facade.clone();
                    let transport = self.transport.clone();
                    tokio::spawn(async move {
                        let joined = AssertUnwindSafe(async {
                            let (facade, transport) =
                                tokio::join!(facade.drain(), transport.drain());
                            let failures = [facade, transport]
                                .into_iter()
                                .filter_map(|result| result.err().map(|error| error.to_string()))
                                .collect::<Vec<_>>();
                            if failures.is_empty() {
                                Ok(())
                            } else {
                                Err(failures.join("; "))
                            }
                        })
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|_| Err("built-in terminal join owner panicked".into()));
                        result.send_replace(Some(joined));
                    });
                    receiver
                })
                .clone()
        };
        loop {
            if let Some(result) = result.borrow().clone() {
                return result.map_err(RuntimeError::ControlPlane);
            }
            result.changed().await.map_err(|_| {
                RuntimeError::ControlPlane("built-in terminal join owner was lost".into())
            })?;
        }
    }
}

impl Drop for Production {
    fn drop(&mut self) {
        self.facade.abort();
        self.transport.abort();
    }
}

#[cfg(test)]
#[path = "production_test_control.rs"]
mod test_control;
#[cfg(test)]
pub(super) use test_control::{test_compose, test_control, test_provider};

#[async_trait]
impl Replicator for Production {
    async fn open(&self) -> Result<String> {
        self.transport.check_health()?;
        self.facade.open().await
    }
    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()> {
        tracing::debug!(?epoch, ?role, prior = ?self.transport.selected_lifecycle(), "ordinary native role selected");
        if *self.epoch.lock().unwrap() == Some(epoch)
            && self.transport.selected_lifecycle() == Some((epoch, role))
        {
            return self.transport.check_health();
        }
        let prior = self.transport.selected_lifecycle();
        let native_epoch = *self.epoch.lock().unwrap();
        if prior.is_some_and(|(old_epoch, old_role)| {
            native_epoch == Some(old_epoch) && old_role == role && old_epoch != epoch
        }) {
            return self.update_epoch(epoch).await;
        }
        let recovering = prior.is_none() && role == ReplicaRole::ActiveSecondary;
        let transition = prior.is_some_and(|(_, old_role)| {
            old_role != role
                && !(old_role == ReplicaRole::IdleSecondary && role == ReplicaRole::ActiveSecondary)
        });
        self.role_changing.store(transition, Ordering::Release);
        let _role_change = RoleChangeGuard(self.role_changing.clone());
        self.transport.begin_role_change(epoch, role);
        self.facade
            .change_role(epoch, role)
            .await
            .inspect_err(|error| {
                tracing::debug!(?epoch, ?role, %error, "ordinary native role failed");
            })?;
        self.recover_on_epoch
            .store(recovering, std::sync::atomic::Ordering::Release);
        self.role_transition_pending
            .store(transition, std::sync::atomic::Ordering::Release);
        self.transport.selected_role(epoch, role);
        *self.epoch.lock().unwrap() = Some(epoch);
        Ok(())
    }
    async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        tracing::debug!(?epoch, selected = ?self.transport.selected_lifecycle(), "ordinary native epoch selected");
        let selected = self.transport.selected_lifecycle();
        if selected == Some((epoch, ReplicaRole::ActiveSecondary))
            && self.transport.sessions.inbound().is_none()
            && self
                .recover_on_epoch
                .load(std::sync::atomic::Ordering::Acquire)
        {
            self.recover_secondary(epoch).await?;
        }
        if selected == Some((epoch, ReplicaRole::IdleSecondary)) {
            // Initial idle selection has no replication queue yet. StartCopy
            // initializes its native epoch/boundary; repeating that selected
            // epoch is not an external epoch advance through incomplete copy.
            self.transport.check_health()?;
            return Ok(());
        }
        if let Some((_, role)) = selected {
            self.transport.begin_role_change(epoch, role);
        }
        self.facade.update_epoch(epoch).await.inspect_err(|error| {
            tracing::debug!(?epoch, %error, "ordinary native epoch failed");
        })?;
        self.settle_changed_role().await.inspect_err(|error| {
            tracing::debug!(?epoch, %error, "ordinary native role session settlement failed");
        })?;
        if let Some((_, role)) = selected {
            self.transport.selected_role(epoch, role);
        }
        *self.epoch.lock().unwrap() = Some(epoch);
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        self.settle().await
    }
    fn abort(&self) {
        self.transport.sessions.abort();
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
        #[cfg(feature = "testing")]
        self.configuration_cut(
            crate::replicator::TestingTcpRouteKind::ConfigurationValidated,
            &current.configuration,
        )
        .await?;
        let selected = (
            current.configuration.clone(),
            (!empty_previous(&previous)).then(|| previous.configuration.clone()),
        );
        self.facade
            .update_catch_up_replica_set_configuration(current, previous)
            .await?;
        #[cfg(feature = "testing")]
        self.configuration_cut(
            crate::replicator::TestingTcpRouteKind::ConfigurationAdmitted,
            &selected.0,
        )
        .await?;
        self.transport
            .selected_configuration(selected.0, selected.1);
        Ok(())
    }
    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()> {
        self.facade.wait_for_catch_up_quorum(mode).await
    }
    async fn update_current_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.configuration(&current, None).await?;
        #[cfg(feature = "testing")]
        self.configuration_cut(
            crate::replicator::TestingTcpRouteKind::ConfigurationValidated,
            &current.configuration,
        )
        .await?;
        let selected = current.configuration.clone();
        self.facade
            .update_current_replica_set_configuration(current)
            .await?;
        #[cfg(feature = "testing")]
        self.configuration_cut(
            crate::replicator::TestingTcpRouteKind::ConfigurationAdmitted,
            &selected,
        )
        .await?;
        self.transport.selected_configuration(selected, None);
        Ok(())
    }
    async fn build_replica(&self, information: ReplicaInformation) -> Result<()> {
        information.validate_public_progress()?;
        let source = self.transport.publication().await?;
        let target = self
            .transport
            .discover(
                &information.identity,
                &information.process_session_id,
                &source.scope.history,
            )
            .await?;
        if target.replication_address != information.replication_address
            || information.storage_incarnation.as_ref() != Some(&target.scope.storage)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let binding = self
            .transport
            .sessions
            .proposal(Binding::new(source, target)?)?;
        if binding.retained_receiver() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let mut result = {
            let mut builds = self.builds.lock().unwrap();
            if let Some(result) = builds.get(&binding.nonce) {
                result.clone()
            } else {
                let facade = self.facade.clone();
                let transport = self.transport.clone();
                let peers = self.peers.clone();
                let persisted = self.context.public_v1_binding().state_persistence
                    == StatePersistence::Persisted;
                let binding = binding.clone();
                let nonce = binding.nonce.clone();
                let (result, receiver) = watch::channel(None);
                transport.clone().spawn(async move {
                    let outcome = async {
                        let incarnation = transport.establish(&binding).await?;
                        let fence = transport.fence(&binding);
                        transport.sessions.effect(&binding, || {
                            Self::prepare(
                                &transport,
                                &peers,
                                persisted,
                                &information,
                                binding.clone(),
                                fence,
                                incarnation,
                            )
                        })??;
                        transport.authorize(&binding).await?;
                        facade.build_replica(information).await?;
                        transport.authorize_completion(&binding).await?;
                        transport.control(&binding, Control::Complete).await?;
                        transport.authorize_completion(&binding).await?;
                        transport.sessions.complete(&binding)
                    }
                    .await;
                    let outcome = match outcome {
                        Ok(()) => Ok(()),
                        Err(error) => {
                            let _ = transport.retire_remote(&binding).await;
                            Err(error.to_string())
                        }
                    };
                    #[cfg(feature = "testing")]
                    transport
                        .test_cut(
                            crate::replicator::TestingTcpRouteKind::BuildResult,
                            &binding,
                        )
                        .await?;
                    result.send_replace(Some(outcome));
                    Ok(())
                });
                builds.insert(nonce, receiver.clone());
                receiver
            }
        };
        loop {
            let observed = result.borrow().clone();
            if let Some(result) = observed {
                result.map_err(RuntimeError::ControlPlane)?;
                self.transport.authorize_completion(&binding).await?;
                self.transport.control(&binding, Control::Complete).await?;
                return self.transport.authorize_completion(&binding).await;
            }
            result
                .changed()
                .await
                .map_err(|_| RuntimeError::ControlPlane("built-in Build owner lost".into()))?;
        }
    }
    async fn remove_replica(&self, replica: ReplicaId) -> Result<()> {
        let binding = self
            .transport
            .sessions
            .binding_for(replica.value())
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let transport = self.transport.clone();
        let facade = self.facade.clone();
        let peers = self.peers.clone();
        let (result, receiver) = tokio::sync::oneshot::channel();
        self.transport.spawn(async move {
            let outcome = async {
                // Native eligibility, fencing and removal share the manager's
                // configuration-adoption lock. A rejected voter is untouched.
                facade
                    .remove_fenced(replica, transport.sessions.clone(), binding.clone())
                    .await?;
                #[cfg(feature = "testing")]
                transport
                    .test_cut(
                        crate::replicator::TestingTcpRouteKind::RemoveAdmitted,
                        &binding,
                    )
                    .await?;
                peers.lock().unwrap().remove(&replica);
                if transport.sessions.source_retirement_selected(&binding) {
                    transport.join_predecessor(&binding).await?;
                    return transport.sessions.source_transport_settled(&binding);
                }
                transport.retire_remote(&binding).await
            }
            .await;
            let _ = result.send(outcome);
            Ok(())
        });
        receiver
            .await
            .map_err(|_| RuntimeError::ControlPlane("built-in Remove owner lost".into()))?
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
