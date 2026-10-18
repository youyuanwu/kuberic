// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-component tests derived from SecondaryCopyReceiver.cpp:24-460,
// ReplicationSecondary.Test.cpp:1726-1911 and ReplicateCopyOperations.Test.cpp:
// 1049-1086, microsoft/service-fabric
// @3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d.

use super::*;

use std::pin::Pin;
use std::sync::Weak;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use bytes::Bytes;
use futures::Stream;
use futures::task::noop_waker_ref;

use crate::protocol::types::Epoch;
use crate::replicator::v1::buffers::{self, BufferLimits, BufferPolicy};
use crate::replicator::v1::operation_queue::QueueOperation;
use crate::replicator::v1::operation_stream::StreamEvent;

fn config(required: bool, eos: bool) -> SecondaryCopyConfig {
    SecondaryCopyConfig {
        initial_capacity: 4,
        max_capacity: 4,
        require_service_ack: required,
        enable_eos_ack: eos,
        copy_done: false,
    }
}

fn receiver(required: bool, eos: bool) -> SecondaryCopyReceiver {
    SecondaryCopyReceiver::new(config(required, eos)).unwrap()
}

fn data(lsn: Lsn, required: bool) -> ReceivedOperation {
    operation(lsn, required, Bytes::from_static(b"copy"))
}

fn empty(lsn: Lsn, required: bool) -> ReceivedOperation {
    operation(lsn, required, Bytes::new())
}

fn operation(lsn: Lsn, required: bool, bytes: Bytes) -> ReceivedOperation {
    ReceivedOperation::new(
        QueueOperation::new(lsn, bytes, true),
        Epoch::new(0, 0),
        required,
    )
}

fn terminal(lsn: Lsn, required: bool) -> ReceivedOperation {
    ReceivedOperation::copy_end_of_stream(lsn, required).unwrap()
}

fn root() -> Arc<dyn Send + Sync> {
    Arc::new(())
}

fn signals(effects: SecondaryCopyEffects) -> SecondaryCopySignals {
    effects.dispatch().unwrap()
}

fn receive(
    receiver: &mut SecondaryCopyReceiver,
    operation: ReceivedOperation,
    last: bool,
) -> SecondaryCopySignals {
    signals(receiver.receive(operation, last))
}

fn ack(
    receiver: &mut SecondaryCopyReceiver,
    operation: &ReceivedOperation,
) -> SecondaryCopySignals {
    let work = operation.acknowledge().unwrap().unwrap();
    signals(receiver.process_ack(work))
}

fn poll(stream: &mut OperationStream) -> Poll<Option<Result<StreamEvent, StreamError>>> {
    Pin::new(stream).poll_next(&mut Context::from_waker(noop_waker_ref()))
}

fn take(stream: &mut OperationStream) -> ReceivedOperation {
    match poll(stream) {
        Poll::Ready(Some(Ok(StreamEvent::Operation(operation)))) => operation,
        _ => panic!("expected copy operation"),
    }
}

fn assert_eof(stream: &mut OperationStream) {
    assert!(matches!(poll(stream), Poll::Ready(None)));
}

#[test]
fn copy_done_initialization_is_closed_eof_without_invented_terminal_or_progress() {
    for required in [false, true] {
        let mut settings = config(required, true);
        settings.copy_done = true;
        let mut receiver = SecondaryCopyReceiver::new(settings).unwrap();
        assert!(receiver.all_operations_received());
        assert!(receiver.copy_queue_closed());
        assert!(!receiver.is_copy_in_progress());
        assert!(!receiver.enqueued_end_of_stream());
        assert!(receiver.all_operations_acked());
        assert_eq!(receiver.pending_ack_progress(), 0);
        assert_eq!(
            receiver.capture_ack(),
            SecondaryCopyAck {
                received: -1,
                applied: -1
            }
        );
        assert_eq!(receiver.pending_ack_progress(), 0);
        assert_eof(&mut receiver.stream());
        let retry = receive(&mut receiver, terminal(1, required), true);
        assert_eq!(retry.admission, CopyAdmission::AdmissionClosed);
        assert!(retry.ack.is_some());
        assert!(!retry.check_copy_sender);
        assert_eof(&mut receiver.stream());
        assert!(matches!(
            receiver.enqueue_eos(&root()),
            Err(SecondaryCopyError::Stream(StreamError::Closed))
        ));
        assert!(!receiver.enqueued_end_of_stream());
    }
}

#[test]
fn count_only_configuration_rejects_invalid_bounds_without_queue_panics() {
    for (initial, max) in [(0, 4), (1, 4), (3, 4), (4, 0), (4, 3), (8, 4)] {
        assert!(matches!(
            SecondaryCopyReceiver::new(SecondaryCopyConfig {
                initial_capacity: initial,
                max_capacity: max,
                ..config(true, false)
            }),
            Err(SecondaryCopyError::InvalidCapacity)
        ));
    }
    let mut receiver = receiver(true, false);
    let large = operation(1, true, Bytes::from(vec![0_u8; 128 * 1024]));
    assert_eq!(
        receive(&mut receiver, large, false).admission,
        CopyAdmission::Admitted
    );
    assert_eq!(receiver.queue().unwrap().total_bytes(), 128 * 1024);
}

#[test]
fn required_and_optimistic_cursors_diverge_before_any_service_dequeue() {
    for required in [false, true] {
        let mut receiver = receiver(required, false);
        let first = data(1, required);
        let received = receive(&mut receiver, first.clone(), false);
        assert_eq!(
            received.ack,
            Some(CopyAckRequest {
                check_pending_limit: !required
            })
        );
        assert_eq!(
            receiver.capture_ack(),
            SecondaryCopyAck {
                received: 1,
                applied: if required { 0 } else { 1 },
            }
        );
        assert!(take(&mut receiver.stream()).same_instance(&first));
        if required {
            assert!(ack(&mut receiver, &first).ack.is_some());
        } else {
            assert!(first.acknowledge().unwrap().is_none());
        }
        assert_eq!(receiver.capture_ack().applied, 1);
    }
}

#[test]
fn native_gap_and_duplicate_trace_preserves_first_identity_and_order() {
    for required in [false, true] {
        let mut receiver = receiver(required, false);
        let second = data(2, required);
        let gap = receive(&mut receiver, second.clone(), false);
        assert_eq!(gap.admission, CopyAdmission::Admitted);
        assert!(!gap.progress_done);
        assert!(gap.ack.is_none());
        let duplicate = data(2, required);
        let retry = receive(&mut receiver, duplicate.clone(), false);
        assert_eq!(retry.admission, CopyAdmission::Duplicate);
        assert!(retry.ack.is_some());
        let mut stream = receiver.stream();
        assert!(poll(&mut stream).is_pending());
        let first = data(1, required);
        let ordered = receive(&mut receiver, first.clone(), false);
        assert!(ordered.progress_done);
        assert!(take(&mut stream).same_instance(&first));
        assert!(take(&mut stream).same_instance(&second));
        assert!(poll(&mut stream).is_pending());
        assert_eq!(receiver.capture_ack().received, 2);
        if required {
            let foreign = duplicate.acknowledge().unwrap().unwrap();
            assert!(matches!(
                receiver.process_ack(foreign).dispatch(),
                Err(SecondaryCopyError::ForeignAck)
            ));
            assert!(second.queue_operation().needs_ack());
            assert!(ack(&mut receiver, &second).ack.is_none());
            assert_eq!(receiver.capture_ack().applied, 0);
            let _ = ack(&mut receiver, &first);
        }
        assert_eq!(receiver.capture_ack().applied, 2);
    }
}

#[test]
fn native_full_terminal_retries_remember_last_but_never_invent_admission() {
    for required in [false, true] {
        let mut receiver = receiver(required, true);
        for lsn in [4, 2, 3] {
            assert!(
                receive(&mut receiver, data(lsn, required), false)
                    .ack
                    .is_none()
            );
        }
        let full = receive(&mut receiver, terminal(5, required), true);
        assert_eq!(full.admission, CopyAdmission::Full);
        assert!(full.ack.is_some());
        assert!(!receiver.all_operations_received());
        let first = data(1, required);
        let _ = receive(&mut receiver, first.clone(), false);
        assert_eq!(receiver.capture_ack().received, 4);
        let mut stream = receiver.stream();
        let mut operations = vec![take(&mut stream)];
        for expected in 2..=4 {
            let operation = take(&mut stream);
            assert_eq!(operation.queue_operation().lsn(), expected);
            operations.push(operation);
        }
        if required {
            assert_eq!(
                receive(&mut receiver, terminal(5, true), true).admission,
                CopyAdmission::Full
            );
            let _ = ack(&mut receiver, &first);
        }
        let eos = terminal(5, required);
        let admitted = receive(&mut receiver, eos.clone(), true);
        assert_eq!(admitted.admission, CopyAdmission::Admitted);
        assert!(admitted.dispatch_closed);
        assert!(receiver.all_operations_received());
        assert!(take(&mut stream).same_instance(&eos));
        assert_eof(&mut stream);
        if required {
            assert!(!admitted.check_copy_sender);
            assert!(!receiver.all_operations_acked());
            let _ = ack(&mut receiver, &eos);
            for operation in &operations[1..] {
                let completion = ack(&mut receiver, operation);
                assert_eq!(
                    completion.check_copy_sender,
                    operation.queue_operation().lsn() == 4
                );
            }
        } else {
            assert!(admitted.check_copy_sender);
        }
        assert!(!receiver.is_copy_in_progress());
        assert!(receiver.all_operations_acked());
        assert_eq!(receiver.capture_ack().applied, -1);
    }
}

#[test]
fn network_eos_before_gap_is_ordered_and_closes_only_at_contiguous_terminal() {
    for required in [false, true] {
        let mut receiver = receiver(required, true);
        let eos = terminal(3, required);
        let received = receive(&mut receiver, eos.clone(), true);
        assert!(received.ack.is_none());
        assert!(!receiver.enqueued_end_of_stream());
        assert!(!receiver.all_operations_received());
        let duplicate = receive(&mut receiver, terminal(3, required), true);
        assert_eq!(duplicate.admission, CopyAdmission::Duplicate);
        assert!(duplicate.ack.is_some());
        let mut stream = receiver.stream();
        assert!(poll(&mut stream).is_pending());
        let first = data(1, required);
        let second = empty(2, required);
        let _ = receive(&mut receiver, first.clone(), false);
        assert!(take(&mut stream).same_instance(&first));
        assert!(poll(&mut stream).is_pending());
        let closed = receive(&mut receiver, second.clone(), false);
        assert!(closed.dispatch_closed);
        assert!(receiver.enqueued_end_of_stream());
        let empty_data = take(&mut stream);
        assert!(empty_data.same_instance(&second));
        assert!(!empty_data.is_end_of_stream());
        assert!(empty_data.queue_operation().data().is_empty());
        assert!(take(&mut stream).same_instance(&eos));
        assert_eof(&mut stream);
        if required {
            assert_eq!(
                receiver.capture_ack(),
                SecondaryCopyAck {
                    received: -1,
                    applied: 0
                }
            );
            assert!(!ack(&mut receiver, &eos).check_copy_sender);
            let _ = ack(&mut receiver, &second);
            assert!(ack(&mut receiver, &first).check_copy_sender);
        }
        assert!(receiver.all_operations_acked());
    }
}

#[test]
fn legacy_empty_last_is_protocol_only_and_retry_before_gap_is_idempotent() {
    for required in [false, true] {
        let mut receiver = receiver(required, false);
        for _ in 0..2 {
            let last = receive(&mut receiver, empty(3, required), true);
            assert_eq!(last.admission, CopyAdmission::TerminalOnly);
            assert!(last.ack.is_some());
            assert!(!last.dispatch_closed);
            assert_eq!(receiver.last_copy_lsn, 2);
        }
        let first = empty(1, required);
        let second = data(2, required);
        let _ = receive(&mut receiver, first.clone(), false);
        let done = receive(&mut receiver, second.clone(), false);
        assert!(done.dispatch_closed);
        let mut stream = receiver.stream();
        assert!(take(&mut stream).same_instance(&first));
        assert!(take(&mut stream).same_instance(&second));
        assert_eof(&mut stream);
        if required {
            let _ = ack(&mut receiver, &second);
            assert!(ack(&mut receiver, &first).check_copy_sender);
            // Native terminal bookkeeping is independent of EOS availability:
            // legacy mode has no EOS callback to satisfy this particular wait.
            assert!(!receiver.all_operations_acked());
            let _ = signals(receiver.clean_queue_on_close(&root(), true));
        }
        assert!(receiver.all_operations_acked());
        assert!(!receiver.is_copy_in_progress());
    }
}

#[test]
fn canonical_network_eos_is_protocol_only_when_legacy_eos_ack_policy_is_disabled() {
    for required in [false, true] {
        let mut receiver = receiver(required, false);
        let first = data(1, required);
        let _ = receive(&mut receiver, first.clone(), false);
        let canonical = terminal(2, required);
        let done = receive(&mut receiver, canonical.clone(), true);
        assert_eq!(done.admission, CopyAdmission::TerminalOnly);
        assert!(done.progress_done);
        assert!(done.dispatch_closed);
        assert_eq!(done.check_copy_sender, !required);
        assert_eq!(
            done.ack,
            Some(CopyAckRequest {
                check_pending_limit: !required
            })
        );
        assert_eq!(receiver.last_copy_lsn, 1);
        assert!(receiver.all_operations_received());
        assert!(receiver.enqueued_end_of_stream());
        assert_eq!(canonical.ack_state(), AckState::NotCalled);
        assert!(canonical.queue_operation().needs_ack());
        let mut stream = receiver.stream();
        assert!(take(&mut stream).same_instance(&first));
        assert_eof(&mut stream);
        if required {
            assert_eq!(
                receiver.capture_ack(),
                SecondaryCopyAck {
                    received: -1,
                    applied: 0
                }
            );
            assert!(ack(&mut receiver, &first).check_copy_sender);
            assert!(!receiver.all_operations_acked());
            let _ = signals(receiver.clean_queue_on_close(&root(), true));
        }
        assert!(!receiver.is_copy_in_progress());
        assert!(receiver.all_operations_acked());
        assert_eq!(canonical.ack_state(), AckState::NotCalled);
    }
}

#[test]
fn empty_first_terminal_has_no_data_and_nonempty_legacy_last_remains_data() {
    for required in [false, true] {
        let mut no_data = receiver(required, false);
        let done = receive(&mut no_data, empty(1, required), true);
        assert!(done.dispatch_closed);
        assert!(done.check_copy_sender);
        assert_eof(&mut no_data.stream());
        let mut nonempty = receiver(required, false);
        let last = data(1, required);
        let done = receive(&mut nonempty, last.clone(), true);
        assert!(done.dispatch_closed);
        let mut stream = nonempty.stream();
        assert!(take(&mut stream).same_instance(&last));
        assert_eof(&mut stream);
        if required {
            assert!(ack(&mut nonempty, &last).check_copy_sender);
        }
    }
}

#[test]
fn legacy_terminal_uses_logical_payload_not_adapter_frame_overhead() {
    let limits = BufferLimits {
        max_segments: 4,
        max_payload_bytes: 16,
    };
    let frame = buffers::encode(
        &[Bytes::new(), Bytes::new()],
        BufferPolicy::FramedSegments,
        limits,
    )
    .unwrap();
    assert!(!frame.is_empty());
    let mut receiver = receiver(false, false);
    let operation = ReceivedOperation::new(
        QueueOperation::from_data(1, frame, true, BufferPolicy::FramedSegments, limits).unwrap(),
        Epoch::new(0, 0),
        false,
    );
    let done = receive(&mut receiver, operation, true);
    assert_eq!(done.admission, CopyAdmission::TerminalOnly);
    assert!(done.dispatch_closed);
    assert_eof(&mut receiver.stream());
}

#[test]
fn terminal_retries_after_done_request_ack_but_never_replay_or_reopen() {
    for required in [false, true] {
        let mut receiver = receiver(required, true);
        let last = terminal(1, required);
        let _ = receive(&mut receiver, last.clone(), true);
        let mut stream = receiver.stream();
        assert!(take(&mut stream).same_instance(&last));
        assert_eof(&mut stream);
        for _ in 0..2 {
            let retry = receive(&mut receiver, terminal(1, required), true);
            assert_eq!(retry.admission, CopyAdmission::AdmissionClosed);
            assert!(retry.ack.is_some());
            assert!(!retry.progress_done);
            assert!(!retry.check_copy_sender);
        }
        if required {
            let _ = ack(&mut receiver, &last);
        }
        assert_eof(&mut receiver.stream());
    }
}

#[test]
fn getter_handles_consume_one_queue_and_terminal_reacquisition_is_permanent() {
    let mut receiver = receiver(false, true);
    let _ = receive(&mut receiver, data(1, false), false);
    let _ = receive(&mut receiver, data(2, false), false);
    let _ = receive(&mut receiver, terminal(3, false), true);
    let mut first = receiver.stream();
    let mut second = receiver.stream();
    assert_eq!(take(&mut first).queue_operation().lsn(), 1);
    assert_eq!(take(&mut second).queue_operation().lsn(), 2);
    drop(first);
    let mut reacquired = receiver.stream();
    assert!(take(&mut reacquired).is_end_of_stream());
    assert_eof(&mut second);
    assert_eof(&mut reacquired);
    assert_eof(&mut receiver.stream());
}

#[test]
fn pending_progress_tracks_completion_and_native_final_sentinel_transition() {
    for required in [false, true] {
        let mut receiver = receiver(required, true);
        assert_eq!(receiver.capture_ack().applied, 0);
        let first = data(1, required);
        let _ = receive(&mut receiver, first.clone(), false);
        assert_eq!(
            receiver.pending_ack_progress(),
            if required { 0 } else { 1 }
        );
        if required {
            let _ = ack(&mut receiver, &first);
        }
        assert_eq!(receiver.pending_ack_progress(), 1);
        assert_eq!(receiver.capture_ack().applied, 1);
        assert_eq!(receiver.pending_ack_progress(), 0);
        let eos = terminal(2, required);
        let _ = receive(&mut receiver, eos.clone(), true);
        if required {
            assert_eq!(receiver.capture_ack().received, -1);
            let _ = ack(&mut receiver, &eos);
        }
        // Native GetAckProgress adds one for the transition to copy-complete.
        assert_eq!(receiver.pending_ack_progress(), 2);
        assert_eq!(
            receiver.capture_ack(),
            SecondaryCopyAck {
                received: -1,
                applied: -1
            }
        );
        assert_eq!(receiver.pending_ack_progress(), 0);
    }
}

#[test]
fn selected_ack_blocks_until_real_start_and_finish_is_an_outside_guard_effect() {
    let mut receiver = receiver(true, false);
    let first = data(1, true);
    let _ = receive(&mut receiver, first.clone(), false);
    let work = first.acknowledge().unwrap().unwrap();
    assert_eq!(first.ack_state(), AckState::ReadyToRun);
    assert!(first.queue_operation().needs_ack());
    assert!(!receiver.queue.as_mut().unwrap().complete());
    let effects = receiver.process_ack(work);
    assert_eq!(first.ack_state(), AckState::Running);
    assert!(!first.queue_operation().needs_ack());
    assert_eq!(receiver.capture_ack().applied, 1);
    assert!(signals(effects).ack.is_some());
    assert_eq!(first.ack_state(), AckState::Completed);
}

#[test]
fn accepted_but_undispatched_gap_ack_cannot_release_its_completion_predicate() {
    let mut receiver = receiver(true, true);
    let eos = terminal(2, true);
    let _ = receive(&mut receiver, eos.clone(), true);
    let work = eos.acknowledge().unwrap().unwrap();
    assert!(matches!(
        receiver.process_ack(work).dispatch(),
        Err(SecondaryCopyError::ForeignAck)
    ));
    assert!(eos.queue_operation().needs_ack());
    assert_eq!(eos.ack_state(), AckState::ReadyToRun);
    assert_eq!(receiver.capture_ack().applied, 0);
    assert!(!receiver.enqueued_end_of_stream());
}

#[test]
fn abandoned_selected_ack_is_not_cancelled_or_completed() {
    let mut receiver = receiver(true, true);
    let eos = terminal(1, true);
    let _ = receive(&mut receiver, eos.clone(), true);
    drop(eos.acknowledge().unwrap().unwrap());
    assert_eq!(eos.ack_state(), AckState::ReadyToRun);
    assert!(eos.queue_operation().needs_ack());
    assert_eq!(receiver.capture_ack().applied, 0);
    assert!(!receiver.all_operations_acked());
    assert!(receiver.is_copy_in_progress());
}

#[test]
fn explicit_queue_close_keeps_committed_ack_work_and_hands_off_without_copy_success() {
    let mut receiver = receiver(true, true);
    let first = data(1, true);
    let _ = receive(&mut receiver, first.clone(), false);
    receiver.close_copy_operation_queue();
    let closed = receive(&mut receiver, data(2, true), false);
    assert_eq!(closed.admission, CopyAdmission::AdmissionClosed);
    assert!(closed.ack.is_some());
    let completed = ack(&mut receiver, &first);
    assert!(completed.check_copy_sender);
    assert!(receiver.is_copy_in_progress());
    assert!(!receiver.all_operations_received());
    assert!(receiver.all_operations_acked());
    let mut stream = receiver.stream();
    assert!(take(&mut stream).same_instance(&first));
    assert!(poll(&mut stream).is_pending());
}

#[test]
fn discard_removes_gaps_but_never_cancels_committed_service_callbacks() {
    for required in [false, true] {
        let mut receiver = receiver(required, false);
        let first = data(1, required);
        let fourth = data(4, required);
        let _ = receive(&mut receiver, first.clone(), false);
        let _ = receive(&mut receiver, fourth.clone(), false);
        let _ = signals(receiver.discard_out_of_order());
        assert!(receiver.queue().unwrap().get_operation(4).is_none());
        assert_eq!(first.ack_state(), AckState::NotCalled);
        assert_eq!(fourth.ack_state(), AckState::NotCalled);
        assert_eq!(receiver.capture_ack().received, 1);
        assert!(!receiver.copy_queue_closed());
        let mut stream = receiver.stream();
        assert!(take(&mut stream).same_instance(&first));
        assert!(poll(&mut stream).is_pending());
        let _ = signals(receiver.clean_queue_on_close(&root(), false));
        assert!(receiver.copy_queue_closed());
        assert_eq!(receiver.capture_ack().applied, if required { 0 } else { 1 });
        assert_eq!(
            first.ack_state(),
            if required {
                AckState::Cancelled
            } else {
                AckState::NotCalled
            }
        );
    }
}

#[test]
fn close_cleanup_cancels_only_unstarted_data_preserves_selected_roots_and_stream() {
    let mut receiver = receiver(true, false);
    let first = data(1, true);
    let second = data(2, true);
    let fourth = data(4, true);
    for operation in [&first, &second, &fourth] {
        let _ = receive(&mut receiver, operation.clone(), false);
    }
    let work = first.acknowledge().unwrap().unwrap();
    let owner: Arc<dyn Send + Sync> = Arc::new(());
    let weak = Arc::downgrade(&owner);
    let _ = signals(receiver.clean_queue_on_close(&owner, false));
    drop(owner);
    assert!(weak.upgrade().is_some());
    assert!(receiver.copy_queue_closed());
    assert_eq!(receiver.queue().unwrap().operation_count(), 0);
    assert_eq!(first.ack_state(), AckState::ReadyToRun);
    assert_eq!(second.ack_state(), AckState::Cancelled);
    assert_eq!(fourth.ack_state(), AckState::NotCalled);
    assert!(second.acknowledge().unwrap().is_none());
    let mut stream = receiver.stream();
    assert!(take(&mut stream).same_instance(&first));
    assert!(take(&mut stream).same_instance(&second));
    assert!(poll(&mut stream).is_pending());
    let effects = receiver.process_ack(work);
    assert!(weak.upgrade().is_some());
    let completed = signals(effects);
    assert!(weak.upgrade().is_none());
    assert!(completed.ack.is_none());
    assert!(!completed.check_copy_sender);
    assert_eq!(receiver.capture_ack().applied, 0);
}

#[test]
fn late_retired_callbacks_validate_identity_before_releasing_any_queue_predicate() {
    let mut retired = receiver(true, false);
    let mut live = receiver(true, false);
    let old = data(1, true);
    let foreign = data(1, true);
    let _ = receive(&mut retired, old.clone(), false);
    let _ = receive(&mut live, foreign.clone(), false);
    let old_work = old.acknowledge().unwrap().unwrap();
    let _ = signals(retired.clean_queue_on_close(&root(), false));
    let foreign_work = foreign.acknowledge().unwrap().unwrap();
    assert!(matches!(
        retired.process_ack(foreign_work).dispatch(),
        Err(SecondaryCopyError::ForeignAck)
    ));
    assert_eq!(foreign.ack_state(), AckState::ReadyToRun);
    assert!(foreign.queue_operation().needs_ack());
    assert_eq!(live.capture_ack().applied, 0);
    let completed = signals(retired.process_ack(old_work));
    assert!(completed.ack.is_none());
    assert_eq!(old.ack_state(), AckState::Completed);
    assert!(foreign.queue_operation().needs_ack());
}

#[test]
fn close_and_stream_fault_preserve_eos_wait_update_failure_ignores_only_wait() {
    for transition in 0..3 {
        let mut receiver = receiver(true, true);
        let eos = terminal(1, true);
        let _ = receive(&mut receiver, eos.clone(), true);
        let owner: Arc<dyn Send + Sync> = Arc::new(());
        let weak = Arc::downgrade(&owner);
        let effects = match transition {
            0 => receiver.clean_queue_on_close(&owner, false),
            1 => receiver.on_stream_report_fault(&owner),
            _ => receiver.clean_queue_on_update_epoch_failure(&owner),
        };
        let _ = signals(effects);
        drop(owner);
        assert!(weak.upgrade().is_some());
        assert_eq!(receiver.stream_faulted(), transition == 1);
        assert_eq!(receiver.all_operations_acked(), transition == 2);
        assert_eq!(eos.ack_state(), AckState::NotCalled);
        assert!(eos.queue_operation().needs_ack());
        let foreign = terminal(1, true);
        assert!(matches!(
            receiver
                .process_ack(foreign.acknowledge().unwrap().unwrap())
                .dispatch(),
            Err(SecondaryCopyError::ForeignAck)
        ));
        assert!(eos.queue_operation().needs_ack());
        let mut stream = receiver.stream();
        assert!(take(&mut stream).same_instance(&eos));
        // Cleanup neither aborts nor drains: terminal EOF was already queued.
        assert_eof(&mut stream);
        let completed = ack(&mut receiver, &eos);
        assert!(completed.ack.is_none());
        assert!(weak.upgrade().is_none());
        assert!(receiver.all_operations_acked());
    }
}

#[test]
fn local_stream_fault_closes_admission_without_host_report_or_dispatch_abort() {
    let mut receiver = receiver(true, true);
    let first = data(1, true);
    let _ = receive(&mut receiver, first.clone(), false);
    let _ = signals(receiver.on_stream_report_fault(&root()));
    assert!(receiver.stream_faulted());
    assert!(receiver.copy_queue_closed());
    assert!(!receiver.all_operations_received());
    assert_eq!(first.ack_state(), AckState::Cancelled);
    let mut stream = receiver.stream();
    assert!(take(&mut stream).same_instance(&first));
    assert!(poll(&mut stream).is_pending());
    let _ = signals(receiver.enqueue_eos(&root()).unwrap());
    let eos = take(&mut stream);
    assert!(eos.is_end_of_stream());
    assert!(!receiver.all_operations_acked());
    let _ = ack(&mut receiver, &eos);
    assert!(receiver.all_operations_acked());
    // Standalone EOS is an operation, not the stream's actual EOF.
    assert!(poll(&mut stream).is_pending());
}

#[test]
fn standalone_eos_is_once_only_rooted_and_foreign_eos_cannot_satisfy_its_wait() {
    let mut receiver = receiver(true, true);
    let owner: Arc<dyn Send + Sync> = Arc::new(());
    let weak = Arc::downgrade(&owner);
    let _ = signals(receiver.enqueue_eos(&owner).unwrap());
    let _ = signals(receiver.enqueue_eos(&owner).unwrap());
    drop(owner);
    let mut stream = receiver.stream();
    let eos = take(&mut stream);
    assert_eq!(eos.queue_operation().lsn(), 0);
    assert!(eos.is_end_of_stream());
    assert!(poll(&mut stream).is_pending());
    assert!(weak.upgrade().is_some());
    let foreign = ReceivedOperation::end_of_stream(true);
    assert!(matches!(
        receiver
            .process_ack(foreign.acknowledge().unwrap().unwrap())
            .dispatch(),
        Err(SecondaryCopyError::ForeignAck)
    ));
    assert!(eos.queue_operation().needs_ack());
    assert!(!receiver.all_operations_acked());
    let _ = ack(&mut receiver, &eos);
    assert!(weak.upgrade().is_none());
    assert!(receiver.all_operations_acked());
    assert!(poll(&mut stream).is_pending());
}

#[test]
fn optimistic_standalone_eos_releases_its_root_on_actual_ack_without_callback_work() {
    let mut receiver = receiver(false, true);
    let owner: Arc<dyn Send + Sync> = Arc::new(());
    let weak = Arc::downgrade(&owner);
    let _ = signals(receiver.enqueue_eos(&owner).unwrap());
    drop(owner);
    let eos = take(&mut receiver.stream());
    assert!(weak.upgrade().is_some());
    assert!(eos.acknowledge().unwrap().is_none());
    assert!(weak.upgrade().is_none());
    assert!(receiver.all_operations_acked());
}

#[test]
fn malformed_operations_leave_terminal_cursor_queue_and_stream_untouched() {
    let mut receiver = receiver(true, true);
    let released = data(1, true);
    released.queue_operation().release_ack_requirement();
    let selected = data(1, true);
    drop(selected.acknowledge().unwrap().unwrap());
    for (operation, last) in [
        (data(0, true), false),
        (data(-1, true), false),
        (data(Lsn::MAX, true), true),
        (data(1, false), false),
        (data(1, true), true),
        (terminal(1, true), false),
        (released, false),
        (selected, false),
        (
            ReceivedOperation::new(
                QueueOperation::new(1, Bytes::new(), true),
                Epoch::new(-1, 0),
                true,
            ),
            false,
        ),
    ] {
        assert!(matches!(
            receiver.receive(operation, last).dispatch(),
            Err(SecondaryCopyError::InvalidOperation(_))
        ));
        assert_eq!(receiver.last_copy_lsn, -1);
        assert_eq!(receiver.queue().unwrap().operation_count(), 0);
        assert!(!receiver.all_operations_received());
        assert!(poll(&mut receiver.stream()).is_pending());
    }
    let mut disabled = SecondaryCopyReceiver::new(config(false, false)).unwrap();
    assert!(matches!(
        disabled.enqueue_eos(&root()),
        Err(SecondaryCopyError::EosDisabled)
    ));
}

struct ReentrantWake {
    receiver: Arc<Mutex<SecondaryCopyReceiver>>,
    called: AtomicUsize,
}

impl Wake for ReentrantWake {
    fn wake(self: Arc<Self>) {
        assert!(self.receiver.try_lock().is_ok());
        self.called.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn stream_wakeup_reenters_receiver_only_when_effects_are_dispatched_outside_guard() {
    let receiver = Arc::new(Mutex::new(receiver(false, false)));
    let mut stream = receiver.lock().unwrap().stream();
    let observer = Arc::new(ReentrantWake {
        receiver: receiver.clone(),
        called: AtomicUsize::new(0),
    });
    let waker = Waker::from(observer.clone());
    assert!(
        Pin::new(&mut stream)
            .poll_next(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let effects = {
        let mut guard = receiver.lock().unwrap();
        let effects = guard.receive(data(1, false), false);
        assert_eq!(observer.called.load(Ordering::SeqCst), 0);
        effects
    };
    let _ = signals(effects);
    assert_eq!(observer.called.load(Ordering::SeqCst), 1);
    assert_eq!(take(&mut stream).queue_operation().lsn(), 1);
}

struct ReentrantRoot {
    receiver: Arc<Mutex<SecondaryCopyReceiver>>,
    dropped: Arc<AtomicUsize>,
}

impl Drop for ReentrantRoot {
    fn drop(&mut self) {
        assert!(self.receiver.try_lock().is_ok());
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn late_ack_root_destructor_reenters_receiver_only_after_effect_delivery() {
    let receiver = Arc::new(Mutex::new(receiver(true, false)));
    let first = data(1, true);
    let effects = receiver.lock().unwrap().receive(first.clone(), false);
    let _ = signals(effects);
    let work = first.acknowledge().unwrap().unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    let owner: Arc<dyn Send + Sync> = Arc::new(ReentrantRoot {
        receiver: receiver.clone(),
        dropped: dropped.clone(),
    });
    let effects = receiver.lock().unwrap().clean_queue_on_close(&owner, false);
    let _ = signals(effects);
    drop(owner);
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    let effects = {
        let mut guard = receiver.lock().unwrap();
        let effects = guard.process_ack(work);
        assert_eq!(first.ack_state(), AckState::Running);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        effects
    };
    let _ = signals(effects);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(first.ack_state(), AckState::Completed);
}

#[test]
fn repeated_optimistic_eos_cleanup_never_replaces_or_releases_the_original_root() {
    let receiver = Arc::new(Mutex::new(receiver(false, true)));
    let original_dropped = Arc::new(AtomicUsize::new(0));
    let owner: Arc<dyn Send + Sync> = Arc::new(ReentrantRoot {
        receiver: receiver.clone(),
        dropped: original_dropped.clone(),
    });
    let effects = receiver.lock().unwrap().enqueue_eos(&owner).unwrap();
    let _ = signals(effects);
    drop(owner);
    assert_eq!(original_dropped.load(Ordering::SeqCst), 0);

    let replacement_dropped = Arc::new(AtomicUsize::new(0));
    let replacement: Arc<dyn Send + Sync> = Arc::new(ReentrantRoot {
        receiver: receiver.clone(),
        dropped: replacement_dropped.clone(),
    });
    for _ in 0..2 {
        let effects = {
            let mut guard = receiver.lock().unwrap();
            let effects = vec![
                guard.clean_queue_on_close(&replacement, false),
                guard.clean_queue_on_update_epoch_failure(&replacement),
                guard.on_stream_report_fault(&replacement),
                guard.enqueue_eos(&replacement).unwrap(),
            ];
            assert!(guard.rooted_callbacks.is_empty());
            assert!(guard.cleanup_callbacks.is_empty());
            assert_eq!(original_dropped.load(Ordering::SeqCst), 0);
            assert_eq!(replacement_dropped.load(Ordering::SeqCst), 0);
            effects
        };
        for effect in effects {
            let _ = signals(effect);
        }
    }
    drop(replacement);
    assert_eq!(replacement_dropped.load(Ordering::SeqCst), 1);
    assert_eq!(original_dropped.load(Ordering::SeqCst), 0);

    let eos = take(&mut receiver.lock().unwrap().stream());
    assert!(eos.acknowledge().unwrap().is_none());
    assert_eq!(original_dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn replacing_optimistic_local_eos_stages_its_reentrant_owner_release_outside_guard() {
    let receiver = Arc::new(Mutex::new(receiver(false, true)));
    let dropped = Arc::new(AtomicUsize::new(0));
    let owner: Arc<dyn Send + Sync> = Arc::new(ReentrantRoot {
        receiver: receiver.clone(),
        dropped: dropped.clone(),
    });
    let effects = receiver.lock().unwrap().enqueue_eos(&owner).unwrap();
    let _ = signals(effects);
    drop(owner);
    let mut stream = receiver.lock().unwrap().stream();
    let local_eos = take(&mut stream);
    assert!(local_eos.is_end_of_stream());
    drop(local_eos);
    assert_eq!(dropped.load(Ordering::SeqCst), 0);

    // A host normally closes copy admission before injecting its local EOS.
    // The bounded API must still safely retire roots if it receives data later.
    let network_eos = terminal(1, false);
    let effects = {
        let mut guard = receiver.lock().unwrap();
        let effects = guard.receive(network_eos.clone(), true);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        effects
    };
    let _ = signals(effects);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(take(&mut stream).same_instance(&network_eos));
    assert_eof(&mut stream);
}

struct PayloadProbe {
    receiver: Weak<Mutex<SecondaryCopyReceiver>>,
    dropped: Arc<AtomicUsize>,
}

impl AsRef<[u8]> for PayloadProbe {
    fn as_ref(&self) -> &[u8] {
        b"owned copy payload"
    }
}

impl Drop for PayloadProbe {
    fn drop(&mut self) {
        let receiver = self.receiver.upgrade().unwrap();
        assert!(receiver.try_lock().is_ok());
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

fn owned_payload(
    receiver: &Arc<Mutex<SecondaryCopyReceiver>>,
    dropped: &Arc<AtomicUsize>,
) -> Bytes {
    Bytes::from_owner(PayloadProbe {
        receiver: Arc::downgrade(receiver),
        dropped: dropped.clone(),
    })
}

#[test]
fn invalid_receive_retires_last_payload_owner_and_delivers_error_outside_guard() {
    let receiver = Arc::new(Mutex::new(receiver(true, true)));
    let dropped = Arc::new(AtomicUsize::new(0));
    let invalid = operation(1, false, owned_payload(&receiver, &dropped));
    let effects = {
        let mut guard = receiver.lock().unwrap();
        let effects = guard.receive(invalid, true);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        assert_eq!(guard.last_copy_lsn, -1);
        assert!(!guard.all_operations_received());
        assert!(!guard.enqueued_end_of_stream());
        assert_eq!(guard.queue().unwrap().operation_count(), 0);
        effects
    };
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    assert!(matches!(
        effects.dispatch(),
        Err(SecondaryCopyError::InvalidOperation(1))
    ));
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(poll(&mut receiver.lock().unwrap().stream()).is_pending());
}

#[test]
fn foreign_ack_retires_last_callback_root_and_payload_without_starting_under_guard() {
    let receiver = Arc::new(Mutex::new(receiver(true, false)));
    let live = data(1, true);
    let effects = receiver.lock().unwrap().receive(live.clone(), false);
    let _ = signals(effects);

    let payload_dropped = Arc::new(AtomicUsize::new(0));
    let foreign = operation(1, true, owned_payload(&receiver, &payload_dropped));
    let predicate = Arc::downgrade(foreign.queue_operation());
    let work = foreign.acknowledge().unwrap().unwrap();
    let root_dropped = Arc::new(AtomicUsize::new(0));
    let owner: Arc<dyn Send + Sync> = Arc::new(ReentrantRoot {
        receiver: receiver.clone(),
        dropped: root_dropped.clone(),
    });
    foreign.ignore_ack_and_keep_owner(owner.clone());
    drop(owner);
    drop(foreign);

    let effects = {
        let mut guard = receiver.lock().unwrap();
        let effects = guard.process_ack(work);
        assert_eq!(
            effects
                .retired_ack_work
                .as_ref()
                .unwrap()
                .operation()
                .ack_state(),
            AckState::ReadyToRun
        );
        assert!(predicate.upgrade().unwrap().needs_ack());
        assert!(live.queue_operation().needs_ack());
        assert_eq!(guard.capture_ack().applied, 0);
        assert_eq!(payload_dropped.load(Ordering::SeqCst), 0);
        assert_eq!(root_dropped.load(Ordering::SeqCst), 0);
        effects
    };
    assert_eq!(payload_dropped.load(Ordering::SeqCst), 0);
    assert_eq!(root_dropped.load(Ordering::SeqCst), 0);
    assert!(matches!(
        effects.dispatch(),
        Err(SecondaryCopyError::ForeignAck)
    ));
    assert_eq!(payload_dropped.load(Ordering::SeqCst), 1);
    assert_eq!(root_dropped.load(Ordering::SeqCst), 1);
    assert!(predicate.upgrade().is_none());
    assert!(live.queue_operation().needs_ack());
    assert_eq!(receiver.lock().unwrap().capture_ack().applied, 0);
}
