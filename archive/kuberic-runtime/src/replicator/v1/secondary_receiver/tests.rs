// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-component tests derived from SecondaryReplicationReceiver.cpp and
// selected ReplicationSecondary.Test.cpp traces at the documented native pin.

use super::*;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use futures::task::noop_waker_ref;

use crate::replicator::v1::buffers::{BufferLimits, BufferPolicy};
use crate::replicator::v1::operation_queue::QueueOperation;
use crate::replicator::v1::operation_stream::StreamEvent;

fn config(persisted: bool, required: bool) -> ReceiverConfig {
    ReceiverConfig {
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
        enable_eos_ack: false,
    }
}

fn receiver(required: bool) -> SecondaryReceiver {
    let mut receiver = SecondaryReceiver::new(config(required, false));
    receiver.initialize(10, epoch(100)).unwrap();
    receiver
}

fn epoch(configuration: i64) -> Epoch {
    Epoch::new(1, configuration)
}

fn operation(lsn: Lsn, configuration: i64, required: bool) -> ReceivedOperation {
    ReceivedOperation::new(
        QueueOperation::new(lsn, Bytes::from_static(b"x"), true),
        epoch(configuration),
        required,
    )
}

fn signals(effects: ReceiverEffects) -> ReceiverSignals {
    effects.dispatch().unwrap()
}

fn receive(
    receiver: &mut SecondaryReceiver,
    operations: Vec<ReceivedOperation>,
) -> ReceiverSignals {
    signals(receiver.receive(epoch(100), operations, None).unwrap())
}

fn ack(receiver: &mut SecondaryReceiver, operation: &ReceivedOperation) -> ReceiverSignals {
    let work = operation.acknowledge().unwrap().unwrap();
    signals(receiver.process_ack(work).unwrap())
}

fn poll(stream: &mut OperationStream) -> Poll<Option<Result<StreamEvent, StreamError>>> {
    Pin::new(stream).poll_next(&mut Context::from_waker(noop_waker_ref()))
}

fn take_operation(stream: &mut OperationStream) -> ReceivedOperation {
    match poll(stream) {
        Poll::Ready(Some(Ok(StreamEvent::Operation(operation)))) => operation,
        _ => panic!("expected replication operation"),
    }
}

fn take_barrier(stream: &mut OperationStream) -> EpochBarrier {
    match poll(stream) {
        Poll::Ready(Some(Ok(StreamEvent::UpdateEpoch(barrier)))) => barrier,
        _ => panic!("expected provider epoch handoff"),
    }
}

fn complete(receiver: &mut SecondaryReceiver, barrier: EpochBarrier) -> ReceiverSignals {
    signals(
        receiver
            .complete_epoch(barrier, Ok(()), false, Arc::new(()))
            .unwrap(),
    )
}

#[test]
fn uninitialized_progress_and_native_ack_sentinels_are_distinct() {
    let mut receiver = SecondaryReceiver::new(config(true, false));
    assert_eq!(
        receiver.current_progress(),
        Err(ReceiverError::NotInitialized)
    );
    assert_eq!(
        receiver.catchup_capability(),
        Err(ReceiverError::NotInitialized)
    );
    assert_eq!(
        receiver.capture_ack(),
        ReceiverAck {
            received: -1,
            applied: -1,
        }
    );
    assert_eq!(receiver.pending_ack_progress(), 0);
    receiver.initialize(10, epoch(100)).unwrap();
    assert_eq!(receiver.current_progress(), Ok(9));
    assert_eq!(receiver.catchup_capability(), Ok(0));
    assert_eq!(receiver.provider_epoch(), None);
}

#[test]
fn persisted_or_setting_selects_required_ack_and_optimistic_completes_before_dequeue() {
    for persisted in [false, true] {
        for setting in [false, true] {
            let required = persisted || setting;
            let mut receiver = SecondaryReceiver::new(config(persisted, setting));
            receiver.initialize(10, epoch(100)).unwrap();
            assert_eq!(receiver.requires_service_ack(), required);
            let operation = operation(10, 100, required);
            let received = receive(&mut receiver, vec![operation.clone()]);
            assert_eq!(received.batch.admitted, 1);
            assert_eq!(
                receiver.capture_ack(),
                ReceiverAck {
                    received: 10,
                    applied: if required { 9 } else { 10 },
                }
            );
            let mut stream = receiver.stream();
            assert!(take_operation(&mut stream).same_instance(&operation));
            if required {
                assert!(ack(&mut receiver, &operation).ack.is_some());
            } else {
                assert!(operation.acknowledge().unwrap().is_none());
            }
            assert_eq!(receiver.capture_ack().applied, 10);
        }
    }
}

#[test]
fn native_out_of_order_trace_dispatches_contiguous_data_and_not_gap_progress() {
    for required in [false, true] {
        let mut receiver = receiver(required);
        let eleven = operation(11, 100, required);
        let ten = operation(10, 100, required);
        let gap = receive(&mut receiver, vec![eleven.clone()]);
        assert!(gap.batch.stopped_on_gap);
        assert!(gap.ack.is_none());
        assert_eq!(receiver.current_progress(), Ok(9));
        let mut stream = receiver.stream();
        assert!(poll(&mut stream).is_pending());
        let _ = receive(&mut receiver, vec![ten.clone()]);
        assert!(take_operation(&mut stream).same_instance(&ten));
        assert!(take_operation(&mut stream).same_instance(&eleven));
        assert_eq!(receiver.current_progress(), Ok(11));
        if required {
            assert!(ack(&mut receiver, &eleven).ack.is_none());
            assert_eq!(receiver.capture_ack().applied, 9);
            let _ = ack(&mut receiver, &ten);
        }
        assert_eq!(receiver.capture_ack().applied, 11);
    }
}

#[test]
fn ready_to_run_ack_still_blocks_the_actual_queue_until_callback_start() {
    let mut receiver = receiver(true);
    let ten = operation(10, 100, true);
    let _ = receive(&mut receiver, vec![ten.clone()]);
    let work = ten.acknowledge().unwrap().unwrap();
    assert_eq!(ten.ack_state(), AckState::ReadyToRun);
    assert!(!receiver.queue.as_mut().unwrap().complete());
    let effects = receiver.process_ack(work).unwrap();
    assert_eq!(ten.ack_state(), AckState::Running);
    assert_eq!(receiver.capture_ack().applied, 10);
    let _ = signals(effects);
    assert_eq!(ten.ack_state(), AckState::Completed);
}

#[test]
fn dropping_selected_ack_work_is_not_cancellation_or_success() {
    let mut receiver = receiver(true);
    let ten = operation(10, 100, true);
    let _ = receive(&mut receiver, vec![ten.clone()]);
    drop(ten.acknowledge().unwrap().unwrap());
    assert_eq!(ten.ack_state(), AckState::ReadyToRun);
    assert_eq!(receiver.capture_ack().applied, 9);
    assert!(ten.acknowledge().is_err());
    assert!(!receiver.all_operations_acked());
}

#[test]
fn fresh_same_lsn_duplicate_does_not_replace_root_epoch_or_dispatch() {
    let mut receiver = receiver(true);
    let ten = operation(10, 100, true);
    let _ = receive(&mut receiver, vec![ten.clone()]);
    let duplicate = operation(10, 900, true);
    let received = receive(&mut receiver, vec![duplicate.clone()]);
    assert_eq!(received.batch.duplicates, 1);
    assert!(received.ack.is_some());
    let mut stream = receiver.stream();
    assert!(take_operation(&mut stream).same_instance(&ten));
    assert!(poll(&mut stream).is_pending());
    assert_eq!(receiver.provider_epoch(), Some(epoch(100)));
    let work = duplicate.acknowledge().unwrap().unwrap();
    assert!(matches!(
        receiver.process_ack(work),
        Err(ReceiverError::ForeignAck)
    ));
    assert_eq!(receiver.capture_ack().applied, 9);
    let _ = ack(&mut receiver, &ten);
    assert_eq!(receiver.capture_ack().applied, 10);
}

#[test]
fn gap_batch_stops_at_first_admitted_nonprogressing_entry_and_skips_hint() {
    let mut receiver = receiver(false);
    let _ = receive(&mut receiver, vec![operation(10, 100, false)]);
    let gap = signals(
        receiver
            .receive(
                epoch(100),
                vec![operation(13, 100, false), operation(11, 100, false)],
                Some(10),
            )
            .unwrap(),
    );
    assert_eq!(gap.batch.admitted, 1);
    assert!(gap.batch.stopped_on_gap);
    let queue = receiver.queue().unwrap();
    assert!(queue.get_operation(11).is_none());
    assert!(queue.get_operation(13).is_some());
    assert_eq!(receiver.catchup_capability(), Ok(10));
}

#[test]
fn completed_hint_cannot_trim_unacknowledged_data_but_later_duplicate_can_trim_history() {
    let mut receiver = receiver(true);
    let ten = operation(10, 100, true);
    let _ = signals(
        receiver
            .receive(epoch(100), vec![ten.clone()], Some(10))
            .unwrap(),
    );
    assert!(receiver.queue().unwrap().get_operation(10).is_some());
    assert_eq!(receiver.catchup_capability(), Ok(0));
    let _ = ack(&mut receiver, &ten);
    assert_eq!(receiver.catchup_capability(), Ok(10));
    let _ = signals(
        receiver
            .receive(epoch(100), vec![operation(10, 100, true)], Some(10))
            .unwrap(),
    );
    assert!(receiver.queue().unwrap().get_operation(10).is_none());
    assert_eq!(receiver.catchup_capability(), Ok(0));
    assert_eq!(receiver.current_progress(), Ok(10));
}

#[test]
fn full_span_rejection_is_observable_and_does_not_invent_progress() {
    let mut config = config(true, false);
    config.queue.max_capacity = 4;
    let mut receiver = SecondaryReceiver::new(config);
    receiver.initialize(10, epoch(100)).unwrap();
    let result = receive(&mut receiver, vec![operation(14, 100, true)]);
    assert!(result.batch.queue_full);
    assert_eq!(result.batch.admitted, 0);
    assert!(result.ack.is_some());
    assert_eq!(receiver.current_progress(), Ok(9));
    assert_eq!(receiver.queue().unwrap().operation_count(), 0);
}

#[test]
fn byte_pressure_suffix_displacement_drops_old_metadata_and_original_epoch() {
    let mut config = config(true, false);
    config.queue.max_bytes = 6;
    let mut receiver = SecondaryReceiver::new(config);
    receiver.initialize(10, epoch(100)).unwrap();
    let large = |lsn, configuration| {
        ReceivedOperation::new(
            QueueOperation::new(lsn, Bytes::from_static(b"four"), true),
            epoch(configuration),
            true,
        )
    };
    let _ = receive(&mut receiver, vec![large(11, 101)]);
    let _ = receive(&mut receiver, vec![large(10, 100)]);
    assert!(receiver.queue().unwrap().get_operation(11).is_none());
    assert!(
        !receiver
            .callback
            .lock()
            .unwrap()
            .operations
            .contains_key(&11)
    );
    let mut stream = receiver.stream();
    let ten = take_operation(&mut stream);
    let _ = ack(&mut receiver, &ten);
    let _ = receive(&mut receiver, vec![operation(11, 100, true)]);
    assert_eq!(take_operation(&mut stream).epoch(), epoch(100));
    assert!(poll(&mut stream).is_pending());
}

#[test]
fn framed_admission_uses_logical_bytes_not_frame_overhead() {
    let mut config = config(false, false);
    config.queue.max_bytes = 1;
    let mut receiver = SecondaryReceiver::new(config);
    receiver.initialize(10, epoch(100)).unwrap();
    let limits = BufferLimits {
        max_segments: 3,
        max_payload_bytes: 10,
    };
    let data = crate::replicator::v1::buffers::encode(
        &[Bytes::new(), Bytes::from_static(b"x"), Bytes::new()],
        BufferPolicy::FramedSegments,
        limits,
    )
    .unwrap();
    assert!(data.len() > 1);
    let operation = ReceivedOperation::new(
        QueueOperation::from_data(10, data, true, BufferPolicy::FramedSegments, limits).unwrap(),
        epoch(100),
        false,
    );
    let _ = receive(&mut receiver, vec![operation.clone()]);
    assert_eq!(receiver.queue().unwrap().total_bytes(), 1);
    assert_eq!(receiver.capture_ack().applied, 10);
    let mut stream = receiver.stream();
    assert!(take_operation(&mut stream).same_instance(&operation));
}

#[test]
fn invalid_batch_input_is_explicit_and_does_not_advance_sending_floor() {
    let mut receiver = receiver(true);
    assert!(matches!(
        receiver.receive(epoch(101), Vec::new(), None),
        Err(ReceiverError::EmptyBatch)
    ));
    assert!(matches!(
        receiver.receive(epoch(101), vec![operation(10, 100, false)], None),
        Err(ReceiverError::InvalidOperation(10))
    ));
    assert!(matches!(
        receiver.receive(epoch(101), vec![operation(Lsn::MAX, 100, true)], None),
        Err(ReceiverError::InvalidOperation(Lsn::MAX))
    ));
    assert!(matches!(
        receiver.receive(epoch(101), vec![operation(10, -1, true)], None),
        Err(ReceiverError::InvalidEpoch)
    ));
    assert!(matches!(
        receiver.receive(epoch(101), vec![operation(10, 100, true)], Some(Lsn::MAX)),
        Err(ReceiverError::InvalidCompletedHint)
    ));
    assert_eq!(receiver.minimum_sending_epoch(), Some(epoch(100)));
    assert_eq!(receiver.current_progress(), Ok(9));
    assert_eq!(receiver.queue().unwrap().operation_count(), 0);
}

#[test]
fn initialization_rejects_invalid_cursor_epoch_and_reinitialization() {
    let mut receiver = SecondaryReceiver::new(config(true, false));
    assert_eq!(
        receiver.initialize(0, epoch(100)),
        Err(ReceiverError::InvalidStart)
    );
    assert_eq!(
        receiver.initialize(Lsn::MAX, epoch(100)),
        Err(ReceiverError::InvalidStart)
    );
    assert_eq!(
        receiver.initialize(10, Epoch::new(-1, 100)),
        Err(ReceiverError::InvalidEpoch)
    );
    receiver.initialize(10, epoch(100)).unwrap();
    assert_eq!(
        receiver.initialize(11, epoch(101)),
        Err(ReceiverError::AlreadyInitialized)
    );
    assert_eq!(receiver.current_progress(), Ok(9));
}

#[test]
fn minimum_sending_epoch_advances_on_gap_without_changing_fixed_copy_epoch() {
    let mut receiver = receiver(true);
    let _ = receive(&mut receiver, vec![operation(11, 100, true)]);
    let _ = signals(
        receiver
            .receive(epoch(102), vec![operation(12, 100, true)], None)
            .unwrap(),
    );
    assert_eq!(receiver.minimum_sending_epoch(), Some(epoch(102)));
    assert!(matches!(
        receiver.receive(epoch(101), vec![operation(10, 100, true)], None),
        Err(ReceiverError::StaleSendingEpoch { .. })
    ));
    let _ = signals(
        receiver
            .receive(epoch(102), vec![operation(10, 100, true)], None)
            .unwrap(),
    );
    assert_eq!(receiver.provider_epoch(), Some(epoch(100)));
    assert_eq!(receiver.copy_epoch, Some(epoch(100)));
    assert_eq!(receiver.current_progress(), Ok(12));
}

#[test]
fn first_original_epoch_below_start_copy_is_preserved_without_spurious_barrier() {
    let mut receiver = receiver(true);
    let ten = operation(10, 99, true);
    let _ = receive(&mut receiver, vec![ten.clone()]);
    assert_eq!(receiver.provider_epoch(), Some(epoch(99)));
    let mut stream = receiver.stream();
    assert!(take_operation(&mut stream).same_instance(&ten));
    let _ = ack(&mut receiver, &ten);
    let _ = receive(&mut receiver, vec![operation(11, 100, true)]);
    assert_eq!(
        take_barrier(&mut stream),
        EpochBarrier {
            epoch: epoch(100),
            previous_epoch_last_lsn: 10,
        }
    );
}

#[test]
fn higher_first_original_epoch_creates_stream_barrier_and_blocks_all_getters() {
    for required in [false, true] {
        let mut receiver = receiver(required);
        let ten = operation(10, 101, required);
        let _ = receive(&mut receiver, vec![ten.clone()]);
        let mut first = receiver.stream();
        let mut second = receiver.stream();
        let barrier = take_barrier(&mut first);
        assert!(poll(&mut first).is_pending());
        assert!(poll(&mut second).is_pending());
        let _ = complete(&mut receiver, barrier);
        assert!(take_operation(&mut second).same_instance(&ten));
        assert!(poll(&mut first).is_pending());
    }
}

#[test]
fn original_epoch_is_established_from_first_contiguous_entry_not_early_gap() {
    let mut receiver = receiver(true);
    let eleven = operation(11, 101, true);
    let _ = receive(&mut receiver, vec![eleven.clone()]);
    assert_eq!(receiver.provider_epoch(), None);
    let ten = operation(10, 100, true);
    let _ = receive(&mut receiver, vec![ten.clone()]);
    let mut stream = receiver.stream();
    assert!(take_operation(&mut stream).same_instance(&ten));
    assert!(poll(&mut stream).is_pending());
    let handoff = ack(&mut receiver, &ten).update_epoch.unwrap();
    assert_eq!(
        handoff,
        EpochBarrier {
            epoch: epoch(101),
            previous_epoch_last_lsn: 10,
        }
    );
    assert!(poll(&mut stream).is_pending());
    let _ = complete(&mut receiver, handoff);
    assert!(take_operation(&mut stream).same_instance(&eleven));
}

#[test]
fn full_barrier_requires_contiguous_old_epoch_acks_then_replays_later_barriers() {
    for clean in [false, true] {
        let mut config = config(true, false);
        config.queue.clean_on_complete = clean;
        let mut receiver = SecondaryReceiver::new(config);
        receiver.initialize(10, epoch(100)).unwrap();
        let operations = [
            operation(10, 100, true),
            operation(11, 100, true),
            operation(12, 101, true),
            operation(13, 102, true),
            operation(14, 102, true),
            operation(15, 103, true),
        ];
        let _ = receive(&mut receiver, operations.to_vec());
        assert_eq!(receiver.current_progress(), Ok(15));
        let mut stream = receiver.stream();
        assert_eq!(take_operation(&mut stream).queue_operation().lsn(), 10);
        assert_eq!(take_operation(&mut stream).queue_operation().lsn(), 11);
        assert!(poll(&mut stream).is_pending());
        assert!(ack(&mut receiver, &operations[1]).update_epoch.is_none());
        let first = ack(&mut receiver, &operations[0]).update_epoch.unwrap();
        assert_eq!(first.previous_epoch_last_lsn, 11);
        assert_eq!(first.epoch, epoch(101));
        let _ = complete(&mut receiver, first);
        assert_eq!(take_operation(&mut stream).queue_operation().lsn(), 12);
        assert!(poll(&mut stream).is_pending());
        let second = ack(&mut receiver, &operations[2]).update_epoch.unwrap();
        assert_eq!(second.previous_epoch_last_lsn, 12);
        assert_eq!(second.epoch, epoch(102));
        let _ = complete(&mut receiver, second);
        assert_eq!(take_operation(&mut stream).queue_operation().lsn(), 13);
        assert_eq!(take_operation(&mut stream).queue_operation().lsn(), 14);
        assert!(poll(&mut stream).is_pending());
        assert!(ack(&mut receiver, &operations[4]).update_epoch.is_none());
        let third = ack(&mut receiver, &operations[3]).update_epoch.unwrap();
        assert_eq!(third.previous_epoch_last_lsn, 14);
        assert_eq!(third.epoch, epoch(103));
        let _ = complete(&mut receiver, third);
        assert_eq!(take_operation(&mut stream).queue_operation().lsn(), 15);
        let _ = ack(&mut receiver, &operations[5]);
        assert!(receiver.all_operations_acked());
        assert_eq!(receiver.capture_ack().applied, 15);
    }
}

#[test]
fn optimistic_multiple_epochs_complete_before_stream_barriers_are_consumed() {
    let mut receiver = receiver(false);
    let _ = receive(
        &mut receiver,
        vec![
            operation(10, 100, false),
            operation(11, 101, false),
            operation(12, 102, false),
        ],
    );
    assert_eq!(
        receiver.capture_ack(),
        ReceiverAck {
            received: 12,
            applied: 12,
        }
    );
    let mut stream = receiver.stream();
    assert_eq!(take_operation(&mut stream).queue_operation().lsn(), 10);
    let first = take_barrier(&mut stream);
    assert_eq!(first.epoch, epoch(101));
    assert!(poll(&mut stream).is_pending());
    let _ = complete(&mut receiver, first);
    assert_eq!(take_operation(&mut stream).queue_operation().lsn(), 11);
    let second = take_barrier(&mut stream);
    assert_eq!(second.epoch, epoch(102));
    let _ = complete(&mut receiver, second);
    assert_eq!(take_operation(&mut stream).queue_operation().lsn(), 12);
}

#[test]
fn mismatched_or_premature_epoch_completion_cannot_release_barrier() {
    let mut receiver = receiver(true);
    let ten = operation(10, 100, true);
    let _ = receive(&mut receiver, vec![ten.clone(), operation(11, 101, true)]);
    let barrier = EpochBarrier {
        epoch: epoch(101),
        previous_epoch_last_lsn: 10,
    };
    assert!(matches!(
        receiver.complete_epoch(barrier, Ok(()), false, Arc::new(())),
        Err(ReceiverError::InvalidEpochCompletion)
    ));
    assert_eq!(ack(&mut receiver, &ten).update_epoch, Some(barrier));
    assert!(matches!(
        receiver.complete_epoch(
            EpochBarrier {
                previous_epoch_last_lsn: 9,
                ..barrier
            },
            Ok(()),
            false,
            Arc::new(())
        ),
        Err(ReceiverError::InvalidEpochCompletion)
    ));
    let _ = complete(&mut receiver, barrier);
    assert!(matches!(
        receiver.complete_epoch(barrier, Ok(()), false, Arc::new(())),
        Err(ReceiverError::InvalidEpochCompletion)
    ));
}

#[test]
fn abandoned_provider_handoff_never_resumes_full_barrier() {
    let mut receiver = receiver(true);
    let ten = operation(10, 100, true);
    let _ = receive(&mut receiver, vec![ten.clone(), operation(11, 101, true)]);
    let mut stream = receiver.stream();
    take_operation(&mut stream);
    let handoff = ack(&mut receiver, &ten);
    assert!(handoff.update_epoch.is_some());
    assert!(poll(&mut stream).is_pending());
    let _ = receive(&mut receiver, vec![operation(12, 101, true)]);
    assert!(poll(&mut stream).is_pending());
    assert_eq!(receiver.capture_ack().applied, 10);
}

#[test]
fn failed_provider_update_retires_cleans_and_surfaces_fault_after_effect_delivery() {
    let mut receiver = receiver(true);
    let _ = receive(&mut receiver, vec![operation(10, 101, true)]);
    let mut stream = receiver.stream();
    let barrier = take_barrier(&mut stream);
    let effects = receiver
        .complete_epoch(barrier, Err("provider failed".into()), false, Arc::new(()))
        .unwrap();
    assert!(matches!(
        effects.dispatch(),
        Err(ReceiverError::ProviderEpoch(message)) if message == "provider failed"
    ));
    assert_eq!(receiver.queue().unwrap().operation_count(), 0);
    assert!(matches!(
        poll(&mut stream),
        Poll::Ready(Some(Err(StreamError::Fault(_))))
    ));
    assert!(matches!(
        receiver.receive(epoch(101), vec![operation(11, 101, true)], None),
        Err(ReceiverError::Closed)
    ));
}

#[test]
fn full_barrier_completion_during_close_discards_without_replaying() {
    let mut receiver = receiver(true);
    let ten = operation(10, 100, true);
    let _ = receive(&mut receiver, vec![ten.clone(), operation(11, 101, true)]);
    let mut stream = receiver.stream();
    take_operation(&mut stream);
    let barrier = ack(&mut receiver, &ten).update_epoch.unwrap();
    let _ = signals(
        receiver
            .complete_epoch(barrier, Ok(()), true, Arc::new(()))
            .unwrap(),
    );
    assert!(poll(&mut stream).is_pending());
    assert!(receiver.queue().unwrap().get_operation(11).is_none());
    assert_eq!(receiver.current_progress(), Ok(10));
}

#[test]
fn shared_receiver_stream_reacquisition_neither_replays_nor_resets_data() {
    let mut receiver = receiver(false);
    let _ = receive(
        &mut receiver,
        vec![operation(10, 100, false), operation(11, 100, false)],
    );
    let mut first = receiver.stream();
    assert_eq!(take_operation(&mut first).queue_operation().lsn(), 10);
    drop(first);
    let mut second = receiver.stream();
    assert_eq!(take_operation(&mut second).queue_operation().lsn(), 11);
    assert!(poll(&mut second).is_pending());
}

#[test]
fn zero_byte_replication_is_data_not_eof_or_eos() {
    let mut receiver = receiver(false);
    let empty = ReceivedOperation::new(
        QueueOperation::new(10, Bytes::new(), true),
        epoch(100),
        false,
    );
    let _ = receive(&mut receiver, vec![empty.clone()]);
    let mut stream = receiver.stream();
    let read = take_operation(&mut stream);
    assert!(read.same_instance(&empty));
    assert!(!read.is_end_of_stream());
    assert!(read.queue_operation().data().is_empty());
    assert!(poll(&mut stream).is_pending());
}

#[test]
fn cleanup_cancels_unstarted_normal_callbacks_without_consuming_service_data() {
    let mut receiver = receiver(true);
    let ten = operation(10, 100, true);
    let _ = receive(&mut receiver, vec![ten.clone()]);
    receiver.clean_queue(Arc::new(()), true, false);
    assert_eq!(ten.ack_state(), AckState::Cancelled);
    assert!(ten.acknowledge().unwrap().is_none());
    assert_eq!(receiver.queue().unwrap().operation_count(), 0);
    let mut stream = receiver.stream();
    assert!(take_operation(&mut stream).same_instance(&ten));
    assert!(poll(&mut stream).is_pending());
}

#[test]
fn ready_ack_survives_retirement_and_keeps_owner_until_actual_callback_finish() {
    let mut receiver = receiver(true);
    let ten = operation(10, 100, true);
    let _ = receive(&mut receiver, vec![ten.clone()]);
    let work = ten.acknowledge().unwrap().unwrap();
    let owner = Arc::new(());
    let weak = Arc::downgrade(&owner);
    let _ = signals(receiver.retire(owner.clone()));
    drop(owner);
    assert!(weak.upgrade().is_some());
    assert_eq!(ten.ack_state(), AckState::ReadyToRun);
    let effects = receiver.process_ack(work).unwrap();
    assert_eq!(ten.ack_state(), AckState::Running);
    assert!(weak.upgrade().is_some());
    let delivered = signals(effects);
    assert!(delivered.ack.is_none());
    assert!(delivered.update_epoch.is_none());
    assert!(weak.upgrade().is_none());
    assert_eq!(ten.ack_state(), AckState::Completed);
}

#[test]
fn ready_callback_after_nonterminal_queue_cleanup_remains_bound_to_original_instance() {
    let mut receiver = receiver(true);
    let ten = operation(10, 100, true);
    let _ = receive(&mut receiver, vec![ten.clone()]);
    let work = ten.acknowledge().unwrap().unwrap();
    receiver.clean_queue(Arc::new(()), true, false);
    assert_eq!(receiver.cleanup_callbacks.len(), 1);
    let delivered = signals(receiver.process_ack(work).unwrap());
    assert!(delivered.ack.is_none());
    assert!(receiver.cleanup_callbacks.is_empty());
    assert_eq!(ten.ack_state(), AckState::Completed);
    assert_eq!(receiver.capture_ack().applied, 9);
}

#[test]
fn retired_receiver_cannot_start_a_foreign_receivers_ready_callback() {
    let mut retired = receiver(true);
    let mut live = receiver(true);
    let live_operation = operation(10, 100, true);
    let _ = receive(&mut live, vec![live_operation.clone()]);
    let _ = signals(retired.retire(Arc::new(())));
    let work = live_operation.acknowledge().unwrap().unwrap();
    assert!(matches!(
        retired.process_ack(work),
        Err(ReceiverError::ForeignAck)
    ));
    assert_eq!(live_operation.ack_state(), AckState::ReadyToRun);
    assert!(live_operation.queue_operation().needs_ack());
    assert_eq!(live.capture_ack().applied, 9);
}

#[test]
fn running_ack_effects_retain_owner_through_retirement_until_dispatched() {
    let mut receiver = receiver(true);
    let ten = operation(10, 100, true);
    let _ = receive(&mut receiver, vec![ten.clone()]);
    let effects = receiver
        .process_ack(ten.acknowledge().unwrap().unwrap())
        .unwrap();
    let owner = Arc::new(());
    let weak = Arc::downgrade(&owner);
    // The queue no longer owns the completed operation. The callback token
    // itself roots its work; native cleanup cannot discover it in history.
    ten.ignore_ack_and_keep_owner(owner.clone());
    let _ = signals(receiver.retire(owner.clone()));
    drop(owner);
    assert!(weak.upgrade().is_some());
    let _ = signals(effects);
    assert!(weak.upgrade().is_none());
}

#[test]
fn discard_out_of_order_preserves_only_native_committed_or_completed_prefix() {
    for required in [false, true] {
        let mut receiver = receiver(required);
        let _ = receive(&mut receiver, vec![operation(10, 100, required)]);
        let _ = receive(&mut receiver, vec![operation(12, 100, required)]);
        receiver.discard_out_of_order();
        assert!(receiver.queue().unwrap().get_operation(12).is_none());
        assert!(receiver.queue().unwrap().get_operation(10).is_some());
        assert_eq!(receiver.current_progress(), Ok(10));
    }
}

#[test]
fn eos_is_explicit_data_ack_not_ordinary_eof_and_is_enqueued_once() {
    let mut config = config(true, false);
    config.enable_eos_ack = true;
    let mut receiver = SecondaryReceiver::new(config);
    receiver.initialize(10, epoch(100)).unwrap();
    let owner = Arc::new(());
    let weak = Arc::downgrade(&owner);
    let _ = signals(receiver.enqueue_eos(owner.clone()).unwrap());
    let _ = signals(receiver.enqueue_eos(owner.clone()).unwrap());
    drop(owner);
    assert!(weak.upgrade().is_some());
    assert!(!receiver.all_operations_acked());
    let mut stream = receiver.stream();
    let eos = take_operation(&mut stream);
    assert!(eos.is_end_of_stream());
    assert!(poll(&mut stream).is_pending());
    let _ = ack(&mut receiver, &eos);
    assert!(receiver.all_operations_acked());
    assert!(weak.upgrade().is_none());
}

#[test]
fn eos_retirement_can_ignore_drain_ack_without_cancelling_its_callback() {
    let mut config = config(true, false);
    config.enable_eos_ack = true;
    let mut receiver = SecondaryReceiver::new(config);
    receiver.initialize(10, epoch(100)).unwrap();
    let _ = signals(receiver.enqueue_eos(Arc::new(())).unwrap());
    let mut stream = receiver.stream();
    let eos = take_operation(&mut stream);
    let _ = signals(receiver.retire(Arc::new(())));
    assert!(receiver.all_operations_acked());
    assert_eq!(eos.ack_state(), AckState::NotCalled);
    let work = eos.acknowledge().unwrap().unwrap();
    let _ = signals(receiver.process_ack(work).unwrap());
    assert_eq!(eos.ack_state(), AckState::Completed);
}

#[test]
fn eos_disabled_is_explicit_and_optimistic_eos_does_not_require_service_ack() {
    let mut disabled = receiver(false);
    assert!(matches!(
        disabled.enqueue_eos(Arc::new(())),
        Err(ReceiverError::EosDisabled)
    ));
    let mut config = config(false, false);
    config.enable_eos_ack = true;
    let mut receiver = SecondaryReceiver::new(config);
    receiver.initialize(10, epoch(100)).unwrap();
    let _ = signals(receiver.enqueue_eos(Arc::new(())).unwrap());
    assert!(receiver.all_operations_acked());
    let mut stream = receiver.stream();
    assert!(take_operation(&mut stream).acknowledge().unwrap().is_none());
}

#[test]
fn clean_on_complete_capability_is_zero_while_dispatch_still_owns_data() {
    let mut config = config(false, false);
    config.queue.clean_on_complete = true;
    let mut receiver = SecondaryReceiver::new(config);
    receiver.initialize(10, epoch(100)).unwrap();
    let _ = receive(&mut receiver, vec![operation(10, 100, false)]);
    assert_eq!(receiver.current_progress(), Ok(10));
    assert_eq!(receiver.catchup_capability(), Ok(0));
    assert_eq!(receiver.queue().unwrap().operation_count(), 0);
    let mut stream = receiver.stream();
    assert_eq!(take_operation(&mut stream).queue_operation().lsn(), 10);
}

#[test]
fn real_ack_scheduler_uses_copy_plus_replication_threshold_and_current_snapshot() {
    let receiver = Arc::new(Mutex::new(receiver(true)));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut sender = AckSender::new(
        Duration::from_millis(10),
        Duration::from_millis(100),
        3,
        Duration::ZERO,
    );
    let callback_receiver = receiver.clone();
    let callback_observed = observed.clone();
    sender.open(
        Arc::new(move |_| {
            callback_observed
                .lock()
                .unwrap()
                .push(callback_receiver.lock().unwrap().capture_ack());
            true
        }),
        Duration::ZERO,
    );
    let ten = operation(10, 100, true);
    let eleven = operation(11, 100, true);
    let issued = {
        let mut receiver = receiver.lock().unwrap();
        receiver.capture_ack();
        let ingress = receive(&mut receiver, vec![ten.clone(), eleven.clone()]);
        assert!(
            receiver
                .schedule_ack(&mut sender, ingress.ack.unwrap(), 0, 3, Duration::ZERO,)
                .unwrap()
                .is_none()
        );
        assert_eq!(sender.deadline(), Some(Duration::from_millis(10)));
        let _ = ack(&mut receiver, &eleven);
        let progress = ack(&mut receiver, &ten);
        receiver
            .schedule_ack(
                &mut sender,
                progress.ack.unwrap(),
                1,
                3,
                Duration::from_millis(1),
            )
            .unwrap()
            .unwrap()
    };
    assert!(observed.lock().unwrap().is_empty());
    {
        let mut receiver = receiver.lock().unwrap();
        let twelve = operation(12, 100, true);
        let _ = receive(&mut receiver, vec![twelve.clone()]);
        let _ = ack(&mut receiver, &twelve);
    }
    issued.dispatch();
    assert_eq!(
        *observed.lock().unwrap(),
        [ReceiverAck {
            received: 12,
            applied: 12,
        }]
    );
    assert_eq!(receiver.lock().unwrap().pending_ack_progress(), 0);
}

#[test]
fn real_batched_ack_timer_reads_latest_receiver_progress_and_failed_threshold_is_atomic() {
    let receiver = Arc::new(Mutex::new(receiver(false)));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut sender = AckSender::new(
        Duration::from_millis(10),
        Duration::from_millis(100),
        10,
        Duration::ZERO,
    );
    let target = receiver.clone();
    let sink = observed.clone();
    sender.open(
        Arc::new(move |_| {
            sink.lock()
                .unwrap()
                .push(target.lock().unwrap().capture_ack());
            true
        }),
        Duration::ZERO,
    );
    {
        let mut receiver = receiver.lock().unwrap();
        receiver.capture_ack();
        let ingress = receive(&mut receiver, vec![operation(10, 100, false)]);
        let request = ingress.ack.unwrap();
        assert!(matches!(
            receiver.schedule_ack(&mut sender, request, -1, 10, Duration::ZERO),
            Err(ReceiverError::InvalidAckThreshold)
        ));
        assert_eq!(sender.deadline(), None);
        assert!(
            receiver
                .schedule_ack(&mut sender, request, 0, 10, Duration::ZERO)
                .unwrap()
                .is_none()
        );
        let _ = receive(&mut receiver, vec![operation(11, 100, false)]);
    }
    sender
        .on_timer(Duration::from_millis(10))
        .unwrap()
        .dispatch();
    assert_eq!(observed.lock().unwrap()[0].applied, 11);
}

#[test]
fn receiver_wakes_consumers_only_when_effects_are_dispatched_outside_owner_guard() {
    use futures::task::{ArcWake, waker};
    use std::sync::Weak;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct CheckWake {
        receiver: Weak<Mutex<SecondaryReceiver>>,
        ran: AtomicBool,
    }
    impl ArcWake for CheckWake {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            let receiver = arc_self.receiver.upgrade().unwrap();
            assert!(receiver.try_lock().is_ok());
            arc_self.ran.store(true, Ordering::SeqCst);
        }
    }
    let receiver = Arc::new(Mutex::new(receiver(false)));
    let check = Arc::new(CheckWake {
        receiver: Arc::downgrade(&receiver),
        ran: AtomicBool::new(false),
    });
    let wake = waker(check.clone());
    let mut stream = receiver.lock().unwrap().stream();
    assert!(
        Pin::new(&mut stream)
            .poll_next(&mut Context::from_waker(&wake))
            .is_pending()
    );
    let effects = receiver
        .lock()
        .unwrap()
        .receive(epoch(100), vec![operation(10, 100, false)], None)
        .unwrap();
    assert!(!check.ran.load(Ordering::SeqCst));
    let _ = signals(effects);
    assert!(check.ran.load(Ordering::SeqCst));
    assert_eq!(take_operation(&mut stream).queue_operation().lsn(), 10);
}

#[test]
fn real_receiver_ack_reaches_session_sender_but_cannot_bypass_ordered_primary_handoff() {
    use crate::replicator::v1::replica_manager::{
        EstablishedConfiguration, ReplicaManager, ReplicaSet, SessionMember,
    };
    use crate::replicator::v1::sender::SenderConfig;
    use crate::replicator::v1::session::{
        AckAction, AckUpdate, ReplicationSession, SendIntent, SendKind, SessionIdentity,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot::error::TryRecvError;

    for required in [false, true] {
        let outgoing = Arc::new(Mutex::new(Vec::<SendIntent>::new()));
        let sent = outgoing.clone();
        let session = ReplicationSession::new(
            SessionIdentity {
                primary_endpoint: "primary".into(),
                remote_endpoint: "secondary".into(),
                remote_incarnation: 1,
            },
            epoch(100),
            0,
            SenderConfig {
                initial_window: 4,
                max_window: 4,
                retry: Duration::from_millis(100),
                decay_factor: 0.0,
                decay_interval: Duration::from_millis(100),
            },
            Arc::new(move |intent| {
                sent.lock().unwrap().push(intent);
                true
            }),
            Duration::ZERO,
        );
        session.open();
        let mut primary = ReplicaManager::new(
            config(required, false).queue,
            0,
            1,
            "primary".into(),
            epoch(100),
            true,
        )
        .unwrap();
        primary
            .configure_established(EstablishedConfiguration {
                current: ReplicaSet {
                    write_quorum: 2,
                    members: vec![SessionMember {
                        replica_id: 2,
                        session: session.clone(),
                        must_catchup: false,
                    }],
                },
                previous: None,
                catchup_configuration: false,
            })
            .unwrap()
            .dispatch(Duration::ZERO)
            .unwrap();
        let admission = primary
            .enqueue(
                Bytes::from_static(b"service data"),
                BufferPolicy::RawSingle,
                BufferLimits {
                    max_segments: 1,
                    max_payload_bytes: 100,
                },
            )
            .unwrap();
        assert_eq!(admission.operation.operation.lsn(), 1);
        let mut completion = admission.completion;
        admission.effects.dispatch(Duration::ZERO).unwrap();
        assert_eq!(completion.try_recv(), Err(TryRecvError::Empty));

        let mut receiver = SecondaryReceiver::new(config(required, false));
        receiver.initialize(1, epoch(100)).unwrap();
        let received_operation = match outgoing.lock().unwrap().pop().unwrap().kind {
            SendKind::Replication {
                operation,
                original_epoch,
                sending_epoch,
                completed_hint,
            } => {
                let received = ReceivedOperation::new(
                    QueueOperation::new(operation.lsn(), operation.data().clone(), true),
                    original_epoch,
                    required,
                );
                let ingress = signals(
                    receiver
                        .receive(sending_epoch, vec![received.clone()], Some(completed_hint))
                        .unwrap(),
                );
                assert!(ingress.ack.is_some());
                received
            }
            SendKind::RequestAck | SendKind::StartCopy { .. } | SendKind::InduceFault { .. } => {
                panic!("expected actual primary data send")
            }
        };
        if required {
            let mut stream = receiver.stream();
            assert!(take_operation(&mut stream).same_instance(&received_operation));
            let _ = ack(&mut receiver, &received_operation);
        }
        let receiver = Arc::new(Mutex::new(receiver));
        let processors = Arc::new(Mutex::new(Vec::new()));
        let callbacks = Arc::new(AtomicUsize::new(0));
        let target = receiver.clone();
        let pending = processors.clone();
        let notified = callbacks.clone();
        let actual_session = session.clone();
        let mut ack_sender = AckSender::new(
            Duration::ZERO,
            Duration::from_millis(100),
            0,
            Duration::ZERO,
        );
        ack_sender.open(
            Arc::new(move |_| {
                let snapshot = target.lock().unwrap().capture_ack();
                let notified = notified.clone();
                pending.lock().unwrap().push(
                    actual_session
                        .update_ack(
                            AckUpdate {
                                replication_received: snapshot.received,
                                replication_applied: snapshot.applied,
                                copy_received: 0,
                                copy_applied: 0,
                                process_copy: false,
                                message_id: "actual receiver ACK".into(),
                            },
                            Some(Arc::new(move || {
                                notified.fetch_add(1, Ordering::SeqCst);
                            })),
                        )
                        .unwrap(),
                );
                true
            }),
            Duration::ZERO,
        );
        let selected = receiver
            .lock()
            .unwrap()
            .schedule_ack(
                &mut ack_sender,
                AckRequest {
                    check_pending_limit: true,
                },
                0,
                0,
                Duration::ZERO,
            )
            .unwrap()
            .unwrap();
        selected.dispatch();
        let processor = processors.lock().unwrap().pop().unwrap();
        let processor = processor
            .next(Duration::ZERO)
            .unwrap()
            .deliver(|action| match action {
                AckAction::ReplicationSends(sends) => sends.dispatch(),
                _ => panic!("replication sender must be first"),
            });
        let copy_owner_handoff = processor.next(Duration::ZERO).unwrap();
        assert_eq!(session.progress(0).received, 1);
        assert_eq!(session.progress(0).applied, 1);
        assert_eq!(callbacks.load(Ordering::SeqCst), 0);
        assert_eq!(primary.queue().current_progress(), 0);
        assert_eq!(completion.try_recv(), Err(TryRecvError::Empty));
        // Copy/build is deliberately not implemented here. Its actual rooted
        // handoff stays unresolved rather than inventing a successful owner.
        drop(copy_owner_handoff);
        assert!(session.is_processing());
    }
}
