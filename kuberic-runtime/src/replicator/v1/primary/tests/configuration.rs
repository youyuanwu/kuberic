// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual primary/manager/session vectors derived from ReplicaManager.cpp:
// 1205-1779 and ReplicationSession.cpp:279-283 at the pin in primary.rs.

use super::*;
use crate::replicator::v1::replica_manager::ConfigurationError;

fn rig() -> Rig {
    rig_with_callback(|_| true)
}

fn rig_with_callback(callback: impl Fn(&PrimaryHandle) -> bool + Send + Sync + 'static) -> Rig {
    let clock = Clock::new();
    let sent = Arc::new(Mutex::new(Sent::default()));
    let target = Arc::new(Mutex::new(None::<PrimaryHandle>));
    let callback_target = target.clone();
    let recorded = sent.clone();
    let mut config = primary_config();
    config.send = Arc::new(move |intent| {
        recorded.lock().unwrap().replication.push(intent);
        let handle = callback_target.lock().unwrap().clone().unwrap();
        callback(&handle)
    });
    let primary = Primary::new(config, clock.clone()).unwrap();
    let handle = primary.handle();
    *target.lock().unwrap() = Some(handle.clone());
    handle
        .update_current_configuration(set(2, Vec::new()))
        .unwrap();
    Rig {
        primary,
        handle,
        clock,
        sent,
    }
}

fn replica(id: i64, progress: Option<Lsn>) -> ReplicaDescriptor {
    ReplicaDescriptor {
        route: route(id),
        remote_incarnation: id,
        progress,
        must_catchup: false,
    }
}

fn set(quorum: usize, replicas: Vec<ReplicaDescriptor>) -> ReplicaConfiguration {
    ReplicaConfiguration {
        write_quorum: quorum,
        replicas,
    }
}

fn session(rig: &Rig, id: i64) -> ReplicationSession {
    rig.handle.root().unwrap().state.lock().unwrap().bindings[&id]
        .session
        .clone()
}

fn current(rig: &Rig, ids: &[i64], quorum: usize) {
    rig.handle
        .update_current_configuration(set(
            quorum,
            ids.iter().map(|id| replica(*id, Some(0))).collect(),
        ))
        .unwrap();
}

fn replication_lsns(rig: &Rig) -> Vec<Lsn> {
    rig.sent
        .lock()
        .unwrap()
        .replication
        .iter()
        .filter_map(|intent| match &intent.kind {
            SendKind::Replication { operation, .. } => Some(operation.lsn()),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn descriptor_creation_opens_actual_session_and_bound_ack_completes_write() {
    let rig = rig();
    current(&rig, &[20], 2);
    let session = session(&rig, 20);
    assert_eq!(session.identity().remote_incarnation, 20);
    assert!(!session.is_promoted());
    let mut write = rig.enqueue();
    assert_eq!(replication_lsns(&rig), vec![1]);
    assert_eq!(write.completion.try_recv(), Err(TryRecvError::Empty));
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(write.completion.try_recv(), Ok(Ok(())));
}

#[tokio::test(start_paused = true)]
async fn new_member_seed_selects_actual_retained_replay_and_original_epoch() {
    let rig = rig();
    let mut first = rig.enqueue();
    let mut second = rig.enqueue();
    let mut third = rig.enqueue();
    rig.handle
        .update_current_configuration(set(2, vec![replica(20, Some(1))]))
        .unwrap();
    assert_eq!(replication_lsns(&rig), vec![2, 3]);
    assert_eq!(first.completion.try_recv(), Ok(Ok(())));
    assert_eq!(second.completion.try_recv(), Err(TryRecvError::Empty));
    assert!(
        rig.sent
            .lock()
            .unwrap()
            .replication
            .iter()
            .all(|intent| matches!(
        intent.kind, SendKind::Replication { original_epoch, sending_epoch, .. }
        if original_epoch == Epoch::new(1, 7) && sending_epoch == Epoch::new(1, 7)))
    );
    rig.handle.receive_ack(&ack(20, 3, 3, -1)).unwrap();
    assert_eq!(second.completion.try_recv(), Ok(Ok(())));
    assert_eq!(third.completion.try_recv(), Ok(Ok(())));
}

#[tokio::test(start_paused = true)]
async fn overlap_uses_one_new_session_and_one_replay_batch() {
    let rig = rig();
    let mut write = rig.enqueue();
    rig.handle
        .update_catchup_configuration(
            Some(set(2, vec![replica(20, Some(0))])),
            set(2, vec![replica(20, Some(0))]),
        )
        .unwrap();
    assert_eq!(replication_lsns(&rig), vec![1]);
    let manager = rig.primary.root.manager.lock().unwrap();
    assert!(manager.contains_session(20));
    drop(manager);
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(write.completion.try_recv(), Ok(Ok(())));
}

#[tokio::test(start_paused = true)]
async fn repeated_configuration_reuses_root_without_replay_or_ack_reset() {
    let rig = rig();
    current(&rig, &[20], 2);
    let root = session(&rig, 20);
    let mut first = rig.enqueue();
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(first.completion.try_recv(), Ok(Ok(())));
    let before = replication_lsns(&rig);
    rig.handle
        .update_current_configuration(set(2, vec![replica(20, None)]))
        .unwrap();
    current(&rig, &[20], 2);
    assert!(root.same_instance(&session(&rig, 20)));
    assert_eq!(root.apply_lsn(0), 1);
    assert_eq!(replication_lsns(&rig), before);
}

#[tokio::test(start_paused = true)]
async fn reused_id_does_not_rebind_changed_descriptor_endpoint_or_progress() {
    let rig = rig();
    current(&rig, &[20], 2);
    let original = session(&rig, 20);
    let mut descriptor = replica(20, Some(400));
    descriptor.route.remote_address = "replacement".into();
    descriptor.route.destination.incarnation = Bytes::from(vec![8; 16]);
    descriptor.remote_incarnation = 999;
    rig.handle
        .update_current_configuration(set(2, vec![descriptor.clone()]))
        .unwrap();
    assert!(original.same_instance(&session(&rig, 20)));
    assert_eq!(original.identity().remote_endpoint, "replica-20");
    assert_eq!(original.identity().remote_incarnation, 20);
    assert_eq!(original.apply_lsn(0), 0);
    let mut write = rig.enqueue();
    let replacement_ack = descriptor
        .route
        .reversed()
        .envelope(ack(20, 1, 1, -1).body.unwrap());
    assert!(rig.handle.receive_ack(&replacement_ack).is_err());
    assert_eq!(write.completion.try_recv(), Err(TryRecvError::Empty));
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(write.completion.try_recv(), Ok(Ok(())));
}

#[tokio::test(start_paused = true)]
async fn unknown_new_current_is_rejected_before_membership_or_sends() {
    let rig = rig();
    let mut write = rig.enqueue();
    assert!(matches!(
        rig.handle
            .update_current_configuration(set(2, vec![replica(20, None)])),
        Err(PrimaryError::UnknownCurrentProgress(20))
    ));
    assert!(rig.primary.root.state.lock().unwrap().bindings.is_empty());
    assert!(replication_lsns(&rig).is_empty());
    assert_eq!(write.completion.try_recv(), Err(TryRecvError::Empty));
    current(&rig, &[20], 2);
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(write.completion.try_recv(), Ok(Ok(())));
}

#[tokio::test(start_paused = true)]
async fn unknown_previous_only_can_be_created_but_cannot_supply_unknown_current() {
    let rig = rig();
    rig.handle
        .update_catchup_configuration(Some(set(2, vec![replica(20, None)])), set(2, Vec::new()))
        .unwrap();
    let root = session(&rig, 20);
    assert!(matches!(
        rig.handle
            .update_current_configuration(set(2, vec![replica(20, None)])),
        Err(PrimaryError::UnknownCurrentProgress(20))
    ));
    assert!(root.same_instance(&session(&rig, 20)));
    current(&rig, &[20], 2);
    assert!(root.same_instance(&session(&rig, 20)));
}

#[tokio::test(start_paused = true)]
async fn new_previous_only_unknown_progress_replays_pending_work_without_voting_for_cc() {
    let rig = rig();
    let mut write = rig.enqueue();
    let paused = rig
        .handle
        .update_catchup_configuration(Some(set(2, vec![replica(20, None)])), set(2, Vec::new()))
        .unwrap();
    assert!(paused.is_empty());
    assert_eq!(replication_lsns(&rig), vec![1]);
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(write.completion.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(
        rig.primary
            .root
            .manager
            .lock()
            .unwrap()
            .queue()
            .current_progress(),
        0
    );
}

#[tokio::test(start_paused = true)]
async fn missing_current_replay_is_atomic_but_previous_only_is_explicitly_paused() {
    let rig = rig();
    current(&rig, &[], 1);
    let mut write = rig.enqueue();
    assert_eq!(write.completion.try_recv(), Ok(Ok(())));
    assert!(
        matches!(rig.handle.update_current_configuration(set(2, vec![replica(20, Some(0))])),
        Err(PrimaryError::Manager(error)) if matches!(*error,
            ManagerError::Configuration(ConfigurationError::MissingCurrentReplay(20))))
    );
    assert!(rig.primary.root.state.lock().unwrap().bindings.is_empty());
    let paused = rig
        .handle
        .update_catchup_configuration(Some(set(2, vec![replica(20, Some(0))])), set(1, Vec::new()))
        .unwrap();
    assert_eq!(paused, vec![20]);
    assert_eq!(session(&rig, 20).retry_deadline(), None);
    assert!(replication_lsns(&rig).is_empty());
}

#[tokio::test(start_paused = true)]
async fn m3_frozen_previous_is_never_live_or_a_current_admission() {
    let rig = rig();
    current(&rig, &[], 1);
    let mut committed = rig.enqueue();
    assert_eq!(committed.completion.try_recv(), Ok(Ok(())));
    let mut frozen = replica(30, None);
    frozen.remote_incarnation = FROZEN_PREVIOUS_INCARNATION;
    assert_eq!(
        rig.handle
            .update_catchup_configuration(Some(set(2, vec![frozen.clone()])), set(1, Vec::new()),)
            .unwrap(),
        vec![30],
    );
    let old = session(&rig, 30);
    assert!(!old.transport_active());
    assert!(replication_lsns(&rig).is_empty());
    assert!(matches!(
        rig.handle
            .update_current_configuration(set(2, vec![frozen.clone()])),
        Err(PrimaryError::InvalidBinding(30)),
    ));
    assert!(matches!(
        rig.handle
            .update_current_configuration(set(2, vec![replica(30, Some(1))])),
        Err(PrimaryError::InvalidBinding(30)),
    ));
    assert!(old.same_instance(&session(&rig, 30)));
    assert!(!old.transport_active());
    let mut pending = rig.enqueue();
    assert!(matches!(
        rig.handle.receive_ack(&ack(30, 2, 2, -1)),
        Err(PrimaryError::InvalidBinding(30))
    ));
    assert_eq!(pending.completion.try_recv(), Err(TryRecvError::Empty));
    current(&rig, &[], 1);
    assert_eq!(pending.completion.try_recv(), Ok(Ok(())));
    assert!(matches!(
        rig.handle.receive_ack(&ack(30, 2, 2, -1)),
        Err(PrimaryError::UnknownSession(30))
    ));
}

#[tokio::test(start_paused = true)]
async fn current_only_update_retires_previous_session_and_its_retry_timer() {
    let mut rig = rig();
    rig.handle
        .update_catchup_configuration(
            Some(set(2, vec![replica(30, Some(0))])),
            set(2, vec![replica(20, Some(0))]),
        )
        .unwrap();
    let removed = session(&rig, 30);
    let mut write = rig.enqueue();
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(write.completion.try_recv(), Err(TryRecvError::Empty));
    rig.handle
        .update_current_configuration(set(2, vec![replica(20, None)]))
        .unwrap();
    assert_eq!(write.completion.try_recv(), Ok(Ok(())));
    assert_eq!(removed.retry_deadline(), None);
    assert!(
        !rig.primary
            .root
            .state
            .lock()
            .unwrap()
            .bindings
            .contains_key(&30)
    );
    assert!(matches!(
        rig.handle.receive_ack(&ack(30, 1, 1, -1)),
        Err(PrimaryError::UnknownSession(30))
    ));
    let before = rig.sent.lock().unwrap().replication.len();
    tokio::time::advance(Duration::from_secs(1)).await;
    rig.pending();
    assert_eq!(rig.sent.lock().unwrap().replication.len(), before);
}

#[tokio::test(start_paused = true)]
async fn existing_previous_configuration_can_contract_but_not_expand() {
    let rig = rig();
    rig.handle
        .update_catchup_configuration(
            Some(set(2, vec![replica(20, Some(0)), replica(30, Some(0))])),
            set(2, vec![replica(40, Some(0))]),
        )
        .unwrap();
    let kept = session(&rig, 20);
    rig.handle
        .update_catchup_configuration(
            Some(set(2, vec![replica(20, None)])),
            set(2, vec![replica(40, None)]),
        )
        .unwrap();
    assert!(kept.same_instance(&session(&rig, 20)));
    assert!(
        !rig.primary
            .root
            .state
            .lock()
            .unwrap()
            .bindings
            .contains_key(&30)
    );
    assert!(matches!(rig.handle.update_catchup_configuration(
        Some(set(2, vec![replica(20, None), replica(50, Some(0))])),
        set(2, vec![replica(40, None)])),
        Err(PrimaryError::Manager(error)) if matches!(*error,
            ManagerError::Configuration(ConfigurationError::PreviousConfigurationExpanded(50)))));
    assert!(
        !rig.primary
            .root
            .state
            .lock()
            .unwrap()
            .bindings
            .contains_key(&50)
    );
}

#[tokio::test(start_paused = true)]
async fn completed_idle_promotion_keeps_actual_build_and_sender_root() {
    let mut rig = rig();
    let build = rig.finish_build(20);
    let root = build.session();
    assert!(!root.is_promoted());
    rig.handle
        .update_current_configuration(set(2, vec![replica(20, None)]))
        .unwrap();
    assert!(root.is_promoted());
    assert!(root.same_instance(&session(&rig, 20)));
    assert!(build.result().unwrap().result.is_ok());
    assert!(matches!(
        rig.handle.remove_idle(20),
        Err(PrimaryError::Manager(_))
    ));
    let mut write = rig.enqueue();
    rig.handle.receive_ack(&ack(20, 1, 1, -100)).unwrap();
    assert_eq!(write.completion.try_recv(), Ok(Ok(())));
}

#[tokio::test(start_paused = true)]
async fn previous_and_idle_overlap_still_has_real_idle_membership_to_promote() {
    let mut rig = rig();
    let build = rig.finish_build(20);
    rig.handle
        .update_catchup_configuration(Some(set(2, vec![replica(20, None)])), set(2, Vec::new()))
        .unwrap();
    assert!(!build.session().is_promoted());
    assert!(!rig.primary.root.manager.lock().unwrap().is_idle(20));
    assert!(
        rig.primary
            .root
            .manager
            .lock()
            .unwrap()
            .has_idle_membership(20)
    );
    rig.handle
        .update_current_configuration(set(2, vec![replica(20, None)]))
        .unwrap();
    assert!(build.session().is_promoted());
    assert!(build.session().same_instance(&session(&rig, 20)));
    assert!(
        !rig.primary
            .root
            .manager
            .lock()
            .unwrap()
            .has_idle_membership(20)
    );
}

#[tokio::test(start_paused = true)]
async fn idle_promotion_resets_sender_averages_once_not_on_duplicate_configuration() {
    let mut rig = rig();
    let build = rig.finish_build(20);
    let _first = rig.enqueue();
    tokio::time::advance(Duration::from_secs(2)).await;
    rig.pending();
    assert_eq!(build.session().averages().0, Duration::from_secs(2));
    rig.handle
        .update_current_configuration(set(2, vec![replica(20, None)]))
        .unwrap();
    assert_eq!(build.session().averages(), (Duration::ZERO, Duration::ZERO));
    tokio::time::advance(Duration::from_secs(2)).await;
    rig.pending();
    let averages = build.session().averages();
    assert_eq!(averages.0, Duration::from_secs(4));
    rig.handle
        .update_current_configuration(set(2, vec![replica(20, None)]))
        .unwrap();
    assert_eq!(build.session().averages(), averages);
}

#[tokio::test(start_paused = true)]
async fn pending_provider_build_is_not_fabricated_success_by_session_promotion() {
    let mut rig = rig();
    let provider = Arc::new(Provider::new(0));
    let build = rig.begin(20, false, provider.clone());
    rig.handle.receive_ack(&ack(20, 0, 0, 0)).unwrap();
    rig.pending();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    rig.handle
        .update_current_configuration(set(2, vec![replica(20, None)]))
        .unwrap();
    assert!(build.session().is_promoted());
    assert!(build.result().is_none());
    provider.gate.add_permits(1);
    rig.pending();
    assert_eq!(rig.sent.lock().unwrap().copy.last(), Some(&(2, true)));
    assert!(build.result().is_none());
    rig.handle.close(Duration::ZERO).unwrap();
    rig.primary.run().await.unwrap().result.unwrap();
    assert!(build.result().is_none());
    build.cancel_for_removal().unwrap();
    rig.primary.drain_builds().await.unwrap();
    assert!(build.result().unwrap().result.is_err());
}

#[tokio::test(start_paused = true)]
async fn current_configuration_rechecks_quorum_and_catchup_without_new_ack() {
    let rig = rig();
    current(&rig, &[20, 30], 3);
    let mut write = rig.enqueue();
    let mut catchup = rig.handle.begin_catchup(CatchupMode::All).unwrap();
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(write.completion.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(catchup.try_recv(), Err(TryRecvError::Empty));
    rig.handle
        .update_current_configuration(set(2, vec![replica(20, None)]))
        .unwrap();
    assert_eq!(write.completion.try_recv(), Ok(Ok(())));
    assert_eq!(catchup.try_recv(), Ok(Ok(())));
}

#[tokio::test(start_paused = true)]
async fn catchup_marks_can_be_removed_by_current_configuration_without_new_ack() {
    let rig = rig();
    let _write = rig.enqueue();
    let mut marked = replica(30, Some(0));
    marked.must_catchup = true;
    rig.handle
        .update_catchup_configuration(None, set(2, vec![replica(20, Some(0)), marked]))
        .unwrap();
    let mut catchup = rig.handle.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(catchup.try_recv(), Err(TryRecvError::Empty));
    current(&rig, &[20, 30], 2);
    assert_eq!(catchup.try_recv(), Ok(Ok(())));
}

#[tokio::test(start_paused = true)]
async fn duplicate_member_rejection_cannot_promote_idle_or_send_candidate_replay() {
    let mut rig = rig();
    let build = rig.finish_build(20);
    let _write = rig.enqueue();
    let before = replication_lsns(&rig);
    assert!(matches!(rig.handle.update_current_configuration(set(2,
        vec![replica(20, None), replica(30, Some(0)), replica(30, Some(0))])),
        Err(PrimaryError::Manager(error)) if matches!(*error,
            ManagerError::Configuration(ConfigurationError::DuplicateReplica(30)))));
    assert!(!build.session().is_promoted());
    assert_eq!(replication_lsns(&rig), before);
    assert!(
        !rig.primary
            .root
            .state
            .lock()
            .unwrap()
            .bindings
            .contains_key(&30)
    );
    assert!(
        rig.primary
            .root
            .manager
            .lock()
            .unwrap()
            .has_idle_membership(20)
    );
}

#[tokio::test(start_paused = true)]
async fn invalid_progress_identity_routes_and_membership_are_failure_atomic() {
    let rig = rig();
    current(&rig, &[20], 2);
    let root = session(&rig, 20);
    let mut cases = Vec::new();
    for progress in [-1, Lsn::MAX, 1] {
        cases.push(replica(30, Some(progress)));
    }
    let mut invalid = replica(30, Some(0));
    invalid.remote_incarnation = -1;
    cases.push(invalid);
    let mut invalid = replica(30, Some(0));
    invalid.route.destination.partition = Bytes::from(vec![9; 16]);
    cases.push(invalid);
    let mut invalid = replica(30, Some(0));
    invalid.route.source.incarnation = Bytes::from(vec![9; 16]);
    cases.push(invalid);
    cases.push(replica(10, Some(0)));
    for invalid in cases {
        assert!(
            rig.handle
                .update_current_configuration(set(2, vec![invalid]))
                .is_err()
        );
        assert!(root.same_instance(&session(&rig, 20)));
        assert_eq!(rig.primary.root.state.lock().unwrap().bindings.len(), 1);
        assert!(replication_lsns(&rig).is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn marks_in_current_only_or_previous_are_rejected_without_installation() {
    let rig = rig();
    let mut marked = replica(20, Some(0));
    marked.must_catchup = true;
    assert!(
        rig.handle
            .update_current_configuration(set(2, vec![marked.clone()]))
            .is_err()
    );
    assert!(
        rig.handle
            .update_catchup_configuration(Some(set(2, vec![marked])), set(2, Vec::new()))
            .is_err()
    );
    assert!(rig.primary.root.state.lock().unwrap().bindings.is_empty());
}

#[tokio::test(start_paused = true)]
async fn zero_quorum_and_aliased_new_identities_are_rejected_atomically() {
    let rig = rig();
    let mut alias = replica(30, Some(0));
    alias.route.remote_address = route(20).remote_address;
    alias.remote_incarnation = 20;
    assert!(
        rig.handle
            .update_current_configuration(set(2, vec![replica(20, Some(0)), alias]))
            .is_err()
    );
    assert!(
        rig.handle
            .update_current_configuration(set(0, Vec::new()))
            .is_err()
    );
    assert!(
        rig.handle
            .update_catchup_configuration(
                Some(set(0, Vec::new())),
                set(2, vec![replica(20, Some(0))])
            )
            .is_err()
    );
    assert!(rig.primary.root.state.lock().unwrap().bindings.is_empty());
    assert!(replication_lsns(&rig).is_empty());
}

#[tokio::test(start_paused = true)]
async fn primary_only_current_quorum_cannot_include_remote_replicas() {
    let rig = rig();
    assert!(matches!(
        rig.handle
            .update_current_configuration(set(1, vec![replica(20, Some(0))])),
        Err(PrimaryError::InvalidCurrentQuorum)
    ));
    assert!(matches!(
        rig.handle
            .update_catchup_configuration(None, set(1, vec![replica(20, Some(0))])),
        Err(PrimaryError::InvalidCurrentQuorum)
    ));
    assert!(rig.primary.root.state.lock().unwrap().bindings.is_empty());
}

#[tokio::test(start_paused = true)]
async fn unmentioned_idle_keeps_its_build_root_and_does_not_become_active() {
    let mut rig = rig();
    let build = rig.finish_build(20);
    current(&rig, &[30], 2);
    assert!(build.session().same_instance(&session(&rig, 20)));
    assert!(!build.session().is_promoted());
    assert!(rig.primary.root.manager.lock().unwrap().is_idle(20));
    let mut write = rig.enqueue();
    rig.handle.receive_ack(&ack(20, 1, 1, 2)).unwrap();
    assert_eq!(write.completion.try_recv(), Err(TryRecvError::Empty));
    rig.handle.receive_ack(&ack(30, 1, 1, -1)).unwrap();
    assert_eq!(write.completion.try_recv(), Ok(Ok(())));
}

#[tokio::test(start_paused = true)]
async fn current_and_previous_distinct_sessions_both_constrain_normal_completion() {
    let rig = rig();
    rig.handle
        .update_catchup_configuration(
            Some(set(2, vec![replica(30, Some(0))])),
            set(2, vec![replica(20, Some(0))]),
        )
        .unwrap();
    let mut write = rig.enqueue();
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(write.completion.try_recv(), Err(TryRecvError::Empty));
    rig.handle.receive_ack(&ack(30, 1, 1, -1)).unwrap();
    assert_eq!(write.completion.try_recv(), Ok(Ok(())));
}

#[tokio::test(start_paused = true)]
async fn replay_callback_can_ack_new_session_before_update_returns() {
    let rig = rig_with_callback(|handle| {
        handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
        true
    });
    let mut write = rig.enqueue();
    current(&rig, &[20], 2);
    assert_eq!(write.completion.try_recv(), Ok(Ok(())));
}

#[tokio::test(start_paused = true)]
async fn replay_callback_can_reconfigure_and_retire_its_own_session() {
    let rig = rig_with_callback(|handle| {
        handle
            .update_current_configuration(set(1, Vec::new()))
            .unwrap();
        true
    });
    let mut write = rig.enqueue();
    current(&rig, &[20], 2);
    assert_eq!(write.completion.try_recv(), Ok(Ok(())));
    assert!(rig.primary.root.state.lock().unwrap().bindings.is_empty());
}

#[tokio::test(start_paused = true)]
async fn replay_callback_can_abort_and_drains_original_pending_write() {
    let mut rig = rig_with_callback(|handle| {
        handle.abort().unwrap();
        true
    });
    let mut write = rig.enqueue();
    current(&rig, &[20], 2);
    assert_eq!(
        write.completion.try_recv(),
        Ok(Err(CompletionError::OperationCanceled))
    );
    let end = rig.primary.run().await.unwrap();
    assert!(end.aborted);
    assert!(end.result.is_err());
}

#[tokio::test(start_paused = true)]
async fn primary_close_waits_for_selected_configuration_send_to_return() {
    let selected = Arc::new(Barrier::new(2));
    let released = Arc::new(Barrier::new(2));
    let signal = selected.clone();
    let wait = released.clone();
    let mut rig = rig_with_callback(move |_| {
        signal.wait();
        wait.wait();
        true
    });
    let mut write = rig.enqueue();
    let handle = rig.handle.clone();
    let update = std::thread::spawn(move || {
        handle
            .update_current_configuration(set(2, vec![replica(20, Some(0))]))
            .unwrap();
    });
    selected.wait();
    rig.handle.close(Duration::ZERO).unwrap();
    assert_eq!(
        write.completion.try_recv(),
        Ok(Err(CompletionError::OperationCanceled))
    );
    rig.pending();
    released.wait();
    update.join().unwrap();
    rig.primary.run().await.unwrap().result.unwrap();
}

#[tokio::test(start_paused = true)]
async fn configuration_wakes_pinned_driver_for_new_session_retry_deadline() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let calls = attempts.clone();
    let mut rig = rig_with_callback(move |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        false
    });
    let _write = rig.enqueue();
    let handle = rig.handle.clone();
    let mut run = Box::pin(rig.primary.run());
    assert!(run.as_mut().now_or_never().is_none());
    handle
        .update_current_configuration(set(2, vec![replica(20, Some(0))]))
        .unwrap();
    assert!(run.as_mut().now_or_never().is_none());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(run.as_mut().now_or_never().is_none());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    handle.close(Duration::ZERO).unwrap();
    run.await.unwrap().result.unwrap();
}

#[tokio::test(start_paused = true)]
async fn send_queue_full_during_configuration_is_retried_not_acknowledged() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let calls = attempts.clone();
    let mut rig = rig_with_callback(move |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        false
    });
    let mut write = rig.enqueue();
    current(&rig, &[20], 2);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    rig.pending();
    tokio::time::advance(Duration::from_secs(1)).await;
    rig.pending();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(write.completion.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test(start_paused = true)]
async fn late_ack_from_retired_identity_cannot_advance_recreated_id() {
    let rig = rig();
    current(&rig, &[20], 2);
    let original = session(&rig, 20);
    current(&rig, &[], 2);
    let mut replacement = replica(20, Some(0));
    replacement.route.destination.incarnation = Bytes::from(vec![9; 16]);
    replacement.route.remote_address = "replacement".into();
    replacement.remote_incarnation = 21;
    rig.handle
        .update_current_configuration(set(2, vec![replacement.clone()]))
        .unwrap();
    assert!(!original.same_instance(&session(&rig, 20)));
    let mut write = rig.enqueue();
    assert!(rig.handle.receive_ack(&ack(20, 1, 1, -1)).is_err());
    assert_eq!(write.completion.try_recv(), Err(TryRecvError::Empty));
    rig.handle
        .receive_ack(
            &replacement
                .route
                .reversed()
                .envelope(ack(20, 1, 1, -1).body.unwrap()),
        )
        .unwrap();
    assert_eq!(write.completion.try_recv(), Ok(Ok(())));
}

#[tokio::test(start_paused = true)]
async fn closing_and_abandoned_owners_reject_configuration() {
    let mut rig = rig();
    rig.handle.close(Duration::from_secs(1)).unwrap();
    assert!(matches!(
        rig.handle.update_current_configuration(set(1, Vec::new())),
        Err(PrimaryError::Closed)
    ));
    assert!(matches!(
        rig.handle
            .update_catchup_configuration(None, set(1, Vec::new())),
        Err(PrimaryError::Closed)
    ));
    rig.primary.run().await.unwrap().result.unwrap();
    let handle = rig.handle.clone();
    drop(rig);
    assert!(matches!(
        handle.update_current_configuration(set(1, Vec::new())),
        Err(PrimaryError::OwnerDropped)
    ));
}

#[tokio::test(start_paused = true)]
async fn actual_build_provider_fault_rejects_later_native_configuration() {
    let mut rig = rig();
    let mut provider = Provider::new(1);
    provider.failure = true;
    let build = rig.begin(20, false, Arc::new(provider));
    rig.handle.receive_ack(&ack(20, 0, 0, 0)).unwrap();
    rig.pending();
    assert!(build.result().unwrap().result.is_err());
    assert!(
        matches!(rig.handle.update_current_configuration(set(1, Vec::new())),
        Err(PrimaryError::Manager(error)) if matches!(*error, ManagerError::ReportedFault { .. }))
    );
}

#[test]
fn invalid_session_settings_are_rejected_without_constructing_panicking_sender() {
    for variant in 0..9 {
        let mut config = primary_config();
        match variant {
            0 => config.sender.initial_window = 0,
            1 => config.sender.max_window = 1,
            2 => config.sender.retry = Duration::ZERO,
            3 => config.sender.decay_factor = f64::NAN,
            4 => config.sender.decay_factor = -0.1,
            5 => config.sender.decay_interval = Duration::ZERO,
            6 => {
                config.sender.initial_window = usize::MAX / 2 + 1;
                config.sender.max_window = 0;
            }
            7 => config.sender.initial_window = 3,
            _ => config.sender.max_window = usize::MAX,
        }
        assert!(matches!(
            Primary::new(config, Clock::new()),
            Err(PrimaryError::InvalidSenderConfig)
        ));
    }
}
