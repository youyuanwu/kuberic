// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.

use super::*;

#[test]
fn retained_receiver_checkpoint_has_no_fabricated_completed_range_or_pending_ack() {
    for prefix in [0, 7, Lsn::MAX - 2] {
        let mut receiver =
            SecondaryReceiver::from_retained(config(true, false), epoch(100), prefix).unwrap();
        let queue = receiver.queue().unwrap();
        assert_eq!(queue.operation_count(), 0);
        assert_eq!(queue.total_bytes(), 0);
        assert_eq!(queue.progress().retained_head, prefix + 1);
        assert_eq!(queue.progress().complete_head, prefix + 1);
        assert_eq!(queue.progress().commit_head, prefix + 1);
        assert_eq!(queue.progress().tail, prefix + 1);
        assert_eq!(receiver.current_progress(), Ok(prefix));
        assert_eq!(receiver.catchup_capability(), Ok(0));
        assert_eq!(receiver.pending_ack_progress(), 0);
        assert_eq!(receiver.provider_epoch(), Some(epoch(100)));
        assert_eq!(receiver.minimum_sending_epoch(), Some(epoch(100)));
        assert_eq!(
            receiver.initialize(1, epoch(101)),
            Err(ReceiverError::AlreadyInitialized)
        );
        assert_eq!(receiver.current_progress(), Ok(prefix));
    }
}

#[test]
fn retained_receiver_invalid_input_rejects_before_constructing_a_queue() {
    for prefix in [-1, Lsn::MIN, Lsn::MAX - 1, Lsn::MAX] {
        assert!(matches!(
            SecondaryReceiver::from_retained(config(true, false), epoch(100), prefix),
            Err(ReceiverError::InvalidStart)
        ));
    }
    assert!(matches!(
        SecondaryReceiver::from_retained(config(true, false), Epoch::new(-1, 0), 7),
        Err(ReceiverError::InvalidEpoch)
    ));
    let mut invalid = config(true, false);
    invalid.queue.initial_capacity = 3;
    assert!(matches!(
        SecondaryReceiver::from_retained(invalid, epoch(100), 7),
        Err(ReceiverError::InvalidStart)
    ));
}

#[test]
fn retained_receiver_uses_normal_queue_cleanup_and_real_service_ack_for_new_suffix() {
    for clean in [false, true] {
        let mut settings = config(true, false);
        settings.queue.clean_on_complete = clean;
        settings.queue.max_completed_count = 4;
        let mut receiver = SecondaryReceiver::from_retained(settings, epoch(100), 9).unwrap();
        let mut stream = receiver.stream();
        let ten = operation(10, 100, true);
        let duplicate_prefix = operation(9, 100, true);
        let duplicate = receive(&mut receiver, vec![duplicate_prefix]);
        assert_eq!(duplicate.batch.duplicates, 1);
        assert_eq!(receiver.current_progress(), Ok(9));
        assert!(poll(&mut stream).is_pending());
        let _ = receive(&mut receiver, vec![ten.clone()]);
        assert!(take_operation(&mut stream).same_instance(&ten));
        assert_eq!(receiver.capture_ack().applied, 9);
        assert_eq!(receiver.catchup_capability(), Ok(0));
        let _ = ack(&mut receiver, &ten);
        assert_eq!(receiver.capture_ack().applied, 10);
        assert_eq!(
            receiver.catchup_capability(),
            Ok(if clean { 0 } else { 10 })
        );
        assert_eq!(
            receiver.queue().unwrap().operation_count(),
            usize::from(!clean)
        );
    }
}

#[test]
fn retained_receiver_epoch_discards_only_uncommitted_suffix_and_keeps_acknowledged_prefix() {
    let mut receiver =
        SecondaryReceiver::from_retained(config(true, false), epoch(100), 9).unwrap();
    let _ = receive(&mut receiver, vec![operation(12, 100, true)]);
    assert_eq!(receiver.queue().unwrap().operation_count(), 1);
    assert_eq!(receiver.current_progress(), Ok(9));
    assert!(receiver.advance_external_epoch(epoch(101)).unwrap());
    assert_eq!(receiver.queue().unwrap().operation_count(), 0);
    assert_eq!(receiver.queue().unwrap().progress().tail, 10);
    assert_eq!(receiver.current_progress(), Ok(9));
    assert_eq!(receiver.catchup_capability(), Ok(0));
    assert!(matches!(
        receiver.receive(epoch(100), vec![operation(10, 100, true)], None),
        Err(ReceiverError::StaleSendingEpoch { .. })
    ));
    let ten = operation(10, 100, true);
    let _ = signals(
        receiver
            .receive(epoch(101), vec![ten.clone()], None)
            .unwrap(),
    );
    let _ = ack(&mut receiver, &ten);
    assert_eq!(receiver.capture_ack().applied, 10);
}

#[test]
fn retained_receiver_epoch_barrier_and_queue_transfer_preserve_real_new_operation() {
    let mut receiver =
        SecondaryReceiver::from_retained(config(true, false), epoch(100), 9).unwrap();
    let mut stream = receiver.stream();
    let ten = operation(10, 101, true);
    let _ = signals(
        receiver
            .receive(epoch(101), vec![ten.clone()], None)
            .unwrap(),
    );
    let barrier = take_barrier(&mut stream);
    assert_eq!(
        barrier,
        EpochBarrier {
            epoch: epoch(101),
            previous_epoch_last_lsn: 9,
        }
    );
    assert!(poll(&mut stream).is_pending());
    let _ = complete(&mut receiver, barrier);
    assert!(take_operation(&mut stream).same_instance(&ten));
    assert!(matches!(
        receiver.take_for_primary(),
        Err(ReceiverError::InvalidEpochCompletion)
    ));
    let _ = ack(&mut receiver, &ten);
    let transfer = receiver.take_for_primary().unwrap();
    assert_eq!(transfer.queue.progress().commit_head, 11);
    assert_eq!(transfer.queue.progress().complete_head, 11);
    assert!(Arc::ptr_eq(
        transfer.queue.get_operation(10).unwrap(),
        ten.queue_operation()
    ));
    assert_eq!(transfer.original_epochs.get(&10), Some(&epoch(101)));
}
