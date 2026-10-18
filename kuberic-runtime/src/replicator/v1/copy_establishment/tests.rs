// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-component witnesses derived from the pinned production sources cited
// in copy_establishment.rs, not native test execution or a formula-only model.

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{FutureExt, StreamExt};
use tokio::sync::Semaphore;

use crate::replicator::v1::buffers::{BufferLimits, BufferPolicy};
use crate::replicator::v1::operation_queue::QueueConfig;
use crate::replicator::v1::sender::{SenderConfig, SenderOperation};
use crate::replicator::v1::session::{AckAction, SendIntent, SendKind, SessionIdentity};
use crate::replicator::v1::transport::{
    Carrier, CarrierLimits, Delivery, OperationMetadata, SendIntentAdapter,
};

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
        sender_address: "primary".into(),
        remote_address: "secondary".into(),
    }
}

fn config() -> CopyConfig {
    CopyConfig {
        sender: SenderConfig {
            initial_window: 4,
            max_window: 8,
            retry: Duration::from_secs(1),
            decay_factor: 0.5,
            decay_interval: Duration::from_secs(1),
        },
        buffer_policy: BufferPolicy::RawSingle,
        buffer_limits: BufferLimits {
            max_segments: 8,
            max_payload_bytes: 1024,
        },
    }
}

fn context_config() -> CopyContextConfig {
    CopyContextConfig {
        initial_queue_size: 4,
        max_queue_size: 8,
        batch_ack_interval: Duration::from_millis(10),
        retry_interval: Duration::from_secs(1),
        max_pending_acknowledgements: 4,
        trace_interval: Duration::ZERO,
    }
}

fn receiver_config(persisted: bool) -> ReceiverConfig {
    ReceiverConfig {
        queue: QueueConfig {
            initial_capacity: 4,
            max_capacity: 8,
            max_bytes: 0,
            max_completed_count: 0,
            max_completed_bytes: 0,
            clean_on_complete: true,
        },
        has_persisted_state: persisted,
        require_service_ack: false,
        enable_eos_ack: true,
    }
}

fn identity() -> SessionIdentity {
    SessionIdentity {
        primary_endpoint: "primary".into(),
        remote_endpoint: "secondary".into(),
        remote_incarnation: 20,
    }
}

fn session(send: Arc<dyn Fn(SendIntent) -> bool + Send + Sync>) -> ReplicationSession {
    let session = ReplicationSession::new(
        identity(),
        Epoch::new(1, 7),
        0,
        config().sender,
        send,
        Duration::ZERO,
    );
    session.open();
    session
}

#[derive(Default)]
struct Observations {
    states: Vec<(Lsn, Vec<Bytes>)>,
}

struct Provider {
    context_calls: AtomicUsize,
    state_calls: AtomicUsize,
    context_gate: Semaphore,
    state_gate: Semaphore,
    observations: Mutex<Observations>,
    context: Vec<Bytes>,
    context_error: bool,
    state_error: bool,
    state_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    context_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Provider {
    fn new(context: Vec<Bytes>) -> Self {
        Self {
            context_calls: AtomicUsize::new(0),
            state_calls: AtomicUsize::new(0),
            context_gate: Semaphore::new(1),
            state_gate: Semaphore::new(1),
            observations: Mutex::new(Observations::default()),
            context,
            context_error: false,
            state_error: false,
            state_hook: None,
            context_hook: None,
        }
    }
}

#[async_trait]
impl StateProvider for Provider {
    async fn update_epoch(&self, _: Epoch, _: Lsn) -> crate::Result<()> {
        panic!("establishment must not invent provider epoch completion")
    }
    async fn last_committed_lsn(&self) -> crate::Result<Lsn> {
        panic!("the build owner supplies the frozen committed boundary")
    }
    async fn get_copy_context(&self) -> crate::Result<OperationDataStream> {
        self.context_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(hook) = &self.context_hook {
            hook();
        }
        self.context_gate.acquire().await.unwrap().forget();
        if self.context_error {
            return Err(RuntimeError::InvalidReplication(
                "context acquisition".into(),
            ));
        }
        Ok(Box::pin(stream::iter(
            self.context.clone().into_iter().map(Ok),
        )))
    }
    async fn get_copy_state(
        &self,
        up_to_lsn: Lsn,
        mut context: OperationDataStream,
    ) -> crate::Result<OperationDataStream> {
        self.state_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(hook) = &self.state_hook {
            hook();
        }
        self.state_gate.acquire().await.unwrap().forget();
        if self.state_error {
            return Err(RuntimeError::InvalidReplication("state acquisition".into()));
        }
        let mut data = Vec::new();
        while let Some(item) = context.next().await {
            data.push(item?);
        }
        self.observations
            .lock()
            .unwrap()
            .states
            .push((up_to_lsn, data));
        Ok(Box::pin(stream::iter([Ok(Bytes::from_static(b"state"))])))
    }
    async fn on_data_loss(&self) -> crate::Result<bool> {
        panic!("establishment must not invoke data-loss recovery")
    }
}

struct PrimaryRig {
    owner: PrimaryEstablishment,
    handle: PrimaryEstablishmentHandle,
    session: ReplicationSession,
    sends: Arc<Mutex<Vec<SendIntent>>>,
    context_acks: Arc<Mutex<Vec<CopyContextAck>>>,
}

impl PrimaryRig {
    fn new(persisted: bool, provider: Arc<Provider>) -> Self {
        let sends = Arc::new(Mutex::new(Vec::new()));
        let sent = sends.clone();
        let session = session(Arc::new(move |intent| {
            sent.lock().unwrap().push(intent);
            true
        }));
        Self::with_session(persisted, provider, session, sends)
    }

    fn with_session(
        persisted: bool,
        provider: Arc<Provider>,
        session: ReplicationSession,
        sends: Arc<Mutex<Vec<SendIntent>>>,
    ) -> Self {
        let context_acks = Arc::new(Mutex::new(Vec::new()));
        let observed = context_acks.clone();
        let owner = PrimaryEstablishment::new(
            route(),
            session.clone(),
            config(),
            persisted.then(context_config),
            provider,
            Arc::new(move |ack, _| {
                observed.lock().unwrap().push(ack);
                true
            }),
            Clock::new(),
        )
        .unwrap();
        let handle = owner.handle();
        Self {
            owner,
            handle,
            session,
            sends,
            context_acks,
        }
    }

    async fn acquire(&mut self) -> Result<(), EstablishError> {
        self.owner
            .acquire_copy(Arc::new(|_| true), Arc::new(|| Ok(23)))
            .await
    }

    fn ack(&mut self, received: Lsn, applied: Lsn, copy: Lsn, error: i32) {
        let processor = self
            .handle
            .receive_ack(&ack(received, applied, copy, error))
            .unwrap();
        if let Some(mut processor) = processor {
            let handle = self.owner.copy().handle();
            while let Some(handoff) = processor.next(Duration::ZERO) {
                processor = handoff.deliver(|action| match action {
                    AckAction::ReplicationSends(batch) => batch.dispatch(),
                    AckAction::CopyReplicationGate {
                        copy_applied,
                        replication_applied,
                    } => {
                        handle
                            .process_replication_ack(copy_applied, replication_applied)
                            .unwrap();
                    }
                    AckAction::PrimaryProgress(notification) => notification.dispatch(),
                    AckAction::CopyAck { received, applied } => {
                        handle.process_copy_ack(received, applied).unwrap();
                    }
                });
            }
        }
    }
}

fn secondary(persisted: bool, provider: Arc<Provider>) -> SecondaryEstablishment {
    SecondaryEstablishment::new(
        route().reversed(),
        receiver_config(persisted),
        config(),
        provider,
        Arc::new(|_| {}),
        Clock::new(),
    )
    .unwrap()
}

fn start(epoch: Epoch, replication_start: Lsn) -> wire::Envelope {
    route().envelope(wire::envelope::Body::StartCopy(wire::StartCopy {
        epoch: Some(wire::Epoch {
            data_loss: epoch.data_loss_number,
            configuration: epoch.configuration_number,
        }),
        replica: 20,
        replication_start,
    }))
}

fn context(lsn: Lsn, is_last: bool, data: &[u8]) -> wire::Envelope {
    route()
        .reversed()
        .envelope(wire::envelope::Body::CopyContextOperation(
            wire::CopyContextOperation {
                metadata: Some(wire::Metadata {
                    sequence: lsn,
                    operation_type: 1,
                    atomic_group: -1,
                }),
                buffers: Some(wire::Buffers {
                    segments: vec![Bytes::copy_from_slice(data)],
                }),
                is_last,
            },
        ))
}

fn ack(received: Lsn, applied: Lsn, copy: Lsn, error: i32) -> wire::Envelope {
    route()
        .reversed()
        .envelope(wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
            replication_received: received,
            replication_applied: applied,
            copy_received: copy,
            copy_applied: copy,
            error,
        }))
}

fn context_ack(sequence: Lsn, error: i32) -> wire::Envelope {
    route().envelope(wire::envelope::Body::CopyContextAck(wire::CopyContextAck {
        sequence,
        error,
    }))
}

#[tokio::test]
async fn provider_acquisition_waits_for_establishment() {
    let provider = Arc::new(Provider::new(Vec::new()));
    let mut rig = PrimaryRig::new(true, provider.clone());
    assert!(matches!(
        rig.acquire().await,
        Err(EstablishError::NotStarted)
    ));
    rig.owner.begin(8).unwrap();
    assert!(rig.acquire().now_or_never().is_none());
    assert_eq!(provider.state_calls.load(Ordering::SeqCst), 0);
    assert_eq!(rig.sends.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn persisted_replication_ack_alone_does_not_establish() {
    let provider = Arc::new(Provider::new(Vec::new()));
    let mut rig = PrimaryRig::new(true, provider.clone());
    rig.owner.begin(8).unwrap();
    rig.ack(7, 7, 0, 0);
    assert!(rig.acquire().now_or_never().is_none());
    assert_eq!(provider.state_calls.load(Ordering::SeqCst), 0);
    assert!(rig.session.start_copy_deadline().is_none());
}

#[tokio::test]
async fn nonpersisted_ack_establishes_empty_context_at_frozen_boundary() {
    let provider = Arc::new(Provider::new(Vec::new()));
    let mut rig = PrimaryRig::new(false, provider.clone());
    rig.owner.begin(8).unwrap();
    rig.ack(7, 7, 0, 0);
    rig.acquire().await.unwrap();
    rig.acquire().await.unwrap();
    assert_eq!(provider.state_calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.context_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        provider.observations.lock().unwrap().states,
        vec![(7, vec![])]
    );
}

#[tokio::test]
async fn first_context_establishes_before_its_stream_eof() {
    let provider = Arc::new(Provider::new(Vec::new()));
    let mut rig = PrimaryRig::new(true, provider.clone());
    rig.owner.begin(8).unwrap();
    rig.handle
        .receive_context(&context(1, false, b"target"))
        .unwrap();
    assert!(rig.acquire().now_or_never().is_none());
    assert_eq!(provider.state_calls.load(Ordering::SeqCst), 1);
    rig.handle.receive_context(&context(2, true, b"")).unwrap();
    rig.acquire().await.unwrap();
    assert_eq!(
        provider.observations.lock().unwrap().states,
        vec![(7, vec![Bytes::from_static(b"target")])]
    );
    assert_eq!(
        rig.context_acks.lock().unwrap().last().unwrap().completed,
        2
    );
}

#[tokio::test]
async fn ordinary_empty_context_is_not_terminal() {
    let mut rig = PrimaryRig::new(true, Arc::new(Provider::new(Vec::new())));
    rig.owner.begin(1).unwrap();
    rig.handle.receive_context(&context(1, false, b"")).unwrap();
    assert!(rig.acquire().now_or_never().is_none());
    rig.handle.receive_context(&context(2, true, b"")).unwrap();
    rig.acquire().await.unwrap();
}

#[tokio::test]
async fn terminal_only_context_is_valid() {
    let provider = Arc::new(Provider::new(Vec::new()));
    let mut rig = PrimaryRig::new(true, provider.clone());
    rig.owner.begin(1).unwrap();
    rig.handle.receive_context(&context(1, true, b"")).unwrap();
    rig.acquire().await.unwrap();
    assert_eq!(
        provider.observations.lock().unwrap().states,
        vec![(0, vec![])]
    );
}

#[tokio::test]
async fn malformed_context_cannot_establish_or_replace_terminal() {
    let mut rig = PrimaryRig::new(true, Arc::new(Provider::new(Vec::new())));
    rig.owner.begin(1).unwrap();
    for envelope in [context(0, false, b"x"), context(1, true, b"not empty")] {
        assert!(rig.handle.receive_context(&envelope).is_err());
    }
    assert!(rig.acquire().now_or_never().is_none());
}

#[tokio::test]
async fn context_before_begin_or_on_nonpersisted_owner_is_explicit_error() {
    let mut rig = PrimaryRig::new(false, Arc::new(Provider::new(Vec::new())));
    assert!(matches!(
        rig.handle.receive_context(&context(1, true, b"")),
        Err(EstablishError::NotStarted)
    ));
    rig.owner.begin(1).unwrap();
    assert!(matches!(
        rig.handle.receive_context(&context(1, true, b"")),
        Err(EstablishError::Protocol(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn waiting_owner_drives_actual_reliable_start_copy_retries() {
    let mut rig = PrimaryRig::new(true, Arc::new(Provider::new(Vec::new())));
    rig.owner.begin(8).unwrap();
    assert!(rig.acquire().now_or_never().is_none());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(rig.acquire().now_or_never().is_none());
    let sends = rig.sends.lock().unwrap();
    assert_eq!(sends.len(), 2);
    for intent in sends.iter() {
        assert!(
            matches!(intent.kind, SendKind::StartCopy { replication_start: 8, epoch } if epoch == Epoch::new(1,7))
        );
        assert_eq!(intent.expires_after, None);
    }
}

#[tokio::test(start_paused = true)]
async fn selected_start_send_survives_close_and_samples_current_epoch() {
    let sends = Arc::new(Mutex::new(Vec::new()));
    let observed = sends.clone();
    let session = session(Arc::new(move |intent| {
        observed.lock().unwrap().push(intent);
        true
    }));
    let selected = session.start_copy(config().sender, 8, Duration::ZERO);
    session.update_sending_epoch(Epoch::new(1, 9));
    session.close_start_copy();
    selected.dispatch();
    assert!(
        matches!(sends.lock().unwrap()[0].kind, SendKind::StartCopy { epoch, .. } if epoch == Epoch::new(1,9))
    );
    assert!(session.start_copy_deadline().is_none());
}

#[tokio::test]
async fn immediate_context_response_can_reenter_after_sender_publication() {
    let slot: Arc<Mutex<Option<PrimaryEstablishmentHandle>>> = Arc::new(Mutex::new(None));
    let reenter = slot.clone();
    let session = session(Arc::new(move |intent| {
        assert!(matches!(intent.kind, SendKind::StartCopy { .. }));
        let handle = reenter.lock().unwrap().clone().unwrap();
        let root = handle.root().unwrap();
        assert!(root.session.start_copy_deadline().is_some());
        handle.receive_context(&context(1, true, b"")).unwrap();
        true
    }));
    let mut rig = PrimaryRig::with_session(
        true,
        Arc::new(Provider::new(Vec::new())),
        session,
        Arc::default(),
    );
    *slot.lock().unwrap() = Some(rig.handle.clone());
    rig.owner.begin(8).unwrap();
    rig.acquire().await.unwrap();
}

#[tokio::test]
async fn start_copy_adapter_uses_bound_replica_and_logical_carrier() {
    let limits = CarrierLimits {
        outbound_count: 4,
        outbound_bytes: 8192,
        inbound_count: 4,
        inbound_bytes: 8192,
        max_wire_bytes: 4096,
        buffers: config().buffer_limits,
    };
    let (carrier, _ingress) = Carrier::new(route(), limits).unwrap();
    let (binding, _monitor) = carrier.bind().unwrap();
    let errors = Arc::new(Mutex::new(Vec::new()));
    let observed = errors.clone();
    let adapter = Arc::new(
        SendIntentAdapter::new(
            identity(),
            route(),
            BufferPolicy::RawSingle,
            OperationMetadata {
                operation_type: 1,
                atomic_group: -1,
            },
            carrier.clone(),
            Arc::new(move |error| observed.lock().unwrap().push(error)),
        )
        .unwrap(),
    );
    let outgoing = adapter.clone();
    let session = session(Arc::new(move |intent| outgoing.send(intent)));
    session
        .start_copy(config().sender, 8, Duration::ZERO)
        .dispatch();
    let service = super::super::transport::ExchangeService::new(binding);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(service.into_service(limits.max_wire_bytes))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    let (remote, mut ingress) = Carrier::new(route().reversed(), limits).unwrap();
    let (binding, _monitor) = remote.bind().unwrap();
    let channel = tonic::transport::Channel::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let driver = super::super::transport::connect(binding, channel)
        .await
        .unwrap();
    let client = tokio::spawn(driver.run());
    let delivery = tokio::time::timeout(Duration::from_secs(3), ingress.recv())
        .await
        .unwrap()
        .unwrap();
    let Delivery::Message { envelope, .. } = delivery else {
        panic!("expected message")
    };
    let Some(wire::envelope::Body::StartCopy(body)) = envelope.body else {
        panic!("expected StartCopy")
    };
    assert_eq!(body.replica, 20);
    assert_eq!(body.replication_start, 8);
    assert_eq!(body.epoch.unwrap().configuration, 7);
    assert!(errors.lock().unwrap().is_empty());
    remote.shutdown();
    carrier.shutdown();
    client.await.unwrap();
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn invalid_start_does_not_consume_begin() {
    let mut rig = PrimaryRig::new(false, Arc::new(Provider::new(Vec::new())));
    for lsn in [0, -1, Lsn::MAX] {
        assert!(matches!(
            rig.owner.begin(lsn),
            Err(EstablishError::InvalidStart)
        ));
    }
    rig.owner.begin(1).unwrap();
    assert!(matches!(
        rig.owner.begin(2),
        Err(EstablishError::AlreadyStarted)
    ));
    assert_eq!(rig.sends.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn full_endpoint_and_sender_identity_admission_precedes_establishment() {
    let mut rig = PrimaryRig::new(false, Arc::new(Provider::new(Vec::new())));
    rig.owner.begin(1).unwrap();
    for which in 0..5 {
        let mut message = ack(1, 1, 0, 0);
        match which {
            0 => message.source.as_mut().unwrap().incarnation = Bytes::from(vec![4; 16]),
            1 => message.source.as_mut().unwrap().partition = Bytes::from(vec![4; 16]),
            2 => message.source.as_mut().unwrap().replica += 1,
            3 => message.destination.as_mut().unwrap().incarnation = Bytes::from(vec![4; 16]),
            _ => message.sender_address = "wrong".into(),
        }
        assert!(matches!(
            rig.handle.receive_ack(&message),
            Err(EstablishError::Protocol(_))
        ));
    }
    assert!(rig.acquire().now_or_never().is_none());
}

#[tokio::test]
async fn primary_rejects_wrong_message_version_and_missing_identifier() {
    let mut rig = PrimaryRig::new(true, Arc::new(Provider::new(Vec::new())));
    rig.owner.begin(1).unwrap();
    let mut message = context(1, true, b"");
    message.version = 2;
    assert!(rig.handle.receive_context(&message).is_err());
    message.version = 1;
    message.message_id.clear();
    assert!(rig.handle.receive_context(&message).is_err());
    assert!(rig.acquire().now_or_never().is_none());
}

#[tokio::test]
async fn remote_error_before_establishment_prevents_provider_acquisition() {
    let provider = Arc::new(Provider::new(Vec::new()));
    let mut rig = PrimaryRig::new(true, provider.clone());
    rig.owner.begin(1).unwrap();
    rig.ack(1, 1, 0, 42);
    assert!(matches!(
        rig.acquire().await,
        Err(EstablishError::Remote(42))
    ));
    assert_eq!(provider.state_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn get_copy_state_failure_is_acquisition_fault_not_end_get_next() {
    let mut provider = Provider::new(Vec::new());
    provider.state_error = true;
    let mut rig = PrimaryRig::new(false, Arc::new(provider));
    rig.owner.begin(1).unwrap();
    rig.ack(1, 1, 0, 0);
    for _ in 0..2 {
        assert!(matches!(
            rig.acquire().await,
            Err(EstablishError::Provider {
                description: "GetCopyState",
                ..
            })
        ));
    }
    assert!(rig.session.start_copy_deadline().is_none());
}

#[tokio::test]
async fn cancel_before_begin_is_not_successful_establishment() {
    let mut rig = PrimaryRig::new(true, Arc::new(Provider::new(Vec::new())));
    rig.handle.cancel().unwrap();
    assert!(matches!(rig.owner.begin(1), Err(EstablishError::Cancelled)));
    assert!(matches!(
        rig.acquire().await,
        Err(EstablishError::Cancelled)
    ));
    assert!(rig.sends.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancel_unblocks_provider_consuming_incoming_context() {
    let provider = Arc::new(Provider::new(Vec::new()));
    let mut rig = PrimaryRig::new(true, provider.clone());
    rig.owner.begin(1).unwrap();
    rig.handle
        .receive_context(&context(1, false, b"target"))
        .unwrap();
    assert!(rig.acquire().now_or_never().is_none());
    rig.handle.cancel().unwrap();
    assert!(matches!(
        rig.acquire().await,
        Err(EstablishError::Provider {
            description: "GetCopyState",
            ..
        })
    ));
    assert_eq!(provider.state_calls.load(Ordering::SeqCst), 1);
    assert!(rig.session.start_copy_deadline().is_none());
}

#[tokio::test]
async fn interrupted_primary_driver_retains_one_acquisition_future() {
    let provider = Arc::new(Provider::new(Vec::new()));
    let permit = provider.state_gate.acquire().await.unwrap();
    let mut rig = PrimaryRig::new(false, provider.clone());
    rig.owner.begin(8).unwrap();
    rig.ack(7, 7, 0, 0);
    assert!(rig.acquire().now_or_never().is_none());
    assert!(rig.acquire().now_or_never().is_none());
    assert_eq!(provider.state_calls.load(Ordering::SeqCst), 1);
    drop(permit);
    rig.acquire().await.unwrap();
}

#[tokio::test]
async fn cancel_does_not_claim_pending_primary_acquisition_has_drained() {
    let provider = Arc::new(Provider::new(Vec::new()));
    let permit = provider.state_gate.acquire().await.unwrap();
    let mut rig = PrimaryRig::new(false, provider.clone());
    rig.owner.begin(8).unwrap();
    rig.ack(7, 7, 0, 0);
    assert!(rig.acquire().now_or_never().is_none());
    rig.handle.cancel().unwrap();
    assert!(rig.acquire().now_or_never().is_none());
    drop(permit);
    assert!(matches!(
        rig.acquire().await,
        Err(EstablishError::Cancelled)
    ));
    assert_eq!(provider.state_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn secondary_initializes_actual_replication_receiver_once() {
    let provider = Arc::new(Provider::new(Vec::new()));
    let secondary = secondary(true, provider.clone());
    let handle = secondary.handle();
    assert!(handle.process_start(&start(Epoch::new(1, 7), 8)).unwrap());
    assert!(!handle.process_start(&start(Epoch::new(1, 7), 99)).unwrap());
    let root = handle.root().unwrap();
    let state = root.state.lock().unwrap();
    assert_eq!(state.receiver.queue().unwrap().progress().complete_head, 8);
    assert_eq!(
        state.receiver.minimum_sending_epoch(),
        Some(Epoch::new(1, 7))
    );
    assert_eq!(provider.context_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn duplicate_start_reissues_ack_without_reacquiring_context() {
    let provider = Arc::new(Provider::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let mut secondary = SecondaryEstablishment::new(
        route().reversed(),
        receiver_config(true),
        config(),
        provider.clone(),
        Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        }),
        Clock::new(),
    )
    .unwrap();
    let handle = secondary.handle();
    handle.process_start(&start(Epoch::new(1, 7), 8)).unwrap();
    secondary.acquire_context(Arc::new(|_| true)).await.unwrap();
    handle.process_start(&start(Epoch::new(1, 7), 8)).unwrap();
    secondary.acquire_context(Arc::new(|_| true)).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(provider.context_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn older_start_is_rejected_and_newer_start_faults_without_restart() {
    let secondary = secondary(true, Arc::new(Provider::new(Vec::new())));
    let handle = secondary.handle();
    handle.process_start(&start(Epoch::new(1, 7), 8)).unwrap();
    assert!(matches!(
        handle.process_start(&start(Epoch::new(1, 6), 8)),
        Err(EstablishError::StaleEpoch)
    ));
    assert!(matches!(
        handle.process_start(&start(Epoch::new(2, 0), 8)),
        Err(EstablishError::UnexpectedEpoch)
    ));
    assert_eq!(
        handle.root().unwrap().state.lock().unwrap().epoch,
        Some(Epoch::new(1, 7))
    );
}

#[tokio::test]
async fn invalid_start_replica_epoch_and_boundary_do_not_initialize() {
    let secondary = secondary(true, Arc::new(Provider::new(Vec::new())));
    let handle = secondary.handle();
    let mut message = start(Epoch::new(1, 7), 8);
    let Some(wire::envelope::Body::StartCopy(body)) = message.body.as_mut() else {
        unreachable!()
    };
    body.replica = 21;
    assert!(matches!(
        handle.process_start(&message),
        Err(EstablishError::WrongReplica)
    ));
    for (epoch, lsn) in [
        (Epoch::new(-1, 0), 8),
        (Epoch::new(1, -1), 8),
        (Epoch::new(1, 7), 0),
        (Epoch::new(1, 7), Lsn::MAX),
    ] {
        assert!(matches!(
            handle.process_start(&start(epoch, lsn)),
            Err(EstablishError::InvalidStart)
        ));
    }
    assert!(
        handle
            .root()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .receiver
            .queue()
            .is_none()
    );
}

#[tokio::test]
async fn secondary_rejects_wrong_incarnation_and_sender_before_start() {
    let secondary = secondary(true, Arc::new(Provider::new(Vec::new())));
    for which in 0..2 {
        let mut message = start(Epoch::new(1, 7), 8);
        if which == 0 {
            message.source.as_mut().unwrap().incarnation = Bytes::from(vec![8; 16]);
        } else {
            message.sender_address = "stale primary".into();
        }
        assert!(matches!(
            secondary.handle().process_start(&message),
            Err(EstablishError::Protocol(_))
        ));
    }
}

#[tokio::test]
async fn nonpersisted_target_never_calls_get_copy_context() {
    let provider = Arc::new(Provider::new(Vec::new()));
    let mut secondary = secondary(false, provider.clone());
    assert!(matches!(
        secondary.acquire_context(Arc::new(|_| true)).await,
        Err(EstablishError::NotStarted)
    ));
    secondary
        .handle()
        .process_start(&start(Epoch::new(1, 7), 8))
        .unwrap();
    secondary.acquire_context(Arc::new(|_| true)).await.unwrap();
    assert_eq!(provider.context_calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        secondary.copy().drive().await,
        Err(CopyError::NotStarted)
    ));
}

#[tokio::test]
async fn interrupted_secondary_driver_retains_one_acquisition_future() {
    let provider = Arc::new(Provider::new(Vec::new()));
    let permit = provider.context_gate.acquire().await.unwrap();
    let mut secondary = secondary(true, provider.clone());
    secondary
        .handle()
        .process_start(&start(Epoch::new(1, 7), 8))
        .unwrap();
    for _ in 0..2 {
        assert!(
            secondary
                .acquire_context(Arc::new(|_| true))
                .now_or_never()
                .is_none()
        );
    }
    drop(permit);
    secondary.acquire_context(Arc::new(|_| true)).await.unwrap();
    assert_eq!(provider.context_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn target_context_acquisition_error_forces_error_ack_and_reports_fault() {
    let mut provider = Provider::new(Vec::new());
    provider.context_error = true;
    let acks = Arc::new(Mutex::new(Vec::new()));
    let observed = acks.clone();
    let mut secondary = SecondaryEstablishment::new(
        route().reversed(),
        receiver_config(true),
        config(),
        Arc::new(provider),
        Arc::new(move |error| observed.lock().unwrap().push(error)),
        Clock::new(),
    )
    .unwrap();
    secondary
        .handle()
        .process_start(&start(Epoch::new(1, 7), 8))
        .unwrap();
    assert!(matches!(
        secondary.acquire_context(Arc::new(|_| true)).await,
        Err(EstablishError::Provider {
            description: "GetCopyContext",
            ..
        })
    ));
    assert_eq!(*acks.lock().unwrap(), vec![0, -1]);
}

#[tokio::test]
async fn close_waits_for_pending_target_acquisition_without_beginning_copy() {
    let provider = Arc::new(Provider::new(Vec::new()));
    let permit = provider.context_gate.acquire().await.unwrap();
    let mut secondary = secondary(true, provider.clone());
    secondary
        .handle()
        .process_start(&start(Epoch::new(1, 7), 8))
        .unwrap();
    assert!(
        secondary
            .acquire_context(Arc::new(|_| true))
            .now_or_never()
            .is_none()
    );
    secondary.handle().close().unwrap();
    assert!(
        secondary
            .acquire_context(Arc::new(|_| true))
            .now_or_never()
            .is_none()
    );
    drop(permit);
    assert!(matches!(
        secondary.acquire_context(Arc::new(|_| true)).await,
        Err(EstablishError::Cancelled)
    ));
    assert!(matches!(
        secondary
            .handle()
            .process_start(&start(Epoch::new(1, 7), 8)),
        Err(EstablishError::Cancelled)
    ));
}

#[tokio::test]
async fn ack_callback_reenters_secondary_outside_its_transition_guard() {
    let slot: Arc<Mutex<Option<SecondaryEstablishmentHandle>>> = Arc::new(Mutex::new(None));
    let target = slot.clone();
    let mut secondary = SecondaryEstablishment::new(
        route().reversed(),
        receiver_config(true),
        config(),
        Arc::new(Provider::new(Vec::new())),
        Arc::new(move |_| {
            target.lock().unwrap().clone().unwrap().close().unwrap();
        }),
        Clock::new(),
    )
    .unwrap();
    *slot.lock().unwrap() = Some(secondary.handle());
    secondary
        .handle()
        .process_start(&start(Epoch::new(1, 7), 8))
        .unwrap();
    assert!(matches!(
        secondary.acquire_context(Arc::new(|_| true)).await,
        Err(EstablishError::Cancelled)
    ));
}

#[tokio::test]
async fn context_ack_requires_start_and_bound_primary_identity() {
    let secondary = secondary(true, Arc::new(Provider::new(Vec::new())));
    assert!(matches!(
        secondary.handle().process_context_ack(&context_ack(1, 0)),
        Err(EstablishError::NotStarted)
    ));
    secondary
        .handle()
        .process_start(&start(Epoch::new(1, 7), 8))
        .unwrap();
    let mut message = context_ack(1, 0);
    message.source.as_mut().unwrap().incarnation = Bytes::from(vec![9; 16]);
    assert!(matches!(
        secondary.handle().process_context_ack(&message),
        Err(EstablishError::Protocol(_))
    ));
}

#[tokio::test]
async fn remote_context_error_finishes_actual_context_sender() {
    let mut secondary = secondary(true, Arc::new(Provider::new(Vec::new())));
    secondary
        .handle()
        .process_start(&start(Epoch::new(1, 7), 8))
        .unwrap();
    secondary.acquire_context(Arc::new(|_| true)).await.unwrap();
    assert!(matches!(
        secondary.handle().process_context_ack(&context_ack(0, 37)),
        Err(EstablishError::Remote(37))
    ));
    assert!(matches!(
        secondary.copy().drive().await,
        Err(CopyError::Remote(37))
    ));
    assert!(secondary.copy().handle().end().unwrap().fault.is_none());
}

#[tokio::test]
async fn persisted_target_context_flows_through_actual_receivers_and_senders() {
    let target_provider = Arc::new(Provider::new(vec![
        Bytes::from_static(b"target context"),
        Bytes::new(),
    ]));
    let source_provider = Arc::new(Provider::new(Vec::new()));
    let mut primary = PrimaryRig::new(true, source_provider.clone());
    let mut target = secondary(true, target_provider.clone());
    primary.owner.begin(8).unwrap();
    target
        .handle()
        .process_start(&start(Epoch::new(1, 7), 8))
        .unwrap();
    let incoming = primary.handle.clone();
    target
        .acquire_context(Arc::new(move |action| {
            let CopySendAction::Operation { operation, is_last } = action else {
                panic!("no RequestAck in context");
            };
            incoming
                .receive_context(&context(operation.lsn(), is_last, operation.data()))
                .unwrap();
            true
        }))
        .await
        .unwrap();
    assert!(target.copy().drive().now_or_never().is_none());
    primary.acquire().await.unwrap();
    let terminal = primary
        .context_acks
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .completed;
    assert_eq!(terminal, 3);
    target
        .handle()
        .process_context_ack(&context_ack(terminal, 0))
        .unwrap();
    target.copy().drive().await.unwrap();
    assert!(target.copy().handle().end().unwrap().result.is_ok());
    assert_eq!(target_provider.context_calls.load(Ordering::SeqCst), 1);
    assert_eq!(source_provider.context_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        source_provider.observations.lock().unwrap().states,
        vec![(7, vec![Bytes::from_static(b"target context"), Bytes::new()])]
    );
    assert!(primary.owner.copy().drive().now_or_never().is_none());
    assert_eq!(
        primary
            .owner
            .copy()
            .handle()
            .snapshot()
            .unwrap()
            .replication_target,
        Some(23)
    );
    assert!(matches!(
        primary.owner.end_copy(),
        Err(EstablishError::Copy(CopyError::NotComplete))
    ));
    for lsn in 1..=23 {
        primary
            .session
            .add(
                SenderOperation {
                    operation: QueueOperation::new(lsn, Bytes::from_static(b"replication"), false),
                    original_epoch: Epoch::new(1, 7),
                },
                0,
                Duration::ZERO,
            )
            .dispatch();
    }
    primary.ack(23, 23, -1, 0);
    primary.owner.copy().drive().await.unwrap();
    assert!(primary.owner.end_copy().unwrap().result.is_ok());
}

#[tokio::test]
async fn weak_handles_do_not_keep_abandoned_establishment_alive() {
    let primary = PrimaryRig::new(true, Arc::new(Provider::new(Vec::new())));
    let handle = primary.handle.clone();
    drop(primary);
    assert!(matches!(handle.cancel(), Err(EstablishError::OwnerDropped)));
    let target = secondary(true, Arc::new(Provider::new(Vec::new())));
    let handle = target.handle();
    drop(target);
    assert!(matches!(handle.close(), Err(EstablishError::OwnerDropped)));
}

#[tokio::test(start_paused = true)]
async fn actual_context_ack_deadline_is_driven_and_callback_samples_current_progress() {
    let mut rig = PrimaryRig::new(true, Arc::new(Provider::new(Vec::new())));
    rig.owner.begin(1).unwrap();
    rig.handle
        .receive_context(&context(1, false, b"first"))
        .unwrap();
    assert!(rig.context_acks.lock().unwrap().is_empty());
    assert_eq!(
        rig.handle.context_ack_deadline().unwrap(),
        Some(Duration::from_millis(10))
    );
    rig.handle
        .receive_context(&context(2, false, b"second"))
        .unwrap();
    tokio::time::advance(Duration::from_millis(10)).await;
    rig.handle.context_ack_timer().unwrap();
    assert_eq!(
        rig.context_acks.lock().unwrap().last().unwrap().completed,
        2
    );
    rig.handle.cancel().unwrap();
    assert!(rig.handle.context_ack_deadline().unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn queue_full_does_not_count_as_establishment_or_withdraw_retry() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let sent = attempts.clone();
    let session = session(Arc::new(move |_| sent.fetch_add(1, Ordering::SeqCst) != 0));
    let mut rig = PrimaryRig::with_session(
        true,
        Arc::new(Provider::new(Vec::new())),
        session,
        Arc::default(),
    );
    rig.owner.begin(1).unwrap();
    assert!(rig.acquire().now_or_never().is_none());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(rig.acquire().now_or_never().is_none());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        rig.session.start_copy_deadline(),
        Some(Duration::from_secs(2))
    );
}

#[tokio::test]
async fn remote_zero_progress_failure_still_closes_start_retries() {
    let mut rig = PrimaryRig::new(true, Arc::new(Provider::new(Vec::new())));
    rig.owner.begin(1).unwrap();
    assert!(rig.handle.receive_ack(&ack(0, 0, 0, 37)).unwrap().is_none());
    assert!(matches!(
        rig.acquire().await,
        Err(EstablishError::Remote(37))
    ));
    assert!(rig.session.start_copy_deadline().is_none());
}

#[tokio::test]
async fn malformed_ack_cannot_establish_nonpersisted_copy() {
    let mut rig = PrimaryRig::new(false, Arc::new(Provider::new(Vec::new())));
    rig.owner.begin(1).unwrap();
    for message in [ack(-1, 0, 0, 0), ack(0, 1, 0, 0), ack(1, 1, -2, 0)] {
        assert!(matches!(
            rig.handle.receive_ack(&message),
            Err(EstablishError::Protocol(_))
        ));
    }
    assert!(rig.acquire().now_or_never().is_none());
}

#[tokio::test]
async fn get_copy_state_can_reenter_cancellation_outside_owner_guard() {
    let slot: Arc<Mutex<Option<PrimaryEstablishmentHandle>>> = Arc::default();
    let reenter = slot.clone();
    let mut provider = Provider::new(Vec::new());
    provider.state_hook = Some(Arc::new(move || {
        reenter.lock().unwrap().clone().unwrap().cancel().unwrap();
    }));
    let mut rig = PrimaryRig::new(false, Arc::new(provider));
    *slot.lock().unwrap() = Some(rig.handle.clone());
    rig.owner.begin(1).unwrap();
    rig.ack(1, 1, 0, 0);
    assert!(matches!(
        rig.acquire().await,
        Err(EstablishError::Cancelled)
    ));
    assert!(matches!(
        rig.owner.copy().drive().await,
        Err(CopyError::NotStarted)
    ));
}

#[tokio::test]
async fn get_copy_context_can_reenter_close_outside_owner_guard() {
    let slot: Arc<Mutex<Option<SecondaryEstablishmentHandle>>> = Arc::default();
    let reenter = slot.clone();
    let mut provider = Provider::new(Vec::new());
    provider.context_hook = Some(Arc::new(move || {
        reenter.lock().unwrap().clone().unwrap().close().unwrap();
    }));
    let mut secondary = secondary(true, Arc::new(provider));
    *slot.lock().unwrap() = Some(secondary.handle());
    secondary
        .handle()
        .process_start(&start(Epoch::new(1, 7), 1))
        .unwrap();
    assert!(matches!(
        secondary.acquire_context(Arc::new(|_| true)).await,
        Err(EstablishError::Cancelled)
    ));
}

#[tokio::test]
async fn selected_copy_callbacks_retain_establishment_roots_without_cycles() {
    let mut rig = PrimaryRig::new(false, Arc::new(Provider::new(Vec::new())));
    rig.owner.begin(1).unwrap();
    rig.ack(1, 1, 0, 0);
    rig.acquire().await.unwrap();
    let handle = rig.handle.clone();
    let primary_root = handle.root().unwrap();
    // One owner root, one selected enumeration callback root and this observer.
    assert_eq!(Arc::strong_count(&primary_root), 3);
    drop(rig);
    assert_eq!(Arc::strong_count(&primary_root), 1);
    drop(primary_root);
    assert!(matches!(handle.root(), Err(EstablishError::OwnerDropped)));
    let mut target = secondary(true, Arc::new(Provider::new(Vec::new())));
    let handle = target.handle();
    handle.process_start(&start(Epoch::new(1, 7), 1)).unwrap();
    target.acquire_context(Arc::new(|_| true)).await.unwrap();
    let target_root = handle.root().unwrap();
    assert_eq!(Arc::strong_count(&target_root), 3);
    drop(target);
    assert_eq!(Arc::strong_count(&target_root), 1);
}

#[tokio::test]
async fn cancel_after_copy_begin_finishes_existing_sender_without_provider_fault() {
    let mut rig = PrimaryRig::new(false, Arc::new(Provider::new(Vec::new())));
    rig.owner.begin(1).unwrap();
    rig.ack(1, 1, 0, 0);
    rig.acquire().await.unwrap();
    assert!(rig.owner.copy().drive().now_or_never().is_none());
    rig.handle.cancel().unwrap();
    assert!(matches!(
        rig.owner.copy().drive().await,
        Err(CopyError::Cancelled)
    ));
    let end = rig.owner.end_copy().unwrap();
    assert!(end.fault.is_none());
}

#[tokio::test]
async fn first_establishment_error_cannot_be_replaced_by_late_context() {
    let mut rig = PrimaryRig::new(true, Arc::new(Provider::new(Vec::new())));
    rig.owner.begin(1).unwrap();
    rig.handle.receive_ack(&ack(0, 0, 0, 37)).unwrap();
    rig.handle.receive_context(&context(1, true, b"")).unwrap();
    assert!(matches!(
        rig.acquire().await,
        Err(EstablishError::Remote(37))
    ));
}

#[tokio::test]
async fn framed_context_preserves_provider_representation_and_empty_terminal() {
    let provider = Arc::new(Provider::new(Vec::new()));
    let session = session(Arc::new(|_| true));
    let mut settings = config();
    settings.buffer_policy = BufferPolicy::FramedSegments;
    let mut primary = PrimaryEstablishment::new(
        route(),
        session,
        settings,
        Some(context_config()),
        provider.clone(),
        Arc::new(|_, _| true),
        Clock::new(),
    )
    .unwrap();
    primary.begin(8).unwrap();
    let mut message = context(1, false, b"first");
    let Some(wire::envelope::Body::CopyContextOperation(body)) = message.body.as_mut() else {
        unreachable!()
    };
    body.buffers
        .as_mut()
        .unwrap()
        .segments
        .push(Bytes::from_static(b"second"));
    primary.handle().receive_context(&message).unwrap();
    primary
        .handle()
        .receive_context(&context(2, true, b""))
        .unwrap();
    primary
        .acquire_copy(Arc::new(|_| true), Arc::new(|| Ok(23)))
        .await
        .unwrap();
    let observations = provider.observations.lock().unwrap();
    let data = &observations.states[0].1[0];
    assert_eq!(
        buffers::decode(data.clone(), settings.buffer_policy, settings.buffer_limits).unwrap(),
        vec![Bytes::from_static(b"first"), Bytes::from_static(b"second")]
    );
}
