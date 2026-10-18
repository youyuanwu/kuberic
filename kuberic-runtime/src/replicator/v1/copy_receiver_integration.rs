// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-component boundary witnesses guided by SecondaryReplicator.cpp:
// 681-825,1500-1570,1967-2055,2087-2108 and RemoteSession.cpp:585-627,
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d,
// src/prod/src/Reliability/Replication/. No copy sender/build success is modeled.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::application::{Lsn, OperationData};
use crate::protocol::types::Epoch;

use super::ack::AckSender;
use super::operation_queue::{QueueConfig, QueueOperation};
use super::received_operation::ReceivedOperation;
use super::secondary_copy_receiver::{
    SecondaryCopyAck, SecondaryCopyConfig, SecondaryCopyReceiver,
};
use super::secondary_receiver::{AckRequest, ReceiverAck, ReceiverConfig, SecondaryReceiver};
use super::sender::{SenderConfig, SenderOperation};
use super::session::{
    AckAction, AckUpdate, ReplicationSession, SendIntent, SendKind, SessionIdentity,
};

struct Receivers {
    replication: SecondaryReceiver,
    copy: SecondaryCopyReceiver,
}

fn receivers(required: bool) -> Receivers {
    let mut replication = SecondaryReceiver::new(ReceiverConfig {
        queue: QueueConfig {
            initial_capacity: 4,
            max_capacity: 8,
            max_bytes: 0,
            max_completed_count: 0,
            max_completed_bytes: 0,
            clean_on_complete: true,
        },
        has_persisted_state: required,
        require_service_ack: false,
        enable_eos_ack: true,
    });
    replication.initialize(1, Epoch::new(1, 7)).unwrap();
    Receivers {
        replication,
        copy: SecondaryCopyReceiver::new(SecondaryCopyConfig {
            initial_capacity: 4,
            max_capacity: 8,
            require_service_ack: required,
            enable_eos_ack: true,
            copy_done: false,
        })
        .unwrap(),
    }
}

fn operation(lsn: Lsn, required: bool) -> ReceivedOperation {
    ReceivedOperation::new(
        QueueOperation::new(lsn, OperationData::from_static(b"data"), true),
        Epoch::new(1, 7),
        required,
    )
}

fn admit_pair(owner: &Arc<Mutex<Receivers>>, lsn: Lsn, required: bool) {
    let replication = operation(lsn, required);
    let copy = operation(lsn, required);
    let (replication_effects, copy_effects) = {
        let mut owner = owner.lock().unwrap();
        (
            owner
                .replication
                .receive(Epoch::new(1, 7), vec![replication.clone()], None)
                .unwrap(),
            owner.copy.receive(copy.clone(), false),
        )
    };
    let _ = replication_effects.dispatch().unwrap();
    let _ = copy_effects.dispatch().unwrap();
    if required {
        let (replication_effects, copy_effects) = {
            let mut owner = owner.lock().unwrap();
            (
                owner
                    .replication
                    .process_ack(replication.acknowledge().unwrap().unwrap())
                    .unwrap(),
                owner.copy.process_ack(copy.acknowledge().unwrap().unwrap()),
            )
        };
        let _ = replication_effects.dispatch().unwrap();
        let _ = copy_effects.dispatch().unwrap();
    }
}

#[test]
fn actual_copy_and_replication_progress_force_one_ack_and_sample_both_at_issuance() {
    for required in [false, true] {
        let owner = Arc::new(Mutex::new(receivers(required)));
        {
            let mut owner = owner.lock().unwrap();
            owner.replication.capture_ack();
            owner.copy.capture_ack();
        }
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let observed = snapshots.clone();
        let target = owner.clone();
        let mut sender = AckSender::new(
            Duration::from_millis(10),
            Duration::from_millis(100),
            2,
            Duration::ZERO,
        );
        sender.open(
            Arc::new(move |_| {
                let snapshot = {
                    let mut owner = target.lock().unwrap();
                    (owner.replication.capture_ack(), owner.copy.capture_ack())
                };
                observed.lock().unwrap().push(snapshot);
                true
            }),
            Duration::ZERO,
        );
        admit_pair(&owner, 1, required);
        let selected = {
            let owner = owner.lock().unwrap();
            assert_eq!(owner.replication.pending_ack_progress(), 1);
            assert_eq!(owner.copy.pending_ack_progress(), 1);
            let request = AckRequest {
                check_pending_limit: true,
            };
            assert!(
                owner
                    .replication
                    .schedule_ack(&mut sender, request, 0, 2, Duration::ZERO)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(sender.deadline(), Some(Duration::from_millis(10)));
            owner
                .replication
                .schedule_ack(
                    &mut sender,
                    request,
                    owner.copy.pending_ack_progress(),
                    2,
                    Duration::from_millis(1),
                )
                .unwrap()
                .unwrap()
        };
        assert_eq!(sender.deadline(), None);
        assert!(snapshots.lock().unwrap().is_empty());
        admit_pair(&owner, 2, required);
        selected.dispatch();
        assert_eq!(
            *snapshots.lock().unwrap(),
            [(
                ReceiverAck {
                    received: 2,
                    applied: 2
                },
                SecondaryCopyAck {
                    received: 2,
                    applied: 2
                }
            )]
        );
        let owner = owner.lock().unwrap();
        assert_eq!(owner.replication.pending_ack_progress(), 0);
        assert_eq!(owner.copy.pending_ack_progress(), 0);
    }
}

#[test]
fn actual_terminal_copy_ack_reaches_session_but_cannot_execute_missing_build_owner() {
    let outgoing = Arc::new(Mutex::new(Vec::<SendIntent>::new()));
    let sent = outgoing.clone();
    let session = ReplicationSession::new(
        SessionIdentity {
            primary_endpoint: "primary".into(),
            remote_endpoint: "secondary".into(),
            remote_incarnation: 1,
        },
        Epoch::new(1, 7),
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
    session
        .add(
            SenderOperation {
                operation: QueueOperation::new(
                    1,
                    OperationData::from_static(b"replication"),
                    false,
                ),
                original_epoch: Epoch::new(1, 7),
            },
            0,
            Duration::ZERO,
        )
        .dispatch();
    let replication = match outgoing.lock().unwrap().pop().unwrap().kind {
        SendKind::Replication {
            operation,
            original_epoch,
            ..
        } => ReceivedOperation::new(
            QueueOperation::new(operation.lsn(), operation.data().clone(), true),
            original_epoch,
            true,
        ),
        SendKind::RequestAck => panic!("expected actual replication send"),
    };
    let mut owner = receivers(true);
    let _ = owner
        .replication
        .receive(Epoch::new(1, 7), vec![replication.clone()], None)
        .unwrap()
        .dispatch()
        .unwrap();
    let _ = owner
        .replication
        .process_ack(replication.acknowledge().unwrap().unwrap())
        .unwrap()
        .dispatch()
        .unwrap();
    let copy = operation(1, true);
    let eos = ReceivedOperation::copy_end_of_stream(2, true).unwrap();
    let _ = owner.copy.receive(copy.clone(), false).dispatch().unwrap();
    let _ = owner.copy.receive(eos.clone(), true).dispatch().unwrap();
    assert_eq!(
        owner.copy.capture_ack(),
        SecondaryCopyAck {
            received: -1,
            applied: 0
        }
    );
    let _ = owner
        .copy
        .process_ack(copy.acknowledge().unwrap().unwrap())
        .dispatch()
        .unwrap();
    assert_eq!(owner.copy.capture_ack().applied, 1);
    let _ = owner
        .copy
        .process_ack(eos.acknowledge().unwrap().unwrap())
        .dispatch()
        .unwrap();
    assert!(owner.copy.all_operations_acked());
    let copy_ack = owner.copy.capture_ack();
    assert_eq!(
        copy_ack,
        SecondaryCopyAck {
            received: -1,
            applied: -1
        }
    );
    let replication_ack = owner.replication.capture_ack();
    let processor = session
        .update_ack(
            AckUpdate {
                replication_received: replication_ack.received,
                replication_applied: replication_ack.applied,
                copy_received: copy_ack.received,
                copy_applied: copy_ack.applied,
                process_copy: true,
                message_id: "actual terminal copy ACK".into(),
            },
            None,
        )
        .unwrap()
        .next(Duration::ZERO)
        .unwrap()
        .deliver(|action| match action {
            AckAction::ReplicationSends(sends) => sends.dispatch(),
            _ => panic!("replication sender must precede copy build gate"),
        });
    assert_eq!(session.progress(0).applied, 1);
    let unresolved_build = processor.next(Duration::ZERO).unwrap();
    assert!(session.is_processing());
    drop(unresolved_build);
    assert!(session.is_processing());
}
