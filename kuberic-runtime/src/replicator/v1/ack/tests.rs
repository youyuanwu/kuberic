// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
// Production-source-derived AckSender scenarios; no isolated native test
// counterpart is claimed. See sf-v1-port-provenance.md.

use std::sync::Mutex;

use super::*;

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn sender(batch: u64) -> (AckSender, Arc<Mutex<Vec<bool>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let callback_calls = calls.clone();
    let mut sender = AckSender::new(ms(batch), ms(100), 2, ms(20));
    sender.open(
        Arc::new(move |trace| {
            callback_calls.lock().unwrap().push(trace);
            true
        }),
        ms(0),
    );
    (sender, calls)
}

#[test]
fn repeated_requests_keep_first_deadline_and_issue_latest_owner_state() {
    let state = Arc::new(Mutex::new(1));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let (state_copy, observations) = (state.clone(), observed.clone());
    let mut sender = AckSender::new(ms(10), ms(100), 2, ms(0));
    sender.open(
        Arc::new(move |_| {
            observations
                .lock()
                .unwrap()
                .push(*state_copy.lock().unwrap());
            true
        }),
        ms(0),
    );
    assert!(sender.schedule(false, ms(1)).is_none());
    *state.lock().unwrap() = 7;
    assert!(sender.schedule(false, ms(8)).is_none());
    assert_eq!(sender.deadline(), Some(ms(11)));
    let work = sender.on_timer(ms(11)).unwrap();
    *state.lock().unwrap() = 9;
    work.dispatch();
    assert_eq!(*observed.lock().unwrap(), [9]);
    assert_eq!(sender.last_issued(), Some(ms(11)));
    assert_eq!(sender.deadline(), None);
}

#[test]
fn force_cancels_pending_deadline_and_also_sends_without_pending_batch() {
    let (mut sender, calls) = sender(10);
    sender.schedule(false, ms(1));
    sender.schedule(true, ms(2)).unwrap().dispatch();
    sender.schedule(true, ms(3)).unwrap().dispatch();
    assert_eq!(sender.deadline(), None);
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert!(sender.close(ms(4)).is_none());
}

#[test]
fn close_flushes_pending_once_with_trace_true_not_a_wire_final_bit() {
    let (mut sender, calls) = sender(10);
    sender.schedule(false, ms(1));
    let final_work = sender.close(ms(2)).unwrap();
    assert_eq!(sender.last_issued(), Some(ms(2)));
    assert!(sender.close(ms(3)).is_none());
    assert!(sender.on_timer(ms(11)).is_none());
    assert!(sender.schedule(true, ms(12)).is_none());
    final_work.dispatch();
    assert_eq!(*calls.lock().unwrap(), [true]);
}

#[test]
fn close_without_pending_batch_does_not_emit_unconditional_final_ack() {
    let (mut sender, calls) = sender(10);
    assert!(sender.close(ms(1)).is_none());
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn zero_batch_sends_every_request_and_has_no_final_flush_obligation() {
    let (mut sender, calls) = sender(0);
    sender.schedule(false, ms(1)).unwrap().dispatch();
    sender.schedule(false, ms(2)).unwrap().dispatch();
    sender.schedule(true, ms(3)).unwrap().dispatch();
    assert_eq!(calls.lock().unwrap().len(), 3);
    assert_eq!(sender.deadline(), None);
    assert!(!sender.force_on_pending_limit());
    assert!(sender.close(ms(4)).is_none());
}

#[test]
fn force_policy_gate_is_construction_time_configuration_not_pending_counter() {
    let (sender, _) = sender(10);
    assert!(sender.force_on_pending_limit());
    let disabled = AckSender::new(ms(10), ms(100), 0, ms(0));
    assert!(!disabled.force_on_pending_limit());
}

#[test]
fn callback_false_does_not_requeue_or_change_issuance_time() {
    let mut sender = AckSender::new(ms(10), ms(100), 2, ms(0));
    sender.open(Arc::new(|_| false), ms(0));
    sender.schedule(false, ms(1));
    sender.on_timer(ms(11)).unwrap().dispatch();
    assert_eq!(sender.last_issued(), Some(ms(11)));
    assert_eq!(sender.deadline(), None);
    assert!(sender.close(ms(12)).is_none());
}

#[test]
fn timer_dispatched_before_force_can_still_enter_while_active() {
    let (mut sender, calls) = sender(10);
    sender.schedule(false, ms(0));
    // Runtime has dispatched the due callback, but it has not entered the core.
    sender.schedule(true, ms(10)).unwrap().dispatch();
    assert_eq!(sender.deadline(), None);
    sender.on_timer(ms(10)).unwrap().dispatch();
    assert_eq!(calls.lock().unwrap().len(), 2);
}

#[test]
fn selected_callback_owns_receiver_after_close_and_scheduler_drop() {
    let (mut sender, calls) = sender(10);
    let weak = Arc::downgrade(&calls);
    let selected = sender.schedule(true, ms(1)).unwrap();
    sender.close(ms(2));
    drop(sender);
    drop(calls);
    assert!(weak.upgrade().is_some());
    selected.dispatch();
    assert!(weak.upgrade().is_none());
}

#[test]
fn trace_uses_strict_boundary_and_coalescing_can_consume_it_without_send() {
    let (mut sender, calls) = sender(10);
    sender.schedule(false, ms(20));
    sender.schedule(false, ms(21)); // consumes trace, emits nothing
    sender.on_timer(ms(30)).unwrap().dispatch();
    sender.schedule(true, ms(41)).unwrap().dispatch(); // exactly 20: false
    sender.schedule(true, ms(42)).unwrap().dispatch();
    assert_eq!(*calls.lock().unwrap(), [false, false, true]);
}

#[test]
fn zero_trace_interval_does_not_reset_elapsed_stopwatch() {
    let (mut sender, calls) = sender(10);
    sender.set_trace_interval(ms(0));
    sender.schedule(true, ms(50)).unwrap().dispatch();
    sender.set_trace_interval(ms(20));
    sender.schedule(true, ms(51)).unwrap().dispatch();
    assert_eq!(*calls.lock().unwrap(), [false, true]);
}

#[test]
fn reopen_preserves_running_trace_stopwatch_instead_of_restarting_it() {
    let (mut sender, calls) = sender(10);
    sender.close(ms(10));
    let recorded = calls.clone();
    sender.open(
        Arc::new(move |trace| {
            recorded.lock().unwrap().push(trace);
            true
        }),
        ms(30),
    );
    sender.schedule(true, ms(30)).unwrap().dispatch();
    assert_eq!(*calls.lock().unwrap(), [true]);
}

#[test]
#[should_panic]
fn batching_must_be_shorter_than_retry() {
    let mut sender = AckSender::new(ms(100), ms(100), 2, ms(0));
    sender.open(Arc::new(|_| true), ms(0));
}
