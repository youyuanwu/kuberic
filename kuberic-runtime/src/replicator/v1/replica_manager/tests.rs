// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Component scenarios derived from ReplicaManager.Test.cpp, ReplicaManager.cpp
// and PrimaryReplicator.CatchupAsyncOperation.cpp at the pinned native revision.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::oneshot::error::TryRecvError;

use super::*;
use crate::replicator::v1::operation_queue::{EnqueueError, QueueConfig};
use crate::replicator::v1::sender::SenderConfig;
use crate::replicator::v1::session::{AckAction, AckUpdate, SendIntent, SendKind, SessionIdentity};

fn now() -> Duration {
    Duration::from_millis(1)
}

fn limits() -> BufferLimits {
    BufferLimits {
        max_segments: 8,
        max_payload_bytes: 100,
    }
}

fn queue_config(max_capacity: usize) -> QueueConfig {
    QueueConfig {
        initial_capacity: 2,
        max_capacity,
        max_bytes: 0,
        max_completed_count: 0,
        max_completed_bytes: 0,
        clean_on_complete: true,
    }
}

fn manager(max_capacity: usize, allow_multiple: bool) -> ReplicaManager {
    ReplicaManager::new(
        queue_config(max_capacity),
        0,
        1,
        "primary".into(),
        Epoch::new(1, 2),
        allow_multiple,
    )
    .unwrap()
}

fn set(write_quorum: usize, members: &[SessionMember]) -> ReplicaSet {
    ReplicaSet {
        write_quorum,
        members: members.to_vec(),
    }
}

fn configure(
    manager: &mut ReplicaManager,
    current: ReplicaSet,
    previous: Option<ReplicaSet>,
    catchup_configuration: bool,
) -> ManagerEffects {
    manager
        .configure_established(EstablishedConfiguration {
            current,
            previous,
            catchup_configuration,
        })
        .unwrap()
}

fn member_with_send(id: i64, send: Arc<dyn Fn(SendIntent) -> bool + Send + Sync>) -> SessionMember {
    member_with_identity(
        id,
        SessionIdentity {
            primary_endpoint: "primary".into(),
            remote_endpoint: format!("replica-{id}"),
            remote_incarnation: 7,
        },
        send,
    )
}

fn member_with_identity(
    id: i64,
    identity: SessionIdentity,
    send: Arc<dyn Fn(SendIntent) -> bool + Send + Sync>,
) -> SessionMember {
    let session = ReplicationSession::new(
        identity,
        Epoch::new(9, 9),
        0,
        SenderConfig {
            initial_window: 16,
            max_window: 16,
            retry: Duration::from_millis(10),
            decay_factor: 0.0,
            decay_interval: Duration::from_millis(100),
        },
        send,
        Duration::ZERO,
    );
    session.open();
    SessionMember {
        replica_id: id,
        session,
        must_catchup: false,
    }
}

fn member(id: i64) -> SessionMember {
    member_with_send(id, Arc::new(|_| true))
}

#[test]
fn live_retained_target_keeps_frozen_pc_and_pending_commitment_until_fresh_root_ack() {
    let mut manager = manager(64, true);
    let old = member(2);
    let original = set(2, std::slice::from_ref(&old));
    configure(&mut manager, original.clone(), None, false)
        .dispatch(now())
        .unwrap();
    let (lsn, mut completion) = enqueue(&mut manager);
    assert_eq!(lsn, 1);
    assert_eq!(completion.try_recv(), Err(TryRecvError::Empty));
    old.session.close();
    let fresh = member_with_identity(
        2,
        SessionIdentity {
            primary_endpoint: "primary".into(),
            remote_endpoint: "fresh-same-retained-member".into(),
            remote_incarnation: 8,
        },
        Arc::new(|_| true),
    );
    manager
        .admit_live_target(2, &old.session, fresh.session.clone())
        .unwrap()
        .dispatch(now())
        .unwrap();
    assert_eq!(completion.try_recv(), Err(TryRecvError::Empty));
    let joint = set(2, std::slice::from_ref(&fresh));
    configure(&mut manager, joint.clone(), Some(original.clone()), true)
        .dispatch(now())
        .unwrap();
    let (current, previous) = manager.configuration_roots();
    assert!(
        current.unwrap().members[0]
            .session
            .same_instance(&fresh.session)
    );
    assert!(
        previous.unwrap().members[0]
            .session
            .same_instance(&old.session)
    );
    assert!(!old.session.same_instance(&fresh.session));
    assert_eq!(completion.try_recv(), Err(TryRecvError::Empty));
    ack(&mut manager, &fresh, 1, 1);
    assert_eq!(completion.try_recv(), Ok(Ok(())));
    assert_eq!(
        old.session.progress(0).applied,
        0,
        "fresh logical support must not rewrite old physical credit"
    );
    assert_eq!(manager.queue().current_progress(), 1);
    let (caught_up, effects) = manager.begin_catchup(CatchupMode::All).unwrap();
    effects.dispatch(now()).unwrap();
    assert_eq!(caught_up.blocking_recv().unwrap(), Ok(()));
    configure(&mut manager, joint, None, false)
        .dispatch(now())
        .unwrap();
    assert!(manager.configuration_roots().1.is_none());
    let (lsn, mut next) = enqueue(&mut manager);
    assert_eq!(lsn, 2);
    assert_eq!(next.try_recv(), Err(TryRecvError::Empty));
    ack(&mut manager, &fresh, 2, 2);
    assert_eq!(next.try_recv(), Ok(Ok(())));
}

#[test]
fn live_retained_target_preserves_true_missing_replay_and_original_root() {
    let mut manager = manager(64, true);
    let old = member(2);
    configure(
        &mut manager,
        set(2, std::slice::from_ref(&old)),
        None,
        false,
    )
    .dispatch(now())
    .unwrap();
    let (_, mut completion) = enqueue(&mut manager);
    ack(&mut manager, &old, 1, 1);
    assert_eq!(completion.try_recv(), Ok(Ok(())));
    old.session.close();
    let fresh = member_with_identity(
        2,
        SessionIdentity {
            primary_endpoint: "primary".into(),
            remote_endpoint: "retained-but-missing-prefix".into(),
            remote_incarnation: 8,
        },
        Arc::new(|_| true),
    );
    assert!(matches!(
        manager.admit_live_target(2, &old.session, fresh.session),
        Err(ManagerError::Configuration(
            ConfigurationError::MissingCurrentReplay(2)
        ))
    ));
    assert!(manager.live_reformation.is_none());
    assert!(manager.current_root(2).unwrap().same_instance(&old.session));
    assert_eq!(manager.queue().current_progress(), 1);
}

#[test]
fn live_retained_target_does_not_replace_or_alias_other_original_member_roots() {
    let mut manager = manager(64, true);
    let old = member(2);
    let unchanged = member(3);
    let original = set(3, &[old.clone(), unchanged.clone()]);
    configure(&mut manager, original.clone(), None, false)
        .dispatch(now())
        .unwrap();
    let (_, mut completion) = enqueue(&mut manager);
    old.session.close();
    let fresh = member_with_identity(
        2,
        SessionIdentity {
            primary_endpoint: "primary".into(),
            remote_endpoint: "fresh-retained-target".into(),
            remote_incarnation: 8,
        },
        Arc::new(|_| true),
    );
    manager
        .admit_live_target(2, &old.session, fresh.session.clone())
        .unwrap()
        .dispatch(now())
        .unwrap();
    let impostor = member_with_identity(
        3,
        SessionIdentity {
            primary_endpoint: "primary".into(),
            remote_endpoint: "changed-unselected-peer".into(),
            remote_incarnation: 9,
        },
        Arc::new(|_| true),
    );
    assert!(
        manager
            .configure_established(EstablishedConfiguration {
                current: set(3, &[fresh.clone(), impostor]),
                previous: Some(original.clone()),
                catchup_configuration: true,
            })
            .is_err()
    );
    assert!(manager.live_reformation.as_ref().unwrap().joint.is_none());
    configure(
        &mut manager,
        set(3, &[fresh.clone(), unchanged.clone()]),
        Some(original),
        true,
    )
    .dispatch(now())
    .unwrap();
    let (current, previous) = manager.configuration_roots();
    for set in [current.unwrap(), previous.unwrap()] {
        assert!(
            set.members
                .iter()
                .find(|member| member.replica_id == 3)
                .unwrap()
                .session
                .same_instance(&unchanged.session)
        );
    }
    ack(&mut manager, &fresh, 1, 1);
    assert_eq!(completion.try_recv(), Err(TryRecvError::Empty));
    ack(&mut manager, &unchanged, 1, 1);
    assert_eq!(completion.try_recv(), Ok(Ok(())));
}

fn enqueue(manager: &mut ReplicaManager) -> (Lsn, CompletionReceiver) {
    let admission = manager
        .enqueue(
            Bytes::from_static(b"entry"),
            BufferPolicy::RawSingle,
            limits(),
        )
        .unwrap();
    let lsn = admission.operation.operation.lsn();
    admission.effects.dispatch(now()).unwrap();
    (lsn, admission.completion)
}

fn held(tail: Lsn) -> (ReplicaManager, Vec<CompletionReceiver>) {
    let mut manager = manager(64, true);
    // Native allows insufficient membership, but normal progress stays pending.
    configure(&mut manager, set(2, &[]), None, false)
        .dispatch(now())
        .unwrap();
    let completions = (1..=tail)
        .map(|expected| {
            let (lsn, receiver) = enqueue(&mut manager);
            assert_eq!(lsn, expected);
            receiver
        })
        .collect();
    (manager, completions)
}

#[derive(Default, Debug, PartialEq, Eq)]
struct AckTrace {
    replication_dispatches: usize,
    /// These remain explicit copy-owner obligations, not fabricated build ACKs.
    unresolved_copy_gates: Vec<(Lsn, Lsn)>,
    primary_notifications: usize,
}

fn ack(
    manager: &mut ReplicaManager,
    member: &SessionMember,
    received: Lsn,
    applied: Lsn,
) -> AckTrace {
    let notifications = Arc::new(AtomicUsize::new(0));
    let notified = notifications.clone();
    let processor = member.session.update_ack(
        AckUpdate {
            replication_received: received,
            replication_applied: applied,
            copy_received: 0,
            copy_applied: 0,
            process_copy: false,
            message_id: format!("{received}/{applied}"),
        },
        Some(Arc::new(move || {
            notified.fetch_add(1, Ordering::SeqCst);
        })),
    );
    let mut trace = AckTrace::default();
    if let Some(mut processor) = processor {
        while let Some(handoff) = processor.next(now()) {
            processor = handoff.deliver(|action| match action {
                AckAction::ReplicationSends(batch) => {
                    trace.replication_dispatches += 1;
                    batch.dispatch();
                }
                AckAction::CopyReplicationGate {
                    copy_applied,
                    replication_applied,
                } => trace
                    .unresolved_copy_gates
                    .push((copy_applied, replication_applied)),
                AckAction::PrimaryProgress(notification) => {
                    notification.dispatch();
                    trace.primary_notifications += 1;
                    manager.update_progress().unwrap().dispatch(now()).unwrap();
                }
                AckAction::CopyAck { .. } => {
                    panic!("replication-only ACK must not fabricate copy completion");
                }
            });
        }
    }
    assert!(!member.session.is_processing());
    assert_eq!(
        notifications.load(Ordering::SeqCst),
        trace.primary_notifications
    );
    trace
}

fn pending(receiver: &mut CompletionReceiver) {
    assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
}

fn succeeded(receiver: &mut CompletionReceiver) {
    assert_eq!(receiver.try_recv(), Ok(Ok(())));
}

#[test]
fn recovery_reset_forwards_actual_queue_boundaries_and_assigns_from_recovered_tail() {
    let mut manager = manager(8, true);
    configure(&mut manager, set(1, &[]), None, false)
        .dispatch(now())
        .unwrap();
    let (_, mut completed) = enqueue(&mut manager);
    succeeded(&mut completed);
    for progress in [24, 2, 0] {
        manager.reset_replication_queue(progress).unwrap();
        assert_eq!(manager.queue().current_progress(), progress);
        assert_eq!(manager.queue().last_sequence_number(), progress);
        assert_eq!(manager.queue().previous_config_catchup_lsn(), progress);
        assert_eq!(manager.queue().previous_config_quorum_lsn(), progress);
        assert_eq!(manager.queue().catchup_capability(), 0);
        assert_eq!(manager.queue().operation_count(), 0);
        assert_eq!(manager.queue().total_bytes(), 0);
        let (lsn, mut completed) = enqueue(&mut manager);
        assert_eq!(lsn, progress + 1);
        succeeded(&mut completed);
    }
    manager.close().dispatch(now()).unwrap();
    assert!(matches!(
        manager.reset_replication_queue(7),
        Err(ManagerError::ObjectClosed)
    ));
}

#[test]
fn recovery_reset_cannot_drop_pending_completions_or_rewrite_original_operations() {
    let (mut manager, mut receivers) = held(1);
    let mut replay = Vec::new();
    assert!(manager.queue().get_operations(1, &mut replay));
    let original = replay[0].clone();
    assert!(matches!(
        manager.reset_replication_queue(24),
        Err(ManagerError::QueueNormalization(
            QueueNormalizationError::PendingOperations
        ))
    ));
    pending(&mut receivers[0]);
    assert_eq!(manager.pending_count(), 1);
    assert_eq!(manager.queue().last_sequence_number(), 1);
    replay.clear();
    assert!(manager.queue().get_operations(1, &mut replay));
    assert!(Arc::ptr_eq(&original.operation, &replay[0].operation));
    assert_eq!(original.original_epoch, replay[0].original_epoch);
}

#[test]
fn assigned_before_quorum_ack_completes_once_without_provider_commit() {
    let mut manager = manager(8, true);
    let remote = member(2);
    configure(
        &mut manager,
        set(2, std::slice::from_ref(&remote)),
        None,
        false,
    )
    .dispatch(now())
    .unwrap();
    let admission = manager
        .enqueue(
            Bytes::from_static(b"early"),
            BufferPolicy::RawSingle,
            limits(),
        )
        .unwrap();
    assert_eq!(admission.operation.operation.lsn(), 1);
    assert_eq!(manager.queue().last_sequence_number(), 1);
    assert_eq!(manager.queue().current_progress(), 0);
    let mut completion = admission.completion;
    pending(&mut completion);
    admission.effects.dispatch(now()).unwrap();
    let trace = ack(&mut manager, &remote, 1, 1);
    assert_eq!(trace.primary_notifications, 1);
    assert_eq!(trace.unresolved_copy_gates, vec![(0, 1)]);
    succeeded(&mut completion);
    assert_eq!(completion.try_recv(), Err(TryRecvError::Closed));
    assert_eq!(manager.pending_count(), 0);
    assert_eq!(manager.queue().operation_count(), 0);
    assert_eq!(ack(&mut manager, &remote, 1, 1), AckTrace::default());
    manager.update_progress().unwrap().dispatch(now()).unwrap();
    assert_eq!(manager.pending_count(), 0);
}

#[test]
fn malformed_and_full_admission_never_consume_sequence_numbers() {
    let mut manager = manager(2, true);
    configure(&mut manager, set(2, &[]), None, false)
        .dispatch(now())
        .unwrap();
    let invalid = manager.enqueue(
        Bytes::from_static(b"not-a-segment-frame"),
        BufferPolicy::FramedSegments,
        limits(),
    );
    assert!(matches!(
        invalid,
        Err(ManagerError::Admission(QueueAdmissionError::Buffer(_)))
    ));
    assert_eq!(manager.queue().last_sequence_number(), 0);
    let (first, _first_completion) = enqueue(&mut manager);
    let (second, _second_completion) = enqueue(&mut manager);
    assert_eq!((first, second), (1, 2));
    assert!(matches!(
        manager.enqueue(
            Bytes::from_static(b"full"),
            BufferPolicy::RawSingle,
            limits()
        ),
        Err(ManagerError::Admission(QueueAdmissionError::Queue(
            EnqueueError::Full
        )))
    ));
    assert_eq!(manager.queue().last_sequence_number(), 2);
    assert_eq!(manager.pending_count(), 2);
}

#[test]
fn native_w2_five_six_commits_six_and_reclaims_five() {
    let (mut manager, mut completions) = held(6);
    let first = member(2);
    let second = member(3);
    configure(
        &mut manager,
        set(2, &[first.clone(), second.clone()]),
        None,
        false,
    )
    .dispatch(now())
    .unwrap();
    ack(&mut manager, &first, 5, 5);
    ack(&mut manager, &second, 6, 6);
    assert_eq!(manager.queue().current_progress(), 6);
    assert_eq!(manager.queue().first_lsn_in_replication_queue(), 6);
    assert_eq!(manager.queue().operation_count(), 1);
    for completion in &mut completions {
        succeeded(completion);
    }
    let view = manager.catchup_observation().unwrap().unwrap();
    assert_eq!((view.quorum, view.all, view.latest), (6, 5, 6));
}

#[test]
fn native_w3_two_three_five_commits_three_and_reclaims_two() {
    let (mut manager, mut completions) = held(5);
    let first = member(2);
    let second = member(3);
    let third = member(4);
    configure(
        &mut manager,
        set(3, &[first.clone(), second.clone(), third.clone()]),
        None,
        false,
    )
    .dispatch(now())
    .unwrap();
    ack(&mut manager, &first, 2, 2);
    ack(&mut manager, &second, 3, 3);
    ack(&mut manager, &third, 5, 5);
    assert_eq!(manager.queue().current_progress(), 3);
    assert_eq!(manager.queue().first_lsn_in_replication_queue(), 3);
    assert_eq!(manager.queue().operation_count(), 3);
    for completion in &mut completions[..3] {
        succeeded(completion);
    }
    for completion in &mut completions[3..] {
        pending(completion);
    }
}

#[test]
fn receive_progress_reclaims_but_all_catchup_requires_apply() {
    let (mut manager, _completions) = held(2);
    let first = member(2);
    let second = member(3);
    configure(
        &mut manager,
        set(2, &[first.clone(), second.clone()]),
        None,
        false,
    )
    .dispatch(now())
    .unwrap();
    let (mut all, effects) = manager.begin_catchup(CatchupMode::All).unwrap();
    effects.dispatch(now()).unwrap();
    ack(&mut manager, &first, 2, 2);
    ack(&mut manager, &second, 2, 0);
    assert_eq!(manager.queue().current_progress(), 2);
    assert_eq!(manager.queue().operation_count(), 0);
    pending(&mut all);
    assert_eq!(manager.catchup_observation().unwrap().unwrap().all, 0);
    ack(&mut manager, &second, 2, 2);
    succeeded(&mut all);
}

#[test]
fn current_and_previous_quorums_constrain_writes_independently_not_catchup() {
    let (mut manager, mut completions) = held(2);
    let current = member(2);
    let previous = member(3);
    configure(
        &mut manager,
        set(2, std::slice::from_ref(&current)),
        Some(set(2, std::slice::from_ref(&previous))),
        true,
    )
    .dispatch(now())
    .unwrap();
    let (mut catchup, effects) = manager.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    effects.dispatch(now()).unwrap();
    ack(&mut manager, &current, 2, 2);
    assert_eq!(manager.queue().current_progress(), 0);
    succeeded(&mut catchup);
    for completion in &mut completions {
        pending(completion);
    }
    ack(&mut manager, &previous, 2, 1);
    assert_eq!(manager.queue().current_progress(), 1);
    succeeded(&mut completions[0]);
    pending(&mut completions[1]);
    ack(&mut manager, &previous, 2, 2);
    succeeded(&mut completions[1]);
}

#[test]
fn insufficient_previous_membership_blocks_queue_but_not_current_catchup() {
    let (mut manager, mut completions) = held(1);
    let current = member(2);
    let previous = member(3);
    configure(
        &mut manager,
        set(2, std::slice::from_ref(&current)),
        Some(set(3, std::slice::from_ref(&previous))),
        true,
    )
    .dispatch(now())
    .unwrap();
    let (mut catchup, effects) = manager.begin_catchup(CatchupMode::All).unwrap();
    effects.dispatch(now()).unwrap();
    ack(&mut manager, &current, 1, 1);
    succeeded(&mut catchup);
    assert_eq!(manager.queue().current_progress(), 0);
    assert_eq!(manager.queue().operation_count(), 1);
    pending(&mut completions[0]);
    // Deactivating PC itself releases the operation without another ACK.
    configure(&mut manager, set(2, &[current]), None, false)
        .dispatch(now())
        .unwrap();
    succeeded(&mut completions[0]);
    assert_eq!(manager.queue().operation_count(), 0);
}

#[test]
fn insufficient_current_membership_holds_normal_and_catchup_progress() {
    let (mut manager, mut completions) = held(1);
    let remote = member(2);
    configure(
        &mut manager,
        set(3, std::slice::from_ref(&remote)),
        None,
        false,
    )
    .dispatch(now())
    .unwrap();
    let (mut catchup, effects) = manager.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    effects.dispatch(now()).unwrap();
    ack(&mut manager, &remote, 1, 1);
    pending(&mut completions[0]);
    pending(&mut catchup);
    assert_eq!(manager.catchup_observation().unwrap(), None);
    configure(&mut manager, set(2, &[remote]), None, false)
        .dispatch(now())
        .unwrap();
    succeeded(&mut completions[0]);
    succeeded(&mut catchup);
}

#[test]
fn cc_w1_ignores_present_remotes_but_enqueue_fast_path_requires_empty_vectors() {
    let (mut manager, mut completions) = held(2);
    let remote = member(2);
    configure(&mut manager, set(1, &[remote]), None, false)
        .dispatch(now())
        .unwrap();
    for completion in &mut completions {
        succeeded(completion);
    }
    assert_eq!(manager.queue().operation_count(), 0);
    let (mut all, effects) = manager.begin_catchup(CatchupMode::All).unwrap();
    effects.dispatch(now()).unwrap();
    succeeded(&mut all);
    let (lsn, mut completion) = enqueue(&mut manager);
    assert_eq!(lsn, 3);
    pending(&mut completion);
    assert_eq!(manager.queue().current_progress(), 2);
    manager.update_progress().unwrap().dispatch(now()).unwrap();
    succeeded(&mut completion);
}

#[test]
fn pc_w1_with_remote_uses_minimum_apply_instead_of_primary_only_sentinel() {
    let (mut manager, mut completions) = held(2);
    let current = member(2);
    let previous = member(3);
    configure(
        &mut manager,
        set(1, &[current]),
        Some(set(1, std::slice::from_ref(&previous))),
        false,
    )
    .dispatch(now())
    .unwrap();
    ack(&mut manager, &previous, 2, 1);
    assert_eq!(manager.queue().current_progress(), 1);
    assert_eq!(manager.queue().first_lsn_in_replication_queue(), 2);
    succeeded(&mut completions[0]);
    pending(&mut completions[1]);
    let view = manager.catchup_observation().unwrap().unwrap();
    assert_eq!((view.quorum, view.all), (2, 2));
    ack(&mut manager, &previous, 2, 2);
    succeeded(&mut completions[1]);
}

#[test]
fn idle_receive_and_copy_finish_target_affect_reclamation_not_quorum_or_all() {
    let (mut manager, mut completions) = held(2);
    let active = member(2);
    let idle = member(3);
    manager
        .add_established_idle(EstablishedIdle {
            replica_id: idle.replica_id,
            session: idle.session.clone(),
            finish_copy_target: Some(1),
        })
        .unwrap()
        .dispatch(now())
        .unwrap();
    configure(
        &mut manager,
        set(2, std::slice::from_ref(&active)),
        None,
        false,
    )
    .dispatch(now())
    .unwrap();
    let (mut all, effects) = manager.begin_catchup(CatchupMode::All).unwrap();
    effects.dispatch(now()).unwrap();
    ack(&mut manager, &active, 2, 2);
    for completion in &mut completions {
        succeeded(completion);
    }
    succeeded(&mut all);
    assert_eq!(manager.queue().operation_count(), 2);
    ack(&mut manager, &idle, 2, 0);
    assert_eq!(manager.queue().first_lsn_in_replication_queue(), 2);
    assert_eq!(manager.queue().operation_count(), 1);
    ack(&mut manager, &idle, 2, 2);
    assert_eq!(manager.queue().operation_count(), 0);
}

#[test]
fn idle_cannot_supply_a_missing_quorum_vote_and_removal_releases_retention() {
    let (mut manager, mut completions) = held(1);
    let idle = member(2);
    manager
        .add_established_idle(EstablishedIdle {
            replica_id: idle.replica_id,
            session: idle.session.clone(),
            finish_copy_target: None,
        })
        .unwrap()
        .dispatch(now())
        .unwrap();
    ack(&mut manager, &idle, 1, 1);
    pending(&mut completions[0]);
    configure(&mut manager, set(1, &[]), None, false)
        .dispatch(now())
        .unwrap();
    succeeded(&mut completions[0]);
    let (_lsn, mut next) = enqueue(&mut manager);
    succeeded(&mut next);
    assert_eq!(manager.queue().operation_count(), 1);
    manager
        .remove_idle(idle.replica_id)
        .unwrap()
        .dispatch(now())
        .unwrap();
    assert_eq!(manager.queue().operation_count(), 0);
}

#[test]
fn catchup_configuration_cap_is_not_a_blanket_allow_multiple_quorum_set_rule() {
    for (allow_multiple, catchup_configuration, expected_commit) in
        [(false, true, 2), (true, true, 3), (false, false, 3)]
    {
        let (mut manager, _completions) = held(2);
        manager.allow_multiple_quorum_set = allow_multiple;
        let remote = member(2);
        configure(
            &mut manager,
            set(2, std::slice::from_ref(&remote)),
            None,
            catchup_configuration,
        )
        .dispatch(now())
        .unwrap();
        let (lsn, mut completion) = enqueue(&mut manager);
        assert_eq!(lsn, 3);
        assert_eq!(manager.queue().previous_config_catchup_lsn(), 2);
        ack(&mut manager, &remote, 3, 3);
        assert_eq!(manager.queue().current_progress(), expected_commit);
        if expected_commit == 2 {
            pending(&mut completion);
            assert_eq!(manager.queue().operation_count(), 1);
            // Catchup projection is the uncapped CC observation.
            assert_eq!(manager.catchup_observation().unwrap().unwrap().quorum, 3);
        } else {
            succeeded(&mut completion);
        }
    }
}

#[test]
fn all_wait_tracks_live_writes_and_completed_wait_does_not_reopen() {
    let (mut manager, _completions) = held(2);
    let remote = member(2);
    configure(
        &mut manager,
        set(2, std::slice::from_ref(&remote)),
        None,
        true,
    )
    .dispatch(now())
    .unwrap();
    let (mut all, effects) = manager.begin_catchup(CatchupMode::All).unwrap();
    effects.dispatch(now()).unwrap();
    let (lsn, _completion) = enqueue(&mut manager);
    assert_eq!(lsn, 3);
    ack(&mut manager, &remote, 2, 2);
    pending(&mut all);
    assert_eq!(manager.queue().previous_config_catchup_lsn(), 2);
    ack(&mut manager, &remote, 3, 3);
    succeeded(&mut all);
    let (_lsn, _completion) = enqueue(&mut manager);
    assert!(manager.catchup_wait.is_none());
    assert_eq!(all.try_recv(), Err(TryRecvError::Closed));
}

#[test]
fn write_quorum_wait_keeps_configuration_boundary_across_new_writes() {
    let (mut manager, _completions) = held(2);
    let remote = member(2);
    configure(
        &mut manager,
        set(2, std::slice::from_ref(&remote)),
        None,
        true,
    )
    .dispatch(now())
    .unwrap();
    let (mut quorum, effects) = manager.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    effects.dispatch(now()).unwrap();
    let (lsn, mut later_write) = enqueue(&mut manager);
    assert_eq!(lsn, 3);
    assert_eq!(manager.queue().previous_config_catchup_lsn(), 2);
    ack(&mut manager, &remote, 2, 2);
    succeeded(&mut quorum);
    pending(&mut later_write);
    assert!(manager.catchup_wait.is_none());
}

#[test]
fn multiple_must_catchup_replicas_must_reach_advancing_current_quorum_not_saved_b() {
    let (mut manager, _completions) = held(2);
    let leader = member(2);
    let mut first = member(3);
    let mut second = member(4);
    first.must_catchup = true;
    second.must_catchup = true;
    configure(
        &mut manager,
        set(2, &[leader.clone(), first.clone(), second.clone()]),
        None,
        true,
    )
    .dispatch(now())
    .unwrap();
    let (mut catchup, effects) = manager.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    effects.dispatch(now()).unwrap();
    enqueue(&mut manager);
    ack(&mut manager, &leader, 3, 3);
    ack(&mut manager, &first, 3, 2);
    ack(&mut manager, &second, 3, 3);
    let view = manager.catchup_observation().unwrap().unwrap();
    assert_eq!(view.configuration_boundary, 2);
    assert_eq!(view.quorum, 3);
    assert_eq!(view.minimum_must_catchup, Some(2));
    pending(&mut catchup);
    ack(&mut manager, &first, 3, 3);
    succeeded(&mut catchup);
    enqueue(&mut manager);
    assert!(manager.catchup_wait.is_none());
}

#[test]
fn configuration_unmark_completes_wait_without_new_ack_and_resets_normal_mode_marks() {
    let (mut manager, _completions) = held(1);
    let fast = member(2);
    let mut marked = member(3);
    marked.must_catchup = true;
    configure(
        &mut manager,
        set(2, &[fast.clone(), marked.clone()]),
        None,
        true,
    )
    .dispatch(now())
    .unwrap();
    let (mut catchup, effects) = manager.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    effects.dispatch(now()).unwrap();
    ack(&mut manager, &fast, 1, 1);
    pending(&mut catchup);
    marked.must_catchup = false;
    configure(&mut manager, set(2, &[fast, marked]), None, false)
        .dispatch(now())
        .unwrap();
    succeeded(&mut catchup);
    assert_eq!(
        manager
            .catchup_observation()
            .unwrap()
            .unwrap()
            .minimum_must_catchup,
        None
    );
}

#[test]
fn configuration_membership_change_completes_write_and_all_without_new_ack() {
    let (mut manager, mut completions) = held(1);
    let fast = member(2);
    let slow = member(3);
    configure(&mut manager, set(3, &[fast.clone(), slow]), None, false)
        .dispatch(now())
        .unwrap();
    let (mut all, effects) = manager.begin_catchup(CatchupMode::All).unwrap();
    effects.dispatch(now()).unwrap();
    ack(&mut manager, &fast, 1, 1);
    pending(&mut completions[0]);
    pending(&mut all);
    configure(&mut manager, set(2, &[fast]), None, false)
        .dispatch(now())
        .unwrap();
    succeeded(&mut completions[0]);
    succeeded(&mut all);
}

#[test]
fn dropped_completion_receiver_does_not_undo_admission_or_reclamation() {
    let mut manager = manager(8, true);
    let remote = member(2);
    configure(
        &mut manager,
        set(2, std::slice::from_ref(&remote)),
        None,
        false,
    )
    .dispatch(now())
    .unwrap();
    let (lsn, completion) = enqueue(&mut manager);
    drop(completion);
    assert_eq!(lsn, 1);
    assert_eq!(manager.pending_count(), 1);
    assert_eq!(manager.queue().operation_count(), 1);
    ack(&mut manager, &remote, 1, 1);
    assert_eq!(manager.pending_count(), 0);
    assert_eq!(manager.queue().current_progress(), 1);
    assert_eq!(manager.queue().operation_count(), 0);
}

#[test]
fn close_releases_cap_completes_reached_writes_then_cancels_rest_and_closes_wait() {
    let (mut manager, _completions) = held(2);
    manager.allow_multiple_quorum_set = false;
    let remote = member(2);
    configure(
        &mut manager,
        set(2, std::slice::from_ref(&remote)),
        None,
        true,
    )
    .dispatch(now())
    .unwrap();
    let (_lsn, mut third) = enqueue(&mut manager);
    let (_lsn, mut fourth) = enqueue(&mut manager);
    let (mut all, effects) = manager.begin_catchup(CatchupMode::All).unwrap();
    effects.dispatch(now()).unwrap();
    ack(&mut manager, &remote, 3, 3);
    assert_eq!(manager.queue().current_progress(), 2);
    pending(&mut third);
    pending(&mut fourth);
    let effects = manager.close();
    assert!(effects.close_progress_error.is_none());
    assert_eq!(manager.pending_count(), 0);
    // Terminal results are owned effects, not callbacks under the manager.
    pending(&mut third);
    effects.dispatch(now()).unwrap();
    succeeded(&mut third);
    assert_eq!(
        fourth.try_recv(),
        Ok(Err(CompletionError::OperationCanceled))
    );
    assert_eq!(all.try_recv(), Ok(Err(CompletionError::ObjectClosed)));
    assert_eq!(manager.queue().current_progress(), 3);
    manager.close().dispatch(now()).unwrap();
    assert!(matches!(
        manager.enqueue(Bytes::new(), BufferPolicy::RawSingle, limits()),
        Err(ManagerError::ObjectClosed)
    ));
    assert!(matches!(
        manager.begin_catchup(CatchupMode::All),
        Err(ManagerError::ObjectClosed)
    ));
    assert!(matches!(
        manager.update_progress(),
        Err(ManagerError::ObjectClosed)
    ));
}

#[test]
fn only_one_pending_catchup_is_allowed_and_explicit_cancellation_is_terminal() {
    let (mut manager, _completions) = held(1);
    let (mut first, effects) = manager.begin_catchup(CatchupMode::All).unwrap();
    effects.dispatch(now()).unwrap();
    assert!(matches!(
        manager.begin_catchup(CatchupMode::WriteQuorum),
        Err(ManagerError::CatchupAlreadyPending)
    ));
    manager.cancel_catchup().dispatch(now()).unwrap();
    assert_eq!(
        first.try_recv(),
        Ok(Err(CompletionError::OperationCanceled))
    );
    let (mut second, effects) = manager.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    effects.dispatch(now()).unwrap();
    pending(&mut second);
    assert_eq!(manager.queue().previous_config_catchup_lsn(), 0);
    manager.close().dispatch(now()).unwrap();
    assert_eq!(second.try_recv(), Ok(Err(CompletionError::ObjectClosed)));
}

#[test]
fn retained_replay_preserves_original_epoch_and_real_sender_identity() {
    let (mut manager, _completions) = held(2);
    let sends = Arc::new(Mutex::new(Vec::new()));
    let capture = sends.clone();
    let remote = member_with_send(
        2,
        Arc::new(move |intent| {
            if let SendKind::Replication {
                operation,
                original_epoch,
                sending_epoch,
                ..
            } = intent.kind
            {
                capture.lock().unwrap().push((
                    operation.lsn(),
                    original_epoch,
                    sending_epoch,
                    intent.identity,
                ));
            }
            true
        }),
    );
    let effects = configure(
        &mut manager,
        set(2, std::slice::from_ref(&remote)),
        None,
        true,
    );
    assert!(sends.lock().unwrap().is_empty());
    effects.dispatch(now()).unwrap();
    let sends = sends.lock().unwrap();
    assert_eq!(sends.len(), 2);
    for (index, (lsn, original, sending, identity)) in sends.iter().enumerate() {
        assert_eq!(*lsn, index as i64 + 1);
        assert_eq!(*original, Epoch::new(1, 2));
        assert_eq!(*sending, Epoch::new(9, 9));
        assert_eq!(*identity, remote.session.identity());
    }
}

#[test]
fn equal_identity_cannot_replace_a_live_session_ack_root() {
    let (mut manager, _completions) = held(1);
    let original = member(2);
    configure(
        &mut manager,
        set(2, std::slice::from_ref(&original)),
        None,
        false,
    )
    .dispatch(now())
    .unwrap();
    let replacement = member(2);
    assert_eq!(original.session.identity(), replacement.session.identity());
    assert!(matches!(
        manager.configure_established(EstablishedConfiguration {
            current: set(2, &[replacement]),
            previous: None,
            catchup_configuration: false,
        }),
        Err(ManagerError::Configuration(
            ConfigurationError::SessionRootChanged(2)
        ))
    ));
    // Cloned handles retain the actual sender and may be shared by CC and PC.
    configure(
        &mut manager,
        set(2, std::slice::from_ref(&original)),
        Some(set(2, std::slice::from_ref(&original))),
        false,
    )
    .dispatch(now())
    .unwrap();
    ack(&mut manager, &original, 1, 1);
    assert_eq!(manager.queue().current_progress(), 1);
}

#[test]
fn new_current_missing_replay_is_error_but_previous_missing_replay_is_explicit_pause() {
    let mut manager = manager(8, true);
    configure(&mut manager, set(1, &[]), None, false)
        .dispatch(now())
        .unwrap();
    for _ in 0..2 {
        let (_lsn, mut completion) = enqueue(&mut manager);
        succeeded(&mut completion);
    }
    assert_eq!(manager.queue().operation_count(), 0);
    let remote = member(2);
    assert!(matches!(
        manager.configure_established(EstablishedConfiguration {
            current: set(2, std::slice::from_ref(&remote)),
            previous: None,
            catchup_configuration: true,
        }),
        Err(ManagerError::Configuration(
            ConfigurationError::MissingCurrentReplay(2)
        ))
    ));
    let effects = configure(
        &mut manager,
        set(1, &[]),
        Some(set(1, std::slice::from_ref(&remote))),
        true,
    );
    assert_eq!(effects.paused_previous_replay, vec![2]);
    effects.dispatch(now()).unwrap();
    assert_eq!(remote.session.retry_deadline(), None);
    let (mut all, effects) = manager.begin_catchup(CatchupMode::All).unwrap();
    effects.dispatch(now()).unwrap();
    succeeded(&mut all);
}

#[test]
fn invalid_membership_marks_and_previous_expansion_are_rejected_before_installing() {
    let (mut manager, _completions) = held(1);
    let first = member(2);
    let second = member(3);
    assert!(matches!(
        manager.configure_established(EstablishedConfiguration {
            current: set(2, &[first.clone(), first.clone()]),
            previous: None,
            catchup_configuration: true,
        }),
        Err(ManagerError::Configuration(
            ConfigurationError::DuplicateReplica(2)
        ))
    ));
    let mut marked = first.clone();
    marked.must_catchup = true;
    assert!(matches!(
        manager.configure_established(EstablishedConfiguration {
            current: set(2, &[marked]),
            previous: None,
            catchup_configuration: false,
        }),
        Err(ManagerError::Configuration(
            ConfigurationError::UnexpectedMustCatchup(2)
        ))
    ));
    configure(
        &mut manager,
        set(2, std::slice::from_ref(&first)),
        Some(set(2, std::slice::from_ref(&second))),
        true,
    )
    .dispatch(now())
    .unwrap();
    assert!(matches!(
        manager.configure_established(EstablishedConfiguration {
            current: set(2, std::slice::from_ref(&first)),
            previous: Some(set(2, &[second, first])),
            catchup_configuration: true,
        }),
        Err(ManagerError::Configuration(
            ConfigurationError::PreviousConfigurationExpanded(2)
        ))
    ));
    assert_eq!(manager.queue().last_sequence_number(), 1);
}

#[test]
fn session_send_callbacks_run_only_when_effects_dispatch_outside_owner_guard() {
    let owner = Arc::new(Mutex::new(manager(8, true)));
    let observed = Arc::new(Mutex::new(0));
    let weak_owner = Arc::downgrade(&owner);
    let capture = observed.clone();
    let remote = member_with_send(
        2,
        Arc::new(move |_| {
            let owner = weak_owner.upgrade().unwrap();
            assert!(owner.try_lock().is_ok(), "send callback under owner guard");
            *capture.lock().unwrap() += 1;
            true
        }),
    );
    let effects = configure(&mut owner.lock().unwrap(), set(2, &[remote]), None, false);
    effects.dispatch(now()).unwrap();
    let admission = owner
        .lock()
        .unwrap()
        .enqueue(
            Bytes::from_static(b"entry"),
            BufferPolicy::RawSingle,
            limits(),
        )
        .unwrap();
    assert_eq!(*observed.lock().unwrap(), 0);
    admission.effects.dispatch(now()).unwrap();
    assert_eq!(*observed.lock().unwrap(), 1);
}

#[test]
fn wait_query_and_enqueue_cannot_bypass_ack_processors_primary_progress_handoff() {
    let (mut manager, mut completions) = held(1);
    let remote = member(2);
    configure(
        &mut manager,
        set(2, std::slice::from_ref(&remote)),
        None,
        true,
    )
    .dispatch(now())
    .unwrap();
    let notifications = Arc::new(AtomicUsize::new(0));
    let notified = notifications.clone();
    let processor = remote
        .session
        .update_ack(
            AckUpdate {
                replication_received: 1,
                replication_applied: 1,
                copy_received: 0,
                copy_applied: 0,
                process_copy: false,
                message_id: "paused-before-primary".into(),
            },
            Some(Arc::new(move || {
                notified.fetch_add(1, Ordering::SeqCst);
            })),
        )
        .unwrap();
    let sends = processor.next(now()).unwrap();
    let processor = sends.deliver(|action| match action {
        AckAction::ReplicationSends(batch) => batch.dispatch(),
        _ => panic!("replication dispatch must precede copy and primary handoffs"),
    });
    assert_eq!(remote.session.apply_lsn(0), 1);
    assert_eq!(manager.queue().current_progress(), 0);
    let copy_gate = processor.next(now()).unwrap();
    let processor = copy_gate.deliver(|action| match action {
        AckAction::CopyReplicationGate {
            copy_applied,
            replication_applied,
        } => assert_eq!((copy_applied, replication_applied), (0, 1)),
        _ => panic!("copy obligation must precede the primary handoff"),
    });
    // Copy gate has been observed, not declared successfully built. The
    // primary-progress owner has not received its ordered handoff yet.
    assert_eq!(notifications.load(Ordering::SeqCst), 0);
    assert_eq!(manager.catchup_observation().unwrap().unwrap().quorum, 0);
    let (mut wait, effects) = manager.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    effects.dispatch(now()).unwrap();
    pending(&mut wait);
    let (lsn, mut later_write) = enqueue(&mut manager);
    assert_eq!(lsn, 2);
    assert_eq!(manager.catchup_observation().unwrap().unwrap().quorum, 0);
    pending(&mut wait);
    pending(&mut completions[0]);
    let primary = processor.next(now()).unwrap();
    let processor = primary.deliver(|action| match action {
        AckAction::PrimaryProgress(notification) => {
            notification.dispatch();
            manager.update_progress().unwrap().dispatch(now()).unwrap();
        }
        _ => panic!("expected the primary-progress owner handoff"),
    });
    assert_eq!(notifications.load(Ordering::SeqCst), 1);
    assert!(processor.next(now()).is_none());
    succeeded(&mut wait);
    succeeded(&mut completions[0]);
    pending(&mut later_write);
    assert_eq!(manager.catchup_observation().unwrap().unwrap().quorum, 1);
}

#[test]
fn invalid_session_identity_incarnation_and_local_membership_are_typed_errors() {
    let mut manager = manager(8, true);
    assert!(matches!(
        manager.enqueue(Bytes::new(), BufferPolicy::RawSingle, limits()),
        Err(ManagerError::ReconfigurationPending)
    ));
    for identity in [
        SessionIdentity {
            primary_endpoint: "other-primary".into(),
            remote_endpoint: "remote".into(),
            remote_incarnation: 7,
        },
        SessionIdentity {
            primary_endpoint: "primary".into(),
            remote_endpoint: String::new(),
            remote_incarnation: 7,
        },
        SessionIdentity {
            primary_endpoint: "primary".into(),
            remote_endpoint: "remote".into(),
            remote_incarnation: -1,
        },
    ] {
        let remote = member_with_identity(2, identity, Arc::new(|_| true));
        assert!(matches!(
            manager.configure_established(EstablishedConfiguration {
                current: set(2, &[remote]),
                previous: None,
                catchup_configuration: false,
            }),
            Err(ManagerError::Configuration(
                ConfigurationError::InvalidSessionIdentity(2)
            ))
        ));
        assert!(manager.current.is_none());
    }
    let local = member(1);
    assert!(matches!(
        manager.configure_established(EstablishedConfiguration {
            current: set(2, &[local]),
            previous: None,
            catchup_configuration: false,
        }),
        Err(ManagerError::Configuration(
            ConfigurationError::LocalReplica(1)
        ))
    ));
    assert_eq!(manager.queue().last_sequence_number(), 0);
}

#[test]
fn retained_queue_recommit_never_reopens_completed_operations_or_waits() {
    let (mut manager, mut completions) = held(2);
    let fast = member(2);
    let idle = member(3);
    manager
        .add_established_idle(EstablishedIdle {
            replica_id: idle.replica_id,
            session: idle.session,
            finish_copy_target: None,
        })
        .unwrap()
        .dispatch(now())
        .unwrap();
    configure(
        &mut manager,
        set(2, std::slice::from_ref(&fast)),
        None,
        false,
    )
    .dispatch(now())
    .unwrap();
    let (mut all, effects) = manager.begin_catchup(CatchupMode::All).unwrap();
    effects.dispatch(now()).unwrap();
    ack(&mut manager, &fast, 2, 2);
    for completion in &mut completions {
        succeeded(completion);
    }
    succeeded(&mut all);
    assert_eq!(manager.queue().operation_count(), 2);
    let slow = member(4);
    configure(
        &mut manager,
        set(3, &[fast.clone(), slow.clone()]),
        None,
        false,
    )
    .dispatch(now())
    .unwrap();
    assert_eq!(manager.queue().current_progress(), 0);
    assert_eq!(manager.pending_count(), 0);
    assert!(manager.catchup_wait.is_none());
    let (lsn, mut third) = enqueue(&mut manager);
    assert_eq!(lsn, 3);
    ack(&mut manager, &slow, 3, 3);
    pending(&mut third);
    ack(&mut manager, &fast, 3, 3);
    succeeded(&mut third);
    for completion in &mut completions {
        assert_eq!(completion.try_recv(), Err(TryRecvError::Closed));
    }
    assert_eq!(all.try_recv(), Err(TryRecvError::Closed));
    assert_eq!(manager.pending_count(), 0);
}

fn process_ack_without_primary_notification(member: &SessionMember, value: Lsn) {
    let mut processor = member
        .session
        .update_ack(
            AckUpdate {
                replication_received: value,
                replication_applied: value,
                copy_received: 0,
                copy_applied: 0,
                process_copy: false,
                message_id: format!("unnotified-{value}"),
            },
            None,
        )
        .unwrap();
    let mut gates = 0;
    while let Some(handoff) = processor.next(now()) {
        processor = handoff.deliver(|action| match action {
            AckAction::ReplicationSends(batch) => batch.dispatch(),
            AckAction::CopyReplicationGate {
                copy_applied,
                replication_applied,
            } => {
                gates += 1;
                assert_eq!((copy_applied, replication_applied), (0, value));
            }
            _ => panic!("no primary callback or copy ACK owner was supplied"),
        });
    }
    assert_eq!(gates, 1);
    assert!(!member.session.is_processing());
}

#[test]
fn failed_configuration_and_idle_removal_are_atomic_and_close_reports_after_draining() {
    let (mut manager, mut completions) = held(1);
    manager.allow_multiple_quorum_set = false;
    let active = member(2);
    let idle = member(3);
    configure(
        &mut manager,
        set(2, std::slice::from_ref(&active)),
        None,
        true,
    )
    .dispatch(now())
    .unwrap();
    let (_lsn, mut second) = enqueue(&mut manager);
    manager
        .add_established_idle(EstablishedIdle {
            replica_id: idle.replica_id,
            session: idle.session.clone(),
            finish_copy_target: None,
        })
        .unwrap()
        .dispatch(now())
        .unwrap();
    let (mut all, effects) = manager.begin_catchup(CatchupMode::All).unwrap();
    effects.dispatch(now()).unwrap();
    assert_eq!(manager.queue().previous_config_catchup_lsn(), 1);
    process_ack_without_primary_notification(&idle, 3);
    assert!(matches!(
        manager.configure_established(EstablishedConfiguration {
            current: set(1, &[]),
            previous: None,
            catchup_configuration: false,
        }),
        Err(ManagerError::InvalidSessionProgress { replica_id: 3, .. })
    ));
    assert_eq!(manager.current.as_ref().unwrap().write_quorum, 2);
    assert!(manager.catchup_configuration);
    assert_eq!(manager.queue().previous_config_catchup_lsn(), 1);
    assert_eq!(manager.idle.len(), 1);
    process_ack_without_primary_notification(&active, 3);
    assert!(matches!(
        manager.remove_idle(3),
        Err(ManagerError::InvalidSessionProgress { replica_id: 2, .. })
    ));
    assert_eq!(manager.idle.len(), 1);
    assert!(matches!(
        manager.enqueue(
            Bytes::from_static(b"invalid"),
            BufferPolicy::RawSingle,
            limits()
        ),
        Err(ManagerError::InvalidSessionProgress { .. })
    ));
    assert_eq!(manager.queue().last_sequence_number(), 2);
    assert!(matches!(
        manager.close().dispatch(now()),
        Err(ManagerError::InvalidSessionProgress { replica_id: 2, .. })
    ));
    assert_eq!(manager.pending_count(), 0);
    assert_eq!(
        completions[0].try_recv(),
        Ok(Err(CompletionError::OperationCanceled))
    );
    assert_eq!(
        second.try_recv(),
        Ok(Err(CompletionError::OperationCanceled))
    );
    assert_eq!(all.try_recv(), Ok(Err(CompletionError::ObjectClosed)));
    manager.close().dispatch(now()).unwrap();
}

#[test]
fn out_of_range_ack_cannot_become_valid_by_admission_while_quorum_is_missing() {
    let (mut manager, mut completions) = held(1);
    let remote = member(2);
    configure(
        &mut manager,
        set(3, std::slice::from_ref(&remote)),
        None,
        false,
    )
    .dispatch(now())
    .unwrap();
    process_ack_without_primary_notification(&remote, 2);
    assert!(matches!(
        manager.enqueue(
            Bytes::from_static(b"must not assign"),
            BufferPolicy::RawSingle,
            limits(),
        ),
        Err(ManagerError::InvalidSessionProgress {
            replica_id: 2,
            received: 2,
            applied: 2,
            latest: 1,
        })
    ));
    assert_eq!(manager.queue().last_sequence_number(), 1);
    assert_eq!(manager.pending_count(), 1);
    pending(&mut completions[0]);
    assert!(manager.catchup_observation().unwrap().is_none());
    manager.close().dispatch(now()).unwrap();
    assert_eq!(
        completions[0].try_recv(),
        Ok(Err(CompletionError::OperationCanceled))
    );
}
