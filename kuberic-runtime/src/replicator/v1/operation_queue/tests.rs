// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Selected native OperationQueue.Test.cpp assertions adapted to the actual Rust
// queue; additional cases derive from production OperationQueue.cpp/.h.
// Pin/paths/cases/representation changes: docs/features/kuberic/sf-v1-port-provenance.md.
// Scenario IDs in comments are provenance, not a second vector catalog.

use super::*;
use bytes::Bytes;
use std::sync::Mutex;

fn config() -> QueueConfig {
    QueueConfig {
        initial_capacity: 4,
        max_capacity: 8,
        max_bytes: 0,
        max_completed_count: 0,
        max_completed_bytes: 0,
        clean_on_complete: false,
    }
}

fn operation(lsn: Lsn, bytes: usize) -> Arc<QueueOperation> {
    QueueOperation::new(lsn, Bytes::from(vec![lsn as u8; bytes]), false)
}

fn framed_operation(lsn: Lsn, bytes: usize) -> Arc<QueueOperation> {
    let limits = BufferLimits {
        max_segments: 3,
        max_payload_bytes: 100,
    };
    let data = buffers::encode(
        &[Bytes::new(), Bytes::from(vec![7; bytes]), Bytes::new()],
        BufferPolicy::FramedSegments,
        limits,
    )
    .unwrap();
    QueueOperation::from_data(lsn, data, false, BufferPolicy::FramedSegments, limits).unwrap()
}

fn enqueue(queue: &mut OperationQueue, lsn: Lsn, bytes: usize) -> Arc<QueueOperation> {
    let op = operation(lsn, bytes);
    queue.try_enqueue(op.clone()).unwrap();
    op
}

fn enqueue_range(queue: &mut OperationQueue, range: std::ops::RangeInclusive<Lsn>, bytes: usize) {
    for lsn in range {
        enqueue(queue, lsn, bytes);
    }
}

fn progress(retained_head: Lsn, complete_head: Lsn, commit_head: Lsn, tail: Lsn) -> QueueProgress {
    QueueProgress {
        retained_head,
        complete_head,
        commit_head,
        tail,
    }
}

type Observations = Arc<Mutex<Vec<(Lsn, QueueProgress)>>>;

fn recorder(observed: Observations) -> CommitCallback {
    Box::new(move |op, state| {
        observed.lock().unwrap().push((op.lsn(), state));
        CallbackControl::Keep
    })
}

#[test]
fn nonzero_progress_without_history_does_not_claim_retained_capability() {
    // Q3: native header 61–90. Zero here is native internal InvalidLSN.
    let queue = OperationQueue::new(config(), 101, false, false);
    assert_eq!(queue.progress(), progress(101, 101, 101, 101));
    assert_eq!(queue.progress().first_available_completed(), 0);
    assert_eq!(queue.progress().first_committed(), 0);
    assert!(!queue.progress().has_pending());
    assert!(!queue.progress().has_committed());
    assert_eq!(queue.operation_count(), 0);
    assert_eq!(queue.total_bytes(), 0);
    assert_eq!(queue.capacity(), 4);
    assert_eq!(queue.capacity_change_count(), 1);
    assert_eq!(queue.convergent_capacity(), 4);
}

#[test]
fn framed_queue_admission_completion_rewind_and_trim_count_only_logical_payload() {
    let mut queue = OperationQueue::new(
        QueueConfig {
            max_bytes: 10,
            ..config()
        },
        1,
        false,
        false,
    );
    let first = framed_operation(1, 4);
    assert!(first.data().len() > 10);
    assert_eq!(first.logical_bytes(), 4);
    queue.try_enqueue(first).unwrap();
    queue.try_enqueue(framed_operation(2, 6)).unwrap();
    assert_eq!(queue.total_bytes(), 10);
    assert_eq!(
        queue.try_enqueue(framed_operation(3, 1)),
        Err(EnqueueError::Full)
    );
    assert!(queue.commit());
    assert!(queue.complete());
    assert_eq!(queue.completed_bytes(), 10);
    assert!(queue.update_last_completed(1));
    assert_eq!(queue.completed_bytes(), 4);
    assert!(queue.complete());
    assert!(queue.update_complete_head(2));
    assert_eq!((queue.total_bytes(), queue.completed_bytes()), (6, 6));
    queue.try_enqueue(framed_operation(3, 4)).unwrap();
    assert_eq!(queue.total_bytes(), 10);
    queue.discard_noncompleted();
    assert_eq!((queue.total_bytes(), queue.completed_bytes()), (6, 6));
    queue.reset(20);
    assert_eq!((queue.total_bytes(), queue.completed_bytes()), (0, 0));
    assert_eq!(queue.operation_count(), 0);
}

#[test]
fn framed_clean_completion_and_suffix_displacement_do_not_subtract_frame_overhead() {
    let mut queue = OperationQueue::new(
        QueueConfig {
            max_bytes: 10,
            clean_on_complete: true,
            ..config()
        },
        1,
        false,
        false,
    );
    queue.try_enqueue(framed_operation(1, 4)).unwrap();
    queue.try_enqueue(framed_operation(3, 6)).unwrap();
    queue.try_enqueue(framed_operation(2, 6)).unwrap();
    assert!(queue.get_operation(3).is_none());
    assert_eq!(queue.total_bytes(), 10);
    assert!(queue.commit());
    assert!(queue.complete());
    assert_eq!((queue.operation_count(), queue.total_bytes()), (0, 0));
    assert_eq!(queue.completed_bytes(), 0);
}

#[test]
fn explicit_queue_data_representation_validates_frames_without_sniffing_raw_data() {
    let limits = BufferLimits {
        max_segments: 3,
        max_payload_bytes: 100,
    };
    let frame = buffers::encode(&[], BufferPolicy::FramedSegments, limits).unwrap();
    let raw =
        QueueOperation::from_data(1, frame.clone(), true, BufferPolicy::RawSingle, limits).unwrap();
    assert_eq!(raw.logical_bytes(), frame.len());
    let segmented =
        QueueOperation::from_data(1, frame, true, BufferPolicy::FramedSegments, limits).unwrap();
    assert_eq!(segmented.logical_bytes(), 0);
    assert!(segmented.needs_ack());
    assert!(matches!(
        QueueOperation::from_data(1, Bytes::new(), false, BufferPolicy::FramedSegments, limits,),
        Err(BufferError::Malformed)
    ));
}

#[test]
fn sparse_receive_and_out_of_order_service_ack_keep_four_boundaries_distinct() {
    // Q1; native randomized out-of-order ancestry, ACK predicate from cpp 677–749.
    let mut queue = OperationQueue::new(config(), 1, true, false);
    let first = QueueOperation::new(1, Bytes::from_static(b"first"), true);
    let second = QueueOperation::new(2, Bytes::from_static(b"second"), true);
    queue.try_enqueue(second.clone()).unwrap();
    assert_eq!(queue.progress(), progress(1, 1, 1, 3));
    assert!(queue.progress().has_pending());
    assert!(!queue.commit());
    assert!(!queue.complete());
    queue.try_enqueue(first.clone()).unwrap();
    assert!(queue.commit());
    assert_eq!(queue.progress(), progress(1, 1, 3, 3));
    assert_eq!(queue.progress().first_committed(), 1);
    second.release_ack_requirement();
    assert!(!queue.complete());
    assert_eq!(queue.completed_count(), 0);
    first.release_ack_requirement();
    assert!(queue.complete());
    assert_eq!(queue.progress(), progress(1, 3, 3, 3));
    assert_eq!(queue.completed_count(), 2);
    assert_eq!(queue.completed_bytes(), 11);
    assert_eq!(queue.total_bytes(), 11);
    assert_eq!(queue.progress().first_available_completed(), 1);
    assert_eq!(queue.progress().first_committed(), 0);
}

#[test]
fn completion_waits_for_commit_but_disabled_ack_gating_does_not_wait_for_service() {
    // Q3 / TestCompleteBeforeCommit (native test 607–615).
    let mut queue = OperationQueue::new(config(), 10, false, false);
    let op = QueueOperation::new(10, Bytes::new(), true);
    queue.try_enqueue(op.clone()).unwrap();
    assert!(!queue.complete());
    assert!(queue.commit());
    assert!(queue.complete());
    assert!(op.needs_ack()); // Queue completion is not an ACK callback.
    assert_eq!(queue.operation_count(), 1); // Empty data is not a hole.
    assert_eq!(queue.total_bytes(), 0);
    assert_eq!(queue.completed_count(), 1);
    assert_eq!(queue.progress(), progress(10, 11, 11, 11));
}

#[test]
fn occupied_and_removed_committed_duplicates_ignore_replacement_payload() {
    let mut queue = OperationQueue::new(config(), 1, false, false);
    let original = enqueue(&mut queue, 1, 7);
    assert_eq!(
        queue.try_enqueue(operation(1, 200)),
        Err(EnqueueError::Duplicate)
    );
    assert!(Arc::ptr_eq(queue.get_operation(1).unwrap(), &original));
    assert_eq!(queue.total_bytes(), 7);
    queue.commit();
    queue.complete();
    assert!(queue.update_complete_head(2));
    assert!(queue.get_operation(1).is_none());
    assert_eq!(
        queue.try_enqueue(operation(1, 200)),
        Err(EnqueueError::Duplicate)
    );
    assert_eq!(queue.progress(), progress(2, 2, 2, 2));
    assert_eq!(queue.operation_count(), 0);
}

#[test]
fn ordered_retrieval_preserves_existing_output_and_prefix_before_a_hole() {
    let mut queue = OperationQueue::new(config(), 1, false, false);
    let first = enqueue(&mut queue, 1, 1);
    enqueue(&mut queue, 3, 3);
    let marker = operation(99, 0);
    let mut out = vec![marker.clone()];
    assert!(!queue.get_operations(1, &mut out));
    assert_eq!(out.iter().map(|op| op.lsn()).collect::<Vec<_>>(), [99, 1]);
    assert!(Arc::ptr_eq(&out[1], &first));
    enqueue(&mut queue, 2, 2);
    out.clear();
    assert!(queue.get_operations(1, &mut out));
    assert_eq!(out.iter().map(|op| op.lsn()).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(out[2].data().len(), 3);
    assert!(queue.get_operations(4, &mut out));
    assert!(queue.get_operations(400, &mut out));
    assert_eq!(out.len(), 3);
}

#[test]
fn count_limit_bounds_positional_span_not_stored_count() {
    let mut queue = OperationQueue::new(config(), 10, false, false);
    enqueue(&mut queue, 17, 1);
    assert_eq!(queue.operation_count(), 1);
    assert_eq!(queue.capacity(), 8);
    assert_eq!(queue.try_enqueue(operation(18, 1)), Err(EnqueueError::Full));
    assert_eq!(queue.progress(), progress(10, 10, 10, 18));
    assert_eq!(queue.total_bytes(), 1);
    enqueue(&mut queue, 10, 0); // Fill a hole; no new tail or capacity.
    assert_eq!(queue.capacity(), 8);
    assert_eq!(queue.operation_count(), 2);
}

#[test]
fn memory_only_limit_permits_distant_sparse_lsn_and_grows_capacity() {
    // TestEnqueueOperationsWithMemoryLimitNoSizeLimit (native test 501–542).
    let mut queue = OperationQueue::new(
        QueueConfig {
            max_capacity: 0,
            max_bytes: 600,
            ..config()
        },
        10,
        false,
        false,
    );
    enqueue(&mut queue, 10, 100);
    enqueue(&mut queue, 1010, 500);
    assert_eq!(queue.capacity(), 1024);
    assert_eq!(queue.total_bytes(), 600);
    assert_eq!(queue.operation_count(), 2);
    assert_eq!(
        queue.try_enqueue(operation(1011, 1)),
        Err(EnqueueError::Full)
    );
    assert_eq!(queue.progress().tail, 1011);
}

#[test]
fn retaining_queue_expands_before_reclaiming_completed_history() {
    // TestOperationNotCleanedupWhenNotNeeded (native test 403–426).
    let mut queue = OperationQueue::new(config(), 1, false, false);
    enqueue_range(&mut queue, 1..=4, 100);
    queue.commit_through(2);
    queue.complete_through(2);
    enqueue(&mut queue, 5, 100);
    assert_eq!(queue.capacity(), 8);
    assert_eq!(queue.progress(), progress(1, 3, 3, 6));
    assert_eq!(queue.completed_bytes(), 200);
    assert_eq!(queue.total_bytes(), 500);
}

#[test]
fn count_pressure_removes_only_required_oldest_history_when_it_suffices() {
    let mut queue = OperationQueue::new(
        QueueConfig {
            max_capacity: 4,
            ..config()
        },
        1,
        false,
        false,
    );
    enqueue_range(&mut queue, 1..=4, 10);
    queue.commit_through(2);
    queue.complete_through(2);
    enqueue(&mut queue, 5, 10);
    assert_eq!(queue.progress(), progress(2, 3, 3, 6));
    assert!(queue.get_operation(1).is_none());
    assert!(queue.get_operation(2).is_some());
    assert_eq!(queue.completed_count(), 1);
    assert_eq!(queue.total_bytes(), 40);
}

#[test]
fn failed_count_admission_can_remove_all_history_without_admitting_input() {
    // TestEnqueueOperations (native test 586–592): failure is not transactional.
    let mut queue = OperationQueue::new(config(), 1, false, false);
    enqueue_range(&mut queue, 1..=8, 10);
    queue.commit_through(4);
    queue.complete_through(4);
    assert_eq!(
        queue.try_enqueue(operation(13, 10)),
        Err(EnqueueError::Full)
    );
    assert_eq!(queue.progress(), progress(5, 5, 5, 9));
    assert_eq!(queue.operation_count(), 4);
    assert_eq!(queue.completed_count(), 0);
    assert_eq!(queue.total_bytes(), 40);
    assert!(queue.get_operation(13).is_none());
    assert_eq!(queue.capacity(), 8);
}

#[test]
fn oversized_next_operation_rejects_without_evicting_history() {
    // Q2 / native test 450–461.
    let mut queue = OperationQueue::new(
        QueueConfig {
            max_bytes: 600,
            ..config()
        },
        10,
        false,
        false,
    );
    enqueue_range(&mut queue, 10..=13, 100);
    queue.commit_through(10);
    queue.complete_through(10);
    assert_eq!(
        queue.try_enqueue(operation(14, 301)),
        Err(EnqueueError::Full)
    );
    assert_eq!(queue.progress(), progress(10, 11, 11, 14));
    assert_eq!(queue.total_bytes(), 400);
    assert_eq!(queue.completed_bytes(), 100);
    enqueue(&mut queue, 14, 100);
    assert_eq!(queue.total_bytes(), 500);
    assert_eq!(queue.capacity(), 8);
}

#[test]
fn exact_byte_admission_evicts_completed_prefix_after_pending_bytes_fit() {
    // Production-derived neighbor of Q2, not an executed native test.
    let mut queue = OperationQueue::new(
        QueueConfig {
            max_bytes: 600,
            ..config()
        },
        10,
        false,
        false,
    );
    enqueue_range(&mut queue, 10..=13, 100);
    queue.commit_through(10);
    queue.complete_through(10);
    enqueue(&mut queue, 14, 300);
    assert_eq!(queue.progress(), progress(11, 11, 11, 15));
    assert_eq!(queue.total_bytes(), 600);
    assert_eq!(queue.completed_bytes(), 0);
    assert_eq!(queue.operation_count(), 4);
    assert!(queue.get_operation(10).is_none());
}

#[test]
fn insufficient_higher_suffix_does_not_drop_any_operations() {
    // Reduced from native test 475–489: only operations above the hole may drop.
    let mut queue = OperationQueue::new(
        QueueConfig {
            max_bytes: 600,
            ..config()
        },
        17,
        false,
        false,
    );
    enqueue(&mut queue, 17, 1);
    enqueue(&mut queue, 18, 1);
    enqueue(&mut queue, 20, 399);
    enqueue(&mut queue, 24, 199);
    assert_eq!(
        queue.try_enqueue(operation(22, 200)),
        Err(EnqueueError::Full)
    );
    assert_eq!(queue.progress(), progress(17, 17, 17, 25));
    assert_eq!(queue.total_bytes(), 600);
    assert!(queue.get_operation(24).is_some());
}

#[test]
fn earlier_hole_displaces_sufficient_uncommitted_suffix_and_lowers_tail() {
    // Native test 490–493 uses 598 (not its stale neighboring "200" comment).
    let mut queue = OperationQueue::new(
        QueueConfig {
            max_bytes: 600,
            ..config()
        },
        17,
        false,
        false,
    );
    enqueue(&mut queue, 17, 1);
    enqueue(&mut queue, 18, 1);
    let later = enqueue(&mut queue, 20, 399);
    enqueue(&mut queue, 24, 199);
    enqueue(&mut queue, 19, 598);
    assert_eq!(queue.progress(), progress(17, 17, 17, 20));
    assert_eq!(queue.total_bytes(), 600);
    assert_eq!(queue.operation_count(), 3);
    assert!(queue.get_operation(20).is_none());
    assert!(queue.get_operation(24).is_none());
    assert_eq!(later.data().len(), 399); // Queue release is not object destruction.
}

#[test]
fn suffix_displacement_preserves_committed_pending_work_and_skips_holes() {
    let mut queue = OperationQueue::new(
        QueueConfig {
            max_bytes: 100,
            ..config()
        },
        1,
        false,
        false,
    );
    enqueue(&mut queue, 1, 60);
    queue.commit();
    enqueue(&mut queue, 4, 40);
    enqueue(&mut queue, 2, 40);
    assert_eq!(queue.progress(), progress(1, 1, 2, 3));
    assert_eq!(queue.total_bytes(), 100);
    assert!(queue.get_operation(1).is_some());
    assert!(queue.get_operation(4).is_none());
    assert_eq!(queue.try_enqueue(operation(3, 1)), Err(EnqueueError::Full));
}

#[test]
fn completed_count_limit_trims_on_strict_excess_not_equality() {
    // Q4 / TestSecondaryQueueTrim (native test 867–901).
    let mut queue = OperationQueue::new(
        QueueConfig {
            max_completed_count: 4,
            ..config()
        },
        1,
        false,
        false,
    );
    enqueue_range(&mut queue, 1..=8, 10);
    queue.commit();
    assert!(!queue.complete_through(0));
    assert_eq!(queue.progress().first_available_completed(), 0);
    queue.complete_through(1);
    assert_eq!(queue.progress().first_available_completed(), 1);
    queue.complete_through(4);
    assert_eq!(queue.progress().first_available_completed(), 1);
    queue.complete_through(5);
    assert_eq!(queue.progress().first_available_completed(), 2);
    queue.complete_through(8);
    assert_eq!(queue.progress(), progress(5, 9, 9, 9));
    assert_eq!(queue.completed_count(), 4);
    assert_eq!(queue.total_bytes(), 40);
    assert_eq!(queue.capacity(), 4);
}

#[test]
fn completed_byte_limit_is_independent_and_zero_count_limit_is_disabled() {
    let mut queue = OperationQueue::new(
        QueueConfig {
            max_completed_bytes: 20,
            ..config()
        },
        1,
        false,
        false,
    );
    enqueue_range(&mut queue, 1..=4, 10);
    queue.commit();
    queue.complete_through(2);
    assert_eq!(queue.progress().retained_head, 1);
    queue.complete_through(3);
    assert_eq!(queue.progress().retained_head, 2);
    assert_eq!(queue.completed_count(), 2);
    assert_eq!(queue.completed_bytes(), 20);
    assert_eq!(queue.operation_count(), 3);
}

#[test]
fn either_enabled_history_limit_can_trigger_trim_and_zero_bytes_still_count() {
    let mut queue = OperationQueue::new(
        QueueConfig {
            max_completed_count: 2,
            max_completed_bytes: 10,
            ..config()
        },
        1,
        false,
        false,
    );
    enqueue(&mut queue, 1, 10);
    enqueue(&mut queue, 2, 1);
    enqueue(&mut queue, 3, 0);
    enqueue(&mut queue, 4, 0);
    queue.commit();
    queue.complete_through(2); // Bytes trigger before count excess.
    assert_eq!(queue.progress().retained_head, 2);
    queue.complete_through(4); // Count triggers despite zero-sized entries.
    assert_eq!(queue.progress().retained_head, 3);
    assert_eq!(queue.completed_count(), 2);
    assert_eq!(queue.completed_bytes(), 0);
    assert_eq!(queue.operation_count(), 2);
}

#[test]
fn clean_completion_releases_queue_reference_and_shrinks_after_expansion() {
    let mut queue = OperationQueue::new(
        QueueConfig {
            clean_on_complete: true,
            ..config()
        },
        1,
        false,
        false,
    );
    let first = enqueue(&mut queue, 1, 10);
    let weak = Arc::downgrade(&first);
    enqueue_range(&mut queue, 2..=8, 10);
    queue.commit();
    assert_eq!(queue.capacity(), 8);
    queue.complete_through(4);
    assert_eq!(queue.progress(), progress(5, 5, 9, 9));
    assert_eq!(queue.capacity(), 4);
    assert_eq!(queue.total_bytes(), 40);
    assert_eq!(queue.completed_bytes(), 0);
    assert_eq!(queue.completed_count(), 0);
    assert!(weak.upgrade().is_some());
    drop(first);
    assert!(weak.upgrade().is_none());
}

#[test]
fn shrink_is_allowed_only_after_expansion_and_growth_uses_convergence() {
    let mut queue = OperationQueue::new(
        QueueConfig {
            initial_capacity: 2,
            max_capacity: 16,
            ..config()
        },
        1,
        false,
        false,
    );
    enqueue_range(&mut queue, 1..=9, 1); // 2 -> 4 -> 8 -> 16
    assert_eq!(queue.capacity(), 16);
    assert_eq!(queue.capacity_change_count(), 4);
    assert_eq!(queue.convergent_capacity(), 8); // ceil power of average 30/4.
    queue.commit();
    queue.complete_through(5);
    queue.update_complete_head(6); // span4 -> shrink8? remaining 6..9 = 4.
    assert_eq!(queue.capacity(), 4);
    assert_eq!(queue.capacity_change_count(), 5);
    assert_eq!(queue.convergent_capacity(), 8);
    queue.complete();
    queue.update_complete_head(10); // No second shrink until an expansion.
    assert_eq!(queue.capacity(), 4);
    enqueue_range(&mut queue, 10..=14, 1); // Direct convergent-capacity growth.
    assert_eq!(queue.capacity(), 8);
    assert_eq!(queue.capacity_change_count(), 6);
    assert_eq!(queue.progress(), progress(10, 10, 10, 15));
}

#[test]
fn prefix_trim_accepts_completion_boundary_without_advancing_progress() {
    let mut queue = OperationQueue::new(config(), 1, false, false);
    enqueue_range(&mut queue, 1..=4, 10);
    queue.commit_through(3);
    queue.complete_through(2);
    assert!(!queue.update_complete_head(1));
    assert!(!queue.update_complete_head(4));
    assert!(queue.update_complete_head(3));
    assert_eq!(queue.progress(), progress(3, 3, 4, 5));
    assert_eq!(queue.total_bytes(), 20);
    assert_eq!(queue.completed_count(), 0);
    assert!(queue.get_operation(3).is_some());
}

#[test]
fn normal_commit_callback_observes_pre_advance_cursor_and_replay_excludes_history() {
    let mut queue = OperationQueue::new(config(), 1, false, false);
    enqueue_range(&mut queue, 1..=3, 1);
    let observed = Observations::default();
    queue.set_commit_callback(Some(recorder(observed.clone())));
    queue.commit_through(2);
    assert_eq!(
        *observed.lock().unwrap(),
        [(1, progress(1, 1, 1, 4)), (2, progress(1, 1, 2, 4)),]
    );
    queue.complete_through(1);
    let replayed = Observations::default();
    queue.set_commit_callback(Some(recorder(replayed.clone())));
    assert_eq!(*replayed.lock().unwrap(), [(2, progress(1, 2, 3, 4))]);
    queue.commit();
    assert_eq!(replayed.lock().unwrap()[1], (3, progress(1, 2, 3, 4)));
}

#[test]
fn ignore_commit_completion_calls_back_after_commit_before_completion_and_stops_at_hole() {
    let mut queue = OperationQueue::new(config(), 1, false, true);
    enqueue(&mut queue, 1, 1);
    enqueue(&mut queue, 3, 1);
    let observed = Observations::default();
    queue.set_commit_callback(Some(recorder(observed.clone())));
    assert!(queue.complete());
    assert_eq!(*observed.lock().unwrap(), [(1, progress(1, 1, 2, 4))]);
    assert_eq!(queue.progress(), progress(1, 2, 2, 4));
    assert!(!queue.complete());
    enqueue(&mut queue, 2, 1);
    queue.complete();
    assert_eq!(observed.lock().unwrap()[1], (2, progress(1, 2, 3, 4)));
    assert_eq!(observed.lock().unwrap()[2], (3, progress(1, 3, 4, 4)));
}

#[test]
fn callback_can_disable_itself_without_stopping_commit_and_can_be_replayed() {
    let mut queue = OperationQueue::new(config(), 1, false, false);
    enqueue_range(&mut queue, 1..=3, 1);
    let observed = Observations::default();
    let sink = observed.clone();
    queue.set_commit_callback(Some(Box::new(move |op, state| {
        sink.lock().unwrap().push((op.lsn(), state));
        CallbackControl::Disable
    })));
    queue.commit();
    assert_eq!(observed.lock().unwrap().len(), 1);
    assert_eq!(queue.progress().commit_head, 4);
    queue.set_commit_callback(Some(recorder(observed.clone())));
    assert_eq!(
        observed
            .lock()
            .unwrap()
            .iter()
            .map(|(lsn, _)| *lsn)
            .collect::<Vec<_>>(),
        [1, 1, 2, 3]
    );
    queue.set_commit_callback(None);
    assert_eq!(observed.lock().unwrap().len(), 4);
}

#[test]
fn callback_self_disable_also_stops_installation_replay() {
    let mut queue = OperationQueue::new(config(), 1, false, false);
    enqueue_range(&mut queue, 1..=3, 1);
    queue.commit();
    let observed = Observations::default();
    let sink = observed.clone();
    queue.set_commit_callback(Some(Box::new(move |op, state| {
        sink.lock().unwrap().push((op.lsn(), state));
        CallbackControl::Disable
    })));
    assert_eq!(*observed.lock().unwrap(), [(1, progress(1, 1, 4, 4))]);
}

#[test]
fn inclusive_targets_cap_normal_completion_at_committed_boundary() {
    let mut queue = OperationQueue::new(config(), 10, false, false);
    enqueue_range(&mut queue, 10..=13, 1);
    assert!(!queue.commit_through(0));
    assert!(!queue.complete_through(13)); // Target valid, but not committed yet.
    assert!(queue.commit_through(11));
    assert!(!queue.commit_through(10));
    assert!(queue.complete_through(13));
    assert_eq!(queue.progress(), progress(10, 12, 12, 14));
    assert!(!queue.complete_through(11));
    assert!(!queue.complete_through(-1));
}

#[test]
fn completion_rewind_clamps_to_retained_floor_without_changing_total_or_commit() {
    let mut queue = OperationQueue::new(config(), 10, false, false);
    enqueue_range(&mut queue, 10..=13, 10);
    queue.commit();
    queue.complete_through(12);
    queue.update_complete_head(11);
    assert!(!queue.update_last_completed(-1));
    assert!(queue.update_last_completed(12));
    assert!(queue.update_last_completed(0));
    assert_eq!(queue.progress(), progress(11, 11, 14, 14));
    assert_eq!(queue.completed_bytes(), 0);
    assert_eq!(queue.total_bytes(), 30);
    assert_eq!(queue.operation_count(), 3);
    assert!(!queue.update_last_completed(0)); // No available history now.
    assert!(queue.update_last_completed(12)); // Forward targeted completion.
    assert_eq!(queue.completed_count(), 2);
}

#[test]
fn commit_rewind_cannot_cross_completion_and_recommit_replays() {
    let mut queue = OperationQueue::new(config(), 1, false, false);
    enqueue_range(&mut queue, 1..=3, 1);
    let observed = Observations::default();
    queue.set_commit_callback(Some(recorder(observed.clone())));
    queue.commit();
    queue.complete_through(1);
    assert!(!queue.update_commit_head(-1));
    assert!(queue.update_commit_head(3)); // Unchanged succeeds.
    assert!(!queue.update_commit_head(0));
    assert!(queue.update_commit_head(1));
    assert_eq!(queue.progress(), progress(1, 2, 2, 4));
    assert!(queue.update_commit_head(3));
    assert_eq!(
        observed
            .lock()
            .unwrap()
            .iter()
            .map(|(lsn, _)| *lsn)
            .collect::<Vec<_>>(),
        [1, 2, 3, 2, 3]
    );
    assert_eq!(queue.total_bytes(), 3);
}

#[test]
fn reset_completed_reclassifies_retained_entries_without_removing_or_dispatching_them() {
    let mut queue = OperationQueue::new(config(), 1, false, false);
    enqueue_range(&mut queue, 1..=3, 10);
    queue.commit();
    queue.complete_through(2);
    let observed = Observations::default();
    queue.set_commit_callback(Some(recorder(observed.clone())));
    assert_eq!(observed.lock().unwrap().len(), 1);
    queue.reset_completed();
    assert_eq!(queue.progress(), progress(1, 1, 4, 4));
    assert_eq!(queue.completed_count(), 0);
    assert_eq!(queue.completed_bytes(), 0);
    assert_eq!(queue.total_bytes(), 30);
    assert_eq!(queue.operation_count(), 3);
    assert_eq!(observed.lock().unwrap().len(), 1);
    queue.set_commit_callback(Some(recorder(observed.clone())));
    assert_eq!(observed.lock().unwrap().len(), 4);
}

#[test]
fn suffix_discards_keep_completed_history_and_distinguish_commit_from_completion() {
    let mut queue = OperationQueue::new(config(), 1, false, false);
    enqueue_range(&mut queue, 1..=3, 10);
    enqueue(&mut queue, 8, 10); // holes in discarded suffix
    queue.commit_through(2);
    queue.complete_through(1);
    queue.discard_noncommitted();
    assert_eq!(queue.progress(), progress(1, 2, 3, 3));
    assert_eq!(queue.total_bytes(), 20);
    assert_eq!(queue.capacity(), 4);
    enqueue(&mut queue, 4, 10);
    queue.discard_noncompleted();
    assert_eq!(queue.progress(), progress(1, 2, 2, 2));
    assert_eq!(queue.total_bytes(), 10);
    assert_eq!(queue.completed_count(), 1);
    assert!(queue.get_operation(1).is_some());
}

#[test]
fn full_reset_preserves_callback_flags_and_limits_but_clears_state_and_resize_history() {
    let mut queue = OperationQueue::new(
        QueueConfig {
            max_bytes: 100,
            clean_on_complete: true,
            ..config()
        },
        1,
        true,
        true,
    );
    let held = QueueOperation::new(1, Bytes::from_static(b"held"), true);
    queue.try_enqueue(held.clone()).unwrap();
    enqueue_range(&mut queue, 2..=6, 1);
    let observed = Observations::default();
    queue.set_commit_callback(Some(recorder(observed.clone())));
    assert_eq!(queue.capacity(), 8);
    queue.reset(101);
    assert_eq!(queue.progress(), progress(101, 101, 101, 101));
    assert_eq!(queue.total_bytes(), 0);
    assert_eq!(queue.completed_bytes(), 0);
    assert_eq!(queue.completed_count(), 0);
    assert_eq!(queue.operation_count(), 0);
    assert_eq!(queue.capacity(), 4);
    assert_eq!(queue.capacity_change_count(), 1);
    assert_eq!(queue.convergent_capacity(), 4);
    assert!(held.needs_ack()); // Reset did not cancel/ACK externally held entry.
    let next = QueueOperation::new(101, Bytes::from_static(b"new"), true);
    queue.try_enqueue(next.clone()).unwrap();
    assert!(!queue.complete()); // ACK requirement survived.
    next.release_ack_requirement();
    assert!(queue.complete()); // Ignore-commit and clean policy survived.
    assert_eq!(
        observed.lock().unwrap()[0],
        (101, progress(101, 101, 102, 102))
    );
    assert_eq!(queue.operation_count(), 0);
    assert_eq!(
        queue.try_enqueue(operation(102, 101)),
        Err(EnqueueError::Full)
    );
}

#[test]
fn transfer_inherits_progress_ack_mode_and_callback_without_replay_or_byte_history_normalization() {
    // L2 queue-owned subset only; no manager promotion/demotion policy.
    let mut queue = OperationQueue::new(config(), 1, true, true);
    enqueue_range(&mut queue, 1..=6, 10);
    let observed = Observations::default();
    queue.set_commit_callback(Some(recorder(observed.clone())));
    queue.complete_through(5);
    assert_eq!(observed.lock().unwrap().len(), 5);
    let mut queue = queue.transfer(QueueConfig {
        initial_capacity: 2,
        max_capacity: 8,
        max_bytes: 20,
        max_completed_count: 1,
        ..config()
    });
    assert_eq!(queue.progress(), progress(1, 6, 6, 7));
    assert_eq!(queue.total_bytes(), 60); // New byte/history limits aren't eager trim.
    assert_eq!(queue.completed_count(), 5);
    assert_eq!(queue.capacity(), 8); // Retained span doesn't fit smaller capacity.
    assert_eq!(queue.capacity_change_count(), 1);
    assert_eq!(queue.convergent_capacity(), 8);
    assert_eq!(observed.lock().unwrap().len(), 5); // No transfer replay.
    queue.complete(); // Native completion-triggered retention now normalizes.
    assert_eq!(queue.progress(), progress(6, 7, 7, 7));
    assert_eq!(queue.total_bytes(), 10);
    assert_eq!(queue.completed_count(), 1);
    assert_eq!(queue.capacity(), 2);
    assert_eq!(observed.lock().unwrap().len(), 6);
    let next = QueueOperation::new(7, Bytes::from_static(b"ack"), true);
    queue.try_enqueue(next.clone()).unwrap();
    assert!(!queue.complete());
    next.release_ack_requirement();
    assert!(queue.complete());
}

#[test]
fn bounded_transfer_forces_shrink_and_resets_statistics_but_memory_only_transfer_does_not() {
    let mut bounded = OperationQueue::new(config(), 1, false, false);
    enqueue_range(&mut bounded, 1..=8, 1);
    bounded.commit_through(2);
    bounded.discard_noncommitted(); // 8 -> 4, disables another ordinary shrink.
    assert_eq!(bounded.capacity(), 4);
    assert_eq!(bounded.capacity_change_count(), 3);
    let bounded = bounded.transfer(QueueConfig {
        initial_capacity: 2,
        max_capacity: 2,
        ..config()
    });
    assert_eq!(bounded.capacity(), 2);
    assert_eq!(bounded.capacity_change_count(), 1);
    assert_eq!(bounded.progress(), progress(1, 1, 3, 3));

    let mut memory_only = OperationQueue::new(config(), 1, false, false);
    enqueue_range(&mut memory_only, 1..=8, 1);
    memory_only.commit_through(2);
    memory_only.discard_noncommitted();
    let memory_only = memory_only.transfer(QueueConfig {
        initial_capacity: 2,
        max_capacity: 0,
        max_bytes: 100,
        ..config()
    });
    assert_eq!(memory_only.capacity(), 4);
    assert_eq!(memory_only.capacity_change_count(), 3);
    assert_eq!(memory_only.convergent_capacity(), 8);
    assert_eq!(memory_only.total_bytes(), 2);
}

#[test]
fn clean_flag_assignment_does_not_eagerly_delete_history() {
    let mut queue = OperationQueue::new(config(), 1, false, false);
    enqueue(&mut queue, 1, 10);
    queue.commit();
    queue.complete();
    queue.set_clean_on_complete(true);
    assert_eq!(queue.progress(), progress(1, 2, 2, 2));
    assert_eq!(queue.operation_count(), 1);
    queue.set_clean_on_complete(false); // Owner restores validity before queue mutation.
    assert!(queue.update_complete_head(2));
    queue.set_clean_on_complete(true);
    queue.set_ignore_commit(true);
    enqueue(&mut queue, 2, 10);
    queue.complete();
    assert_eq!(queue.progress(), progress(3, 3, 3, 3));
}

#[test]
#[should_panic(expected = "commit target crosses hole")]
fn targeted_commit_requires_contiguous_operations_instead_of_skipping_a_gap() {
    let mut queue = OperationQueue::new(config(), 1, false, false);
    enqueue(&mut queue, 2, 1);
    queue.commit_through(2);
}

#[test]
#[should_panic(expected = "completion target requires ACK")]
fn targeted_completion_requires_eligible_ack_state_instead_of_scanning_past_it() {
    let mut queue = OperationQueue::new(config(), 1, true, false);
    queue
        .try_enqueue(QueueOperation::new(1, Bytes::new(), true))
        .unwrap();
    queue.commit();
    queue.complete_through(1);
}

#[test]
#[should_panic(expected = "completion target must be below tail")]
fn targeted_completion_checks_tail_even_for_an_empty_queue() {
    let mut queue = OperationQueue::new(config(), 1, false, false);
    queue.complete_through(1);
}

#[test]
#[should_panic]
fn setting_ignore_commit_even_to_existing_value_requires_equal_boundaries() {
    let mut queue = OperationQueue::new(config(), 1, false, false);
    enqueue(&mut queue, 1, 1);
    queue.commit();
    queue.set_ignore_commit(false);
}

#[test]
fn configuration_assertions_preserve_independent_zero_limits_and_native_clean_conjunction() {
    for bad in [
        QueueConfig {
            initial_capacity: 1,
            ..config()
        },
        QueueConfig {
            initial_capacity: 3,
            ..config()
        },
        QueueConfig {
            max_capacity: 3,
            ..config()
        },
        QueueConfig {
            max_capacity: 2,
            ..config()
        },
        QueueConfig {
            max_capacity: 0,
            max_bytes: 0,
            ..config()
        },
        QueueConfig {
            clean_on_complete: true,
            max_completed_count: 1,
            max_completed_bytes: 1,
            ..config()
        },
    ] {
        assert!(std::panic::catch_unwind(|| OperationQueue::new(bad, 1, false, false)).is_err());
    }

    // Native rejects both nonzero history limits together in clean mode, not either alone.
    let _queue = OperationQueue::new(
        QueueConfig {
            clean_on_complete: true,
            max_completed_count: 1,
            ..config()
        },
        1,
        false,
        false,
    );
}

#[test]
fn transferred_over_limit_history_does_not_turn_no_progress_commit_into_validation() {
    // Review regression: cpp 990–1126 checks invariants only on advancement.
    for with_gap in [false, true] {
        let mut queue = OperationQueue::new(
            QueueConfig {
                initial_capacity: 2,
                ..config()
            },
            1,
            false,
            false,
        );
        enqueue_range(&mut queue, 1..=3, 1);
        queue.commit();
        queue.complete_through(2);
        if with_gap {
            enqueue(&mut queue, 5, 1);
        }
        let mut queue = queue.transfer(QueueConfig {
            initial_capacity: 2,
            max_completed_count: 1,
            ..config()
        });
        let before = queue.progress();
        assert!(!queue.commit()); // Tail reached or a hole: no invariant check.
        assert!(!queue.commit_through(3)); // Already committed, valid interval.
        assert_eq!(queue.progress(), before);
        assert_eq!(queue.completed_count(), 2); // Still not eagerly normalized.
        assert_eq!(queue.completed_bytes(), 2);
    }
}

#[test]
fn transferred_over_limit_history_survives_noop_suffix_discards() {
    // cpp 1244–1251 returns immediately when there is no suffix to discard.
    let mut queue = OperationQueue::new(config(), 1, false, false);
    enqueue_range(&mut queue, 1..=3, 1);
    queue.commit();
    queue.complete();
    let mut queue = queue.transfer(QueueConfig {
        max_completed_count: 1,
        ..config()
    });
    queue.discard_noncommitted();
    queue.discard_noncompleted();
    assert_eq!(queue.progress(), progress(1, 4, 4, 4));
    assert_eq!(queue.operation_count(), 3);
    assert_eq!(queue.completed_count(), 3);
    assert_eq!(queue.total_bytes(), 3);
}

#[test]
fn count_pressure_can_normalize_transferred_history_after_a_no_pressure_byte_pass() {
    // cpp 306–318 must not validate/trim when total bytes already fit.
    let settings = QueueConfig {
        max_bytes: 100,
        ..config()
    };
    let mut queue = OperationQueue::new(settings, 1, false, false);
    enqueue_range(&mut queue, 1..=8, 1);
    queue.commit();
    queue.complete_through(5);
    let mut queue = queue.transfer(QueueConfig {
        max_completed_count: 1,
        ..settings
    });
    enqueue(&mut queue, 13, 1);
    assert_eq!(queue.progress(), progress(6, 6, 9, 14));
    assert_eq!(queue.operation_count(), 4);
    assert_eq!(queue.completed_count(), 0);
    assert_eq!(queue.completed_bytes(), 0);
    assert_eq!(queue.total_bytes(), 4);
    assert!(queue.get_operation(13).is_some());
}
