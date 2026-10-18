// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
// Paused-time tests execute the real V1 cores through their Tokio owners.

use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::time::advance;

use super::*;
use crate::replicator::v1::operation_queue::QueueOperation;
use crate::replicator::v1::session::SendKind;

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn config(window: usize) -> SenderConfig {
    SenderConfig {
        initial_window: window,
        max_window: window,
        retry: ms(10),
        decay_factor: 0.0,
        decay_interval: ms(100),
    }
}

fn identity() -> SessionIdentity {
    SessionIdentity {
        primary_endpoint: "primary/actor".into(),
        remote_endpoint: "secondary/actor".into(),
        remote_incarnation: 7,
    }
}

fn op(lsn: Lsn) -> SenderOperation {
    SenderOperation {
        operation: QueueOperation::new(lsn, Bytes::from_static(b"owned-operation"), true),
        original_epoch: Epoch::new(0, 9),
    }
}

fn ack(received: Lsn, applied: Lsn) -> AckUpdate {
    AckUpdate {
        replication_received: received,
        replication_applied: applied,
        copy_received: 0,
        copy_applied: 0,
        process_copy: false,
        message_id: format!("{received}/{applied}"),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Handoff {
    Gate(Lsn, Lsn),
    Copy(Lsn, Lsn),
}

/// This test owner records delivery obligations, not copy/build success.
struct RecordingOwner(mpsc::UnboundedSender<Handoff>);

impl CopyAckOwner for RecordingOwner {
    fn replication_gate(
        &self,
        copy_applied: Lsn,
        replication_applied: Lsn,
    ) -> Result<(), OwnerError> {
        self.0
            .send(Handoff::Gate(copy_applied, replication_applied))
            .map_err(|error| Box::new(error) as OwnerError)
    }

    fn copy_ack(&self, received: Lsn, applied: Lsn) -> Result<(), OwnerError> {
        self.0
            .send(Handoff::Copy(received, applied))
            .map_err(|error| Box::new(error) as OwnerError)
    }
}

fn session(
    window: usize,
    send: Arc<dyn Fn(SendIntent) -> bool + Send + Sync>,
    clock: Clock,
) -> (SessionRuntime, mpsc::UnboundedReceiver<Handoff>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        SessionRuntime::start(
            identity(),
            Epoch::new(1, 2),
            0,
            config(window),
            send,
            Arc::new(RecordingOwner(tx)),
            clock,
        ),
        rx,
    )
}

fn ack_runtime(clock: Clock) -> AckRuntime {
    AckRuntime::start(ms(5), ms(10), 2, ms(100), clock)
}

#[derive(Debug)]
struct SendObservation {
    at: Duration,
    lsn: Option<Lsn>,
    epoch: Option<Epoch>,
    original: Option<Epoch>,
    hint: Option<Lsn>,
    expiry: Option<Duration>,
}

fn observe(intent: SendIntent, clock: &Clock) -> SendObservation {
    assert_eq!(intent.identity, identity());
    let (lsn, epoch, original, hint) = match intent.kind {
        SendKind::Replication {
            operation,
            original_epoch,
            sending_epoch,
            completed_hint,
        } => (
            Some(operation.lsn()),
            Some(sending_epoch),
            Some(original_epoch),
            Some(completed_hint),
        ),
        SendKind::RequestAck => (None, None, None, None),
    };
    SendObservation {
        at: clock.now(),
        lsn,
        epoch,
        original,
        hint,
        expiry: intent.expires_after,
    }
}

#[tokio::test(start_paused = true)]
async fn retry_boundary_admission_does_not_postpone_and_dispatch_samples_current_epoch() {
    let clock = Clock::new();
    let observed_clock = clock.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (mut runtime, _handoffs) = session(
        2,
        Arc::new(move |intent| {
            tx.send(observe(intent, &observed_clock)).unwrap();
            true
        }),
        clock,
    );
    let handle = runtime.handle();
    // Let the task become idle before admission: the notification is mandatory.
    tokio::task::yield_now().await;
    handle.add(op(1), 4).unwrap();
    let first = rx.recv().await.unwrap();
    assert_eq!((first.at, first.lsn), (ms(0), Some(1)));
    assert_eq!(first.original, Some(Epoch::new(0, 9)));
    assert_eq!(first.expiry, Some(ms(10)));
    advance(ms(5)).await;
    handle.add(op(2), 8).unwrap();
    assert_eq!(rx.recv().await.unwrap().lsn, Some(2));
    assert_eq!(handle.retry_deadline().unwrap(), Some(ms(10)));
    handle.update_sending_epoch(Epoch::new(1, 3)).unwrap();
    advance(ms(4)).await;
    tokio::task::yield_now().await;
    assert!(rx.try_recv().is_err());
    advance(ms(1)).await;
    let retry = rx.recv().await.unwrap();
    assert_eq!((retry.at, retry.lsn), (ms(10), Some(1)));
    assert_eq!(retry.epoch, Some(Epoch::new(1, 3)));
    assert_eq!(retry.original, Some(Epoch::new(0, 9)));
    assert_eq!(retry.hint, Some(8));
    assert!(rx.try_recv().is_err());
    runtime.shutdown().await.unwrap();
    assert_eq!(handle.progress(0), Err(AccessError::Stopped));
}

#[tokio::test(start_paused = true)]
async fn received_ack_retires_retry_apply_pending_requests_ack_only_on_timer_then_disables() {
    let clock = Clock::new();
    let observed_clock = clock.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (mut runtime, mut handoffs) = session(
        1,
        Arc::new(move |intent| {
            tx.send(observe(intent, &observed_clock)).unwrap();
            true
        }),
        clock,
    );
    let handle = runtime.handle();
    handle.add(op(1), 0).unwrap();
    assert_eq!(rx.recv().await.unwrap().lsn, Some(1));
    advance(ms(2)).await;
    handle.update_ack(ack(1, 0), None).unwrap();
    assert_eq!(handoffs.recv().await, Some(Handoff::Gate(0, 0)));
    assert_eq!(handle.progress(0).unwrap().received, 1);
    assert_eq!(handle.retry_deadline().unwrap(), Some(ms(12)));
    assert!(!handle.is_processing().unwrap());
    assert!(rx.try_recv().is_err(), "ACK processing must not RequestAck");
    advance(ms(9)).await;
    tokio::task::yield_now().await;
    assert!(rx.try_recv().is_err());
    advance(ms(1)).await;
    let request = rx.recv().await.unwrap();
    assert_eq!((request.at, request.lsn), (ms(12), None));
    assert_eq!(request.expiry, None);
    handle.update_ack(ack(1, 1), None).unwrap();
    assert_eq!(handoffs.recv().await, Some(Handoff::Gate(0, 1)));
    assert_eq!(handle.retry_deadline().unwrap(), None);
    assert_eq!(handle.progress(0).unwrap().applied, 1);
    advance(ms(30)).await;
    tokio::task::yield_now().await;
    assert!(rx.try_recv().is_err());
    runtime.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn replay_batch_keeps_selected_operations_and_roots_after_reentrant_close() {
    let holder = Arc::new(Mutex::new(None::<SessionHandle>));
    let callback_holder = holder.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let root_lifetime = Arc::new(());
    let weak_lifetime = Arc::downgrade(&root_lifetime);
    let callback_lifetime = root_lifetime.clone();
    let (mut runtime, _handoffs) = session(
        2,
        Arc::new(move |intent| {
            let _keep_rooted = &callback_lifetime;
            let handle = callback_holder.lock().unwrap().clone().unwrap();
            let SendKind::Replication { operation, .. } = intent.kind else {
                panic!("closed sender must not request ACK");
            };
            // Both operations were selected before this first callback closed.
            handle.close().unwrap();
            handle.progress(0).unwrap();
            tx.send(operation.lsn()).unwrap();
            true
        }),
        Clock::new(),
    );
    let handle = runtime.handle();
    *holder.lock().unwrap() = Some(handle.clone());
    handle.add_replay(vec![op(1), op(2)], 0).unwrap();
    drop(root_lifetime);
    assert!(weak_lifetime.upgrade().is_some());
    assert_eq!(rx.recv().await, Some(1));
    assert_eq!(rx.recv().await, Some(2));
    assert_eq!(handle.retry_deadline().unwrap(), None);
    advance(ms(20)).await;
    tokio::task::yield_now().await;
    assert!(rx.try_recv().is_err());
    runtime.shutdown().await.unwrap();
    assert!(weak_lifetime.upgrade().is_none());
}

#[tokio::test(start_paused = true)]
async fn retry_selected_batch_survives_close_during_first_timer_send() {
    let holder = Arc::new(Mutex::new(None::<SessionHandle>));
    let callback_holder = holder.clone();
    let clock = Clock::new();
    let observed_clock = clock.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (mut runtime, _handoffs) = session(
        2,
        Arc::new(move |intent| {
            let event = observe(intent, &observed_clock);
            if event.at == ms(10) {
                callback_holder
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .close()
                    .unwrap();
            }
            tx.send(event).unwrap();
            true
        }),
        clock,
    );
    let handle = runtime.handle();
    *holder.lock().unwrap() = Some(handle.clone());
    handle.add_replay(vec![op(1), op(2)], 0).unwrap();
    assert_eq!(rx.recv().await.unwrap().lsn, Some(1));
    assert_eq!(rx.recv().await.unwrap().lsn, Some(2));
    advance(ms(10)).await;
    assert_eq!(rx.recv().await.unwrap().lsn, Some(1));
    assert_eq!(rx.recv().await.unwrap().lsn, Some(2));
    assert_eq!(handle.retry_deadline().unwrap(), None);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn queue_full_stops_one_native_batch_but_does_not_fabricate_ack_progress() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (mut runtime, _handoffs) = session(
        2,
        Arc::new(move |intent| {
            if let SendKind::Replication { operation, .. } = intent.kind {
                tx.send(operation.lsn()).unwrap();
            }
            false
        }),
        Clock::new(),
    );
    let handle = runtime.handle();
    handle.add_replay(vec![op(1), op(2)], 0).unwrap();
    assert_eq!(rx.recv().await, Some(1));
    assert!(rx.try_recv().is_err());
    assert_eq!(handle.progress(0).unwrap().received, 0);
    advance(ms(10)).await;
    assert_eq!(rx.recv().await, Some(1));
    assert!(rx.try_recv().is_err());
    runtime.shutdown().await.unwrap();
}

struct OrderedOwner {
    events: mpsc::UnboundedSender<String>,
    handle: Arc<Mutex<Option<SessionHandle>>>,
}

impl CopyAckOwner for OrderedOwner {
    fn replication_gate(
        &self,
        copy_applied: Lsn,
        replication_applied: Lsn,
    ) -> Result<(), OwnerError> {
        let handle = self.handle.lock().unwrap().clone().unwrap();
        handle.progress(0).unwrap(); // Reentry must not hold a transition guard.
        self.events
            .send(format!("gate:{copy_applied}/{replication_applied}"))?;
        Ok(())
    }

    fn copy_ack(&self, received: Lsn, applied: Lsn) -> Result<(), OwnerError> {
        self.events.send(format!("copy:{received}/{applied}"))?;
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn ack_processor_handoffs_keep_order_and_drain_reentrant_coalesced_progress_after_close() {
    let holder = Arc::new(Mutex::new(None::<SessionHandle>));
    let callback_holder = holder.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let send_tx = tx.clone();
    let owner = Arc::new(OrderedOwner {
        events: tx.clone(),
        handle: holder.clone(),
    });
    let mut runtime = SessionRuntime::start(
        identity(),
        Epoch::new(1, 2),
        0,
        config(1),
        Arc::new(move |intent| {
            if let SendKind::Replication { operation, .. } = intent.kind {
                send_tx.send(format!("send:{}", operation.lsn())).unwrap();
                if operation.lsn() == 2 {
                    let handle = callback_holder.lock().unwrap().clone().unwrap();
                    let mut latest = ack(2, 1);
                    latest.process_copy = true;
                    latest.copy_received = -1;
                    latest.copy_applied = -1;
                    handle.update_ack(latest, None).unwrap();
                }
            }
            true
        }),
        owner,
        Clock::new(),
    );
    let handle = runtime.handle();
    *holder.lock().unwrap() = Some(handle.clone());
    handle.add_replay(vec![op(1), op(2)], 0).unwrap();
    assert_eq!(rx.recv().await.as_deref(), Some("send:1"));
    let mut first = ack(1, 0);
    first.process_copy = true;
    first.copy_received = 3;
    first.copy_applied = 2;
    let progress_tx = tx.clone();
    let progress_handle = handle.clone();
    handle
        .update_ack(
            first,
            Some(Arc::new(move || {
                progress_handle.progress(0).unwrap();
                progress_tx.send("primary".into()).unwrap();
            })),
        )
        .unwrap();
    let mut events = Vec::new();
    for _ in 0..7 {
        events.push(rx.recv().await.unwrap());
    }
    assert_eq!(
        events,
        [
            "send:2",
            "gate:2/0",
            "primary",
            "copy:3/2",
            "gate:-1/1",
            "primary",
            "copy:-1/-1",
        ]
    );
    assert!(!handle.is_processing().unwrap());
    // Native ACK ingress/processor ownership outlives session close.
    handle.close().unwrap();
    let mut after_close = ack(2, 2);
    after_close.process_copy = true;
    after_close.copy_received = -1;
    after_close.copy_applied = -1;
    handle.update_ack(after_close, None).unwrap();
    assert_eq!(rx.recv().await.as_deref(), Some("gate:-1/1"));
    assert_eq!(rx.recv().await.as_deref(), Some("copy:-1/-1"));
    assert!(!handle.is_processing().unwrap());
    runtime.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn ack_first_deadline_coalesces_latest_payload_and_force_cancels_future_selection() {
    let clock = Clock::new();
    let observed_clock = clock.clone();
    let mut runtime = ack_runtime(clock);
    let handle = runtime.handle();
    let latest_payload = Arc::new(AtomicUsize::new(1));
    let payload = latest_payload.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    handle
        .open(Arc::new(move |trace| {
            tx.send((observed_clock.now(), payload.load(Ordering::SeqCst), trace))
                .unwrap();
            false // Issuance is final even when transport rejects the send.
        }))
        .unwrap();
    assert!(handle.force_on_pending_limit().unwrap());
    tokio::task::yield_now().await;
    handle.schedule(false).unwrap();
    advance(ms(3)).await;
    latest_payload.store(2, Ordering::SeqCst);
    handle.schedule(false).unwrap();
    assert_eq!(handle.deadline().unwrap(), Some(ms(5)));
    advance(ms(1)).await;
    tokio::task::yield_now().await;
    assert!(rx.try_recv().is_err());
    advance(ms(1)).await;
    assert_eq!(rx.recv().await, Some((ms(5), 2, false)));
    assert_eq!(handle.last_issued().unwrap(), Some(ms(5)));
    handle.schedule(false).unwrap();
    advance(ms(2)).await;
    latest_payload.store(3, Ordering::SeqCst);
    handle.schedule(true).unwrap();
    assert_eq!(rx.recv().await, Some((ms(7), 3, false)));
    assert_eq!(handle.deadline().unwrap(), None);
    advance(ms(20)).await;
    tokio::task::yield_now().await;
    assert!(rx.try_recv().is_err());
    handle.close().unwrap();
    assert!(rx.try_recv().is_err(), "no final ACK without pending work");
    runtime.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn ack_close_emits_final_pending_ack_once_and_callbacks_reenter_without_guards() {
    let mut runtime = ack_runtime(Clock::new());
    let handle = runtime.handle();
    let callback_handle = handle.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    handle
        .open(Arc::new(move |trace| {
            assert_eq!(callback_handle.deadline().unwrap(), None);
            callback_handle.schedule(false).unwrap(); // Closed: no new work.
            callback_handle.close().unwrap();
            tx.send(trace).unwrap();
            true
        }))
        .unwrap();
    handle.schedule(false).unwrap();
    handle.close().unwrap();
    handle.close().unwrap();
    assert_eq!(rx.recv().await, Some(true));
    advance(ms(20)).await;
    tokio::task::yield_now().await;
    assert!(rx.try_recv().is_err());
    runtime.shutdown().await.unwrap();
    runtime.join().await.unwrap();
    assert_eq!(handle.schedule(true), Err(AccessError::Stopped));
}

#[tokio::test(start_paused = true)]
async fn actual_ack_timer_callback_reenters_and_arms_the_next_deadline_without_manual_notify() {
    let clock = Clock::new();
    let callback_clock = clock.clone();
    let mut runtime = ack_runtime(clock);
    let handle = runtime.handle();
    let callback_handle = handle.clone();
    let issued = AtomicUsize::new(0);
    let (tx, mut rx) = mpsc::unbounded_channel();
    handle
        .open(Arc::new(move |_| {
            if issued.fetch_add(1, Ordering::SeqCst) == 0 {
                callback_handle.schedule(false).unwrap();
            } else {
                callback_handle.close().unwrap();
            }
            tx.send((callback_clock.now(), callback_handle.deadline().unwrap()))
                .unwrap();
            true
        }))
        .unwrap();
    handle.schedule(false).unwrap();
    advance(ms(5)).await;
    assert_eq!(rx.recv().await, Some((ms(5), Some(ms(10)))));
    advance(ms(5)).await;
    assert_eq!(rx.recv().await, Some((ms(10), None)));
    runtime.shutdown().await.unwrap();
    assert_eq!(rx.recv().await, None);
}

#[tokio::test(start_paused = true)]
async fn ack_selected_force_survives_close_and_reads_payload_at_dispatch() {
    let mut runtime = ack_runtime(Clock::new());
    let handle = runtime.handle();
    let payload = Arc::new(AtomicUsize::new(1));
    let callback_payload = payload.clone();
    let root = Arc::new(());
    let weak = Arc::downgrade(&root);
    let callback_root = root.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    handle
        .open(Arc::new(move |trace| {
            let _root = &callback_root;
            tx.send((callback_payload.load(Ordering::SeqCst), trace))
                .unwrap();
            true
        }))
        .unwrap();
    handle.schedule(true).unwrap();
    handle.close().unwrap();
    payload.store(9, Ordering::SeqCst);
    drop(root);
    assert!(weak.upgrade().is_some());
    assert_eq!(rx.recv().await, Some((9, false)));
    assert!(rx.try_recv().is_err());
    runtime.shutdown().await.unwrap();
    assert!(weak.upgrade().is_none());
}

#[tokio::test(start_paused = true)]
async fn ack_entered_timer_is_not_retracted_by_force_but_close_blocks_fresh_timer_entry() {
    let mut runtime = ack_runtime(Clock::new());
    let handle = runtime.handle();
    let (tx, mut rx) = mpsc::unbounded_channel();
    handle
        .open(Arc::new(move |trace| {
            tx.send(trace).unwrap();
            true
        }))
        .unwrap();
    handle.schedule(false).unwrap();
    // Exercise the exact sleep-completion/native-entry boundary independently
    // of callback delivery. This is the real worker entry, not a fake scheduler.
    handle.schedule(true).unwrap();
    assert_eq!(handle.deadline().unwrap(), None);
    let selected = runtime
        .root
        .as_ref()
        .unwrap()
        .enter_timer()
        .unwrap()
        .unwrap();
    handle.close().unwrap();
    selected.dispatch();
    assert_eq!(rx.recv().await, Some(false));
    assert_eq!(rx.recv().await, Some(false)); // Entered timer and force each run once.
    assert!(
        runtime
            .root
            .as_ref()
            .unwrap()
            .enter_timer()
            .unwrap()
            .is_none()
    ); // Inactive: no effect.
    runtime.shutdown().await.unwrap();
    assert_eq!(rx.recv().await, None);
}

#[tokio::test(start_paused = true)]
async fn ack_timer_selected_before_close_retains_callback_root_until_dispatch() {
    let mut runtime = ack_runtime(Clock::new());
    let handle = runtime.handle();
    let callback_handle = handle.clone();
    let root = Arc::new(());
    let weak = Arc::downgrade(&root);
    let callback_root = root.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    handle
        .open(Arc::new(move |trace| {
            let _root = &callback_root;
            callback_handle.close().unwrap();
            tx.send(trace).unwrap();
            true
        }))
        .unwrap();
    handle.schedule(false).unwrap();
    // A callback that has selected its AckDispatch survives the subsequent close.
    let selected = runtime
        .root
        .as_ref()
        .unwrap()
        .enter_timer()
        .unwrap()
        .unwrap();
    handle.close().unwrap();
    drop(root);
    assert!(weak.upgrade().is_some());
    selected.dispatch();
    assert_eq!(rx.recv().await, Some(false));
    assert!(
        rx.try_recv().is_err(),
        "selection cleared pending final ACK"
    );
    runtime.shutdown().await.unwrap();
    assert!(weak.upgrade().is_none());
}

#[tokio::test(start_paused = true)]
async fn ack_trace_clock_starts_at_open_and_survives_close_reopen_with_strict_boundary() {
    let mut runtime = AckRuntime::start(ms(5), ms(10), 0, ms(6), Clock::new());
    let handle = runtime.handle();
    assert!(!handle.force_on_pending_limit().unwrap());
    advance(ms(20)).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let first_tx = tx.clone();
    handle
        .open(Arc::new(move |trace| {
            first_tx.send(trace).unwrap();
            true
        }))
        .unwrap();
    handle.schedule(true).unwrap();
    assert_eq!(rx.recv().await, Some(false)); // Construction did not start trace.
    advance(ms(6)).await;
    handle.schedule(true).unwrap();
    assert_eq!(rx.recv().await, Some(false)); // Strict >, not >=.
    handle.close().unwrap();
    advance(ms(1)).await;
    handle
        .open(Arc::new(move |trace| {
            tx.send(trace).unwrap();
            true
        }))
        .unwrap();
    handle.schedule(true).unwrap();
    assert_eq!(rx.recv().await, Some(true)); // Reopen did not restart stopwatch.
    handle.set_trace_interval(Duration::ZERO).unwrap();
    handle.schedule(true).unwrap();
    assert_eq!(rx.recv().await, Some(false));
    runtime.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn ack_coalesced_schedule_consumes_trace_interval_without_emitting_an_ack() {
    let mut runtime = AckRuntime::start(ms(5), ms(10), 0, ms(6), Clock::new());
    let handle = runtime.handle();
    let (tx, mut rx) = mpsc::unbounded_channel();
    handle
        .open(Arc::new(move |trace| {
            tx.send(trace).unwrap();
            true
        }))
        .unwrap();
    advance(ms(7)).await;
    handle.schedule(false).unwrap(); // Trace interval consumed, not dispatched.
    assert!(rx.try_recv().is_err());
    advance(ms(5)).await;
    assert_eq!(rx.recv().await, Some(false));
    runtime.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn zero_batch_is_immediate_no_timer_no_pending_final_and_shutdown_drains_selected_work() {
    let mut runtime = AckRuntime::start(ms(0), ms(10), 10, ms(0), Clock::new());
    let handle = runtime.handle();
    let (tx, mut rx) = mpsc::unbounded_channel();
    handle
        .open(Arc::new(move |trace| {
            tx.send(trace).unwrap();
            true
        }))
        .unwrap();
    assert!(!handle.force_on_pending_limit().unwrap());
    handle.schedule(false).unwrap();
    assert_eq!(handle.deadline().unwrap(), None);
    // No worker polling has happened yet: shutdown must drain selected effects.
    runtime.shutdown().await.unwrap();
    assert_eq!(rx.recv().await, Some(false));
    assert_eq!(rx.recv().await, None);
}

struct RejectOwner;

impl CopyAckOwner for RejectOwner {
    fn replication_gate(&self, _: Lsn, _: Lsn) -> Result<(), OwnerError> {
        Err(std::io::Error::other("copy gate owner unavailable").into())
    }

    fn copy_ack(&self, _: Lsn, _: Lsn) -> Result<(), OwnerError> {
        panic!("a failed gate must not be silently continued");
    }
}

#[tokio::test(start_paused = true)]
async fn required_owner_error_is_reported_at_join_not_fabricated_as_copy_success() {
    let mut runtime = SessionRuntime::start(
        identity(),
        Epoch::new(1, 2),
        0,
        config(1),
        Arc::new(|_| true),
        Arc::new(RejectOwner),
        Clock::new(),
    );
    let handle = runtime.handle();
    let mut update = ack(1, 0);
    update.process_copy = true;
    update.copy_received = 1;
    handle.update_ack(update, None).unwrap();
    let error = runtime.join().await.unwrap_err();
    assert!(matches!(error, TaskError::Owner(_)));
    assert!(error.to_string().contains("copy gate owner unavailable"));
    assert_eq!(handle.progress(0), Err(AccessError::Failed));
    assert!(matches!(
        runtime.join().await,
        Err(TaskError::Access(AccessError::Failed))
    ));
}

#[tokio::test(start_paused = true)]
async fn session_callback_panic_is_join_error_and_does_not_strand_a_live_handle_or_root() {
    let root = Arc::new(());
    let weak = Arc::downgrade(&root);
    let callback_root = root.clone();
    let (mut runtime, _handoffs) = session(
        1,
        Arc::new(move |_| {
            let _root = &callback_root;
            panic!("session send failure");
        }),
        Clock::new(),
    );
    drop(root);
    let handle = runtime.handle();
    handle.add(op(1), 0).unwrap();
    let error = runtime.join().await.unwrap_err();
    assert!(matches!(error, TaskError::Join(error) if error.is_panic()));
    assert_eq!(handle.close(), Err(AccessError::Failed));
    assert!(weak.upgrade().is_none());
}

#[tokio::test(start_paused = true)]
async fn ack_callback_panic_is_join_error_even_when_shutdown_drains_final_pending_ack() {
    let mut runtime = ack_runtime(Clock::new());
    let handle = runtime.handle();
    handle
        .open(Arc::new(|_| panic!("ACK owner panic")))
        .unwrap();
    handle.schedule(false).unwrap();
    let error = runtime.shutdown().await.unwrap_err();
    assert!(matches!(error, TaskError::Join(error) if error.is_panic()));
    assert_eq!(handle.schedule(true), Err(AccessError::Failed));
}

#[tokio::test(start_paused = true)]
async fn dropping_unjoined_owner_is_observable_abort_containment_and_releases_weak_handle_cycles() {
    let mut runtime = ack_runtime(Clock::new());
    let handle = runtime.handle();
    let callback_handle = handle.clone();
    let root = Arc::new(());
    let weak = Arc::downgrade(&root);
    let callback_root = root.clone();
    handle
        .open(Arc::new(move |_| {
            let _root = &callback_root;
            callback_handle.schedule(false).unwrap();
            true
        }))
        .unwrap();
    handle.schedule(false).unwrap();
    drop(root);
    // Cancelled join futures do not lose ownership of the JoinHandle.
    {
        let mut join = std::pin::pin!(runtime.join());
        tokio::select! {
            biased;
            result = &mut join => panic!("active worker unexpectedly completed: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
    }
    drop(runtime);
    assert_eq!(handle.schedule(true), Err(AccessError::OwnerDropped));
    // Abort containment is completed when Tokio drops the task future.
    tokio::task::yield_now().await;
    assert!(weak.upgrade().is_none());
}

#[tokio::test(start_paused = true)]
async fn session_owner_drop_releases_pending_operation_and_callback_roots_without_detached_service()
{
    let root = Arc::new(());
    let weak = Arc::downgrade(&root);
    let callback_root = root.clone();
    let (runtime, _handoffs) = session(
        1,
        Arc::new(move |_| {
            let _root = &callback_root;
            true
        }),
        Clock::new(),
    );
    let handle = runtime.handle();
    handle.add(op(1), 0).unwrap();
    drop(root);
    drop(runtime);
    assert_eq!(handle.progress(0), Err(AccessError::OwnerDropped));
    tokio::task::yield_now().await;
    assert!(weak.upgrade().is_none());
}

#[tokio::test(start_paused = true)]
async fn session_staging_full_rejects_before_core_mutation_even_for_empty_batches_then_recovers() {
    let clock = Clock::new();
    let observed_clock = clock.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (mut runtime, mut handoffs) = session(
        2,
        Arc::new(move |intent| {
            tx.send(observe(intent, &observed_clock)).unwrap();
            true
        }),
        clock,
    );
    let handle = runtime.handle();
    handle.add(op(1), 4).unwrap();
    for _ in 1..MAX_STAGED_EFFECTS {
        handle.add(op(1), 4).unwrap(); // Duplicate selection makes an empty batch.
    }
    assert_eq!(
        runtime
            .root
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .effects
            .len(),
        MAX_STAGED_EFFECTS
    );
    assert_eq!(handle.add(op(2), 99), Err(AccessError::StagingFull));
    assert_eq!(
        handle.add_replay(vec![op(2), op(3)], 99),
        Err(AccessError::StagingFull)
    );
    assert_eq!(
        handle.update_ack(ack(1, 0), None),
        Err(AccessError::StagingFull)
    );
    assert!(!handle.is_processing().unwrap());
    let progress = handle.progress(0).unwrap();
    assert_eq!((progress.not_received, progress.received), (1, 0));
    assert_eq!(handle.retry_deadline().unwrap(), Some(ms(10)));
    let first = rx.recv().await.unwrap();
    assert_eq!((first.lsn, first.hint), (Some(1), Some(4)));
    // The rejected ACK did not enter the cached/processing state. Retrying the
    // same ACK must launch and complete the real owner handoff.
    handle.update_ack(ack(1, 0), None).unwrap();
    assert_eq!(handoffs.recv().await, Some(Handoff::Gate(0, 0)));
    assert_eq!(handle.progress(0).unwrap().received, 1);
    handle.add_replay(vec![op(2), op(3)], 8).unwrap();
    assert_eq!(rx.recv().await.unwrap().lsn, Some(2));
    assert_eq!(rx.recv().await.unwrap().lsn, Some(3));
    runtime.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn full_session_staging_still_coalesces_acks_into_the_already_rooted_processor() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (mut runtime, mut handoffs) = session(
        1,
        Arc::new(move |_| {
            tx.send(()).unwrap();
            true
        }),
        Clock::new(),
    );
    let handle = runtime.handle();
    handle.add(op(1), 0).unwrap();
    handle.update_ack(ack(1, 0), None).unwrap();
    for _ in 2..MAX_STAGED_EFFECTS {
        handle.add(op(1), 0).unwrap();
    }
    assert_eq!(
        runtime
            .root
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .effects
            .len(),
        MAX_STAGED_EFFECTS
    );
    handle.update_ack(ack(1, 1), None).unwrap();
    assert_eq!(
        runtime
            .root
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .effects
            .len(),
        MAX_STAGED_EFFECTS,
        "coalescing must not stage a second processor"
    );
    rx.recv().await.unwrap();
    assert_eq!(handoffs.recv().await, Some(Handoff::Gate(0, 0)));
    assert_eq!(handoffs.recv().await, Some(Handoff::Gate(0, 1)));
    assert_eq!(handle.progress(0).unwrap().applied, 1);
    assert!(!handle.is_processing().unwrap());
    assert_eq!(handle.retry_deadline().unwrap(), None);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn ack_staging_reserves_final_close_slot_rejects_force_before_mutation_and_recovers() {
    let mut runtime = ack_runtime(Clock::new());
    let handle = runtime.handle();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let callback: Arc<dyn Fn(bool) -> bool + Send + Sync> = Arc::new(move |trace| {
        tx.send(trace).unwrap();
        true
    });
    handle.open(callback.clone()).unwrap();
    for _ in 0..MAX_STAGED_EFFECTS - 1 {
        handle.schedule(true).unwrap();
    }
    handle.schedule(false).unwrap(); // Coalesced pending work uses no queue slot.
    assert_eq!(handle.deadline().unwrap(), Some(ms(5)));
    assert_eq!(handle.schedule(true), Err(AccessError::StagingFull));
    assert_eq!(
        handle.deadline().unwrap(),
        Some(ms(5)),
        "rejected force must not clear admitted pending work"
    );
    handle.close().unwrap(); // The reserved slot must always admit the final ACK.
    assert_eq!(
        runtime
            .root
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .effects
            .len(),
        MAX_STAGED_EFFECTS
    );
    assert_eq!(handle.open(callback.clone()), Err(AccessError::StagingFull));
    handle.schedule(true).unwrap(); // Ordinary native close remains a no-op.
    handle.close().unwrap();
    for _ in 0..MAX_STAGED_EFFECTS - 1 {
        assert_eq!(rx.recv().await, Some(false));
    }
    assert_eq!(rx.recv().await, Some(true));
    assert!(rx.try_recv().is_err());
    handle.open(callback.clone()).unwrap();
    handle.schedule(true).unwrap();
    assert_eq!(rx.recv().await, Some(false));
    runtime.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn zero_batch_nonforced_ack_uses_the_same_bounded_immediate_admission() {
    let mut runtime = AckRuntime::start(ms(0), ms(10), 0, ms(0), Clock::new());
    let handle = runtime.handle();
    let (tx, mut rx) = mpsc::unbounded_channel();
    handle
        .open(Arc::new(move |_| {
            tx.send(()).unwrap();
            true
        }))
        .unwrap();
    for _ in 0..MAX_STAGED_EFFECTS - 1 {
        handle.schedule(false).unwrap();
    }
    assert_eq!(handle.schedule(false), Err(AccessError::StagingFull));
    for _ in 0..MAX_STAGED_EFFECTS - 1 {
        rx.recv().await.unwrap();
    }
    handle.schedule(false).unwrap();
    rx.recv().await.unwrap();
    runtime.shutdown().await.unwrap();
    assert_eq!(rx.recv().await, None);
}

#[tokio::test(start_paused = true)]
async fn timer_selected_at_full_staging_uses_owned_local_work_and_survives_close() {
    let mut runtime = ack_runtime(Clock::new());
    let handle = runtime.handle();
    let (tx, mut rx) = mpsc::unbounded_channel();
    handle
        .open(Arc::new(move |trace| {
            tx.send(trace).unwrap();
            true
        }))
        .unwrap();
    for _ in 0..MAX_STAGED_EFFECTS - 1 {
        handle.schedule(true).unwrap();
    }
    handle.schedule(false).unwrap();
    // Inject only the native already-entered boundary, as in the race tests:
    // real core selection must not need another staging slot.
    let selected = runtime
        .root
        .as_ref()
        .unwrap()
        .enter_timer()
        .unwrap()
        .unwrap();
    assert_eq!(
        runtime
            .root
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .effects
            .len(),
        MAX_STAGED_EFFECTS - 1
    );
    handle.close().unwrap();
    selected.dispatch();
    runtime.shutdown().await.unwrap();
    for _ in 0..MAX_STAGED_EFFECTS {
        assert_eq!(rx.recv().await, Some(false));
    }
    assert_eq!(rx.recv().await, None);
}

#[tokio::test(start_paused = true)]
async fn session_shutdown_rejects_new_reentrant_ingress_but_drains_selected_sends_and_processor() {
    let holder = Arc::new(Mutex::new(None::<SessionHandle>));
    let callback_holder = holder.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (mut runtime, mut handoffs) = session(
        2,
        Arc::new(move |intent| {
            let handle = callback_holder.lock().unwrap().clone().unwrap();
            assert_eq!(handle.add(op(3), 99), Err(AccessError::ShuttingDown));
            assert_eq!(
                handle.add_replay(vec![op(3), op(4)], 99),
                Err(AccessError::ShuttingDown)
            );
            assert_eq!(
                handle.update_ack(ack(2, 1), None),
                Err(AccessError::ShuttingDown),
                "shutdown must reject even a coalescible ACK"
            );
            handle.progress(0).unwrap();
            handle.close().unwrap();
            if let SendKind::Replication { operation, .. } = intent.kind {
                tx.send(operation.lsn()).unwrap();
            }
            true
        }),
        Clock::new(),
    );
    let handle = runtime.handle();
    *holder.lock().unwrap() = Some(handle.clone());
    handle.add_replay(vec![op(1), op(2)], 0).unwrap();
    handle.update_ack(ack(1, 0), None).unwrap();
    runtime.shutdown().await.unwrap();
    assert_eq!(rx.recv().await, Some(1));
    assert_eq!(rx.recv().await, Some(2));
    assert_eq!(rx.recv().await, None);
    assert_eq!(handoffs.recv().await, Some(Handoff::Gate(0, 0)));
    assert_eq!(handoffs.recv().await, None);
}

#[tokio::test(start_paused = true)]
async fn ack_shutdown_rejects_reentrant_schedule_and_open_but_drains_reserved_final_callback() {
    let mut runtime = ack_runtime(Clock::new());
    let handle = runtime.handle();
    let callback_handle = handle.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    handle
        .open(Arc::new(move |trace| {
            assert_eq!(
                callback_handle.schedule(false),
                Err(AccessError::ShuttingDown)
            );
            assert_eq!(
                callback_handle.schedule(true),
                Err(AccessError::ShuttingDown)
            );
            assert_eq!(
                callback_handle.open(Arc::new(|_| true)),
                Err(AccessError::ShuttingDown)
            );
            assert_eq!(callback_handle.deadline().unwrap(), None);
            callback_handle.close().unwrap();
            tx.send(trace).unwrap();
            true
        }))
        .unwrap();
    for _ in 0..MAX_STAGED_EFFECTS - 1 {
        handle.schedule(true).unwrap();
    }
    handle.schedule(false).unwrap();
    runtime.shutdown().await.unwrap();
    for _ in 0..MAX_STAGED_EFFECTS - 1 {
        assert_eq!(rx.recv().await, Some(false));
    }
    assert_eq!(rx.recv().await, Some(true));
    assert_eq!(rx.recv().await, None);
}
