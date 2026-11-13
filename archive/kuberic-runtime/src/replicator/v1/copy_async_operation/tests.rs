// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-component vectors guided by CopyAsyncOperation.cpp:70-378,
// CopyAsyncOperation.Test.cpp:276-449 and ComProxyAsyncEnumOperation.cpp:90-195,
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d.

use super::*;

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Wake, Waker};

use bytes::Bytes;
use futures::{Stream, StreamExt};

use crate::replicator::v1::buffers;
use crate::replicator::v1::copy_context_receiver::{CopyContextConfig, CopyContextReceiver};
use crate::replicator::v1::operation_stream::StreamEvent;
use crate::replicator::v1::received_operation::ReceivedOperation;
use crate::replicator::v1::secondary_copy_receiver::{SecondaryCopyConfig, SecondaryCopyReceiver};

fn config(capacity: usize) -> CopyConfig {
    CopyConfig {
        sender: SenderConfig {
            initial_window: capacity,
            max_window: capacity,
            retry: Duration::from_secs(1),
            decay_factor: 0.5,
            decay_interval: Duration::from_secs(1),
        },
        buffer_policy: BufferPolicy::RawSingle,
        buffer_limits: BufferLimits {
            max_segments: 16,
            max_payload_bytes: 1024 * 1024,
        },
    }
}

struct Script {
    items: VecDeque<crate::Result<Bytes>>,
    polls: Arc<AtomicUsize>,
    ended: bool,
}

impl Stream for Script {
    type Item = crate::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        assert!(!self.ended, "provider was polled after EOF");
        self.polls.fetch_add(1, Ordering::SeqCst);
        let item = self.items.pop_front();
        self.ended = item.is_none();
        Poll::Ready(item)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Send(Lsn, Bytes, bool),
    RequestAck,
    Last(Lsn),
}

struct Rig {
    operation: CopyAsyncOperation,
    handle: CopyHandle,
    events: Arc<Mutex<Vec<Event>>>,
    polls: Arc<AtomicUsize>,
}

impl Rig {
    fn new(items: impl IntoIterator<Item = crate::Result<Bytes>>, settings: CopyConfig) -> Self {
        let polls = Arc::new(AtomicUsize::new(0));
        let provider = Script {
            items: items.into_iter().collect(),
            polls: polls.clone(),
            ended: false,
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let sends = events.clone();
        let lasts = events.clone();
        let operation = CopyAsyncOperation::new(
            settings,
            Box::pin(provider),
            Arc::new(move |action| {
                sends.lock().unwrap().push(match action {
                    CopySendAction::Operation { operation, is_last } => {
                        Event::Send(operation.lsn(), operation.data().clone(), is_last)
                    }
                    CopySendAction::RequestAck => Event::RequestAck,
                });
                true
            }),
            Arc::new(move |lsn| lasts.lock().unwrap().push(Event::Last(lsn))),
            Clock::new(),
        )
        .unwrap();
        let handle = operation.handle();
        Self {
            operation,
            handle,
            events,
            polls,
        }
    }

    fn data(count: usize, capacity: usize) -> Self {
        Self::new(
            (0..count).map(|_| Ok(Bytes::from_static(b"copy"))),
            config(capacity),
        )
    }

    async fn pending(&mut self) {
        assert!(poll_drive(&mut self.operation).await.is_pending());
    }

    fn snapshot(&self) -> CopySnapshot {
        self.handle.snapshot().unwrap()
    }

    fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
}

async fn poll_drive(operation: &mut CopyAsyncOperation) -> Poll<Result<(), CopyError>> {
    let future = operation.drive();
    tokio::pin!(future);
    futures::poll!(future)
}

#[tokio::test(start_paused = true)]
async fn empty_provider_emits_numbered_eos_and_waits_for_explicit_success_request() {
    let mut rig = Rig::data(0, 4);
    rig.pending().await;
    assert_eq!(rig.polls.load(Ordering::SeqCst), 1);
    assert_eq!(
        rig.events(),
        [Event::Last(1), Event::Send(1, Bytes::new(), true)]
    );
    assert!(!rig.handle.try_complete_with_success_if_copy_done().unwrap());
    rig.handle.update_progress(1, 1, true).unwrap();
    assert!(rig.snapshot().ready_to_complete);
    assert!(rig.snapshot().result.is_none());
    assert!(rig.handle.try_complete_with_success_if_copy_done().unwrap());
    assert!(matches!(
        poll_drive(&mut rig.operation).await,
        Poll::Ready(Ok(()))
    ));
    let end = rig.handle.end().unwrap();
    assert!(end.result.is_ok());
    assert!(end.fault.is_none());
}

#[tokio::test(start_paused = true)]
async fn empty_data_is_an_item_and_provider_is_never_polled_after_eof() {
    let mut rig = Rig::new([Ok(Bytes::new()), Ok(Bytes::from_static(b"x"))], config(4));
    rig.pending().await;
    assert_eq!(rig.polls.load(Ordering::SeqCst), 3);
    assert_eq!(
        rig.events(),
        [
            Event::Send(1, Bytes::new(), false),
            Event::Send(2, Bytes::from_static(b"x"), false),
            Event::Last(3),
            Event::Send(3, Bytes::new(), true),
        ]
    );
    rig.pending().await;
    rig.pending().await;
    assert_eq!(rig.polls.load(Ordering::SeqCst), 3);
    assert!(rig.snapshot().enumeration_done);
}

#[tokio::test(start_paused = true)]
async fn full_queue_retains_exactly_one_parked_item_without_further_polls() {
    let mut rig = Rig::data(15, 4);
    rig.pending().await;
    assert_eq!(rig.snapshot().queued, 4);
    assert_eq!(rig.snapshot().last_admitted, 4);
    assert_eq!(rig.snapshot().parked, Some(5));
    assert_eq!(rig.polls.load(Ordering::SeqCst), 5);
    rig.pending().await;
    rig.handle.on_timer().unwrap().dispatch();
    rig.pending().await;
    assert_eq!(rig.polls.load(Ordering::SeqCst), 5);
}

#[tokio::test(start_paused = true)]
async fn received_ack_stops_resends_but_only_applied_ack_releases_queue() {
    let mut rig = Rig::data(7, 4);
    rig.pending().await;
    rig.handle.update_progress(4, 0, false).unwrap();
    assert_eq!(rig.handle.progress().unwrap().received, 4);
    assert_eq!(rig.handle.progress().unwrap().applied, 0);
    assert_eq!(rig.snapshot().queued, 4);
    assert_eq!(rig.snapshot().parked, Some(5));
    rig.handle.update_progress(4, 2, false).unwrap();
    assert_eq!(rig.snapshot().last_admitted, 5);
    assert_eq!(rig.snapshot().parked, None);
    assert_eq!(
        rig.polls.load(Ordering::SeqCst),
        5,
        "ACK must not poll provider inline"
    );
    rig.pending().await;
    assert_eq!(rig.snapshot().queued, 4);
    assert_eq!(rig.snapshot().parked, Some(7));
    assert_eq!(rig.polls.load(Ordering::SeqCst), 7);
}

#[tokio::test(start_paused = true)]
async fn parked_eof_notifies_last_once_before_admission_and_is_not_reenumerated() {
    let mut rig = Rig::data(4, 4);
    rig.pending().await;
    assert_eq!(rig.snapshot().parked, Some(5));
    assert_eq!(rig.snapshot().terminal, Some(5));
    assert_eq!(rig.events().last(), Some(&Event::Last(5)));
    rig.handle.update_progress(2, 2, false).unwrap();
    assert_eq!(
        rig.events().last(),
        Some(&Event::Send(5, Bytes::new(), true))
    );
    rig.pending().await;
    assert_eq!(rig.polls.load(Ordering::SeqCst), 5);
    assert_eq!(
        rig.events()
            .iter()
            .filter(|event| **event == Event::Last(5))
            .count(),
        1
    );
    rig.handle.update_progress(5, 5, true).unwrap();
    assert!(rig.snapshot().ready_to_complete);
    assert!(rig.snapshot().result.is_none());
}

#[tokio::test(start_paused = true)]
async fn count_queue_grows_to_configured_maximum_without_a_copy_memory_bound() {
    let mut settings = config(2);
    settings.sender.max_window = 8;
    settings.buffer_limits.max_payload_bytes = 2 * 1024 * 1024;
    let mut rig = Rig::new(
        (0..10).map(|_| Ok(Bytes::from(vec![1; 1024 * 1024]))),
        settings,
    );
    rig.pending().await;
    assert_eq!(rig.snapshot().queued, 8);
    assert_eq!(rig.snapshot().parked, Some(9));
    assert_eq!(rig.polls.load(Ordering::SeqCst), 9);
}

#[tokio::test(start_paused = true)]
async fn duplicate_and_regressive_acks_do_not_replay_parked_data_or_regress_progress() {
    let mut rig = Rig::data(8, 4);
    rig.pending().await;
    rig.handle.update_progress(2, 2, false).unwrap();
    rig.handle.update_progress(1, 1, false).unwrap();
    rig.handle.update_progress(2, 2, false).unwrap();
    assert_eq!(rig.snapshot().last_admitted, 5);
    assert_eq!(rig.handle.progress().unwrap().applied, 2);
    assert_eq!(
        rig.events()
            .iter()
            .filter(|event| matches!(event, Event::Send(5, _, _)))
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn provider_error_is_completion_with_end_get_next_fault_not_eof() {
    let mut rig = Rig::new(
        [
            Ok(Bytes::from_static(b"before")),
            Err(RuntimeError::Application("provider failed".into())),
            Ok(Bytes::new()),
        ],
        config(4),
    );
    assert!(matches!(
        poll_drive(&mut rig.operation).await,
        Poll::Ready(Err(CopyError::Provider(_)))
    ));
    assert_eq!(rig.polls.load(Ordering::SeqCst), 2);
    assert_eq!(rig.snapshot().terminal, None);
    let end = rig.handle.end().unwrap();
    assert!(matches!(end.result, Err(CopyError::Provider(_))));
    assert!(
        matches!(end.fault.as_ref().map(|fault| fault.error.as_ref()), Some(RuntimeError::Application(message)) if message == "provider failed")
    );
    assert_eq!(end.fault.unwrap().description, "EndGetNext");
}

#[tokio::test(start_paused = true)]
async fn provider_operation_cancelled_is_not_a_fault_or_successful_eof() {
    let mut rig = Rig::new([Err(RuntimeError::OperationCancelled)], config(4));
    assert!(matches!(
        poll_drive(&mut rig.operation).await,
        Poll::Ready(Err(CopyError::Cancelled))
    ));
    assert!(rig.handle.end().unwrap().fault.is_none());
    assert_eq!(rig.snapshot().terminal, None);
}

#[tokio::test(start_paused = true)]
async fn cancellation_before_first_poll_completes_at_driver_without_calling_provider() {
    let mut rig = Rig::data(3, 4);
    rig.handle.cancel().unwrap();
    assert!(rig.snapshot().cancel_requested);
    assert!(rig.snapshot().result.is_none());
    assert!(matches!(
        poll_drive(&mut rig.operation).await,
        Poll::Ready(Err(CopyError::Cancelled))
    ));
    assert_eq!(rig.polls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn cancel_inflight_provider_waits_for_completion_and_discards_its_data() {
    let (send, receive) = futures::channel::mpsc::unbounded();
    let mut operation = CopyAsyncOperation::new(
        config(4),
        Box::pin(receive),
        Arc::new(|_| panic!("cancelled GetNext must not send")),
        Arc::new(|_| panic!("cancelled GetNext must not fabricate EOF")),
        Clock::new(),
    )
    .unwrap();
    let handle = operation.handle();
    assert!(poll_drive(&mut operation).await.is_pending());
    handle.cancel().unwrap();
    assert!(handle.snapshot().unwrap().provider_in_flight);
    assert!(handle.snapshot().unwrap().result.is_none());
    assert!(poll_drive(&mut operation).await.is_pending());
    send.unbounded_send(Ok(Bytes::from_static(b"late")))
        .unwrap();
    assert!(matches!(
        poll_drive(&mut operation).await,
        Poll::Ready(Err(CopyError::Cancelled))
    ));
    assert_eq!(handle.snapshot().unwrap().queued, 0);
    assert!(!handle.snapshot().unwrap().provider_in_flight);
}

#[tokio::test(start_paused = true)]
async fn inflight_provider_error_after_cancel_preserves_error_and_fault_precedence() {
    let (send, receive) = futures::channel::mpsc::unbounded();
    let mut operation = CopyAsyncOperation::new(
        config(4),
        Box::pin(receive),
        Arc::new(|_| true),
        Arc::new(|_| {}),
        Clock::new(),
    )
    .unwrap();
    let handle = operation.handle();
    assert!(poll_drive(&mut operation).await.is_pending());
    handle.cancel().unwrap();
    send.unbounded_send(Err(RuntimeError::Application("late failure".into())))
        .unwrap();
    assert!(matches!(
        poll_drive(&mut operation).await,
        Poll::Ready(Err(CopyError::Provider(_)))
    ));
    assert!(handle.end().unwrap().fault.is_some());
}

#[tokio::test(start_paused = true)]
async fn cancel_from_nonfull_send_callback_is_requested_before_driver_completion() {
    let slot = Arc::new(OnceLock::<CopyHandle>::new());
    let callback_slot = slot.clone();
    let mut operation = CopyAsyncOperation::new(
        config(4),
        Box::pin(futures::stream::iter([Ok(Bytes::new()), Ok(Bytes::new())])),
        Arc::new(move |_| {
            let handle = callback_slot.get().unwrap();
            handle.cancel().unwrap();
            let snapshot = handle.snapshot().unwrap();
            assert!(snapshot.cancel_requested);
            assert!(snapshot.result.is_none());
            true
        }),
        Arc::new(|_| panic!("must stop before EOF")),
        Clock::new(),
    )
    .unwrap();
    let handle = operation.handle();
    slot.set(handle.clone()).ok().unwrap();
    assert!(matches!(
        poll_drive(&mut operation).await,
        Poll::Ready(Err(CopyError::Cancelled))
    ));
    assert!(handle.end().unwrap().fault.is_none());
}

#[tokio::test(start_paused = true)]
async fn cancel_when_full_completes_immediately_without_an_extra_provider_poll() {
    let mut rig = Rig::data(15, 4);
    rig.pending().await;
    rig.handle.cancel().unwrap();
    assert!(matches!(
        rig.snapshot().result,
        Some(Err(CopyError::Cancelled))
    ));
    assert!(matches!(
        poll_drive(&mut rig.operation).await,
        Poll::Ready(Err(CopyError::Cancelled))
    ));
    assert_eq!(rig.polls.load(Ordering::SeqCst), 5);
    assert!(rig.handle.end().unwrap().fault.is_none());
}

#[tokio::test(start_paused = true)]
async fn cancellation_in_eof_callback_is_immediate_and_end_closes_sender_before_add() {
    let slot = Arc::new(OnceLock::<CopyHandle>::new());
    let callback_slot = slot.clone();
    let mut operation = CopyAsyncOperation::new(
        config(4),
        Box::pin(futures::stream::empty()),
        Arc::new(|_| panic!("End in EOF callback must close before sender Add")),
        Arc::new(move |lsn| {
            assert_eq!(lsn, 1);
            let handle = callback_slot.get().unwrap();
            handle.cancel().unwrap();
            assert!(matches!(
                handle.snapshot().unwrap().result,
                Some(Err(CopyError::Cancelled))
            ));
            assert!(matches!(
                handle.end().unwrap().result,
                Err(CopyError::Cancelled)
            ));
        }),
        Clock::new(),
    )
    .unwrap();
    slot.set(operation.handle()).ok().unwrap();
    assert!(matches!(
        poll_drive(&mut operation).await,
        Poll::Ready(Err(CopyError::Cancelled))
    ));
}

#[tokio::test(start_paused = true)]
async fn cancel_after_parked_eof_is_immediate_and_cannot_be_replaced_by_success() {
    let mut rig = Rig::data(4, 4);
    rig.pending().await;
    rig.handle.cancel().unwrap();
    assert!(matches!(
        rig.snapshot().result,
        Some(Err(CopyError::Cancelled))
    ));
    rig.handle.update_progress(4, 4, false).unwrap();
    rig.handle.update_progress(5, 5, true).unwrap();
    assert!(!rig.handle.try_complete_with_success_if_copy_done().unwrap());
    assert!(matches!(
        rig.handle.end().unwrap().result,
        Err(CopyError::Cancelled)
    ));
}

#[tokio::test(start_paused = true)]
async fn final_ack_marks_ready_but_never_completes_a_missing_replication_build_gate() {
    let mut rig = Rig::data(2, 4);
    rig.pending().await;
    rig.handle.update_progress(3, 3, true).unwrap();
    assert!(rig.snapshot().ready_to_complete);
    assert!(rig.snapshot().result.is_none());
    rig.pending().await;
    assert!(matches!(rig.handle.end(), Err(CopyError::NotComplete)));
    rig.handle.on_timer().unwrap().dispatch();
    assert_eq!(rig.events().len(), 4);
}

#[tokio::test(start_paused = true)]
async fn ack_of_terminal_without_outer_is_last_does_not_invent_ready_state() {
    let mut rig = Rig::data(1, 4);
    rig.pending().await;
    rig.handle.update_progress(2, 2, false).unwrap();
    assert!(!rig.snapshot().ready_to_complete);
    assert!(!rig.handle.try_complete_with_success_if_copy_done().unwrap());
    rig.handle.update_progress(2, 2, true).unwrap();
    assert!(rig.snapshot().ready_to_complete);
}

#[tokio::test(start_paused = true)]
async fn rejects_wire_sentinels_future_and_inconsistent_acks_without_mutation() {
    let mut rig = Rig::data(8, 4);
    rig.pending().await;
    for (received, applied, last) in [
        (-1, 0, false),
        (0, -1, false),
        (5, 4, false),
        (4, 5, false),
        (1, 2, false),
        (2, 2, true),
    ] {
        assert!(matches!(
            rig.handle.update_progress(received, applied, last),
            Err(CopyError::InvalidAck)
        ));
    }
    assert_eq!(rig.snapshot().last_admitted, 4);
    assert_eq!(rig.snapshot().parked, Some(5));
    assert_eq!(rig.snapshot().queued, 4);
    assert_eq!(rig.handle.progress().unwrap().received, 0);
}

#[test]
fn invalid_copy_config_returns_explicit_error_instead_of_panicking() {
    let mut cases = Vec::new();
    for (initial, maximum) in [(0, 4), (1, 4), (3, 4), (8, 4), (4, 0), (4, 7)] {
        let mut settings = config(4);
        settings.sender.initial_window = initial;
        settings.sender.max_window = maximum;
        cases.push(settings);
    }
    let mut settings = config(4);
    settings.sender.retry = Duration::ZERO;
    cases.push(settings);
    let mut settings = config(4);
    settings.sender.decay_factor = f64::NAN;
    cases.push(settings);
    let mut settings = config(4);
    settings.sender.decay_interval = Duration::ZERO;
    cases.push(settings);
    for settings in cases {
        assert!(matches!(
            CopyAsyncOperation::new(
                settings,
                Box::pin(futures::stream::empty()),
                Arc::new(|_| true),
                Arc::new(|_| {}),
                Clock::new(),
            ),
            Err(CopyError::InvalidConfig)
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn malformed_representation_fails_explicitly_without_eof_or_provider_fault() {
    let mut settings = config(4);
    settings.buffer_policy = BufferPolicy::FramedSegments;
    let mut rig = Rig::new([Ok(Bytes::from_static(b"bad frame"))], settings);
    assert!(matches!(
        poll_drive(&mut rig.operation).await,
        Poll::Ready(Err(CopyError::Buffer(BufferError::Malformed)))
    ));
    assert!(rig.events().is_empty());
    assert_eq!(rig.snapshot().queued, 0);
    assert!(rig.handle.end().unwrap().fault.is_none());
}

#[tokio::test(start_paused = true)]
async fn framed_empty_segments_are_data_and_queue_uses_logical_not_adapter_bytes() {
    let mut settings = config(4);
    settings.buffer_policy = BufferPolicy::FramedSegments;
    let frame = buffers::encode(
        &[Bytes::new(), Bytes::new()],
        settings.buffer_policy,
        settings.buffer_limits,
    )
    .unwrap();
    let mut rig = Rig::new([Ok(frame.clone())], settings);
    rig.pending().await;
    assert_eq!(rig.events()[0], Event::Send(1, frame, false));
    let state = rig.operation.root.state.lock().unwrap();
    assert_eq!(state.queue.total_bytes(), 0);
    assert_eq!(state.queue.get_operation(1).unwrap().logical_bytes(), 0);
    assert_eq!(
        state.queue.get_operation(1).unwrap().representation(),
        BufferPolicy::FramedSegments
    );
}

#[tokio::test(start_paused = true)]
async fn retry_timer_reselects_after_false_send_without_consuming_more_provider_data() {
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let observed = attempts.clone();
    let mut operation = CopyAsyncOperation::new(
        config(2),
        Box::pin(futures::stream::iter((0..4).map(|_| Ok(Bytes::new())))),
        Arc::new(move |action| {
            if let CopySendAction::Operation { operation, .. } = action {
                observed.lock().unwrap().push(operation.lsn());
            }
            false
        }),
        Arc::new(|_| {}),
        Clock::new(),
    )
    .unwrap();
    let handle = operation.handle();
    assert!(poll_drive(&mut operation).await.is_pending());
    assert_eq!(*attempts.lock().unwrap(), [1, 2]);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(poll_drive(&mut operation).await.is_pending());
    assert_eq!(*attempts.lock().unwrap(), [1, 2, 1]);
    assert_eq!(handle.snapshot().unwrap().parked, Some(3));
}

#[tokio::test(start_paused = true)]
async fn receipt_without_apply_drives_request_ack_and_not_fake_copy_completion() {
    let mut rig = Rig::data(6, 4);
    rig.pending().await;
    rig.handle.update_progress(4, 0, false).unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    rig.pending().await;
    assert_eq!(rig.events().last(), Some(&Event::RequestAck));
    assert_eq!(rig.handle.progress().unwrap().received_not_applied, 4);
    assert!(rig.snapshot().result.is_none());
}

#[tokio::test(start_paused = true)]
async fn pending_provider_does_not_block_sender_retries_or_gain_a_second_get_next() {
    let (send, receive) = futures::channel::mpsc::unbounded();
    send.unbounded_send(Ok(Bytes::from_static(b"first")))
        .unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = attempts.clone();
    let mut operation = CopyAsyncOperation::new(
        config(4),
        Box::pin(receive),
        Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            true
        }),
        Arc::new(|_| {}),
        Clock::new(),
    )
    .unwrap();
    let handle = operation.handle();
    assert!(poll_drive(&mut operation).await.is_pending());
    assert!(handle.snapshot().unwrap().provider_in_flight);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(poll_drive(&mut operation).await.is_pending());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(handle.snapshot().unwrap().provider_in_flight);
    assert_eq!(handle.snapshot().unwrap().last_admitted, 1);
}

#[tokio::test(start_paused = true)]
async fn end_requires_completion_is_idempotent_and_preserves_cancellation_fault_state() {
    let mut rig = Rig::data(7, 4);
    rig.pending().await;
    assert!(matches!(rig.handle.end(), Err(CopyError::NotComplete)));
    rig.handle.cancel().unwrap();
    for _ in 0..2 {
        let end = rig.handle.end().unwrap();
        assert!(matches!(end.result, Err(CopyError::Cancelled)));
        assert!(end.fault.is_none());
    }
    let count = rig.events().len();
    tokio::time::advance(Duration::from_secs(2)).await;
    rig.handle.on_timer().unwrap().dispatch();
    assert_eq!(rig.events().len(), count);
    assert!(matches!(
        rig.handle.update_progress(1, 1, false),
        Err(CopyError::Ended)
    ));
}

#[tokio::test(start_paused = true)]
async fn progress_and_latency_views_reuse_actual_sender_and_keep_receive_apply_distinct() {
    let mut rig = Rig::data(6, 4);
    rig.pending().await;
    tokio::time::advance(Duration::from_millis(100)).await;
    rig.handle.update_progress(2, 1, false).unwrap();
    let progress = rig.handle.progress().unwrap();
    assert_eq!((progress.received, progress.applied), (2, 1));
    assert_eq!(progress.last_ack_at, Some(Duration::from_millis(100)));
    assert_eq!(progress.received_not_applied, 1);
    assert_eq!(progress.not_received, 3);
    assert_eq!(
        rig.handle.averages().unwrap(),
        (Duration::ZERO, Duration::ZERO)
    );
}

#[tokio::test(start_paused = true)]
async fn selected_retry_batch_roots_owner_and_survives_end_before_dispatch() {
    let mut rig = Rig::data(6, 4);
    rig.pending().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    let selected = rig.handle.on_timer().unwrap();
    rig.handle.cancel().unwrap();
    rig.handle.end().unwrap();
    let before = rig.events().len();
    let events = rig.events.clone();
    let handle = rig.handle.clone();
    drop(rig);
    assert!(
        handle.snapshot().is_ok(),
        "selected batch retains the actual owner"
    );
    selected.dispatch();
    assert!(events.lock().unwrap().len() > before);
    assert!(matches!(handle.snapshot(), Err(CopyError::OwnerDropped)));
}

#[test]
fn weak_handles_do_not_keep_an_abandoned_provider_or_owner_alive() {
    struct Dropped(Arc<AtomicUsize>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let root = Dropped(drops.clone());
    let provider = futures::stream::poll_fn(move |_| {
        let _ = &root;
        Poll::Pending
    });
    let operation = CopyAsyncOperation::new(
        config(4),
        Box::pin(provider),
        Arc::new(|_| true),
        Arc::new(|_| {}),
        Clock::new(),
    )
    .unwrap();
    let handle = operation.handle();
    drop(operation);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(matches!(handle.cancel(), Err(CopyError::OwnerDropped)));
}

#[tokio::test(start_paused = true)]
async fn selected_send_retains_provider_through_owner_release_and_callback_dispatch() {
    struct ProviderRoot(Arc<AtomicUsize>);
    impl Drop for ProviderRoot {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let lifetime = ProviderRoot(drops.clone());
    let mut items = VecDeque::from([Ok(Bytes::new()), Ok(Bytes::new()), Ok(Bytes::new())]);
    let provider = futures::stream::poll_fn(move |_| {
        let _ = &lifetime;
        Poll::Ready(items.pop_front())
    });
    let observed = drops.clone();
    let mut operation = CopyAsyncOperation::new(
        config(2),
        Box::pin(provider),
        Arc::new(move |_| {
            assert_eq!(observed.load(Ordering::SeqCst), 0);
            true
        }),
        Arc::new(|_| {}),
        Clock::new(),
    )
    .unwrap();
    let handle = operation.handle();
    assert!(poll_drive(&mut operation).await.is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    let selected = handle.on_timer().unwrap();
    handle.cancel().unwrap();
    handle.end().unwrap();
    drop(operation);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    selected.dispatch();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(matches!(handle.snapshot(), Err(CopyError::OwnerDropped)));
}

struct ReentrantBytes {
    bytes: Vec<u8>,
    handle: Arc<OnceLock<CopyHandle>>,
    drops: Arc<AtomicUsize>,
}

impl AsRef<[u8]> for ReentrantBytes {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for ReentrantBytes {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.get()
            && let Ok(root) = handle.root()
        {
            assert!(
                root.state.try_lock().is_ok(),
                "payload destructor ran under copy guard"
            );
        }
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

fn reentrant_data(slot: &Arc<OnceLock<CopyHandle>>, drops: &Arc<AtomicUsize>) -> Bytes {
    Bytes::from_owner(ReentrantBytes {
        bytes: vec![1, 2, 3],
        handle: slot.clone(),
        drops: drops.clone(),
    })
}

#[tokio::test(start_paused = true)]
async fn applied_queue_payload_retirement_runs_outside_transition_guard() {
    let slot = Arc::new(OnceLock::new());
    let drops = Arc::new(AtomicUsize::new(0));
    let mut operation = CopyAsyncOperation::new(
        config(4),
        Box::pin(futures::stream::iter([Ok(reentrant_data(&slot, &drops))])),
        Arc::new(|_| true),
        Arc::new(|_| {}),
        Clock::new(),
    )
    .unwrap();
    let handle = operation.handle();
    slot.set(handle.clone()).ok().unwrap();
    assert!(poll_drive(&mut operation).await.is_pending());
    handle.update_progress(1, 1, false).unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn received_ack_releases_sender_only_payload_outside_guard_after_applied_only_ack() {
    let slot = Arc::new(OnceLock::new());
    let drops = Arc::new(AtomicUsize::new(0));
    let mut operation = CopyAsyncOperation::new(
        config(4),
        Box::pin(futures::stream::iter([Ok(reentrant_data(&slot, &drops))])),
        Arc::new(|_| true),
        Arc::new(|_| {}),
        Clock::new(),
    )
    .unwrap();
    let handle = operation.handle();
    slot.set(handle.clone()).ok().unwrap();
    assert!(poll_drive(&mut operation).await.is_pending());
    handle.update_progress(0, 1, false).unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    handle.update_progress(1, 1, false).unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn end_releases_sender_only_payload_outside_guard_and_never_reports_success() {
    let slot = Arc::new(OnceLock::new());
    let drops = Arc::new(AtomicUsize::new(0));
    let mut operation = CopyAsyncOperation::new(
        config(4),
        Box::pin(futures::stream::iter([Ok(reentrant_data(&slot, &drops))])),
        Arc::new(|_| true),
        Arc::new(|_| {}),
        Clock::new(),
    )
    .unwrap();
    let handle = operation.handle();
    slot.set(handle.clone()).ok().unwrap();
    assert!(poll_drive(&mut operation).await.is_pending());
    handle.update_progress(0, 1, false).unwrap();
    handle.cancel().unwrap();
    assert!(matches!(
        handle.end().unwrap().result,
        Err(CopyError::Cancelled)
    ));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn cancelled_inflight_payload_retirement_is_outside_guard() {
    let slot = Arc::new(OnceLock::new());
    let drops = Arc::new(AtomicUsize::new(0));
    let (send, receive) = futures::channel::mpsc::unbounded();
    let mut operation = CopyAsyncOperation::new(
        config(4),
        Box::pin(receive),
        Arc::new(|_| true),
        Arc::new(|_| {}),
        Clock::new(),
    )
    .unwrap();
    let handle = operation.handle();
    slot.set(handle.clone()).ok().unwrap();
    assert!(poll_drive(&mut operation).await.is_pending());
    handle.cancel().unwrap();
    send.unbounded_send(Ok(reentrant_data(&slot, &drops)))
        .unwrap();
    assert!(matches!(
        poll_drive(&mut operation).await,
        Poll::Ready(Err(CopyError::Cancelled))
    ));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

struct ReentrantWake {
    root: Weak<Root>,
    wakes: AtomicUsize,
}

impl Wake for ReentrantWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(root) = self.root.upgrade() {
            assert!(
                root.state.try_lock().is_ok(),
                "driver waker called under transition guard"
            );
        }
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn ack_and_cancel_wake_registered_driver_outside_guard() {
    let mut rig = Rig::data(7, 4);
    let wake = Arc::new(ReentrantWake {
        root: Arc::downgrade(&rig.operation.root),
        wakes: AtomicUsize::new(0),
    });
    let waker = Waker::from(wake.clone());
    let mut context = Context::from_waker(&waker);
    let future = rig.operation.drive();
    tokio::pin!(future);
    assert!(future.as_mut().poll(&mut context).is_pending());
    rig.handle.update_progress(1, 1, false).unwrap();
    rig.handle.cancel().unwrap();
    assert!(wake.wakes.load(Ordering::SeqCst) > 0);
}

#[tokio::test(start_paused = true)]
async fn actual_secondary_copy_receivers_consume_sent_empty_data_and_numbered_terminal() {
    for required in [false, true] {
        let receiver = Arc::new(Mutex::new(
            SecondaryCopyReceiver::new(SecondaryCopyConfig {
                initial_capacity: 4,
                max_capacity: 4,
                require_service_ack: required,
                enable_eos_ack: true,
                copy_done: false,
            })
            .unwrap(),
        ));
        let target = receiver.clone();
        let mut operation = CopyAsyncOperation::new(
            config(4),
            Box::pin(futures::stream::iter([
                Ok(Bytes::new()),
                Ok(Bytes::from_static(b"copy")),
            ])),
            Arc::new(move |action| {
                if let CopySendAction::Operation { operation, is_last } = action {
                    let received = if is_last {
                        ReceivedOperation::copy_end_of_stream(operation.lsn(), required).unwrap()
                    } else {
                        ReceivedOperation::new(
                            QueueOperation::new(operation.lsn(), operation.data().clone(), true),
                            Epoch::new(0, 0),
                            required,
                        )
                    };
                    let effects = target.lock().unwrap().receive(received, is_last);
                    let _ = effects.dispatch().unwrap();
                }
                true
            }),
            Arc::new(|_| {}),
            Clock::new(),
        )
        .unwrap();
        let handle = operation.handle();
        assert!(poll_drive(&mut operation).await.is_pending());
        let mut stream = receiver.lock().unwrap().stream();
        for lsn in 1..=3 {
            let Some(Ok(StreamEvent::Operation(item))) = stream.next().await else {
                panic!("missing actual received copy operation");
            };
            assert_eq!(item.queue_operation().lsn(), lsn);
            assert_eq!(item.is_end_of_stream(), lsn == 3);
            if required {
                let effects = receiver
                    .lock()
                    .unwrap()
                    .process_ack(item.acknowledge().unwrap().unwrap());
                let _ = effects.dispatch().unwrap();
            }
        }
        assert!(stream.next().await.is_none());
        let ack = receiver.lock().unwrap().capture_ack();
        assert_eq!((ack.received, ack.applied), (-1, -1));
        // Test boundary adapter only. Production sentinel/build ownership is pending.
        handle.update_progress(3, 3, true).unwrap();
        assert!(handle.snapshot().unwrap().ready_to_complete);
        assert!(handle.snapshot().unwrap().result.is_none());
    }
}

#[tokio::test(start_paused = true)]
async fn actual_primary_context_receiver_hides_protocol_terminal_and_acknowledges_its_lsn() {
    let receiver = Arc::new(Mutex::new(
        CopyContextReceiver::new(CopyContextConfig {
            initial_queue_size: 4,
            max_queue_size: 4,
            batch_ack_interval: Duration::ZERO,
            retry_interval: Duration::from_secs(1),
            max_pending_acknowledgements: 1,
            trace_interval: Duration::ZERO,
        })
        .unwrap(),
    ));
    let acks = Arc::new(Mutex::new(Vec::new()));
    let observed = acks.clone();
    let send_ack: Arc<dyn Fn(_, _) -> bool + Send + Sync> = Arc::new(move |ack, _| {
        observed.lock().unwrap().push(ack);
        true
    });
    receiver
        .lock()
        .unwrap()
        .open(&send_ack, Duration::ZERO)
        .unwrap();
    let target = receiver.clone();
    let mut operation = CopyAsyncOperation::new(
        config(4),
        Box::pin(futures::stream::iter([
            Ok(Bytes::new()),
            Ok(Bytes::from_static(b"context")),
        ])),
        Arc::new(move |action| {
            if let CopySendAction::Operation { operation, is_last } = action {
                let received = if is_last {
                    ReceivedOperation::copy_end_of_stream(operation.lsn(), false).unwrap()
                } else {
                    ReceivedOperation::new(
                        QueueOperation::new(operation.lsn(), operation.data().clone(), true),
                        Epoch::new(0, 0),
                        false,
                    )
                };
                let effects = target
                    .lock()
                    .unwrap()
                    .process(received, is_last, Duration::ZERO);
                effects.dispatch().unwrap();
            }

            true
        }),
        Arc::new(|_| {}),
        Clock::new(),
    )
    .unwrap();
    let handle = operation.handle();
    assert!(poll_drive(&mut operation).await.is_pending());
    let mut stream = receiver.lock().unwrap().stream();
    assert_eq!(stream.next().await.unwrap().unwrap(), Bytes::new());
    assert_eq!(
        stream.next().await.unwrap().unwrap(),
        Bytes::from_static(b"context")
    );
    assert!(stream.next().await.is_none());
    let ack = *acks.lock().unwrap().last().unwrap();
    assert_eq!(ack.completed, 3);
    assert_eq!(ack.error_code, 0);
    handle.update_progress(3, 3, true).unwrap();
    assert!(handle.snapshot().unwrap().result.is_none());
    let effects = receiver.lock().unwrap().close(0, Duration::ZERO);
    effects.dispatch().unwrap();
}

#[tokio::test(start_paused = true)]
async fn native_twelve_item_drained_success_preserves_last_callback_before_sender_add() {
    let slot = Arc::new(OnceLock::<CopyHandle>::new());
    let sends = slot.clone();
    let lasts = slot.clone();
    let last_count = Arc::new(AtomicUsize::new(0));
    let count = last_count.clone();
    let mut operation = CopyAsyncOperation::new(
        config(8),
        Box::pin(futures::stream::iter((0..12).map(|_| Ok(Bytes::new())))),
        Arc::new(move |action| {
            if let CopySendAction::Operation { operation, .. } = action
                && operation.lsn() < 12
            {
                sends
                    .get()
                    .unwrap()
                    .update_progress(operation.lsn(), operation.lsn(), false)
                    .unwrap();
            }
            true
        }),
        Arc::new(move |lsn| {
            assert_eq!(lsn, 13);
            count.fetch_add(1, Ordering::SeqCst);
            let handle = lasts.get().unwrap();
            handle.update_progress(lsn, lsn, true).unwrap();
            assert!(handle.try_complete_with_success_if_copy_done().unwrap());
            assert!(handle.end().unwrap().result.is_ok());
        }),
        Clock::new(),
    )
    .unwrap();
    slot.set(operation.handle()).ok().unwrap();
    assert!(matches!(
        poll_drive(&mut operation).await,
        Poll::Ready(Ok(()))
    ));
    assert_eq!(last_count.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn native_fifteen_item_queue_eight_ack_below_four_cancels_from_full_retry() {
    let slot = Arc::new(OnceLock::<CopyHandle>::new());
    let callback = slot.clone();
    let polls = Arc::new(AtomicUsize::new(0));
    let provider = Script {
        items: (0..15).map(|_| Ok(Bytes::new())).collect(),
        polls: polls.clone(),
        ended: false,
    };
    let mut operation = CopyAsyncOperation::new(
        config(8),
        Box::pin(provider),
        Arc::new(move |action| {
            if let CopySendAction::Operation { operation, .. } = action {
                let handle = callback.get().unwrap();
                if handle.snapshot().unwrap().parked.is_some() {
                    handle.cancel().unwrap();
                    assert!(matches!(
                        handle.snapshot().unwrap().result,
                        Some(Err(CopyError::Cancelled))
                    ));
                }
                if operation.lsn() < 4 {
                    handle
                        .update_progress(operation.lsn(), operation.lsn(), false)
                        .unwrap();
                    true
                } else {
                    false
                }
            } else {
                true
            }
        }),
        Arc::new(|_| panic!("native queue-full vector never reaches EOF")),
        Clock::new(),
    )
    .unwrap();
    slot.set(operation.handle()).ok().unwrap();
    assert!(poll_drive(&mut operation).await.is_pending());
    assert_eq!(polls.load(Ordering::SeqCst), 12);
    assert_eq!(slot.get().unwrap().snapshot().unwrap().parked, Some(12));
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(matches!(
        poll_drive(&mut operation).await,
        Poll::Ready(Err(CopyError::Cancelled))
    ));
    assert_eq!(polls.load(Ordering::SeqCst), 12);
}

#[tokio::test(start_paused = true)]
async fn real_executor_driver_wakes_after_applied_progress_and_joins_on_full_cancel() {
    let rig = Rig::data(7, 2);
    let handle = rig.handle.clone();
    let polls = rig.polls.clone();
    let mut operation = rig.operation;
    let task = tokio::spawn(async move {
        let result = operation.drive().await;
        (operation, result)
    });
    tokio::task::yield_now().await;
    assert_eq!(handle.snapshot().unwrap().parked, Some(3));
    handle.update_progress(1, 1, false).unwrap();
    assert_eq!(polls.load(Ordering::SeqCst), 3);
    tokio::task::yield_now().await;
    assert_eq!(polls.load(Ordering::SeqCst), 4);
    assert_eq!(handle.snapshot().unwrap().parked, Some(4));
    handle.cancel().unwrap();
    let (_operation, result) = task.await.unwrap();
    assert!(matches!(result, Err(CopyError::Cancelled)));
    assert!(handle.end().unwrap().fault.is_none());
}

#[tokio::test(start_paused = true)]
async fn in_flight_cancel_on_real_executor_waits_for_provider_eof_without_sending_it() {
    let (send, receive) = futures::channel::mpsc::unbounded();
    let mut operation = CopyAsyncOperation::new(
        config(4),
        Box::pin(receive),
        Arc::new(|_| panic!("cancelled provider EOF is not a send")),
        Arc::new(|_| panic!("cancelled provider EOF is not an enumerator callback")),
        Clock::new(),
    )
    .unwrap();
    let handle = operation.handle();
    let task = tokio::spawn(async move {
        let result = operation.drive().await;
        (operation, result)
    });
    tokio::task::yield_now().await;
    assert!(handle.snapshot().unwrap().provider_in_flight);
    handle.cancel().unwrap();
    tokio::task::yield_now().await;
    assert!(!task.is_finished());
    drop(send);
    let (_operation, result) = task.await.unwrap();
    assert!(matches!(result, Err(CopyError::Cancelled)));
    assert!(handle.snapshot().unwrap().terminal.is_none());
}

#[tokio::test(start_paused = true)]
async fn provider_poll_can_reenter_copy_handle_without_a_transition_guard() {
    let slot = Arc::new(OnceLock::<CopyHandle>::new());
    let callback = slot.clone();
    let mut operation = CopyAsyncOperation::new(
        config(4),
        Box::pin(futures::stream::poll_fn(move |_| {
            let root = callback.get().unwrap().root().unwrap();
            assert!(root.state.try_lock().is_ok());
            Poll::Ready(None)
        })),
        Arc::new(|_| true),
        Arc::new(|_| {}),
        Clock::new(),
    )
    .unwrap();
    slot.set(operation.handle()).ok().unwrap();
    assert!(poll_drive(&mut operation).await.is_pending());
    assert_eq!(slot.get().unwrap().snapshot().unwrap().terminal, Some(1));
}

#[tokio::test(start_paused = true)]
async fn malformed_payload_destruction_is_outside_transition_guard() {
    let slot = Arc::new(OnceLock::new());
    let drops = Arc::new(AtomicUsize::new(0));
    let mut settings = config(4);
    settings.buffer_policy = BufferPolicy::FramedSegments;
    let mut operation = CopyAsyncOperation::new(
        settings,
        Box::pin(futures::stream::iter([Ok(reentrant_data(&slot, &drops))])),
        Arc::new(|_| true),
        Arc::new(|_| {}),
        Clock::new(),
    )
    .unwrap();
    slot.set(operation.handle()).ok().unwrap();
    assert!(matches!(
        poll_drive(&mut operation).await,
        Poll::Ready(Err(CopyError::Buffer(BufferError::Malformed)))
    ));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn zero_received_terminal_ack_sets_ready_but_only_end_closes_its_sender() {
    let mut rig = Rig::data(1, 4);
    rig.pending().await;
    rig.handle.update_progress(0, 2, true).unwrap();
    assert!(rig.operation.root.state.lock().unwrap().sender.is_active());
    assert_eq!(rig.snapshot().queued, 0);
    assert!(rig.snapshot().ready_to_complete);
    assert!(rig.handle.try_complete_with_success_if_copy_done().unwrap());
    rig.handle.end().unwrap();
    assert!(!rig.operation.root.state.lock().unwrap().sender.is_active());
}

#[tokio::test(start_paused = true)]
async fn sequence_exhaustion_is_an_explicit_error_not_overflow_or_terminal_eof() {
    let mut rig = Rig::data(1, 4);
    rig.operation.root.state.lock().unwrap().queue = OperationQueue::new(
        QueueConfig {
            initial_capacity: 4,
            max_capacity: 4,
            max_bytes: 0,
            max_completed_count: 0,
            max_completed_bytes: 0,
            clean_on_complete: true,
        },
        Lsn::MAX - 1,
        false,
        true,
    );
    assert!(matches!(
        poll_drive(&mut rig.operation).await,
        Poll::Ready(Err(CopyError::SequenceExhausted))
    ));
    assert_eq!(rig.snapshot().last_admitted, Lsn::MAX - 1);
    assert!(rig.snapshot().terminal.is_none());
    assert!(rig.handle.end().unwrap().fault.is_none());
}
