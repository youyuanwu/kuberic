// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-component assertions derived from ComFromBytesOperation.cpp:127-257
// and ComFromBytesOperation.h at microsoft/service-fabric
// @3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d. These are production-branch-derived
// Rust tests, not native test executions or a duplicate callback-state model.

use super::*;
use crate::application::Lsn;
use crate::replicator::v1::operation_queue::{EnqueueError, OperationQueue, QueueConfig};
use std::sync::{Barrier, Weak, mpsc};
use std::thread;

fn operation(lsn: Lsn, callback_required: bool) -> ReceivedOperation {
    ReceivedOperation::new(
        QueueOperation::new(lsn, OperationData::from_static(b"operation"), true),
        Epoch::new(3, 7),
        callback_required,
    )
}

fn selected(operation: &ReceivedOperation) -> AckWork {
    operation.acknowledge().unwrap().unwrap()
}

fn assert_state(operation: &ReceivedOperation, state: AckState, needs_ack: bool) {
    assert_eq!(operation.ack_state(), state);
    assert_eq!(operation.queue_operation().needs_ack(), needs_ack);
}

fn owner() -> (Arc<()>, Weak<()>) {
    let owner = Arc::new(());
    let weak = Arc::downgrade(&owner);
    (owner, weak)
}

fn queue(require_ack: bool, ignore_commit: bool) -> OperationQueue {
    OperationQueue::new(
        QueueConfig {
            initial_capacity: 4,
            max_capacity: 8,
            max_bytes: 0,
            max_completed_count: 0,
            max_completed_bytes: 0,
            clean_on_complete: true,
        },
        1,
        require_ack,
        ignore_commit,
    )
}

#[test]
fn clones_preserve_instance_queue_payload_and_original_epoch() {
    let operation = operation(41, true);
    let clone = operation.clone();
    assert!(operation.same_instance(&clone));
    assert!(Arc::ptr_eq(
        operation.queue_operation(),
        clone.queue_operation()
    ));
    assert_eq!(clone.queue_operation().lsn(), 41);
    assert_eq!(clone.queue_operation().data().as_ref(), b"operation");
    assert_eq!(clone.epoch(), Epoch::new(3, 7));
    assert!(clone.callback_required());
    assert!(!clone.is_end_of_stream());
    assert_state(&operation, AckState::NotCalled, true);
    let mut work = selected(&clone);
    assert!(work.operation().same_instance(&operation));
    assert_state(&operation, AckState::ReadyToRun, true);
    work.start().unwrap();
    assert_state(&clone, AckState::Running, false);
    work.finish().unwrap();
    assert_state(&operation, AckState::Completed, false);
}

#[test]
#[should_panic(expected = "a received operation requires a fresh ACK-blocking queue entry")]
fn already_released_queue_entry_cannot_start_a_new_received_lifecycle() {
    let queue_operation = QueueOperation::new(1, OperationData::new(), true);
    queue_operation.release_ack_requirement();
    ReceivedOperation::new(queue_operation, Epoch::new(3, 7), true);
}

#[test]
fn equal_lsn_does_not_alias_callback_instances_or_replace_queued_operation() {
    let original = operation(1, true);
    let replacement = operation(1, true);
    assert!(!original.same_instance(&replacement));
    let mut queue = queue(true, false);
    queue
        .try_enqueue(original.queue_operation().clone())
        .unwrap();
    assert_eq!(
        queue.try_enqueue(replacement.queue_operation().clone()),
        Err(EnqueueError::Duplicate)
    );
    assert!(Arc::ptr_eq(
        queue.get_operation(1).unwrap(),
        original.queue_operation()
    ));
    let mut work = selected(&replacement);
    work.start().unwrap();
    work.finish().unwrap();
    assert!(queue.commit());
    assert!(!queue.complete());
    assert_state(&original, AckState::NotCalled, true);
    assert_state(&replacement, AckState::Completed, false);
}

#[test]
fn repeated_ack_is_invalid_in_ready_running_and_completed_states() {
    let operation = operation(1, true);
    let mut work = selected(&operation);
    assert_state(&operation, AckState::ReadyToRun, true);
    assert!(matches!(
        operation.acknowledge(),
        Err(AckError::InvalidOperation)
    ));
    work.start().unwrap();
    assert_state(&operation, AckState::Running, false);
    assert!(matches!(
        operation.acknowledge(),
        Err(AckError::InvalidOperation)
    ));
    work.finish().unwrap();
    assert!(matches!(
        operation.acknowledge(),
        Err(AckError::InvalidOperation)
    ));
    assert_state(&operation, AckState::Completed, false);
}

#[test]
fn callback_cannot_start_twice() {
    let operation = operation(1, true);
    let mut work = selected(&operation);
    work.start().unwrap();
    assert_eq!(work.start(), Err(AckError::InvalidOperation));
    assert_state(&operation, AckState::Running, false);
    work.finish().unwrap();
}

#[test]
fn finish_before_start_is_invalid_and_does_not_complete_or_unroot_work() {
    let operation = operation(1, true);
    let work = selected(&operation);
    let (owner, weak) = owner();
    operation.ignore_ack_and_keep_owner(owner);
    assert_eq!(work.finish(), Err(AckError::InvalidOperation));
    assert_state(&operation, AckState::ReadyToRun, true);
    assert!(weak.upgrade().is_some());
    drop(operation);
    assert!(weak.upgrade().is_none());
}

#[test]
fn no_callback_ack_completes_immediately_without_selected_work() {
    let operation = operation(1, false);
    assert!(!operation.callback_required());
    assert_state(&operation, AckState::NotCalled, true);
    assert!(operation.acknowledge().unwrap().is_none());
    assert_state(&operation, AckState::Completed, false);
    assert!(matches!(
        operation.acknowledge(),
        Err(AckError::InvalidOperation)
    ));
}

#[test]
fn ordinary_retirement_cancels_not_called_and_late_acks_succeed_without_work() {
    for callback_required in [false, true] {
        let operation = operation(1, callback_required);
        let (owner, weak) = owner();
        operation.ignore_ack_and_keep_owner(owner);
        assert!(weak.upgrade().is_none());
        assert_state(&operation, AckState::Cancelled, false);
        for _ in 0..3 {
            assert!(operation.acknowledge().unwrap().is_none());
            assert_state(&operation, AckState::Cancelled, false);
        }
    }
}

#[test]
fn ordinary_ready_retirement_retains_owner_and_queue_requirement_until_start() {
    let operation = operation(1, true);
    let mut work = selected(&operation);
    let (owner, weak) = owner();
    operation.ignore_ack_and_keep_owner(owner);
    assert_state(&operation, AckState::ReadyToRun, true);
    assert!(weak.upgrade().is_some());
    work.start().unwrap();
    assert_state(&operation, AckState::Running, false);
    assert!(weak.upgrade().is_some());
    work.finish().unwrap();
    assert_state(&operation, AckState::Completed, false);
    assert!(weak.upgrade().is_none());
}

#[test]
fn ordinary_running_retirement_roots_until_real_finish_not_queue_release() {
    let operation = operation(1, true);
    let mut work = selected(&operation);
    work.start().unwrap();
    let (owner, weak) = owner();
    operation.ignore_ack_and_keep_owner(owner);
    assert_state(&operation, AckState::Running, false);
    assert!(weak.upgrade().is_some());
    work.finish().unwrap();
    assert!(weak.upgrade().is_none());
}

#[test]
fn ordinary_completed_and_cancelled_retirement_do_not_retain_owner() {
    for completed in [false, true] {
        let operation = operation(1, false);
        if completed {
            assert!(operation.acknowledge().unwrap().is_none());
        } else {
            operation.ignore_ack_and_keep_owner(Arc::new(()));
        }
        let state = operation.ack_state();
        let (owner, weak) = owner();
        operation.ignore_ack_and_keep_owner(owner);
        assert!(weak.upgrade().is_none());
        assert_state(&operation, state, false);
    }
}

#[test]
fn eos_uses_native_invalid_metadata_and_is_distinct_from_empty_ordinary_data() {
    let eos = ReceivedOperation::end_of_stream(true);
    assert!(eos.is_end_of_stream());
    assert!(eos.callback_required());
    assert_eq!(eos.queue_operation().lsn(), 0);
    assert!(eos.queue_operation().data().is_empty());
    assert_eq!(eos.epoch(), Epoch::new(0, 0));
    assert_state(&eos, AckState::NotCalled, true);
    let ordinary = ReceivedOperation::new(
        QueueOperation::new(1, OperationData::new(), true),
        Epoch::new(0, 0),
        true,
    );
    assert!(!ordinary.is_end_of_stream());
    assert!(!eos.same_instance(&ReceivedOperation::end_of_stream(true)));
}

#[test]
fn eos_not_called_retirement_keeps_root_and_never_cancels_ack() {
    let eos = ReceivedOperation::end_of_stream(true);
    let (owner, weak) = owner();
    eos.ignore_ack_and_keep_owner(owner);
    assert_state(&eos, AckState::NotCalled, true);
    assert!(weak.upgrade().is_some());
    let mut work = selected(&eos);
    assert_state(&eos, AckState::ReadyToRun, true);
    assert!(weak.upgrade().is_some());
    work.start().unwrap();
    assert_state(&eos, AckState::Running, false);
    assert!(weak.upgrade().is_some());
    work.finish().unwrap();
    assert_state(&eos, AckState::Completed, false);
    assert!(weak.upgrade().is_none());
}

#[test]
fn eos_ready_and_running_retirement_keep_owner_until_finish() {
    for running in [false, true] {
        let eos = ReceivedOperation::end_of_stream(true);
        let mut work = selected(&eos);
        if running {
            work.start().unwrap();
        }
        let (owner, weak) = owner();
        eos.ignore_ack_and_keep_owner(owner);
        assert_state(
            &eos,
            if running {
                AckState::Running
            } else {
                AckState::ReadyToRun
            },
            !running,
        );
        assert!(weak.upgrade().is_some());
        if !running {
            work.start().unwrap();
        }
        work.finish().unwrap();
        assert!(weak.upgrade().is_none());
    }
}

#[test]
fn eos_without_callback_ack_completes_and_releases_pending_owner() {
    let eos = ReceivedOperation::end_of_stream(false);
    assert!(!eos.callback_required());
    let (owner, weak) = owner();
    eos.ignore_ack_and_keep_owner(owner);
    assert_state(&eos, AckState::NotCalled, true);
    assert!(weak.upgrade().is_some());
    assert!(eos.acknowledge().unwrap().is_none());
    assert_state(&eos, AckState::Completed, false);
    assert!(weak.upgrade().is_none());
}

#[test]
fn completed_eos_does_not_acquire_a_new_owner() {
    let eos = ReceivedOperation::end_of_stream(true);
    let mut work = selected(&eos);
    work.start().unwrap();
    work.finish().unwrap();
    let (owner, weak) = owner();
    eos.ignore_ack_and_keep_owner(owner);
    assert!(weak.upgrade().is_none());
    assert_state(&eos, AckState::Completed, false);
}

#[test]
fn repeated_pending_retirement_replaces_root_without_changing_ack_state() {
    for eos in [false, true] {
        let operation = if eos {
            ReceivedOperation::end_of_stream(true)
        } else {
            operation(1, true)
        };
        let mut work = selected(&operation);
        let (first, first_weak) = owner();
        operation.ignore_ack_and_keep_owner(first);
        let (second, second_weak) = owner();
        operation.ignore_ack_and_keep_owner(second);
        assert!(first_weak.upgrade().is_none());
        assert!(second_weak.upgrade().is_some());
        assert_state(&operation, AckState::ReadyToRun, true);
        work.start().unwrap();
        work.finish().unwrap();
        assert!(second_weak.upgrade().is_none());
    }
}

#[test]
fn selected_work_roots_the_exact_operation_after_external_handle_is_dropped() {
    let operation = operation(1, true);
    let weak_operation = Arc::downgrade(&operation.inner);
    let queue_operation = operation.queue_operation().clone();
    let mut work = selected(&operation);
    drop(operation);
    assert!(weak_operation.upgrade().is_some());
    assert!(Arc::ptr_eq(
        work.operation().queue_operation(),
        &queue_operation
    ));
    assert_state(work.operation(), AckState::ReadyToRun, true);
    work.start().unwrap();
    work.finish().unwrap();
    assert!(weak_operation.upgrade().is_none());
    assert!(!queue_operation.needs_ack());
}

#[test]
fn dropping_ready_work_neither_starts_nor_cancels_nor_unroots_callback() {
    for eos in [false, true] {
        let operation = if eos {
            ReceivedOperation::end_of_stream(true)
        } else {
            operation(1, true)
        };
        let work = selected(&operation);
        let (owner, weak) = owner();
        operation.ignore_ack_and_keep_owner(owner);
        drop(work);
        assert_state(&operation, AckState::ReadyToRun, true);
        assert!(weak.upgrade().is_some());
        assert!(matches!(
            operation.acknowledge(),
            Err(AckError::InvalidOperation)
        ));
        drop(operation);
        assert!(weak.upgrade().is_none());
    }
}

#[test]
fn dropping_running_work_does_not_complete_or_release_retained_owner() {
    for eos in [false, true] {
        let operation = if eos {
            ReceivedOperation::end_of_stream(true)
        } else {
            operation(1, true)
        };
        let mut work = selected(&operation);
        work.start().unwrap();
        let (owner, weak) = owner();
        operation.ignore_ack_and_keep_owner(owner);
        drop(work);
        assert_state(&operation, AckState::Running, false);
        assert!(weak.upgrade().is_some());
        assert!(matches!(
            operation.acknowledge(),
            Err(AckError::InvalidOperation)
        ));
        drop(operation);
        assert!(weak.upgrade().is_none());
    }
}

struct ReentrantOwner {
    operation: ReceivedOperation,
    observed: Arc<Mutex<Vec<AckState>>>,
}

impl Drop for ReentrantOwner {
    fn drop(&mut self) {
        self.observed
            .lock()
            .unwrap()
            .push(self.operation.ack_state());
    }
}

#[test]
fn real_finish_releases_owner_outside_ack_state_lock_and_breaks_owner_cycle() {
    let operation = operation(1, true);
    let mut work = selected(&operation);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let owner = Arc::new(ReentrantOwner {
        operation: operation.clone(),
        observed: observed.clone(),
    });
    let weak_owner = Arc::downgrade(&owner);
    let weak_operation = Arc::downgrade(&operation.inner);
    operation.ignore_ack_and_keep_owner(owner);
    work.start().unwrap();
    drop(operation);
    assert!(weak_owner.upgrade().is_some());
    work.finish().unwrap();
    assert_eq!(*observed.lock().unwrap(), vec![AckState::Completed]);
    assert!(weak_owner.upgrade().is_none());
    assert!(weak_operation.upgrade().is_none());
}

#[test]
fn synchronous_eos_ack_releases_reentrant_owner_outside_state_lock() {
    let eos = ReceivedOperation::end_of_stream(false);
    let observed = Arc::new(Mutex::new(Vec::new()));
    eos.ignore_ack_and_keep_owner(Arc::new(ReentrantOwner {
        operation: eos.clone(),
        observed: observed.clone(),
    }));
    assert!(eos.acknowledge().unwrap().is_none());
    assert_eq!(*observed.lock().unwrap(), vec![AckState::Completed]);
}

#[test]
fn concurrent_duplicate_acks_select_exactly_one_callback() {
    let operation = operation(1, true);
    let barrier = Arc::new(Barrier::new(16));
    let results = thread::scope(|scope| {
        let handles: Vec<_> = (0..16)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    operation.acknowledge()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    let mut selected = None;
    let mut invalid = 0;
    for result in results {
        match result {
            Ok(Some(work)) => {
                assert!(selected.replace(work).is_none());
            }
            Err(AckError::InvalidOperation) => invalid += 1,
            Ok(None) => panic!("an unretired callback-required ACK cannot disappear"),
        }
    }
    assert_eq!(invalid, 15);
    assert_state(&operation, AckState::ReadyToRun, true);
    let mut work = selected.unwrap();
    work.start().unwrap();
    work.finish().unwrap();
    assert_state(&operation, AckState::Completed, false);
}

#[test]
fn ordinary_ack_retirement_race_is_either_cancelled_or_rooted_ready_work() {
    for _ in 0..32 {
        let operation = operation(1, true);
        let barrier = Barrier::new(3);
        let (owner, weak) = owner();
        let work = thread::scope(|scope| {
            let ack = scope.spawn(|| {
                barrier.wait();
                operation.acknowledge().unwrap()
            });
            let retire = scope.spawn(|| {
                barrier.wait();
                operation.ignore_ack_and_keep_owner(owner);
            });
            barrier.wait();
            retire.join().unwrap();
            ack.join().unwrap()
        });
        if let Some(mut work) = work {
            assert_state(&operation, AckState::ReadyToRun, true);
            assert!(weak.upgrade().is_some());
            work.start().unwrap();
            work.finish().unwrap();
            assert_state(&operation, AckState::Completed, false);
        } else {
            assert_state(&operation, AckState::Cancelled, false);
            assert!(operation.acknowledge().unwrap().is_none());
        }
        assert!(weak.upgrade().is_none());
    }
}

#[test]
fn eos_ack_retirement_race_always_keeps_real_pending_callback_and_root() {
    for _ in 0..32 {
        let eos = ReceivedOperation::end_of_stream(true);
        let barrier = Barrier::new(3);
        let (owner, weak) = owner();
        let mut work = thread::scope(|scope| {
            let ack = scope.spawn(|| {
                barrier.wait();
                selected(&eos)
            });
            let retire = scope.spawn(|| {
                barrier.wait();
                eos.ignore_ack_and_keep_owner(owner);
            });
            barrier.wait();
            retire.join().unwrap();
            ack.join().unwrap()
        });
        assert_state(&eos, AckState::ReadyToRun, true);
        assert!(weak.upgrade().is_some());
        work.start().unwrap();
        work.finish().unwrap();
        assert_state(&eos, AckState::Completed, false);
        assert!(weak.upgrade().is_none());
    }
}

#[test]
fn finish_retirement_race_never_leaves_a_completed_operation_rooted() {
    for eos in [false, true] {
        for _ in 0..32 {
            let operation = if eos {
                ReceivedOperation::end_of_stream(true)
            } else {
                operation(1, true)
            };
            let mut work = selected(&operation);
            work.start().unwrap();
            let barrier = Barrier::new(3);
            let (owner, weak) = owner();
            thread::scope(|scope| {
                let finish = scope.spawn(|| {
                    barrier.wait();
                    work.finish().unwrap();
                });
                let retire = scope.spawn(|| {
                    barrier.wait();
                    operation.ignore_ack_and_keep_owner(owner);
                });
                barrier.wait();
                finish.join().unwrap();
                retire.join().unwrap();
            });
            assert_state(&operation, AckState::Completed, false);
            assert!(weak.upgrade().is_none());
        }
    }
}

#[test]
fn no_callback_ack_retirement_race_never_selects_work_or_retains_owner() {
    for eos in [false, true] {
        for _ in 0..32 {
            let operation = if eos {
                ReceivedOperation::end_of_stream(false)
            } else {
                operation(1, false)
            };
            let barrier = Barrier::new(3);
            let (owner, weak) = owner();
            thread::scope(|scope| {
                let ack = scope.spawn(|| {
                    barrier.wait();
                    assert!(operation.acknowledge().unwrap().is_none());
                });
                let retire = scope.spawn(|| {
                    barrier.wait();
                    operation.ignore_ack_and_keep_owner(owner);
                });
                barrier.wait();
                ack.join().unwrap();
                retire.join().unwrap();
            });
            assert!(!operation.queue_operation().needs_ack());
            if eos {
                assert_eq!(operation.ack_state(), AckState::Completed);
            } else {
                assert!(matches!(
                    operation.ack_state(),
                    AckState::Completed | AckState::Cancelled
                ));
            }
            assert!(weak.upgrade().is_none());
        }
    }
}

#[test]
fn actual_queue_completion_stops_at_ready_work_and_only_running_unblocks_it() {
    let mut queue = queue(true, false);
    let first = operation(1, true);
    let second = operation(2, true);
    let third = operation(3, true);
    for operation in [&first, &second, &third] {
        queue
            .try_enqueue(operation.queue_operation().clone())
            .unwrap();
    }
    assert!(queue.commit());
    assert!(!queue.complete());
    let mut first_work = selected(&first);
    let mut second_work = selected(&second);
    let mut third_work = selected(&third);
    assert!(!queue.complete());
    first_work.start().unwrap();
    assert!(queue.complete());
    assert_eq!(queue.progress().complete_head, 2);
    assert_eq!(queue.operation_count(), 2);
    assert_state(&first, AckState::Running, false);
    assert_state(&second, AckState::ReadyToRun, true);
    first_work.finish().unwrap();
    third_work.start().unwrap();
    assert!(!queue.complete());
    assert_eq!(queue.progress().complete_head, 2);
    second_work.start().unwrap();
    assert!(queue.complete());
    assert_eq!(queue.progress().complete_head, 4);
    assert_eq!(queue.operation_count(), 0);
    assert_state(&second, AckState::Running, false);
    assert_state(&third, AckState::Running, false);
    second_work.finish().unwrap();
    third_work.finish().unwrap();
}

#[test]
fn waiting_for_receiver_serialization_keeps_ready_operation_blocking_queue() {
    let operation = operation(1, true);
    let mut queue = queue(true, false);
    queue
        .try_enqueue(operation.queue_operation().clone())
        .unwrap();
    assert!(queue.commit());
    let queue = Mutex::new(queue);
    let (ready_sender, ready_receiver) = mpsc::channel();
    let (owner, weak) = owner();
    thread::scope(|scope| {
        let mut receiver_guard = queue.lock().unwrap();
        let worker = scope.spawn(|| {
            let mut work = selected(&operation);
            ready_sender.send(()).unwrap();
            {
                let mut receiver_guard = queue.lock().unwrap();
                work.start().unwrap();
                assert!(receiver_guard.complete());
                assert_eq!(receiver_guard.progress().complete_head, 2);
            }
            work.finish().unwrap();
        });
        ready_receiver.recv().unwrap();
        assert_state(&operation, AckState::ReadyToRun, true);
        assert!(!receiver_guard.complete());
        operation.ignore_ack_and_keep_owner(owner);
        assert!(weak.upgrade().is_some());
        drop(receiver_guard);
        worker.join().unwrap();
    });
    assert_state(&operation, AckState::Completed, false);
    assert!(weak.upgrade().is_none());
}

#[test]
fn ordinary_cancellation_unblocks_actual_queue_but_late_ack_has_no_work() {
    let operation = operation(1, true);
    let mut queue = queue(true, false);
    queue
        .try_enqueue(operation.queue_operation().clone())
        .unwrap();
    assert!(queue.commit());
    assert!(!queue.complete());
    operation.ignore_ack_and_keep_owner(Arc::new(()));
    assert!(queue.complete());
    assert_eq!(queue.progress().complete_head, 2);
    assert!(operation.acknowledge().unwrap().is_none());
    assert_state(&operation, AckState::Cancelled, false);
}

#[test]
fn optimistic_queue_completes_before_service_ack_without_rewriting_ack_state() {
    let operation = operation(1, false);
    let mut queue = queue(false, true);
    queue
        .try_enqueue(operation.queue_operation().clone())
        .unwrap();
    assert!(queue.complete());
    assert_eq!(queue.progress().complete_head, 2);
    assert_eq!(queue.progress().commit_head, 2);
    assert_eq!(queue.operation_count(), 0);
    assert_state(&operation, AckState::NotCalled, true);
    assert!(operation.acknowledge().unwrap().is_none());
    assert_state(&operation, AckState::Completed, false);
}
