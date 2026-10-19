// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Selected logical fields/routing and send-result behavior translated from
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d,
// Reliability/Replication/ReplicationTransport.cpp:168-355,443-520,684-945,
// ReplicationEndpointId.h:23-80 and RemoteSession.cpp:757-790.
// Tokio/tonic queues and stream generations are Rust representation/lifetime
// substitutions, not native wire interoperability or a receiver state machine.

//! A private, unselected, bounded logical carrier. The owner supplies BOTH full
//! endpoint identities; reconnect changes only a local stream generation.
//! Receiving an envelope is a demux handoff, never an application ACK or role
//! decision. No tasks are spawned: the caller owns the client driver/server.

#![allow(dead_code, reason = "private, unselected V1 carrier")]

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::task::AtomicWaker;
use futures::{Stream, future::poll_fn};
use prost::Message;
use thiserror::Error;
use tokio::sync::watch;
use tokio::time::Instant;
use tonic::{Request, Response, Status};

use super::buffers::{self, BufferLimits, BufferPolicy};
use super::session::{SendIntent, SendKind, SessionIdentity};

#[allow(
    unreachable_pub,
    reason = "generated types stay behind the private carrier"
)]
pub(super) mod wire {
    tonic::include_proto!("kuberic.sf.v1");
}

const VERSION: u32 = 1;

#[derive(Clone, Debug)]
pub(super) struct Route {
    pub(super) source: wire::Endpoint,
    pub(super) destination: wire::Endpoint,
    pub(super) sender_address: String,
    pub(super) remote_address: String,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(super) enum ProtocolError {
    #[error("unsupported carrier version {0}")]
    UnsupportedVersion(u32),
    #[error("missing protobuf {0}")]
    Missing(&'static str),
    #[error("invalid protobuf shape: {0}")]
    Shape(&'static str),
    #[error("wrong source endpoint, including partition/replica/incarnation")]
    WrongSource,
    #[error("wrong destination endpoint, including partition/replica/incarnation")]
    WrongDestination,
    #[error("wrong sender address")]
    WrongSender,
    #[error("message exceeds a logical or wire bound")]
    TooLarge,
    #[error("stream has been superseded")]
    StaleStream,
}

pub(super) fn validate_endpoint(endpoint: &wire::Endpoint) -> Result<(), ProtocolError> {
    if endpoint.partition.len() != 16 || endpoint.incarnation.len() != 16 {
        return Err(ProtocolError::Shape("endpoint GUID length"));
    }
    Ok(())
}

impl Route {
    pub(super) fn incoming(&self, envelope: &wire::Envelope) -> Result<Self, ProtocolError> {
        let source = envelope
            .source
            .as_ref()
            .ok_or(ProtocolError::Missing("source"))?;
        let destination = envelope
            .destination
            .as_ref()
            .ok_or(ProtocolError::Missing("destination"))?;
        validate_endpoint(source)?;
        validate_endpoint(destination)?;
        if destination != &self.source {
            return Err(ProtocolError::WrongDestination);
        }
        if source.partition != self.source.partition {
            return Err(ProtocolError::WrongSource);
        }
        if envelope.sender_address.is_empty() {
            return Err(ProtocolError::Missing("sender address"));
        }
        Ok(Self {
            source: self.source.clone(),
            destination: source.clone(),
            sender_address: self.sender_address.clone(),
            remote_address: envelope.sender_address.clone(),
        })
    }

    pub(super) fn validate(&self) -> Result<(), ProtocolError> {
        validate_endpoint(&self.source)?;
        validate_endpoint(&self.destination)?;
        if self.sender_address.is_empty() || self.remote_address.is_empty() {
            return Err(ProtocolError::Shape("empty sender/remote address"));
        }
        Ok(())
    }

    pub(super) fn envelope(&self, body: wire::envelope::Body) -> wire::Envelope {
        wire::Envelope {
            version: VERSION,
            source: Some(self.source.clone()),
            destination: Some(self.destination.clone()),
            sender_address: self.sender_address.clone(),
            message_id: uuid::Uuid::new_v4().to_string(),
            body: Some(body),
        }
    }

    pub(super) fn reversed(&self) -> Self {
        Self {
            source: self.destination.clone(),
            destination: self.source.clone(),
            sender_address: self.remote_address.clone(),
            remote_address: self.sender_address.clone(),
        }
    }

    pub(super) fn check(
        &self,
        envelope: &wire::Envelope,
        inbound: bool,
    ) -> Result<(), ProtocolError> {
        let source = envelope
            .source
            .as_ref()
            .ok_or(ProtocolError::Missing("source"))?;
        let destination = envelope
            .destination
            .as_ref()
            .ok_or(ProtocolError::Missing("destination"))?;
        validate_endpoint(source)?;
        validate_endpoint(destination)?;
        let (expected_source, expected_destination, expected_sender) = if inbound {
            (&self.destination, &self.source, &self.remote_address)
        } else {
            (&self.source, &self.destination, &self.sender_address)
        };
        if source != expected_source {
            return Err(ProtocolError::WrongSource);
        }
        if destination != expected_destination {
            return Err(ProtocolError::WrongDestination);
        }
        if &envelope.sender_address != expected_sender {
            return Err(ProtocolError::WrongSender);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct CarrierLimits {
    pub(super) outbound_count: usize,
    pub(super) outbound_bytes: usize,
    /// Queued owner deliveries. A receive pump also has at most one staged
    /// decoded message, independently bounded by max_wire_bytes.
    pub(super) inbound_count: usize,
    /// Encoded bytes retained in that delivery queue, not protobuf object size.
    pub(super) inbound_bytes: usize,
    pub(super) max_wire_bytes: usize,
    pub(super) buffers: BufferLimits,
}

fn validate_body(envelope: &wire::Envelope, limits: CarrierLimits) -> Result<(), ProtocolError> {
    if envelope.encoded_len() > limits.max_wire_bytes {
        return Err(ProtocolError::TooLarge);
    }
    validate_shape(envelope, limits.buffers)
}

pub(super) fn validate_shape(
    envelope: &wire::Envelope,
    limits: BufferLimits,
) -> Result<(), ProtocolError> {
    use wire::envelope::Body;
    if envelope.version != VERSION {
        return Err(ProtocolError::UnsupportedVersion(envelope.version));
    }
    if envelope.message_id.is_empty() {
        return Err(ProtocolError::Missing("message_id"));
    }
    let check_buffers = |buffers: &Option<wire::Buffers>| {
        let buffers = buffers.as_ref().ok_or(ProtocolError::Missing("buffers"))?;
        super::buffers::payload_len(&buffers.segments, limits)
            .map_err(|_| ProtocolError::TooLarge)?;
        Ok::<_, ProtocolError>(buffers.segments.len())
    };
    match envelope
        .body
        .as_ref()
        .ok_or(ProtocolError::Missing("body"))?
    {
        Body::StartCopy(body) => {
            body.epoch
                .as_ref()
                .ok_or(ProtocolError::Missing("start-copy epoch"))?;
        }
        Body::ReplicationOperation(body) => {
            let metadata = body
                .metadata
                .as_ref()
                .ok_or(ProtocolError::Missing("operation metadata"))?;
            body.original_epoch
                .as_ref()
                .ok_or(ProtocolError::Missing("original epoch"))?;
            body.sending_epoch
                .as_ref()
                .ok_or(ProtocolError::Missing("sending epoch"))?;
            let count = check_buffers(&body.buffers)?;
            let operations = body
                .last_sequence
                .checked_sub(body.first_sequence)
                .and_then(|span| span.checked_add(1))
                .and_then(|span| usize::try_from(span).ok())
                .ok_or(ProtocolError::Shape("batch sequence range"))?;
            if operations == 0
                || operations != body.buffer_counts.len()
                || metadata.sequence != body.first_sequence
                || body.last_in_batch < body.last_sequence
                || body
                    .buffer_counts
                    .iter()
                    .try_fold(0usize, |total, count| total.checked_add(*count as usize))
                    != Some(count)
            {
                return Err(ProtocolError::Shape("batch buffer counts/range"));
            }
        }
        Body::CopyOperation(body) => {
            body.metadata
                .as_ref()
                .ok_or(ProtocolError::Missing("copy metadata"))?;
            body.epoch
                .as_ref()
                .ok_or(ProtocolError::Missing("copy epoch"))?;
            check_buffers(&body.buffers)?;
        }
        Body::CopyContextOperation(body) => {
            body.metadata
                .as_ref()
                .ok_or(ProtocolError::Missing("copy-context metadata"))?;
            check_buffers(&body.buffers)?;
        }
        Body::InduceFault(body) if body.target_incarnation.len() != 16 => {
            return Err(ProtocolError::Shape("fault target GUID length"));
        }
        Body::ReplicationAck(_)
        | Body::CopyContextAck(_)
        | Body::RequestAck(_)
        | Body::InduceFault(_) => {}
    }
    Ok(())
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(super) enum SendError {
    #[error("bounded send queue is full")]
    QueueFull,
    #[error("message too large")]
    TooLarge,
    #[error("carrier is closed")]
    Closed,
    #[error("no active stream; reconnect belongs to the owner")]
    Transient,
    #[error("invalid message: {0}")]
    Invalid(ProtocolError),
    #[error("send intent does not match the explicitly bound logical session")]
    WrongSession,
    #[error("operation representation: {0}")]
    Representation(buffers::BufferError),
    #[error("operation representation differs from the explicitly selected adapter policy")]
    WrongRepresentation,
    #[error("message expiration is not representable")]
    InvalidExpiration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Terminal {
    RemoteClosed,
    Rpc { code: tonic::Code, message: String },
    Superseded,
    Shutdown,
    OwnerDropped,
    StreamDropped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum StreamStatus {
    Running,
    Terminated(Terminal),
}

/// One retained terminal result per explicitly owned stream; watch capacity is
/// one and there are no intermediate status updates to overwrite.
pub(super) struct Monitor(watch::Receiver<StreamStatus>);

impl Monitor {
    pub(super) fn status(&self) -> StreamStatus {
        self.0.borrow().clone()
    }

    pub(super) async fn terminated(&mut self) -> Terminal {
        loop {
            if let StreamStatus::Terminated(terminal) = self.status() {
                return terminal;
            }
            // Lease Drop always publishes a terminal before the sender closes.
            self.0
                .changed()
                .await
                .expect("lease must publish a terminal status");
        }
    }
}

#[derive(Debug)]
pub(super) enum Delivery {
    Message {
        generation: u64,
        envelope: Box<wire::Envelope>,
    },
    Rejected {
        generation: u64,
        error: ProtocolError,
    },
    Expired {
        generation: u64,
        message_id: String,
    },
    SupersededSend {
        generation: u64,
        message_id: String,
    },
}

impl Delivery {
    fn size(&self) -> usize {
        match self {
            Self::Message { envelope, .. } => envelope.encoded_len(),
            Self::Expired { message_id, .. } | Self::SupersededSend { message_id, .. } => {
                message_id.len()
            }
            Self::Rejected { .. } => 0,
        }
    }
}

struct Queued {
    envelope: wire::Envelope,
    generation: u64,
    expires: Option<Instant>,
    size: usize,
}

struct State {
    generation: u64,
    active: bool,
    closed: bool,
    owner_alive: bool,
    outgoing: VecDeque<Queued>,
    outgoing_bytes: usize,
    incoming: VecDeque<Delivery>,
    incoming_bytes: usize,
    current: Option<Arc<Lease>>,
}

struct Shared {
    route: Route,
    limits: CarrierLimits,
    state: Mutex<State>,
    inbound_ready: AtomicWaker,
}

#[derive(Clone)]
pub(super) struct Carrier(Arc<Shared>);

pub(super) struct Ingress(Arc<Shared>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ShutdownReport {
    pub(super) unsent_count: usize,
    pub(super) unsent_bytes: usize,
}

impl Carrier {
    pub(super) fn new(
        route: Route,
        limits: CarrierLimits,
    ) -> Result<(Self, Ingress), ProtocolError> {
        route.validate()?;
        if limits.outbound_count == 0
            || limits.outbound_bytes == 0
            || limits.inbound_count == 0
            || limits.inbound_bytes < limits.max_wire_bytes
            || limits.max_wire_bytes == 0
        {
            return Err(ProtocolError::Shape(
                "zero capacity or inbound byte budget below maximum wire message",
            ));
        }
        let shared = Arc::new(Shared {
            route,
            limits,
            state: Mutex::new(State {
                generation: 0,
                active: false,
                closed: false,
                owner_alive: true,
                outgoing: VecDeque::new(),
                outgoing_bytes: 0,
                incoming: VecDeque::new(),
                incoming_bytes: 0,
                current: None,
            }),
            inbound_ready: AtomicWaker::new(),
        });
        Ok((Self(shared.clone()), Ingress(shared)))
    }

    pub(super) fn bind(&self) -> Result<(Binding, Monitor), SendError> {
        let mut state = self.0.state.lock().unwrap();
        if state.closed || !state.owner_alive {
            return Err(SendError::Closed);
        }
        if let Some(previous) = state.current.take() {
            previous.finish(Terminal::Superseded);
        }
        state.generation = state.generation.checked_add(1).ok_or(SendError::Closed)?;
        state.active = true;
        let (status, receiver) = watch::channel(StreamStatus::Running);
        let lease = Arc::new(Lease {
            generation: state.generation,
            status,
            send_wake: AtomicWaker::new(),
            receive_wake: AtomicWaker::new(),
        });
        state.current = Some(lease.clone());
        Ok((
            Binding {
                shared: self.0.clone(),
                lease,
            },
            Monitor(receiver),
        ))
    }

    pub(super) fn try_send(
        &self,
        envelope: wire::Envelope,
        expires_after: Option<Duration>,
    ) -> Result<(), SendError> {
        validate_body(&envelope, self.0.limits).map_err(|error| match error {
            ProtocolError::TooLarge => SendError::TooLarge,
            error => SendError::Invalid(error),
        })?;
        self.0
            .route
            .check(&envelope, false)
            .map_err(SendError::Invalid)?;
        let size = envelope.encoded_len();
        if size > self.0.limits.outbound_bytes {
            return Err(SendError::TooLarge);
        }
        let expires = expires_after
            .map(|duration| {
                Instant::now()
                    .checked_add(duration)
                    .ok_or(SendError::InvalidExpiration)
            })
            .transpose()?;
        let mut state = self.0.state.lock().unwrap();
        if state.closed || !state.owner_alive {
            return Err(SendError::Closed);
        }
        if !state.active {
            return Err(SendError::Transient);
        }
        if state.outgoing.len() >= self.0.limits.outbound_count
            || size > self.0.limits.outbound_bytes - state.outgoing_bytes
        {
            return Err(SendError::QueueFull);
        }
        let generation = state.generation;
        state.outgoing.push_back(Queued {
            envelope,
            generation,
            expires,
            size,
        });
        state.outgoing_bytes += size;
        let lease = state.current.clone();
        drop(state);
        if let Some(lease) = lease {
            lease.send_wake.wake();
        }
        Ok(())
    }

    /// Explicitly stops this carrier; reports queued payloads rather than
    /// claiming a native session drain. Storage lives until handles are dropped.
    pub(super) fn shutdown(&self) -> ShutdownReport {
        let mut state = self.0.state.lock().unwrap();
        state.closed = true;
        state.active = false;
        if let Some(lease) = &state.current {
            lease.finish(Terminal::Shutdown);
        }
        let report = ShutdownReport {
            unsent_count: state.outgoing.len(),
            unsent_bytes: state.outgoing_bytes,
        };
        drop(state);
        self.0.inbound_ready.wake();
        report
    }
}

impl Ingress {
    pub(super) async fn recv(&mut self) -> Option<Delivery> {
        poll_fn(|cx| {
            self.0.inbound_ready.register(cx.waker());
            let mut state = self.0.state.lock().unwrap();
            if let Some(mut delivery) = state.incoming.pop_front() {
                state.incoming_bytes -= delivery.size();
                if let Delivery::Message { generation, .. } = &delivery
                    && *generation != state.generation
                {
                    delivery = Delivery::Rejected {
                        generation: *generation,
                        error: ProtocolError::StaleStream,
                    };
                }
                let lease = state.current.clone();
                drop(state);
                if let Some(lease) = lease {
                    lease.send_wake.wake();
                    lease.receive_wake.wake();
                }
                return Poll::Ready(Some(delivery));
            }
            if state.closed {
                Poll::Ready(None)
            } else {
                Poll::Pending
            }
        })
        .await
    }
}

impl Drop for Ingress {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.owner_alive = false;
        state.active = false;
        if let Some(lease) = &state.current {
            lease.finish(Terminal::OwnerDropped);
        }
    }
}

struct Lease {
    generation: u64,
    status: watch::Sender<StreamStatus>,
    send_wake: AtomicWaker,
    receive_wake: AtomicWaker,
}

impl Lease {
    fn finish(&self, terminal: Terminal) {
        self.status.send_if_modified(|status| {
            if *status != StreamStatus::Running {
                return false;
            }
            *status = StreamStatus::Terminated(terminal);
            true
        });
        self.send_wake.wake();
        self.receive_wake.wake();
    }

    fn terminal(&self) -> Option<Terminal> {
        match &*self.status.borrow() {
            StreamStatus::Running => None,
            StreamStatus::Terminated(terminal) => Some(terminal.clone()),
        }
    }
}

pub(super) struct Binding {
    shared: Arc<Shared>,
    lease: Arc<Lease>,
}

impl Binding {
    fn finish(&self, terminal: Terminal) -> Terminal {
        self.lease.finish(terminal);
        let mut state = self.shared.state.lock().unwrap();
        if state.generation == self.lease.generation {
            state.active = false;
        }
        self.lease.terminal().unwrap()
    }

    fn poll_live(&self, cx: &Context<'_>, sending: bool) -> Option<Terminal> {
        if sending {
            self.lease.send_wake.register(cx.waker());
        } else {
            self.lease.receive_wake.register(cx.waker());
        }
        self.lease.terminal()
    }

    fn poll_deliver(&self, cx: &Context<'_>, pending: &mut Option<Delivery>) -> Poll<()> {
        self.lease.receive_wake.register(cx.waker());
        let Some(mut delivery) = pending.take() else {
            return Poll::Ready(());
        };
        let mut state = self.shared.state.lock().unwrap();
        if let Delivery::Message { generation, .. } = &delivery
            && *generation != state.generation
        {
            delivery = Delivery::Rejected {
                generation: *generation,
                error: ProtocolError::StaleStream,
            };
        }
        let size = delivery.size();
        if state.incoming.len() >= self.shared.limits.inbound_count
            || size > self.shared.limits.inbound_bytes - state.incoming_bytes
        {
            *pending = Some(delivery);
            return Poll::Pending;
        }
        state.incoming.push_back(delivery);
        state.incoming_bytes += size;
        drop(state);
        self.shared.inbound_ready.wake();
        Poll::Ready(())
    }

    fn received(&self, envelope: wire::Envelope) -> Delivery {
        let generation = self.lease.generation;
        match validate_body(&envelope, self.shared.limits)
            .and_then(|()| self.shared.route.check(&envelope, true))
        {
            Ok(()) => Delivery::Message {
                generation,
                envelope: Box::new(envelope),
            },
            Err(error) => Delivery::Rejected { generation, error },
        }
    }
}

impl Drop for Binding {
    fn drop(&mut self) {
        self.finish(Terminal::StreamDropped);
    }
}

/// Pull-based outbound wire stream. Expiration is checked per message at the
/// transport handoff, not installed as a whole-RPC deadline.
struct SendStream {
    binding: Arc<Binding>,
}

impl Stream for SendStream {
    type Item = wire::Envelope;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.binding.poll_live(cx, true).is_some() {
            return Poll::Ready(None);
        }
        loop {
            let mut state = this.binding.shared.state.lock().unwrap();
            // Generation validation and queue selection share the guard.
            if state.generation != this.binding.lease.generation {
                return Poll::Ready(None);
            }
            let Some(queued) = state.outgoing.front() else {
                return Poll::Pending;
            };
            let report = if queued.generation != this.binding.lease.generation {
                Some(Delivery::SupersededSend {
                    generation: queued.generation,
                    message_id: queued.envelope.message_id.clone(),
                })
            } else if queued
                .expires
                .is_some_and(|expires| Instant::now() >= expires)
            {
                Some(Delivery::Expired {
                    generation: queued.generation,
                    message_id: queued.envelope.message_id.clone(),
                })
            } else {
                None
            };
            if let Some(report) = report {
                let size = report.size();
                if state.incoming.len() >= this.binding.shared.limits.inbound_count
                    || size > this.binding.shared.limits.inbound_bytes - state.incoming_bytes
                {
                    // Keep the expired payload admitted until its bounded
                    // notification can be retained, rather than losing status.
                    return Poll::Pending;
                }
                let queued = state.outgoing.pop_front().unwrap();
                state.outgoing_bytes -= queued.size;
                state.incoming_bytes += size;
                state.incoming.push_back(report);
                drop(state);
                this.binding.shared.inbound_ready.wake();
            } else {
                let queued = state.outgoing.pop_front().unwrap();
                state.outgoing_bytes -= queued.size;
                return Poll::Ready(Some(queued.envelope));
            }
        }
    }
}

type Incoming = Pin<Box<dyn Stream<Item = Result<wire::Envelope, Status>> + Send>>;

struct ReceivePump {
    binding: Arc<Binding>,
    incoming: Incoming,
    pending: Option<Delivery>,
}

impl ReceivePump {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Terminal> {
        if let Some(terminal) = self.binding.poll_live(cx, false) {
            return Poll::Ready(terminal);
        }
        loop {
            if self
                .binding
                .poll_deliver(cx, &mut self.pending)
                .is_pending()
            {
                return Poll::Pending;
            }
            match self.incoming.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    return Poll::Ready(self.binding.finish(Terminal::RemoteClosed));
                }
                Poll::Ready(Some(Err(status))) => {
                    return Poll::Ready(self.binding.finish(rpc_terminal(status)));
                }
                Poll::Ready(Some(Ok(envelope))) => {
                    self.pending = Some(self.binding.received(envelope));
                }
            }
        }
    }
}

impl Drop for ReceivePump {
    fn drop(&mut self) {
        self.binding.finish(Terminal::StreamDropped);
    }
}

fn rpc_terminal(status: Status) -> Terminal {
    Terminal::Rpc {
        code: status.code(),
        message: status.message().chars().take(256).collect(),
    }
}

pub(super) struct ClientDriver(ReceivePump);

impl ClientDriver {
    /// Run under an explicitly owned join, or await directly. Dropping the
    /// driver records StreamDropped, not successful session shutdown/drain.
    pub(super) async fn run(mut self) -> Terminal {
        poll_fn(|cx| self.0.poll(cx)).await
    }
}

pub(super) async fn connect(
    binding: Binding,
    channel: tonic::transport::Channel,
) -> Result<ClientDriver, Terminal> {
    let binding = Arc::new(binding);
    let limit = binding.shared.limits.max_wire_bytes;
    let mut client = wire::carrier_client::CarrierClient::new(channel)
        .max_decoding_message_size(limit)
        .max_encoding_message_size(limit);
    let request = SendStream {
        binding: binding.clone(),
    };
    let mut driver = ClientDriver(ReceivePump {
        binding: binding.clone(),
        incoming: Box::pin(futures::stream::pending()),
        pending: None,
    });
    match client.exchange(Request::new(request)).await {
        Ok(response) => {
            driver.0.incoming = Box::pin(response.into_inner());
            Ok(driver)
        }
        Err(status) => Err(binding.finish(rpc_terminal(status))),
    }
}

/// One explicitly prepared stream binding, consumed once. A future listener/
/// demux owner supplies each new binding rather than letting the carrier invent
/// sessions. Construct a fresh service/binding for an owner-approved reconnect.
#[derive(Clone)]
pub(super) struct ExchangeService(Arc<Mutex<Option<Binding>>>);

impl ExchangeService {
    pub(super) fn new(binding: Binding) -> Self {
        Self(Arc::new(Mutex::new(Some(binding))))
    }

    /// At most one next binding is staged. An unused previous binding is
    /// returned to its owner, never accumulated in a reconnect queue.
    pub(super) fn replace_binding(&self, binding: Binding) -> Option<Binding> {
        self.0.lock().unwrap().replace(binding)
    }

    pub(super) fn into_service(
        self,
        max_wire_bytes: usize,
    ) -> wire::carrier_server::CarrierServer<Self> {
        wire::carrier_server::CarrierServer::new(self)
            .max_decoding_message_size(max_wire_bytes)
            .max_encoding_message_size(max_wire_bytes)
    }
}

struct ExchangeStream {
    send: SendStream,
    receive: ReceivePump,
}

impl Stream for ExchangeStream {
    type Item = Result<wire::Envelope, Status>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.receive.poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Pin::new(&mut this.send)
            .poll_next(cx)
            .map(|item| item.map(Ok))
    }
}

#[tonic::async_trait]
impl wire::carrier_server::Carrier for ExchangeService {
    type ExchangeStream = Pin<Box<dyn Stream<Item = Result<wire::Envelope, Status>> + Send>>;

    async fn exchange(
        &self,
        request: Request<tonic::Streaming<wire::Envelope>>,
    ) -> Result<Response<Self::ExchangeStream>, Status> {
        let binding = self.0.lock().unwrap().take().ok_or_else(|| {
            Status::failed_precondition("owner must supply a fresh stream binding")
        })?;
        let binding = Arc::new(binding);
        Ok(Response::new(Box::pin(ExchangeStream {
            send: SendStream {
                binding: binding.clone(),
            },
            receive: ReceivePump {
                binding,
                incoming: Box::pin(request.into_inner()),
                pending: None,
            },
        })))
    }
}

/// Operation metadata absent from QueueOperation is supplied explicitly by its
/// owner. This adapter never guesses atomic groups or endpoint GUIDs.
#[derive(Clone, Copy)]
pub(super) struct OperationMetadata {
    pub(super) operation_type: u32,
    pub(super) atomic_group: i64,
}

pub(super) struct SendIntentAdapter {
    identity: SessionIdentity,
    route: Route,
    policy: BufferPolicy,
    metadata: OperationMetadata,
    carrier: Carrier,
    report_error: Arc<dyn Fn(SendError) + Send + Sync>,
}

impl SendIntentAdapter {
    pub(super) fn new(
        identity: SessionIdentity,
        route: Route,
        policy: BufferPolicy,
        metadata: OperationMetadata,
        carrier: Carrier,
        report_error: Arc<dyn Fn(SendError) + Send + Sync>,
    ) -> Result<Self, ProtocolError> {
        route.validate()?;
        // Route ownership is supplied separately from the legacy logical ID;
        // comparing GUIDs to its integer incarnation would invent identity.
        if route.source != carrier.0.route.source
            || route.destination != carrier.0.route.destination
            || route.sender_address != carrier.0.route.sender_address
            || route.remote_address != carrier.0.route.remote_address
        {
            return Err(ProtocolError::Shape("adapter/carrier route mismatch"));
        }
        Ok(Self {
            identity,
            route,
            policy,
            metadata,
            carrier,
            report_error,
        })
    }

    pub(super) fn send(&self, intent: SendIntent) -> bool {
        match self.try_send(intent) {
            Ok(()) => true,
            Err(SendError::QueueFull) => false,
            Err(error) => {
                (self.report_error)(error);
                true
            }
        }
    }

    fn try_send(&self, intent: SendIntent) -> Result<(), SendError> {
        if intent.identity != self.identity {
            return Err(SendError::WrongSession);
        }
        let body = match intent.kind {
            SendKind::StartCopy {
                replication_start,
                epoch,
            } => wire::envelope::Body::StartCopy(wire::StartCopy {
                epoch: Some(wire::Epoch {
                    data_loss: epoch.data_loss_number,
                    configuration: epoch.configuration_number,
                }),
                replica: self.route.destination.replica,
                replication_start,
            }),
            SendKind::RequestAck => wire::envelope::Body::RequestAck(wire::RequestAck {}),
            SendKind::InduceFault { reason } => {
                wire::envelope::Body::InduceFault(wire::InduceFault {
                    target_replica: self.route.destination.replica,
                    target_incarnation: self.route.destination.incarnation.clone(),
                    reason,
                })
            }
            SendKind::Replication {
                operation,
                original_epoch,
                sending_epoch,
                completed_hint,
            } => {
                if operation.representation() != self.policy {
                    return Err(SendError::WrongRepresentation);
                }
                let segments = buffers::decode(
                    operation.data().clone(),
                    self.policy,
                    self.carrier.0.limits.buffers,
                )
                .map_err(|error| match error {
                    buffers::BufferError::TooLarge | buffers::BufferError::TooManySegments => {
                        SendError::TooLarge
                    }
                    error => SendError::Representation(error),
                })?;
                wire::envelope::Body::ReplicationOperation(wire::ReplicationOperation {
                    metadata: Some(wire::Metadata {
                        sequence: operation.lsn(),
                        operation_type: self.metadata.operation_type,
                        atomic_group: self.metadata.atomic_group,
                    }),
                    original_epoch: Some(wire::Epoch {
                        data_loss: original_epoch.data_loss_number,
                        configuration: original_epoch.configuration_number,
                    }),
                    sending_epoch: Some(wire::Epoch {
                        data_loss: sending_epoch.data_loss_number,
                        configuration: sending_epoch.configuration_number,
                    }),
                    buffer_counts: vec![segments.len() as u32],
                    buffers: Some(wire::Buffers { segments }),
                    first_sequence: operation.lsn(),
                    last_sequence: operation.lsn(),
                    last_in_batch: operation.lsn(),
                    completed_hint,
                })
            }
        };
        self.carrier
            .try_send(self.route.envelope(body), intent.expires_after)
    }
}

#[cfg(test)]
mod tests;
