// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-component witnesses derived from the production sources in secondary.rs.
// These are Rust tests, not execution of the native test binaries.

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Wake, Waker};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Future, FutureExt, StreamExt, stream};
use tokio::sync::Semaphore;

use crate::application::OperationDataStream;
use crate::error::RuntimeError;
use crate::replicator::v1::buffers::{BufferLimits, BufferPolicy};
use crate::replicator::v1::operation_queue::{QueueConfig, QueueOperation};
use crate::replicator::v1::operation_stream::StreamEvent;
use crate::replicator::v1::received_operation::AckState;
use crate::replicator::v1::sender::SenderConfig;

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn config(persisted: bool, required: bool, eos: bool) -> SecondaryConfig {
    SecondaryConfig {
        replication: ReceiverConfig {
            queue: QueueConfig {
                initial_capacity: 4,
                max_capacity: 8,
                max_bytes: 0,
                max_completed_count: 0,
                max_completed_bytes: 0,
                clean_on_complete: false,
            },
            has_persisted_state: persisted,
            require_service_ack: required,
            enable_eos_ack: eos,
        },
        copy: SecondaryCopyConfig {
            initial_capacity: 4,
            max_capacity: 8,
            require_service_ack: persisted || required,
            enable_eos_ack: eos,
            copy_done: false,
        },
        context: CopyConfig {
            sender: SenderConfig {
                initial_window: 4,
                max_window: 8,
                retry: Duration::from_secs(1),
                decay_factor: 0.5,
                decay_interval: Duration::from_secs(1),
            },
            buffer_policy: BufferPolicy::RawSingle,
            buffer_limits: BufferLimits {
                max_segments: 4,
                max_payload_bytes: 1024,
            },
        },
        batch_ack: Duration::from_millis(10),
        retry_ack: Duration::from_secs(1),
        max_pending_acks: 4,
        trace_interval: Duration::ZERO,
    }
}

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

fn start() -> wire::Envelope {
    route().envelope(wire::envelope::Body::StartCopy(wire::StartCopy {
        epoch: Some(wire::Epoch {
            data_loss: 1,
            configuration: 7,
        }),
        replica: 20,
        replication_start: 1,
    }))
}

fn operation(lsn: Lsn, configuration: i64, required: bool) -> ReceivedOperation {
    ReceivedOperation::new(
        QueueOperation::new(lsn, Bytes::from_static(b"data"), true),
        Epoch::new(1, configuration),
        required,
    )
}

struct Provider {
    contexts: AtomicUsize,
    epochs: Mutex<Vec<EpochBarrier>>,
    context_gate: Semaphore,
    epoch_gate: Semaphore,
    fail_context: bool,
    fail_context_next: bool,
    fail_epoch: bool,
    epoch_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Provider {
    fn new() -> Self {
        Self {
            contexts: AtomicUsize::new(0),
            epochs: Mutex::new(Vec::new()),
            context_gate: Semaphore::new(1),
            epoch_gate: Semaphore::new(1),
            fail_context: false,
            fail_context_next: false,
            fail_epoch: false,
            epoch_hook: None,
        }
    }
}

#[async_trait]
impl StateProvider for Provider {
    async fn update_epoch(&self, epoch: Epoch, previous_epoch_last_lsn: Lsn) -> crate::Result<()> {
        self.epochs.lock().unwrap().push(EpochBarrier {
            epoch,
            previous_epoch_last_lsn,
        });
        if let Some(hook) = &self.epoch_hook {
            hook();
        }
        self.epoch_gate.acquire().await.unwrap().forget();
        if self.fail_epoch {
            return Err(RuntimeError::InvalidReplication("epoch failure".into()));
        }
        Ok(())
    }
    async fn get_copy_context(&self) -> crate::Result<OperationDataStream> {
        self.contexts.fetch_add(1, Ordering::SeqCst);
        self.context_gate.acquire().await.unwrap().forget();
        if self.fail_context {
            return Err(RuntimeError::InvalidReplication("context failure".into()));
        }
        let items = if self.fail_context_next {
            vec![Err(RuntimeError::InvalidReplication(
                "context enumeration failure".into(),
            ))]
        } else {
            Vec::new()
        };
        Ok(Box::pin(stream::iter(items)))
    }
    async fn last_committed_lsn(&self) -> crate::Result<Lsn> {
        panic!("secondary lifetime does not query provider commitment")
    }
    async fn get_copy_state(
        &self,
        _: Lsn,
        _: OperationDataStream,
    ) -> crate::Result<OperationDataStream> {
        panic!("secondary must not acquire source copy state")
    }
    async fn on_data_loss(&self) -> crate::Result<bool> {
        panic!("secondary lifetime does not invoke data loss")
    }
}

struct Rig {
    owner: Secondary,
    handle: SecondaryHandle,
    provider: Arc<Provider>,
    acks: Arc<Mutex<Vec<SecondaryAck>>>,
    context_sends: Arc<AtomicUsize>,
    required: bool,
}

impl Rig {
    fn new(config: SecondaryConfig) -> Self {
        Self::with_provider(config, Arc::new(Provider::new()))
    }

    fn with_provider(config: SecondaryConfig, provider: Arc<Provider>) -> Self {
        let acks = Arc::new(Mutex::new(Vec::new()));
        let sent = acks.clone();
        let context_sends = Arc::new(AtomicUsize::new(0));
        let contexts = context_sends.clone();
        let owner = Secondary::new(
            route().reversed(),
            config,
            provider.clone(),
            SecondaryCallbacks {
                ack: Arc::new(move |_, ack, _| {
                    sent.lock().unwrap().push(ack);
                    true
                }),
                context: Arc::new(move |_, _| {
                    contexts.fetch_add(1, Ordering::SeqCst);
                    true
                }),
                fault: Arc::new(|_| {}),
            },
            Clock::new(),
        )
        .unwrap();
        Self {
            handle: owner.handle(),
            owner,
            provider,
            acks,
            context_sends,
            required: config.copy.require_service_ack,
        }
    }

    fn start(&self) {
        assert!(self.handle.process_start(&start()).unwrap());
    }

    fn replicate(&self, lsn: Lsn, epoch: i64) -> ReceivedOperation {
        let operation = operation(lsn, epoch, self.required);
        self.handle
            .receive_replication(Epoch::new(1, epoch), vec![operation.clone()], None)
            .unwrap();
        operation
    }
}

async fn take(stream: &mut OperationStream) -> ReceivedOperation {
    match stream.next().await.unwrap().unwrap() {
        StreamEvent::Operation(operation) => operation,
        _ => panic!("expected operation"),
    }
}

async fn marker(stream: &mut OperationStream) -> EpochBarrier {
    match stream.next().await.unwrap().unwrap() {
        StreamEvent::UpdateEpoch(barrier) => barrier,
        _ => panic!("expected epoch marker"),
    }
}

fn ack_copy(handle: &SecondaryHandle, operation: &ReceivedOperation) {
    if let Some(work) = operation.acknowledge().unwrap() {
        handle.acknowledge_copy(work).unwrap();
    }
}

fn ack_replication(handle: &SecondaryHandle, operation: &ReceivedOperation) {
    if let Some(work) = operation.acknowledge().unwrap() {
        handle.acknowledge_replication(work).unwrap();
    }
}

#[tokio::test]
async fn legacy_required_ack_promotion_rejects_received_but_unacked_copy() {
    let rig = Rig::new(config(true, false, false));
    rig.start();
    let _copy = rig.handle.copy_stream().unwrap();
    rig.handle
        .receive_copy(operation(1, 7, true), false)
        .unwrap();
    rig.handle
        .receive_copy(
            ReceivedOperation::copy_end_of_stream(2, true).unwrap(),
            true,
        )
        .unwrap();
    assert!(matches!(
        rig.handle.close_for_primary(),
        Err(SecondaryError::IncompleteCopy)
    ));
    assert!(
        rig.owner
            .root
            .state
            .lock()
            .unwrap()
            .copy
            .all_operations_received()
    );
}

#[tokio::test]
async fn eos_promotion_accepts_received_copy_but_waits_its_real_ack() {
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    let mut copy = rig.handle.copy_stream().unwrap();
    let mut replication = rig.handle.replication_stream().unwrap();
    rig.handle
        .receive_copy(
            ReceivedOperation::copy_end_of_stream(1, true).unwrap(),
            true,
        )
        .unwrap();
    rig.handle.close_for_primary().unwrap();
    let last = take(&mut copy).await;
    let eos = take(&mut replication).await;
    ack_replication(&rig.handle, &eos);
    assert!(rig.owner.run().now_or_never().is_none());
    ack_copy(&rig.handle, &last);
    assert!(rig.owner.run().await.unwrap().streams_drained);
    rig.owner.take_for_primary().unwrap();
    assert!(matches!(
        rig.owner.take_for_primary(),
        Err(SecondaryError::InvalidTransfer)
    ));
}

#[tokio::test]
async fn promotion_waits_already_running_provider_epoch_without_restarting_it() {
    for fail_epoch in [false, true] {
        let provider = Arc::new(Provider {
            epoch_gate: Semaphore::new(0),
            fail_epoch,
            ..Provider::new()
        });
        let mut rig = Rig::with_provider(config(true, false, false), provider);
        rig.start();
        rig.handle
            .receive_copy(
                ReceivedOperation::copy_end_of_stream(1, true).unwrap(),
                true,
            )
            .unwrap();
        let mut stream = rig.handle.replication_stream().unwrap();
        rig.replicate(1, 8);
        let barrier = marker(&mut stream).await;
        rig.handle.update_stream_epoch(barrier).unwrap();
        assert!(rig.owner.run().now_or_never().is_none());
        rig.handle.close_for_primary().unwrap();
        assert!(rig.owner.run().now_or_never().is_none());
        assert_eq!(rig.provider.epochs.lock().unwrap().len(), 1);
        rig.provider.epoch_gate.add_permits(1);
        assert!(rig.owner.run().now_or_never().is_none());
        let data = take(&mut stream).await;
        ack_replication(&rig.handle, &data);
        assert!(rig.owner.run().await.unwrap().streams_drained);
        if fail_epoch {
            assert!(matches!(
                rig.owner.take_for_primary(),
                Err(SecondaryError::Provider(_))
            ));
        } else {
            rig.owner.take_for_primary().unwrap();
        }
    }
}

#[tokio::test]
async fn secondary_external_epoch_discards_actual_out_of_order_suffix() {
    let mut rig = Rig::new(config(false, false, false));
    rig.start();
    rig.handle
        .receive_copy(
            ReceivedOperation::copy_end_of_stream(1, false).unwrap(),
            true,
        )
        .unwrap();
    rig.replicate(3, 7);
    assert_eq!(
        rig.owner
            .root
            .replication
            .lock()
            .unwrap()
            .queue()
            .unwrap()
            .operation_count(),
        1
    );
    assert!(rig.handle.begin_external_epoch(Epoch::new(1, 8)).unwrap());
    rig.owner.wait_external_epoch().await.unwrap();
    let receiver = rig.owner.root.replication.lock().unwrap();
    assert_eq!(receiver.queue().unwrap().operation_count(), 0);
    assert_eq!(receiver.minimum_sending_epoch(), Some(Epoch::new(1, 8)));
    assert!(rig.provider.epochs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn start_initializes_the_shared_actual_receiver_once() {
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    assert!(!rig.handle.process_start(&start()).unwrap());
    assert_eq!(
        rig.owner
            .root
            .replication
            .lock()
            .unwrap()
            .current_progress()
            .unwrap(),
        0
    );
    assert!(rig.owner.run().now_or_never().is_none());
    assert_eq!(rig.provider.contexts.load(Ordering::SeqCst), 1);
    assert!(rig.owner.run().now_or_never().is_none());
    assert_eq!(rig.provider.contexts.load(Ordering::SeqCst), 1);
    assert_eq!(rig.context_sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn volatile_context_is_not_acquired() {
    let mut rig = Rig::new(config(false, false, true));
    rig.start();
    assert!(rig.owner.run().now_or_never().is_none());
    assert_eq!(rig.provider.contexts.load(Ordering::SeqCst), 0);
    assert_eq!(rig.context_sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn copy_before_start_is_rejected() {
    let rig = Rig::new(config(true, false, true));
    assert!(matches!(
        rig.handle.receive_copy(operation(1, 7, true), false),
        Err(SecondaryError::Receiver(_))
    ));
}

#[tokio::test]
async fn configuration_rejects_mismatched_ack_and_eos_modes() {
    for mismatch in 0..3 {
        let mut settings = config(true, false, true);
        match mismatch {
            0 => settings.copy.require_service_ack = false,
            1 => settings.copy.enable_eos_ack = false,
            _ => settings.batch_ack = settings.retry_ack,
        }
        assert!(matches!(
            Secondary::new(
                route().reversed(),
                settings,
                Arc::new(Provider::new()),
                SecondaryCallbacks {
                    ack: Arc::new(|_, _, _| true),
                    context: Arc::new(|_, _| true),
                    fault: Arc::new(|_| {}),
                },
                Clock::new()
            ),
            Err(SecondaryError::InvalidConfig)
        ));
    }
}

#[tokio::test]
async fn owner_abandonment_is_explicit_and_does_not_root_a_cycle() {
    let rig = Rig::new(config(true, false, true));
    rig.start();
    let operation = rig.replicate(1, 7);
    let mut work = operation.acknowledge().unwrap().unwrap();
    let provider = Arc::downgrade(&rig.provider);
    let handle = rig.handle.clone();
    rig.handle.close().unwrap();
    drop(rig);
    assert!(matches!(handle.close(), Err(SecondaryError::OwnerDropped)));
    assert!(
        provider.upgrade().is_some(),
        "selected cleanup callback must retain the actual provider"
    );
    work.start().unwrap();
    work.finish().unwrap();
    assert!(
        provider.upgrade().is_none(),
        "callback completion must release the provider without a receiver cycle"
    );
}

#[tokio::test]
async fn legacy_close_before_start_is_not_a_stream_drain() {
    let mut rig = Rig::new(config(true, false, false));
    rig.handle.close().unwrap();
    let end = rig.owner.run().await.unwrap();
    assert!(end.result.is_ok() && !end.streams_drained && !end.aborted);
    assert_eq!(rig.provider.contexts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn legacy_close_does_not_wait_for_held_data_ack() {
    let mut rig = Rig::new(config(true, false, false));
    rig.start();
    let mut stream = rig.handle.replication_stream().unwrap();
    let data = rig.replicate(1, 7);
    assert!(take(&mut stream).await.same_instance(&data));
    rig.handle.close().unwrap();
    assert!(rig.owner.run().await.unwrap().result.is_ok());
    assert_eq!(data.ack_state(), AckState::Cancelled);
    assert!(matches!(
        stream.next().await,
        Some(Err(StreamError::Closed))
    ));
}

#[tokio::test]
async fn legacy_close_does_not_wait_for_pending_context_acquisition() {
    let mut provider = Provider::new();
    provider.context_gate = Semaphore::new(0);
    let mut rig = Rig::with_provider(config(true, false, false), Arc::new(provider));
    rig.start();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.handle.close().unwrap();
    assert!(rig.owner.run().await.unwrap().result.is_ok());
    assert!(rig.owner.root.state.lock().unwrap().context_acquiring);
    rig.handle.abort().unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.provider.context_gate.add_permits(1);
    assert!(rig.owner.run().await.unwrap().aborted);
    assert_eq!(rig.provider.contexts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn eos_close_aborts_uninitialized_streams_without_waiting_forever() {
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    rig.handle.close().unwrap();
    assert!(rig.owner.run().await.unwrap().result.is_ok());
    let mut stream = rig.handle.replication_stream().unwrap();
    assert!(matches!(
        stream.next().await,
        Some(Err(StreamError::Closed))
    ));
}

#[tokio::test]
async fn eos_close_waits_for_copy_dispatch_and_its_eos_ack() {
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    let mut copy = rig.handle.copy_stream().unwrap();
    rig.handle
        .receive_copy(operation(1, 7, true), false)
        .unwrap();
    rig.handle.close().unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    let data = take(&mut copy).await;
    assert_eq!(data.ack_state(), AckState::NotCalled);
    ack_copy(&rig.handle, &data);
    let eos = take(&mut copy).await;
    assert!(eos.is_end_of_stream());
    assert!(rig.owner.run().now_or_never().is_none());
    ack_copy(&rig.handle, &eos);
    assert!(rig.owner.run().await.unwrap().streams_drained);
}

#[tokio::test]
async fn incomplete_copy_discards_unpumped_replication_before_eos() {
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    let mut replication = rig.handle.replication_stream().unwrap();
    let data = rig.replicate(1, 7);
    rig.handle.close().unwrap();
    let eos = take(&mut replication).await;
    assert!(eos.is_end_of_stream());
    assert_eq!(data.ack_state(), AckState::Cancelled);
    ack_replication(&rig.handle, &eos);
    assert!(rig.owner.run().await.unwrap().result.is_ok());
}

#[tokio::test]
async fn volatile_eos_close_waits_for_dequeue_not_application_ack() {
    let mut rig = Rig::new(config(false, false, true));
    rig.start();
    let mut copy = rig.handle.copy_stream().unwrap();
    let mut replication = rig.handle.replication_stream().unwrap();
    rig.handle.close().unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    assert!(take(&mut copy).await.is_end_of_stream());
    assert!(rig.owner.run().now_or_never().is_none());
    assert!(take(&mut replication).await.is_end_of_stream());
    assert!(rig.owner.run().await.unwrap().streams_drained);
}

#[tokio::test]
async fn volatile_required_ack_still_waits_for_eos_ack() {
    let mut rig = Rig::new(config(false, true, true));
    rig.start();
    let mut replication = rig.handle.replication_stream().unwrap();
    rig.handle.close().unwrap();
    let eos = take(&mut replication).await;
    assert!(rig.owner.run().now_or_never().is_none());
    ack_replication(&rig.handle, &eos);
    assert!(rig.owner.run().await.unwrap().streams_drained);
}

#[tokio::test]
async fn completed_copy_close_waits_for_remaining_replication_data_ack() {
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    let mut copy = rig.handle.copy_stream().unwrap();
    let last = ReceivedOperation::copy_end_of_stream(1, true).unwrap();
    rig.handle.receive_copy(last, true).unwrap();
    ack_copy(&rig.handle, &take(&mut copy).await);
    let mut replication = rig.handle.replication_stream().unwrap();
    rig.replicate(1, 7);
    let data = take(&mut replication).await;
    rig.handle.close().unwrap();
    let eos = take(&mut replication).await;
    ack_replication(&rig.handle, &eos);
    assert!(rig.owner.run().now_or_never().is_none());
    ack_replication(&rig.handle, &data);
    assert!(rig.owner.run().await.unwrap().streams_drained);
}

#[tokio::test]
async fn selected_data_ack_survives_close_cleanup() {
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    let data = rig.replicate(1, 7);
    let work = data.acknowledge().unwrap().unwrap();
    let mut replication = rig.handle.replication_stream().unwrap();
    rig.handle.close().unwrap();
    rig.handle.acknowledge_replication(work).unwrap();
    assert_eq!(data.ack_state(), AckState::Completed);
    ack_replication(&rig.handle, &take(&mut replication).await);
    assert!(rig.owner.run().await.unwrap().result.is_ok());
}

#[tokio::test]
async fn stream_fault_cleans_both_receivers_but_preserves_eos_ack_wait() {
    let mut provider = Provider::new();
    provider.context_gate = Semaphore::new(0);
    let mut rig = Rig::with_provider(config(true, false, true), Arc::new(provider));
    rig.start();
    assert!(rig.owner.run().now_or_never().is_none());
    let mut copy = rig.handle.copy_stream().unwrap();
    let mut replication = rig.handle.replication_stream().unwrap();
    let data = rig.replicate(1, 7);
    rig.handle
        .receive_copy(operation(1, 7, true), false)
        .unwrap();
    rig.handle.close().unwrap();
    rig.handle
        .report_stream_fault("service failed".into())
        .unwrap();
    let copy_data = take(&mut copy).await;
    assert_eq!(copy_data.ack_state(), AckState::Cancelled);
    assert_eq!(data.ack_state(), AckState::Cancelled);
    let copy_eos = take(&mut copy).await;
    let replication_eos = take(&mut replication).await;
    assert!(rig.owner.run().now_or_never().is_none());
    ack_copy(&rig.handle, &copy_eos);
    let end = rig.owner.run().await.unwrap();
    assert!(end.result.is_err() && end.fault.is_some() && !end.streams_drained);
    // A failed copy drain does not start replication/provider waits. Abort
    // still owns the pending provider call and ignores outstanding EOS ACKs.
    assert_eq!(replication_eos.ack_state(), AckState::NotCalled);
    rig.handle.abort().unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.provider.context_gate.add_permits(1);
    assert!(rig.owner.run().await.unwrap().aborted);
}

#[tokio::test]
async fn stream_fault_rejects_new_data_and_preserves_first_fault() {
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    rig.handle.report_stream_fault("first".into()).unwrap();
    rig.handle.report_stream_fault("second".into()).unwrap();
    assert!(matches!(
        rig.handle
            .receive_replication(Epoch::new(1, 7), vec![operation(1, 7, true)], None),
        Err(SecondaryError::Fault(_))
    ));
    rig.handle.abort().unwrap();
    let end = rig.owner.run().await.unwrap();
    assert!(end.fault.unwrap().to_string().contains("first"));
}

#[tokio::test]
async fn abort_is_not_a_successful_drain_and_is_idempotent() {
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    let mut replication = rig.handle.replication_stream().unwrap();
    rig.replicate(1, 7);
    rig.handle.abort().unwrap();
    rig.handle.abort().unwrap();
    let end = rig.owner.run().await.unwrap();
    assert!(
        end.aborted && !end.streams_drained && matches!(end.result, Err(SecondaryError::Aborted))
    );
    assert!(matches!(
        replication.next().await,
        Some(Err(StreamError::Closed))
    ));
}

#[tokio::test]
async fn abort_waits_for_actual_context_provider_return() {
    let mut provider = Provider::new();
    provider.context_gate = Semaphore::new(0);
    let mut rig = Rig::with_provider(config(true, false, true), Arc::new(provider));
    rig.start();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.handle.abort().unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.provider.context_gate.add_permits(1);
    let end = rig.owner.run().await.unwrap();
    assert!(end.aborted);
    assert!(matches!(
        end.context_result,
        Some(Err(EstablishError::Cancelled))
    ));
    assert_eq!(rig.context_sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn context_provider_faults_preserve_distinct_acquisition_and_enumeration_aftermath() {
    for enumeration in [false, true] {
        let mut provider = Provider::new();
        provider.fail_context = !enumeration;
        provider.fail_context_next = enumeration;
        let mut rig = Rig::with_provider(config(true, false, true), Arc::new(provider));
        rig.start();
        assert!(rig.owner.run().now_or_never().is_none());
        assert_eq!(
            rig.acks.lock().unwrap().iter().any(|ack| ack.error != 0),
            !enumeration
        );
        assert!(matches!(
            rig.handle.process_start(&start()),
            Err(SecondaryError::Fault(_))
        ));
        rig.handle.close().unwrap();
        let end = rig.owner.run().await.unwrap();
        assert!(matches!(
            end.context_result,
            Some(Err(
                EstablishError::Provider { .. } | EstablishError::Copy(CopyError::Provider(_))
            ))
        ));
        assert!(end.fault.is_some());
    }
    // A remote context failure has no provider fault; its negative ACK must
    // remain sticky when duplicate StartCopy requests another ACK.
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    assert!(rig.owner.run().now_or_never().is_none());
    let message = route().envelope(wire::envelope::Body::CopyContextAck(wire::CopyContextAck {
        sequence: 0,
        error: -7,
    }));
    assert!(matches!(
        rig.handle.process_context_ack(&message),
        Err(SecondaryError::Establish(EstablishError::Remote(-7)))
    ));
    assert!(rig.owner.run().now_or_never().is_none());
    assert!(!rig.handle.process_start(&start()).unwrap());
    assert_ne!(rig.acks.lock().unwrap().last().unwrap().error, 0);
    assert!(rig.owner.root.state.lock().unwrap().fault.is_none());
}

#[tokio::test]
async fn stream_marker_invokes_provider_once_and_unblocks_actual_stream() {
    let mut provider = Provider::new();
    provider.epoch_gate = Semaphore::new(0);
    let mut rig = Rig::with_provider(config(true, false, true), Arc::new(provider));
    rig.start();
    let mut replication = rig.handle.replication_stream().unwrap();
    rig.replicate(1, 8);
    let barrier = marker(&mut replication).await;
    assert_eq!(barrier.previous_epoch_last_lsn, 0);
    rig.handle.update_stream_epoch(barrier).unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    assert!(rig.owner.run().now_or_never().is_none());
    assert_eq!(rig.provider.epochs.lock().unwrap().as_slice(), &[barrier]);
    assert!(replication.next().now_or_never().is_none());
    rig.provider.epoch_gate.add_permits(1);
    assert!(rig.owner.run().now_or_never().is_none());
    assert_eq!(take(&mut replication).await.queue_operation().lsn(), 1);
}

#[tokio::test]
async fn full_ack_barrier_runs_only_after_preceding_data_ack() {
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    let first = rig.replicate(1, 7);
    rig.replicate(2, 8);
    assert!(rig.owner.run().now_or_never().is_none());
    assert!(rig.provider.epochs.lock().unwrap().is_empty());
    ack_replication(&rig.handle, &first);
    assert!(rig.owner.run().now_or_never().is_none());
    assert_eq!(
        rig.provider.epochs.lock().unwrap().as_slice(),
        &[EpochBarrier {
            epoch: Epoch::new(1, 8),
            previous_epoch_last_lsn: 1
        }]
    );
}

#[tokio::test]
async fn legacy_close_waits_for_reserved_epoch_even_before_first_provider_poll() {
    let mut provider = Provider::new();
    provider.epoch_gate = Semaphore::new(0);
    let mut rig = Rig::with_provider(config(true, false, false), Arc::new(provider));
    rig.start();
    let first = rig.replicate(1, 7);
    rig.replicate(2, 8);
    ack_replication(&rig.handle, &first);
    rig.handle.close().unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    assert_eq!(rig.provider.epochs.lock().unwrap().len(), 1);
    rig.provider.epoch_gate.add_permits(1);
    assert!(rig.owner.run().await.unwrap().result.is_ok());
}

#[tokio::test]
async fn abort_waits_for_pending_epoch_callback() {
    let mut provider = Provider::new();
    provider.epoch_gate = Semaphore::new(0);
    let mut rig = Rig::with_provider(config(true, false, true), Arc::new(provider));
    rig.start();
    let mut replication = rig.handle.replication_stream().unwrap();
    rig.replicate(1, 8);
    let barrier = marker(&mut replication).await;
    rig.handle.update_stream_epoch(barrier).unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.handle.abort().unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.provider.epoch_gate.add_permits(1);
    assert!(rig.owner.run().await.unwrap().aborted);
}

#[tokio::test]
async fn actual_epoch_failure_is_observable_after_abort() {
    let mut provider = Provider::new();
    provider.epoch_gate = Semaphore::new(0);
    provider.fail_epoch = true;
    let mut rig = Rig::with_provider(config(true, false, true), Arc::new(provider));
    rig.start();
    let mut replication = rig.handle.replication_stream().unwrap();
    rig.replicate(1, 8);
    rig.handle
        .update_stream_epoch(marker(&mut replication).await)
        .unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.handle.abort().unwrap();
    rig.provider.epoch_gate.add_permits(1);
    let end = rig.owner.run().await.unwrap();
    assert!(end.fault.unwrap().to_string().contains("epoch failure"));
}

#[tokio::test]
async fn invalid_or_duplicate_epoch_handoff_does_not_replace_pending_provider() {
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    let mut replication = rig.handle.replication_stream().unwrap();
    rig.replicate(1, 8);
    let barrier = marker(&mut replication).await;
    let wrong = EpochBarrier {
        previous_epoch_last_lsn: 99,
        ..barrier
    };
    assert!(rig.handle.update_stream_epoch(wrong).is_err());
    rig.handle.update_stream_epoch(barrier).unwrap();
    assert!(matches!(
        rig.handle.update_stream_epoch(barrier),
        Err(SecondaryError::EpochPending)
    ));
    assert!(rig.owner.run().now_or_never().is_none());
    assert_eq!(rig.provider.epochs.lock().unwrap().as_slice(), &[barrier]);
}

#[tokio::test(start_paused = true)]
async fn pinned_driver_resamples_new_ack_deadlines_and_snapshots_live_progress() {
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    let mut driver = Box::pin(rig.owner.run());
    assert!(driver.as_mut().now_or_never().is_none());
    let before = rig.acks.lock().unwrap().len();
    rig.handle
        .receive_replication(Epoch::new(1, 7), vec![operation(1, 7, true)], None)
        .unwrap();
    assert!(driver.as_mut().now_or_never().is_none());
    tokio::time::advance(Duration::from_millis(10)).await;
    assert!(driver.as_mut().now_or_never().is_none());
    let acks = rig.acks.lock().unwrap();
    assert_eq!(acks.len(), before + 1);
    assert_eq!(acks.last().unwrap().replication.received, 1);
    assert_eq!(acks.last().unwrap().replication.applied, 0);
}

#[tokio::test]
async fn pinned_close_driver_wakes_on_dispatch_dequeue_and_ack() {
    let mut rig = Rig::new(config(false, true, true));
    rig.start();
    let mut replication = rig.handle.replication_stream().unwrap();
    rig.handle.close().unwrap();
    let mut driver = Box::pin(rig.owner.run());
    assert!(driver.as_mut().now_or_never().is_none());
    let eos = take(&mut replication).await;
    assert!(driver.as_mut().now_or_never().is_none());
    ack_replication(&rig.handle, &eos);
    assert!(driver.await.unwrap().streams_drained);
}

#[tokio::test]
async fn ack_callback_can_reenter_close_without_owner_guard() {
    let config = config(false, false, true);
    let handle_slot = Arc::new(Mutex::new(None::<SecondaryHandle>));
    let callback_handle = handle_slot.clone();
    let calls = AtomicUsize::new(0);
    let mut owner = Secondary::new(
        route().reversed(),
        config,
        Arc::new(Provider::new()),
        SecondaryCallbacks {
            ack: Arc::new(move |_, _, _| {
                let handle = callback_handle.lock().unwrap().as_ref().unwrap().clone();
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    handle.receive_copy(operation(1, 7, false), false).unwrap();
                }
                handle.close().unwrap();
                true
            }),
            context: Arc::new(|_, _| true),
            fault: Arc::new(|_| {}),
        },
        Clock::new(),
    )
    .unwrap();
    let handle = owner.handle();
    *handle_slot.lock().unwrap() = Some(handle.clone());
    handle.process_start(&start()).unwrap();
    assert!(owner.run().await.unwrap().result.is_ok());
}

#[tokio::test]
async fn context_ack_completes_actual_sender_without_an_error_ack() {
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.handle
        .process_context_ack(&route().envelope(wire::envelope::Body::CopyContextAck(
            wire::CopyContextAck {
                sequence: -1,
                error: 0,
            },
        )))
        .unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    assert!(matches!(
        rig.owner.root.state.lock().unwrap().context_result,
        Some(Ok(()))
    ));
    assert!(rig.acks.lock().unwrap().iter().all(|ack| ack.error == 0));
}

#[tokio::test]
async fn copy_completion_cancels_context_sender_without_inventing_a_provider_fault() {
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    let mut copy = rig.handle.copy_stream().unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.handle
        .receive_copy(
            ReceivedOperation::copy_end_of_stream(1, true).unwrap(),
            true,
        )
        .unwrap();
    ack_copy(&rig.handle, &take(&mut copy).await);
    assert!(rig.owner.run().now_or_never().is_none());
    assert!(rig.acks.lock().unwrap().iter().all(|ack| ack.error == 0));
    let mut provider = Provider::new();
    provider.context_gate = Semaphore::new(0);
    provider.fail_context = true;
    let mut rig = Rig::with_provider(config(true, false, true), Arc::new(provider));
    rig.start();
    assert!(rig.owner.run().now_or_never().is_none());
    let mut copy = rig.handle.copy_stream().unwrap();
    rig.handle
        .receive_copy(
            ReceivedOperation::copy_end_of_stream(1, true).unwrap(),
            true,
        )
        .unwrap();
    ack_copy(&rig.handle, &take(&mut copy).await);
    rig.provider.context_gate.add_permits(1);
    assert!(rig.owner.run().now_or_never().is_none());
    assert!(
        rig.acks.lock().unwrap().iter().all(|ack| ack.error == 0),
        "a late context acquisition error must not send copy failure after copy completion"
    );
    assert!(
        rig.owner.root.state.lock().unwrap().fault.is_some(),
        "the actual provider acquisition fault must still be retained"
    );
}

#[tokio::test]
async fn eos_close_waits_for_started_context_acquisition_after_stream_drain() {
    let mut provider = Provider::new();
    provider.context_gate = Semaphore::new(0);
    let mut rig = Rig::with_provider(config(true, false, true), Arc::new(provider));
    rig.start();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.handle.close().unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.provider.context_gate.add_permits(1);
    let end = rig.owner.run().await.unwrap();
    assert!(end.result.is_ok() && end.streams_drained);
    assert!(matches!(
        end.context_result,
        Some(Err(EstablishError::Cancelled))
    ));
}

#[tokio::test]
async fn provider_context_error_keeps_precedence_after_abort() {
    let mut provider = Provider::new();
    provider.context_gate = Semaphore::new(0);
    provider.fail_context = true;
    let mut rig = Rig::with_provider(config(true, false, true), Arc::new(provider));
    rig.start();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.handle.abort().unwrap();
    rig.provider.context_gate.add_permits(1);
    let end = rig.owner.run().await.unwrap();
    assert!(matches!(
        end.context_result,
        Some(Err(EstablishError::Provider {
            description: "GetCopyContext",
            ..
        }))
    ));
}

#[tokio::test]
async fn stream_epoch_failure_reports_fault_but_resumes_dequeue_without_full_barrier_cleanup() {
    let mut provider = Provider::new();
    provider.fail_epoch = true;
    let mut rig = Rig::with_provider(config(true, false, true), Arc::new(provider));
    rig.start();
    let mut replication = rig.handle.replication_stream().unwrap();
    let operation = rig.replicate(1, 8);
    rig.handle
        .update_stream_epoch(marker(&mut replication).await)
        .unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    assert!(take(&mut replication).await.same_instance(&operation));
    assert_eq!(operation.ack_state(), AckState::NotCalled);
    assert!(matches!(
        rig.owner.root.state.lock().unwrap().fault,
        Some(SecondaryError::Provider(_))
    ));
    assert!(!rig.owner.root.state.lock().unwrap().failed_epoch_drain);
}

#[tokio::test]
async fn full_epoch_failure_cleans_both_queues_and_returns_failed_close_without_false_drain() {
    let mut provider = Provider::new();
    provider.fail_epoch = true;
    let mut rig = Rig::with_provider(config(true, false, true), Arc::new(provider));
    rig.start();
    let first = rig.replicate(1, 7);
    let held = rig.replicate(2, 8);
    let copy_data = operation(1, 7, true);
    rig.handle.receive_copy(copy_data.clone(), false).unwrap();
    ack_replication(&rig.handle, &first);
    assert!(rig.owner.run().now_or_never().is_none());
    assert_eq!(held.ack_state(), AckState::Cancelled);
    assert_eq!(copy_data.ack_state(), AckState::Cancelled);
    assert!(
        !rig.owner
            .root
            .replication
            .lock()
            .unwrap()
            .has_pending_epoch(EpochBarrier {
                epoch: Epoch::new(1, 8),
                previous_epoch_last_lsn: 1
            })
    );
    rig.handle.close().unwrap();
    let end = rig.owner.run().await.unwrap();
    assert!(end.result.is_err() && end.fault.is_some() && !end.streams_drained);
}

#[tokio::test]
async fn failed_full_epoch_close_does_not_claim_context_drained_and_abort_still_waits() {
    let mut provider = Provider::new();
    provider.context_gate = Semaphore::new(0);
    provider.epoch_gate = Semaphore::new(0);
    provider.fail_epoch = true;
    let mut rig = Rig::with_provider(config(true, false, true), Arc::new(provider));
    rig.start();
    assert!(rig.owner.run().now_or_never().is_none());
    let first = rig.replicate(1, 7);
    rig.replicate(2, 8);
    ack_replication(&rig.handle, &first);
    assert!(rig.owner.run().now_or_never().is_none());
    rig.handle.close().unwrap();
    rig.provider.epoch_gate.add_permits(1);
    let end = rig.owner.run().await.unwrap();
    assert!(end.result.is_err() && !end.streams_drained);
    assert!(rig.owner.root.state.lock().unwrap().context_acquiring);
    rig.handle.abort().unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.provider.context_gate.add_permits(1);
    assert!(rig.owner.run().await.unwrap().aborted);
}

#[tokio::test]
async fn provider_epoch_can_reenter_abort_without_guard_deadlock() {
    let handle_slot = Arc::new(Mutex::new(None::<SecondaryHandle>));
    let callback = handle_slot.clone();
    let mut provider = Provider::new();
    provider.epoch_hook = Some(Arc::new(move || {
        callback.lock().unwrap().as_ref().unwrap().abort().unwrap()
    }));
    let mut rig = Rig::with_provider(config(true, false, true), Arc::new(provider));
    *handle_slot.lock().unwrap() = Some(rig.handle.clone());
    rig.start();
    let mut replication = rig.handle.replication_stream().unwrap();
    rig.replicate(1, 8);
    rig.handle
        .update_stream_epoch(marker(&mut replication).await)
        .unwrap();
    assert!(rig.owner.run().await.unwrap().aborted);
}

#[tokio::test]
async fn closed_owner_rejects_new_start_and_provider_epoch_requests() {
    let mut rig = Rig::new(config(true, false, true));
    rig.handle.close().unwrap();
    rig.owner.run().await.unwrap();
    assert!(matches!(
        rig.handle.process_start(&start()),
        Err(SecondaryError::Closed)
    ));
    assert!(matches!(
        rig.handle.update_stream_epoch(EpochBarrier {
            epoch: Epoch::new(1, 8),
            previous_epoch_last_lsn: 0,
        }),
        Err(SecondaryError::Closed)
    ));
}

#[tokio::test(start_paused = true)]
async fn pinned_context_driver_keeps_actual_sender_retries_after_acquisition() {
    let mut rig = Rig::new(config(true, false, true));
    rig.start();
    let mut driver = Box::pin(rig.owner.run());
    assert!(driver.as_mut().now_or_never().is_none());
    assert_eq!(rig.context_sends.load(Ordering::SeqCst), 1);
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    let mut context = Context::from_waker(&waker);
    assert!(driver.as_mut().poll(&mut context).is_pending());
    let before = wakes.0.load(Ordering::SeqCst);
    assert!(driver.as_mut().poll(&mut context).is_pending());
    assert_eq!(
        wakes.0.load(Ordering::SeqCst),
        before,
        "cached acquisition must not spin/self-wake a parked enumeration"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(driver.as_mut().now_or_never().is_none());
    assert_eq!(rig.context_sends.load(Ordering::SeqCst), 2);
    assert_eq!(rig.provider.contexts.load(Ordering::SeqCst), 1);
}
