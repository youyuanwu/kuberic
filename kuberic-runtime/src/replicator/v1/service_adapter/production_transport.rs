//! Authenticated engine sessions and retained carrier ownership.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures::{FutureExt, Stream};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;
use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status};

use crate::protocol::public_operations::{
    ORDINARY_ACTION_FORMAT, ReplicaDiscoveryQuery, ReplicaPublication, ReplicaPublicationStage,
};
use crate::protocol::types::{ConfigurationDescriptor, Epoch, HistoryContext, ReplicaRole};
use crate::replicator::ReplicatorFactoryContext;
use crate::{Result, RuntimeError};

use super::super::buffers::{BufferLimits, BufferPolicy, decode};
use super::super::copy_async_operation::CopySendAction;
use super::super::copy_context_receiver::CopyContextAck;
use super::super::ingress::{IngressError, MessageRegistry};
use super::super::primary::PrimaryError;
use super::super::secondary::SecondaryAck;
use super::super::session::{SendIntent, SendKind};
use super::super::transport::{
    Binding as CarrierBinding, Carrier, CarrierLimits, Delivery, ExchangeService, Ingress, Route,
    SendError, connect_authenticated, wire,
};
use super::engine_session::{
    Binding, Control, ControlEnvelope, Endpoint, Fence, Sessions, partition,
};
use super::facade::Facade;
use crate::replicator::Replicator;

const MAX_PAYLOAD: usize = 52_428_800;
const MAX_WIRE: usize = MAX_PAYLOAD + 64 * 1024;
const BINDING_HEADER: &str = "kuberic-engine-session-bin";
const CONTROL_HEADER: &str = "kuberic-engine-control";
const MAX_CONTROL: usize = 64 * 1024;
pub(super) const ACK_CAPACITY: usize = 48;

#[cfg(test)]
#[path = "production_replacement_transport_tests.rs"]
mod replacement_tests;

pub(super) struct Ack {
    pub(super) binding: Binding,
    pub(super) credit: Box<dyn FnOnce() -> Result<()> + Send>,
    settlement: Arc<AckSettlement>,
    _queued: TaskTrackerToken,
}

struct AckSettlement {
    binding: Binding,
    tasks: TaskTracker,
    failure: Mutex<Option<String>>,
    local_drain: AtomicBool,
}

pub(super) fn buffer_limits() -> BufferLimits {
    BufferLimits {
        max_segments: 1,
        max_payload_bytes: MAX_PAYLOAD,
    }
}

fn limits() -> CarrierLimits {
    CarrierLimits {
        outbound_count: 1024,
        outbound_bytes: MAX_WIRE * 2,
        inbound_count: 1024,
        inbound_bytes: MAX_WIRE * 2,
        max_wire_bytes: MAX_WIRE,
        buffers: buffer_limits(),
    }
}

fn application(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ControlPlane(error.to_string())
}

fn publication_admitted(
    now: &ReplicaPublication,
    origin: &ReplicaPublication,
    binding: &Binding,
    source: bool,
    completion: bool,
) -> bool {
    let role = if source {
        now.role == ReplicaRole::Primary
    } else {
        matches!(
            now.role,
            ReplicaRole::IdleSecondary | ReplicaRole::ActiveSecondary
        )
    };
    let stage = match now.stage {
        ReplicaPublicationStage::AchievedScope => role,
        ReplicaPublicationStage::DiscoveryPending => !completion && now.role == ReplicaRole::None,
    };
    if binding.replacement().is_some() {
        return stage && replacement_scope(now, binding, source);
    }
    let current = &now.scope.configuration;
    let captured = &origin.scope.configuration;
    let extends = |next: &ConfigurationDescriptor, prior: &ConfigurationDescriptor| {
        next.epoch == prior.epoch
            && next.primary_id == prior.primary_id
            && prior
                .members
                .iter()
                .all(|member| next.members.contains(member))
            && next.members.iter().all(|member| {
                prior.members.contains(member) || member.role == ReplicaRole::ActiveSecondary
            })
    };
    let original = current.current == captured.current && current.replicas == captured.replicas;
    let local = &now.scope.storage.replica;
    let members = current
        .previous
        .iter()
        .chain(std::iter::once(&current.current))
        .flat_map(|configuration| &configuration.members)
        .filter(|member| &member.identity != local)
        .map(|member| (&member.identity, member.role))
        .collect::<BTreeMap<_, _>>();
    let peers_match = current.replicas.len() == members.len()
        && current
            .replicas
            .iter()
            .all(|peer| &peer.identity != local && members.get(&peer.identity) == Some(&peer.role));
    let promoted = extends(&current.current, &captured.current)
        && current.current.members.iter().any(|member| {
            member.identity == binding.secondary().identity
                && member.role == ReplicaRole::ActiveSecondary
        });
    let retained = now.scope.epoch == origin.scope.epoch
        && current.current.primary_id == captured.current.primary_id
        && current.current.members.iter().any(|member| {
            member.identity == binding.secondary().identity
                && member.role == ReplicaRole::ActiveSecondary
        })
        && current.current.members.iter().any(|member| {
            member.identity == binding.primary().identity && member.role == ReplicaRole::Primary
        })
        && peers_match;
    stage
        && (original || (promoted && peers_match) || retained)
        && (current.previous == captured.previous
            || current.previous.is_none()
            || (retained || promoted)
                && current.previous.as_ref().is_some_and(|previous| {
                    previous.primary_id == current.current.primary_id
                        && previous.epoch == current.current.epoch
                })
            || (promoted
                && current.previous.as_ref().is_some_and(|previous| {
                    extends(previous, &captured.current) && extends(&current.current, previous)
                })))
}

fn replacement_scope(now: &ReplicaPublication, binding: &Binding, source: bool) -> bool {
    let origin = if source {
        &binding.source
    } else {
        &binding.target
    };
    replacement_scope_from(now, binding, origin)
}

fn replacement_scope_from(
    now: &ReplicaPublication,
    binding: &Binding,
    origin: &ReplicaPublication,
) -> bool {
    let Some(old) = binding.replacement() else {
        return false;
    };
    let original = &binding.source.scope.configuration.current;
    let scope = &now.scope.configuration;
    if now.validate().is_err()
        || now.scope.epoch != binding.epoch()
        || now.scope.history != *binding.history()
        || Endpoint::publication(now) != Endpoint::publication(origin)
    {
        return false;
    }

    let mut successor = original.clone();
    let Some(slot) = successor
        .members
        .iter_mut()
        .find(|member| member.identity == old.identity)
    else {
        return false;
    };
    slot.identity = binding.secondary().identity;
    successor.configuration_id = successor.expected_id();
    let mut previous = original.clone();
    let Some(epoch) = previous.epoch.configuration_number.checked_sub(1) else {
        return false;
    };
    previous.epoch.configuration_number = epoch;
    previous.configuration_id = previous.expected_id();
    let initial = scope.current == *original && scope.previous.is_none();
    let admitted = scope.current == successor
        && (scope.previous.as_ref() == Some(&previous) || scope.previous.is_none());
    if (!initial && !admitted)
        || scope.current.primary_id != binding.primary().identity.replica_id
        || original.write_quorum <= 1
    {
        return false;
    }
    scope.replicas.iter().all(|peer| {
        if peer.identity == old.identity {
            return peer == &old;
        }
        if peer.identity == binding.secondary().identity {
            return super::engine_session::endpoint_matches_peer(&binding.secondary(), peer)
                && peer.role == ReplicaRole::ActiveSecondary
                && !peer.must_catchup;
        }
        if peer.identity == binding.primary().identity {
            return super::engine_session::endpoint_matches_peer(&binding.primary(), peer)
                && peer.role == ReplicaRole::Primary
                && !peer.must_catchup;
        }
        origin.scope.configuration.replicas.iter().any(|selected| {
            selected.identity == peer.identity
                && selected.storage_incarnation == peer.storage_incarnation
                && selected.process_session_id == peer.process_session_id
                && selected.replication_address == peer.replication_address
                && selected.role == peer.role
                && selected.must_catchup == peer.must_catchup
        })
    })
}

// Healthy peers require actual source confirmation for this bounded shape.
fn replacement_cohort_shape(now: &ReplicaPublication, binding: &Binding, source: bool) -> bool {
    let origin = if source {
        &binding.source
    } else {
        &binding.target
    };
    let original = &binding.source.scope.configuration.current;
    let current = &now.scope.configuration.current;
    if now.validate().is_err()
        || now.scope.history != *binding.history()
        || now.scope.epoch != binding.epoch()
        || Endpoint::publication(now) != Endpoint::publication(origin)
        || current.epoch != original.epoch
        || current.primary_id != original.primary_id
        || current.write_quorum != original.write_quorum
        || current.members.len() != original.members.len()
    {
        return false;
    }
    let changed: Vec<_> = original
        .members
        .iter()
        .filter(|member| !current.members.contains(member))
        .collect();
    if changed.len() != 1 {
        return false;
    }
    let old = changed[0];
    if old.role != ReplicaRole::ActiveSecondary
        || old.identity.replica_id == binding.secondary().identity.replica_id
        || !current.members.iter().any(|member| {
            member.identity.replica_id == old.identity.replica_id
                && member.identity != old.identity
                && member.role == old.role
        })
    {
        return false;
    }
    let mut previous = original.clone();
    let Some(epoch) = previous.epoch.configuration_number.checked_sub(1) else {
        return false;
    };
    previous.epoch.configuration_number = epoch;
    previous.configuration_id = previous.expected_id();
    now.scope.configuration.previous.is_none()
        || now.scope.configuration.previous.as_ref() == Some(&previous)
}

fn validate_message_epochs(
    admitted: crate::protocol::types::Epoch,
    envelope: &wire::Envelope,
) -> Result<()> {
    let epoch = |value: &Option<wire::Epoch>| {
        value
            .as_ref()
            .filter(|value| value.data_loss >= 0 && value.configuration >= 0)
            .map(|value| crate::protocol::types::Epoch::new(value.data_loss, value.configuration))
            .ok_or_else(|| {
                RuntimeError::AuthorityMismatch("native frame lacks a valid epoch".into())
            })
    };
    let authorized = match envelope.body.as_ref() {
        Some(wire::envelope::Body::StartCopy(operation)) => epoch(&operation.epoch)? == admitted,
        Some(wire::envelope::Body::CopyOperation(operation)) => {
            epoch(&operation.epoch)? == admitted
        }
        Some(wire::envelope::Body::ReplicationOperation(operation)) => {
            let sending = epoch(&operation.sending_epoch)?;
            let original = epoch(&operation.original_epoch)?;
            sending == admitted && original <= sending
        }
        _ => true,
    };
    if authorized {
        Ok(())
    } else {
        Err(RuntimeError::AuthorityMismatch(
            "native frame epoch is outside the admitted transport scope".into(),
        ))
    }
}

fn endpoint(
    resource: &crate::protocol::types::ResourceUid,
    endpoint: &Endpoint,
) -> Result<wire::Endpoint> {
    Ok(wire::Endpoint {
        partition: bytes::Bytes::copy_from_slice(&partition(resource)),
        replica: endpoint.identity.replica_id.value(),
        incarnation: bytes::Bytes::copy_from_slice(&endpoint.incarnation()?),
    })
}

pub(super) fn local_route(history: &HistoryContext, local: &Endpoint) -> Result<Route> {
    Ok(Route {
        source: endpoint(&history.resource_uid, local)?,
        destination: endpoint(&history.resource_uid, local)?,
        sender_address: local.address.clone(),
        remote_address: local.address.clone(),
    })
}

pub(super) fn route(binding: &Binding, local: &Endpoint) -> Result<Route> {
    let primary_endpoint = binding.primary();
    let secondary_endpoint = binding.secondary();
    let primary = Route {
        source: endpoint(&binding.history().resource_uid, &primary_endpoint)?,
        destination: endpoint(&binding.history().resource_uid, &secondary_endpoint)?,
        sender_address: primary_endpoint.address.clone(),
        remote_address: secondary_endpoint.address.clone(),
    };
    if local == &primary_endpoint {
        Ok(primary)
    } else if local == &secondary_endpoint {
        Ok(primary.reversed())
    } else {
        Err(RuntimeError::AuthorityNotAdmitted)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicV1DataServiceRegistration {
    Carrier,
}

impl PublicV1DataServiceRegistration {
    pub(crate) const fn name(self) -> &'static str {
        "CarrierServer"
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PublicV1DataServiceManifest;

impl PublicV1DataServiceManifest {
    pub(crate) const fn registrations(self) -> [PublicV1DataServiceRegistration; 1] {
        [PublicV1DataServiceRegistration::Carrier]
    }

    fn carrier(self, service: Listener) -> wire::carrier_server::CarrierServer<Listener> {
        wire::carrier_server::CarrierServer::new(service)
            .max_decoding_message_size(MAX_WIRE)
            .max_encoding_message_size(MAX_WIRE)
    }
}

pub(crate) const PUBLIC_V1_DATA_SERVICES: PublicV1DataServiceManifest = PublicV1DataServiceManifest;

struct Peer {
    binding: Binding,
    fence: Fence,
    route: Route,
    carrier: Carrier,
    connecting: AtomicBool,
    incarnation: i64,
    reformation_boundary: Option<crate::application::Lsn>,
    initial_ack: Arc<AtomicI64>,
    native_installed: AtomicBool,
    fresh_ack: AtomicI64,
    ack_generation: u64,
    ack_owner: Option<uuid::Uuid>,
    current: Arc<AtomicBool>,
    tasks: TaskTracker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Effect {
    Control(Control),
    Peer(bool),
}

type EffectResult = std::result::Result<i64, String>;
type EffectObserver = watch::Receiver<Option<EffectResult>>;

pub(super) struct Transport {
    context: ReplicatorFactoryContext,
    credentials: Arc<str>,
    local: Endpoint,
    history: HistoryContext,
    registry: MessageRegistry,
    peers: Mutex<BTreeMap<i64, Arc<Peer>>>,
    retiring_peers: Mutex<BTreeMap<String, Arc<Peer>>>,
    stop: watch::Sender<bool>,
    tasks: TaskTracker,
    failure: Arc<Mutex<Option<String>>>,
    next_incarnation: AtomicI64,
    pub(super) sessions: Arc<Sessions>,
    facade: Mutex<Option<Weak<Facade>>>,
    production: Mutex<Option<Weak<super::production::Production>>>,
    controls: Arc<tokio::sync::Semaphore>,
    pub(super) continuations: tokio::sync::Mutex<()>,
    lifecycle: Mutex<Option<(Epoch, ReplicaRole)>>,
    configuration: Mutex<Option<(ConfigurationDescriptor, Option<ConfigurationDescriptor>)>>,
    acknowledgements: Mutex<Option<tokio::sync::mpsc::Sender<Ack>>>,
    ack_settlements: Mutex<BTreeMap<String, Arc<AckSettlement>>>,
    effects: Mutex<BTreeMap<(String, Effect), (Binding, EffectObserver)>>,
    #[cfg(feature = "testing")]
    native_credits: AtomicI64,
    #[cfg(feature = "testing")]
    ingress_observations: Mutex<std::collections::VecDeque<(String, String)>>,
    #[cfg(test)]
    test_publications: Mutex<Vec<ReplicaPublication>>,
}

impl Transport {
    pub(super) fn production_owner(&self) -> Result<Arc<super::production::Production>> {
        self.production
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(RuntimeError::Closed)
    }

    pub(super) async fn register_configuration(
        &self,
        current: &crate::replicator::ReplicaSetConfiguration,
        previous: Option<&crate::replicator::ReplicaSetConfiguration>,
    ) -> Result<()> {
        let source = self.publication().await?;
        if !self.context.ordinary_hosting()
            || current.configuration != source.scope.configuration.current
            || current
                .replicas
                .iter()
                .chain(previous.into_iter().flat_map(|set| &set.replicas))
                .all(|peer| peer.identity == self.local.identity)
        {
            return Ok(());
        }
        let invocation =
            super::warm_source_transfer::invocation(&self.context, &source, current, previous)
                .await
                .map_err(|error| {
                    RuntimeError::ControlPlane(format!("configuration invocation: {error}"))
                })?;
        self.current_normal_primary(source.scope.epoch)
            .map_err(|error| {
                RuntimeError::ControlPlane(format!("configuration native Primary: {error}"))
            })?;
        self.sessions
            .register_configuration(invocation)
            .map_err(|error| {
                RuntimeError::ControlPlane(format!("configuration registration: {error}"))
            })
    }

    fn current_normal_primary(&self, epoch: Epoch) -> Result<super::super::primary::PrimaryHandle> {
        self.check_health()?;
        if self.selected_lifecycle() != Some((epoch, ReplicaRole::Primary)) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let facade = self
            .facade
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(RuntimeError::Closed)?;
        if !self
            .production
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|owner| owner.native_primary_at(epoch))
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let super::Target::Primary(primary) = facade.service().target().map_err(application)?
        else {
            return Err(RuntimeError::AuthorityNotAdmitted);
        };
        Ok(primary)
    }

    pub(super) async fn associate_configuration(&self, binding: &Binding) -> Result<()> {
        if !self.context.ordinary_hosting() {
            return Ok(());
        }
        self.sessions.associate_configuration(binding)?;
        let primary = self.current_normal_primary(binding.epoch())?;
        let last = primary.current_progress().map_err(application)?;
        let first = primary.catchup_capability().map_err(application)?;
        let source = self.publication().await?;
        let current = super::warm_source_transfer::ConfigurationInvocation::invocation_for_binding(
            &self.context,
            binding,
        )
        .await?;
        self.sessions.check_configuration(binding, &current)?;
        let receiver = binding
            .source
            .scope
            .configuration
            .replicas
            .iter()
            .find(|peer| {
                peer.identity == binding.secondary().identity && peer.is_up && peer.is_ready
            })
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if source != binding.source
            || receiver.current_progress < 0
            || receiver.current_progress > last
            || (receiver.current_progress < last
                && (first <= 0 || first > receiver.current_progress.saturating_add(1)))
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }

    pub(super) fn native_incarnation(&self, binding: &Binding) -> Result<i64> {
        self.peers
            .lock()
            .unwrap()
            .values()
            .find(|peer| peer.binding == *binding)
            .map(|peer| peer.incarnation)
            .or_else(|| {
                self.retiring_peers
                    .lock()
                    .unwrap()
                    .get(&binding.nonce)
                    .filter(|peer| peer.binding == *binding)
                    .map(|peer| peer.incarnation)
            })
            .ok_or(RuntimeError::AuthorityNotAdmitted)
    }

    #[cfg(test)]
    pub(super) fn predecessor_carrier_withdrawn(&self, binding: &Binding) -> bool {
        self.retiring_peers
            .lock()
            .unwrap()
            .get(&binding.nonce)
            .is_some_and(|peer| {
                peer.binding == *binding
                    && !peer.current.load(Ordering::Acquire)
                    && peer.tasks.is_closed()
            })
    }

    pub(super) fn live_target_native_installed(&self, binding: &Binding) -> Result<()> {
        let peers = self.peers.lock().unwrap();
        let peer = peers
            .values()
            .find(|peer| peer.binding == *binding)
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if binding.live_target.is_none() || peer.initial_ack.load(Ordering::Acquire) < 0 {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        peer.native_installed.store(true, Ordering::Release);
        Ok(())
    }

    pub(super) async fn start(
        context: ReplicatorFactoryContext,
        advertised: String,
        registry: MessageRegistry,
    ) -> Result<Arc<Self>> {
        let credentials = context.native_credentials().await?;
        let listener = TcpListener::bind(context.public_v1_replication_address())
            .await
            .map_err(application)?;
        let port = listener.local_addr().map_err(application)?.port();
        let uri: tonic::codegen::http::Uri = advertised.parse().map_err(application)?;
        if uri.scheme_str() != Some("http") || uri.host().is_none() {
            return Err(RuntimeError::InvalidReplication(
                "replication endpoint must be an http URI".into(),
            ));
        }
        let advertised = if uri.port_u16() == Some(0) {
            let mut parts = uri.into_parts();
            let authority = parts
                .authority
                .as_ref()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            parts.authority = Some(
                format!("{}:{port}", authority.host())
                    .parse()
                    .map_err(application)?,
            );
            tonic::codegen::http::Uri::from_parts(parts)
                .map_err(application)?
                .to_string()
        } else {
            advertised
        };
        let (history, local) = context.native_local_endpoint(advertised).await?;
        let local = Endpoint {
            identity: local.identity,
            process_session_id: local.process_session_id,
            storage: local.storage,
            creation_id: context.creation_id()?,
            address: local.address,
        };
        let owner = Arc::new(Self {
            context,
            credentials,
            local,
            history,
            registry,
            peers: Mutex::new(BTreeMap::new()),
            retiring_peers: Mutex::new(BTreeMap::new()),
            stop: watch::channel(false).0,
            tasks: TaskTracker::new(),
            failure: Arc::new(Mutex::new(None)),
            next_incarnation: AtomicI64::new(1),
            sessions: Arc::new(Sessions::default()),
            facade: Mutex::new(None),
            production: Mutex::new(None),
            controls: Arc::new(tokio::sync::Semaphore::new(32)),
            continuations: tokio::sync::Mutex::new(()),
            lifecycle: Mutex::new(None),
            configuration: Mutex::new(None),
            acknowledgements: Mutex::new(None),
            ack_settlements: Mutex::new(BTreeMap::new()),
            effects: Mutex::new(BTreeMap::new()),
            #[cfg(feature = "testing")]
            native_credits: AtomicI64::new(0),
            #[cfg(feature = "testing")]
            ingress_observations: Mutex::new(std::collections::VecDeque::new()),
            #[cfg(test)]
            test_publications: Mutex::new(Vec::new()),
        });
        let service = Listener(owner.clone());
        #[cfg(feature = "testing")]
        crate::replicator::testing_transport::retain_native_transport(
            owner.local.identity.instance_id.as_str(),
            owner.local.process_session_id.clone(),
            owner
                .context
                .reservation
                .ok_or(RuntimeError::AuthorityNotAdmitted)?,
        );
        #[cfg(feature = "testing")]
        {
            let weak = Arc::downgrade(&owner);
            crate::replicator::testing_transport::retain_inspection(
                owner.local.identity.instance_id.as_str(),
                move || {
                    let Some(owner) = weak.upgrade() else {
                        return serde_json::Value::Null;
                    };
                    serde_json::json!({
                        "production": owner.production.lock().unwrap().as_ref()
                            .and_then(Weak::upgrade).map(|owner| owner.inspect()),
                        "peers": owner.inspect_peers(),
                        "ackOwners": owner.inspect_acks(),
                        "sessions": owner.sessions.inspect(),
                        "tasks": owner.cardinality(),
                        "failure": *owner.failure.lock().unwrap(),
                        "effects": owner.effects.lock().unwrap().iter().map(|((nonce, effect), (_, result))| {
                            serde_json::json!({"nonce": nonce, "effect": format!("{effect:?}"), "result": result.borrow().clone()})
                        }).collect::<Vec<_>>(),
                        "retiringPeers": owner.retiring_peers.lock().unwrap().values().map(|peer| {
                            serde_json::json!({"nonce": peer.binding.nonce, "tasks": peer.tasks.len(), "closed": peer.tasks.is_closed()})
                        }).collect::<Vec<_>>(),
                        "ackCapacity": owner.acknowledgements.lock().unwrap().as_ref()
                            .map(|sender| sender.capacity()),
                        "controlsAvailable": owner.controls.available_permits(),
                        "ackPending": owner.ack_settlements.lock().unwrap().values()
                            .map(|settlement| settlement.tasks.len()).sum::<usize>(),
                        "nativeCredits": owner.native_credits.load(Ordering::Acquire),
                        "ingressObservations": *owner.ingress_observations.lock().unwrap(),
                        "receivers": owner.registry.inspect_receivers(),
                        "recoveries": owner.registry.inspect_recoveries(),
                        "lifecycle": owner.selected_lifecycle(),
                    })
                },
            );
        }
        let mut shutdown = owner.stop.subscribe();
        owner.spawn(async move {
            tonic::transport::Server::builder()
                .add_service(PUBLIC_V1_DATA_SERVICES.carrier(service))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                    let _ = shutdown.wait_for(|stopped| *stopped).await;
                })
                .await
                .map_err(application)
        });
        Ok(owner)
    }

    pub(super) fn spawn(
        &self,
        future: impl std::future::Future<Output = Result<()>> + Send + 'static,
    ) {
        let failure = self.failure.clone();
        let sessions = self.sessions.clone();
        let context = self.context.clone();
        self.tasks.spawn(async move {
            let result = AssertUnwindSafe(future).catch_unwind().await;
            let error = match result {
                Ok(Ok(())) => return,
                Ok(Err(error)) => error.to_string(),
                Err(_) => "native transport task panicked".into(),
            };
            tracing::error!(%error, "native transport owner failed");
            sessions.retire_all();
            failure.lock().unwrap().get_or_insert(error);
            if let Err(report) = context
                .report_fault(crate::protocol::types::FaultType::Transient)
                .await
            {
                tracing::warn!(%report, "native transport fault could not be reported");
            }
        });
    }

    fn spawn_peer(
        self: &Arc<Self>,
        binding: &Binding,
        tasks: &TaskTracker,
        future: impl std::future::Future<Output = Result<()>> + Send + 'static,
    ) {
        let token = tasks.token();
        let failure = self.failure.clone();
        let owner = Arc::downgrade(self);
        let binding = binding.clone();
        self.spawn(async move {
            let _joined = token;
            let result = match AssertUnwindSafe(future).catch_unwind().await {
                Ok(result) => result,
                Err(_) => Err(application("native peer task panicked")),
            };
            if let Err(error) = &result {
                // Publish before releasing the exact peer's join token.
                if let Some(owner) = owner.upgrade() {
                    owner.quarantine(&binding);
                }
                failure
                    .lock()
                    .unwrap()
                    .get_or_insert_with(|| error.to_string());
            }
            result
        });
    }

    #[cfg(test)]
    pub(super) fn test_peer_task(
        self: &Arc<Self>,
        binding: &Binding,
        future: impl std::future::Future<Output = Result<()>> + Send + 'static,
    ) {
        let peer = self
            .peers
            .lock()
            .unwrap()
            .values()
            .find(|peer| peer.binding == *binding)
            .cloned()
            .unwrap();
        self.spawn_peer(binding, &peer.tasks, future);
    }

    pub(super) fn check_health(&self) -> Result<()> {
        match self.failure.lock().unwrap().as_ref() {
            Some(error) => Err(application(format!(
                "native transport owner failed: {error}"
            ))),
            None if *self.stop.borrow() => Err(RuntimeError::Closed),
            None => Ok(()),
        }
    }
    pub(super) async fn fail_effect(&self, binding: &Binding, error: &RuntimeError) {
        if self.sessions.partial_effect(binding) {
            self.failure
                .lock()
                .unwrap()
                .get_or_insert_with(|| error.to_string());
            let _ = self.sessions.retire(binding);
            if let Err(cleanup) = self.join_predecessor(binding).await {
                tracing::warn!(%cleanup, "native failed effect cleanup retained");
            }
        } else {
            let _ = self.sessions.retire(binding);
        }
    }
    async fn owned_effect(
        self: &Arc<Self>,
        binding: &Binding,
        effect: Effect,
        future: impl std::future::Future<Output = Result<i64>> + Send + 'static,
    ) -> Result<i64> {
        let mut observer = {
            let mut effects = self.effects.lock().unwrap();
            let key = (binding.nonce.clone(), effect);
            if let Some((exact, observer)) = effects.get(&key) {
                if exact != binding {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                observer.clone()
            } else {
                if effects.len() >= 4096 {
                    return Err(RuntimeError::QueueFull);
                }
                let (result, observer) = watch::channel(None);
                effects.insert(key, (binding.clone(), observer.clone()));
                let owner = self.clone();
                let binding = binding.clone();
                self.spawn(async move {
                        let outcome = match AssertUnwindSafe(future).catch_unwind().await {
                            Ok(result) => result.map_err(|error| error.to_string()),
                            Err(_) => Err("native private effect owner panicked".into()),
                        };
                        #[cfg(test)]
                        if let Err(error) = &outcome
                            && std::env::var_os("KUBERIC_M3_NATIVE_SCOPE_DIAGNOSTICS").is_some()
                        {
                            eprintln!("native owned effect {effect:?} failed: {error}");
                        }
                        if let Err(error) = &outcome
                            && owner.sessions.partial_effect(&binding)
                        {
                            owner.failure.lock().unwrap().get_or_insert(error.clone());
                            // Keep the failed slot and every partial Peer. A retry is
                            // an observer, never another native replacement attempt.
                            let _ = owner.sessions.retire(&binding);
                            let cleanup = owner.join_predecessor(&binding).await;
                            if let Err(cleanup) = cleanup {
                                tracing::warn!(%cleanup, "failed private effect retained cleanup failure");
                            }
                        }
                        result.send_replace(Some(outcome));
                        Ok(())
                    });
                observer
            }
        };
        loop {
            if let Some(result) = observer.borrow().clone() {
                return result.map_err(application);
            }
            observer
                .changed()
                .await
                .map_err(|_| application("native private effect owner lost"))?;
        }
    }

    fn current_peer(&self, binding: &Binding, outgoing: bool) -> Result<Arc<Peer>> {
        self.check_health()?;
        self.sessions.active(binding)?;
        let peer = self
            .peers
            .lock()
            .unwrap()
            .values()
            .find(|peer| peer.binding == *binding)
            .cloned()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if !peer.current.load(Ordering::Acquire)
            || peer.tasks.is_closed()
            || peer.tasks.len() < if outgoing { 2 } else { 1 }
            || outgoing && !peer.connecting.load(Ordering::Acquire)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        peer.fence.check()?;
        if self.local == binding.secondary() {
            let facade = self.facade.lock().unwrap().as_ref().and_then(Weak::upgrade);
            #[cfg(test)]
            if facade.is_none() && !self.context.ordinary_hosting() {
                return Ok(peer);
            }
            let facade = facade.ok_or(RuntimeError::Closed)?;
            let super::Target::Secondary(secondary) =
                facade.service().target().map_err(application)?
            else {
                return Err(RuntimeError::AuthorityNotAdmitted);
            };
            if secondary.transport_identity().map_err(application)?
                != (
                    peer.ack_owner.ok_or(RuntimeError::AuthorityNotAdmitted)?,
                    peer.ack_generation,
                )
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
        }
        Ok(peer)
    }

    pub(super) fn local(&self) -> &Endpoint {
        &self.local
    }

    pub(super) fn history(&self) -> &HistoryContext {
        &self.history
    }

    pub(super) fn attach(&self, facade: &Arc<Facade>) {
        *self.facade.lock().unwrap() = Some(Arc::downgrade(facade));
    }

    pub(super) fn attach_production(&self, production: &Arc<super::production::Production>) {
        *self.production.lock().unwrap() = Some(Arc::downgrade(production));
    }

    #[cfg(feature = "testing")]
    pub(super) fn inspect_peers(&self) -> serde_json::Value {
        serde_json::json!(
            self.peers
                .lock()
                .unwrap()
                .values()
                .map(|peer| serde_json::json!({
                    "binding": peer.binding,
                    "incarnation": peer.incarnation,
                    "ackOwner": peer.ack_owner.map(|owner| owner.to_string()),
                    "ackGeneration": peer.ack_generation,
                    "reformationBoundary": peer.reformation_boundary,
                    "initialNativeAck": peer.initial_ack.load(Ordering::Acquire),
                    "processedNativeAck": peer.fresh_ack.load(Ordering::Acquire),
                    "owner": Arc::as_ptr(peer) as usize,
                    "current": peer.current.load(Ordering::Acquire),
                }))
                .collect::<Vec<_>>()
        )
    }

    #[cfg(feature = "testing")]
    pub(super) fn inspect_acks(&self) -> serde_json::Value {
        serde_json::json!(
            self.ack_settlements
                .lock()
                .unwrap()
                .values()
                .map(|owner| serde_json::json!({
                    "binding": owner.binding,
                    "owner": Arc::as_ptr(owner) as usize,
                    "closed": owner.tasks.is_closed(),
                    "failure": *owner.failure.lock().unwrap(),
                }))
                .collect::<Vec<_>>()
        )
    }

    #[cfg(test)]
    pub(super) fn controls_available(&self) -> usize {
        self.controls.available_permits()
    }

    #[cfg(all(test, feature = "testing"))]
    pub(super) fn native_credit_count(&self) -> i64 {
        self.native_credits.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn pending_sends(&self, binding: &Binding) -> Option<usize> {
        self.peers
            .lock()
            .unwrap()
            .values()
            .find(|peer| peer.binding == *binding)
            .map(|peer| peer.carrier.pending_sends())
    }

    #[cfg(all(test, feature = "testing"))]
    pub(super) fn removal_receivers(&self) -> serde_json::Value {
        serde_json::json!({
            "receivers": self.registry.inspect_receivers(),
            "ingress": *self.ingress_observations.lock().unwrap(),
        })
    }

    #[cfg(test)]
    pub(super) fn test_production(&self) -> Arc<super::production::Production> {
        self.production
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .unwrap()
    }

    pub(super) async fn join_predecessor(&self, binding: &Binding) -> Result<()> {
        let peer = self.sessions.retire_with(binding, || {
            let mut peers = self.peers.lock().unwrap();
            let replica = if self.local == binding.primary() {
                binding.secondary().identity.replica_id.value()
            } else {
                binding.primary().identity.replica_id.value()
            };
            if peers
                .get(&replica)
                .is_some_and(|peer| peer.binding != *binding)
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            let mut retiring = self.retiring_peers.lock().unwrap();
            if let Some(peer) = peers.remove(&replica) {
                retiring.insert(binding.nonce.clone(), peer);
            }
            Ok(retiring.get(&binding.nonce).cloned())
        })?;
        if let Some(peer) = peer {
            peer.current.store(false, Ordering::Release);
            peer.carrier.shutdown();
            peer.tasks.close();
            peer.tasks.wait().await;
        }
        let settlement = self
            .ack_settlements
            .lock()
            .unwrap()
            .get(&binding.nonce)
            .cloned();
        if let Some(settlement) = settlement {
            settlement.tasks.close();
            settlement.tasks.wait().await;
            if let Some(error) = settlement.failure.lock().unwrap().as_ref() {
                return Err(application(error));
            }
        }
        self.check_health()?;
        // Authenticated fresh publication on this exact storage is possible
        // only after the agent's predecessor writer/creation containment barrier.
        Ok(())
    }

    pub(super) async fn settle_role_sessions(&self) -> Result<()> {
        for binding in self.sessions.retiring() {
            self.join_predecessor(&binding).await?;
            self.sessions.settled(&binding)?;
        }
        self.sessions.role_receiver_settled()
    }

    pub(super) fn begin_role_change(&self, epoch: Epoch, role: ReplicaRole) {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        if lifecycle.is_some_and(|(prior, old_role)| {
            prior != epoch
                || !(old_role == role
                    || (old_role == ReplicaRole::IdleSecondary
                        && role == ReplicaRole::ActiveSecondary))
        }) {
            self.sessions.retire_all();
            *lifecycle = None;
        }
    }

    pub(super) fn selected_role(&self, epoch: Epoch, role: ReplicaRole) {
        self.begin_role_change(epoch, role);
        *self.lifecycle.lock().unwrap() = Some((epoch, role));
    }

    pub(super) fn selected_lifecycle(&self) -> Option<(Epoch, ReplicaRole)> {
        *self.lifecycle.lock().unwrap()
    }

    pub(super) fn fence(&self, binding: &Binding) -> Fence {
        self.sessions.fence(binding)
    }

    pub(super) fn selected_configuration(
        &self,
        current: ConfigurationDescriptor,
        previous: Option<ConfigurationDescriptor>,
    ) {
        let prior = self.configuration.lock().unwrap().clone();
        for binding in self.sessions.all() {
            if binding.epoch() == current.epoch
                && binding.primary().identity == self.local.identity
                && prior.as_ref().is_some_and(|(current, previous)| {
                    current
                        .members
                        .iter()
                        .chain(previous.iter().flat_map(|p| &p.members))
                        .any(|member| member.identity == binding.secondary().identity)
                })
                && !current
                    .members
                    .iter()
                    .chain(previous.iter().flat_map(|p| &p.members))
                    .any(|member| {
                        member.identity == binding.secondary().identity
                            && member.role == ReplicaRole::ActiveSecondary
                    })
            {
                let _ = self.sessions.retire(&binding);
            }
        }
        *self.configuration.lock().unwrap() = Some((current, previous));
    }

    pub(super) fn live_target_relation(
        &self,
        old: &Binding,
        source: &ReplicaPublication,
        target: &ReplicaPublication,
    ) -> Result<super::engine_session::LiveTarget> {
        let configuration = self.configuration.lock().unwrap();
        let (accepted, previous) = configuration
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if previous.is_some()
            || source.scope.configuration.previous.as_ref() != Some(accepted)
            || old.primary() != self.local
            || old.primary() != Endpoint::publication(source)
            || old.history() != &source.scope.history
            || old.secondary().storage != target.scope.storage
            || old.secondary().process_session_id == target.scope.process_session_id
            || old.secondary().creation_id == target.scope.creation_id
            || !self.sessions.predecessor_completed(old)
            || source.scope.configuration.replicas.len() + 1 != accepted.members.len()
            || !source.scope.configuration.replicas.iter().all(|peer| {
                accepted
                    .members
                    .iter()
                    .any(|member| member.identity == peer.identity && member.role == peer.role)
                    && self
                        .sessions
                        .binding_for(peer.identity.replica_id.value())
                        .is_some_and(|known| {
                            known.primary() == self.local
                                && known.history() == old.history()
                                && self.sessions.predecessor_completed(&known)
                                && if peer.identity == target.scope.storage.replica {
                                    known == *old
                                        && peer.storage_incarnation == target.scope.storage
                                        && super::engine_session::endpoint_matches_peer(
                                            &Endpoint::publication(target),
                                            peer,
                                        )
                                } else {
                                    super::engine_session::endpoint_matches_peer(
                                        &known.secondary(),
                                        peer,
                                    )
                                }
                        })
            })
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(super::engine_session::LiveTarget {
            predecessor: old.nonce.clone(),
            previous: accepted.clone(),
            source: old.primary(),
            target: old.secondary(),
        })
    }

    pub(super) fn abort(&self) {
        self.sessions.abort();
        self.stop.send_replace(true);
        for peer in self.peers.lock().unwrap().values() {
            peer.current.store(false, Ordering::Release);
            peer.carrier.shutdown();
        }
        self.tasks.close();
    }

    pub(super) async fn drain(&self) -> Result<()> {
        self.abort();
        self.tasks.wait().await;
        match self.failure.lock().unwrap().as_ref() {
            Some(error) => Err(application(error)),
            None => Ok(()),
        }
    }

    pub(super) fn cardinality(&self) -> usize {
        self.tasks.len()
    }

    pub(super) fn acknowledgements(self: &Arc<Self>) -> tokio::sync::mpsc::Sender<Ack> {
        let mut sender = self.acknowledgements.lock().unwrap();
        sender
            .get_or_insert_with(|| {
                let (sender, mut receiver) = tokio::sync::mpsc::channel::<Ack>(ACK_CAPACITY);
                let owner = self.clone();
                let mut stop = self.stop.subscribe();
                self.spawn(async move {
                    loop {
                        let ack = tokio::select! {
                            biased;
                            _ = stop.wait_for(|stopped| *stopped) => break,
                            ack = receiver.recv() => match ack {
                                Some(ack) => ack,
                                None => break,
                            },
                        };
                        let Ack {
                            binding,
                            credit,
                            settlement,
                            _queued,
                        } = ack;
                        let outcome = AssertUnwindSafe(owner.credit(&binding, credit, &settlement))
                            .catch_unwind()
                            .await
                            .unwrap_or_else(|_| Err(application("native ACK owner panicked")));
                        if let Err(error) = outcome {
                            #[cfg(feature = "testing")]
                            if std::env::var_os("KUBERIC_M3_ELECTION_DIAGNOSTICS").is_some() {
                                eprintln!("native queued receipt failure: receiver={}, nonce={}, error={error:?}",
                                    owner.local.identity.instance_id, binding.nonce);
                            }
                            tracing::warn!(
                                receiver = %owner.local.identity.instance_id,
                                nonce = %binding.nonce,
                                %error,
                                "native queued service receipt failed"
                            );
                            let error = error.to_string();
                            settlement
                                .failure
                                .lock()
                                .unwrap()
                                .get_or_insert(error.clone());
                            owner.failure.lock().unwrap().get_or_insert(error.clone());
                            // Publish the actual result before releasing retirement's join.
                            drop(_queued);
                            return Err(application(error));
                        }
                        drop(_queued);
                    }
                    // Closing drops queued work without granting native credit.
                    receiver.close();
                    Ok(())
                });
                sender
            })
            .clone()
    }

    fn ack_settlement(&self, binding: &Binding) -> Arc<AckSettlement> {
        self.ack_settlements
            .lock()
            .unwrap()
            .entry(binding.nonce.clone())
            .or_insert_with(|| {
                Arc::new(AckSettlement {
                    binding: binding.clone(),
                    tasks: TaskTracker::new(),
                    failure: Mutex::new(None),
                    local_drain: AtomicBool::new(false),
                })
            })
            .clone()
    }

    pub(super) fn queue_ack(
        &self,
        binding: Binding,
        permit: tokio::sync::mpsc::OwnedPermit<Ack>,
        credit: Box<dyn FnOnce() -> Result<()> + Send>,
    ) -> Result<()> {
        // Caller holds the session effect gate: retirement cannot close this
        // session's join between registration and enqueue.
        let settlement = self.ack_settlement(&binding);
        if settlement.binding != binding || settlement.tasks.is_closed() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        permit.send(Ack {
            binding,
            credit,
            _queued: settlement.tasks.token(),
            settlement,
        });
        Ok(())
    }

    fn fence_unavailable_receiver(&self, binding: &Binding) -> Result<()> {
        // This retires a real, copy-completed receiver, never establishes one.
        // Keep its local native work, but permanently deny this nonce's wire.
        if self.local != binding.secondary() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let facade = self
            .facade
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if !matches!(facade.service().target(), Ok(super::Target::Secondary(_))) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let settlement = self.ack_settlement(binding);
        self.sessions
            .retire_unavailable_receiver_credit(binding, || {
                settlement.tasks.close();
                settlement.local_drain.store(true, Ordering::Release);
                Ok(())
            })??;
        if let Some(peer) = self
            .peers
            .lock()
            .unwrap()
            .get(&binding.primary().identity.replica_id.value())
            .filter(|peer| peer.binding == *binding)
        {
            peer.current.store(false, Ordering::Release);
            peer.carrier.shutdown();
        }
        Ok(())
    }

    pub(super) fn local_receiver_drain(&self, binding: &Binding) -> bool {
        let enabled = self
            .ack_settlements
            .lock()
            .unwrap()
            .get(&binding.nonce)
            .is_some_and(|settlement| {
                settlement.binding == *binding && settlement.local_drain.load(Ordering::Acquire)
            });
        enabled
            && self
                .sessions
                .drain_retiring_receiver_credit(binding, || Ok(()))
                .is_ok()
    }

    pub(super) fn drain_local_receipt(
        self: &Arc<Self>,
        binding: &Binding,
        credit: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        // The native ticket still belongs to this exact retiring receiver.
        // Caller/duplicate validation precedes this native-failure boundary.
        if !self.local_receiver_drain(binding) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.sessions.drain_retiring_receiver_credit(binding, || {
            let facade = self
                .facade
                .lock()
                .unwrap()
                .as_ref()
                .and_then(Weak::upgrade)
                .ok_or(RuntimeError::Closed)?;
            if facade.service().aborted.load(Ordering::Acquire) {
                return Err(RuntimeError::Closed);
            }
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(credit))
                .unwrap_or_else(|_| Err(application("native local receipt owner panicked")));
            if let Err(error) = outcome {
                let failure = error.to_string();
                self.ack_settlement(binding)
                    .failure
                    .lock()
                    .unwrap()
                    .get_or_insert(failure.clone());
                self.failure.lock().unwrap().get_or_insert(failure.clone());
                self.spawn(async move { Err(application(failure)) });
                return Err(error);
            }
            #[cfg(feature = "testing")]
            self.native_credits.fetch_add(1, Ordering::AcqRel);
            Ok(())
        })?
    }

    async fn credit(
        &self,
        binding: &Binding,
        credit: Box<dyn FnOnce() -> Result<()> + Send>,
        settlement: &AckSettlement,
    ) -> Result<()> {
        // Catch the callback itself while its session gate is still held.
        // The outer task catch cannot prevent poisoning that gate on unwind.
        let credit = || {
            std::panic::catch_unwind(AssertUnwindSafe(credit))
                .unwrap_or_else(|_| Err(application("native ACK owner panicked")))
        };
        #[cfg(feature = "testing")]
        self.test_cut(
            crate::replicator::TestingTcpRouteKind::BeforeCredit,
            binding,
        )
        .await?;
        if settlement.local_drain.load(Ordering::Acquire) {
            return match self.sessions.drain_retiring_receiver_credit(binding, || {
                credit()?;
                #[cfg(feature = "testing")]
                self.native_credits.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }) {
                Ok(result) => result,
                Err(_) if self.sessions.active(binding).is_err() => Ok(()),
                Err(error) => Err(error),
            };
        }
        if self.sessions.active(binding).is_err() {
            return Ok(());
        }
        let confirmation = async {
            self.authorize_publications(binding, false).await?;
            if self.local == binding.secondary() {
                self.control(binding, Control::Confirm).await?;
            }
            self.authorize(binding).await
        }
        .await;
        if let Err(error) = confirmation {
            if matches!(error, RuntimeError::PeerUnavailable(_))
                && self.fence_unavailable_receiver(binding).is_ok()
            {
                tracing::debug!(
                    receiver = %self.local.identity.instance_id,
                    nonce = %binding.nonce,
                    %error,
                    "fencing unavailable receiver wire session before local receipt drainage"
                );
                let credited = self.sessions.drain_retiring_receiver_credit(binding, || {
                    credit()?;
                    #[cfg(feature = "testing")]
                    self.native_credits.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                });
                return match credited {
                    Ok(result) => result,
                    Err(_) if self.sessions.active(binding).is_err() => Ok(()),
                    Err(failure) => Err(failure),
                };
            }
            if matches!(
                error,
                RuntimeError::AuthorityNotAdmitted | RuntimeError::Closed
            ) && self.sessions.active(binding).is_err()
            {
                return Ok(());
            }
            self.quarantine(binding);
            if self.local == binding.primary() {
                // Losing confirmation fences this peer, not the Primary's
                // healthy quorum. Its immutable completion witness and joins
                // remain required before any recovered sender can replace it.
                return Ok(());
            }
            return Err(error);
        }
        match self.sessions.effect(binding, || {
            credit()?;
            #[cfg(feature = "testing")]
            self.native_credits.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }) {
            Ok(result) => result,
            Err(_) => Ok(()),
        }
    }

    pub(super) async fn publication(&self) -> Result<ReplicaPublication> {
        let publication = self
            .context
            .local_publication()
            .await?
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        publication
            .validate()
            .map_err(|error| RuntimeError::AuthorityMismatch(error.into()))?;
        if Endpoint::publication(&publication) != self.local {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(publication)
    }

    pub(super) async fn discover(
        &self,
        identity: &crate::protocol::types::ReplicaIdentity,
        process: &crate::protocol::types::ProcessSessionId,
        history: &HistoryContext,
    ) -> Result<ReplicaPublication> {
        #[cfg(test)]
        if let Some(publication) = self
            .test_publications
            .lock()
            .unwrap()
            .iter()
            .find(|publication| {
                &publication.scope.storage.replica == identity
                    && &publication.scope.process_session_id == process
                    && &publication.scope.history == history
            })
            .cloned()
        {
            return Ok(publication);
        }
        let publication = self
            .context
            .discover_peer(&ReplicaDiscoveryQuery {
                format: ORDINARY_ACTION_FORMAT,
                history: history.clone(),
                replica: identity.clone(),
                process_session_id: Some(process.clone()),
            })
            .await?
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        publication
            .validate()
            .map_err(|error| RuntimeError::AuthorityMismatch(error.into()))?;
        Ok(publication)
    }

    pub(super) async fn recovery_peer(
        &self,
        identity: &crate::protocol::types::ReplicaIdentity,
        process: &crate::protocol::types::ProcessSessionId,
        history: &HistoryContext,
    ) -> Result<ReplicaPublication> {
        let mut stop = self.stop.subscribe();
        loop {
            match self.discover(identity, process, history).await {
                Err(RuntimeError::PeerUnavailable(_)) => {
                    tokio::select! {
                        biased;
                        _ = stop.wait_for(|stopped| *stopped) => return Err(RuntimeError::Closed),
                        _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                    }
                }
                result => return result,
            }
        }
    }

    pub(super) async fn validate_publications(&self, binding: &Binding) -> Result<()> {
        binding.validate()?;
        let local = self.publication().await?;
        if !binding.exact_publication(&local) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let remote = if binding.primary() == self.local {
            binding.secondary()
        } else if binding.secondary() == self.local {
            binding.primary()
        } else {
            return Err(RuntimeError::AuthorityNotAdmitted);
        };
        let publication = if binding.recovery {
            self.recovery_peer(
                &remote.identity,
                &remote.process_session_id,
                binding.history(),
            )
            .await?
        } else {
            self.discover(
                &remote.identity,
                &remote.process_session_id,
                binding.history(),
            )
            .await?
        };
        if !binding.exact_publication(&publication) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }

    pub(super) async fn select_build(&self, binding: &mut Binding) -> Result<()> {
        if self.context.ordinary_hosting() || binding.original_member_idle() {
            let checked = super::build_provenance::select(&self.context, binding).await?;
            if self.local != binding.primary()
                || self.selected_lifecycle() != Some((binding.epoch(), ReplicaRole::Primary))
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            self.sessions.associate_build(binding, &checked)?;
        }
        Ok(())
    }

    async fn validate_build_operation(&self, binding: &Binding, primary: bool) -> Result<()> {
        if !self.context.ordinary_hosting() && !binding.original_member_idle() {
            return Ok(());
        }
        let checked = super::build_provenance::check(&self.context, binding).await?;
        if primary {
            if self.selected_lifecycle() != Some((binding.epoch(), ReplicaRole::Primary)) {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            self.sessions.check_build(binding, &checked)?;
        } else {
            if self.selected_lifecycle() != Some((binding.epoch(), ReplicaRole::IdleSecondary)) {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            let facade = self
                .facade
                .lock()
                .unwrap()
                .as_ref()
                .and_then(Weak::upgrade)
                .ok_or(RuntimeError::Closed)?;
            if !matches!(
                facade.service().target().map_err(application)?,
                super::Target::Secondary(_)
            ) {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            self.sessions.associate_build(binding, &checked)?;
        }
        Ok(())
    }

    pub(super) async fn authorize(&self, binding: &Binding) -> Result<()> {
        self.authorize_publications(binding, false).await
    }

    async fn confirm_source_scope(&self, binding: &Binding) -> Result<()> {
        // Construction reuses the actual source proposal, not a claim that its
        // Peer already exists. Data/receipt confirmation still requires Confirm.
        let owned = self
            .effects
            .lock()
            .unwrap()
            .iter()
            .any(|((nonce, effect), (exact, _))| {
                nonce == &binding.nonce
                    && exact == binding
                    && matches!(
                        effect,
                        Effect::Control(Control::Reserve | Control::Reform | Control::Continue)
                    )
            });
        match self.control(binding, Control::Confirm).await {
            Ok(()) => Ok(()),
            Err(error) if owned => self
                .control(binding, Control::Activate)
                .await
                .map_err(|_| error),
            Err(error) => Err(error),
        }
    }

    pub(super) async fn authorize_completion(&self, binding: &Binding) -> Result<()> {
        self.authorize_publications(binding, true).await
    }

    fn fresh_reformation_ready(&self, binding: &Binding) -> Result<bool> {
        let peer = self
            .peers
            .lock()
            .unwrap()
            .values()
            .find(|peer| peer.binding == *binding)
            .cloned()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let Some(boundary) = peer.reformation_boundary else {
            return Ok(true);
        };
        if !peer.current.load(Ordering::Acquire)
            || peer.fence.check().is_err()
            || (!binding.reformation && peer.fresh_ack.load(Ordering::Acquire) < boundary)
        {
            return Ok(false);
        }
        let facade = self
            .facade
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(RuntimeError::Closed)?;
        let primary = match facade.service().target().map_err(application)? {
            super::Target::Primary(primary) => primary,
            _ => return Err(RuntimeError::AuthorityNotAdmitted),
        };
        let identity = super::super::session::SessionIdentity {
            primary_endpoint: peer.route.sender_address.clone(),
            remote_endpoint: peer.route.remote_address.clone(),
            remote_incarnation: peer.incarnation,
        };
        let native = primary
            .continuation_session(&peer.route, &identity, binding.epoch())
            .map_err(application)?;
        let progress = native.progress(0);
        let required = if binding.reformation {
            binding
                .source
                .scope
                .configuration
                .replicas
                .iter()
                .find(|description| description.identity == binding.secondary().identity)
                .ok_or(RuntimeError::AuthorityNotAdmitted)?
                .current_progress
        } else {
            boundary
        };
        Ok(
            !native.is_processing()
                && progress.received >= required
                && progress.applied >= required,
        )
    }

    pub(super) fn reformation_checkpoint(&self, binding: &Binding) -> Result<Arc<AtomicI64>> {
        self.peers
            .lock()
            .unwrap()
            .values()
            .find(|peer| peer.binding == *binding && peer.reformation_boundary.is_some())
            .map(|peer| peer.initial_ack.clone())
            .ok_or(RuntimeError::AuthorityNotAdmitted)
    }

    fn record_reformation_checkpoint(
        &self,
        binding: &Binding,
        ack: &wire::ReplicationAck,
    ) -> Result<()> {
        self.sessions.effect(binding, || {
            let peer = self
                .peers
                .lock()
                .unwrap()
                .values()
                .find(|peer| peer.binding == *binding)
                .cloned()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            let boundary = peer
                .reformation_boundary
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            if peer.initial_ack.load(Ordering::Acquire) >= 0 {
                return Ok(());
            }
            if ack.error != 0
                || ack.replication_applied < 0
                || ack.replication_applied != ack.replication_received
                || ack.replication_received > boundary
                || ack.copy_received != -1
                || ack.copy_applied != -1
            {
                return Err(RuntimeError::InvalidReplication(
                    "invalid fresh receiver checkpoint ACK".into(),
                ));
            }
            match peer.initial_ack.compare_exchange(
                -1,
                ack.replication_applied,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => Ok(()),
                Err(_) => Ok(()),
            }
        })?
    }

    pub(super) fn fresh_reformation_ready_for(&self, incarnation: i64) -> bool {
        let binding = self
            .peers
            .lock()
            .unwrap()
            .values()
            .find(|peer| peer.incarnation == incarnation)
            .map(|peer| peer.binding.clone());
        binding.is_some_and(|binding| self.fresh_reformation_ready(&binding).unwrap_or(false))
    }

    pub(super) async fn wait_for_reformation_checkpoint(&self, binding: &Binding) -> Result<()> {
        self.wait_for_reformation_ack(binding, false).await
    }

    async fn wait_for_reformation_ack(&self, binding: &Binding, installed: bool) -> Result<()> {
        let mut stop = self.stop.subscribe();
        let mut request = tokio::time::interval(Duration::from_secs(5));
        loop {
            self.check_health()?;
            self.authorize(binding).await?;
            if if installed {
                self.fresh_reformation_ready(binding)?
            } else {
                self.reformation_checkpoint(binding)?
                    .load(Ordering::Acquire)
                    >= 0
            } {
                return Ok(());
            }
            tokio::select! {
                biased;
                _ = stop.wait_for(|stopped| *stopped) => return Err(RuntimeError::Closed),
                _ = request.tick() => {
                    let route = route(binding, &self.local)?;
                    self.replication(SendIntent {
                        identity: super::super::session::SessionIdentity {
                            primary_endpoint: route.sender_address,
                            remote_endpoint: route.remote_address,
                            remote_incarnation: self.native_incarnation(binding)?,
                        },
                        kind: SendKind::RequestAck,
                        expires_after: None,
                    });
                }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    }

    // Reports only whether this invocation already obtained source confirmation.
    // Callers still revalidate publications after the await; this is not a grant.
    async fn authorize_publications(&self, binding: &Binding, completion: bool) -> Result<()> {
        self.check_health()?;
        self.sessions.active(binding).inspect_err(|_error| {
            #[cfg(test)]
            if std::env::var_os("KUBERIC_M3_NATIVE_SCOPE_DIAGNOSTICS").is_some() {
                eprintln!("native authorization session-stage refusal: completion={completion}, error={_error}");
            }
        })?;
        let local = self.publication().await?;
        let primary = self.local == binding.primary();
        let remote = if primary {
            binding.secondary()
        } else {
            binding.primary()
        };
        let publication = if binding.recovery {
            self.recovery_peer(
                &remote.identity,
                &remote.process_session_id,
                binding.history(),
            )
            .await?
        } else {
            self.discover(
                &remote.identity,
                &remote.process_session_id,
                binding.history(),
            )
            .await?
        };
        let (source, target) = if primary {
            (&local, &publication)
        } else {
            (&publication, &local)
        };
        if binding.target.role == ReplicaRole::IdleSecondary
            && target.role == ReplicaRole::ActiveSecondary
            && (!self.sessions.completed(binding)
                || !primary
                    && self.selected_lifecycle()
                        != Some((binding.epoch(), ReplicaRole::ActiveSecondary)))
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let changed_lease = source.scope.configuration.current
            != binding.source.scope.configuration.current
            || target.scope.configuration.current != binding.target.scope.configuration.current
            || source.scope.configuration.previous != binding.source.scope.configuration.previous
            || target.scope.configuration.previous != binding.target.scope.configuration.previous;
        if changed_lease {
            if !self.sessions.completed(binding)
                || target.role == ReplicaRole::ActiveSecondary
                    && self.local == binding.secondary()
                    && self.selected_lifecycle()
                        != Some((binding.epoch(), ReplicaRole::ActiveSecondary))
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            if primary {
                let installed = self
                    .configuration
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or(RuntimeError::AuthorityNotAdmitted)?;
                if installed.0 != source.scope.configuration.current
                    && source.stage != ReplicaPublicationStage::DiscoveryPending
                {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                let facade = self
                    .facade
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .ok_or(RuntimeError::Closed)?;
                let super::Target::Primary(native) =
                    facade.service().target().map_err(application)?
                else {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                };
                let peer = self.current_peer(binding, true)?;
                let current = native.current_transport_sessions().map_err(application)?;
                let current_root = current.as_ref().is_some_and(|current| {
                    current.epoch == binding.epoch()
                        && current.members.iter().any(|(id, identity, active)| {
                            *id == binding.secondary().identity.replica_id.value()
                                && *active
                                && identity.remote_incarnation == peer.incarnation
                                && identity.primary_endpoint == peer.route.sender_address
                                && identity.remote_endpoint == peer.route.remote_address
                        })
                });
                let copied_idle = source.stage == ReplicaPublicationStage::DiscoveryPending
                    && binding.target.role == ReplicaRole::IdleSecondary
                    && native
                        .completed_copy_transport(
                            &peer.route,
                            &super::super::session::SessionIdentity {
                                primary_endpoint: peer.route.sender_address.clone(),
                                remote_endpoint: peer.route.remote_address.clone(),
                                remote_incarnation: peer.incarnation,
                            },
                            binding.epoch(),
                        )
                        .is_ok();
                if !current_root && !copied_idle {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
            } else {
                self.confirm_source_scope(binding).await?;
            }
        }
        if completion
            && (source.scope.configuration.current != target.scope.configuration.current
                || source.scope.configuration.previous != target.scope.configuration.previous)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        for (is_source, now, origin) in [
            (true, source, &binding.source),
            (false, target, &binding.target),
        ] {
            let ordinary_scope = publication_admitted(
                now,
                origin,
                binding,
                is_source,
                completion
                    && !binding.admitted_reconfiguration()
                    && !(binding.recovery && !is_source),
            ) && (binding.replacement().is_none()
                || now.scope.configuration.current == binding.source.scope.configuration.current
                || self.sessions.completed(binding));
            let cohort_scope = if !ordinary_scope
                && binding.replacement().is_none()
                && !completion
                && replacement_cohort_shape(now, binding, is_source)
                && ((now.stage == ReplicaPublicationStage::DiscoveryPending
                    && now.role == ReplicaRole::None)
                    || (now.stage == ReplicaPublicationStage::AchievedScope
                        && now.role
                            == if is_source {
                                ReplicaRole::Primary
                            } else {
                                ReplicaRole::ActiveSecondary
                            })) {
                if primary {
                    self.sessions
                        .completed_replacement_for(binding)
                        .is_some_and(|selected| replacement_scope_from(now, &selected, origin))
                } else {
                    self.confirm_source_scope(binding).await?;
                    true
                }
            } else {
                false
            };
            if Endpoint::publication(now) != Endpoint::publication(origin)
                || now.scope.history != origin.scope.history
                || now.scope.epoch != origin.scope.epoch
                || now.scope.configuration.current.primary_id
                    != binding.primary().identity.replica_id
                || (!ordinary_scope && !cohort_scope)
            {
                #[cfg(test)]
                if std::env::var_os("KUBERIC_M3_NATIVE_SCOPE_DIAGNOSTICS").is_some() {
                    eprintln!(
                        "native publication refusal: source={is_source}, completion={completion}, ordinary={ordinary_scope}, cohort={cohort_scope}, physical={}, history={}, epoch={:?}/{:?}, stage={:?}, role={:?}, current={:?}/{:?}, previous={:?}/{:?}, peers={}",
                        Endpoint::publication(now) == Endpoint::publication(origin),
                        now.scope.history == origin.scope.history,
                        now.scope.epoch,
                        origin.scope.epoch,
                        now.stage,
                        now.role,
                        now.scope.configuration.current.configuration_id,
                        origin.scope.configuration.current.configuration_id,
                        now.scope
                            .configuration
                            .previous
                            .as_ref()
                            .map(|previous| &previous.configuration_id),
                        origin
                            .scope
                            .configuration
                            .previous
                            .as_ref()
                            .map(|previous| &previous.configuration_id),
                        now.scope.configuration.replicas.len()
                    );
                }
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            for peer in &now.scope.configuration.replicas {
                if (!peer.is_up || !peer.is_ready) && !peer.must_catchup {
                    // These descriptors retain logical quorum obligations,
                    // not a claim that an active native root exists.
                    continue;
                }
                if primary
                    && origin.scope.configuration.replicas.contains(peer)
                    && self
                        .production
                        .lock()
                        .unwrap()
                        .as_ref()
                        .and_then(Weak::upgrade)
                        .is_some_and(|owner| owner.frozen_previous_peer(now, peer))
                {
                    continue;
                }
                let endpoint =
                    if peer.identity == binding.primary().identity {
                        Some(binding.primary())
                    } else if peer.identity == binding.secondary().identity {
                        Some(binding.secondary())
                    } else if primary {
                        let session = self
                            .sessions
                            .metadata_for_peer(peer)
                            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
                        // This validates an unrelated configured descriptor, not
                        // that peer's effects/ACKs. Its own admission still checks
                        // Active above. Retirement must not fence healthy quorum.
                        let previous_only =
                            !now.scope
                                .configuration
                                .current
                                .members
                                .iter()
                                .any(|member| member.identity == peer.identity)
                                && now.scope.configuration.previous.as_ref().is_some_and(
                                    |previous| {
                                        previous
                                            .members
                                            .iter()
                                            .any(|member| member.identity == peer.identity)
                                    },
                                )
                                && !peer.is_up
                                && !peer.is_ready
                                && !peer.must_catchup
                                && self.sessions.retained_completed(&session)
                                && session.epoch().data_loss_number
                                    == binding.epoch().data_loss_number
                                && session.epoch().configuration_number
                                    < binding.epoch().configuration_number;
                        let continuing_current = peer.role == ReplicaRole::ActiveSecondary
                            && binding
                                .source
                                .scope
                                .configuration
                                .replicas
                                .iter()
                                .any(|selected| {
                                    selected.identity == peer.identity
                                        && selected.storage_incarnation == peer.storage_incarnation
                                        && selected.process_session_id == peer.process_session_id
                                        && selected.replication_address == peer.replication_address
                                        && selected.is_up
                                        && selected.is_ready
                                        && selected.role == ReplicaRole::ActiveSecondary
                                })
                            && now
                                .scope
                                .configuration
                                .current
                                .members
                                .iter()
                                .any(|member| {
                                    member.identity == peer.identity
                                        && member.role == ReplicaRole::ActiveSecondary
                                })
                            && self.sessions.retained_completed(&session)
                            && self.sessions.active(&session).is_err()
                            && session.epoch().data_loss_number == binding.epoch().data_loss_number
                            && session.epoch().configuration_number
                                < binding.epoch().configuration_number;
                        if session.primary() != binding.primary()
                            || session.history() != binding.history()
                            || (session.epoch() != binding.epoch()
                                && !previous_only
                                && !continuing_current)
                        {
                            return Err(RuntimeError::AuthorityNotAdmitted);
                        }
                        Some(session.secondary())
                    } else {
                        None
                    };
                let matches = if let Some(endpoint) = endpoint {
                    peer.identity == endpoint.identity
                        && peer.process_session_id == endpoint.process_session_id
                        && peer.storage_incarnation == endpoint.storage
                        && peer.replication_address == endpoint.address
                        || self
                            .sessions
                            .binding_for(peer.identity.replica_id.value())
                            .is_some_and(|replacement| {
                                replacement.recovery
                                    && (peer.identity != binding.secondary().identity
                                        || self.sessions.active(&replacement).is_ok())
                                    && replacement.secondary() == endpoint
                                    && replacement
                                        .source
                                        .scope
                                        .configuration
                                        .replicas
                                        .contains(peer)
                                    && peer.storage_incarnation == endpoint.storage
                            })
                } else {
                    // The source checks every descriptor against its actual
                    // private bindings, including pending compatible scopes.
                    // Requiring unrelated receivers to be reachable here
                    // would turn a single-peer outage into lost healthy quorum.
                    // A confirmation checks both complete publications.
                    self.confirm_source_scope(binding).await?;
                    true
                };
                if !matches {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
            }
        }
        let lifecycle = *self.lifecycle.lock().unwrap();
        if !lifecycle.is_some_and(|(epoch, role)| {
            epoch == binding.epoch()
                && if primary {
                    role == ReplicaRole::Primary
                } else {
                    matches!(
                        role,
                        ReplicaRole::IdleSecondary | ReplicaRole::ActiveSecondary
                    )
                }
        }) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if primary {
            let configuration = self.configuration.lock().unwrap();
            let matches = |publication: &ReplicaPublication| {
                let scope = &publication.scope.configuration;
                (binding.replacement().is_some()
                    && replacement_scope(
                        publication,
                        binding,
                        publication.scope.storage == binding.source.scope.storage,
                    ))
                    || self
                        .sessions
                        .completed_replacement_for(binding)
                        .is_some_and(|selected| {
                            let origin =
                                if publication.scope.storage == binding.source.scope.storage {
                                    &binding.source
                                } else {
                                    &binding.target
                                };
                            replacement_scope_from(publication, &selected, origin)
                        })
                    || (scope.current == binding.source.scope.configuration.current
                        && scope.previous == binding.source.scope.configuration.previous)
                    || configuration.as_ref().is_some_and(|(current, previous)| {
                        let selected = &scope.current == current
                            && (&scope.previous == previous || scope.previous.is_none());
                        let prior =
                            previous.as_ref() == Some(&scope.current) && scope.previous.is_none();
                        // A pending callback may advertise its next joint scope
                        // before installation. Only existing sessions continue;
                        // native voting still uses the installed configuration.
                        let installing = publication.stage
                            == ReplicaPublicationStage::DiscoveryPending
                            && scope.current.epoch == current.epoch
                            && scope.current.primary_id == current.primary_id
                            && scope.current.members.iter().any(|member| {
                                member.identity == binding.secondary().identity
                                    && member.role == ReplicaRole::ActiveSecondary
                            });
                        (selected || prior || installing)
                            && (current.members.iter().any(|member| {
                                member.identity == binding.secondary().identity
                                    && member.role == ReplicaRole::ActiveSecondary
                            }) || current == &binding.source.scope.configuration.current)
                    })
            };
            if !matches(source) || !matches(target) {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
        }
        self.sessions.active(binding)?;
        Ok(())
    }

    pub(super) async fn confirm_effect(&self, binding: &Binding) -> Result<()> {
        let result = async {
            self.current_peer(binding, self.local == binding.primary())?;
            self.authorize_publications(binding, false).await?;
            if self.local == binding.secondary() {
                self.control(binding, Control::Confirm).await?;
            }
            self.authorize(binding).await?;
            self.current_peer(binding, self.local == binding.primary())
                .map(|_| ())
        }
        .await;
        if let Err(error) = &result {
            #[cfg(feature = "testing")]
            if std::env::var_os("KUBERIC_M3_ELECTION_DIAGNOSTICS").is_some() {
                eprintln!(
                    "native effect confirmation failure: receiver={}, nonce={}, error={error:?}",
                    self.local.identity.instance_id, binding.nonce
                );
            }
            tracing::warn!(
                receiver = %self.local.identity.instance_id,
                nonce = %binding.nonce,
                %error,
                "native effect confirmation failed"
            );
            if !matches!(error, RuntimeError::PeerUnavailable(_))
                || self.fence_unavailable_receiver(binding).is_err()
            {
                self.quarantine(binding);
            }
        }
        result
    }

    fn quarantine(&self, binding: &Binding) {
        // Already fenced traffic is not a new fault in the retained receiver.
        // A failed confirmation of an active receiver still contains it.
        self.sessions.quarantine(binding, |receiver| {
            if receiver && self.local == binding.secondary() {
                if let Some(facade) = self.facade.lock().unwrap().as_ref().and_then(Weak::upgrade) {
                    facade.abort();
                }
                if let Some(peer) = self
                    .peers
                    .lock()
                    .unwrap()
                    .get(&binding.primary().identity.replica_id.value())
                    .filter(|peer| peer.binding == *binding)
                {
                    peer.current.store(false, Ordering::Release);
                    peer.carrier.shutdown();
                }
            }
        });
        if self.local == binding.primary() {
            self.observe_transport_quorum();
        }
    }

    pub(super) fn observe_transport_quorum(&self) {
        let owner = self
            .production
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade);
        if let Some(owner) = owner {
            owner.observe_transport_quorum();
        }
    }

    pub(super) async fn effect_future<T>(
        &self,
        binding: &Binding,
        future: impl std::future::Future<Output = T>,
    ) -> Result<T> {
        let mut future = Box::pin(future);
        let mut issued = false;
        futures::future::poll_fn(|cx| {
            if issued {
                return future.as_mut().poll(cx).map(Ok);
            }
            // Once issued, opaque provider work must be polled to real settlement,
            // even if retirement wins while it awaits. Its result is checked separately.
            match self.sessions.effect(binding, || future.as_mut().poll(cx)) {
                Ok(poll) => {
                    issued = true;
                    poll.map(Ok)
                }
                Err(error) => std::task::Poll::Ready(Err(error)),
            }
        })
        .await
    }

    pub(super) fn confirmed_stream(
        self: &Arc<Self>,
        binding: Binding,
        stream: crate::application::OperationDataStream,
    ) -> crate::application::OperationDataStream {
        use futures::StreamExt;
        let owner = self.clone();
        Box::pin(futures::stream::unfold(
            (stream, false),
            move |(mut stream, ended)| {
                let owner = owner.clone();
                let binding = binding.clone();
                async move {
                    if ended {
                        return None;
                    }
                    let result = async {
                        owner.confirm_effect(&binding).await?;
                        let item = owner.effect_future(&binding, stream.next()).await?;
                        owner.confirm_effect(&binding).await?;
                        Ok(item)
                    }
                    .await;
                    match result {
                        Ok(Some(item)) => Some((item, (stream, false))),
                        Ok(None) => None,
                        Err(error) => Some((Err(error), (stream, true))),
                    }
                }
            },
        ))
    }

    fn authorization(&self) -> String {
        format!("Bearer {}", self.credentials)
    }

    fn metadata(&self, binding: &Binding, control: bool) -> Result<MetadataMap> {
        let mut metadata = MetadataMap::new();
        metadata.insert(
            "authorization",
            self.authorization().parse().map_err(application)?,
        );
        let encoded = serde_json::to_vec(binding).map_err(application)?;
        if encoded.len() > MAX_CONTROL {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        metadata.insert_bin(
            BINDING_HEADER,
            tonic::metadata::MetadataValue::from_bytes(&encoded),
        );
        if control {
            metadata.insert(CONTROL_HEADER, "1".parse().unwrap());
        }
        Ok(metadata)
    }

    async fn address(&self, binding: &Binding, _command: Option<Control>) -> Result<String> {
        let route = route(binding, &self.local)?;
        let address = route.remote_address;
        #[cfg(feature = "testing")]
        let address = crate::replicator::testing_transport::route(
            match _command {
                Some(
                    Control::Reserve
                    | Control::Continue
                    | Control::CheckContinuation
                    | Control::Reform
                    | Control::CheckReformation,
                ) => crate::replicator::TestingTcpRouteKind::EngineReserve,
                Some(Control::Activate) => crate::replicator::TestingTcpRouteKind::EngineActivate,
                Some(Control::Confirm) => crate::replicator::TestingTcpRouteKind::EngineConfirm,
                Some(Control::Complete) => crate::replicator::TestingTcpRouteKind::EngineComplete,
                Some(Control::Retire) => crate::replicator::TestingTcpRouteKind::EngineRetire,
                Some(Control::Recover | Control::Recovered) => {
                    crate::replicator::TestingTcpRouteKind::EngineConfirm
                }
                // Reformation reconnects an initialized receiver without a
                // copy/Build exchange. Keep its actual carrier dial distinct
                // from the initial-copy cut and its diagnostic counter.
                None if binding.reformation => crate::replicator::TestingTcpRouteKind::Replication,
                None => crate::replicator::TestingTcpRouteKind::Build,
            },
            self.local.identity.instance_id.as_str(),
            if self.local == binding.primary() {
                binding.secondary().identity.instance_id.to_string()
            } else {
                binding.primary().identity.instance_id.to_string()
            }
            .as_str(),
            address,
        )
        .await
        .map_err(application)?;
        Ok(address)
    }

    pub(super) async fn control(&self, binding: &Binding, command: Control) -> Result<()> {
        let mut stop = self.stop.subscribe();
        loop {
            match self.control_once(binding, command).await {
                Err(RuntimeError::QueueFull) => {
                    tokio::select! {
                        biased;
                        _ = stop.wait_for(|stopped| *stopped) => return Err(RuntimeError::Closed),
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                    }
                }
                Err(RuntimeError::PeerUnavailable(_))
                    if !matches!(
                        command,
                        Control::Confirm | Control::CheckContinuation | Control::CheckReformation
                    ) =>
                {
                    tokio::select! {
                        biased;
                        _ = stop.wait_for(|stopped| *stopped) => return Err(RuntimeError::Closed),
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                    }
                }
                result => return result,
            }
        }
    }

    async fn control_once(&self, binding: &Binding, command: Control) -> Result<()> {
        let retry_connect = binding.recovery
            && matches!(
                command,
                Control::Reserve | Control::Activate | Control::Recover | Control::Recovered
            );
        let control = ControlEnvelope {
            binding: binding.clone(),
            command,
        };
        let bytes = serde_json::to_vec(&control).map_err(application)?;
        if bytes.len() > MAX_CONTROL {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let route = route(binding, &self.local)?;
        let request = route.envelope(wire::envelope::Body::EngineSessionControl(
            wire::EngineSessionControl {
                canonical_json: bytes.into(),
            },
        ));
        let exchange = async {
            let channel = tonic::transport::Endpoint::from_shared(
                self.address(binding, Some(command)).await?,
            )
            .map_err(application)?
            .connect_timeout(Duration::from_secs(5))
            .connect()
            .await
            .map_err(|error| {
                if retry_connect {
                    RuntimeError::QueueFull
                } else {
                    RuntimeError::PeerUnavailable(error.to_string())
                }
            })?;
            let mut client = wire::carrier_client::CarrierClient::new(channel)
                .max_decoding_message_size(MAX_CONTROL)
                .max_encoding_message_size(MAX_CONTROL);
            let mut request = Request::new(futures::stream::iter([request]));
            *request.metadata_mut() = self.metadata(binding, true)?;
            let response = client.exchange(request).await.map_err(|error| {
                if error.code() == tonic::Code::ResourceExhausted
                    || (retry_connect
                        && matches!(
                            error.code(),
                            tonic::Code::Unavailable | tonic::Code::DeadlineExceeded
                        ))
                {
                    RuntimeError::QueueFull
                } else if error.code() == tonic::Code::PermissionDenied {
                    if matches!(
                        command,
                        Control::Recover
                            | Control::Recovered
                            | Control::Reform
                            | Control::CheckReformation
                            | Control::CheckContinuation
                    ) {
                        RuntimeError::ControlPlane(format!(
                            "{command:?} session rejected: {}",
                            error.message()
                        ))
                    } else {
                        RuntimeError::AuthorityNotAdmitted
                    }
                } else {
                    crate::control::peer_observation_error(error)
                }
            })?;
            if response
                .metadata()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                != Some(self.authorization().as_str())
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            let mut response = response.into_inner();
            let envelope = response
                .message()
                .await
                .map_err(crate::control::peer_observation_error)?
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            route.check(&envelope, true).map_err(application)?;
            let Some(wire::envelope::Body::EngineSessionControl(body)) = envelope.body else {
                return Err(RuntimeError::AuthorityNotAdmitted);
            };
            if body.canonical_json.len() > MAX_CONTROL
                || serde_json::from_slice::<ControlEnvelope>(&body.canonical_json)
                    .map_err(application)?
                    != control
                || response
                    .message()
                    .await
                    .map_err(crate::control::peer_observation_error)?
                    .is_some()
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            Ok(())
        };
        let mut stop = self.stop.subscribe();
        if *stop.borrow() {
            return Err(RuntimeError::Closed);
        }
        // A ready wire result wins over simultaneous shutdown; only an
        // unfinished network wait is cancelled, never a selected failure.
        tokio::select! {
            biased;
            result = tokio::time::timeout(Duration::from_secs(5), exchange) => {
                result.map_err(|_| {
                    if retry_connect {
                        RuntimeError::QueueFull
                    } else {
                        RuntimeError::PeerUnavailable("engine session control timed out".into())
                    }
                })?
            },
            _ = stop.wait_for(|stopped| *stopped) => Err(RuntimeError::Closed),
        }
    }

    pub(super) async fn establish(self: &Arc<Self>, binding: &Binding) -> Result<i64> {
        self.validate_publications(binding).await?;
        self.control(binding, Control::Reserve).await?;
        self.authorize(binding).await?;
        self.admit(binding.clone(), true).await
    }

    async fn receive_control(self: &Arc<Self>, control: &ControlEnvelope) -> Result<()> {
        if matches!(
            control.command,
            Control::Confirm | Control::CheckContinuation | Control::CheckReformation
        ) {
            return self.receive_control_inner(control).await;
        }
        let retained = self.effects.lock().unwrap().contains_key(&(
            control.binding.nonce.clone(),
            Effect::Control(control.command),
        ));
        if !retained && control.command == Control::Complete {
            // A rejected context check selected no effect and owns no failure
            // of the existing root. Actual completion remains memoized below.
            self.authorize_completion(&control.binding).await?;
        }
        let owner = self.clone();
        let selected = control.clone();
        self.owned_effect(
            &control.binding,
            Effect::Control(control.command),
            async move { owner.receive_control_inner(&selected).await.map(|()| 0) },
        )
        .await?;
        if matches!(
            control.command,
            Control::Reserve | Control::Reform | Control::Continue
        ) {
            self.current_peer(&control.binding, false)?;
            self.authorize(&control.binding).await?;
            self.current_peer(&control.binding, false)?;
        } else if control.command == Control::Activate {
            if !control.binding.recovery && !control.binding.retained_receiver() {
                self.validate_build_operation(&control.binding, true)
                    .await?;
            }
            self.authorize(&control.binding).await?;
        } else if matches!(control.command, Control::Complete | Control::Recovered) {
            self.current_peer(&control.binding, self.local == control.binding.primary())?;
            self.authorize_completion(&control.binding).await?;
            self.current_peer(&control.binding, self.local == control.binding.primary())?;
        } else if control.command == Control::Recover && retained {
            let owner = self
                .production
                .lock()
                .unwrap()
                .as_ref()
                .and_then(Weak::upgrade)
                .ok_or(RuntimeError::Closed)?;
            owner.recover_source(&control.binding, false).await?;
        }
        Ok(())
    }

    async fn receive_control_inner(self: &Arc<Self>, control: &ControlEnvelope) -> Result<()> {
        let binding = &control.binding;
        #[cfg(feature = "testing")]
        if control.command == Control::Confirm {
            self.test_cut(
                crate::replicator::TestingTcpRouteKind::ConfirmReceived,
                binding,
            )
            .await?;
        }
        let primary = self.local == binding.primary();
        if !primary && self.local != binding.secondary() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        match control.command {
            Control::CheckReformation | Control::Reform if !primary && binding.reformation => {
                let _continuation = self.continuations.lock().await;
                self.validate_publications(binding).await?;
                if self.selected_lifecycle()
                    != Some((binding.epoch(), ReplicaRole::ActiveSecondary))
                {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                let facade = self
                    .facade
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .ok_or(RuntimeError::Closed)?;
                if self.sessions.inbound().is_none() {
                    let secondary = facade
                        .service()
                        .demotion_receiver(binding.epoch())
                        .map_err(application)?;
                    self.sessions.first_demotion_receiver(binding)?;
                    let checked =
                        super::warm_source_transfer::check(&self.context, binding, None).await?;
                    if control.command == Control::CheckReformation {
                        return Ok(());
                    }
                    self.control(binding, Control::Activate).await?;
                    self.settle_role_sessions().await?;
                    self.validate_publications(binding).await?;
                    let current =
                        super::warm_source_transfer::check(&self.context, binding, None).await?;
                    let root = facade
                        .service()
                        .demotion_receiver(binding.epoch())
                        .map_err(application)?;
                    if current != checked
                        || !root.same_instance(&secondary)
                        || self.selected_lifecycle()
                            != Some((binding.epoch(), ReplicaRole::ActiveSecondary))
                    {
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    }
                    self.sessions.associate_demotion_receiver(binding, || {
                        root.check_demotion_root(binding.epoch())
                            .map_err(application)
                    })?;
                    #[cfg(feature = "testing")]
                    self.test_cut(
                        crate::replicator::TestingTcpRouteKind::ReceiverReplaced,
                        binding,
                    )
                    .await?;
                    self.sessions.activate_target(binding)?;
                    #[cfg(feature = "testing")]
                    self.test_cut(
                        crate::replicator::TestingTcpRouteKind::TargetActivated,
                        binding,
                    )
                    .await?;
                    self.peer(binding.clone(), false).await?;
                    return Ok(());
                }
                let normal = self
                    .sessions
                    .inbound()
                    .filter(|old| old.epoch() == binding.epoch())
                    .map(|old| async move {
                        let checked =
                            super::warm_source_transfer::check(&self.context, binding, Some(&old))
                                .await?;
                        let predecessor = self.sessions.normal_predecessor(binding, &checked)?;
                        Ok::<_, RuntimeError>((checked, predecessor))
                    });
                let (checked, old) = if let Some(normal) = normal {
                    let (checked, old) = normal.await?;
                    (Some(checked), old)
                } else {
                    (None, self.sessions.reformation_predecessor(binding)?)
                };
                let facade = self
                    .facade
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .ok_or(RuntimeError::Closed)?;
                let secondary = match facade.service().target().map_err(application)? {
                    super::Target::Secondary(secondary) => secondary,
                    _ => return Err(RuntimeError::AuthorityNotAdmitted),
                };
                let predecessor = route(&old, &self.local)?;
                let successor = route(binding, &self.local)?;
                secondary
                    .reform_transport(&predecessor, &successor, binding.epoch(), false)
                    .map_err(application)?;
                let progress = secondary.current_progress().map_err(application)?;
                let capability = secondary.catchup_capability().map_err(application)?;
                if !binding
                    .source
                    .scope
                    .configuration
                    .replicas
                    .iter()
                    .any(|peer| {
                        peer.identity == self.local.identity
                            && peer.is_up
                            && peer.is_ready
                            && if checked.is_some() {
                                peer.current_progress >= 0 && progress >= peer.current_progress
                            } else {
                                peer.current_progress == progress
                                    && peer.catch_up_capability == capability
                            }
                    })
                {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                if control.command == Control::CheckReformation {
                    return Ok(());
                }
                // Authenticate the exact source's selected proposal before joining
                // or rotating anything. A readiness probe never grants a session.
                self.control(binding, Control::Activate).await?;
                if let Some(checked) = &checked {
                    let current =
                        super::warm_source_transfer::check(&self.context, binding, Some(&old))
                            .await?;
                    if &current != checked {
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    }
                    self.sessions.retire_normal_predecessor(binding, checked)?;
                }
                self.join_predecessor(&old).await?;
                self.validate_publications(binding).await?;
                if self.selected_lifecycle()
                    != Some((binding.epoch(), ReplicaRole::ActiveSecondary))
                {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                if let Some(checked) = &checked {
                    let current =
                        super::warm_source_transfer::check(&self.context, binding, Some(&old))
                            .await?;
                    if &current != checked {
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    }
                    self.sessions.reform_normal_receiver(binding, checked, || {
                        secondary
                            .reform_transport(&predecessor, &successor, binding.epoch(), true)
                            .map_err(application)
                    })?;
                } else {
                    self.sessions.reform_receiver(binding, || {
                        secondary
                            .reform_transport(&predecessor, &successor, binding.epoch(), true)
                            .map_err(application)
                    })?;
                }
                #[cfg(feature = "testing")]
                self.test_cut(
                    crate::replicator::TestingTcpRouteKind::ReceiverReplaced,
                    binding,
                )
                .await?;
                self.sessions.activate_target(binding)?;
                #[cfg(feature = "testing")]
                self.test_cut(
                    crate::replicator::TestingTcpRouteKind::TargetActivated,
                    binding,
                )
                .await?;
                self.peer(binding.clone(), false).await?;
                Ok(())
            }
            Control::Continue | Control::CheckContinuation if !primary => {
                let _continuation = self.continuations.lock().await;
                self.validate_publications(binding).await?;
                if self.selected_lifecycle()
                    != Some((binding.epoch(), ReplicaRole::ActiveSecondary))
                {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                let old = self.sessions.continuation_predecessor(binding)?;
                if self.sessions.inbound().as_ref() != Some(&old) {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                let facade = self
                    .facade
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .ok_or(RuntimeError::Closed)?;
                let secondary = match facade.service().target().map_err(application)? {
                    super::Target::Secondary(secondary) => secondary,
                    _ => return Err(RuntimeError::AuthorityNotAdmitted),
                };
                secondary
                    .check_continuation_transport(&route(binding, &self.local)?, binding.epoch())
                    .map_err(application)?;
                if control.command == Control::CheckContinuation {
                    return Ok(());
                }
                self.control(binding, Control::Activate).await?;
                self.join_predecessor(&old).await?;
                self.validate_publications(binding).await?;
                if self.selected_lifecycle()
                    != Some((binding.epoch(), ReplicaRole::ActiveSecondary))
                {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                let native_route = route(binding, &self.local)?;
                self.sessions.continue_session(binding, true, || {
                    secondary
                        .continue_transport(&native_route, binding.epoch())
                        .map_err(application)
                })?;
                #[cfg(feature = "testing")]
                self.test_cut(
                    crate::replicator::TestingTcpRouteKind::ReceiverReplaced,
                    binding,
                )
                .await?;
                self.sessions.activate_target(binding)?;
                #[cfg(feature = "testing")]
                self.test_cut(
                    crate::replicator::TestingTcpRouteKind::TargetActivated,
                    binding,
                )
                .await?;
                self.peer(binding.clone(), false).await?;
                Ok(())
            }
            Control::Recover | Control::Recovered if primary && binding.recovery => {
                #[cfg(feature = "testing")]
                if control.command == Control::Recover {
                    self.test_cut(
                        crate::replicator::TestingTcpRouteKind::RecoveryReceived,
                        binding,
                    )
                    .await?;
                }
                let owner = self
                    .production
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .ok_or(RuntimeError::Closed)?;
                owner
                    .recover_source(binding, control.command == Control::Recovered)
                    .await
            }
            Control::Reserve if !primary => {
                self.validate_publications(binding).await?;
                self.validate_build_operation(binding, false).await?;
                if self
                    .production
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .is_some_and(|owner| owner.requires_retained_reformation())
                {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                self.sessions.reserve(binding)?;
                // Reservation exists before the authenticated source can become Active.
                self.control(binding, Control::Activate).await?;
                self.validate_publications(binding).await?;
                self.validate_build_operation(binding, false).await?;
                self.sessions.activate_target(binding)?;
                #[cfg(feature = "testing")]
                self.test_cut(
                    crate::replicator::TestingTcpRouteKind::TargetActivated,
                    binding,
                )
                .await?;
                self.peer(binding.clone(), false).await?;
                Ok(())
            }
            Control::Activate if primary => {
                self.validate_publications(binding).await?;
                if binding.reformation {
                    self.associate_configuration(binding).await?;
                }
                if !binding.recovery && !binding.retained_receiver() {
                    self.validate_build_operation(binding, true).await?;
                }
                if self.sessions.active(binding).is_ok() {
                    Ok(())
                } else {
                    self.sessions.activate_source(binding)
                }
            }
            Control::Confirm if primary => {
                self.current_peer(binding, true)?;
                self.authorize(binding).await?;
                self.current_peer(binding, true).map(|_| ())
            }
            Control::Complete if !primary => {
                self.authorize_completion(binding).await?;
                self.confirm_effect(binding).await?;
                self.authorize_completion(binding).await?;
                self.sessions.complete(binding)
            }
            Control::Retire if !primary => {
                let owns_receiver = self.sessions.inbound().as_ref() == Some(binding);
                self.sessions.retire(binding)?;
                if owns_receiver {
                    let facade = self
                        .facade
                        .lock()
                        .unwrap()
                        .as_ref()
                        .and_then(Weak::upgrade)
                        .ok_or(RuntimeError::Closed)?;
                    facade.abort();
                    let peer = self
                        .peers
                        .lock()
                        .unwrap()
                        .get(&binding.primary().identity.replica_id.value())
                        .cloned();
                    if let Some(peer) = &peer {
                        peer.current.store(false, Ordering::Release);
                        peer.carrier.shutdown();
                        peer.tasks.close();
                    }
                    let (joined, ()) = tokio::join!(facade.drain(), async {
                        if let Some(peer) = peer {
                            peer.tasks.wait().await;
                        }
                    });
                    let settlement = self
                        .ack_settlements
                        .lock()
                        .unwrap()
                        .get(&binding.nonce)
                        .cloned();
                    if let Some(settlement) = settlement {
                        settlement.tasks.close();
                        settlement.tasks.wait().await;
                        if let Some(error) = settlement.failure.lock().unwrap().as_ref() {
                            return Err(application(error));
                        }
                    }
                    joined?;
                    self.check_health()?;
                }
                self.sessions.settled(binding)
            }
            _ => Err(RuntimeError::AuthorityNotAdmitted),
        }
    }

    pub(super) async fn retire_remote(self: &Arc<Self>, binding: &Binding) -> Result<()> {
        self.sessions.retire(binding)?;
        if let Some(peer) = self
            .peers
            .lock()
            .unwrap()
            .get(&binding.secondary().identity.replica_id.value())
            .filter(|peer| peer.binding == *binding)
        {
            peer.current.store(false, Ordering::Release);
            peer.carrier.shutdown();
        }
        let owns_peer = self
            .peers
            .lock()
            .unwrap()
            .values()
            .any(|peer| peer.binding == *binding)
            || self
                .retiring_peers
                .lock()
                .unwrap()
                .contains_key(&binding.nonce)
            || self
                .ack_settlements
                .lock()
                .unwrap()
                .contains_key(&binding.nonce);
        if owns_peer {
            self.join_predecessor(binding).await?;
        }
        match self.control(binding, Control::Retire).await {
            Ok(()) => self.sessions.settled(binding),
            Err(error) => {
                let owner = self.clone();
                let binding = binding.clone();
                self.spawn(async move {
                    let mut stop = owner.stop.subscribe();
                    loop {
                        tokio::select! {
                            biased;
                            _ = stop.wait_for(|stopped| *stopped) => return Ok(()),
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        }
                        if owner.control(&binding, Control::Retire).await.is_ok() {
                            owner.sessions.settled(&binding)?;
                            return Ok(());
                        }
                    }
                });
                Err(error)
            }
        }
    }

    pub(super) async fn admit(self: &Arc<Self>, binding: Binding, outgoing: bool) -> Result<i64> {
        self.peer(binding, outgoing)
            .await
            .map(|peer| peer.incarnation)
    }

    async fn peer(self: &Arc<Self>, binding: Binding, outgoing: bool) -> Result<Arc<Peer>> {
        let owner = self.clone();
        let selected = binding.clone();
        self.owned_effect(&binding, Effect::Peer(outgoing), async move {
            owner
                .create_peer(selected, outgoing)
                .await
                .map(|peer| peer.incarnation)
        })
        .await?;
        self.authorize(&binding).await?;
        self.current_peer(&binding, outgoing)
    }

    async fn create_peer(self: &Arc<Self>, binding: Binding, outgoing: bool) -> Result<Arc<Peer>> {
        if *self.stop.borrow() {
            return Err(RuntimeError::Closed);
        }
        self.authorize(&binding).await?;
        #[cfg(feature = "testing")]
        self.test_cut(
            crate::replicator::TestingTcpRouteKind::PeerCreating,
            &binding,
        )
        .await?;
        let route = route(&binding, &self.local)?;
        let fence = self.fence(&binding);
        let id = route.destination.replica;
        let (ack_owner, ack_generation) = if self.local == binding.secondary() {
            match self.facade.lock().unwrap().as_ref().and_then(Weak::upgrade) {
                Some(facade) => match facade.service().target().map_err(application)? {
                    super::Target::Secondary(secondary) => {
                        let (owner, generation) =
                            secondary.transport_identity().map_err(application)?;
                        (Some(owner), generation)
                    }
                    _ => return Err(RuntimeError::AuthorityNotAdmitted),
                },
                None => (None, 0),
            }
        } else {
            (None, 0)
        };
        let peer = self.sessions.effect(&binding, || -> Result<Arc<Peer>> {
            let mut peers = self.peers.lock().unwrap();
            if let Some(peer) = peers.get(&id).filter(|peer| peer.binding == binding) {
                Ok(peer.clone())
            } else {
                let previous = peers.remove(&id);
                let reuse_incarnation = previous
                    .as_ref()
                    .filter(|previous| previous.binding == binding)
                    .map(|previous| previous.incarnation);
                if let Some(previous) = previous {
                    previous.current.store(false, Ordering::Release);
                    previous.carrier.shutdown();
                    previous.tasks.close();
                    self.retiring_peers
                        .lock()
                        .unwrap()
                        .insert(previous.binding.nonce.clone(), previous);
                }
                let (carrier, ingress) =
                    Carrier::new(route.clone(), limits()).map_err(application)?;
                let incarnation = match reuse_incarnation {
                    Some(incarnation) => incarnation,
                    None => self
                        .next_incarnation
                        .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                            value.checked_add(1)
                        })
                        .map_err(|_| RuntimeError::Closed)?,
                };
                let current = Arc::new(AtomicBool::new(true));
                let peer = Arc::new(Peer {
                    binding: binding.clone(),
                    fence,
                    route,
                    carrier,
                    connecting: AtomicBool::new(false),
                    incarnation,
                    reformation_boundary: if self.local == binding.primary() && binding.reformation
                    {
                        let facade = self
                            .facade
                            .lock()
                            .unwrap()
                            .as_ref()
                            .and_then(Weak::upgrade)
                            .ok_or(RuntimeError::Closed)?;
                        let primary = match facade.service().target().map_err(application)? {
                            super::Target::Primary(primary) => primary,
                            _ => return Err(RuntimeError::AuthorityNotAdmitted),
                        };
                        Some(primary.current_progress().map_err(application)?)
                    } else {
                        None
                    },
                    fresh_ack: AtomicI64::new(-1),
                    initial_ack: Arc::new(AtomicI64::new(-1)),
                    native_installed: AtomicBool::new(binding.live_target.is_none()),
                    ack_generation,
                    ack_owner,
                    current: current.clone(),
                    tasks: TaskTracker::new(),
                });
                self.spawn_peer(
                    &binding,
                    &peer.tasks,
                    self.clone().run_ingress(binding.clone(), current, ingress),
                );
                peers.insert(id, peer.clone());
                Ok(peer)
            }
        })??;
        #[cfg(feature = "testing")]
        self.test_cut(
            crate::replicator::TestingTcpRouteKind::PeerInstalled,
            &binding,
        )
        .await?;
        if outgoing {
            self.sessions.effect(&binding, || -> Result<()> {
                if !peer.connecting.swap(true, Ordering::AcqRel) {
                    // Admit the first native send before returning to Build. Otherwise
                    // StartCopy races the unpolled connection task and is lost until retry.
                    let (initial, _) = peer.carrier.bind().map_err(application)?;
                    let owner = self.clone();
                    let connection = peer.clone();
                    self.spawn_peer(&binding, &peer.tasks, async move {
                        owner.run_connection(connection, initial).await
                    });
                }
                Ok(())
            })??;
        }
        Ok(peer)
    }

    pub(super) async fn run_ingress(
        self: Arc<Self>,
        binding: Binding,
        current: Arc<AtomicBool>,
        mut ingress: Ingress,
    ) -> Result<()> {
        let mut shutdown = self.stop.subscribe();
        loop {
            let delivery = tokio::select! {
                biased;
                _ = shutdown.wait_for(|stopped| *stopped) => return Ok(()),
                delivery = ingress.recv() => delivery,
            };
            match delivery {
                Some(Delivery::Message { envelope, .. }) => {
                    if !current.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    #[cfg(feature = "testing")]
                    self.test_cut(
                        crate::replicator::TestingTcpRouteKind::BeforeDispatch,
                        &binding,
                    )
                    .await?;
                    if let Err(error) = self.confirm_effect(&binding).await {
                        tracing::debug!(%error, "retired native delivery rejected");
                        #[cfg(feature = "testing")]
                        self.observe_ingress(&envelope, "sessionRejected".into());
                        continue;
                    }
                    if !current.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    if let Err(error) = validate_message_epochs(binding.epoch(), &envelope) {
                        tracing::warn!(%error, "unauthorized native frame epoch rejected");
                        #[cfg(feature = "testing")]
                        self.observe_ingress(&envelope, "epochRejected".into());
                        continue;
                    }
                    if binding.retained_receiver()
                        && !matches!(
                            envelope.body,
                            Some(
                                wire::envelope::Body::ReplicationOperation(_)
                                    | wire::envelope::Body::ReplicationAck(_)
                                    | wire::envelope::Body::RequestAck(_)
                            )
                        )
                    {
                        #[cfg(feature = "testing")]
                        self.observe_ingress(&envelope, "continuationCopyRejected".into());
                        continue;
                    }
                    if binding.live_target.is_some()
                        && self.local == binding.primary()
                        && let Some(wire::envelope::Body::ReplicationAck(ack)) = &envelope.body
                    {
                        let installed = self
                            .peers
                            .lock()
                            .unwrap()
                            .values()
                            .find(|peer| peer.binding == binding)
                            .is_some_and(|peer| peer.native_installed.load(Ordering::Acquire));
                        if !installed {
                            self.record_reformation_checkpoint(&binding, ack)?;
                            continue;
                        }
                    }
                    let dispatched = match self.sessions.effect(&binding, || {
                        #[cfg(all(test, feature = "testing"))]
                        if let Some(wire::envelope::Body::ReplicationAck(ack)) = &envelope.body {
                            self.sessions.removal_cut(
                                super::engine_session::RemovalCut::Dispatch {
                                    replica: envelope.source.as_ref().unwrap().replica,
                                    applied: ack.replication_applied,
                                },
                            );
                        }
                        self.registry.dispatch(&envelope)
                    }) {
                        Ok(dispatched) => dispatched,
                        Err(_) => continue,
                    };
                    #[cfg(feature = "testing")]
                    self.observe_ingress(&envelope, format!("{dispatched:?}"));
                    match dispatched {
                        Err(IngressError::Primary(PrimaryError::UnknownSession(replica)))
                            if matches!(
                                envelope.body,
                                Some(wire::envelope::Body::ReplicationAck(_))
                            ) =>
                        {
                            tracing::debug!(replica, "retired native session ACK dropped");
                            // Before a new source root exists, retain only this
                            // authenticated new receiver's real native checkpoint.
                            if binding.reformation
                                && self.local == binding.primary()
                                && let Some(wire::envelope::Body::ReplicationAck(ack)) =
                                    &envelope.body
                            {
                                self.record_reformation_checkpoint(&binding, ack)?;
                            }
                        }

                        result => {
                            let outcome = result.map_err(application)?;
                            if matches!(outcome, super::super::ingress::Disposition::Processed)
                                && self.local == binding.primary()
                                && binding.reformation
                                && let Some(wire::envelope::Body::ReplicationAck(ack)) =
                                    &envelope.body
                                && ack.replication_received >= 0
                                && ack.replication_applied >= 0
                                && ack.replication_applied <= ack.replication_received
                            {
                                if ack.error != 0
                                    || ack.copy_received != -1
                                    || ack.copy_applied != -1
                                {
                                    return Err(RuntimeError::InvalidReplication(
                                        "fresh receiver ACK contains copy/error credit".into(),
                                    ));
                                }
                                self.sessions.effect(&binding, || {
                                    if let Some(peer) = self
                                        .peers
                                        .lock()
                                        .unwrap()
                                        .values()
                                        .find(|peer| peer.binding == binding)
                                    {
                                        peer.fresh_ack
                                            .fetch_max(ack.replication_applied, Ordering::AcqRel);
                                    }
                                })?;
                            }
                        }
                    }
                }
                Some(Delivery::Rejected { error, .. }) => {
                    tracing::warn!(%error, "invalid native carrier envelope rejected");
                }
                Some(Delivery::Expired { .. } | Delivery::SupersededSend { .. }) => {}
                None => return Ok(()),
            }
        }
    }

    #[cfg(feature = "testing")]
    fn observe_ingress(&self, envelope: &wire::Envelope, outcome: String) {
        let mut observations = self.ingress_observations.lock().unwrap();
        // Bounded, read-only evidence from this actual owner's completed ingress.
        if observations.len() == 64 {
            observations.pop_front();
        }
        observations.push_back((envelope.message_id.clone(), outcome));
    }

    #[cfg(feature = "testing")]
    pub(super) async fn test_cut(
        &self,
        kind: crate::replicator::TestingTcpRouteKind,
        binding: &Binding,
    ) -> Result<()> {
        crate::replicator::testing_transport::route(
            kind,
            self.local.identity.instance_id.as_str(),
            if self.local == binding.primary() {
                binding.secondary().identity.instance_id.to_string()
            } else {
                binding.primary().identity.instance_id.to_string()
            }
            .as_str(),
            self.local.address.clone(),
        )
        .await
        .map_err(application)?;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn test_owner(
        context: ReplicatorFactoryContext,
        local: Endpoint,
        history: HistoryContext,
        test_publications: Vec<ReplicaPublication>,
        registry: MessageRegistry,
    ) -> Arc<Self> {
        Arc::new(Self {
            context,
            local,
            history,
            registry,
            credentials: Arc::from("test-token"),
            peers: Mutex::new(BTreeMap::new()),
            retiring_peers: Mutex::new(BTreeMap::new()),
            stop: watch::channel(false).0,
            tasks: TaskTracker::new(),
            failure: Arc::new(Mutex::new(None)),
            next_incarnation: AtomicI64::new(1),
            sessions: Arc::new(Sessions::default()),
            facade: Mutex::new(None),
            production: Mutex::new(None),
            controls: Arc::new(tokio::sync::Semaphore::new(32)),
            continuations: tokio::sync::Mutex::new(()),
            lifecycle: Mutex::new(None),
            configuration: Mutex::new(None),
            acknowledgements: Mutex::new(None),
            ack_settlements: Mutex::new(BTreeMap::new()),
            effects: Mutex::new(BTreeMap::new()),
            #[cfg(feature = "testing")]
            native_credits: AtomicI64::new(0),
            #[cfg(feature = "testing")]
            ingress_observations: Mutex::new(std::collections::VecDeque::new()),
            test_publications: Mutex::new(test_publications),
        })
    }

    #[cfg(test)]
    pub(super) fn test_publish(&self, publications: Vec<ReplicaPublication>) {
        *self.test_publications.lock().unwrap() = publications;
    }

    #[cfg(test)]
    pub(super) fn test_listen(self: &Arc<Self>, listener: TcpListener) {
        let service = Listener(self.clone());
        let mut stop = self.stop.subscribe();
        self.spawn(async move {
            tonic::transport::Server::builder()
                .add_service(PUBLIC_V1_DATA_SERVICES.carrier(service))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                    let _ = stop.wait_for(|stopped| *stopped).await;
                })
                .await
                .map_err(application)
        });
    }

    async fn run_connection(
        self: Arc<Self>,
        peer: Arc<Peer>,
        initial: CarrierBinding,
    ) -> Result<()> {
        let mut initial = Some(initial);
        let mut shutdown = self.stop.subscribe();
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            if self.authorize(&peer.binding).await.is_err() {
                if self.local == peer.binding.primary() {
                    self.quarantine(&peer.binding);
                }
                peer.carrier.shutdown();
                return Ok(());
            }
            let binding = match initial.take() {
                Some(binding) => binding,
                None => match peer.carrier.bind() {
                    Ok((binding, _)) => binding,
                    Err(SendError::Closed) => {
                        if self.local == peer.binding.primary() {
                            self.quarantine(&peer.binding);
                        }
                        return Ok(());
                    }
                    Err(error) => return Err(application(error)),
                },
            };
            let connected = async {
                let address = self.address(&peer.binding, None).await?;
                let endpoint = tonic::transport::Endpoint::from_shared(address)
                    .map_err(application)?
                    .connect_timeout(Duration::from_secs(5));
                let channel = endpoint.connect().await.map_err(application)?;
                let metadata = self.metadata(&peer.binding, false)?;
                let driver =
                    connect_authenticated(binding, channel, metadata, Some(self.authorization()))
                        .await
                        .map_err(|error| application(format!("{error:?}")))?;
                Ok::<_, RuntimeError>(driver.run().await)
            };
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|stopped| *stopped) => return Ok(()),
                result = connected => {
                    tracing::debug!(peer = peer.route.remote_address, ?result, "native carrier disconnected");
                }
            }
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|stopped| *stopped) => return Ok(()),
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }

    fn send_body(
        &self,
        route: &Route,
        body: wire::envelope::Body,
        expires: Option<Duration>,
        expected: Option<&Binding>,
    ) -> bool {
        if *self.stop.borrow() {
            return true;
        }
        let peer = self
            .peers
            .lock()
            .unwrap()
            .get(&route.destination.replica)
            .cloned();
        let Some(peer) = peer.filter(|peer| {
            peer.route.source == route.source
                && peer.route.destination == route.destination
                && peer.route.remote_address == route.remote_address
                && peer.route.sender_address == route.sender_address
                && expected.is_none_or(|binding| &peer.binding == binding)
                && peer.current.load(Ordering::Acquire)
                && peer.fence.check().is_ok()
        }) else {
            // Absence is transient delivery loss. The native sender retains/retries;
            // it is neither a fabricated ACK nor queue-full.
            return true;
        };
        if peer.binding.retained_receiver()
            && !matches!(
                &body,
                wire::envelope::Body::ReplicationOperation(_)
                    | wire::envelope::Body::ReplicationAck(_)
                    | wire::envelope::Body::RequestAck(_)
            )
        {
            return true;
        }
        match peer.carrier.try_send(route.envelope(body), expires) {
            Ok(()) => true,
            Err(SendError::QueueFull) => false,
            Err(SendError::Closed) => {
                if self.local == peer.binding.primary() {
                    self.quarantine(&peer.binding);
                }
                true
            }
            Err(SendError::Transient) => true,
            Err(error) => {
                tracing::error!(%error, "native outbound representation rejected");
                self.failure
                    .lock()
                    .unwrap()
                    .get_or_insert_with(|| error.to_string());
                true
            }
        }
    }

    pub(super) fn replication(&self, intent: SendIntent) -> bool {
        let peer = self
            .peers
            .lock()
            .unwrap()
            .values()
            .find(|peer| {
                peer.route.sender_address == intent.identity.primary_endpoint
                    && peer.route.remote_address == intent.identity.remote_endpoint
                    && peer.incarnation == intent.identity.remote_incarnation
            })
            .cloned();
        let Some(peer) = peer else {
            return true;
        };
        let route = &peer.route;
        let epoch = |epoch: crate::protocol::types::Epoch| wire::Epoch {
            data_loss: epoch.data_loss_number,
            configuration: epoch.configuration_number,
        };
        let body = match intent.kind {
            SendKind::StartCopy {
                replication_start,
                epoch: selected,
            } => wire::envelope::Body::StartCopy(wire::StartCopy {
                epoch: Some(epoch(selected)),
                replica: route.destination.replica,
                replication_start,
            }),
            SendKind::Replication {
                operation,
                original_epoch,
                sending_epoch,
                completed_hint,
            } => {
                let segments = match decode(
                    operation.data().clone(),
                    BufferPolicy::RawSingle,
                    buffer_limits(),
                ) {
                    Ok(segments) => segments,
                    Err(error) => {
                        self.failure
                            .lock()
                            .unwrap()
                            .get_or_insert_with(|| error.to_string());
                        return true;
                    }
                };
                wire::envelope::Body::ReplicationOperation(wire::ReplicationOperation {
                    metadata: Some(metadata(operation.lsn())),
                    original_epoch: Some(epoch(original_epoch)),
                    sending_epoch: Some(epoch(sending_epoch)),
                    first_sequence: operation.lsn(),
                    last_sequence: operation.lsn(),
                    last_in_batch: operation.lsn(),
                    buffer_counts: vec![segments.len() as u32],
                    buffers: Some(wire::Buffers { segments }),
                    completed_hint,
                })
            }
            SendKind::RequestAck => wire::envelope::Body::RequestAck(wire::RequestAck {}),
            SendKind::InduceFault { reason } => {
                wire::envelope::Body::InduceFault(wire::InduceFault {
                    target_replica: route.destination.replica,
                    target_incarnation: route.destination.incarnation.clone(),
                    reason,
                })
            }
        };
        self.send_body(route, body, intent.expires_after, None)
    }

    pub(super) fn copy(
        &self,
        route: &Route,
        action: CopySendAction,
        context: bool,
        epoch: crate::protocol::types::Epoch,
    ) -> bool {
        self.copy_for_binding(route, action, context, epoch, None)
    }

    fn copy_for_binding(
        &self,
        route: &Route,
        action: CopySendAction,
        context: bool,
        epoch: crate::protocol::types::Epoch,
        expected: Option<&Binding>,
    ) -> bool {
        let captured = expected
            .is_none()
            .then(|| {
                self.peers
                    .lock()
                    .unwrap()
                    .get(&route.destination.replica)
                    .map(|peer| peer.binding.clone())
            })
            .flatten();
        let expected = expected.or(captured.as_ref());
        if expected.is_none() {
            return true;
        }
        if expected.is_some_and(Binding::retained_receiver) {
            return true;
        }
        let body = match action {
            CopySendAction::RequestAck => wire::envelope::Body::RequestAck(wire::RequestAck {}),
            CopySendAction::Operation { operation, is_last } => {
                let buffers = Some(wire::Buffers {
                    segments: if is_last {
                        vec![]
                    } else {
                        vec![operation.data().clone()]
                    },
                });
                if context {
                    wire::envelope::Body::CopyContextOperation(wire::CopyContextOperation {
                        metadata: Some(metadata(operation.lsn())),
                        buffers,
                        is_last,
                    })
                } else {
                    wire::envelope::Body::CopyOperation(wire::CopyOperation {
                        metadata: Some(metadata(operation.lsn())),
                        buffers,
                        is_last,
                        epoch: Some(wire::Epoch {
                            data_loss: epoch.data_loss_number,
                            configuration: epoch.configuration_number,
                        }),
                        replica: route.destination.replica,
                    })
                }
            }
        };
        self.send_body(route, body, None, expected)
    }

    pub(super) fn bound_copy(
        &self,
        binding: &Binding,
        route: &Route,
        action: CopySendAction,
        epoch: crate::protocol::types::Epoch,
    ) -> bool {
        self.copy_for_binding(route, action, false, epoch, Some(binding))
    }

    pub(super) fn bound_context_ack(
        &self,
        binding: &Binding,
        route: &Route,
        ack: CopyContextAck,
    ) -> bool {
        self.send_body(
            route,
            wire::envelope::Body::CopyContextAck(wire::CopyContextAck {
                sequence: ack.completed,
                error: ack.error_code,
            }),
            None,
            Some(binding),
        )
    }

    pub(super) fn ack(&self, route: &Route, ack: SecondaryAck) -> bool {
        let binding = {
            let peers = self.peers.lock().unwrap();
            let Some(peer) = peers.get(&route.destination.replica).filter(|peer| {
                self.local == peer.binding.secondary()
                    && peer.ack_generation == ack.transport_generation
                    && peer.ack_owner == Some(ack.transport_owner)
            }) else {
                return true;
            };
            peer.binding.clone()
        };
        self.send_body(
            route,
            wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
                replication_received: ack.replication.received,
                replication_applied: ack.replication.applied,
                copy_received: ack.copy.received,
                copy_applied: ack.copy.applied,
                error: ack.error,
            }),
            None,
            Some(&binding),
        )
    }
}

fn metadata(sequence: i64) -> wire::Metadata {
    wire::Metadata {
        sequence,
        operation_type: 1,
        atomic_group: -1,
    }
}

#[derive(Clone)]
struct Listener(Arc<Transport>);

#[tonic::async_trait]
impl wire::carrier_server::Carrier for Listener {
    type ExchangeStream =
        Pin<Box<dyn Stream<Item = std::result::Result<wire::Envelope, Status>> + Send>>;

    async fn exchange(
        &self,
        request: Request<tonic::Streaming<wire::Envelope>>,
    ) -> std::result::Result<Response<Self::ExchangeStream>, Status> {
        let expected = self.0.authorization();
        if request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            != Some(expected.as_str())
        {
            return Err(Status::unauthenticated(
                "invalid native carrier credentials",
            ));
        }
        let encoded = request
            .metadata()
            .get_bin(BINDING_HEADER)
            .ok_or_else(|| Status::unauthenticated("missing native endpoint binding"))?
            .to_bytes()
            .map_err(|_| Status::invalid_argument("invalid native endpoint binding"))?;
        if encoded.len() > 64 * 1024 {
            return Err(Status::resource_exhausted(
                "native endpoint binding exceeds bound",
            ));
        }
        let binding: Binding = serde_json::from_slice(&encoded)
            .map_err(|_| Status::invalid_argument("invalid native endpoint binding"))?;
        binding
            .validate()
            .map_err(|error| Status::permission_denied(error.to_string()))?;
        if request.metadata().get(CONTROL_HEADER).is_some() {
            let permit = self
                .0
                .controls
                .clone()
                .try_acquire_owned()
                .map_err(|_| Status::resource_exhausted("engine controls are full"))?;
            let mut incoming = request.into_inner();
            let envelope = tokio::time::timeout(Duration::from_secs(5), incoming.message())
                .await
                .map_err(|_| Status::deadline_exceeded("engine control frame"))??
                .ok_or_else(|| Status::invalid_argument("missing engine control"))?;
            let native_route = route(&binding, &self.0.local)
                .map_err(|error| Status::permission_denied(error.to_string()))?;
            native_route
                .check(&envelope, true)
                .map_err(|error| Status::permission_denied(error.to_string()))?;
            if envelope.version != 1 || envelope.message_id.is_empty() {
                return Err(Status::invalid_argument("invalid engine control envelope"));
            }
            let Some(wire::envelope::Body::EngineSessionControl(body)) = envelope.body else {
                return Err(Status::permission_denied("data on control-only exchange"));
            };
            if body.canonical_json.len() > MAX_CONTROL {
                return Err(Status::resource_exhausted("engine control exceeds bound"));
            }
            let control: ControlEnvelope = serde_json::from_slice(&body.canonical_json)
                .map_err(|_| Status::invalid_argument("invalid engine control"))?;
            if control.binding != binding
                || tokio::time::timeout(Duration::from_secs(5), incoming.message())
                    .await
                    .map_err(|_| Status::deadline_exceeded("engine control termination"))??
                    .is_some()
            {
                return Err(Status::permission_denied("crossed engine control"));
            }
            // Completion observers can wait for real copy/provider work. Keep
            // one existing control slot available for its activation and ACKs.
            if control.command == Control::Recovered && self.0.controls.available_permits() == 0 {
                return Err(Status::resource_exhausted(
                    "recovery observation must leave progress capacity",
                ));
            }
            let (result, mut completed) = watch::channel(None);
            let owner = self.0.clone();
            self.0.spawn(async move {
                let _permit = permit;
                let outcome = AssertUnwindSafe(owner.receive_control(&control))
                    .catch_unwind()
                    .await
                    .map_err(|_| Status::permission_denied("engine control owner panicked"))
                    .and_then(|result| {
                        result.map_err(|error| match error {
                            RuntimeError::QueueFull => {
                                Status::resource_exhausted(error.to_string())
                            }
                            RuntimeError::Closed => Status::unavailable(error.to_string()),
                            error => Status::permission_denied(error.to_string()),
                        })
                    });
                #[cfg(feature = "testing")]
                if outcome.is_ok()
                    && control.command == Control::Retire
                    && owner
                        .test_cut(
                            crate::replicator::TestingTcpRouteKind::RetireResponse,
                            &control.binding,
                        )
                        .await
                        .is_err()
                {
                    return Ok(());
                }
                #[cfg(feature = "testing")]
                if outcome.is_ok()
                    && control.command == Control::Reserve
                    && owner
                        .test_cut(
                            crate::replicator::TestingTcpRouteKind::ReserveResponse,
                            &control.binding,
                        )
                        .await
                        .is_err()
                {
                    // Only this RPC observer loses its response. The actual
                    // memoized operation and Peer are already retained.
                    return Ok(());
                }
                result.send_replace(Some(outcome));
                Ok(())
            });
            loop {
                if let Some(result) = completed.borrow().clone() {
                    result?;
                    break;
                }
                completed
                    .changed()
                    .await
                    .map_err(|_| Status::unavailable("engine control owner lost"))?;
            }
            let envelope = native_route.envelope(wire::envelope::Body::EngineSessionControl(body));
            let mut response = Response::new(
                Box::pin(futures::stream::iter([Ok(envelope)])) as Self::ExchangeStream
            );
            response.metadata_mut().insert(
                "authorization",
                expected
                    .parse()
                    .map_err(|_| Status::internal("engine credentials"))?,
            );
            return Ok(response);
        }
        if binding.secondary() != self.0.local {
            return Err(Status::permission_denied(
                "native connection targets another process",
            ));
        }
        let peer = self
            .0
            .peer(binding, false)
            .await
            .map_err(|error| Status::permission_denied(error.to_string()))?;
        let (binding, _) = peer
            .carrier
            .bind()
            .map_err(|error| Status::unavailable(error.to_string()))?;
        let mut response = ExchangeService::new(binding).exchange(request).await?;
        response.metadata_mut().insert(
            "authorization",
            expected
                .parse()
                .map_err(|_| Status::internal("engine credentials"))?,
        );
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::Epoch;

    fn wire_epoch(epoch: Epoch) -> Option<wire::Epoch> {
        Some(wire::Epoch {
            data_loss: epoch.data_loss_number,
            configuration: epoch.configuration_number,
        })
    }

    #[test]
    fn frame_epochs_require_admitted_sending_scope_but_preserve_historical_originals() {
        let admitted = Epoch::new(2, 3);
        for (sending, original, accepted) in [
            (admitted, admitted, true),
            (admitted, Epoch::new(2, 1), true),
            (admitted, Epoch::new(1, 99), true),
            (Epoch::new(2, 4), admitted, false),
            (Epoch::new(3, 1), admitted, false),
            (Epoch::new(2, 2), Epoch::new(2, 1), false),
            (admitted, Epoch::new(2, 4), false),
            (admitted, Epoch::new(3, 1), false),
            (admitted, Epoch::new(-1, 1), false),
        ] {
            let envelope = wire::Envelope {
                body: Some(wire::envelope::Body::ReplicationOperation(
                    wire::ReplicationOperation {
                        original_epoch: wire_epoch(original),
                        sending_epoch: wire_epoch(sending),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            };
            assert_eq!(
                validate_message_epochs(admitted, &envelope).is_ok(),
                accepted
            );
        }
    }

    #[test]
    fn copy_frames_cannot_advance_or_reuse_a_different_epoch() {
        let admitted = Epoch::new(2, 3);
        for epoch in [
            None,
            Some(Epoch::new(2, 2)),
            Some(admitted),
            Some(Epoch::new(3, 1)),
        ] {
            let expected = epoch == Some(admitted);
            for body in [
                wire::envelope::Body::StartCopy(wire::StartCopy {
                    epoch: epoch.and_then(wire_epoch),
                    ..Default::default()
                }),
                wire::envelope::Body::CopyOperation(wire::CopyOperation {
                    epoch: epoch.and_then(wire_epoch),
                    ..Default::default()
                }),
            ] {
                let envelope = wire::Envelope {
                    body: Some(body),
                    ..Default::default()
                };
                assert_eq!(
                    validate_message_epochs(admitted, &envelope).is_ok(),
                    expected
                );
            }
        }
    }
}
