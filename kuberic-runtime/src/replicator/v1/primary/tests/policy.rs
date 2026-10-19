// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-owner vectors derived from ReplicaManager.cpp:716-778,1832-2076
// and ReplicationSession.cpp:127-283 at the pin in primary.rs.

use super::*;
use crate::replicator::v1::replica_manager::ConfigurationError;

fn policy() -> QueuePolicyConfig {
    QueuePolicyConfig {
        trace_interval: Duration::from_secs(1),
        idle_max_lag: Duration::from_secs(2),
        restart_idle: false,
        restart_active: false,
        idle_usage_percent: 85,
        active_usage_percent: 90,
        active_min_age: Duration::from_secs(3),
        extra_active: 0,
    }
}

fn rig() -> Rig {
    let clock = Clock::new();
    let sent = Arc::new(Mutex::new(Sent::default()));
    let mut config = primary_config();
    config.queue.max_capacity = 8;
    config.queue_policy = policy();
    config.send = callbacks(&sent).replication;
    let primary = Primary::new(config, clock.clone()).unwrap();
    let handle = primary.handle();
    Rig {
        primary,
        handle,
        clock,
        sent,
    }
}

fn pending_build(rig: &mut Rig, id: i64) -> BuildIdleHandle {
    let build = rig.begin(id, false, Arc::new(Provider::new(1)));
    rig.handle.receive_ack(&ack(id, 0, 0, 0)).unwrap();
    rig.pending();
    assert_eq!(build.finish_copy_target().unwrap(), Some(0));
    build
}

fn queue(rig: &Rig) -> (Lsn, Lsn, u32) {
    let manager = rig.primary.root.manager.lock().unwrap();
    (
        manager.queue().current_progress(),
        manager.queue().first_lsn_in_replication_queue(),
        manager.queue().utilization_percent(),
    )
}

fn faults(rig: &Rig) -> usize {
    rig.sent
        .lock()
        .unwrap()
        .replication
        .iter()
        .filter(|intent| matches!(intent.kind, SendKind::InduceFault { .. }))
        .count()
}

#[tokio::test(start_paused = true)]
async fn periodic_freshness_uses_committed_not_issued_and_strict_elapsed_boundary() {
    let mut rig = rig();
    rig.active(&[20], 2);
    let build = pending_build(&mut rig, 30);
    let _writes: Vec<_> = (0..4).map(|_| rig.enqueue()).collect();
    rig.handle.receive_ack(&ack(20, 4, 3, -1)).unwrap();
    assert_eq!(queue(&rig), (3, 1, 50));
    for _ in 0..2 {
        tokio::time::advance(Duration::from_secs(1)).await;
        rig.pending();
    }
    assert_eq!(build.finish_copy_target().unwrap(), Some(0));
    tokio::time::advance(Duration::from_secs(1)).await;
    rig.pending();
    assert_eq!(build.finish_copy_target().unwrap(), Some(3));
    assert!(!build.session().idle_faulted());
    assert!(build.result().is_none());
    rig.handle.abort().unwrap();
}

#[tokio::test(start_paused = true)]
async fn pressure_retargets_to_issued_before_fault_and_rechecks_freed_capacity() {
    let mut rig = rig();
    rig.active(&[20], 2);
    let build = pending_build(&mut rig, 30);
    let _writes: Vec<_> = (0..6).map(|_| rig.enqueue()).collect();
    rig.handle.receive_ack(&ack(20, 6, 6, -1)).unwrap();
    rig.handle.receive_ack(&ack(30, 6, 0, 0)).unwrap();
    assert_eq!(queue(&rig), (6, 1, 75));
    let mut config = policy();
    config.restart_idle = true;
    config.idle_usage_percent = 50;
    rig.handle.update_queue_policy(config).unwrap();
    let seventh = rig.enqueue();
    assert_eq!(seventh.lsn, 7);
    assert_eq!(build.finish_copy_target().unwrap(), Some(7));
    assert_eq!(queue(&rig), (6, 7, 12));
    assert!(!build.session().idle_faulted());
    assert_eq!(faults(&rig), 0);
    rig.handle.abort().unwrap();
}

#[tokio::test(start_paused = true)]
async fn idle_fault_closes_sender_before_reclaim_and_defers_notification_until_both_copy_sentinels()
{
    let mut rig = rig();
    let active = rig.active(&[20], 2);
    let build = pending_build(&mut rig, 30);
    let _writes: Vec<_> = (0..6).map(|_| rig.enqueue()).collect();
    rig.handle.receive_ack(&ack(20, 6, 6, -1)).unwrap();
    let mut config = policy();
    config.restart_idle = true;
    rig.handle.update_queue_policy(config).unwrap();
    let _seventh = rig.enqueue();
    let session = build.session();
    assert!(session.idle_faulted());
    assert_eq!(session.retry_deadline(), None);
    assert_eq!(session.fault_deadline(), None);
    assert_eq!(queue(&rig), (6, 7, 12));
    assert!(build.result().is_none());
    rig.handle.receive_ack(&ack(30, 7, 7, 2)).unwrap();
    assert_eq!(
        session.progress(0).applied,
        0,
        "faulted sender ignores late ACKs"
    );
    assert_eq!(
        session.fault_deadline(),
        None,
        "ordinary terminal number is not the -1 sentinel"
    );
    let mut receive_only = ack(30, 7, 7, -1);
    let Some(wire::envelope::Body::ReplicationAck(body)) = receive_only.body.as_mut() else {
        unreachable!()
    };
    body.copy_applied = 2;
    rig.handle.receive_ack(&receive_only).unwrap();
    assert_eq!(session.fault_deadline(), None);
    rig.handle.receive_ack(&ack(30, 7, 7, -1)).unwrap();
    assert_eq!(session.fault_deadline(), Some(Duration::from_secs(1)));
    rig.pending();
    for expected in 1..=2 {
        tokio::time::advance(Duration::from_secs(1)).await;
        rig.pending();
        assert_eq!(faults(&rig), expected);
    }
    let promotions = rig.handle.configure_established(
        EstablishedConfiguration {
            current: ReplicaSet {
                write_quorum: 2,
                members: vec![SessionMember {
                    replica_id: 30,
                    session: session.clone(),
                    must_catchup: false,
                }],
            },
            previous: None,
            catchup_configuration: false,
        },
        vec![route(30)],
    );
    assert!(matches!(promotions, Err(PrimaryError::Manager(error))
        if matches!(error.as_ref(), ManagerError::Configuration(ConfigurationError::FaultedIdlePromotion(30)))));
    let current = || ReplicaSet {
        write_quorum: 2,
        members: vec![SessionMember {
            replica_id: 20,
            session: active[0].clone(),
            must_catchup: false,
        }],
    };
    rig.handle
        .configure_established(
            EstablishedConfiguration {
                current: current(),
                previous: Some(ReplicaSet {
                    write_quorum: 2,
                    members: vec![SessionMember {
                        replica_id: 30,
                        session: session.clone(),
                        must_catchup: false,
                    }],
                }),
                catchup_configuration: true,
            },
            vec![route(20), route(30)],
        )
        .unwrap();
    assert!(
        !session.is_promoted(),
        "PC overlap is not idle-to-active promotion"
    );
    rig.handle
        .configure_established(
            EstablishedConfiguration {
                current: current(),
                previous: None,
                catchup_configuration: false,
            },
            vec![route(20)],
        )
        .unwrap();
    rig.handle.remove_idle(30).unwrap();
    assert_eq!(session.fault_deadline(), None);
    tokio::time::advance(Duration::from_secs(2)).await;
    rig.pending();
    assert_eq!(faults(&rig), 2);
    rig.handle.abort().unwrap();
}

#[tokio::test(start_paused = true)]
async fn already_completed_idle_cannot_be_disabled_or_faulted_by_pressure() {
    let mut rig = rig();
    rig.active(&[20], 2);
    let build = rig.finish_build(30);
    let _writes: Vec<_> = (0..6).map(|_| rig.enqueue()).collect();
    rig.handle.receive_ack(&ack(20, 6, 6, -1)).unwrap();
    let mut config = policy();
    config.restart_idle = true;
    rig.handle.update_queue_policy(config).unwrap();
    let _seventh = rig.enqueue();
    assert!(!build.session().idle_faulted());
    assert!(build.result().unwrap().result.is_ok());
    assert_eq!(queue(&rig), (6, 1, 87));
    rig.handle.abort().unwrap();
}

#[tokio::test(start_paused = true)]
async fn rejected_admission_still_runs_pressure_without_consuming_an_lsn() {
    let mut rig = rig();
    rig.active(&[20], 2);
    let build = pending_build(&mut rig, 30);
    let _writes: Vec<_> = (0..8).map(|_| rig.enqueue()).collect();
    rig.handle.receive_ack(&ack(20, 8, 8, -1)).unwrap();
    let mut config = policy();
    config.restart_idle = true;
    rig.handle.update_queue_policy(config).unwrap();
    assert!(
        matches!(rig.handle.enqueue(Bytes::new(), BufferPolicy::RawSingle, limits()),
        Err(PrimaryError::Manager(error)) if matches!(error.as_ref(), ManagerError::Admission(
            super::super::super::replication_queue_manager::QueueAdmissionError::Queue(
                super::super::super::operation_queue::EnqueueError::Full))))
    );
    assert!(build.session().idle_faulted());
    assert_eq!(queue(&rig), (8, 9, 0));
    let next = rig.enqueue();
    assert_eq!(next.lsn, 9);
    rig.handle.abort().unwrap();
}

#[tokio::test(start_paused = true)]
async fn active_fault_preserves_quorum_all_and_live_mustcatchup_while_releasing_retention() {
    let mut rig = rig();
    let sessions = rig.active(&[20, 30], 2);
    let _writes: Vec<_> = (0..6).map(|_| rig.enqueue()).collect();
    rig.handle.receive_ack(&ack(20, 6, 6, -1)).unwrap();
    rig.handle
        .configure_established(
            EstablishedConfiguration {
                current: ReplicaSet {
                    write_quorum: 2,
                    members: vec![
                        SessionMember {
                            replica_id: 20,
                            session: sessions[0].clone(),
                            must_catchup: false,
                        },
                        SessionMember {
                            replica_id: 30,
                            session: sessions[1].clone(),
                            must_catchup: true,
                        },
                    ],
                },
                previous: None,
                catchup_configuration: true,
            },
            vec![route(20), route(30)],
        )
        .unwrap();
    let mut catchup = rig.handle.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    rig.pending();
    tokio::time::advance(Duration::from_secs(2)).await;
    rig.pending();
    assert!(sessions[1].averages().0 > sessions[0].averages().0);
    let mut config = policy();
    config.restart_active = true;
    config.active_usage_percent = 80;
    rig.handle.update_queue_policy(config).unwrap();
    let _seventh = rig.enqueue();
    assert!(
        !sessions[1].active_faulted(),
        "age guard protects a recent burst"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    rig.pending();
    let _eighth = rig.enqueue();
    assert!(sessions[1].active_faulted());
    assert_eq!(queue(&rig), (6, 7, 25));
    assert_eq!(sessions[1].retry_deadline(), None);
    let view = rig
        .primary
        .root
        .manager
        .lock()
        .unwrap()
        .catchup_observation()
        .unwrap()
        .unwrap();
    assert_eq!(
        (view.quorum, view.all, view.minimum_must_catchup),
        (6, 0, Some(0))
    );
    assert_eq!(catchup.try_recv(), Err(TryRecvError::Empty));
    rig.handle.receive_ack(&ack(30, 8, 8, -1)).unwrap();
    assert_eq!(sessions[1].progress(0).applied, 0);
    assert_eq!(catchup.try_recv(), Err(TryRecvError::Empty));
    tokio::time::advance(Duration::from_secs(1)).await;
    rig.pending();
    assert_eq!(faults(&rig), 1);
    rig.handle.abort().unwrap();
    assert_eq!(sessions[1].fault_deadline(), None);
    assert_eq!(catchup.await.unwrap(), Err(CompletionError::ObjectClosed));
    let end = rig.primary.run().await.unwrap();
    assert!(end.aborted);
}

#[tokio::test(start_paused = true)]
async fn active_policy_requires_spare_members_quorum_progress_and_latency_outlier() {
    for (quorum, extra, acknowledge_fast, threshold) in [
        (1, 0, false, 50),
        (4, 0, true, 50),
        (3, 0, true, 50),
        (2, 1, true, 50),
        (2, 0, false, 50),
        (2, 0, true, 101),
    ] {
        let mut rig = rig();
        let sessions = rig.active(&[20, 30], quorum);
        let _writes: Vec<_> = (0..7).map(|_| rig.enqueue()).collect();
        if acknowledge_fast {
            rig.handle.receive_ack(&ack(20, 7, 7, -1)).unwrap();
        }
        rig.pending();
        tokio::time::advance(Duration::from_secs(4)).await;
        rig.pending();
        let mut config = policy();
        config.restart_active = true;
        config.active_usage_percent = threshold;
        config.extra_active = extra;
        rig.handle.update_queue_policy(config).unwrap();
        let _eighth = rig.enqueue();
        assert!(sessions.iter().all(|session| !session.active_faulted()));
        rig.handle.abort().unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn private_dynamic_settings_and_terminal_admission_are_explicit() {
    let mut config = primary_config();
    config.queue_policy.trace_interval = Duration::ZERO;
    assert!(matches!(
        Primary::new(config, Clock::new()),
        Err(PrimaryError::InvalidQueuePolicy)
    ));
    let mut rig = rig();
    let mut updated = policy();
    updated.idle_max_lag = Duration::ZERO;
    assert!(matches!(
        rig.handle.update_queue_policy(updated),
        Err(PrimaryError::InvalidQueuePolicy)
    ));
    let volatile = QueuePolicyConfig::for_persisted(false);
    assert!(!volatile.restart_idle && !volatile.restart_active);
    let persisted = QueuePolicyConfig::for_persisted(true);
    assert!(persisted.restart_idle && persisted.restart_active);
    assert_eq!(persisted.interval(), Duration::from_secs(30));
    rig.handle.close(Duration::ZERO).unwrap();
    assert!(matches!(
        rig.handle.update_queue_policy(policy()),
        Err(PrimaryError::Closed)
    ));
    assert!(rig.primary.run().await.unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn outlier_uses_fastest_remote_quorum_population_deviation_and_strict_comparison() {
    for (slow_ms, faulted) in [(1700, false), (1701, true)] {
        let mut rig = rig();
        let sessions = rig.active(&[20, 30, 40], 3);
        let _writes: Vec<_> = (0..7).map(|_| rig.enqueue()).collect();
        tokio::time::advance(Duration::from_millis(1100)).await;
        rig.handle.receive_ack(&ack(20, 6, 6, -1)).unwrap();
        tokio::time::advance(Duration::from_millis(400)).await;
        rig.handle.receive_ack(&ack(30, 6, 6, -1)).unwrap();
        tokio::time::advance(Duration::from_millis(slow_ms - 1500)).await;
        rig.handle.receive_ack(&ack(40, 1, 0, -1)).unwrap();
        assert_eq!(sessions[0].averages().0, Duration::from_millis(1100));
        assert_eq!(sessions[1].averages().0, Duration::from_millis(1500));
        assert_eq!(sessions[2].averages().0, Duration::from_millis(slow_ms));
        assert_eq!(queue(&rig), (6, 2, 75));
        let mut config = policy();
        config.restart_active = true;
        config.active_min_age = Duration::ZERO;
        config.active_usage_percent = 75;
        let (effects, result) = rig
            .primary
            .root
            .manager
            .lock()
            .unwrap()
            .apply_queue_pressure(config, rig.clock.now());
        effects.dispatch(rig.clock.now()).unwrap();
        result.unwrap();
        assert_eq!(sessions[2].active_faulted(), faulted);
        rig.handle.abort().unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn duration_outlier_must_also_be_the_least_receive_progress_replica() {
    let mut rig = rig();
    let sessions = rig.active(&[20, 30, 40], 2);
    let _writes: Vec<_> = (0..7).map(|_| rig.enqueue()).collect();
    rig.handle.receive_ack(&ack(20, 7, 7, -1)).unwrap();
    tokio::time::advance(Duration::from_secs(2)).await;
    rig.handle.receive_ack(&ack(40, 2, 0, -1)).unwrap();
    assert!(sessions[2].averages().0 > sessions[1].averages().0);
    let mut config = policy();
    config.restart_active = true;
    config.active_min_age = Duration::ZERO;
    config.active_usage_percent = 80;
    let (effects, result) = rig
        .primary
        .root
        .manager
        .lock()
        .unwrap()
        .apply_queue_pressure(config, rig.clock.now());
    effects.dispatch(rig.clock.now()).unwrap();
    result.unwrap();
    assert!(sessions.iter().all(|session| !session.active_faulted()));
    rig.handle.abort().unwrap();
}

#[tokio::test(start_paused = true)]
async fn missing_copy_owner_reports_primary_failure_without_hiding_an_admitted_write() {
    let mut rig = rig();
    rig.active(&[20], 2);
    let idle = rig.session(30, callbacks(&rig.sent).replication);
    rig.primary
        .root
        .manager
        .lock()
        .unwrap()
        .add_established_idle(crate::replicator::v1::replica_manager::EstablishedIdle {
            replica_id: 30,
            session: idle,
            finish_copy_target: None,
        })
        .unwrap()
        .dispatch(rig.clock.now())
        .unwrap();
    let mut config = policy();
    config.restart_idle = true;
    config.idle_usage_percent = 0;
    rig.handle.update_queue_policy(config).unwrap();
    let admission = rig.enqueue();
    assert_eq!(
        admission.lsn, 1,
        "policy failure must not hide the assigned LSN"
    );
    assert!(
        matches!(rig.handle.enqueue(Bytes::new(), BufferPolicy::RawSingle, limits()),
        Err(PrimaryError::Manager(error)) if matches!(error.as_ref(),
            ManagerError::BuildProgress(message) if message.contains("no copy owner")))
    );
    assert!(matches!(rig.primary.run().await,
        Err(PrimaryError::Manager(error)) if matches!(error.as_ref(),
            ManagerError::BuildProgress(message) if message.contains("no copy owner"))));
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
    rig.handle.abort().unwrap();
    assert!(rig.primary.run().await.unwrap().result.is_err());
}

#[tokio::test(start_paused = true)]
async fn periodic_callback_resamples_dynamic_intervals_without_restarting_the_existing_tick() {
    let mut rig = rig();
    rig.active(&[20], 2);
    let build = pending_build(&mut rig, 30);
    let _writes: Vec<_> = (0..4).map(|_| rig.enqueue()).collect();
    rig.handle.receive_ack(&ack(20, 4, 3, -1)).unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    rig.pending();
    let mut config = policy();
    config.idle_max_lag = Duration::from_millis(500);
    config.trace_interval = Duration::from_millis(100);
    rig.handle.update_queue_policy(config).unwrap();
    tokio::time::advance(Duration::from_millis(500)).await;
    rig.pending();
    assert_eq!(build.finish_copy_target().unwrap(), Some(0));
    tokio::time::advance(Duration::from_millis(500)).await;
    rig.pending();
    assert_eq!(build.finish_copy_target().unwrap(), Some(3));
    assert_eq!(
        rig.primary.root.state.lock().unwrap().policy_deadline,
        Duration::from_millis(2100)
    );
    rig.handle.abort().unwrap();
}

#[tokio::test(start_paused = true)]
async fn selected_fault_notification_can_reenter_actual_primary_close_and_drain_its_activity() {
    let mut rig = rig();
    let fast = rig.session(20, callbacks(&rig.sent).replication);
    let handle = rig.handle.clone();
    let sent = rig.sent.clone();
    let slow = rig.session(
        30,
        Arc::new(move |intent| {
            if matches!(intent.kind, SendKind::InduceFault { .. }) {
                handle.close(Duration::ZERO).unwrap();
            }
            sent.lock().unwrap().replication.push(intent);
            true
        }),
    );
    rig.configure(
        vec![
            SessionMember {
                replica_id: 20,
                session: fast,
                must_catchup: false,
            },
            SessionMember {
                replica_id: 30,
                session: slow.clone(),
                must_catchup: false,
            },
        ],
        2,
    );
    let _writes: Vec<_> = (0..7).map(|_| rig.enqueue()).collect();
    rig.handle.receive_ack(&ack(20, 7, 7, -1)).unwrap();
    rig.pending();
    tokio::time::advance(Duration::from_secs(4)).await;
    rig.pending();
    let mut config = policy();
    config.restart_active = true;
    config.active_usage_percent = 80;
    rig.handle.update_queue_policy(config).unwrap();
    let _eighth = rig.enqueue();
    assert!(slow.active_faulted());
    tokio::time::advance(Duration::from_secs(1)).await;
    let end = rig.primary.run().await.unwrap();
    assert!(end.result.is_ok());
    assert_eq!(faults(&rig), 1);
    assert_eq!(slow.fault_deadline(), None);
}
