use bytes::Bytes;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::{FutureExt, StreamExt, stream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{advance, timeout};

use super::*;
use crate::protocol::types::Epoch;
use crate::replicator::v1::async_runtime::{
    AckRuntime, Clock, CopyAckOwner, OwnerError, SessionRuntime,
};
use crate::replicator::v1::operation_queue::QueueOperation;
use crate::replicator::v1::sender::{SenderConfig, SenderOperation};
use crate::replicator::v1::session::{AckUpdate, ReplicationSession};

fn route() -> Route {
    Route {
        source: wire::Endpoint {
            partition: Bytes::from(vec![1; 16]),
            replica: 10,
            incarnation: Bytes::from(vec![2; 16]),
        },
        destination: wire::Endpoint {
            partition: Bytes::from(vec![1; 16]),
            replica: 20,
            incarnation: Bytes::from(vec![3; 16]),
        },
        sender_address: "primary/actor".into(),
        remote_address: "secondary/actor".into(),
    }
}

fn limits() -> CarrierLimits {
    CarrierLimits {
        outbound_count: 4,
        outbound_bytes: 8192,
        inbound_count: 2,
        inbound_bytes: 8192,
        max_wire_bytes: 4096,
        buffers: BufferLimits {
            max_segments: 8,
            max_payload_bytes: 1024,
        },
    }
}

fn request_ack(route: &Route) -> wire::Envelope {
    route.envelope(wire::envelope::Body::RequestAck(wire::RequestAck {}))
}

fn identity() -> SessionIdentity {
    SessionIdentity {
        primary_endpoint: "logical/primary".into(),
        remote_endpoint: "logical/remote".into(),
        remote_incarnation: 71,
    }
}

fn replication_intent(data: Bytes) -> SendIntent {
    intent_for_operation(QueueOperation::new(1, data, true))
}

fn framed_replication_intent(data: Bytes) -> SendIntent {
    intent_for_operation(
        QueueOperation::from_data(
            1,
            data,
            true,
            BufferPolicy::FramedSegments,
            limits().buffers,
        )
        .unwrap(),
    )
}

fn intent_for_operation(operation: Arc<QueueOperation>) -> SendIntent {
    SendIntent {
        identity: identity(),
        kind: SendKind::Replication {
            operation,
            original_epoch: Epoch::new(1, 9),
            sending_epoch: Epoch::new(2, 3),
            completed_hint: 77,
        },
        expires_after: Some(Duration::from_secs(5)),
    }
}

fn adapter(
    carrier: Carrier,
    policy: BufferPolicy,
    errors: Arc<Mutex<Vec<SendError>>>,
) -> SendIntentAdapter {
    SendIntentAdapter::new(
        identity(),
        route(),
        policy,
        OperationMetadata {
            operation_type: 0x42,
            atomic_group: -7,
        },
        carrier,
        Arc::new(move |error| errors.lock().unwrap().push(error)),
    )
    .unwrap()
}

fn send_stream(binding: Binding) -> SendStream {
    SendStream {
        binding: Arc::new(binding),
    }
}

async fn receive(ingress: &mut Ingress) -> Delivery {
    timeout(Duration::from_secs(3), ingress.recv())
        .await
        .unwrap()
        .unwrap()
}

#[derive(Default)]
struct WakeCounter(AtomicUsize);

impl futures::task::ArcWake for WakeCounter {
    fn wake_by_ref(counter: &Arc<Self>) {
        counter.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn superseded_send_stream_cannot_replace_live_generation_admission_waker() {
    let (carrier, _ingress) = Carrier::new(route(), limits()).unwrap();
    let (old_binding, _old_monitor) = carrier.bind().unwrap();
    let mut old = send_stream(old_binding);
    let (new_binding, _new_monitor) = carrier.bind().unwrap();
    let mut new = send_stream(new_binding);
    let new_counter = Arc::new(WakeCounter::default());
    let new_waker = futures::task::waker(new_counter.clone());
    let mut new_context = Context::from_waker(&new_waker);
    let old_counter = Arc::new(WakeCounter::default());
    let old_waker = futures::task::waker(old_counter.clone());
    let mut old_context = Context::from_waker(&old_waker);
    assert!(Pin::new(&mut new).poll_next(&mut new_context).is_pending());
    assert!(matches!(
        Pin::new(&mut old).poll_next(&mut old_context),
        Poll::Ready(None)
    ));
    let envelope = request_ack(&route());
    carrier.try_send(envelope.clone(), None).unwrap();
    assert_eq!(
        new_counter.0.load(Ordering::SeqCst),
        1,
        "admission must wake the current generation, not the last superseded poller"
    );
    assert_eq!(old_counter.0.load(Ordering::SeqCst), 0);
    assert!(
        matches!(Pin::new(&mut new).poll_next(&mut new_context), Poll::Ready(Some(actual)) if actual == envelope)
    );
}

#[tokio::test]
async fn superseded_send_stream_cannot_replace_live_expiry_ingress_space_waker() {
    let (carrier, mut ingress) = Carrier::new(
        route(),
        CarrierLimits {
            inbound_count: 1,
            ..limits()
        },
    )
    .unwrap();
    let (old_binding, _old_monitor) = carrier.bind().unwrap();
    let mut old = send_stream(old_binding);
    let (new_binding, _new_monitor) = carrier.bind().unwrap();
    let root = Arc::new(new_binding);
    let mut occupied = Some(root.received(request_ack(&route().reversed())));
    poll_fn(|cx| root.poll_deliver(cx, &mut occupied)).await;
    let mut new = SendStream { binding: root };
    let expired = request_ack(&route());
    carrier
        .try_send(expired.clone(), Some(Duration::ZERO))
        .unwrap();
    let next = request_ack(&route());
    carrier.try_send(next.clone(), None).unwrap();
    let new_counter = Arc::new(WakeCounter::default());
    let new_waker = futures::task::waker(new_counter.clone());
    let mut new_context = Context::from_waker(&new_waker);
    let old_counter = Arc::new(WakeCounter::default());
    let old_waker = futures::task::waker(old_counter.clone());
    let mut old_context = Context::from_waker(&old_waker);
    assert!(Pin::new(&mut new).poll_next(&mut new_context).is_pending());
    assert!(matches!(
        Pin::new(&mut old).poll_next(&mut old_context),
        Poll::Ready(None)
    ));
    assert!(matches!(
        receive(&mut ingress).await,
        Delivery::Message { .. }
    ));
    assert_eq!(
        new_counter.0.load(Ordering::SeqCst),
        1,
        "free ingress space must wake the current expiry reporter"
    );
    assert_eq!(old_counter.0.load(Ordering::SeqCst), 0);
    assert!(
        matches!(Pin::new(&mut new).poll_next(&mut new_context), Poll::Ready(Some(actual)) if actual == next)
    );
    assert!(
        matches!(receive(&mut ingress).await, Delivery::Expired { message_id, .. } if message_id == expired.message_id)
    );
}

#[tokio::test]
async fn in_flight_superseded_receive_cannot_replace_live_ingress_space_waker() {
    let (carrier, mut ingress) = Carrier::new(
        route(),
        CarrierLimits {
            inbound_count: 1,
            ..limits()
        },
    )
    .unwrap();
    let (old_binding, _old_monitor) = carrier.bind().unwrap();
    let old = Arc::new(old_binding);
    let old_counter = Arc::new(WakeCounter::default());
    let old_waker = futures::task::waker(old_counter.clone());
    let old_context = Context::from_waker(&old_waker);
    assert_eq!(old.poll_live(&old_context, false), None);
    let (new_binding, _new_monitor) = carrier.bind().unwrap();
    old_counter.0.store(0, Ordering::SeqCst);
    let root = Arc::new(new_binding);
    let mut occupied = Some(root.received(request_ack(&route().reversed())));
    poll_fn(|cx| root.poll_deliver(cx, &mut occupied)).await;
    let expected = request_ack(&route().reversed());
    let mut new = ReceivePump {
        pending: Some(root.received(expected.clone())),
        binding: root,
        incoming: Box::pin(stream::pending()),
    };
    let new_counter = Arc::new(WakeCounter::default());
    let new_waker = futures::task::waker(new_counter.clone());
    let mut new_context = Context::from_waker(&new_waker);
    assert!(new.poll(&mut new_context).is_pending());
    // Resume the old pump's delivery phase after its pre-reconnect liveness
    // check, deterministically exercising the same interleaving as a late poll.
    let mut stale = Some(old.received(request_ack(&route().reversed())));
    assert!(old.poll_deliver(&old_context, &mut stale).is_pending());
    assert!(matches!(
        receive(&mut ingress).await,
        Delivery::Message { .. }
    ));
    assert_eq!(
        new_counter.0.load(Ordering::SeqCst),
        1,
        "free ingress space must wake the current receive pump"
    );
    assert_eq!(old_counter.0.load(Ordering::SeqCst), 0);
    assert!(new.poll(&mut new_context).is_pending());
    assert!(
        new.pending.is_none(),
        "the awakened live pump must finish its queued delivery"
    );
    assert!(
        matches!(receive(&mut ingress).await, Delivery::Message { generation: 2, envelope } if *envelope == expected)
    );
}

#[test]
fn nonblocking_admission_distinguishes_count_bytes_too_large_transient_and_closed() {
    let envelope = request_ack(&route());
    let size = envelope.encoded_len();
    let (carrier, _ingress) = Carrier::new(
        route(),
        CarrierLimits {
            outbound_count: 1,
            ..limits()
        },
    )
    .unwrap();
    assert_eq!(
        carrier.try_send(envelope.clone(), None),
        Err(SendError::Transient)
    );
    let (_binding, _monitor) = carrier.bind().unwrap();
    assert_eq!(carrier.try_send(envelope.clone(), None), Ok(()));
    assert_eq!(
        carrier.try_send(envelope.clone(), None),
        Err(SendError::QueueFull)
    );
    let report = carrier.shutdown();
    assert_eq!(
        report,
        ShutdownReport {
            unsent_count: 1,
            unsent_bytes: size
        }
    );
    assert_eq!(
        carrier.try_send(envelope.clone(), None),
        Err(SendError::Closed)
    );

    let (carrier, _ingress) = Carrier::new(
        route(),
        CarrierLimits {
            outbound_bytes: size * 2 - 1,
            ..limits()
        },
    )
    .unwrap();
    let (_binding, _monitor) = carrier.bind().unwrap();
    assert!(carrier.try_send(envelope.clone(), None).is_ok());
    assert_eq!(
        carrier.try_send(envelope.clone(), None),
        Err(SendError::QueueFull),
        "bytes, not count, bind admission"
    );

    let (carrier, _ingress) = Carrier::new(
        route(),
        CarrierLimits {
            outbound_bytes: size - 1,
            ..limits()
        },
    )
    .unwrap();
    let (_binding, _monitor) = carrier.bind().unwrap();
    assert_eq!(carrier.try_send(envelope, None), Err(SendError::TooLarge));
}

#[tokio::test(start_paused = true)]
async fn finite_message_expiry_is_observable_without_closing_infinite_request_ack_stream() {
    let (carrier, mut ingress) = Carrier::new(route(), limits()).unwrap();
    let (binding, monitor) = carrier.bind().unwrap();
    let mut outgoing = send_stream(binding);
    let expired = request_ack(&route());
    carrier
        .try_send(expired.clone(), Some(Duration::from_millis(10)))
        .unwrap();
    let infinite = request_ack(&route());
    carrier.try_send(infinite.clone(), None).unwrap();
    advance(Duration::from_millis(10)).await;
    assert_eq!(outgoing.next().await.unwrap(), infinite);
    assert!(
        matches!(receive(&mut ingress).await, Delivery::Expired { generation: 1, message_id } if message_id == expired.message_id)
    );
    assert_eq!(monitor.status(), StreamStatus::Running);
    let later = request_ack(&route());
    carrier.try_send(later.clone(), None).unwrap();
    assert_eq!(
        outgoing.next().await.unwrap(),
        later,
        "expiry must not kill the RPC/session"
    );
}

#[tokio::test]
async fn send_intent_callback_only_returns_false_for_queue_full_and_reports_all_other_errors() {
    let (carrier, _ingress) = Carrier::new(
        route(),
        CarrierLimits {
            outbound_count: 1,
            ..limits()
        },
    )
    .unwrap();
    let errors = Arc::new(Mutex::new(Vec::new()));
    let adapter = adapter(carrier.clone(), BufferPolicy::RawSingle, errors.clone());
    assert!(adapter.send(replication_intent(Bytes::new())));
    assert_eq!(*errors.lock().unwrap(), vec![SendError::Transient]);
    let (binding, _monitor) = carrier.bind().unwrap();
    assert!(adapter.send(replication_intent(Bytes::from_static(b"held"))));
    assert!(!adapter.send(replication_intent(Bytes::new())));
    assert_eq!(
        errors.lock().unwrap().len(),
        1,
        "queue-full is control flow, not an invented fault"
    );
    let mut outgoing = send_stream(binding);
    let operation = outgoing.next().await.unwrap();
    assert!(
        matches!(operation.body, Some(wire::envelope::Body::ReplicationOperation(body))
        if body.buffers.as_ref().unwrap().segments == [Bytes::from_static(b"held")])
    );
    assert!(adapter.send(replication_intent(Bytes::from(vec![0; 1025]))));
    assert_eq!(
        errors.lock().unwrap().last(),
        Some(&SendError::TooLarge),
        "too-large MUST reach the required fault owner"
    );
    carrier.shutdown();
    assert!(adapter.send(replication_intent(Bytes::new())));
    assert_eq!(errors.lock().unwrap().last(), Some(&SendError::Closed));
    let mut wrong = replication_intent(Bytes::new());
    wrong.identity.remote_incarnation += 1;
    assert!(adapter.send(wrong));
    assert_eq!(
        errors.lock().unwrap().last(),
        Some(&SendError::WrongSession)
    );
}

#[tokio::test]
async fn actual_intent_representation_is_explicit_even_for_magic_prefixed_raw_payloads() {
    let (carrier, _ingress) = Carrier::new(route(), limits()).unwrap();
    let (binding, _monitor) = carrier.bind().unwrap();
    let mut outgoing = send_stream(binding);
    let errors = Arc::new(Mutex::new(Vec::new()));
    let raw = adapter(carrier.clone(), BufferPolicy::RawSingle, errors.clone());
    let frame = buffers::encode(&[], BufferPolicy::FramedSegments, limits().buffers).unwrap();
    assert!(raw.send(replication_intent(frame.clone())));
    let Some(wire::envelope::Body::ReplicationOperation(body)) =
        outgoing.next().await.unwrap().body
    else {
        panic!("expected replication");
    };
    assert_eq!(body.buffers.unwrap().segments, vec![frame.clone()]);
    let framed = adapter(carrier, BufferPolicy::FramedSegments, errors.clone());
    assert!(framed.send(framed_replication_intent(frame)));
    let Some(wire::envelope::Body::ReplicationOperation(body)) =
        outgoing.next().await.unwrap().body
    else {
        panic!("expected replication");
    };
    assert!(
        body.buffers.unwrap().segments.is_empty(),
        "explicit framed empty vector, not singleton"
    );
    assert_eq!(body.buffer_counts, [0]);
    assert_eq!(
        body.original_epoch.unwrap(),
        wire::Epoch {
            data_loss: 1,
            configuration: 9
        }
    );
    assert_eq!(
        body.sending_epoch.unwrap(),
        wire::Epoch {
            data_loss: 2,
            configuration: 3
        }
    );
    assert_eq!(body.metadata.unwrap().atomic_group, -7);
    assert_eq!(body.completed_hint, 77);
    assert!(framed.send(replication_intent(Bytes::from_static(b"bad frame"))));
    assert!(matches!(
        errors.lock().unwrap().last(),
        Some(SendError::WrongRepresentation)
    ));
}

#[tokio::test]
async fn framed_adapter_rejects_valid_frames_admitted_as_raw_queue_operations() {
    let (carrier, _ingress) = Carrier::new(route(), limits()).unwrap();
    let (binding, _monitor) = carrier.bind().unwrap();
    let mut outgoing = send_stream(binding);
    let errors = Arc::new(Mutex::new(Vec::new()));
    let adapter = adapter(carrier, BufferPolicy::FramedSegments, errors.clone());
    let frame = buffers::encode(&[], BufferPolicy::FramedSegments, limits().buffers).unwrap();
    let raw = QueueOperation::new(1, frame.clone(), true);
    assert_eq!(raw.representation(), BufferPolicy::RawSingle);
    assert_eq!(raw.logical_bytes(), frame.len());
    assert!(adapter.send(intent_for_operation(raw)));
    assert_eq!(
        *errors.lock().unwrap(),
        vec![SendError::WrongRepresentation]
    );
    assert!(
        outgoing.next().now_or_never().is_none(),
        "mismatched accounting/adapter policy must not reach the wire"
    );
    let explicit = QueueOperation::from_data(
        1,
        frame,
        true,
        BufferPolicy::FramedSegments,
        limits().buffers,
    )
    .unwrap();
    assert_eq!(explicit.logical_bytes(), 0);
    assert_eq!(explicit.representation(), BufferPolicy::FramedSegments);
    assert!(adapter.send(intent_for_operation(explicit)));
    let Some(wire::envelope::Body::ReplicationOperation(body)) =
        outgoing.next().await.unwrap().body
    else {
        panic!("expected explicitly framed operation");
    };
    assert!(body.buffers.unwrap().segments.is_empty());
}

#[tokio::test]
async fn admitted_wire_payload_outlives_queue_operation_and_framing_overhead_is_not_logical_data() {
    let (carrier, _ingress) = Carrier::new(route(), limits()).unwrap();
    let (binding, _monitor) = carrier.bind().unwrap();
    let mut outgoing = send_stream(binding);
    let errors = Arc::new(Mutex::new(Vec::new()));
    let adapter = adapter(carrier, BufferPolicy::FramedSegments, errors.clone());
    let expected = Bytes::from(vec![0xab; limits().buffers.max_payload_bytes]);
    let framed = buffers::encode(
        &[Bytes::new(), expected.clone(), Bytes::new()],
        BufferPolicy::FramedSegments,
        limits().buffers,
    )
    .unwrap();
    assert!(framed.len() > limits().buffers.max_payload_bytes);
    let operation = QueueOperation::from_data(
        1,
        framed,
        true,
        BufferPolicy::FramedSegments,
        limits().buffers,
    )
    .unwrap();
    assert_eq!(
        operation.logical_bytes(),
        limits().buffers.max_payload_bytes
    );
    let released = Arc::downgrade(&operation);
    assert!(adapter.send(SendIntent {
        identity: identity(),
        kind: SendKind::Replication {
            operation,
            original_epoch: Epoch::new(1, 9),
            sending_epoch: Epoch::new(2, 3),
            completed_hint: 0,
        },
        expires_after: None,
    }));
    assert!(
        released.upgrade().is_none(),
        "the adapter roots the payload without retaining the queue owner"
    );
    let Some(wire::envelope::Body::ReplicationOperation(body)) =
        outgoing.next().await.unwrap().body
    else {
        panic!("expected owned replication bytes");
    };
    assert_eq!(
        body.buffers.unwrap().segments,
        vec![Bytes::new(), expected, Bytes::new()]
    );
    assert!(
        errors.lock().unwrap().is_empty(),
        "1024 logical bytes plus framing/wire overhead fit independent bounds"
    );
}

#[tokio::test]
async fn malformed_presence_version_batch_and_full_route_are_rejected_not_delivered() {
    let (carrier, _ingress) = Carrier::new(route(), limits()).unwrap();
    let (binding, _monitor) = carrier.bind().unwrap();
    let incoming = route().reversed();
    let good = request_ack(&incoming);
    for (mut envelope, error) in [
        (good.clone(), ProtocolError::Missing("body")),
        (good.clone(), ProtocolError::UnsupportedVersion(2)),
        (good.clone(), ProtocolError::WrongSource),
        (good.clone(), ProtocolError::WrongDestination),
        (good.clone(), ProtocolError::WrongSender),
        (good.clone(), ProtocolError::Missing("source")),
    ] {
        match error {
            ProtocolError::Missing("body") => envelope.body = None,
            ProtocolError::UnsupportedVersion(_) => envelope.version = 2,
            ProtocolError::WrongSource => {
                envelope.source.as_mut().unwrap().incarnation = Bytes::from(vec![9; 16])
            }
            ProtocolError::WrongDestination => {
                envelope.destination.as_mut().unwrap().incarnation = Bytes::from(vec![9; 16])
            }
            ProtocolError::WrongSender => envelope.sender_address = "impostor".into(),
            ProtocolError::Missing("source") => envelope.source = None,
            _ => unreachable!(),
        }
        assert!(
            matches!(binding.received(envelope), Delivery::Rejected { error: actual, .. } if actual == error)
        );
    }
    for malformed in [
        wire::ReplicationOperation::default(),
        wire::ReplicationOperation {
            metadata: Some(wire::Metadata::default()),
            original_epoch: Some(wire::Epoch::default()),
            sending_epoch: Some(wire::Epoch::default()),
            buffers: Some(wire::Buffers { segments: vec![] }),
            first_sequence: i64::MIN,
            last_sequence: i64::MAX,
            ..Default::default()
        },
    ] {
        let envelope = incoming.envelope(wire::envelope::Body::ReplicationOperation(malformed));
        assert!(matches!(
            binding.received(envelope),
            Delivery::Rejected { .. }
        ));
    }
    assert!(
        wire::Envelope::decode(&b"\xff"[..]).is_err(),
        "malformed protobuf is not an empty successful body"
    );
}

#[tokio::test]
async fn reconnect_fences_both_buffered_delivery_and_old_stream_without_changing_incarnation() {
    let (carrier, mut ingress) = Carrier::new(route(), limits()).unwrap();
    let (old_binding, mut old_monitor) = carrier.bind().unwrap();
    let old = Arc::new(old_binding);
    let mut pending = Some(old.received(request_ack(&route().reversed())));
    poll_fn(|cx| old.poll_deliver(cx, &mut pending)).await;
    carrier.try_send(request_ack(&route()), None).unwrap();
    let (new_binding, new_monitor) = carrier.bind().unwrap();
    assert_eq!(old_monitor.terminated().await, Terminal::Superseded);
    assert!(matches!(
        receive(&mut ingress).await,
        Delivery::Rejected {
            generation: 1,
            error: ProtocolError::StaleStream
        }
    ));
    let mut late = Some(old.received(request_ack(&route().reversed())));
    poll_fn(|cx| old.poll_deliver(cx, &mut late)).await;
    assert!(matches!(
        receive(&mut ingress).await,
        Delivery::Rejected {
            error: ProtocolError::StaleStream,
            ..
        }
    ));
    let mut old_stream = SendStream { binding: old };
    assert!(old_stream.next().await.is_none());
    let mut new_stream = send_stream(new_binding);
    let next = request_ack(&route());
    carrier.try_send(next.clone(), None).unwrap();
    assert_eq!(new_stream.next().await.unwrap(), next);
    assert!(matches!(
        receive(&mut ingress).await,
        Delivery::SupersededSend { generation: 1, .. }
    ));
    assert_eq!(new_monitor.status(), StreamStatus::Running);
    assert_eq!(
        carrier.0.route.destination.incarnation,
        Bytes::from(vec![3; 16])
    );
}

#[tokio::test]
async fn ingress_count_and_bytes_backpressure_without_losing_error_or_terminal_status() {
    let limit = limits();
    let (carrier, mut ingress) = Carrier::new(
        route(),
        CarrierLimits {
            inbound_count: 1,
            ..limit
        },
    )
    .unwrap();
    let (binding, mut monitor) = carrier.bind().unwrap();
    let binding = Arc::new(binding);
    let incoming = request_ack(&route().reversed());
    let mut pump = ReceivePump {
        binding: binding.clone(),
        incoming: Box::pin(stream::iter(vec![
            Ok(incoming.clone()),
            Ok(incoming),
            Err(Status::unavailable("test reset")),
        ])),
        pending: None,
    };
    assert!(poll_fn(|cx| Poll::Ready(pump.poll(cx))).await.is_pending());
    assert_eq!(carrier.0.state.lock().unwrap().incoming.len(), 1);
    assert!(
        pump.pending.is_some(),
        "one bounded staging message waits instead of an unbounded queue"
    );
    assert!(matches!(
        receive(&mut ingress).await,
        Delivery::Message { .. }
    ));
    assert!(matches!(
        poll_fn(|cx| pump.poll(cx)).await,
        Terminal::Rpc {
            code: tonic::Code::Unavailable,
            ..
        }
    ));
    assert!(matches!(
        receive(&mut ingress).await,
        Delivery::Message { .. }
    ));
    assert!(matches!(
        monitor.terminated().await,
        Terminal::Rpc {
            code: tonic::Code::Unavailable,
            ..
        }
    ));
    assert!(
        matches!(
            monitor.status(),
            StreamStatus::Terminated(Terminal::Rpc { .. })
        ),
        "terminal result remains retained without a status queue fallback"
    );

    let (carrier, mut ingress) = Carrier::new(
        route(),
        CarrierLimits {
            inbound_count: 8,
            inbound_bytes: limit.max_wire_bytes,
            ..limit
        },
    )
    .unwrap();
    let (binding, _monitor) = carrier.bind().unwrap();
    let binding = Arc::new(binding);
    // Large IDs are wire overhead, not application payload.
    let mut envelope = request_ack(&route().reversed());
    envelope.message_id = "x".repeat(2100);
    let mut first = Some(binding.received(envelope.clone()));
    poll_fn(|cx| binding.poll_deliver(cx, &mut first)).await;
    let mut second = Some(binding.received(envelope));
    assert!(
        poll_fn(|cx| Poll::Ready(binding.poll_deliver(cx, &mut second)))
            .await
            .is_pending()
    );
    assert!(matches!(
        receive(&mut ingress).await,
        Delivery::Message { .. }
    ));
    poll_fn(|cx| binding.poll_deliver(cx, &mut second)).await;
    assert!(matches!(
        receive(&mut ingress).await,
        Delivery::Message { .. }
    ));
}

struct Loopback {
    client: Carrier,
    client_ingress: Ingress,
    server: Carrier,
    server_ingress: Ingress,
    driver: JoinHandle<Terminal>,
    listener: JoinHandle<Result<(), tonic::transport::Error>>,
    stop: oneshot::Sender<()>,
    service: ExchangeService,
    channel: tonic::transport::Channel,
}

#[derive(Clone)]
struct RejectExchange(Status);

#[tonic::async_trait]
impl wire::carrier_server::Carrier for RejectExchange {
    type ExchangeStream = Pin<Box<dyn Stream<Item = Result<wire::Envelope, Status>> + Send>>;

    async fn exchange(
        &self,
        _request: Request<tonic::Streaming<wire::Envelope>>,
    ) -> Result<Response<Self::ExchangeStream>, Status> {
        Err(self.0.clone())
    }
}

#[tokio::test]
async fn actual_tonic_failed_exchange_preserves_rpc_error_in_return_value_and_monitor() {
    for status in [
        Status::permission_denied("owner refused stream admission"),
        Status::unavailable("route temporarily unavailable"),
    ] {
        let expected = rpc_terminal(status.clone());
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = tcp.local_addr().unwrap();
        let accepted = stream::unfold(tcp, |listener| async move {
            Some((listener.accept().await.map(|(socket, _)| socket), listener))
        });
        let (stop, stopped) = oneshot::channel();
        let listener = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(wire::carrier_server::CarrierServer::new(RejectExchange(
                    status,
                )))
                .serve_with_incoming_shutdown(accepted, async {
                    let _ = stopped.await;
                }),
        );
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let (carrier, _ingress) = Carrier::new(route(), limits()).unwrap();
        let (binding, mut monitor) = carrier.bind().unwrap();
        let actual = timeout(Duration::from_secs(3), connect(binding, channel))
            .await
            .unwrap()
            .err()
            .expect("the actual tonic service must reject Exchange");
        assert_eq!(
            actual, expected,
            "request stream teardown must not mask the actual RPC code/message"
        );
        assert_eq!(monitor.terminated().await, expected);
        assert_eq!(monitor.status(), StreamStatus::Terminated(expected));
        carrier.shutdown();
        stop.send(()).unwrap();
        timeout(Duration::from_secs(3), listener)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

impl Loopback {
    async fn start() -> Self {
        let (client, client_ingress) = Carrier::new(route(), limits()).unwrap();
        let (server, server_ingress) = Carrier::new(route().reversed(), limits()).unwrap();
        let (server_binding, _monitor) = server.bind().unwrap();
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = tcp.local_addr().unwrap();
        let accepted = stream::unfold(tcp, |listener| async move {
            let accepted = listener.accept().await.map(|(socket, _)| socket);
            Some((accepted, listener))
        });
        let (stop, stopped) = oneshot::channel();
        let service = ExchangeService::new(server_binding);
        let listener = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(service.clone().into_service(limits().max_wire_bytes))
                .serve_with_incoming_shutdown(accepted, async {
                    let _ = stopped.await;
                }),
        );
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let (client_binding, _monitor) = client.bind().unwrap();
        let driver = connect(client_binding, channel.clone()).await.unwrap();
        let driver = tokio::spawn(driver.run());
        Self {
            client,
            client_ingress,
            server,
            server_ingress,
            driver,
            listener,
            stop,
            service,
            channel,
        }
    }

    async fn reconnect(&mut self) {
        let (server_binding, _monitor) = self.server.bind().unwrap();
        assert!(self.service.replace_binding(server_binding).is_none());
        let (client_binding, _monitor) = self.client.bind().unwrap();
        let driver = connect(client_binding, self.channel.clone()).await.unwrap();
        let old = std::mem::replace(&mut self.driver, tokio::spawn(driver.run()));
        assert_eq!(
            timeout(Duration::from_secs(3), old).await.unwrap().unwrap(),
            Terminal::Superseded
        );
    }

    async fn close(self) {
        self.client.shutdown();
        self.server.shutdown();
        assert!(matches!(
            timeout(Duration::from_secs(3), self.driver)
                .await
                .unwrap()
                .unwrap(),
            Terminal::Shutdown
        ));
        self.stop.send(()).unwrap();
        timeout(Duration::from_secs(3), self.listener)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn actual_tonic_reconnect_requires_fresh_owner_binding_and_keeps_full_endpoint_identity() {
    let mut loopback = Loopback::start().await;
    let first = request_ack(&route());
    loopback.client.try_send(first.clone(), None).unwrap();
    assert!(
        matches!(receive(&mut loopback.server_ingress).await, Delivery::Message { generation: 1, envelope } if *envelope == first)
    );
    loopback.reconnect().await;
    let after = request_ack(&route());
    loopback.client.try_send(after.clone(), None).unwrap();
    assert!(
        matches!(receive(&mut loopback.server_ingress).await, Delivery::Message { generation: 2, envelope } if *envelope == after)
    );
    assert_eq!(loopback.client.0.route.destination, route().destination);
    let reply = request_ack(&route().reversed());
    loopback.server.try_send(reply.clone(), None).unwrap();
    assert!(
        matches!(receive(&mut loopback.client_ingress).await, Delivery::Message { generation: 2, envelope } if *envelope == reply)
    );
    loopback.close().await;
}

#[tokio::test]
async fn actual_tonic_rejects_wrong_incarnations_and_missing_body_at_bounded_ingress() {
    let (carrier, mut ingress) = Carrier::new(
        route().reversed(),
        CarrierLimits {
            inbound_count: 1,
            ..limits()
        },
    )
    .unwrap();
    let (binding, mut monitor) = carrier.bind().unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    let accepted = stream::unfold(tcp, |listener| async move {
        Some((listener.accept().await.map(|(socket, _)| socket), listener))
    });
    let (stop, stopped) = oneshot::channel();
    let service = ExchangeService::new(binding);
    let listener = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(service.into_service(limits().max_wire_bytes))
            .serve_with_incoming_shutdown(accepted, async {
                let _ = stopped.await;
            }),
    );
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut raw_client = wire::carrier_client::CarrierClient::new(channel);
    let mut wrong_source = request_ack(&route());
    wrong_source.source.as_mut().unwrap().incarnation = Bytes::from(vec![9; 16]);
    let mut wrong_destination = request_ack(&route());
    wrong_destination.destination.as_mut().unwrap().incarnation = Bytes::from(vec![9; 16]);
    let mut missing_body = request_ack(&route());
    missing_body.body = None;
    let response = raw_client
        .exchange(stream::iter(vec![
            wrong_source,
            wrong_destination,
            missing_body,
        ]))
        .await
        .unwrap();
    let mut response = response.into_inner();
    for expected in [
        ProtocolError::WrongSource,
        ProtocolError::WrongDestination,
        ProtocolError::Missing("body"),
    ] {
        assert!(
            matches!(receive(&mut ingress).await, Delivery::Rejected { error, .. } if error == expected)
        );
    }
    assert!(
        timeout(Duration::from_secs(3), response.next())
            .await
            .unwrap()
            .is_none(),
        "a rejected frame does not fabricate a service ACK"
    );
    assert_eq!(monitor.terminated().await, Terminal::RemoteClosed);
    carrier.shutdown();
    stop.send(()).unwrap();
    timeout(Duration::from_secs(3), listener)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn expiry_reporting_is_itself_bounded_and_retains_admitted_payload_until_owner_has_space() {
    let (carrier, mut ingress) = Carrier::new(
        route(),
        CarrierLimits {
            inbound_count: 1,
            ..limits()
        },
    )
    .unwrap();
    let (binding, _monitor) = carrier.bind().unwrap();
    let root = Arc::new(binding);
    let mut occupied = Some(root.received(request_ack(&route().reversed())));
    poll_fn(|cx| root.poll_deliver(cx, &mut occupied)).await;
    let mut outgoing = SendStream { binding: root };
    let expired = request_ack(&route());
    carrier
        .try_send(expired.clone(), Some(Duration::ZERO))
        .unwrap();
    assert!(outgoing.next().now_or_never().is_none());
    assert_eq!(
        carrier.0.state.lock().unwrap().outgoing.len(),
        1,
        "do not drop payload/status into an unbounded or lossy side channel"
    );
    assert!(matches!(
        receive(&mut ingress).await,
        Delivery::Message { .. }
    ));
    assert!(outgoing.next().now_or_never().is_none());
    assert_eq!(carrier.0.state.lock().unwrap().outgoing.len(), 0);
    assert!(
        matches!(receive(&mut ingress).await, Delivery::Expired { message_id, .. } if message_id == expired.message_id)
    );
}

fn families(route: &Route) -> Vec<wire::Envelope> {
    use wire::envelope::Body;
    let metadata = Some(wire::Metadata {
        sequence: 11,
        operation_type: 0x42,
        atomic_group: -71,
    });
    let epoch = Some(wire::Epoch {
        data_loss: 5,
        configuration: 8,
    });
    [
        Body::StartCopy(wire::StartCopy {
            epoch,
            replica: 20,
            replication_start: 12,
        }),
        Body::ReplicationOperation(wire::ReplicationOperation {
            metadata,
            original_epoch: Some(wire::Epoch {
                data_loss: 1,
                configuration: 9,
            }),
            sending_epoch: epoch,
            buffers: Some(wire::Buffers {
                segments: vec![Bytes::from_static(b"\0\xff"), Bytes::new(), Bytes::new()],
            }),
            first_sequence: 11,
            last_sequence: 12,
            last_in_batch: 13,
            buffer_counts: vec![1, 2],
            completed_hint: 9,
        }),
        Body::CopyOperation(wire::CopyOperation {
            metadata,
            epoch,
            buffers: Some(wire::Buffers {
                segments: vec![Bytes::new(), Bytes::from_static(b"copy")],
            }),
            replica: 20,
            is_last: true,
        }),
        Body::CopyContextOperation(wire::CopyContextOperation {
            metadata,
            buffers: Some(wire::Buffers { segments: vec![] }),
            is_last: true,
        }),
        Body::ReplicationAck(wire::ReplicationAck {
            replication_received: 13,
            replication_applied: 9,
            copy_received: -1,
            copy_applied: -1,
            error: -71,
        }),
        Body::CopyContextAck(wire::CopyContextAck {
            sequence: 7,
            error: -99,
        }),
        Body::RequestAck(wire::RequestAck {}),
        Body::InduceFault(wire::InduceFault {
            target_replica: 20,
            target_incarnation: route.destination.incarnation.clone(),
            reason: "owner-selected reason".into(),
        }),
    ]
    .into_iter()
    .map(|body| route.envelope(body))
    .collect()
}

#[tokio::test]
async fn actual_tonic_bidirectional_exchange_roundtrips_all_eight_logical_families_losslessly() {
    let mut loopback = Loopback::start().await;
    for envelope in families(&route()) {
        loopback.client.try_send(envelope.clone(), None).unwrap();
        assert!(
            matches!(receive(&mut loopback.server_ingress).await, Delivery::Message { envelope: actual, .. } if *actual == envelope)
        );
        assert!(
            loopback.client_ingress.recv().now_or_never().is_none(),
            "RPC receipt is not an ACK"
        );
    }
    for envelope in families(&route().reversed()) {
        loopback.server.try_send(envelope.clone(), None).unwrap();
        assert!(
            matches!(receive(&mut loopback.client_ingress).await, Delivery::Message { envelope: actual, .. } if *actual == envelope)
        );
    }
    loopback.close().await;
}

#[tokio::test]
async fn actual_session_intent_reaches_tonic_but_progress_only_changes_on_independent_received_and_applied_acks()
 {
    let mut loopback = Loopback::start().await;
    let errors = Arc::new(Mutex::new(Vec::new()));
    let adapter = adapter(
        loopback.client.clone(),
        BufferPolicy::FramedSegments,
        errors.clone(),
    );
    let session = ReplicationSession::new(
        identity(),
        Epoch::new(2, 3),
        0,
        SenderConfig {
            initial_window: 1,
            max_window: 1,
            retry: Duration::from_secs(5),
            decay_factor: 0.0,
            decay_interval: Duration::from_secs(1),
        },
        Arc::new(move |intent| adapter.send(intent)),
        Duration::ZERO,
    );
    session.open();
    let segments = vec![
        Bytes::new(),
        Bytes::from_static(b"actual operation"),
        Bytes::new(),
    ];
    let data = buffers::encode(&segments, BufferPolicy::FramedSegments, limits().buffers).unwrap();
    let operation = QueueOperation::from_data(
        1,
        data,
        true,
        BufferPolicy::FramedSegments,
        limits().buffers,
    )
    .unwrap();
    assert_eq!(operation.logical_bytes(), b"actual operation".len());
    session
        .add(
            SenderOperation {
                operation: operation.clone(),
                original_epoch: Epoch::new(1, 9),
            },
            0,
            Duration::ZERO,
        )
        .dispatch();
    assert_eq!(session.progress(0).received, 0);
    let Delivery::Message { envelope, .. } = receive(&mut loopback.server_ingress).await else {
        panic!("expected message");
    };
    let Some(wire::envelope::Body::ReplicationOperation(body)) = envelope.body else {
        panic!("expected replication");
    };
    assert_eq!(body.buffers.unwrap().segments, segments);
    assert_eq!(
        body.original_epoch.unwrap(),
        wire::Epoch {
            data_loss: 1,
            configuration: 9
        }
    );
    assert_eq!(
        body.sending_epoch.unwrap(),
        wire::Epoch {
            data_loss: 2,
            configuration: 3
        }
    );
    assert_eq!(
        session.progress(0).applied,
        0,
        "wire admission and receive are not service ACKs"
    );
    assert!(operation.needs_ack());
    for applied in [0, 1] {
        let envelope = route()
            .reversed()
            .envelope(wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
                replication_received: 1,
                replication_applied: applied,
                copy_received: -1,
                copy_applied: -1,
                error: 0,
            }));
        loopback.server.try_send(envelope, None).unwrap();
        let Delivery::Message { envelope, .. } = receive(&mut loopback.client_ingress).await else {
            panic!("expected independent ACK");
        };
        let Some(wire::envelope::Body::ReplicationAck(ack)) = envelope.body else {
            panic!("expected ACK");
        };
        // Explicit test owner opts into core processing only after wire delivery.
        let mut processor = session
            .update_ack(
                AckUpdate {
                    replication_received: ack.replication_received,
                    replication_applied: ack.replication_applied,
                    copy_received: ack.copy_received,
                    copy_applied: ack.copy_applied,
                    process_copy: false,
                    message_id: envelope.message_id,
                },
                None,
            )
            .unwrap();
        let mut owner_obligations = Vec::new();
        while let Some(handoff) = processor.next(Duration::from_millis(1)) {
            processor = handoff.deliver(|action| owner_obligations.push(action));
        }
        assert_eq!(session.progress(0).received, 1);
        assert_eq!(session.progress(0).applied, applied);
        assert!(!session.is_processing());
    }
    assert!(errors.lock().unwrap().is_empty());
    session.close();
    loopback.close().await;
}

#[tokio::test]
async fn slow_fault_retry_reaches_actual_tonic_with_the_bound_target_identity() {
    let mut loopback = Loopback::start().await;
    let errors = Arc::new(Mutex::new(Vec::new()));
    let adapter = adapter(
        loopback.client.clone(),
        BufferPolicy::RawSingle,
        errors.clone(),
    );
    let session = ReplicationSession::new(
        identity(),
        Epoch::new(2, 3),
        0,
        SenderConfig {
            initial_window: 1,
            max_window: 1,
            retry: Duration::from_secs(5),
            decay_factor: 0.0,
            decay_interval: Duration::from_secs(1),
        },
        Arc::new(move |intent| adapter.send(intent)),
        Duration::ZERO,
    );
    session.open();
    assert!(session.try_fault_active("actual slow sender".into(), Duration::ZERO));
    for seconds in [5, 10] {
        session
            .on_fault_timer(Duration::from_secs(seconds))
            .unwrap()
            .dispatch();
        let Delivery::Message { envelope, .. } = receive(&mut loopback.server_ingress).await else {
            panic!("expected fault notification")
        };
        assert_eq!(envelope.destination.as_ref(), Some(&route().destination));
        let Some(wire::envelope::Body::InduceFault(body)) = envelope.body else {
            panic!("expected typed InduceFault")
        };
        assert_eq!(body.target_replica, route().destination.replica);
        assert_eq!(body.target_incarnation, route().destination.incarnation);
        assert_eq!(body.reason, "actual slow sender");
        assert_eq!(session.progress(0).applied, 0);
    }
    session.close();
    assert!(session.on_fault_timer(Duration::from_secs(15)).is_none());
    assert!(errors.lock().unwrap().is_empty());
    loopback.close().await;
}

#[derive(Debug, PartialEq, Eq)]
enum RecordedCopyHandoff {
    ReplicationGate {
        copy_applied: i64,
        replication_applied: i64,
    },
    CopyAck {
        received: i64,
        applied: i64,
    },
}

struct RecordingCopyAckOwner(tokio::sync::mpsc::Sender<RecordedCopyHandoff>);

impl CopyAckOwner for RecordingCopyAckOwner {
    fn replication_gate(
        &self,
        copy_applied: i64,
        replication_applied: i64,
    ) -> Result<(), OwnerError> {
        self.0
            .try_send(RecordedCopyHandoff::ReplicationGate {
                copy_applied,
                replication_applied,
            })
            .map_err(|error| Box::new(error) as OwnerError)
    }

    fn copy_ack(&self, received: i64, applied: i64) -> Result<(), OwnerError> {
        self.0
            .try_send(RecordedCopyHandoff::CopyAck { received, applied })
            .map_err(|error| Box::new(error) as OwnerError)
    }
}

#[tokio::test]
async fn actual_session_and_ack_runtimes_exchange_over_tonic_and_record_required_copy_handoffs() {
    let mut loopback = Loopback::start().await;
    let errors = Arc::new(Mutex::new(Vec::new()));
    let adapter = adapter(
        loopback.client.clone(),
        BufferPolicy::FramedSegments,
        errors.clone(),
    );
    // The bounded recording owner accepts concrete obligations for inspection;
    // it does not implement or claim copy/build/quorum completion.
    let (record, mut handoffs) = tokio::sync::mpsc::channel(4);
    let retry = Duration::from_secs(60);
    let clock = Clock::new();
    let mut session = SessionRuntime::start(
        identity(),
        Epoch::new(2, 3),
        0,
        SenderConfig {
            initial_window: 1,
            max_window: 1,
            retry,
            decay_factor: 0.0,
            decay_interval: Duration::from_secs(1),
        },
        Arc::new(move |intent| adapter.send(intent)),
        Arc::new(RecordingCopyAckOwner(record)),
        clock.clone(),
    );
    let session_handle = session.handle();
    let mut acks = AckRuntime::start(Duration::from_millis(1), retry, 4, Duration::ZERO, clock);
    let ack_handle = acks.handle();
    let latest_ack = Arc::new(Mutex::new(wire::ReplicationAck {
        replication_received: 1,
        replication_applied: 0,
        copy_received: 8,
        copy_applied: 7,
        error: 0,
    }));
    let payload = latest_ack.clone();
    let reverse = loopback.server.clone();
    let ack_errors = errors.clone();
    ack_handle
        .open(Arc::new(move |_should_trace| {
            let ack = *payload.lock().unwrap();
            let envelope = route()
                .reversed()
                .envelope(wire::envelope::Body::ReplicationAck(ack));
            match reverse.try_send(envelope, None) {
                Ok(()) => true,
                Err(error) => {
                    ack_errors.lock().unwrap().push(error);
                    false
                }
            }
        }))
        .unwrap();
    let segments = vec![
        Bytes::new(),
        Bytes::from_static(b"runtime-owned payload"),
        Bytes::new(),
    ];
    let data = buffers::encode(&segments, BufferPolicy::FramedSegments, limits().buffers).unwrap();
    let operation = QueueOperation::from_data(
        1,
        data,
        true,
        BufferPolicy::FramedSegments,
        limits().buffers,
    )
    .unwrap();
    assert_eq!(operation.logical_bytes(), b"runtime-owned payload".len());
    session_handle
        .add(
            SenderOperation {
                operation: operation.clone(),
                original_epoch: Epoch::new(1, 9),
            },
            0,
        )
        .unwrap();
    let Delivery::Message { envelope, .. } = receive(&mut loopback.server_ingress).await else {
        panic!("SessionRuntime must dispatch its actual SendIntent over tonic");
    };
    let Some(wire::envelope::Body::ReplicationOperation(body)) = envelope.body else {
        panic!("expected runtime replication operation");
    };
    assert_eq!(body.buffers.unwrap().segments, segments);
    assert_eq!(
        body.original_epoch.unwrap(),
        wire::Epoch {
            data_loss: 1,
            configuration: 9
        }
    );
    assert_eq!(
        body.sending_epoch.unwrap(),
        wire::Epoch {
            data_loss: 2,
            configuration: 3
        }
    );
    assert_eq!(session_handle.progress(0).unwrap().received, 0);
    assert_eq!(session_handle.progress(0).unwrap().applied, 0);
    assert!(operation.needs_ack());
    assert!(loopback.client_ingress.recv().now_or_never().is_none());
    for applied in [0, 1] {
        latest_ack.lock().unwrap().replication_applied = applied;
        ack_handle.schedule(false).unwrap();
        let Delivery::Message { envelope, .. } = receive(&mut loopback.client_ingress).await else {
            panic!("AckRuntime must deliver a separately scheduled reverse ACK");
        };
        let Some(wire::envelope::Body::ReplicationAck(ack)) = envelope.body else {
            panic!("expected reverse logical ACK");
        };
        assert_eq!(ack.replication_received, 1);
        assert_eq!(ack.replication_applied, applied);
        assert_eq!(
            session_handle.progress(0).unwrap().applied,
            0,
            "carrier delivery alone cannot apply an ACK"
        );
        session_handle
            .update_ack(
                AckUpdate {
                    replication_received: ack.replication_received,
                    replication_applied: ack.replication_applied,
                    copy_received: ack.copy_received,
                    copy_applied: ack.copy_applied,
                    process_copy: true,
                    message_id: envelope.message_id,
                },
                None,
            )
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(3), handoffs.recv())
                .await
                .unwrap()
                .unwrap(),
            RecordedCopyHandoff::ReplicationGate {
                copy_applied: 7,
                replication_applied: applied
            },
        );
        assert_eq!(
            timeout(Duration::from_secs(3), handoffs.recv())
                .await
                .unwrap()
                .unwrap(),
            RecordedCopyHandoff::CopyAck {
                received: 8,
                applied: 7
            },
        );
        assert_eq!(session_handle.progress(0).unwrap().received, 1);
        assert_eq!(session_handle.progress(0).unwrap().applied, applied);
        assert!(!session_handle.is_processing().unwrap());
    }
    acks.shutdown().await.unwrap();
    session.shutdown().await.unwrap();
    assert!(handoffs.try_recv().is_err());
    assert!(errors.lock().unwrap().is_empty());
    loopback.close().await;
}

#[tokio::test]
async fn owned_driver_shutdown_and_abandoned_stream_are_typed_not_success_shaped() {
    let (carrier, ingress) = Carrier::new(route(), limits()).unwrap();
    let (binding, mut monitor) = carrier.bind().unwrap();
    let stream = send_stream(binding);
    drop(ingress);
    assert_eq!(monitor.terminated().await, Terminal::OwnerDropped);
    drop(stream);
    let (carrier, _ingress) = Carrier::new(route(), limits()).unwrap();
    let (binding, mut monitor) = carrier.bind().unwrap();
    drop(binding);
    assert_eq!(monitor.terminated().await, Terminal::StreamDropped);
    assert_eq!(
        carrier.try_send(request_ack(&route()), None),
        Err(SendError::Transient)
    );
}
