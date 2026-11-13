// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual shared-stream tests derived from the pinned OperationStream sources
// and Rust-specific wake/cancellation adaptations. See sf-v1-port-provenance.md.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Wake;

use bytes::Bytes;
use futures::executor::{LocalPool, block_on};
use futures::future::{AbortHandle, Abortable};
use futures::task::LocalSpawnExt;
use futures::{Future, StreamExt};

use super::*;
use crate::replicator::v1::operation_queue::QueueOperation;

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl WakeCount {
    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

fn operation(lsn: Lsn) -> ReceivedOperation {
    ReceivedOperation::new(
        QueueOperation::new(lsn, Bytes::from_static(b"data"), true),
        Epoch::new(1, 2),
        true,
    )
}

fn barrier() -> EpochBarrier {
    EpochBarrier {
        epoch: Epoch::new(2, 3),
        previous_epoch_last_lsn: 7,
    }
}

fn poll(
    stream: &mut OperationStream,
    wake: &Arc<WakeCount>,
) -> Poll<Option<Result<StreamEvent, StreamError>>> {
    let waker = Waker::from(wake.clone());
    Pin::new(stream).poll_next(&mut Context::from_waker(&waker))
}

fn pending(stream: &mut OperationStream, wake: &Arc<WakeCount>) {
    assert!(poll(stream, wake).is_pending());
}

fn eof(stream: &mut OperationStream, wake: &Arc<WakeCount>) {
    assert!(matches!(poll(stream, wake), Poll::Ready(None)));
}

fn take_operation(result: Poll<Option<Result<StreamEvent, StreamError>>>) -> ReceivedOperation {
    match result {
        Poll::Ready(Some(Ok(StreamEvent::Operation(operation)))) => operation,
        _ => panic!("expected an operation"),
    }
}

fn take_barrier(result: Poll<Option<Result<StreamEvent, StreamError>>>) -> EpochBarrier {
    match result {
        Poll::Ready(Some(Ok(StreamEvent::UpdateEpoch(barrier)))) => barrier,
        _ => panic!("expected an epoch handoff"),
    }
}

fn stream_error(result: Poll<Option<Result<StreamEvent, StreamError>>>) -> StreamError {
    match result {
        Poll::Ready(Some(Err(error))) => error,
        _ => panic!("expected a stream error"),
    }
}

#[test]
fn empty_open_is_pending_and_close_is_permanent_eof() {
    let queue = SharedDispatchQueue::new();
    let mut stream = queue.stream();
    let wake = Arc::new(WakeCount::default());
    pending(&mut stream, &wake);
    assert_eq!(wake.count(), 0);

    let staged = queue.close();
    assert_eq!(wake.count(), 0);
    staged.dispatch();
    assert_eq!(wake.count(), 1);
    eof(&mut stream, &wake);
    eof(&mut stream, &wake);
    eof(&mut queue.stream(), &wake);
    assert_eq!(queue.append(vec![]).err(), Some(StreamError::Closed));
}

#[test]
fn lifecycle_snapshot_observes_initialization_without_consuming_data() {
    let queue = SharedDispatchQueue::new();
    assert!(!queue.snapshot().initialized);
    assert!(queue.snapshot().drained);
    let operation = operation(1);
    queue
        .append(vec![DispatchEntry::Operation(operation.clone())])
        .unwrap()
        .dispatch();
    assert!(!queue.snapshot().drained);
    let mut stream = queue.stream();
    assert!(queue.snapshot().initialized);
    let wake = Arc::new(WakeCount::default());
    assert!(take_operation(poll(&mut stream, &wake)).same_instance(&operation));
    assert!(queue.snapshot().drained);
    // An empty open queue is drained, but not at EOF.
    pending(&mut stream, &wake);
    drop(stream);
    assert!(queue.snapshot().initialized);
}

#[test]
fn lifecycle_clear_pending_preserves_an_inflight_epoch_handoff() {
    let queue = SharedDispatchQueue::new();
    queue
        .append(vec![
            DispatchEntry::UpdateEpoch(barrier()),
            DispatchEntry::Operation(operation(1)),
        ])
        .unwrap()
        .dispatch();
    let mut stream = queue.stream();
    let wake = Arc::new(WakeCount::default());
    assert_eq!(take_barrier(poll(&mut stream, &wake)), barrier());
    queue.clear_pending().dispatch();
    assert!(!queue.snapshot().drained);
    assert_eq!(queue.blocked_epoch(), Some(barrier()));
    queue.close().dispatch();
    pending(&mut stream, &wake);
    queue.finish_epoch(barrier()).unwrap().dispatch();
    assert!(queue.snapshot().drained);
    eof(&mut stream, &wake);
}

#[test]
fn clones_and_reacquired_handles_share_consumption_without_replay() {
    let queue = SharedDispatchQueue::new();
    let other_owner = queue.clone();
    let first = operation(1);
    let second = operation(2);
    queue
        .append(vec![
            DispatchEntry::Operation(first.clone()),
            DispatchEntry::Operation(second.clone()),
        ])
        .unwrap()
        .dispatch();
    let wake = Arc::new(WakeCount::default());
    let mut stream = queue.stream();
    assert!(take_operation(poll(&mut stream, &wake)).same_instance(&first));
    drop(stream);
    let mut stream = other_owner.stream();
    assert!(take_operation(poll(&mut stream, &wake)).same_instance(&second));
    pending(&mut stream, &wake);
    pending(&mut queue.stream(), &wake);
}

#[test]
fn explicit_eos_is_data_and_does_not_mean_queue_eof() {
    let queue = SharedDispatchQueue::new();
    let eos = ReceivedOperation::end_of_stream(true);
    queue
        .append(vec![
            DispatchEntry::Operation(eos.clone()),
            DispatchEntry::Operation(operation(1)),
        ])
        .unwrap()
        .dispatch();
    let mut stream = queue.stream();
    let wake = Arc::new(WakeCount::default());
    let result = take_operation(poll(&mut stream, &wake));
    assert!(result.is_end_of_stream());
    assert!(result.same_instance(&eos));
    assert_eq!(result.queue_operation().lsn(), 0);
    let next = take_operation(poll(&mut stream, &wake));
    assert_eq!(next.queue_operation().lsn(), 1);
    assert_eq!(next.epoch(), Epoch::new(1, 2));
    pending(&mut stream, &wake);
}

#[test]
fn append_broadcasts_only_when_staged_wake_is_dispatched() {
    let queue = SharedDispatchQueue::new();
    let mut first = queue.stream();
    let mut second = queue.stream();
    let first_wake = Arc::new(WakeCount::default());
    let second_wake = Arc::new(WakeCount::default());
    pending(&mut first, &first_wake);
    pending(&mut second, &second_wake);
    let staged = queue
        .append(vec![DispatchEntry::Operation(operation(1))])
        .unwrap();
    assert_eq!(first_wake.count(), 0);
    assert_eq!(second_wake.count(), 0);
    staged.dispatch();
    assert_eq!(first_wake.count(), 1);
    assert_eq!(second_wake.count(), 1);
    assert_eq!(
        take_operation(poll(&mut second, &second_wake))
            .queue_operation()
            .lsn(),
        1
    );
    pending(&mut first, &first_wake);
    queue.close().dispatch();
    assert_eq!(first_wake.count(), 2);
    eof(&mut first, &first_wake);
}

#[test]
fn empty_append_keeps_waiters_registered_without_spurious_wakeup() {
    let queue = SharedDispatchQueue::new();
    let mut stream = queue.stream();
    let wake = Arc::new(WakeCount::default());
    pending(&mut stream, &wake);
    queue.append(vec![]).unwrap().dispatch();
    assert_eq!(wake.count(), 0);
    assert_eq!(queue.state.lock().unwrap().waiters.len(), 1);
    queue.close().dispatch();
    assert_eq!(wake.count(), 1);
}

#[test]
fn dropping_registered_reader_unregisters_without_stranding_another() {
    let queue = SharedDispatchQueue::new();
    let mut cancelled = queue.stream();
    let mut remaining = queue.stream();
    let cancelled_wake = Arc::new(WakeCount::default());
    let remaining_wake = Arc::new(WakeCount::default());
    pending(&mut cancelled, &cancelled_wake);
    pending(&mut remaining, &remaining_wake);
    assert_eq!(queue.state.lock().unwrap().waiters.len(), 2);
    drop(cancelled);
    assert_eq!(queue.state.lock().unwrap().waiters.len(), 1);
    queue
        .append(vec![DispatchEntry::Operation(operation(3))])
        .unwrap()
        .dispatch();
    assert_eq!(cancelled_wake.count(), 0);
    assert_eq!(remaining_wake.count(), 1);
    assert_eq!(
        take_operation(poll(&mut remaining, &remaining_wake))
            .queue_operation()
            .lsn(),
        3
    );
}

#[test]
fn cancellation_after_wake_collection_cannot_steal_another_readers_wake() {
    let queue = SharedDispatchQueue::new();
    let mut cancelled = queue.stream();
    let mut remaining = queue.stream();
    let cancelled_wake = Arc::new(WakeCount::default());
    let remaining_wake = Arc::new(WakeCount::default());
    pending(&mut cancelled, &cancelled_wake);
    pending(&mut remaining, &remaining_wake);
    let staged = queue
        .append(vec![DispatchEntry::Operation(operation(4))])
        .unwrap();
    drop(cancelled);
    staged.dispatch();
    assert_eq!(remaining_wake.count(), 1);
    assert_eq!(
        take_operation(poll(&mut remaining, &remaining_wake))
            .queue_operation()
            .lsn(),
        4
    );
}

#[test]
fn cancelling_next_future_replaces_its_waker_on_repoll() {
    let queue = SharedDispatchQueue::new();
    let mut stream = queue.stream();
    let old = Arc::new(WakeCount::default());
    let current = Arc::new(WakeCount::default());
    {
        let mut next = stream.next();
        let waker = Waker::from(old.clone());
        assert!(
            Pin::new(&mut next)
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
    }
    pending(&mut stream, &current);
    queue
        .append(vec![DispatchEntry::Operation(operation(5))])
        .unwrap()
        .dispatch();
    assert_eq!(old.count(), 0);
    assert_eq!(current.count(), 1);
    assert_eq!(
        take_operation(poll(&mut stream, &current))
            .queue_operation()
            .lsn(),
        5
    );
}

struct GuardCheckingWake {
    queue: SharedDispatchQueue,
    owner_guard: Arc<Mutex<()>>,
    calls: AtomicUsize,
}

impl Wake for GuardCheckingWake {
    fn wake(self: Arc<Self>) {
        assert!(self.owner_guard.try_lock().is_ok());
        assert!(self.queue.state.try_lock().is_ok());
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn all_transitions_stage_wakes_outside_queue_and_caller_guards() {
    let queue = SharedDispatchQueue::new();
    let owner_guard = Arc::new(Mutex::new(()));
    let checker = Arc::new(GuardCheckingWake {
        queue: queue.clone(),
        owner_guard: owner_guard.clone(),
        calls: AtomicUsize::new(0),
    });
    let waker = Waker::from(checker.clone());
    let mut cx = Context::from_waker(&waker);
    let mut stream = queue.stream();
    assert!(Pin::new(&mut stream).poll_next(&mut cx).is_pending());
    let staged = {
        let _guard = owner_guard.lock().unwrap();
        queue
            .append(vec![DispatchEntry::UpdateEpoch(barrier())])
            .unwrap()
    };
    assert_eq!(checker.calls.load(Ordering::SeqCst), 0);
    staged.dispatch();
    assert_eq!(
        take_barrier(Pin::new(&mut stream).poll_next(&mut cx)),
        barrier()
    );
    assert!(Pin::new(&mut stream).poll_next(&mut cx).is_pending());
    let staged = {
        let _guard = owner_guard.lock().unwrap();
        queue.finish_epoch(barrier()).unwrap()
    };
    assert_eq!(checker.calls.load(Ordering::SeqCst), 1);
    staged.dispatch();
    assert!(Pin::new(&mut stream).poll_next(&mut cx).is_pending());
    let staged = {
        let _guard = owner_guard.lock().unwrap();
        queue.abort(StreamError::Fault("provider failed".into()))
    };
    assert_eq!(checker.calls.load(Ordering::SeqCst), 2);
    staged.dispatch();
    assert_eq!(checker.calls.load(Ordering::SeqCst), 3);

    let queue = SharedDispatchQueue::new();
    let mut stream = queue.stream();
    let checker = Arc::new(GuardCheckingWake {
        queue: queue.clone(),
        owner_guard: owner_guard.clone(),
        calls: AtomicUsize::new(0),
    });
    let waker = Waker::from(checker.clone());
    assert!(
        Pin::new(&mut stream)
            .poll_next(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let staged = {
        let _guard = owner_guard.lock().unwrap();
        queue.close()
    };
    assert_eq!(checker.calls.load(Ordering::SeqCst), 0);
    staged.dispatch();
    assert_eq!(checker.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn close_rejects_append_and_drains_before_eof() {
    let queue = SharedDispatchQueue::new();
    queue
        .append(vec![
            DispatchEntry::Operation(operation(1)),
            DispatchEntry::Operation(operation(2)),
        ])
        .unwrap()
        .dispatch();
    queue.close().dispatch();
    queue.close().dispatch();
    assert_eq!(
        queue
            .append(vec![DispatchEntry::Operation(operation(3))])
            .err(),
        Some(StreamError::Closed)
    );
    block_on(async {
        let mut first = queue.stream();
        let mut second = queue.stream();
        let Some(Ok(StreamEvent::Operation(operation))) = first.next().await else {
            panic!("expected first operation");
        };
        assert_eq!(operation.queue_operation().lsn(), 1);
        let Some(Ok(StreamEvent::Operation(operation))) = second.next().await else {
            panic!("expected second operation");
        };
        assert_eq!(operation.queue_operation().lsn(), 2);
        assert!(first.next().await.is_none());
        assert!(second.next().await.is_none());
        assert!(queue.stream().next().await.is_none());
    });
}

#[test]
fn abort_broadcasts_one_error_per_existing_and_reacquired_handle() {
    let queue = SharedDispatchQueue::new();
    let mut first = queue.stream();
    let mut second = queue.stream();
    let first_wake = Arc::new(WakeCount::default());
    let second_wake = Arc::new(WakeCount::default());
    pending(&mut first, &first_wake);
    pending(&mut second, &second_wake);
    let error = StreamError::Fault("dispatch failed".into());
    let staged = queue.abort(error.clone());
    assert_eq!(first_wake.count(), 0);
    assert_eq!(second_wake.count(), 0);
    staged.dispatch();
    assert_eq!(first_wake.count(), 1);
    assert_eq!(second_wake.count(), 1);
    assert_eq!(stream_error(poll(&mut first, &first_wake)), error);
    eof(&mut first, &first_wake);
    assert_eq!(stream_error(poll(&mut second, &second_wake)), error);
    eof(&mut second, &second_wake);
    let mut reacquired = queue.stream();
    assert_eq!(stream_error(poll(&mut reacquired, &first_wake)), error);
    eof(&mut reacquired, &first_wake);
    assert_eq!(queue.append(vec![]).err(), Some(error));
}

#[test]
fn abort_discards_pending_data_and_preserves_first_fault() {
    let queue = SharedDispatchQueue::new();
    queue
        .append(vec![
            DispatchEntry::Operation(operation(1)),
            DispatchEntry::UpdateEpoch(barrier()),
            DispatchEntry::Operation(ReceivedOperation::end_of_stream(true)),
        ])
        .unwrap()
        .dispatch();
    let error = StreamError::Fault("first".into());
    queue.abort(error.clone()).dispatch();
    queue.abort(StreamError::Fault("second".into())).dispatch();
    queue.close().dispatch();
    assert!(queue.state.lock().unwrap().entries.is_empty());
    let mut stream = queue.stream();
    let wake = Arc::new(WakeCount::default());
    assert_eq!(stream_error(poll(&mut stream, &wake)), error);
    eof(&mut stream, &wake);
}

#[test]
fn abort_overrides_a_draining_close_but_not_permanent_graceful_eof() {
    let queue = SharedDispatchQueue::new();
    queue
        .append(vec![DispatchEntry::Operation(operation(1))])
        .unwrap()
        .dispatch();
    queue.close().dispatch();
    queue.abort(StreamError::Closed).dispatch();
    let wake = Arc::new(WakeCount::default());
    let mut stream = queue.stream();
    assert_eq!(stream_error(poll(&mut stream, &wake)), StreamError::Closed);
    eof(&mut stream, &wake);

    let queue = SharedDispatchQueue::new();
    let mut existing = queue.stream();
    queue.close().dispatch();
    eof(&mut existing, &wake);
    queue
        .abort(StreamError::Fault("too late".into()))
        .dispatch();
    eof(&mut existing, &wake);
    eof(&mut queue.stream(), &wake);
}

#[test]
fn marker_blocks_every_handle_until_explicit_matching_completion() {
    let queue = SharedDispatchQueue::new();
    let expected = operation(8);
    queue
        .append(vec![
            DispatchEntry::Operation(operation(7)),
            DispatchEntry::UpdateEpoch(barrier()),
            DispatchEntry::Operation(expected.clone()),
        ])
        .unwrap()
        .dispatch();
    let mut first = queue.stream();
    let mut second = queue.stream();
    let first_wake = Arc::new(WakeCount::default());
    let second_wake = Arc::new(WakeCount::default());
    assert_eq!(
        take_operation(poll(&mut first, &first_wake))
            .queue_operation()
            .lsn(),
        7
    );
    assert_eq!(queue.blocked_epoch(), None);
    assert_eq!(take_barrier(poll(&mut second, &second_wake)), barrier());
    assert_eq!(queue.blocked_epoch(), Some(barrier()));
    pending(&mut first, &first_wake);
    pending(&mut second, &second_wake);
    let staged = queue.finish_epoch(barrier()).unwrap();
    assert_eq!(first_wake.count(), 0);
    assert_eq!(second_wake.count(), 0);
    staged.dispatch();
    assert_eq!(first_wake.count(), 1);
    assert_eq!(second_wake.count(), 1);
    assert_eq!(queue.blocked_epoch(), None);
    assert!(take_operation(poll(&mut first, &first_wake)).same_instance(&expected));
    pending(&mut second, &second_wake);
}

#[test]
fn invalid_epoch_completion_is_atomic_and_does_not_wake_readers() {
    let queue = SharedDispatchQueue::new();
    assert_eq!(
        queue.finish_epoch(barrier()).err(),
        Some(StreamError::InvalidEpochCompletion)
    );
    queue
        .append(vec![
            DispatchEntry::UpdateEpoch(barrier()),
            DispatchEntry::Operation(operation(8)),
        ])
        .unwrap()
        .dispatch();
    let mut stream = queue.stream();
    let wake = Arc::new(WakeCount::default());
    assert_eq!(take_barrier(poll(&mut stream, &wake)), barrier());
    pending(&mut stream, &wake);
    for wrong in [
        EpochBarrier {
            epoch: Epoch::new(2, 4),
            ..barrier()
        },
        EpochBarrier {
            previous_epoch_last_lsn: 6,
            ..barrier()
        },
    ] {
        assert_eq!(
            queue.finish_epoch(wrong).err(),
            Some(StreamError::InvalidEpochCompletion)
        );
        assert_eq!(queue.blocked_epoch(), Some(barrier()));
        assert_eq!(queue.state.lock().unwrap().waiters.len(), 1);
        assert_eq!(queue.state.lock().unwrap().entries.len(), 1);
        assert_eq!(wake.count(), 0);
    }
    queue.finish_epoch(barrier()).unwrap().dispatch();
    assert_eq!(wake.count(), 1);
    assert_eq!(
        take_operation(poll(&mut stream, &wake))
            .queue_operation()
            .lsn(),
        8
    );
    assert_eq!(
        queue.finish_epoch(barrier()).err(),
        Some(StreamError::InvalidEpochCompletion)
    );
}

#[test]
fn forgotten_marker_and_dropped_reader_do_not_complete_provider_handoff() {
    let queue = SharedDispatchQueue::new();
    queue
        .append(vec![
            DispatchEntry::UpdateEpoch(barrier()),
            DispatchEntry::Operation(operation(8)),
        ])
        .unwrap()
        .dispatch();
    let wake = Arc::new(WakeCount::default());
    let mut stream = queue.stream();
    assert!(matches!(
        poll(&mut stream, &wake),
        Poll::Ready(Some(Ok(StreamEvent::UpdateEpoch(_))))
    ));
    drop(stream);
    let mut reacquired = queue.stream();
    for _ in 0..3 {
        pending(&mut reacquired, &wake);
    }
    queue.close().dispatch();
    pending(&mut reacquired, &wake);
    assert_eq!(queue.blocked_epoch(), Some(barrier()));
    queue.finish_epoch(barrier()).unwrap().dispatch();
    assert_eq!(
        take_operation(poll(&mut reacquired, &wake))
            .queue_operation()
            .lsn(),
        8
    );
    eof(&mut reacquired, &wake);
}

#[test]
fn close_with_last_marker_stays_pending_until_explicit_completion() {
    let queue = SharedDispatchQueue::new();
    queue
        .append(vec![DispatchEntry::UpdateEpoch(barrier())])
        .unwrap()
        .dispatch();
    queue.close().dispatch();
    let mut stream = queue.stream();
    let mut other = queue.stream();
    let wake = Arc::new(WakeCount::default());
    assert_eq!(take_barrier(poll(&mut stream, &wake)), barrier());
    pending(&mut stream, &wake);
    pending(&mut other, &wake);
    queue.finish_epoch(barrier()).unwrap().dispatch();
    eof(&mut stream, &wake);
    eof(&mut other, &wake);
    eof(&mut queue.stream(), &wake);
}

#[test]
fn abort_unblocks_epoch_waiters_as_error_and_rejects_late_completion() {
    let queue = SharedDispatchQueue::new();
    queue
        .append(vec![
            DispatchEntry::UpdateEpoch(barrier()),
            DispatchEntry::Operation(operation(8)),
        ])
        .unwrap()
        .dispatch();
    let mut stream = queue.stream();
    let mut other = queue.stream();
    let wake = Arc::new(WakeCount::default());
    let other_wake = Arc::new(WakeCount::default());
    assert_eq!(take_barrier(poll(&mut stream, &wake)), barrier());
    pending(&mut stream, &wake);
    pending(&mut other, &other_wake);
    let error = StreamError::Fault("provider update failed".into());
    queue.abort(error.clone()).dispatch();
    assert_eq!(queue.blocked_epoch(), None);
    assert_eq!(wake.count(), 1);
    assert_eq!(other_wake.count(), 1);
    assert_eq!(
        queue.finish_epoch(barrier()).err(),
        Some(StreamError::InvalidEpochCompletion)
    );
    assert_eq!(stream_error(poll(&mut stream, &wake)), error);
    assert_eq!(stream_error(poll(&mut other, &other_wake)), error);
    eof(&mut stream, &wake);
    eof(&mut other, &other_wake);
}

#[test]
fn consecutive_markers_require_individual_completions() {
    let queue = SharedDispatchQueue::new();
    let second = EpochBarrier {
        epoch: Epoch::new(2, 4),
        previous_epoch_last_lsn: 8,
    };
    queue
        .append(vec![
            DispatchEntry::UpdateEpoch(barrier()),
            DispatchEntry::UpdateEpoch(second),
            DispatchEntry::Operation(operation(9)),
        ])
        .unwrap()
        .dispatch();
    let wake = Arc::new(WakeCount::default());
    let mut stream = queue.stream();
    assert_eq!(take_barrier(poll(&mut stream, &wake)), barrier());
    pending(&mut stream, &wake);
    queue.finish_epoch(barrier()).unwrap().dispatch();
    assert_eq!(take_barrier(poll(&mut stream, &wake)), second);
    pending(&mut stream, &wake);
    assert_eq!(
        queue.finish_epoch(barrier()).err(),
        Some(StreamError::InvalidEpochCompletion)
    );
    assert_eq!(queue.blocked_epoch(), Some(second));
    queue.finish_epoch(second).unwrap().dispatch();
    assert_eq!(
        take_operation(poll(&mut stream, &wake))
            .queue_operation()
            .lsn(),
        9
    );
}

#[test]
fn async_executor_observes_staged_wake_and_consumes_without_replay() {
    let queue = SharedDispatchQueue::new();
    let output = Rc::new(RefCell::new(Vec::new()));
    let mut pool = LocalPool::new();
    let mut stream = queue.stream();
    let captured = output.clone();
    pool.spawner()
        .spawn_local(async move {
            while let Some(event) = stream.next().await {
                match event {
                    Ok(StreamEvent::Operation(operation)) => {
                        captured
                            .borrow_mut()
                            .push(operation.queue_operation().lsn());
                    }
                    _ => panic!("unexpected stream event"),
                }
            }
        })
        .unwrap();
    pool.run_until_stalled();
    let staged = queue
        .append(vec![
            DispatchEntry::Operation(operation(1)),
            DispatchEntry::Operation(operation(2)),
        ])
        .unwrap();
    pool.run_until_stalled();
    assert!(output.borrow().is_empty());
    staged.dispatch();
    pool.run_until_stalled();
    assert_eq!(&*output.borrow(), &[1, 2]);
    queue.close().dispatch();
    pool.run();
    assert!(block_on(queue.stream().next()).is_none());
}

#[test]
fn async_reader_cancellation_leaves_other_registered_reader_live() {
    let queue = SharedDispatchQueue::new();
    let output = Rc::new(RefCell::new(None));
    let mut pool = LocalPool::new();
    let (cancel, registration) = AbortHandle::new_pair();
    let mut cancelled = queue.stream();
    pool.spawner()
        .spawn_local(async move {
            let _ = Abortable::new(
                async move {
                    let _ = cancelled.next().await;
                },
                registration,
            )
            .await;
        })
        .unwrap();
    let mut remaining = queue.stream();
    let captured = output.clone();
    pool.spawner()
        .spawn_local(async move {
            let Some(Ok(StreamEvent::Operation(operation))) = remaining.next().await else {
                panic!("expected operation for surviving reader");
            };
            *captured.borrow_mut() = Some(operation.queue_operation().lsn());
        })
        .unwrap();
    pool.run_until_stalled();
    assert_eq!(queue.state.lock().unwrap().waiters.len(), 2);
    cancel.abort();
    pool.run_until_stalled();
    assert_eq!(queue.state.lock().unwrap().waiters.len(), 1);
    queue
        .append(vec![DispatchEntry::Operation(operation(11))])
        .unwrap()
        .dispatch();
    pool.run();
    assert_eq!(*output.borrow(), Some(11));
    assert!(queue.state.lock().unwrap().waiters.is_empty());
}

#[test]
fn async_epoch_handoff_blocks_appended_data_until_explicit_success() {
    let queue = SharedDispatchQueue::new();
    let output = Rc::new(RefCell::new(Vec::new()));
    let mut pool = LocalPool::new();
    let mut stream = queue.stream();
    let captured = output.clone();
    pool.spawner()
        .spawn_local(async move {
            let Some(Ok(StreamEvent::UpdateEpoch(marker))) = stream.next().await else {
                panic!("expected epoch marker");
            };
            assert_eq!(marker, barrier());
            captured.borrow_mut().push(0);
            while let Some(event) = stream.next().await {
                let Ok(StreamEvent::Operation(operation)) = event else {
                    panic!("expected ordinary operation");
                };
                captured
                    .borrow_mut()
                    .push(operation.queue_operation().lsn());
            }
        })
        .unwrap();
    pool.run_until_stalled();
    queue
        .append(vec![DispatchEntry::UpdateEpoch(barrier())])
        .unwrap()
        .dispatch();
    pool.run_until_stalled();
    assert_eq!(&*output.borrow(), &[0]);
    assert_eq!(queue.blocked_epoch(), Some(barrier()));
    queue
        .append(vec![DispatchEntry::Operation(operation(8))])
        .unwrap()
        .dispatch();
    pool.run_until_stalled();
    assert_eq!(&*output.borrow(), &[0]);
    queue.close().dispatch();
    pool.run_until_stalled();
    assert_eq!(&*output.borrow(), &[0]);
    let staged = queue.finish_epoch(barrier()).unwrap();
    pool.run_until_stalled();
    assert_eq!(&*output.borrow(), &[0]);
    staged.dispatch();
    pool.run();
    assert_eq!(&*output.borrow(), &[0, 8]);
    assert_eq!(queue.blocked_epoch(), None);
}
