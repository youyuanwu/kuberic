// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-component vectors derived from microsoft/service-fabric
// @3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d, Reliability/Replication:
// CopyContextReceiver.cpp:27-63,80-97,105-145,147-234;
// ComOperationDataAsyncEnumerator.cpp:59-75,90-157,188-196;
// AckSender.cpp:98-208; ReplicationSession.cpp:466-509.
// Selected native scenario ancestry: ReplicateCopyOperations.Test.cpp:430-438,
// 744-807,1049-1086,1972-2013 (retry, zero context items, batching, numbered last).
// Production-derived deterministic gap/terminal/close vectors and Rust-specific
// lifetime tests are adaptations, not native test executions or parity claims.

use std::sync::Weak;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Wake, Waker};

use futures::task::noop_waker_ref;

use super::*;
use crate::protocol::types::Epoch;

type Sent = Arc<Mutex<Vec<(CopyContextAck, bool)>>>;
type DataPoll = Poll<Option<Result<OperationData, CopyContextError>>>;

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn config() -> CopyContextConfig {
    CopyContextConfig {
        initial_queue_size: 4,
        max_queue_size: 8,
        batch_ack_interval: ms(10),
        retry_interval: ms(100),
        max_pending_acknowledgements: 2,
        trace_interval: ms(1000),
    }
}

fn receiver_with(config: CopyContextConfig) -> (CopyContextReceiver, Sent) {
    let mut receiver = CopyContextReceiver::new(config).unwrap();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let callback_sent = sent.clone();
    let send: SendContextAck = Arc::new(move |ack, trace| {
        callback_sent.lock().unwrap().push((ack, trace));
        true
    });
    receiver.open(&send, ms(0)).unwrap();
    (receiver, sent)
}

fn receiver() -> (CopyContextReceiver, Sent) {
    receiver_with(config())
}

fn operation(lsn: Lsn, data: OperationData) -> ReceivedOperation {
    ReceivedOperation::new(
        QueueOperation::new(lsn, data, true),
        Epoch::new(1, 7),
        false,
    )
}

fn data(lsn: Lsn, value: &'static [u8]) -> ReceivedOperation {
    operation(lsn, OperationData::from_static(value))
}

fn terminal(lsn: Lsn) -> ReceivedOperation {
    ReceivedOperation::copy_end_of_stream(lsn, false).unwrap()
}

fn receive(
    receiver: &mut CopyContextReceiver,
    lsn: Lsn,
    value: &'static [u8],
    now: u64,
) -> ContextObservation {
    receiver
        .process(data(lsn, value), false, ms(now))
        .dispatch()
        .unwrap()
}

fn finish(receiver: &mut CopyContextReceiver, lsn: Lsn, now: u64) -> ContextObservation {
    receiver
        .process(terminal(lsn), true, ms(now))
        .dispatch()
        .unwrap()
}

fn poll(stream: &mut CopyContextStream) -> DataPoll {
    Pin::new(stream).poll_next(&mut Context::from_waker(noop_waker_ref()))
}

fn item(stream: &mut CopyContextStream) -> OperationData {
    match poll(stream) {
        Poll::Ready(Some(Ok(data))) => data,
        other => panic!("expected context data, got {other:?}"),
    }
}

fn eof(stream: &mut CopyContextStream) {
    assert_eq!(poll(stream), Poll::Ready(None));
}

fn ack(completed: Lsn, error_code: i32) -> CopyContextAck {
    CopyContextAck {
        completed,
        error_code,
    }
}

#[test]
fn ordered_admission_completes_before_dequeue_without_service_ack() {
    let (mut receiver, _) = receiver();
    let first = data(1, b"first");
    let observed = first.clone();
    let effects = receiver.process(first, false, ms(0));
    assert_eq!(receiver.completed(), 1);
    assert_eq!(receiver.queue.operation_count(), 0);
    assert_eq!(receiver.current_ack(), ack(1, 0));
    assert_eq!(observed.ack_state(), AckState::NotCalled);
    assert!(observed.queue_operation().needs_ack());
    assert!(!observed.callback_required());
    let outcome = effects.dispatch().unwrap();
    assert_eq!(outcome.admission, ContextAdmission::Accepted);
    assert!(outcome.ack_requested);
    assert!(!outcome.force_ack);
    let mut stream = receiver.stream();
    assert_eq!(item(&mut stream).as_ref(), b"first");
    assert_eq!(observed.ack_state(), AckState::NotCalled);
    assert!(poll(&mut stream).is_pending());
}

#[test]
fn terminal_before_gap_closes_only_after_all_ordered_data_is_admitted() {
    let (mut receiver, sent) = receiver();
    let mut stream = receiver.stream();
    let last = finish(&mut receiver, 5, 0);
    assert!(last.ack_requested);
    assert!(!last.force_ack);
    assert!(!receiver.all_operations_received());
    assert_eq!(receiver.current_ack(), ack(0, 0));
    assert!(!receive(&mut receiver, 3, b"3", 1).ack_requested);
    assert!(poll(&mut stream).is_pending());
    receive(&mut receiver, 1, b"1", 2);
    assert_eq!(receiver.completed(), 1);
    assert_eq!(item(&mut stream).as_ref(), b"1");
    assert!(!receive(&mut receiver, 4, b"4", 3).ack_requested);
    let filled = receive(&mut receiver, 2, b"2", 4);
    assert!(filled.force_ack);
    assert!(receiver.all_operations_received());
    assert_eq!(receiver.completed(), 4);
    assert_eq!(receiver.current_ack(), ack(5, 0));
    assert_eq!(receiver.queue.operation_count(), 0);
    for expected in [b"2", b"3", b"4"] {
        assert_eq!(item(&mut stream).as_ref(), expected);
    }
    eof(&mut stream);
    assert_eq!(*sent.lock().unwrap(), vec![(ack(5, 0), false)]);
}

#[test]
fn pending_and_completed_duplicates_schedule_ack_without_replacing_or_replaying_data() {
    let (mut receiver, _) = receiver();
    assert!(!receive(&mut receiver, 2, b"original", 0).ack_requested);
    let duplicate = receive(&mut receiver, 2, b"replacement", 1);
    assert_eq!(duplicate.admission, ContextAdmission::Duplicate);
    assert!(duplicate.ack_requested);
    assert_eq!(receiver.queue.operation_count(), 1);
    receive(&mut receiver, 1, b"first", 2);
    let duplicate = receive(&mut receiver, 1, b"stale", 3);
    assert_eq!(duplicate.admission, ContextAdmission::Duplicate);
    assert!(duplicate.ack_requested);
    assert_eq!(receiver.completed(), 2);
    let mut stream = receiver.stream();
    assert_eq!(item(&mut stream).as_ref(), b"first");
    assert_eq!(item(&mut stream).as_ref(), b"original");
    assert!(poll(&mut stream).is_pending());
}

#[test]
fn count_bound_includes_holes_and_full_input_can_be_retried_after_gap_completion() {
    let mut settings = config();
    settings.max_queue_size = 4;
    let (mut receiver, _) = receiver_with(settings);
    assert!(!receive(&mut receiver, 4, b"4", 0).ack_requested);
    let full = receive(&mut receiver, 5, b"retry", 1);
    assert_eq!(full.admission, ContextAdmission::Full);
    assert!(full.ack_requested);
    assert_eq!(receiver.queue.operation_count(), 1);
    assert_eq!(receiver.completed(), 0);
    receive(&mut receiver, 1, b"1", 2);
    receive(&mut receiver, 2, b"2", 3);
    receive(&mut receiver, 3, b"3", 4);
    assert_eq!(receiver.completed(), 4);
    assert_eq!(
        receive(&mut receiver, 5, b"retry", 5).admission,
        ContextAdmission::Accepted
    );
    finish(&mut receiver, 6, 6);
    let mut stream = receiver.stream();
    for expected in [b"1".as_slice(), b"2", b"3", b"4", b"retry"] {
        assert_eq!(item(&mut stream).as_ref(), expected);
    }
    eof(&mut stream);
}

#[test]
fn context_queue_has_no_invented_memory_bound() {
    let mut settings = config();
    settings.initial_queue_size = 2;
    settings.max_queue_size = 2;
    let (mut receiver, _) = receiver_with(settings);
    let payload = OperationData::from(vec![42; 128 * 1024]);
    let expected = payload.clone();
    receiver
        .process(operation(2, payload), false, ms(0))
        .dispatch()
        .unwrap();
    assert_eq!(receiver.queue.total_bytes(), expected.len());
    receive(&mut receiver, 1, b"first", 1);
    let mut stream = receiver.stream();
    assert_eq!(item(&mut stream).as_ref(), b"first");
    assert_eq!(item(&mut stream), expected);
    assert_eq!(receiver.queue.total_bytes(), 0);
}

#[test]
fn empty_ordinary_data_is_an_item_but_terminal_metadata_is_eof() {
    let (mut receiver, _) = receiver();
    receive(&mut receiver, 1, b"", 0);
    let mut stream = receiver.stream();
    assert!(item(&mut stream).is_empty());
    assert!(poll(&mut stream).is_pending());
    finish(&mut receiver, 2, 1);
    assert!(receiver.all_operations_received());
    assert_eq!(receiver.completed(), 1);
    assert_eq!(receiver.current_ack(), ack(2, 0));
    eof(&mut stream);
}

#[test]
fn zero_context_data_uses_numbered_terminal_one_not_an_empty_data_item() {
    let (mut receiver, sent) = receiver();
    let last = finish(&mut receiver, 1, 0);
    assert!(last.force_ack);
    assert!(receiver.all_operations_received());
    assert_eq!(receiver.completed(), 0);
    assert_eq!(receiver.current_ack(), ack(1, 0));
    assert_eq!(receiver.last_ack_sent(), 1);
    assert_eq!(*sent.lock().unwrap(), vec![(ack(1, 0), false)]);
    eof(&mut receiver.stream());
}

#[test]
fn framed_empty_terminal_checks_logical_payload_not_adapter_overhead() {
    use crate::replicator::v1::buffers::{self, BufferLimits, BufferPolicy};

    let limits = BufferLimits {
        max_segments: 4,
        max_payload_bytes: 16,
    };
    let frame = buffers::encode(
        &[OperationData::new(), OperationData::new()],
        BufferPolicy::FramedSegments,
        limits,
    )
    .unwrap();
    assert!(!frame.is_empty());
    let terminal = ReceivedOperation::new(
        QueueOperation::from_data(1, frame, true, BufferPolicy::FramedSegments, limits).unwrap(),
        Epoch::new(0, 0),
        false,
    );
    let (mut receiver, sent) = receiver();
    let outcome = receiver.process(terminal, true, ms(0)).dispatch().unwrap();
    assert_eq!(outcome.admission, ContextAdmission::Terminal);
    assert!(outcome.force_ack);
    assert!(receiver.all_operations_received());
    assert_eq!(*sent.lock().unwrap(), [(ack(1, 0), false)]);
    eof(&mut receiver.stream());
}

#[test]
fn getters_share_consumption_and_terminal_reacquisition_never_replays() {
    let (mut receiver, _) = receiver();
    receive(&mut receiver, 1, b"one", 0);
    receive(&mut receiver, 2, b"two", 1);
    let mut first = receiver.stream();
    let mut second = receiver.stream();
    assert_eq!(item(&mut first).as_ref(), b"one");
    drop(first);
    let mut reacquired = receiver.stream();
    assert_eq!(item(&mut reacquired).as_ref(), b"two");
    finish(&mut receiver, 3, 2);
    eof(&mut second);
    eof(&mut reacquired);
    eof(&mut receiver.stream());
    eof(&mut receiver.stream());
}

#[test]
fn terminal_retries_and_replacement_before_completion_preserve_native_last_seen_bookkeeping() {
    let (mut receiver, _) = receiver();
    finish(&mut receiver, 5, 0);
    assert_eq!(
        finish(&mut receiver, 5, 1).admission,
        ContextAdmission::Terminal
    );
    // Native assigns lastCopyContextSequenceNumber on every isLast arrival.
    finish(&mut receiver, 4, 2);
    assert!(!receive(&mut receiver, 3, b"3", 3).ack_requested);
    receive(&mut receiver, 1, b"1", 4);
    assert!(!receiver.all_operations_received());
    receive(&mut receiver, 2, b"2", 5);
    assert!(receiver.all_operations_received());
    assert_eq!(receiver.current_ack(), ack(4, 0));
    let ignored = finish(&mut receiver, 5, 6);
    assert_eq!(ignored.admission, ContextAdmission::IgnoredCompleted);
    assert!(ignored.ack_requested);
    assert_eq!(receiver.current_ack(), ack(4, 0));
}

#[test]
fn malformed_terminal_is_rejected_before_metadata_or_progress_mutation() {
    let (mut receiver, _) = receiver();
    receive(&mut receiver, 3, b"queued", 0);
    finish(&mut receiver, 5, 1);
    let before = (
        receiver.queue.progress(),
        receiver.current_ack(),
        receiver.ack_deadline(),
    );
    for (marker, expected) in [
        (data(4, b"not-empty"), CopyContextError::InvalidTerminal(4)),
        (terminal(3), CopyContextError::InvalidTerminal(3)),
    ] {
        assert_eq!(
            receiver.process(marker, true, ms(2)).dispatch(),
            Err(expected)
        );
        assert_eq!(receiver.snapshot.lock().unwrap().terminal, Some(5));
        assert_eq!(
            (
                receiver.queue.progress(),
                receiver.current_ack(),
                receiver.ack_deadline(),
            ),
            before
        );
    }
    assert_eq!(
        receiver
            .process(data(5, b"beyond"), false, ms(3))
            .dispatch(),
        Err(CopyContextError::DataAfterTerminal(5))
    );
    assert_eq!(receiver.queue.progress(), before.0);
    assert!(!receiver.all_operations_received());
}

#[test]
fn invalid_context_operations_are_explicit_and_do_not_mutate_queue() {
    let (mut receiver, sent) = receiver();
    let already_acknowledged = data(1, b"done");
    assert!(already_acknowledged.acknowledge().unwrap().is_none());
    let inputs = [
        data(0, b"zero"),
        data(Lsn::MAX, b"overflow"),
        ReceivedOperation::end_of_stream(false),
        ReceivedOperation::copy_end_of_stream(1, false).unwrap(),
        ReceivedOperation::new(
            QueueOperation::new(1, OperationData::new(), true),
            Epoch::new(1, 7),
            true,
        ),
        ReceivedOperation::new(
            QueueOperation::new(1, OperationData::new(), true),
            Epoch::new(-1, 7),
            false,
        ),
        already_acknowledged,
    ];
    for operation in inputs {
        let lsn = operation.queue_operation().lsn();
        assert_eq!(
            receiver.process(operation, false, ms(0)).dispatch(),
            Err(CopyContextError::InvalidOperation(lsn))
        );
        assert_eq!(receiver.completed(), 0);
        assert_eq!(receiver.queue.operation_count(), 0);
        assert_eq!(receiver.current_ack(), ack(0, 0));
        assert_eq!(receiver.ack_deadline(), None);
    }
    assert!(sent.lock().unwrap().is_empty());
}

#[test]
fn invalid_settings_are_errors_instead_of_queue_or_scheduler_assertions() {
    for (initial, maximum) in [(0, 4), (1, 4), (3, 4), (8, 4), (4, 0), (4, 7)] {
        let mut settings = config();
        settings.initial_queue_size = initial;
        settings.max_queue_size = maximum;
        assert_eq!(
            CopyContextReceiver::new(settings).err(),
            Some(CopyContextError::InvalidQueueConfig)
        );
    }
    let mut settings = config();
    settings.batch_ack_interval = settings.retry_interval;
    assert_eq!(
        CopyContextReceiver::new(settings).err(),
        Some(CopyContextError::InvalidAckConfig)
    );
    if usize::BITS >= 64 {
        settings = config();
        settings.max_pending_acknowledgements = usize::MAX;
        assert_eq!(
            CopyContextReceiver::new(settings).err(),
            Some(CopyContextError::InvalidAckConfig)
        );
    }
}

#[test]
fn ack_sender_cannot_be_opened_twice_or_after_receiver_close() {
    let (mut receiver, _) = receiver();
    let send: SendContextAck = Arc::new(|_, _| true);
    assert_eq!(
        receiver.open(&send, ms(1)),
        Err(CopyContextError::AlreadyOpen)
    );
    receiver.close(0, ms(2)).dispatch().unwrap();
    assert_eq!(receiver.open(&send, ms(3)), Err(CopyContextError::Closed));
}

#[test]
fn batching_coalesces_without_postponing_and_timer_samples_current_progress() {
    let (mut receiver, sent) = receiver();
    receive(&mut receiver, 1, b"1", 1);
    assert_eq!(receiver.ack_deadline(), Some(ms(11)));
    receive(&mut receiver, 2, b"2", 2);
    assert_eq!(receiver.ack_deadline(), Some(ms(11)));
    assert!(sent.lock().unwrap().is_empty());
    let effects = receiver.on_ack_timer(ms(11));
    assert_eq!(receiver.last_ack_issued_at(), Some(ms(11)));
    assert_eq!(receiver.last_ack_sent(), 0);
    effects.dispatch().unwrap();
    assert_eq!(receiver.last_ack_sent(), 2);
    assert_eq!(*sent.lock().unwrap(), vec![(ack(2, 0), false)]);
    assert_eq!(receiver.ack_deadline(), None);
}

#[test]
fn context_pending_threshold_is_strict_and_force_cancels_batch_deadline() {
    let (mut receiver, sent) = receiver();
    assert!(!receive(&mut receiver, 1, b"1", 0).force_ack);
    assert!(!receive(&mut receiver, 2, b"2", 1).force_ack);
    assert_eq!(receiver.ack_deadline(), Some(ms(10)));
    assert!(receive(&mut receiver, 3, b"3", 2).force_ack);
    assert_eq!(receiver.ack_deadline(), None);
    assert_eq!(receiver.last_ack_issued_at(), Some(ms(2)));
    assert_eq!(*sent.lock().unwrap(), vec![(ack(3, 0), false)]);
    assert!(!receive(&mut receiver, 4, b"4", 3).force_ack);
    assert_eq!(receiver.ack_deadline(), Some(ms(13)));
}

#[test]
fn context_zero_pending_threshold_still_forces_despite_ack_sender_replication_predicate() {
    let mut settings = config();
    settings.max_pending_acknowledgements = 0;
    let (mut receiver, sent) = receiver_with(settings);
    assert!(!receiver.ack_sender.force_on_pending_limit());
    assert!(receive(&mut receiver, 1, b"1", 0).force_ack);
    assert_eq!(*sent.lock().unwrap(), vec![(ack(1, 0), false)]);
    assert_eq!(receiver.ack_deadline(), None);
}

#[test]
fn delayed_immediate_ack_effect_samples_issuance_state_and_false_does_not_requeue() {
    let mut settings = config();
    settings.batch_ack_interval = Duration::ZERO;
    let mut receiver = CopyContextReceiver::new(settings).unwrap();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let callback_sent = sent.clone();
    let send: SendContextAck = Arc::new(move |ack, _| {
        callback_sent.lock().unwrap().push(ack);
        false
    });
    receiver.open(&send, ms(0)).unwrap();
    let first = receiver.process(data(1, b"1"), false, ms(1));
    let second = receiver.process(data(2, b"2"), false, ms(2));
    assert_eq!(receiver.last_ack_sent(), 0);
    first.dispatch().unwrap();
    second.dispatch().unwrap();
    assert_eq!(*sent.lock().unwrap(), vec![ack(2, 0), ack(2, 0)]);
    assert_eq!(receiver.last_ack_sent(), 2);
    assert_eq!(receiver.ack_deadline(), None);
    assert_eq!(receiver.last_ack_issued_at(), Some(ms(2)));
    receiver.on_ack_timer(ms(100)).dispatch().unwrap();
    receiver.close(0, ms(101)).dispatch().unwrap();
    assert_eq!(sent.lock().unwrap().len(), 2);
}

#[test]
fn a_selected_timer_can_issue_after_force_without_reviving_a_deadline() {
    let (mut receiver, sent) = receiver();
    receive(&mut receiver, 1, b"1", 0);
    receive(&mut receiver, 2, b"2", 1);
    receive(&mut receiver, 3, b"3", 2);
    assert_eq!(receiver.ack_deadline(), None);
    receiver.on_ack_timer(ms(10)).dispatch().unwrap();
    assert_eq!(
        *sent.lock().unwrap(),
        vec![(ack(3, 0), false), (ack(3, 0), false)]
    );
    assert_eq!(receiver.ack_deadline(), None);
}

#[test]
fn successful_close_flushes_pending_ack_and_disables_late_timer() {
    let (mut receiver, sent) = receiver();
    receive(&mut receiver, 1, b"1", 0);
    let close = receiver.close(0, ms(1));
    assert!(sent.lock().unwrap().is_empty());
    assert_eq!(receiver.ack_deadline(), None);
    assert_eq!(receiver.last_ack_issued_at(), Some(ms(1)));
    close.dispatch().unwrap();
    assert_eq!(*sent.lock().unwrap(), vec![(ack(1, 0), true)]);
    receiver.on_ack_timer(ms(10)).dispatch().unwrap();
    assert_eq!(sent.lock().unwrap().len(), 1);
}

#[test]
fn error_close_forces_current_error_ack_instead_of_double_flushing_pending_ack() {
    let (mut receiver, sent) = receiver();
    let mut pending = receiver.stream();
    assert!(poll(&mut pending).is_pending());
    receive(&mut receiver, 1, b"not-consumed", 0);
    let close = receiver.close(-17, ms(1));
    assert_eq!(receiver.current_ack(), ack(1, -17));
    assert_eq!(receiver.last_ack_sent(), 0);
    close.dispatch().unwrap();
    assert_eq!(*sent.lock().unwrap(), vec![(ack(1, -17), false)]);
    assert_eq!(
        poll(&mut pending),
        Poll::Ready(Some(Err(CopyContextError::EnumeratorClosed(-17))))
    );
    eof(&mut pending);
    assert_eq!(receiver.ack_deadline(), None);
    receiver.on_ack_timer(ms(10)).dispatch().unwrap();
    assert_eq!(sent.lock().unwrap().len(), 1);
}

#[test]
fn error_close_with_terminal_before_gap_does_not_ack_missing_context() {
    let (mut receiver, sent) = receiver();
    finish(&mut receiver, 3, 0);
    assert!(!receive(&mut receiver, 2, b"gap-before-this", 1).ack_requested);
    assert_eq!(receiver.queue.operation_count(), 1);
    receiver.close(-19, ms(2)).dispatch().unwrap();
    assert_eq!(receiver.completed(), 0);
    assert!(!receiver.all_operations_received());
    assert_eq!(receiver.current_ack(), ack(0, -19));
    assert_eq!(receiver.queue.operation_count(), 0);
    assert_eq!(*sent.lock().unwrap(), vec![(ack(0, -19), false)]);
}

#[test]
fn successful_explicit_close_allows_only_an_already_pending_dequeue() {
    let (mut receiver, _) = receiver();
    let mut pending = receiver.stream();
    assert!(poll(&mut pending).is_pending());
    receive(&mut receiver, 1, b"already-in-flight", 0);
    receiver.close(0, ms(1)).dispatch().unwrap();
    let mut acquired_after_close = receiver.stream();
    assert_eq!(
        poll(&mut acquired_after_close),
        Poll::Ready(Some(Err(CopyContextError::EnumeratorClosed(0))))
    );
    eof(&mut acquired_after_close);
    assert_eq!(item(&mut pending).as_ref(), b"already-in-flight");
    assert_eq!(
        poll(&mut pending),
        Poll::Ready(Some(Err(CopyContextError::EnumeratorClosed(0))))
    );
    eof(&mut pending);
}

#[test]
fn successful_close_finishes_pending_empty_dequeue_with_eof_not_a_data_marker() {
    let (mut receiver, sent) = receiver();
    let mut pending = receiver.stream();
    assert!(poll(&mut pending).is_pending());
    receiver.close(0, ms(0)).dispatch().unwrap();
    eof(&mut pending);
    assert!(sent.lock().unwrap().is_empty());
}

#[test]
fn repeated_close_updates_error_state_but_inactive_input_does_not_reopen_ack_sender() {
    let (mut receiver, sent) = receiver();
    receiver.close(-23, ms(0)).dispatch().unwrap();
    receiver.close(-24, ms(1)).dispatch().unwrap();
    let ignored = receive(&mut receiver, 1, b"late", 2);
    assert_eq!(ignored.admission, ContextAdmission::IgnoredClosed);
    assert!(ignored.ack_requested);
    assert_eq!(receiver.completed(), 0);
    assert_eq!(receiver.current_ack(), ack(0, -24));
    assert_eq!(*sent.lock().unwrap(), vec![(ack(0, -23), false)]);
    assert_eq!(receiver.ack_deadline(), None);
}

#[test]
fn data_only_adapter_reports_unexpected_epoch_eos_and_shared_stream_faults() {
    let (receiver, _) = receiver();
    let barrier = EpochBarrier {
        epoch: Epoch::new(2, 8),
        previous_epoch_last_lsn: 0,
    };
    receiver
        .dispatch
        .append(vec![DispatchEntry::UpdateEpoch(barrier)])
        .unwrap()
        .dispatch();
    let mut stream = receiver.stream();
    assert_eq!(
        poll(&mut stream),
        Poll::Ready(Some(Err(CopyContextError::UnexpectedEpoch(barrier))))
    );
    assert_eq!(receiver.dispatch.blocked_epoch(), Some(barrier));
    eof(&mut stream);

    let (receiver, _) = self::receiver();
    receiver
        .dispatch
        .append(vec![DispatchEntry::Operation(
            ReceivedOperation::end_of_stream(false),
        )])
        .unwrap()
        .dispatch();
    let mut stream = receiver.stream();
    assert_eq!(
        poll(&mut stream),
        Poll::Ready(Some(Err(CopyContextError::UnexpectedEndOfStream)))
    );
    eof(&mut stream);

    let (receiver, _) = self::receiver();
    let fault = StreamError::Fault("unexpected dispatch failure".into());
    receiver.dispatch.abort(fault.clone()).dispatch();
    let mut stream = receiver.stream();
    assert_eq!(
        poll(&mut stream),
        Poll::Ready(Some(Err(CopyContextError::Stream(fault))))
    );
    eof(&mut stream);
}

#[test]
fn unexpected_dispatch_admission_failure_is_not_silently_successful() {
    let (mut receiver, _) = receiver();
    let fault = StreamError::Fault("closed outside the context owner".into());
    receiver.dispatch.abort(fault.clone()).dispatch();
    assert_eq!(
        receiver.process(data(1, b"data"), false, ms(0)).dispatch(),
        Err(CopyContextError::Stream(fault))
    );
    assert_eq!(receiver.ack_deadline(), None);
}

fn assert_unlocked(owner: &Weak<Mutex<CopyContextReceiver>>) {
    if let Some(owner) = owner.upgrade() {
        assert!(owner.try_lock().is_ok(), "effect ran under receiver guard");
    }
}

struct PayloadProbe {
    owner: Weak<Mutex<CopyContextReceiver>>,
    drops: Arc<AtomicUsize>,
}

impl AsRef<[u8]> for PayloadProbe {
    fn as_ref(&self) -> &[u8] {
        b"rooted context"
    }
}

impl Drop for PayloadProbe {
    fn drop(&mut self) {
        assert_unlocked(&self.owner);
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

fn rooted_data(
    owner: &Arc<Mutex<CopyContextReceiver>>,
    lsn: Lsn,
    drops: &Arc<AtomicUsize>,
) -> ReceivedOperation {
    operation(
        lsn,
        OperationData::from_owner(PayloadProbe {
            owner: Arc::downgrade(owner),
            drops: drops.clone(),
        }),
    )
}

#[test]
fn rejected_and_aborted_payload_destructors_are_staged_outside_owner_guard() {
    let (receiver, _) = receiver();
    let owner = Arc::new(Mutex::new(receiver));
    let drops = Arc::new(AtomicUsize::new(0));
    let pending = rooted_data(&owner, 3, &drops);
    let retired = rooted_data(&owner, 3, &drops);
    let invalid = rooted_data(&owner, 4, &drops);
    let ready = rooted_data(&owner, 1, &drops);
    let pending_effects = owner.lock().unwrap().process(pending, false, ms(0));
    pending_effects.dispatch().unwrap();
    let duplicate_effects = owner.lock().unwrap().process(retired, false, ms(1));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    duplicate_effects.dispatch().unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let invalid_effects = owner.lock().unwrap().process(invalid, true, ms(2));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        invalid_effects.dispatch(),
        Err(CopyContextError::InvalidTerminal(4))
    );
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    let admitted_effects = owner.lock().unwrap().process(ready, false, ms(3));
    admitted_effects.dispatch().unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    let close = owner.lock().unwrap().close(-31, ms(4));
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    close.dispatch().unwrap();
    // One queued data item and one still-gapped item retire on error close.
    assert_eq!(drops.load(Ordering::SeqCst), 4);
}

#[test]
fn rejected_open_borrows_transport_so_its_last_root_is_released_outside_guard() {
    let (receiver, _) = receiver();
    let owner = Arc::new(Mutex::new(receiver));
    let drops = Arc::new(AtomicUsize::new(0));
    let root = PayloadProbe {
        owner: Arc::downgrade(&owner),
        drops: drops.clone(),
    };
    let send: SendContextAck = Arc::new(move |_, _| {
        let _root = &root;
        true
    });
    assert_eq!(
        owner.lock().unwrap().open(&send, ms(0)),
        Err(CopyContextError::AlreadyOpen)
    );
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(send);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

struct WakeProbe {
    owner: Weak<Mutex<CopyContextReceiver>>,
    wakes: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl Wake for WakeProbe {
    fn wake(self: Arc<Self>) {
        assert_unlocked(&self.owner);
        let owner = self.owner.upgrade().unwrap();
        assert_eq!(owner.lock().unwrap().completed(), 1);
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }
}

impl Drop for WakeProbe {
    fn drop(&mut self) {
        assert_unlocked(&self.owner);
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn pending_waker_is_rooted_until_effect_delivery_and_reenters_only_outside_guard() {
    let (receiver, _) = receiver();
    let owner = Arc::new(Mutex::new(receiver));
    let mut stream = owner.lock().unwrap().stream();
    let wakes = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let wake = Arc::new(WakeProbe {
        owner: Arc::downgrade(&owner),
        wakes: wakes.clone(),
        drops: drops.clone(),
    });
    let weak_wake = Arc::downgrade(&wake);
    let waker = Waker::from(wake);
    assert!(
        Pin::new(&mut stream)
            .poll_next(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(waker);
    let effects = owner.lock().unwrap().process(data(1, b"one"), false, ms(0));
    assert!(weak_wake.upgrade().is_some());
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    effects.dispatch().unwrap();
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(weak_wake.upgrade().is_none());
    assert_eq!(item(&mut stream).as_ref(), b"one");
}

#[test]
fn error_close_wakes_pending_context_reader_only_when_effects_are_dispatched() {
    let (mut receiver, _) = receiver();
    receive(&mut receiver, 1, b"one", 0);
    let owner = Arc::new(Mutex::new(receiver));
    let mut stream = owner.lock().unwrap().stream();
    assert_eq!(item(&mut stream).as_ref(), b"one");
    let wakes = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let waker = Waker::from(Arc::new(WakeProbe {
        owner: Arc::downgrade(&owner),
        wakes: wakes.clone(),
        drops: drops.clone(),
    }));
    assert!(
        Pin::new(&mut stream)
            .poll_next(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(waker);
    let effects = owner.lock().unwrap().close(-37, ms(1));
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    effects.dispatch().unwrap();
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        poll(&mut stream),
        Poll::Ready(Some(Err(CopyContextError::EnumeratorClosed(-37))))
    );
    eof(&mut stream);
}

#[test]
fn transport_callback_reenters_owner_and_ack_snapshot_only_after_guards_release() {
    let mut settings = config();
    settings.batch_ack_interval = Duration::ZERO;
    let owner = Arc::new(Mutex::new(CopyContextReceiver::new(settings).unwrap()));
    let weak = Arc::downgrade(&owner);
    let snapshot = owner.lock().unwrap().snapshot.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = calls.clone();
    let send: SendContextAck = Arc::new(move |current, _| {
        assert_unlocked(&weak);
        assert!(snapshot.try_lock().is_ok());
        let owner = weak.upgrade().unwrap();
        let owner = owner.lock().unwrap();
        assert_eq!(owner.last_ack_sent(), current.completed);
        assert_eq!(owner.current_ack(), current);
        callback_calls.fetch_add(1, Ordering::SeqCst);
        true
    });
    owner.lock().unwrap().open(&send, ms(0)).unwrap();
    let effects = owner.lock().unwrap().process(data(1, b"one"), false, ms(1));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    effects.dispatch().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn staged_ack_roots_transport_after_owner_drop_and_observes_later_close_error() {
    let mut settings = config();
    settings.batch_ack_interval = Duration::ZERO;
    let mut receiver = CopyContextReceiver::new(settings).unwrap();
    let root = Arc::new(());
    let weak_root = Arc::downgrade(&root);
    let sent = Arc::new(Mutex::new(Vec::new()));
    let callback_sent = sent.clone();
    let send: SendContextAck = Arc::new(move |current, _| {
        let _root = &root;
        callback_sent.lock().unwrap().push(current);
        false
    });
    receiver.open(&send, ms(0)).unwrap();
    drop(send);
    let arrival = receiver.process(data(1, b"one"), false, ms(1));
    let close = receiver.close(-41, ms(2));
    drop(receiver);
    assert!(weak_root.upgrade().is_some());
    arrival.dispatch().unwrap();
    assert_eq!(*sent.lock().unwrap(), vec![ack(1, -41)]);
    close.dispatch().unwrap();
    assert_eq!(*sent.lock().unwrap(), vec![ack(1, -41), ack(1, -41)]);
    assert!(weak_root.upgrade().is_none());
}
