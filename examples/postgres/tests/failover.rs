use kuberic_agent::store::AgentStore;
use kuberic_protocol::types::{AccessStatus, ReplicaRole, SwitchoverRequestId, TransitionKind};
use kuberic_runtime_internal::authority::AdmittedAuthority;
use kuberic_runtime_internal::effects::RuntimeEffectAction;
use postgres_replicated::testing::{
    PgPod, RecoveryStage, TestDataDir, native_configuration, native_identity,
};
use std::time::Duration;

#[derive(Clone, Copy)]
enum Failure {
    Promotion,
    Observation,
    Shutdown,
}

#[derive(Default)]
struct Case {
    planned: bool,
    cut: Option<RecoveryStage>,
    policy_cut: Option<RecoveryStage>,
    live_gap: bool,
    quorum_loss: bool,
    changed_session: bool,
    reopen_cut: bool,
    failure: Option<Failure>,
    joint_gap: bool,
    explicit_demote: bool,
    alternate_candidate: bool,
}

async fn exercise_primary_change(case: Case) {
    let Case {
        planned,
        cut,
        policy_cut,
        live_gap,
        quorum_loss,
        changed_session,
        reopen_cut,
        failure,
        joint_gap,
        explicit_demote,
        alternate_candidate,
    } = case;
    let root = TestDataDir::new("p5-elect");
    let source = PgPod::new(root.path().join("s"), native_identity(1, "source")).await;
    let fault_marker = root.path().join("helper-fault");
    let bin = match failure {
        Some(Failure::Promotion) => postgres_replicated::testing::wrapped_pg_bin(
            root.path(),
            "pg_ctl",
            &format!(
                "#!/bin/sh\nif test \"$1\" = promote; then echo 'injected promotion failure' >&2; exit 9; fi\nexec '{}/pg_ctl' \"$@\"\n",
                postgres_replicated::testing::find_pg_bin().display()
            ),
        ),
        Some(Failure::Observation) => postgres_replicated::testing::wrapped_pg_bin(
            root.path(),
            "pg_controldata",
            &format!(
                "#!/bin/sh\nif test -f '{}'; then echo malformed; exit 0; fi\nexec '{}/pg_controldata' \"$@\"\n",
                fault_marker.display(),
                postgres_replicated::testing::find_pg_bin().display()
            ),
        ),
        None | Some(Failure::Shutdown) => postgres_replicated::testing::find_pg_bin(),
    };
    let target = PgPod::with_bin(root.path().join("t"), native_identity(2, "target"), bin).await;
    let other = PgPod::new(root.path().join("o"), native_identity(3, "other")).await;
    source.singleton().await;
    let (sql, connection) = source
        .application
        .instance()
        .connect_application()
        .await
        .unwrap();
    sql.batch_execute("CREATE TABLE recovered(id int primary key)")
        .await
        .unwrap();
    let build = source.authorize(&target, "initial").await;
    source.build(&target, &build).await.unwrap();
    let build = source.authorize(&other, "other").await;
    source.build(&other, &build).await.unwrap();
    target.peer(&other).await;
    other.peer(&target).await;
    let mut previous = native_configuration(
        &[
            source.identity.clone(),
            target.identity.clone(),
            other.identity.clone(),
        ],
        0,
        2,
    );
    let accepted_previous = previous.clone();
    other.admit(previous.clone()).await;
    other
        .effect(RuntimeEffectAction::ChangeRole(
            ReplicaRole::ActiveSecondary,
        ))
        .await
        .unwrap();
    target.admit(previous.clone()).await;
    target
        .effect(RuntimeEffectAction::ChangeRole(
            ReplicaRole::ActiveSecondary,
        ))
        .await
        .unwrap();
    source.admit(previous.clone()).await;
    source
        .effect(RuntimeEffectAction::SetAccessStatus {
            read: AccessStatus::Granted,
            write: AccessStatus::Granted,
        })
        .await
        .unwrap();
    if joint_gap {
        let next = native_configuration(
            &[
                source.identity.clone(),
                target.identity.clone(),
                other.identity.clone(),
            ],
            0,
            3,
        );
        for pod in [&target, &other, &source] {
            pod.effect(RuntimeEffectAction::AdmitAuthority(Box::new(
                AdmittedAuthority {
                    local_identity: pod.identity.clone(),
                    previous_configuration: Some(previous.clone()),
                    current_configuration: next.clone(),
                    transition_kind: Some(TransitionKind::Failover),
                    switchover_handoff: None,
                    scale_up: None,
                    secondary_removal: None,
                },
            )))
            .await
            .unwrap();
        }
        source
            .effect(RuntimeEffectAction::SetAccessStatus {
                read: AccessStatus::Granted,
                write: AccessStatus::Granted,
            })
            .await
            .unwrap();
        previous = next;
    }
    drop(sql);
    connection.await.unwrap();
    let (sql, _) = source
        .application
        .instance()
        .connect_application()
        .await
        .unwrap();
    if !planned {
        request_replay_pause(&other).await;
    }
    let published_progress = source.runtime.snapshot().await.current_progress;
    if joint_gap {
        assert!(
            source
                .runtime
                .snapshot()
                .await
                .authority
                .unwrap()
                .previous_configuration
                .is_some()
        );
    }
    tokio::time::timeout(
        Duration::from_secs(10),
        sql.batch_execute("INSERT INTO recovered VALUES(1)"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        source.runtime.snapshot().await.current_progress,
        published_progress
    );
    if !planned {
        let (lagging, task) = admin(&other).await;
        assert_eq!(
            lagging
                .query_one("SELECT count(*) FROM recovered WHERE id=1", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            0
        );
        drop(lagging);
        task.await.unwrap().unwrap();
    }
    let (old_admin, old_admin_connection) = admin(&source).await;
    if let Some(stage) = policy_cut {
        postgres_replicated::access::PgAccessController::new(source.application.instance())
            .close_external()
            .await
            .unwrap();
        let gate = source.application.native_driver().pause_recovery(stage);
        {
            let grant = source.effect(RuntimeEffectAction::SetAccessStatus {
                read: AccessStatus::Granted,
                write: AccessStatus::Granted,
            });
            tokio::pin!(grant);
            tokio::select! {
                result = &mut grant => panic!("grant finished before {stage:?}: {result:?}"),
                entered = tokio::time::timeout(Duration::from_secs(10), gate.entered.notified()) => entered.unwrap(),
            }
        }
        gate.release.notify_one();
        assert!(
            source
                .application
                .instance()
                .connect_application()
                .await
                .is_err()
        );
    }
    let mut current = native_configuration(
        &[
            target.identity.clone(),
            source.identity.clone(),
            other.identity.clone(),
        ],
        0,
        previous.epoch.configuration_number + 1,
    );
    let handoff = if planned {
        source
            .effect(RuntimeEffectAction::PrepareSwitchover {
                preparation_generation: 1,
                request_id: SwitchoverRequestId::new("handoff"),
                source: source.identity.clone(),
                target: target.identity.clone(),
                starting_configuration_id: previous.configuration_id.clone(),
                starting_epoch: previous.epoch,
            })
            .await
            .unwrap();
        let handoff = source.store.load_state().await.unwrap().prepared_switchover;
        source
            .effect(RuntimeEffectAction::AdmitAuthority(Box::new(
                AdmittedAuthority {
                    local_identity: source.identity.clone(),
                    previous_configuration: Some(previous.clone()),
                    current_configuration: current.clone(),
                    transition_kind: Some(TransitionKind::PlannedSwitchover),
                    switchover_handoff: handoff.clone(),
                    scale_up: None,
                    secondary_removal: None,
                },
            )))
            .await
            .unwrap();
        if let Some(stage @ (RecoveryStage::SourceFenceIntent | RecoveryStage::SourceStopped)) = cut
        {
            cancel_role_at(&source, ReplicaRole::ActiveSecondary, stage).await;
        }
        if matches!(failure, Some(Failure::Shutdown)) {
            let processes = postgres_replicated::testing::ProcessProbe::postgres(
                source.application.instance().data_dir(),
            );
            processes.signal(rustix::process::Signal::QUIT);
            processes.assert_exited().await;
            assert!(
                source
                    .effect(RuntimeEffectAction::ChangeRole(
                        ReplicaRole::ActiveSecondary
                    ))
                    .await
                    .is_err()
            );
            assert!(!source.application.instance().is_running().await);
            assert_eq!(
                source.runtime.partition_report().await.reported_fault,
                Some(kuberic_protocol::types::FaultType::Permanent)
            );
            assert!(
                sql.simple_query("INSERT INTO recovered VALUES(99)")
                    .await
                    .is_err()
            );
            assert!(
                old_admin
                    .simple_query("INSERT INTO recovered VALUES(98)")
                    .await
                    .is_err()
            );
            assert!(
                target
                    .application
                    .instance()
                    .connect_application()
                    .await
                    .is_err()
            );
            return;
        }
        source
            .effect(RuntimeEffectAction::ChangeRole(
                ReplicaRole::ActiveSecondary,
            ))
            .await
            .unwrap();
        assert!(!source.application.instance().is_running().await);
        handoff
    } else {
        if explicit_demote {
            source
                .effect(RuntimeEffectAction::ChangeRole(ReplicaRole::None))
                .await
                .unwrap();
        } else if !live_gap {
            source.application.instance().stop().await.unwrap();
        }
        None
    };
    if !live_gap {
        assert!(
            sql.simple_query("INSERT INTO recovered VALUES(99)")
                .await
                .is_err()
        );
    }
    let transition = if planned {
        TransitionKind::PlannedSwitchover
    } else {
        TransitionKind::Failover
    };
    if joint_gap {
        previous = accepted_previous;
    }
    if alternate_candidate {
        let provisional = native_configuration(
            &[
                other.identity.clone(),
                source.identity.clone(),
                target.identity.clone(),
            ],
            0,
            current.epoch.configuration_number,
        );
        for pod in [&target, &other] {
            pod.effect(RuntimeEffectAction::AdmitAuthority(Box::new(
                AdmittedAuthority {
                    local_identity: pod.identity.clone(),
                    previous_configuration: Some(previous.clone()),
                    current_configuration: provisional.clone(),
                    transition_kind: Some(TransitionKind::Failover),
                    switchover_handoff: None,
                    scale_up: None,
                    secondary_removal: None,
                },
            )))
            .await
            .unwrap();
        }
        let rejected = other
            .runtime
            .primary_replicator()
            .await
            .unwrap()
            .change_role(provisional.epoch, ReplicaRole::Primary)
            .await;
        assert!(matches!(
            rejected,
            Err(kuberic_runtime::RuntimeError::ReconfigurationPending)
        ));
        assert!(
            other
                .application
                .instance()
                .connect_application()
                .await
                .is_err()
        );
        previous = provisional;
        current = native_configuration(
            &[
                target.identity.clone(),
                source.identity.clone(),
                other.identity.clone(),
            ],
            0,
            previous.epoch.configuration_number + 1,
        );
    }
    other
        .effect(RuntimeEffectAction::AdmitAuthority(Box::new(
            AdmittedAuthority {
                local_identity: other.identity.clone(),
                previous_configuration: Some(previous.clone()),
                current_configuration: current.clone(),
                transition_kind: Some(transition),
                switchover_handoff: handoff.clone(),
                scale_up: None,
                secondary_removal: None,
            },
        )))
        .await
        .unwrap();
    target
        .effect(RuntimeEffectAction::AdmitAuthority(Box::new(
            AdmittedAuthority {
                local_identity: target.identity.clone(),
                previous_configuration: Some(previous),
                current_configuration: current,
                transition_kind: Some(transition),
                switchover_handoff: handoff,
                scale_up: None,
                secondary_removal: None,
            },
        )))
        .await
        .unwrap();
    if let Some(stage) = cut.filter(|stage| {
        !matches!(
            stage,
            RecoveryStage::SourceFenceIntent | RecoveryStage::SourceStopped
        )
    }) {
        cancel_role_at(&target, ReplicaRole::Primary, stage).await;
        if reopen_cut {
            let saved = target.application.native_driver().durable_state().await;
            target.runtime.abort();
            drop(target);
            let service = std::sync::Arc::new(
                postgres_replicated::PgService::deferred(postgres_replicated::PgServiceConfig {
                    resource_uid: saved.identity.resource_uid.clone(),
                    application_root: root.path().join("t/application"),
                    pg_data: root.path().join("t/pgdata"),
                    pg_bin: postgres_replicated::testing::find_pg_bin(),
                    pg_port: postgres_replicated::testing::allocate_port().await,
                    replication_address: "http://127.0.0.1:1".into(),
                })
                .with_coordination_token("restart-test".into()),
            );
            let store = std::sync::Arc::new(
                kuberic_agent::sqlite_store::SqliteStore::open_existing(
                    kuberic_agent::sqlite_store::SqliteStore::metadata_database_path(
                        &root.path().join("t"),
                    ),
                    None,
                )
                .unwrap(),
            );
            let runtime = kuberic_agent::hosting::PodRuntime::new(
                saved.identity.replica.clone(),
                service.clone(),
                store.clone(),
            );
            runtime
                .bind_replica_session(
                    saved.identity.resource_uid.clone(),
                    kuberic_protocol::types::ProcessSessionId::new("reopened-session"),
                )
                .unwrap();
            let result = runtime
                .reconstruct(
                    kuberic_runtime::application::OpenMode::Existing,
                    ReplicaRole::Primary,
                    AccessStatus::ReconfigurationPending,
                    AccessStatus::ReconfigurationPending,
                    Some((ReplicaRole::Primary, false, false)),
                )
                .await;
            if stage != RecoveryStage::Ready {
                assert!(
                    result.is_err(),
                    "incomplete election must not reuse a retired session: {stage:?}"
                );
            }
            assert!(service.instance().connect_application().await.is_err());
            assert_ne!(
                runtime.partition_report().await.write_status,
                AccessStatus::Granted
            );
            assert!(store.load_state().await.unwrap().pending_effect.is_some());
            if result.is_ok() {
                let sequence = store.load_state().await.unwrap().next_effect_sequence;
                let grant = runtime
                    .apply_effect(kuberic_runtime_internal::effects::RuntimeEffect {
                        operation_id: kuberic_protocol::types::OperationId::new("reopened-grant"),
                        sequence,
                        action: RuntimeEffectAction::SetAccessStatus {
                            read: AccessStatus::Granted,
                            write: AccessStatus::Granted,
                        },
                    })
                    .await;
                assert!(grant.is_err());
                assert!(service.instance().connect_application().await.is_err());
            }
            runtime.abort();
            drop(old_admin);
            let _ = old_admin_connection.await;
            return;
        }
    }
    if changed_session {
        target
            .effect(RuntimeEffectAction::RegisterPeerSession {
                identity: other.identity.clone(),
                session: kuberic_protocol::types::ProcessSessionId::new("replaced-responder"),
            })
            .await
            .unwrap();
    }
    if matches!(failure, Some(Failure::Observation)) {
        std::fs::write(&fault_marker, b"").unwrap();
    }
    if quorum_loss {
        other.application.instance().stop().await.unwrap();
    }
    let activated = if live_gap {
        let initial = target
            .application
            .native_driver()
            .pause_recovery(RecoveryStage::InitialRound);
        let role = target.effect(RuntimeEffectAction::ChangeRole(ReplicaRole::Primary));
        tokio::pin!(role);
        tokio::select! {
            result = &mut role => panic!("election passed initial cut: {result:?}"),
            entered = tokio::time::timeout(Duration::from_secs(10), initial.entered.notified()) => entered.unwrap(),
        }
        tokio::time::timeout(
            Duration::from_secs(10),
            sql.batch_execute("INSERT INTO recovered VALUES(3)"),
        )
        .await
        .unwrap()
        .unwrap();
        for pod in [&target, &other] {
            request_replay_pause(pod).await;
        }
        let accepted_unknown = tokio::time::timeout(
            Duration::from_millis(250),
            sql.batch_execute("INSERT INTO recovered VALUES(4)"),
        )
        .await;
        assert!(
            accepted_unknown.is_err(),
            "already accepted work must wait for synchronous replay"
        );
        let drained = target
            .application
            .native_driver()
            .pause_recovery(RecoveryStage::ReceiversDrained);
        initial.release.notify_one();
        tokio::select! {
            result = &mut role => panic!("election passed drain cut: {result:?}"),
            entered = tokio::time::timeout(Duration::from_secs(15), drained.entered.notified()) => entered.unwrap(),
        }
        let (late, _) = source
            .application
            .instance()
            .connect_application()
            .await
            .unwrap();
        let unknown = tokio::time::timeout(
            Duration::from_millis(250),
            late.batch_execute("INSERT INTO recovered VALUES(5)"),
        )
        .await;
        assert!(
            unknown.is_err(),
            "no supported synchronous commit may finish after receiver drain"
        );
        source.application.instance().stop().await.unwrap();
        assert!(
            sql.simple_query("INSERT INTO recovered VALUES(99)")
                .await
                .is_err()
        );
        drained.release.notify_one();
        role.await
    } else {
        target
            .effect(RuntimeEffectAction::ChangeRole(ReplicaRole::Primary))
            .await
    };
    assert!(
        old_admin
            .simple_query("INSERT INTO recovered VALUES(98)")
            .await
            .is_err()
    );
    drop(old_admin);
    let _ = old_admin_connection.await.unwrap();
    if failure.is_some()
        || changed_session
        || quorum_loss
        || policy_cut.is_some_and(|stage| stage != RecoveryStage::PolicyAccepted)
    {
        assert!(activated.is_err());
        assert!(
            target
                .application
                .instance()
                .connect_application()
                .await
                .is_err()
        );
        assert_ne!(
            target.runtime.snapshot().await.write_status,
            AccessStatus::Granted
        );
        if let Some(failure) = failure {
            let expected = match failure {
                Failure::Promotion => kuberic_protocol::types::FaultType::Permanent,
                Failure::Observation => kuberic_protocol::types::FaultType::Permanent,
                Failure::Shutdown => kuberic_protocol::types::FaultType::Permanent,
            };
            assert_eq!(
                target.runtime.partition_report().await.reported_fault,
                Some(expected)
            );
        }
        return;
    }
    activated.unwrap();
    assert!(
        target
            .application
            .instance()
            .connect_application()
            .await
            .is_err()
    );
    let (admin, admin_connection) = admin(&target).await;
    assert_eq!(
        admin
            .query_one("SELECT count(*) FROM recovered WHERE id=1", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    assert_eq!(
        admin
            .query_one("SELECT count(*) FROM recovered WHERE id=99", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        admin
            .query_one("SELECT count(*) FROM recovered WHERE id=98", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    if live_gap {
        assert_eq!(
            admin
                .query_one("SELECT count(*) FROM recovered WHERE id=3", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            1
        );
    }
    target
        .effect(RuntimeEffectAction::SetAccessStatus {
            read: AccessStatus::Granted,
            write: AccessStatus::Granted,
        })
        .await
        .unwrap();
    let (sql, _) = target
        .application
        .instance()
        .connect_application()
        .await
        .unwrap();
    sql.batch_execute("INSERT INTO recovered VALUES(2)")
        .await
        .unwrap();
    if alternate_candidate {
        other.refresh().await;
        assert_eq!(other.runtime.partition_report().await.reported_fault, None);
    }
    if planned && cut.is_none() {
        other.application.instance().stop().await.unwrap();
        assert!(
            target
                .effect(RuntimeEffectAction::RefreshApplicationProgress)
                .await
                .is_err()
        );
        assert_ne!(
            target.runtime.partition_report().await.write_status,
            AccessStatus::Granted
        );
        assert!(
            sql.simple_query("INSERT INTO recovered VALUES(77)")
                .await
                .is_err()
        );
        assert!(
            admin
                .simple_query("INSERT INTO recovered VALUES(78)")
                .await
                .is_err()
        );
        let (check, task) = self::admin(&target).await;
        assert_eq!(
            check
                .query_one("SELECT count(*) FROM recovered WHERE id IN (77,78)", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            0
        );
        assert_eq!(
            check
                .query_one("SELECT count(*) FROM recovered WHERE id=2", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            1
        );
        drop(check);
        task.await.unwrap().unwrap();
    }
    drop(admin);
    let _ = admin_connection.await.unwrap();
    if explicit_demote {
        let reopened = source.reopen().await;
        assert!(!reopened.application.instance().is_running().await);
        assert!(
            reopened
                .application
                .instance()
                .connect_application()
                .await
                .is_err()
        );
    }
}

async fn admin(
    pod: &PgPod,
) -> (
    tokio_postgres::Client,
    tokio::task::JoinHandle<std::result::Result<(), tokio_postgres::Error>>,
) {
    let (client, connection) = tokio_postgres::connect(
        &pod.application
            .instance()
            .connection_string()
            .replace("dbname=postgres", "dbname=kuberic"),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap();
    (client, tokio::spawn(connection))
}

async fn request_replay_pause(pod: &PgPod) {
    let (client, task) = pod.application.instance().connect().await.unwrap();
    client
        .simple_query("SELECT pg_wal_replay_pause()")
        .await
        .unwrap();
    assert!(
        client
            .query_one("SELECT pg_is_wal_replay_paused()", &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    drop(client);
    task.await.unwrap();
}

async fn cancel_role_at(pod: &PgPod, role: ReplicaRole, stage: RecoveryStage) {
    let gate = pod.application.native_driver().pause_recovery(stage);
    {
        let change = pod.effect(RuntimeEffectAction::ChangeRole(role));
        tokio::pin!(change);
        tokio::select! {
            result = &mut change => panic!("role finished before {stage:?}: {result:?}"),
            entered = tokio::time::timeout(Duration::from_secs(30), gate.entered.notified()) => entered.unwrap(),
        }
    }
    gate.release.notify_one();
    assert!(
        pod.store
            .load_state()
            .await
            .unwrap()
            .pending_effect
            .is_some()
    );
    assert_ne!(
        pod.runtime.partition_report().await.write_status,
        AccessStatus::Granted
    );
    assert!(
        pod.application
            .instance()
            .connect_application()
            .await
            .is_err()
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn acknowledged_row_is_replayed_before_candidate_primary_callback_completes() {
    exercise_primary_change(Case::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn planned_switchover_requires_source_shutdown_before_target_writes() {
    exercise_primary_change(Case {
        planned: true,
        ..Default::default()
    })
    .await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn cancelled_election_callbacks_resume_only_their_durable_recovery_cut() {
    for stage in [
        RecoveryStage::InitialRound,
        RecoveryStage::ReceiversDrained,
        RecoveryStage::FinalRound,
        RecoveryStage::Promoted,
        RecoveryStage::Ready,
    ] {
        exercise_primary_change(Case {
            cut: Some(stage),
            ..Default::default()
        })
        .await;
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn cancelled_source_demotion_replays_the_stopped_receipt_before_handoff() {
    for stage in [
        RecoveryStage::SourceFenceIntent,
        RecoveryStage::SourceStopped,
    ] {
        exercise_primary_change(Case {
            planned: true,
            cut: Some(stage),
            ..Default::default()
        })
        .await;
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn interrupted_policy_acceptance_allows_only_the_matched_generation() {
    for stage in [
        RecoveryStage::PolicyInvalidated,
        RecoveryStage::PolicyApplied,
        RecoveryStage::PolicyAccepted,
    ] {
        exercise_primary_change(Case {
            policy_cut: Some(stage),
            ..Default::default()
        })
        .await;
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn final_round_includes_between_round_commit_and_draining_revokes_old_acknowledgements() {
    exercise_primary_change(Case {
        live_gap: true,
        ..Default::default()
    })
    .await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn insufficient_exact_responders_keep_the_sf_role_and_partition_write_closed() {
    exercise_primary_change(Case {
        quorum_loss: true,
        ..Default::default()
    })
    .await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn stale_required_responder_session_cannot_activate_candidate() {
    exercise_primary_change(Case {
        changed_session: true,
        ..Default::default()
    })
    .await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn fresh_session_reopen_at_each_election_cut_remains_write_closed() {
    for stage in [
        RecoveryStage::InitialRound,
        RecoveryStage::ReceiversDrained,
        RecoveryStage::FinalRound,
        RecoveryStage::Promoted,
        RecoveryStage::Ready,
    ] {
        exercise_primary_change(Case {
            cut: Some(stage),
            reopen_cut: true,
            ..Default::default()
        })
        .await;
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn promotion_and_observation_failures_stay_closed_with_classified_faults() {
    for failure in [Failure::Promotion, Failure::Observation] {
        exercise_primary_change(Case {
            failure: Some(failure),
            ..Default::default()
        })
        .await;
    }
    exercise_primary_change(Case {
        planned: true,
        failure: Some(Failure::Shutdown),
        ..Default::default()
    })
    .await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn pc_cc_policy_change_and_report_gap_preserve_the_between_round_acknowledgement() {
    exercise_primary_change(Case {
        joint_gap: true,
        live_gap: true,
        ..Default::default()
    })
    .await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn explicit_demotion_fences_retained_clients_and_former_primary_restart() {
    exercise_primary_change(Case {
        explicit_demote: true,
        ..Default::default()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_accepted_policy_cannot_activate_a_durably_built_candidate() {
    let root = TestDataDir::new("p5-missing");
    let source = PgPod::new(root.path().join("s"), native_identity(1, "source")).await;
    let target = PgPod::new(root.path().join("t"), native_identity(2, "target")).await;
    source.singleton().await;
    let build = source.authorize(&target, "missing-policy").await;
    source.build(&target, &build).await.unwrap();
    let previous = native_configuration(&[source.identity.clone(), target.identity.clone()], 0, 2);
    target.admit(previous.clone()).await;
    target
        .effect(RuntimeEffectAction::ChangeRole(
            ReplicaRole::ActiveSecondary,
        ))
        .await
        .unwrap();
    source.admit(previous.clone()).await;
    source.application.instance().stop().await.unwrap();
    let current = native_configuration(&[target.identity.clone(), source.identity.clone()], 0, 3);
    target
        .effect(RuntimeEffectAction::AdmitAuthority(Box::new(
            AdmittedAuthority {
                local_identity: target.identity.clone(),
                previous_configuration: Some(previous),
                current_configuration: current,
                transition_kind: Some(TransitionKind::Failover),
                switchover_handoff: None,
                scale_up: None,
                secondary_removal: None,
            },
        )))
        .await
        .unwrap();
    let error = target
        .effect(RuntimeEffectAction::ChangeRole(ReplicaRole::Primary))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("missing accepted PostgreSQL synchronous policy"),
        "{error}"
    );
    assert_ne!(
        target.runtime.partition_report().await.write_status,
        AccessStatus::Granted
    );
    assert!(
        target
            .application
            .instance()
            .connect_application()
            .await
            .is_err()
    );
    let (admin, _) = target.application.instance().connect().await.unwrap();
    assert!(
        admin
            .query_one("SELECT pg_is_in_recovery()", &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn refused_candidate_can_follow_the_exact_higher_epoch_winner_without_old_receipt_poisoning()
{
    exercise_primary_change(Case {
        alternate_candidate: true,
        ..Default::default()
    })
    .await;
}
