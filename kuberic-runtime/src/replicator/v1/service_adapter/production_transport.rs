//! Production carrier ownership. Host authority is rechecked for every native
//! delivery; RPC completion is never a replication or application ACK.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{FutureExt, Stream};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::task::TaskTracker;
use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status};

use crate::host::native_transport::{NativeBinding, NativeEndpoint, partition};
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
    Carrier, CarrierLimits, Delivery, ExchangeService, Ingress, Route, SendError,
    connect_with_metadata, wire,
};

const MAX_PAYLOAD: usize = 52_428_800;
const MAX_WIRE: usize = MAX_PAYLOAD + 64 * 1024;
const BINDING_HEADER: &str = "kuberic-native-binding-bin";

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
    endpoint: &NativeEndpoint,
) -> Result<wire::Endpoint> {
    Ok(wire::Endpoint {
        partition: bytes::Bytes::copy_from_slice(&partition(resource)),
        replica: endpoint.identity.replica_id.value(),
        incarnation: bytes::Bytes::copy_from_slice(&endpoint.incarnation()?),
    })
}

pub(super) fn route(binding: &NativeBinding, local: &NativeEndpoint) -> Result<Route> {
    let primary = Route {
        source: endpoint(&binding.history.resource_uid, &binding.primary)?,
        destination: endpoint(&binding.history.resource_uid, &binding.secondary)?,
        sender_address: binding.primary.address.clone(),
        remote_address: binding.secondary.address.clone(),
    };
    if local == &binding.primary {
        Ok(primary)
    } else if local == &binding.secondary {
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
    binding: NativeBinding,
    route: Route,
    carrier: Carrier,
    connecting: AtomicBool,
    incarnation: i64,
    current: Arc<AtomicBool>,
}

pub(super) struct Transport {
    context: ReplicatorFactoryContext,
    credentials: Arc<str>,
    local: NativeEndpoint,
    registry: MessageRegistry,
    peers: Mutex<BTreeMap<i64, Arc<Peer>>>,
    stop: watch::Sender<bool>,
    tasks: TaskTracker,
    failure: Arc<Mutex<Option<String>>>,
    next_incarnation: AtomicI64,
}

impl Transport {
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
        let (_, local) = context.native_local_endpoint(advertised).await?;
        let owner = Arc::new(Self {
            context,
            credentials,
            local,
            registry,
            peers: Mutex::new(BTreeMap::new()),
            stop: watch::channel(false).0,
            tasks: TaskTracker::new(),
            failure: Arc::new(Mutex::new(None)),
            next_incarnation: AtomicI64::new(1),
        });
        let service = Listener(owner.clone());
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
        let context = self.context.clone();
        self.tasks.spawn(async move {
            let result = AssertUnwindSafe(future).catch_unwind().await;
            let error = match result {
                Ok(Ok(())) => return,
                Ok(Err(error)) => error.to_string(),
                Err(_) => "native transport task panicked".into(),
            };
            tracing::error!(%error, "native transport owner failed");
            failure.lock().unwrap().get_or_insert(error);
            if let Err(report) = context
                .report_fault(crate::protocol::types::FaultType::Transient)
                .await
            {
                tracing::warn!(%report, "native transport fault could not be reported");
            }
        });
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

    pub(super) fn local(&self) -> &NativeEndpoint {
        &self.local
    }

    pub(super) fn abort(&self) {
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

    pub(super) async fn admit(
        self: &Arc<Self>,
        binding: NativeBinding,
        outgoing: bool,
    ) -> Result<i64> {
        self.peer(binding, outgoing)
            .await
            .map(|peer| peer.incarnation)
    }

    async fn peer(self: &Arc<Self>, binding: NativeBinding, outgoing: bool) -> Result<Arc<Peer>> {
        if *self.stop.borrow() {
            return Err(RuntimeError::Closed);
        }
        self.context.validate_native_binding(&binding).await?;
        let route = route(&binding, &self.local)?;
        let id = route.destination.replica;
        let peer = {
            let mut peers = self.peers.lock().unwrap();
            if let Some(peer) = peers.get(&id).filter(|peer| peer.binding == binding) {
                peer.clone()
            } else {
                let previous = peers.remove(&id);
                let reuse_incarnation = previous
                    .as_ref()
                    .filter(|previous| {
                        previous.binding.history == binding.history
                            && previous.binding.primary == binding.primary
                            && previous.binding.secondary == binding.secondary
                    })
                    .map(|previous| previous.incarnation);
                if let Some(previous) = previous {
                    previous.current.store(false, Ordering::Release);
                    previous.carrier.shutdown();
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
                    route,
                    carrier,
                    connecting: AtomicBool::new(false),
                    incarnation,
                    current: current.clone(),
                });
                self.spawn(self.clone().run_ingress(binding, current, ingress));
                peers.insert(id, peer.clone());
                peer
            }
        };
        if outgoing && !peer.connecting.swap(true, Ordering::AcqRel) {
            let owner = self.clone();
            let connection = peer.clone();
            self.spawn(async move { owner.run_connection(connection).await });
        }
        Ok(peer)
    }

    pub(super) async fn run_ingress(
        self: Arc<Self>,
        binding: NativeBinding,
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
                    // Validate the current journal, not an old successful handshake.
                    if let Err(error) = self.context.validate_native_binding(&binding).await {
                        tracing::debug!(%error, "retired native delivery rejected");
                        continue;
                    }
                    if !current.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    if let Err(error) = validate_message_epochs(binding.epoch, &envelope) {
                        tracing::warn!(%error, "unauthorized native frame epoch rejected");
                        continue;
                    }
                    match self.registry.dispatch(&envelope) {
                        Err(IngressError::Primary(PrimaryError::UnknownSession(replica)))
                            if matches!(
                                envelope.body,
                                Some(wire::envelope::Body::ReplicationAck(_))
                            ) =>
                        {
                            tracing::debug!(replica, "retired native session ACK dropped");
                        }
                        result => {
                            result.map_err(application)?;
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

    #[cfg(test)]
    pub(super) fn test_owner(
        context: ReplicatorFactoryContext,
        local: NativeEndpoint,
        registry: MessageRegistry,
    ) -> Arc<Self> {
        Arc::new(Self {
            context,
            local,
            registry,
            credentials: Arc::from("test-token"),
            peers: Mutex::new(BTreeMap::new()),
            stop: watch::channel(false).0,
            tasks: TaskTracker::new(),
            failure: Arc::new(Mutex::new(None)),
            next_incarnation: AtomicI64::new(1),
        })
    }

    async fn run_connection(self: Arc<Self>, peer: Arc<Peer>) -> Result<()> {
        let mut shutdown = self.stop.subscribe();
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            if self
                .context
                .validate_native_binding(&peer.binding)
                .await
                .is_err()
            {
                peer.carrier.shutdown();
                return Ok(());
            }
            let (binding, _) = match peer.carrier.bind() {
                Ok(binding) => binding,
                Err(SendError::Closed) => return Ok(()),
                Err(error) => return Err(application(error)),
            };
            let connected = async {
                let address = peer.route.remote_address.clone();
                #[cfg(feature = "testing")]
                let address = crate::replicator::testing_transport::route(
                    if peer.binding.build.is_some() {
                        crate::replicator::TestingTcpRouteKind::Build
                    } else {
                        crate::replicator::TestingTcpRouteKind::Replication
                    },
                    self.local.identity.instance_id.as_str(),
                    peer.binding.secondary.identity.instance_id.as_str(),
                    address,
                )
                .await
                .map_err(application)?;
                let endpoint = tonic::transport::Endpoint::from_shared(address)
                    .map_err(application)?
                    .connect_timeout(Duration::from_secs(5));
                let channel = endpoint.connect().await.map_err(application)?;
                let mut metadata = MetadataMap::new();
                metadata.insert(
                    "authorization",
                    format!("Bearer {}", self.credentials)
                        .parse()
                        .map_err(application)?,
                );
                let encoded = serde_json::to_vec(&peer.binding).map_err(application)?;
                metadata.insert_bin(
                    BINDING_HEADER,
                    tonic::metadata::MetadataValue::from_bytes(&encoded),
                );
                let driver = connect_with_metadata(binding, channel, metadata)
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
        expected: Option<&NativeBinding>,
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
        }) else {
            // Absence is transient delivery loss. The native sender retains/retries;
            // it is neither a fabricated ACK nor queue-full.
            return true;
        };
        match peer.carrier.try_send(route.envelope(body), expires) {
            Ok(()) => true,
            Err(SendError::QueueFull) => false,
            Err(SendError::Closed | SendError::Transient) => true,
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
        expected: Option<&NativeBinding>,
    ) -> bool {
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
        binding: &NativeBinding,
        route: &Route,
        action: CopySendAction,
        epoch: crate::protocol::types::Epoch,
    ) -> bool {
        self.copy_for_binding(route, action, false, epoch, Some(binding))
    }

    pub(super) fn bound_context_ack(
        &self,
        binding: &NativeBinding,
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
            None,
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
        let expected = format!("Bearer {}", self.0.credentials);
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
        let binding: NativeBinding = serde_json::from_slice(&encoded)
            .map_err(|_| Status::invalid_argument("invalid native endpoint binding"))?;
        // Only the admitted primary establishes this bidirectional connection.
        if binding.secondary != self.0.local {
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
        ExchangeService::new(binding).exchange(request).await
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
