// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
// Production-derived RemoteSession/ReplicationSession component scenarios.
// These handoff tests do NOT execute CopySender, ReplicaManager or a carrier.

use std::sync::Barrier;

use bytes::Bytes;

use super::*;

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn config() -> SenderConfig {
    SenderConfig {
        initial_window: 1,
        max_window: 1,
        retry: ms(10),
        decay_factor: 0.0,
        decay_interval: ms(100),
    }
}

fn identity() -> SessionIdentity {
    SessionIdentity {
        primary_endpoint: "primary/actor-1".into(),
        remote_endpoint: "secondary/actor-2".into(),
        remote_incarnation: 7,
    }
}

fn session(send: SendCallback) -> ReplicationSession {
    let session = ReplicationSession::new(identity(), Epoch::new(1, 2), 0, config(), send, ms(0));
    session.open();
    session
}

fn op(lsn: Lsn) -> SenderOperation {
    SenderOperation {
        operation: QueueOperation::new(lsn, Bytes::from_static(b"session-entry"), true),
        original_epoch: Epoch::new(0, 9),
    }
}

fn ack(received: Lsn, applied: Lsn, copy_received: Lsn, copy_applied: Lsn) -> AckUpdate {
    AckUpdate {
        replication_received: received,
        replication_applied: applied,
        copy_received,
        copy_applied,
        process_copy: true,
        message_id: format!("ack-{received}-{applied}-{copy_received}-{copy_applied}"),
    }
}

fn replication_ack(received: Lsn, applied: Lsn) -> AckUpdate {
    let mut update = ack(received, applied, 0, 0);
    update.process_copy = false;
    update
}

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Sends,
    Gate(Lsn, Lsn),
    Primary,
    Copy(Lsn, Lsn),
}

fn deliver(action: AckAction, events: &mut Vec<Event>) {
    match action {
        AckAction::ReplicationSends(batch) => {
            events.push(Event::Sends);
            batch.dispatch();
        }
        AckAction::CopyReplicationGate {
            copy_applied,
            replication_applied,
        } => {
            events.push(Event::Gate(copy_applied, replication_applied));
        }
        AckAction::PrimaryProgress(notification) => {
            events.push(Event::Primary);
            notification.dispatch();
        }
        AckAction::CopyAck { received, applied } => events.push(Event::Copy(received, applied)),
    }
}

fn drive(mut processor: AckProcessor, now: Duration) -> Vec<Event> {
    let mut events = Vec::new();
    while let Some(handoff) = processor.next(now) {
        processor = handoff.deliver(|action| deliver(action, &mut events));
    }
    events
}

#[test]
fn pass_closes_start_copy_then_dispatches_replication_before_copy_gate_primary_copy() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sent = log.clone();
    let session = session(Arc::new(move |intent| {
        if let SendKind::Replication { operation, .. } = intent.kind {
            sent.lock()
                .unwrap()
                .push(format!("send-{}", operation.lsn()));
        }
        true
    }));
    session.add_replay(vec![op(1), op(2)], 0, ms(0)).dispatch();
    log.lock().unwrap().clear();
    let mut start_copy = ReliableOperationSender::new(config(), 0, ms(0));
    start_copy.open();
    session.install_start_copy_sender(start_copy);
    let notified = log.clone();
    let processor = session
        .update_ack(
            ack(1, 0, 5, 3),
            Some(Arc::new(move || {
                notified.lock().unwrap().push("primary".into());
            })),
        )
        .unwrap();
    assert!(
        session
            .state
            .lock()
            .unwrap()
            .start_copy
            .as_ref()
            .unwrap()
            .is_active()
    );
    let handoff = processor.next(ms(1)).unwrap();
    assert!(
        !session
            .state
            .lock()
            .unwrap()
            .start_copy
            .as_ref()
            .unwrap()
            .is_active()
    );
    let mut events = Vec::new();
    let processor = handoff.deliver(|action| deliver(action, &mut events));
    assert_eq!(*log.lock().unwrap(), ["send-2"]);
    events.extend(drive(processor, ms(1)));
    assert_eq!(
        events,
        [
            Event::Sends,
            Event::Gate(3, 0),
            Event::Primary,
            Event::Copy(5, 3)
        ]
    );
    assert_eq!(*log.lock().unwrap(), ["send-2", "primary"]);
    assert!(!session.is_processing());
}

#[test]
fn arrival_during_handoff_coalesces_and_retains_initiating_callback() {
    let session = session(Arc::new(|_| true));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let first = calls.clone();
    let processor = session
        .update_ack(
            ack(1, 0, 1, 0),
            Some(Arc::new(move || {
                first.lock().unwrap().push("first");
            })),
        )
        .unwrap();
    let processor = processor.next(ms(1)).unwrap().deliver(|action| {
        let AckAction::ReplicationSends(batch) = action else {
            panic!()
        };
        batch.dispatch();
    });
    let gate = processor.next(ms(1)).unwrap();
    assert!(session.is_processing());
    let later = calls.clone();
    assert!(
        session
            .update_ack(
                ack(2, 1, -1, -1),
                Some(Arc::new(move || {
                    later.lock().unwrap().push("later");
                }))
            )
            .is_none()
    );
    let processor = gate.deliver(|action| {
        assert!(matches!(
            action,
            AckAction::CopyReplicationGate {
                replication_applied: 0,
                ..
            }
        ));
    });
    let events = drive(processor, ms(2));
    assert_eq!(
        events,
        [
            Event::Primary,
            Event::Copy(1, 0),
            Event::Sends,
            Event::Gate(-1, 1),
            Event::Primary,
            Event::Copy(-1, -1),
        ]
    );
    assert_eq!(*calls.lock().unwrap(), ["first", "first"]);
    assert!(!session.is_processing());
}

#[test]
fn cached_replication_pair_can_regress_one_field_but_sender_never_does() {
    let session = session(Arc::new(|_| true));
    let processor = session.update_ack(ack(2, 1, 0, 0), None).unwrap();
    let processor = processor.next(ms(1)).unwrap().deliver(|action| {
        let AckAction::ReplicationSends(batch) = action else {
            panic!()
        };
        batch.dispatch();
    });
    assert!(session.update_ack(ack(3, 0, 0, 0), None).is_none());
    assert_eq!(session.state.lock().unwrap().cached.replication_applied, 0);
    let events = drive(processor, ms(2));
    assert!(events.contains(&Event::Gate(0, 1)));
    assert_eq!((session.receive_lsn(0), session.apply_lsn(0)), (3, 1));
}

#[test]
fn copy_terminal_markers_are_independent_and_absorbing() {
    let session = session(Arc::new(|_| true));
    drive(session.update_ack(ack(0, 0, -1, 2), None).unwrap(), ms(1));
    let events = drive(session.update_ack(ack(0, 0, 9, -1), None).unwrap(), ms(2));
    assert_eq!(
        events,
        [Event::Sends, Event::Gate(-1, 0), Event::Copy(-1, -1)]
    );
    assert!(session.update_ack(ack(0, 0, 20, 20), None).is_none());
    assert_eq!(session.state.lock().unwrap().cached.copy_received, -1);
}

#[test]
fn replication_only_duplicate_changes_cached_flag_not_current_pass_or_message_id() {
    let session = session(Arc::new(|_| true));
    let update = ack(1, 0, 2, 1);
    let message = update.message_id.clone();
    let processor = session.update_ack(update, None).unwrap();
    assert!(session.update_ack(replication_ack(1, 0), None).is_none());
    let state = session.state.lock().unwrap();
    assert!(!state.cached.process_copy);
    assert_eq!(state.latest_message_id, message);
    drop(state);
    let events = drive(processor, ms(1));
    assert_eq!(events, [Event::Sends, Event::Gate(1, 0), Event::Copy(2, 1)]);
}

#[test]
fn coalesced_replication_only_update_disables_copy_in_later_pass_only() {
    let session = session(Arc::new(|_| true));
    let processor = session.update_ack(ack(1, 0, 2, 1), None).unwrap();
    assert!(session.update_ack(replication_ack(2, 1), None).is_none());
    assert_eq!(
        drive(processor, ms(1)),
        [
            Event::Sends,
            Event::Gate(1, 0),
            Event::Copy(2, 1),
            Event::Sends,
            Event::Gate(1, 1),
        ]
    );
}

#[test]
fn copy_only_progress_does_not_invoke_primary_but_still_hands_off_copy_gate() {
    let session = session(Arc::new(|_| true));
    let callback: ProgressCallback = Arc::new(|| panic!("copy-only is not primary progress"));
    assert_eq!(
        drive(
            session.update_ack(ack(0, 0, 2, 1), Some(callback)).unwrap(),
            ms(1)
        ),
        [Event::Sends, Event::Gate(1, 0), Event::Copy(2, 1),]
    );
}

#[test]
fn close_does_not_drain_selected_processor_or_reject_post_close_ack_input() {
    let session = session(Arc::new(|_| panic!("closed sender selected new send")));
    let processor = session.update_ack(ack(1, 0, 1, 0), None).unwrap();
    session.close();
    assert!(session.is_processing());
    drive(processor, ms(1));
    assert_eq!(session.progress(0).received, 0); // closed sender did not advance
    let events = drive(session.update_ack(ack(2, 1, 2, 1), None).unwrap(), ms(2));
    assert_eq!(events, [Event::Sends, Event::Gate(1, 0), Event::Copy(2, 1)]);
    session.add(op(3), 0, ms(3)).dispatch();
    session.on_timer(ms(20)).dispatch();
}

#[test]
fn selected_batch_retains_session_and_operation_after_close_and_handle_drop() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let recorded = log.clone();
    let session = session(Arc::new(move |intent| {
        let SendKind::Replication { operation, .. } = intent.kind else {
            panic!()
        };
        recorded.lock().unwrap().push(operation.data().clone());
        true
    }));
    let entry = op(1);
    let weak_entry = Arc::downgrade(&entry.operation);
    let weak_session = Arc::downgrade(&session.state);
    let batch = session.add(entry, 0, ms(0));
    session.close();
    drop(session);
    assert!(weak_session.upgrade().is_some());
    assert!(weak_entry.upgrade().is_some());
    batch.dispatch();
    assert_eq!(*log.lock().unwrap(), [Bytes::from_static(b"session-entry")]);
    assert!(weak_session.upgrade().is_none());
    assert!(weak_entry.upgrade().is_none());
}

#[test]
fn each_send_samples_current_epoch_separately_outside_guard() {
    let reentrant = Arc::new(Mutex::new(None::<std::sync::Weak<Mutex<State>>>));
    let root = reentrant.clone();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let recorded = observations.clone();
    let mut settings = config();
    settings.initial_window = 4;
    let session = ReplicationSession::new(
        identity(),
        Epoch::new(1, 2),
        0,
        settings,
        Arc::new(move |intent| {
            assert_eq!(intent.identity, identity());
            assert_eq!(intent.expires_after, Some(ms(10)));
            let SendKind::Replication {
                operation,
                original_epoch,
                sending_epoch,
                completed_hint,
            } = intent.kind
            else {
                panic!()
            };
            assert_eq!(original_epoch, Epoch::new(0, 9));
            assert_eq!(completed_hint, 8);
            recorded
                .lock()
                .unwrap()
                .push((operation.lsn(), sending_epoch));
            let state = root.lock().unwrap().as_ref().unwrap().upgrade().unwrap();
            ReplicationSession { state }.update_sending_epoch(Epoch::new(1, 3));
            true
        }),
        ms(0),
    );
    *reentrant.lock().unwrap() = Some(Arc::downgrade(&session.state));
    session.open();
    let batch = session.add_replay(vec![op(1), op(2)], 8, ms(0));
    batch.dispatch(); // Reentrant epoch update would deadlock under the guard.
    assert_eq!(
        *observations.lock().unwrap(),
        [(1, Epoch::new(1, 2)), (2, Epoch::new(1, 3))]
    );
}

#[test]
fn request_ack_is_infinite_and_does_not_invent_epoch_or_hint_payload() {
    let requests = Arc::new(Mutex::new(0));
    let count = requests.clone();
    let session = session(Arc::new(move |intent| {
        if matches!(intent.kind, SendKind::RequestAck) {
            assert_eq!(intent.expires_after, None);
            *count.lock().unwrap() += 1;
        }
        true
    }));
    session.add(op(1), 5, ms(0)).dispatch();
    drive(
        session.update_ack(replication_ack(1, 0), None).unwrap(),
        ms(1),
    );
    assert_eq!(*requests.lock().unwrap(), 0);
    assert_eq!(session.retry_deadline(), Some(ms(11)));
    session
        .on_timer(session.retry_deadline().unwrap())
        .dispatch();
    assert_eq!(*requests.lock().unwrap(), 1);
    session.close();
    assert_eq!(session.retry_deadline(), None);
}

#[test]
fn callback_queue_full_does_not_turn_send_into_receive_progress() {
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let recorded = attempts.clone();
    let session = session(Arc::new(move |intent| {
        if let SendKind::Replication { operation, .. } = intent.kind {
            recorded.lock().unwrap().push(operation.lsn());
        }
        false
    }));
    session.add(op(1), 0, ms(0)).dispatch();
    assert_eq!(session.receive_lsn(0), 0);
    session.on_timer(ms(10)).dispatch();
    assert_eq!(*attempts.lock().unwrap(), [1, 1]);
    assert_eq!(session.apply_lsn(0), 0);
}

#[test]
fn progress_views_clamp_negative_and_idle_target_is_supplied_not_completed() {
    let session = ReplicationSession::new(
        identity(),
        Epoch::new(1, 2),
        -1,
        config(),
        Arc::new(|_| true),
        ms(0),
    );
    assert_eq!(session.identity(), identity());
    assert_eq!(
        (
            session.receive_lsn(0),
            session.apply_lsn(0),
            session.idle_progress(None)
        ),
        (0, 0, 0)
    );
    session.open();
    drive(
        session.update_ack(replication_ack(10, 3), None).unwrap(),
        ms(1),
    );
    assert_eq!(session.idle_progress(None), 10);
    assert_eq!(session.idle_progress(Some(6)), 6);
    assert_eq!(session.idle_progress(Some(2)), 3);
    assert_eq!(session.idle_progress(Some(20)), 10);
    session.receive_lsn(7);
    assert_eq!(session.state.lock().unwrap().sender.progress(0).applied, 3);
}

#[test]
#[should_panic]
fn sending_epoch_cannot_decrease() {
    let session = session(Arc::new(|_| true));
    session.update_sending_epoch(Epoch::new(1, 1));
}

#[test]
fn concurrent_ingress_launches_one_rooted_processor_and_finish_allows_next() {
    let session = session(Arc::new(|_| true));
    let barrier = Arc::new(Barrier::new(2));
    let processors = std::thread::scope(|scope| {
        let threads: Vec<_> = (1..=2)
            .map(|lsn| {
                let session = session.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    session.update_ack(replication_ack(lsn, 0), None)
                })
            })
            .collect();
        threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(processors.len(), 1);
    drive(processors.into_iter().next().unwrap(), ms(1));
    assert_eq!(session.receive_lsn(0), 2);
    assert!(!session.is_processing());
    let next = session.update_ack(replication_ack(2, 1), None).unwrap();
    drive(next, ms(2));
    assert_eq!(session.apply_lsn(0), 1);
}

#[test]
fn processor_root_survives_handle_drop_through_last_handoff() {
    let session = session(Arc::new(|_| true));
    let weak = Arc::downgrade(&session.state);
    let processor = session.update_ack(ack(1, 0, 1, 0), None).unwrap();
    session.close();
    drop(session);
    assert!(weak.upgrade().is_some());
    drive(processor, ms(1));
    assert!(weak.upgrade().is_none());
}

#[test]
fn reentrant_close_during_send_does_not_retract_remaining_selected_sends() {
    let root = Arc::new(Mutex::new(None::<std::sync::Weak<Mutex<State>>>));
    let reentrant = root.clone();
    let count = Arc::new(Mutex::new(0));
    let sent = count.clone();
    let mut settings = config();
    settings.initial_window = 4;
    let session = ReplicationSession::new(
        identity(),
        Epoch::new(1, 2),
        0,
        settings,
        Arc::new(move |_| {
            *sent.lock().unwrap() += 1;
            let state = reentrant
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .upgrade()
                .unwrap();
            ReplicationSession { state }.close();
            true
        }),
        ms(0),
    );
    *root.lock().unwrap() = Some(Arc::downgrade(&session.state));
    session.open();
    session.add_replay(vec![op(1), op(2)], 0, ms(0)).dispatch();
    assert_eq!(*count.lock().unwrap(), 2);
}

#[test]
fn replication_only_pass_does_not_close_installed_start_copy_sender() {
    let session = session(Arc::new(|_| true));
    let mut start_copy = ReliableOperationSender::new(config(), 0, ms(0));
    start_copy.open();
    session.install_start_copy_sender(start_copy);
    drive(
        session.update_ack(replication_ack(1, 0), None).unwrap(),
        ms(1),
    );
    assert!(
        session
            .state
            .lock()
            .unwrap()
            .start_copy
            .as_ref()
            .unwrap()
            .is_active()
    );
    session.close();
    assert!(
        !session
            .state
            .lock()
            .unwrap()
            .start_copy
            .as_ref()
            .unwrap()
            .is_active()
    );
}

#[test]
fn session_statistics_reset_does_not_reset_sender_progress_or_pending_work() {
    let session = session(Arc::new(|_| true));
    session.add(op(1), 0, ms(0)).dispatch();
    session.on_timer(ms(11)).dispatch();
    assert_eq!(session.averages(), (ms(11), ms(11)));
    session.reset_average_statistics(ms(12));
    assert_eq!(session.averages(), (ms(0), ms(0)));
    assert_eq!(session.receive_lsn(0), 0);
    session.on_timer(ms(23)).dispatch();
    assert_eq!(session.averages(), (ms(23), ms(23)));
}

#[test]
fn dropping_unconsumed_processor_does_not_claim_finished_work() {
    let session = session(Arc::new(|_| true));
    let processor = session.update_ack(ack(1, 0, 1, 0), None).unwrap();
    drop(processor);
    assert!(session.is_processing());
    assert!(session.update_ack(ack(2, 0, 2, 0), None).is_none());
    // Integration must retain/drive its selected continuation, not task-abort.
}
