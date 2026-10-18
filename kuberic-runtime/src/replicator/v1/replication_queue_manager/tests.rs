// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Behavioral tests of the actual Rust primary queue owner, derived from
// ReplicationQueueManager.cpp/.h at the pin documented in the parent module.

use super::*;
use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::replicator::v1::buffers;
use crate::replicator::v1::operation_queue::CallbackControl;

fn config() -> QueueConfig {
    QueueConfig {
        initial_capacity: 2,
        max_capacity: 4,
        max_bytes: 0,
        max_completed_count: 0,
        max_completed_bytes: 0,
        clean_on_complete: false,
    }
}

fn limits() -> BufferLimits {
    BufferLimits {
        max_segments: 3,
        max_payload_bytes: 64,
    }
}

fn enqueue(queue: &mut ReplicationQueueManager, data: &'static [u8]) -> SenderOperation {
    queue
        .enqueue(
            Bytes::from_static(data),
            Epoch::new(1, 2),
            BufferPolicy::RawSingle,
            limits(),
        )
        .unwrap()
}

#[test]
fn successful_assignment_is_immediate_but_not_committed_or_completed() {
    let mut queue = ReplicationQueueManager::new(config(), 40);
    assert_eq!(queue.current_progress(), 40);
    assert_eq!(queue.catchup_capability(), 0);
    assert_eq!(queue.previous_config_catchup_lsn(), 0);
    assert_eq!(queue.previous_config_quorum_lsn(), 0);
    assert_eq!(queue.check_with_catchup_lsn(50), 0);

    let held = enqueue(&mut queue, b"first");
    assert_eq!(held.operation.lsn(), 41);
    assert_eq!(held.original_epoch, Epoch::new(1, 2));
    assert!(!held.operation.needs_ack());
    assert_eq!(queue.current_progress(), 40);
    assert_eq!(queue.last_sequence_number(), 41);
    assert_eq!(queue.operation_count(), 1);
    assert_eq!(queue.total_bytes(), 5);
    assert_eq!(queue.first_lsn_in_replication_queue(), 41);
    assert_eq!(queue.catchup_capability(), 0);
}

#[test]
fn full_queue_does_not_consume_lsn_and_completion_releases_only_queue_ownership() {
    let mut queue = ReplicationQueueManager::new(config(), 0);
    let held = enqueue(&mut queue, b"a");
    for _ in 0..3 {
        enqueue(&mut queue, b"b");
    }
    assert!(matches!(
        queue.enqueue(
            Bytes::from_static(b"rejected"),
            Epoch::new(2, 1),
            BufferPolicy::RawSingle,
            limits()
        ),
        Err(QueueAdmissionError::Queue(EnqueueError::Full))
    ));
    assert_eq!(queue.last_sequence_number(), 4);
    assert_eq!(queue.next_sequence_number, 5);
    assert_eq!(queue.original_epochs.len(), 4);
    assert_eq!(queue.total_bytes(), 4);

    assert_eq!(
        queue.update_queue(Some(1), Some(1)).unwrap(),
        QueueUpdate {
            old_committed: 0,
            new_committed: 1,
            removed_items: true,
        }
    );
    assert_eq!(queue.operation_count(), 3);
    assert_eq!(queue.original_epochs.len(), 3);
    assert_eq!(held.operation.data().as_ref(), b"a");
    assert_eq!(Arc::strong_count(&held.operation), 1);
    let next = enqueue(&mut queue, b"retry");
    assert_eq!(next.operation.lsn(), 5);
}

#[test]
fn framed_payload_bounds_count_logical_bytes_and_bad_frames_do_not_assign() {
    let mut queue = ReplicationQueueManager::new(
        QueueConfig {
            max_capacity: 0,
            max_bytes: 5,
            ..config()
        },
        0,
    );
    let frame = buffers::encode(
        &[Bytes::new(), Bytes::from_static(b"abc"), Bytes::new()],
        BufferPolicy::FramedSegments,
        limits(),
    )
    .unwrap();
    assert!(frame.len() > 5);
    let held = queue
        .enqueue(
            frame.clone(),
            Epoch::new(0, 8),
            BufferPolicy::FramedSegments,
            limits(),
        )
        .unwrap();
    assert_eq!(held.operation.logical_bytes(), 3);
    assert_eq!(
        held.operation.representation(),
        BufferPolicy::FramedSegments
    );
    assert!(matches!(
        queue.enqueue(
            frame,
            Epoch::new(0, 9),
            BufferPolicy::FramedSegments,
            limits()
        ),
        Err(QueueAdmissionError::Queue(EnqueueError::Full))
    ));
    assert!(matches!(
        queue.enqueue(
            Bytes::from_static(b"invalid frame"),
            Epoch::new(0, 9),
            BufferPolicy::FramedSegments,
            limits()
        ),
        Err(QueueAdmissionError::Buffer(BufferError::Malformed))
    ));
    assert!(matches!(
        queue.enqueue(
            Bytes::from_static(b"raw"),
            Epoch::new(0, 9),
            BufferPolicy::RawSingle,
            BufferLimits {
                max_payload_bytes: 2,
                ..limits()
            }
        ),
        Err(QueueAdmissionError::Buffer(BufferError::TooLarge))
    ));
    let second = enqueue(&mut queue, b"de");
    assert_eq!(second.operation.lsn(), 2);
    assert_eq!(queue.total_bytes(), 5);
    queue.update_queue(None, None).unwrap();
    assert_eq!(queue.total_bytes(), 0);
    assert_eq!(held.operation.logical_bytes(), 3);
}

#[test]
fn overflow_is_rejected_before_admission_even_when_representation_is_invalid() {
    let mut queue = ReplicationQueueManager::new(config(), Lsn::MAX - 2);
    let held = enqueue(&mut queue, b"last");
    assert_eq!(held.operation.lsn(), Lsn::MAX - 1);
    assert!(matches!(
        queue.enqueue(
            Bytes::new(),
            Epoch::new(0, 1),
            BufferPolicy::FramedSegments,
            limits()
        ),
        Err(QueueAdmissionError::SequenceNumberOverflow)
    ));
    assert_eq!(queue.last_sequence_number(), Lsn::MAX - 1);
    assert_eq!(queue.operation_count(), 1);
    assert_eq!(queue.total_bytes(), 4);
    assert_eq!(queue.original_epochs.len(), 1);
    queue.update_queue(None, None).unwrap();
    assert_eq!(queue.current_progress(), Lsn::MAX - 1);
}

#[test]
fn quorum_commit_all_completion_and_retained_capability_are_distinct() {
    let mut queue = ReplicationQueueManager::new(config(), 0);
    let first = enqueue(&mut queue, b"a");
    let second = enqueue(&mut queue, b"bb");
    enqueue(&mut queue, b"ccc");

    let update = queue.update_queue(Some(2), Some(0)).unwrap();
    assert_eq!(update.old_committed, 0);
    assert_eq!(update.new_committed, 2);
    assert!(!update.removed_items);
    assert_eq!(queue.current_progress(), 2);
    assert_eq!(queue.last_sequence_number(), 3);
    assert_eq!(queue.catchup_capability(), 1);
    assert_eq!(queue.first_lsn_in_replication_queue(), 1);
    assert_eq!(queue.total_bytes(), 6);

    queue.update_queue(Some(2), Some(1)).unwrap();
    assert_eq!(queue.current_progress(), 2);
    assert_eq!(queue.catchup_capability(), 2);
    assert_eq!(queue.first_lsn_in_replication_queue(), 2);
    assert_eq!(queue.total_bytes(), 5);
    assert_eq!(Arc::strong_count(&first.operation), 1);
    assert_eq!(Arc::strong_count(&second.operation), 2);

    queue.update_queue(Some(2), Some(99)).unwrap();
    assert_eq!(queue.current_progress(), 2);
    assert_eq!(queue.catchup_capability(), 0);
    assert_eq!(queue.first_lsn_in_replication_queue(), 3);
    assert_eq!(queue.operation_count(), 1);
    assert_eq!(queue.total_bytes(), 3);
    assert_eq!(Arc::strong_count(&second.operation), 1);
}

#[test]
fn no_remote_sentinel_commits_tail_but_specific_all_progress_still_retains() {
    let mut queue = ReplicationQueueManager::new(config(), 5);
    enqueue(&mut queue, b"a");
    enqueue(&mut queue, b"b");
    assert_eq!(
        queue.update_queue(None, Some(5)).unwrap(),
        QueueUpdate {
            old_committed: 5,
            new_committed: 7,
            removed_items: false,
        }
    );
    assert_eq!(queue.catchup_capability(), 6);
    let update = queue.update_queue(Some(7), None).unwrap();
    assert!(update.removed_items);
    assert_eq!(queue.operation_count(), 0);
    assert_eq!(queue.current_progress(), 7);
    assert_eq!(queue.catchup_capability(), 0);
}

#[test]
fn projection_neither_mutates_queue_nor_captures_configuration_boundaries() {
    let mut queue = ReplicationQueueManager::new(config(), 0);
    enqueue(&mut queue, b"a");
    enqueue(&mut queue, b"b");
    enqueue(&mut queue, b"c");
    queue.update_queue(Some(1), Some(0)).unwrap();
    queue.update_catchup_completion_lsn();
    let before = queue.queue.progress();
    assert_eq!(
        queue.catchup_progress(None, Some(2)).unwrap(),
        CatchupProgress {
            committed: 3,
            completed: 2,
            latest: 3,
            previous_last: 3,
        }
    );
    assert_eq!(queue.catchup_progress(Some(0), None).unwrap().committed, 0);
    assert_eq!(queue.queue.progress(), before);
    assert_eq!(queue.operation_count(), 3);
    assert_eq!(queue.previous_config_quorum_lsn(), 1);
}

#[test]
fn explicit_configuration_capture_raises_boundaries_without_rewinds() {
    let mut queue = ReplicationQueueManager::new(config(), 0);
    enqueue(&mut queue, b"a");
    enqueue(&mut queue, b"b");
    queue.update_queue(Some(2), Some(0)).unwrap();
    assert_eq!(queue.previous_config_catchup_lsn(), 0);
    queue.update_catchup_completion_lsn();
    assert_eq!(queue.previous_config_catchup_lsn(), 2);
    assert_eq!(queue.previous_config_quorum_lsn(), 2);
    queue.update_queue(Some(1), Some(0)).unwrap();
    queue.update_catchup_completion_lsn();
    assert_eq!(queue.current_progress(), 1);
    assert_eq!(queue.previous_config_quorum_lsn(), 2);
    enqueue(&mut queue, b"c");
    assert_eq!(queue.previous_config_catchup_lsn(), 2);
    assert_eq!(queue.check_with_catchup_lsn(3), 2);
    assert_eq!(queue.check_with_catchup_lsn(1), 1);
    queue.update_catchup_completion_lsn();
    assert_eq!(queue.previous_config_catchup_lsn(), 3);
    assert_eq!(queue.previous_config_quorum_lsn(), 2);
}

#[test]
fn replay_preserves_each_original_epoch_and_released_history_is_unavailable() {
    let mut queue = ReplicationQueueManager::new(config(), 0);
    let old = enqueue(&mut queue, b"old");
    let new = queue
        .enqueue(
            Bytes::from_static(b"new"),
            Epoch::new(7, 9),
            BufferPolicy::RawSingle,
            limits(),
        )
        .unwrap();
    let mut replay = vec![old.clone()];
    assert!(queue.get_operations(1, &mut replay));
    assert_eq!(replay.len(), 3);
    assert_eq!(replay[1].original_epoch, Epoch::new(1, 2));
    assert_eq!(replay[2].original_epoch, Epoch::new(7, 9));
    assert!(Arc::ptr_eq(&replay[2].operation, &new.operation));
    queue.update_queue(Some(1), Some(1)).unwrap();
    let mut replay = Vec::new();
    assert!(!queue.get_operations(1, &mut replay));
    assert!(replay.is_empty());
    assert!(queue.get_operations(2, &mut replay));
    assert_eq!(replay[0].original_epoch, Epoch::new(7, 9));
    assert_eq!(queue.original_epochs.len(), 1);
    assert!(queue.get_operations(3, &mut replay));
    assert_eq!(replay.len(), 1);
}

#[test]
fn invalid_progress_is_rejected_before_either_cursor_changes() {
    let mut queue = ReplicationQueueManager::new(config(), 0);
    enqueue(&mut queue, b"held");
    let before = queue.queue.progress();
    assert!(matches!(
        queue.update_queue(Some(2), None),
        Err(QueueProgressError::OutOfRange {
            value: 2,
            latest: 1
        })
    ));
    assert!(matches!(
        queue.update_queue(Some(1), Some(-1)),
        Err(QueueProgressError::OutOfRange { value: -1, .. })
    ));
    assert!(matches!(
        queue.catchup_progress(Some(-1), None),
        Err(QueueProgressError::OutOfRange { value: -1, .. })
    ));
    assert_eq!(queue.queue.progress(), before);
    assert_eq!(queue.operation_count(), 1);
    assert_eq!(queue.original_epochs.len(), 1);
}

#[test]
fn reset_requires_finished_work_and_updates_boundaries_after_queue_reset() {
    let mut queue = ReplicationQueueManager::new(config(), 0);
    let held = enqueue(&mut queue, b"held");
    assert_eq!(
        queue.reset_queue(20),
        Err(QueueNormalizationError::PendingOperations)
    );
    assert_eq!(queue.last_sequence_number(), 1);
    queue.update_queue(None, None).unwrap();
    assert_eq!(
        queue.reset_queue(Lsn::MAX),
        Err(QueueNormalizationError::InvalidInitialProgress)
    );
    assert_eq!(queue.current_progress(), 1);
    queue.reset_queue(20).unwrap();
    assert_eq!(queue.current_progress(), 20);
    assert_eq!(queue.last_sequence_number(), 20);
    assert_eq!(queue.previous_config_catchup_lsn(), 20);
    assert_eq!(queue.previous_config_quorum_lsn(), 20);
    assert_eq!(queue.catchup_capability(), 0);
    assert!(queue.original_epochs.is_empty());
    assert_eq!(held.operation.data().as_ref(), b"held");
    assert_eq!(enqueue(&mut queue, b"after reset").operation.lsn(), 21);
}

#[test]
fn transfer_discards_unfinished_work_reclassifies_history_and_preserves_issued_gap() {
    let mut secondary = OperationQueue::new(config(), 1, true, true);
    let applied = QueueOperation::new(1, Bytes::from_static(b"applied"), true);
    let discarded = QueueOperation::new(2, Bytes::from_static(b"pending"), true);
    secondary.try_enqueue(applied.clone()).unwrap();
    secondary.try_enqueue(discarded.clone()).unwrap();
    applied.release_ack_requirement();
    assert!(secondary.complete());
    let callbacks = Arc::new(AtomicUsize::new(0));
    let observed = callbacks.clone();
    secondary.set_commit_callback(Some(Box::new(move |_, _| {
        observed.fetch_add(1, Ordering::Relaxed);
        CallbackControl::Keep
    })));
    let mut queue = ReplicationQueueManager::from_transferred_queue(
        config(),
        secondary,
        BTreeMap::from([(1, Epoch::new(1, 1)), (2, Epoch::new(1, 2))]),
    )
    .unwrap();
    assert_eq!(queue.current_progress(), 1);
    assert_eq!(queue.last_sequence_number(), 1);
    assert_eq!(queue.catchup_capability(), 1);
    assert_eq!(queue.previous_config_catchup_lsn(), 1);
    assert_eq!(queue.previous_config_quorum_lsn(), 1);
    assert_eq!(queue.original_epochs.len(), 1);
    assert_eq!(Arc::strong_count(&discarded), 1);
    assert!(discarded.needs_ack());
    let mut replay = Vec::new();
    assert!(queue.get_operations(1, &mut replay));
    assert_eq!(replay[0].original_epoch, Epoch::new(1, 1));
    assert!(Arc::ptr_eq(&replay[0].operation, &applied));
    assert_eq!(enqueue(&mut queue, b"new").operation.lsn(), 3);
    replay.clear();
    assert!(!queue.get_operations(1, &mut replay));
    assert_eq!(replay.len(), 1);
    let before = queue.queue.progress();
    assert_eq!(
        queue.update_queue(Some(3), None),
        Err(QueueProgressError::MissingOperation { lsn: 2 })
    );
    assert_eq!(queue.queue.progress(), before);
    assert_eq!(queue.update_queue(None, None).unwrap().new_committed, 1);
    assert_eq!(queue.operation_count(), 1);
    assert_eq!(queue.original_epochs.len(), 1);
    assert_eq!(callbacks.load(Ordering::Relaxed), 0);
}

#[test]
fn transfer_rejects_missing_original_epoch_instead_of_substituting_current_epoch() {
    let mut secondary = OperationQueue::new(config(), 1, false, true);
    secondary
        .try_enqueue(QueueOperation::new(
            1,
            Bytes::from_static(b"applied"),
            false,
        ))
        .unwrap();
    secondary.complete();
    assert!(matches!(
        ReplicationQueueManager::from_transferred_queue(config(), secondary, BTreeMap::new()),
        Err(QueueNormalizationError::MissingOriginalEpoch { lsn: 1 })
    ));
}
