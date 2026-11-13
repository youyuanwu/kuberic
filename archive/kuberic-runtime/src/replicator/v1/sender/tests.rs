// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Selected ancestry: ReliableOperationSender.Test.cpp's receive/add/window/
// latency cases and DecayAverage.Test.cpp, at the pin in ../sender.rs.
// Additional boundary/replay/close cases derive from production source.

use bytes::Bytes;

use super::*;
use crate::replicator::v1::operation_queue::{OperationQueue, QueueConfig};

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn config(window: usize, max_window: usize) -> SenderConfig {
    SenderConfig {
        initial_window: window,
        max_window,
        retry: ms(10),
        decay_factor: 0.0,
        decay_interval: ms(100),
    }
}

fn sender(window: usize, max_window: usize) -> ReliableOperationSender {
    let mut sender = ReliableOperationSender::new(config(window, max_window), 0, ms(0));
    sender.open();
    sender
}

fn queue(first: Lsn, last: Lsn) -> OperationQueue {
    let mut queue = OperationQueue::new(
        QueueConfig {
            initial_capacity: 8,
            max_capacity: 0,
            max_bytes: 1_000_000,
            max_completed_count: 0,
            max_completed_bytes: 0,
            clean_on_complete: true,
        },
        first,
        false,
        true,
    );
    for lsn in first..=last {
        queue
            .try_enqueue(QueueOperation::new(lsn, Bytes::from_static(b"entry"), true))
            .unwrap();
    }
    queue
}

fn operations(first: Lsn, last: Lsn) -> Vec<SenderOperation> {
    let queue = queue(first, last);
    let mut entries = Vec::new();
    assert!(queue.get_operations(first, &mut entries));
    entries
        .into_iter()
        .map(|operation| SenderOperation {
            operation,
            original_epoch: Epoch::new(1, 2),
        })
        .collect()
}

fn op(lsn: Lsn) -> SenderOperation {
    operations(lsn, lsn).pop().unwrap()
}

fn selected(batch: SendBatch) -> Vec<Lsn> {
    let mut result = Vec::new();
    batch.dispatch(|action, _| {
        result.push(match action {
            SendAction::Operation(entry) => entry.operation.lsn(),
            SendAction::RequestAck => -1,
        });
        true
    });
    result
}

fn pending(sender: &ReliableOperationSender) -> Vec<Lsn> {
    sender.pending.keys().copied().collect()
}

#[test]
fn receive_ack_retires_sends_while_apply_ack_is_independent() {
    let mut sender = sender(8, 8);
    assert_eq!(
        selected(sender.add_replay(operations(1, 3), 0, ms(0))),
        [1, 2, 3]
    );
    for (receive, apply, expected) in [
        (1, 0, vec![2, 3]),
        (2, 0, vec![3]),
        (2, 1, vec![3]),
        (3, 2, vec![]),
    ] {
        assert!(sender.on_ack(receive, apply, ms(1)).advanced);
        assert_eq!(pending(&sender), expected);
        assert!(sender.deadline().is_some());
    }
    assert!(sender.on_ack(3, 3, ms(2)).advanced);
    assert_eq!(sender.deadline(), None);
}

#[test]
fn request_ack_is_timer_only_and_retains_completed_hint() {
    let mut sender = sender(4, 4);
    sender.add_replay(operations(1, 3), 9, ms(0));
    assert!(sender.on_ack(3, 0, ms(1)).sends.is_empty());
    assert_eq!(sender.deadline(), Some(ms(11)));
    let mut seen = Vec::new();
    sender.on_timer(ms(11)).dispatch(|action, hint| {
        assert!(matches!(action, SendAction::RequestAck));
        seen.push(hint);
        false
    });
    assert_eq!(seen, [9]);
    assert_eq!(sender.deadline(), Some(ms(21)));
    assert!(sender.on_ack(3, 0, ms(12)).sends.is_empty());
    assert_eq!(sender.deadline(), Some(ms(22)));
    sender.on_ack(3, 3, ms(13));
    assert_eq!(sender.deadline(), None);
}

#[test]
fn first_timer_does_not_shrink_and_later_no_progress_timers_halve_to_one() {
    let mut sender = sender(8, 8);
    sender.add_replay(operations(1, 8), 0, ms(0));
    assert_eq!(selected(sender.on_timer(ms(10))).len(), 8);
    assert_eq!(sender.window(), 8);
    assert_eq!(selected(sender.on_timer(ms(20))).len(), 4);
    assert_eq!(sender.window(), 4);
    sender.on_timer(ms(30));
    assert_eq!(sender.window(), 2);
    sender.on_timer(ms(40));
    sender.on_timer(ms(50));
    assert_eq!(sender.window(), 1);
}

#[test]
fn apply_progress_resets_halving_but_duplicate_ack_does_not() {
    let mut sender = sender(4, 16);
    sender.add(op(1), 0, ms(0));
    sender.on_ack(1, 0, ms(1));
    assert_eq!(sender.window(), 8);
    sender.on_timer(ms(11));
    sender.on_timer(ms(21));
    assert_eq!(sender.window(), 4);
    sender.on_ack(1, 0, ms(22));
    sender.on_timer(ms(32));
    assert_eq!(sender.window(), 2);
    sender.on_ack(1, 1, ms(33));
    assert_eq!(sender.window(), 4);
    sender.add(op(2), 0, ms(34));
    sender.on_timer(ms(44));
    assert_eq!(sender.window(), 4);
}

#[test]
fn default_window_maximum_allows_growth_from_eight() {
    let mut sender = sender(8, 0);
    sender.add(op(1), 0, ms(0));
    sender.on_ack(1, 1, ms(1));
    assert_eq!(sender.window(), 16);
    assert_eq!(sender.max_window, 1024);
}

#[test]
fn large_initial_window_uses_four_times_initial_fallback() {
    let mut sender = sender(2048, 0);
    for lsn in 1..=6 {
        sender.add(op(lsn), 0, ms(lsn as u64));
        sender.on_ack(lsn, lsn, ms(lsn as u64));
    }
    assert_eq!(sender.window(), 8192);
}

#[test]
fn duplicate_ack_does_not_repeatedly_grow_window() {
    let mut sender = sender(8, 0);
    sender.add(op(1), 0, ms(0));
    sender.on_ack(1, 1, ms(1));
    for _ in 0..6 {
        assert!(!sender.on_ack(1, 1, ms(2)).advanced);
    }
    assert_eq!(sender.window(), 16);
}

#[test]
fn configured_maximum_tests_old_window_without_extra_clamp() {
    let mut sender = sender(4, 6);
    sender.add(op(1), 0, ms(0));
    sender.on_ack(1, 1, ms(1));
    assert_eq!(sender.window(), 8);
}

#[test]
fn sparse_single_adds_are_ordered_and_window_counts_entries_not_positions() {
    let mut sender = sender(2, 2);
    assert_eq!(selected(sender.add(op(50), 0, ms(0))), [50]);
    assert_eq!(selected(sender.add(op(2), 0, ms(0))), [2]);
    assert!(sender.add(op(20), 0, ms(0)).is_empty());
    assert_eq!(pending(&sender), [2, 20, 50]);
    assert_eq!(sender.progress(0).not_received, 50);
    assert_eq!(selected(sender.on_timer(ms(10))), [2, 20]);
}

#[test]
fn never_selected_operation_is_eligible_before_first_retry_deadline() {
    let mut sender = sender(2, 2);
    sender.add_replay(operations(1, 3), 0, ms(0));
    assert_eq!(sender.pending[&3].selected_at, None);
    assert_eq!(selected(sender.on_ack(2, 0, ms(1)).sends), [3]);
    assert_eq!(sender.pending[&3].selected_at, Some(ms(1)));
}

#[test]
fn retry_is_inclusive_and_skips_recent_entries_without_stopping_scan() {
    let mut sender = sender(4, 4);
    sender.add(op(1), 0, ms(0));
    sender.add(op(2), 0, ms(2));
    assert!(sender.on_ack(0, 0, ms(9)).sends.is_empty());
    assert_eq!(selected(sender.on_ack(0, 0, ms(10)).sends), [1]);
    // LSN1 is now recent, LSN2 is eligible and must still be examined.
    assert_eq!(selected(sender.on_ack(0, 0, ms(12)).sends), [2]);
    assert_eq!(sender.deadline(), Some(ms(22)));
}

#[test]
fn adding_more_work_does_not_postpone_armed_retry() {
    let mut sender = sender(4, 4);
    sender.add(op(1), 0, ms(0));
    sender.add(op(2), 0, ms(8));
    assert_eq!(sender.deadline(), Some(ms(10)));
}

#[test]
fn backpressure_stops_batch_but_all_selected_timestamps_remain_advanced() {
    let mut sender = sender(4, 4);
    let batch = sender.add_replay(operations(1, 3), 0, ms(0));
    let mut attempted = Vec::new();
    batch.dispatch(|action, _| {
        let SendAction::Operation(entry) = action else {
            panic!()
        };
        attempted.push(entry.operation.lsn());
        false
    });
    assert_eq!(attempted, [1]);
    assert!(sender.on_ack(0, 0, ms(9)).sends.is_empty());
    assert_eq!(selected(sender.on_ack(0, 0, ms(10)).sends), [1, 2, 3]);
}

#[test]
fn replay_merges_real_queue_range_without_replacing_duplicates() {
    let mut sender = sender(4, 4);
    let original = op(2);
    sender.add(original.clone(), 0, ms(0));
    let range = operations(1, 4);
    assert!(!Arc::ptr_eq(&original.operation, &range[1].operation));
    assert_eq!(selected(sender.add_replay(range, 0, ms(1))), [1, 3, 4]);
    assert!(Arc::ptr_eq(
        &original.operation,
        &sender.pending[&2].operation.operation
    ));
    assert_eq!(sender.pending[&2].selected_at, Some(ms(0)));
    assert_eq!(selected(sender.on_ack(0, 0, ms(10)).sends), [2]);
}

#[test]
fn duplicate_single_add_preserves_original_epoch_and_identity() {
    let mut sender = sender(4, 4);
    let entry = op(1);
    sender.add(entry.clone(), 0, ms(0));
    let mut replacement = op(1);
    replacement.original_epoch = Epoch::new(9, 9);
    assert!(sender.add(replacement, 7, ms(1)).is_empty());
    assert_eq!(
        sender.pending[&1].operation.original_epoch,
        Epoch::new(1, 2)
    );
    assert!(Arc::ptr_eq(
        &entry.operation,
        &sender.pending[&1].operation.operation
    ));
    assert_eq!(sender.completed_hint, 7);
}

#[test]
fn empty_replay_does_not_update_hint_but_all_duplicate_replay_does() {
    let mut sender = sender(4, 4);
    sender.add(op(1), 8, ms(0));
    sender.add_replay(vec![], 99, ms(1));
    assert_eq!(sender.completed_hint, 8);
    sender.add_replay(operations(1, 1), 3, ms(2));
    assert_eq!(sender.completed_hint, 3);
    sender.progress(0);
    assert_eq!(sender.completed_hint, 3);
    sender.progress(2);
    assert_eq!(sender.completed_hint, 2);
}

#[test]
fn immediate_hint_is_call_argument_not_stored_nonzero_hint() {
    let mut sender = sender(4, 4);
    sender.progress(19);
    let mut hints = Vec::new();
    sender.add(op(1), 0, ms(0)).dispatch(|_, hint| {
        hints.push(hint);
        true
    });
    sender.on_timer(ms(10)).dispatch(|_, hint| {
        hints.push(hint);
        true
    });
    assert_eq!(hints, [0, 19]);
}

#[test]
fn older_ack_updates_last_processed_time_but_not_high_water_marks() {
    let mut sender = sender(4, 4);
    sender.add_replay(operations(1, 8), 0, ms(0));
    sender.on_ack(6, 2, ms(1));
    assert!(sender.on_ack(5, 3, ms(2)).advanced);
    assert!(!sender.on_ack(4, 1, ms(3)).advanced);
    assert_eq!(
        sender.progress(0),
        SenderProgress {
            received: 6,
            applied: 3,
            last_ack_at: Some(ms(3)),
            not_received: 2,
            received_not_applied: 3,
        }
    );
}

#[test]
fn close_releases_pending_but_selected_work_outlives_owner_queue_and_sender() {
    let mut queue = queue(1, 1);
    let entry = queue.get_operation(1).unwrap().clone();
    let weak = Arc::downgrade(&entry);
    let mut sender = sender(2, 2);
    let batch = sender.add(
        SenderOperation {
            operation: entry,
            original_epoch: Epoch::new(1, 2),
        },
        0,
        ms(0),
    );
    sender.close();
    queue.reset(1);
    drop(queue);
    drop(sender);
    assert!(weak.upgrade().is_some());
    batch.dispatch(|action, _| {
        let SendAction::Operation(entry) = action else {
            panic!()
        };
        assert_eq!(entry.operation.data(), &Bytes::from_static(b"entry"));
        assert!(entry.operation.needs_ack()); // No fake local service ACK.
        true
    });
    assert!(weak.upgrade().is_none());
}

#[test]
fn inactive_entries_do_nothing_and_reopen_does_not_reset_progress() {
    let mut sender = sender(4, 8);
    sender.add(op(1), 0, ms(0));
    sender.on_ack(1, 1, ms(1));
    sender.close();
    sender.close();
    assert!(!sender.is_active());
    assert!(sender.add(op(2), 9, ms(2)).is_empty());
    assert!(sender.on_timer(ms(10)).is_empty());
    assert!(!sender.on_ack(2, 2, ms(11)).advanced);
    sender.open();
    assert_eq!(sender.progress(0).applied, 1);
    assert_eq!(sender.window(), 8);
    assert_eq!(selected(sender.add(op(2), 0, ms(12))), [2]);
}

#[test]
#[should_panic]
fn invalid_ack_pair_asserts_even_when_inactive() {
    let mut sender = sender(4, 4);
    sender.close();
    sender.on_ack(1, 2, ms(1));
}

#[test]
fn latency_watches_separate_receive_apply_and_retries_do_not_restart_them() {
    let mut sender = sender(4, 4);
    sender.add_replay(operations(1, 4), 0, ms(0));
    sender.on_ack(2, 1, ms(2));
    assert_eq!(sender.latencies.items.len(), 4);
    assert_eq!(sender.latencies.items[&1].applied.stopped, Some(ms(2)));
    assert_eq!(sender.latencies.items[&2].received.stopped, Some(ms(2)));
    assert_eq!(sender.latencies.items[&2].applied.stopped, None);
    sender.on_timer(ms(10)); // strict > retry: no collection yet
    assert_eq!(sender.averages(), (ms(0), ms(0)));
    sender.on_timer(ms(11));
    assert_eq!(sender.latencies.items.len(), 3);
    assert_eq!(sender.averages(), (ms(6), ms(8)));
    assert_eq!(sender.latencies.items[&3].received.started, ms(0));
}

#[test]
fn pending_apply_updates_only_apply_average_and_full_completion_only_collects() {
    let mut sender = sender(2, 2);
    sender.add_replay(operations(1, 2), 0, ms(0));
    sender.on_ack(2, 0, ms(2));
    sender.on_timer(ms(11));
    assert_eq!(sender.averages(), (ms(0), ms(11)));
    sender.on_ack(2, 2, ms(22));
    assert_eq!(sender.averages(), (ms(0), ms(11)));
    assert!(sender.latencies.items.is_empty());
}

#[test]
fn window_blocked_latency_starts_at_admission_and_reset_keeps_live_watches() {
    let mut sender = sender(1, 1);
    sender.add_replay(operations(1, 2), 0, ms(0));
    assert_eq!(sender.pending[&2].selected_at, None);
    sender.on_timer(ms(11));
    assert_eq!(sender.averages(), (ms(11), ms(11)));
    sender.reset_average_statistics(ms(12));
    assert_eq!(sender.averages(), (ms(0), ms(0)));
    assert_eq!(sender.latencies.items.len(), 2);
    sender.on_timer(ms(23));
    assert_eq!(sender.averages(), (ms(23), ms(23)));
}

#[test]
fn dynamic_decay_settings_preserve_aggregate_and_factor_one_averages_samples() {
    let mut sender = sender(1, 1);
    sender.add(op(1), 0, ms(0));
    sender.on_timer(ms(11));
    sender.set_decay_settings(1.0, ms(100));
    sender.on_timer(ms(23));
    assert_eq!(sender.averages(), (ms(17), ms(17)));
    sender.set_decay_settings(0.0, ms(100));
    // Native refreshes dynamic settings at selection, not at the setting write.
    assert_eq!(sender.averages(), (ms(17), ms(17)));
    sender.on_ack(0, 0, ms(24));
    assert_eq!(sender.averages(), (ms(23), ms(23)));
}

#[test]
fn decay_average_latest_simple_average_saturation_and_reset() {
    let mut average = DecayAverage::new(0.0, ms(100));
    average.update(15_000, ms(0)); // 1.5ms observation truncates to 1ms
    assert_eq!(average.value(), ms(1));
    average.update((u32::MAX as i128 + 10) * 10_000, ms(1));
    assert_eq!(average.value(), ms(u32::MAX as u64));
    average.reset(ms(2));
    average.settings(1.0, ms(100));
    average.update(10 * 10_000, ms(3));
    average.update(20 * 10_000, ms(4));
    assert_eq!(average.value(), ms(15));
}

#[test]
fn decay_average_ages_prior_observations_and_replaces_at_coefficient_boundary() {
    let mut average = DecayAverage::new(0.5, ms(100));
    average.update(10 * 10_000, ms(0));
    average.update(40 * 10_000, ms(100));
    assert_eq!(average.value(), ms(30));
    average.settings(0.001, ms(100));
    average.update(60 * 10_000, ms(200)); // coefficient == .001 replaces
    assert_eq!(average.value(), ms(60));
}

#[test]
fn latency_mean_uses_signed_tick_division_at_each_iteration() {
    let mut sender = sender(4, 4);
    sender.add(op(1), 0, Duration::ZERO);
    sender.add(op(2), 0, Duration::from_nanos(100));
    sender.add(op(3), 0, Duration::from_nanos(300));
    let mean = sender.latencies.compute(Duration::from_nanos(500));
    assert_eq!(mean, (4, 4)); // observations 5,4,2 ticks; sequential truncation
}
