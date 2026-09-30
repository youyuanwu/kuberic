use std::time::Duration;

use kuberic_protocol::types::AccessStatus;
use kuberic_runtime_internal::effects::RuntimeEffectAction;
use postgres_replicated::{
    build::{PgBuildMethod, PgBuildStage, PgLineage, decode, encode},
    native::PgNativeObserver,
    testing::{PgPod, TestDataDir, native_identity},
};

#[test_log::test(tokio::test)]
async fn former_primary_rejoins_by_rewind_or_explicit_fresh_fallback() {
    for fallback in [false, true] {
        let root = TestDataDir::new("nr");
        let bin = if fallback {
            postgres_replicated::testing::wrapped_pg_bin(
                root.path(),
                "pg_rewind",
                "#!/bin/sh\nprintf 'missing retained WAL for rewind' >&2\nexit 1\n",
            )
        } else {
            postgres_replicated::testing::find_pg_bin()
        };
        let former =
            PgPod::with_bin(root.path().join("f"), native_identity(1, "former"), bin).await;
        let successor = PgPod::new(root.path().join("s"), native_identity(2, "successor")).await;
        former.singleton().await;
        write_source(&former).await;
        let build = former.authorize(&successor, "initial").await;
        former.build(&successor, &build).await.unwrap();
        former
            .effect(RuntimeEffectAction::SetAccessStatus {
                read: AccessStatus::ReconfigurationPending,
                write: AccessStatus::ReconfigurationPending,
            })
            .await
            .unwrap();
        former.application.instance().stop().await.unwrap();
        let successor = successor.promoted_fixture().await;
        successor
            .admit(postgres_replicated::testing::native_configuration(
                std::slice::from_ref(&successor.identity),
                0,
                3,
            ))
            .await;
        successor
            .effect(RuntimeEffectAction::ChangeRole(
                kuberic_protocol::types::ReplicaRole::Primary,
            ))
            .await
            .unwrap();
        let authority = successor.authorize(&former, "rejoin").await;
        successor.build(&former, &authority).await.unwrap();
        assert_data(&former).await;
        let progress = former
            .application
            .native_driver()
            .durable_state()
            .await
            .native_build
            .unwrap();
        assert_eq!(
            progress.method,
            if fallback {
                PgBuildMethod::Fresh
            } else {
                PgBuildMethod::Rewind
            }
        );
        assert!(progress.request.lineage.timeline > 1);
        former.refresh().await;
        assert!(former.runtime.snapshot().await.builds[0].completed);
    }
}

#[tokio::test]
async fn agent_dispatch_admits_and_builds_through_exact_native_route() {
    let root = TestDataDir::new("nw");
    let source = PgPod::new(root.path().join("s"), native_identity(1, "source")).await;
    let mut target = PgPod::new(root.path().join("t"), native_identity(2, "target")).await;
    source.singleton().await;
    write_source(&source).await;
    use kuberic_runtime::PrimaryReplicator;
    let error = source
        .application
        .native_driver()
        .build_replica(kuberic_runtime::replicator::ReplicaInformation::new(
            kuberic_protocol::types::OperationId::new("wire-native"),
            target.identity.clone(),
            String::new(),
        ))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("missing exact target replication endpoint"),
        "{error}"
    );
    assert!(
        source
            .application
            .native_driver()
            .durable_state()
            .await
            .outbound_builds
            .is_empty()
    );
    let control = target.start_control().await;
    source
        .dispatch_native(&target, control, "wire-native")
        .await;
    target.refresh().await;
    assert_data(&target).await;
    assert_eq!(target.runtime.snapshot().await.builds.len(), 1);
}

#[tokio::test]
async fn replacement_and_retired_sessions_cannot_publish_old_completion() {
    let root = TestDataDir::new("ns");
    let source = PgPod::new(root.path().join("s"), native_identity(1, "source")).await;
    let old = PgPod::new(root.path().join("old"), native_identity(2, "old")).await;
    source.singleton().await;
    write_source(&source).await;
    let authority = source.authorize(&old, "old-build").await;
    source.build(&old, &authority).await.unwrap();
    let progress = old
        .application
        .native_driver()
        .durable_state()
        .await
        .native_build
        .unwrap();
    let replacement = PgPod::new(root.path().join("new"), native_identity(2, "replacement")).await;
    assert!(replacement.inject(&progress.request).await.is_err());
    let replacement_authority = source.authorize(&replacement, "replacement-build").await;
    source
        .build(&replacement, &replacement_authority)
        .await
        .unwrap();
    assert_data(&replacement).await;
    let old = old.reopen().await;
    old.peer(&source).await;
    assert!(old.inject(&progress.request).await.is_err());
    old.refresh().await;
    assert!(old.runtime.snapshot().await.builds.is_empty());
    source.peer(&old).await;
    source.refresh().await;
    assert!(
        !source
            .runtime
            .snapshot()
            .await
            .builds
            .iter()
            .any(|b| b.authority.build_id == authority.build_id)
    );
    let request = replacement
        .application
        .native_driver()
        .durable_state()
        .await
        .native_build
        .unwrap()
        .request;
    replacement
        .effect(RuntimeEffectAction::RetireBuild(
            replacement_authority.build_id.clone(),
        ))
        .await
        .unwrap();
    assert!(replacement.inject(&request).await.is_err());
    replacement.refresh().await;
    assert!(replacement.runtime.snapshot().await.builds.is_empty());
}

#[tokio::test]
async fn delayed_completion_cannot_publish_after_target_session_replacement() {
    let root = TestDataDir::new("ndelay");
    let source = PgPod::new(root.path().join("s"), native_identity(1, "source")).await;
    let target = PgPod::new(root.path().join("t"), native_identity(2, "target")).await;
    source.singleton().await;
    write_source(&source).await;
    let authority = source.authorize(&target, "delayed").await;
    let gate = target
        .application
        .native_driver()
        .pause_build(PgBuildStage::Complete);
    let work = source.build(&target, &authority);
    tokio::pin!(work);
    tokio::select! {
        result = &mut work => panic!("completed before publication gate: {result:?}"),
        entered = tokio::time::timeout(Duration::from_secs(30), gate.entered.notified()) => { entered.unwrap(); }
    }
    source
        .effect(RuntimeEffectAction::RegisterPeerSession {
            identity: target.identity.clone(),
            session: kuberic_protocol::types::ProcessSessionId::new("replacement-process"),
        })
        .await
        .unwrap();
    gate.release.notify_one();
    assert!(work.await.is_err());
    source.refresh().await;
    assert!(source.runtime.snapshot().await.builds.is_empty());
}

#[tokio::test]
async fn cancelling_owned_basebackup_reaps_helpers_and_rebuilds_partial_stage() {
    let root = TestDataDir::new("nh");
    let marker = root.path().join("backup.pid");
    let bin = postgres_replicated::testing::wrapped_pg_bin(
        root.path(),
        "pg_basebackup",
        &format!(
            "#!/bin/sh\nprintf '%s' $$ > '{}'\nsleep 60\n",
            marker.display()
        ),
    );
    let source = PgPod::new(root.path().join("s"), native_identity(1, "source")).await;
    let target = PgPod::with_bin(root.path().join("t"), native_identity(2, "target"), bin).await;
    source.singleton().await;
    write_source(&source).await;
    let authority = source.authorize(&target, "cancel-helper").await;
    {
        let work = source.build(&target, &authority);
        tokio::pin!(work);
        tokio::select! {
            result = &mut work => panic!("backup exited before cancellation: {result:?}"),
            _ = async {
                tokio::time::timeout(Duration::from_secs(15), async {
                    while !marker.exists() { tokio::time::sleep(Duration::from_millis(20)).await; }
                }).await.unwrap();
            } => {}
        }
        let pid = std::fs::read_to_string(&marker).unwrap().parse().unwrap();
        let helper = postgres_replicated::testing::ProcessProbe::process(pid);
        let children = postgres_replicated::testing::ProcessProbe::descendants(pid);
        assert_eq!(
            target
                .application
                .native_driver()
                .durable_state()
                .await
                .native_build
                .unwrap()
                .stage,
            PgBuildStage::Copying
        );
        target.runtime.abort();
        assert!(work.await.is_err());
        helper.assert_reaped();
        children.assert_reaped();
    }
    let target = target.reopen().await;
    let authority = source.authorize(&target, "cancel-helper").await;
    source.build(&target, &authority).await.unwrap();
    assert_data(&target).await;
}

#[tokio::test]
async fn incompatible_real_timeline_is_rejected_without_replacing_data() {
    let root = TestDataDir::new("ntli");
    let source = PgPod::new(root.path().join("s"), native_identity(1, "source")).await;
    let target = PgPod::new(root.path().join("t"), native_identity(2, "target")).await;
    source.singleton().await;
    write_source(&source).await;
    let authority = source.authorize(&target, "initial").await;
    source.build(&target, &authority).await.unwrap();
    let target = target.promoted_fixture().await;
    let before = target
        .application
        .instance()
        .control_identity()
        .await
        .unwrap();
    assert!(before.1 > 1);
    let authority = source.authorize(&target, "wrong-timeline").await;
    let error = source.build(&target, &authority).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("incompatible PostgreSQL system identity/timeline"),
        "{error}"
    );
    assert_eq!(
        target
            .application
            .instance()
            .control_identity()
            .await
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn source_restart_rebinds_streaming_without_reusing_old_sessions() {
    let root = TestDataDir::new("nsrc");
    let source = PgPod::new(root.path().join("s"), native_identity(1, "source")).await;
    let target = PgPod::new(root.path().join("t"), native_identity(2, "target")).await;
    source.singleton().await;
    write_source(&source).await;
    let authority = source.authorize(&target, "restart-source").await;
    source.build(&target, &authority).await.unwrap();
    let old_request = target
        .application
        .native_driver()
        .durable_state()
        .await
        .native_build
        .unwrap()
        .request;
    let source = source.reopen().await;
    target.peer(&source).await;
    assert!(target.inject(&old_request).await.is_err());
    source.refresh().await;
    assert!(source.runtime.snapshot().await.builds.is_empty());
    let restored = source.authorize(&target, "restart-source").await;
    assert_eq!(restored, authority);
    source.build(&target, &restored).await.unwrap();
    assert_data(&target).await;
    assert_ne!(
        old_request.source_session,
        target
            .application
            .native_driver()
            .durable_state()
            .await
            .native_build
            .unwrap()
            .request
            .source_session
    );
}

#[tokio::test]
async fn real_receive_and_flush_without_replay_do_not_complete_frozen_boundary() {
    let root = TestDataDir::new("nbound");
    let source = PgPod::new(root.path().join("s"), native_identity(1, "source")).await;
    let target = PgPod::new(root.path().join("t"), native_identity(2, "target")).await;
    source.singleton().await;
    write_source(&source).await;
    let build = source.authorize(&target, "first").await;
    source.build(&target, &build).await.unwrap();
    let (control, _) = target.application.instance().connect().await.unwrap();
    control
        .simple_query("SELECT pg_wal_replay_pause()")
        .await
        .unwrap();
    let (sql, _) = source
        .application
        .instance()
        .connect_application()
        .await
        .unwrap();
    sql.batch_execute("INSERT INTO build_receipts VALUES (3, 'frozen')")
        .await
        .unwrap();
    let authority = source.authorize(&target, "frozen").await;
    let boundary = authority.replication_boundary_lsn;
    let observer = PgNativeObserver::new(target.application.instance().clone());
    let mut progress = target
        .application
        .native_driver()
        .durable_state()
        .await
        .native_build
        .unwrap();
    progress.request.authority = authority;
    progress.stage = PgBuildStage::Recovering;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            progress.evidence = observer.snapshot().await.unwrap().evidence;
            if progress.evidence.as_ref().unwrap().flush_lsn >= boundary {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(progress.evidence.as_ref().unwrap().received_lsn.unwrap() >= boundary);
    assert!(progress.evidence.as_ref().unwrap().replay_lsn.unwrap() < boundary);
    assert!(!progress.recovered());
    progress.stage = PgBuildStage::Complete;
    assert!(progress.validate().is_err());
    control
        .simple_query("SELECT pg_wal_replay_resume()")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            progress.evidence = observer.snapshot().await.unwrap().evidence;
            if progress.recovered() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        progress.evidence.as_ref().unwrap().replay_lsn,
        Some(boundary)
    );
    progress.validate().unwrap();
    let bytes = encode(&progress).unwrap();
    assert_eq!(
        decode::<postgres_replicated::build::PgBuildProgress>(&bytes).unwrap(),
        progress
    );
    let json = String::from_utf8(bytes).unwrap();
    for malformed in [
        json.replacen("\"sequence\":", "\"sequence\":1,\"sequence\":", 1),
        json.replacen("\"stage\":\"Complete\",", "", 1),
        json.replacen("{", "{\"unknown\":1,", 1),
        format!(" {json}"),
    ] {
        assert!(
            decode::<postgres_replicated::build::PgBuildProgress>(malformed.as_bytes()).is_err()
        );
    }
    progress.evidence.as_mut().unwrap().timeline_id += 1;
    assert!(!progress.recovered());
    assert!(progress.validate().is_err());
}

async fn write_source(source: &PgPod) {
    let (sql, _) = source
        .application
        .instance()
        .connect_application()
        .await
        .unwrap();
    sql.batch_execute("CREATE TABLE build_receipts(id int PRIMARY KEY, value text); INSERT INTO build_receipts VALUES (1, 'native-copy'), (2, 'durable-boundary')")
        .await.unwrap();
}

#[tokio::test]
async fn admitted_standby_restart_and_build_retirement_preserve_readable_data() {
    use kuberic_protocol::types::ReplicaRole;
    use kuberic_runtime::application::OpenMode;
    let root = TestDataDir::new("sf-restart");
    let source = PgPod::new(root.path().join("s"), native_identity(1, "source")).await;
    let target = PgPod::new(root.path().join("t"), native_identity(2, "target")).await;
    source.singleton().await;
    write_source(&source).await;
    let build = source.authorize(&target, "accepted-build").await;
    source.build(&target, &build).await.unwrap();
    let configuration = postgres_replicated::testing::native_configuration(
        &[source.identity.clone(), target.identity.clone()],
        0,
        2,
    );
    target.admit(configuration.clone()).await;
    target
        .effect(RuntimeEffectAction::ChangeRole(
            ReplicaRole::ActiveSecondary,
        ))
        .await
        .unwrap();
    target
        .effect(RuntimeEffectAction::SetAccessStatus {
            read: AccessStatus::Granted,
            write: AccessStatus::NotPrimary,
        })
        .await
        .unwrap();
    source.admit(configuration).await;
    target
        .effect(RuntimeEffectAction::RetireBuild(build.build_id.clone()))
        .await
        .unwrap();
    source
        .effect(RuntimeEffectAction::RetireBuild(build.build_id.clone()))
        .await
        .unwrap();
    assert!(target.application.instance().is_running().await);
    let (reader, _) = target
        .application
        .instance()
        .connect_application()
        .await
        .unwrap();
    assert_eq!(
        reader
            .query("SELECT id FROM build_receipts", &[])
            .await
            .unwrap()
            .len(),
        2
    );
    drop(reader);

    let target = target.reopen().await;
    target
        .runtime
        .reconstruct(
            OpenMode::Existing,
            ReplicaRole::ActiveSecondary,
            AccessStatus::Granted,
            AccessStatus::NotPrimary,
            None,
        )
        .await
        .unwrap();
    assert!(!target.application.instance().is_running().await);
    target.peer(&source).await;
    target.refresh().await;
    source.peer(&target).await;
    assert!(target.runtime.snapshot().await.builds.is_empty());
    let (reader, _) = target
        .application
        .instance()
        .connect_application()
        .await
        .unwrap();
    assert_eq!(
        reader
            .query("SELECT id FROM build_receipts", &[])
            .await
            .unwrap()
            .len(),
        2
    );
    source
        .effect(RuntimeEffectAction::SetAccessStatus {
            read: AccessStatus::Granted,
            write: AccessStatus::Granted,
        })
        .await
        .unwrap();
    let (writer, _) = source
        .application
        .instance()
        .connect_application()
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        writer.batch_execute("INSERT INTO build_receipts VALUES (3, 'fresh-session')"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        reader
            .query("SELECT id FROM build_receipts", &[])
            .await
            .unwrap()
            .len(),
        3
    );
}

async fn assert_data(target: &PgPod) {
    let (sql, _) = target.application.instance().connect().await.unwrap();
    assert!(
        sql.query_one("SELECT pg_is_in_recovery()", &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let connection = format!(
        "host={} port={} dbname=kuberic",
        target.application.instance().data_dir().display(),
        target.application.instance().port()
    );
    let (sql, connection) = tokio_postgres::connect(&connection, tokio_postgres::NoTls)
        .await
        .unwrap();
    let task = tokio::spawn(connection);
    let rows = sql
        .query("SELECT id, value FROM build_receipts ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<_, i32>(0), 1);
    assert_eq!(rows[1].get::<_, String>(1), "durable-boundary");
    drop(sql);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn fresh_native_build_requires_durable_replay_and_exact_lineage() {
    let root = TestDataDir::new("nb");
    let source = PgPod::new(root.path().join("s"), native_identity(1, "s")).await;
    let target = PgPod::new(root.path().join("t"), native_identity(2, "t")).await;
    source.singleton().await;
    write_source(&source).await;
    let authority = source.authorize(&target, "fresh").await;
    assert!(authority.replication_boundary_lsn > 0);
    source.build(&target, &authority).await.unwrap();
    target.refresh().await;
    assert_data(&target).await;
    let source_state = source.application.native_driver().durable_state().await;
    let target_state = target.application.native_driver().durable_state().await;
    assert_eq!(
        source_state.system_identifier,
        target_state.system_identifier
    );
    assert_eq!(source_state.timeline_id, target_state.timeline_id);
    assert_eq!(
        source_state.timeline_history_digest,
        target_state.timeline_history_digest
    );
    let report = target.runtime.snapshot().await;
    assert_eq!(report.builds.len(), 1);
    assert!(report.builds[0].completed);
    assert!(target_state.replay_lsn.unwrap() >= authority.replication_boundary_lsn);
    assert_ne!(report.read_status, AccessStatus::Granted);
    assert!(
        target
            .application
            .instance()
            .connect_application()
            .await
            .is_err()
    );
    let configuration = postgres_replicated::testing::native_configuration(
        &[source.identity.clone(), target.identity.clone()],
        0,
        2,
    );
    target.admit(configuration.clone()).await;
    target.runtime.cancel_configuration_work().await.unwrap();
    target
        .effect(RuntimeEffectAction::ChangeRole(
            kuberic_protocol::types::ReplicaRole::ActiveSecondary,
        ))
        .await
        .unwrap();
    target
        .effect(RuntimeEffectAction::SetAccessStatus {
            read: AccessStatus::Granted,
            write: AccessStatus::NotPrimary,
        })
        .await
        .unwrap();
    let (reader, _) = target
        .application
        .instance()
        .connect_application()
        .await
        .unwrap();
    assert_eq!(
        reader
            .query("SELECT id FROM build_receipts", &[])
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        reader
            .batch_execute("INSERT INTO build_receipts VALUES (99, 'forbidden')")
            .await
            .unwrap_err()
            .as_db_error()
            .unwrap()
            .code(),
        &tokio_postgres::error::SqlState::READ_ONLY_SQL_TRANSACTION
    );
    source.admit(configuration).await;
    source
        .effect(RuntimeEffectAction::WaitForCatchup)
        .await
        .unwrap();
    source.refresh().await;
    assert!(source.runtime.snapshot().await.catch_up_complete);
    source
        .effect(RuntimeEffectAction::SetAccessStatus {
            read: AccessStatus::Granted,
            write: AccessStatus::Granted,
        })
        .await
        .unwrap();
    let (writer, _) = source
        .application
        .instance()
        .connect_application()
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        writer.batch_execute("INSERT INTO build_receipts VALUES (3, 'remote-apply')"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        reader
            .query_one("SELECT value FROM build_receipts WHERE id = 3", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "remote-apply"
    );
}

#[tokio::test]
async fn interrupted_native_stages_reopen_with_fresh_sessions() {
    for stage in [
        PgBuildStage::Intent,
        PgBuildStage::Installed,
        PgBuildStage::Recovering,
        PgBuildStage::Complete,
    ] {
        let root = TestDataDir::new("nc");
        let source = PgPod::new(root.path().join("s"), native_identity(1, "s")).await;
        let target = PgPod::new(root.path().join("t"), native_identity(2, "t")).await;
        source.singleton().await;
        write_source(&source).await;
        let authority = source.authorize(&target, "crash").await;
        let gate = target.application.native_driver().pause_build(stage);
        let before = target
            .application
            .native_driver()
            .durable_state()
            .await
            .system_identifier;
        let old_request = {
            let build = source.build(&target, &authority);
            tokio::pin!(build);
            tokio::select! {
                result = &mut build => panic!("build completed before {stage:?}: {result:?}"),
                entered = tokio::time::timeout(Duration::from_secs(30), gate.entered.notified()) => { entered.unwrap(); }
            }
            let durable = target.application.native_driver().durable_state().await;
            assert_eq!(durable.native_build.as_ref().unwrap().stage, stage);
            assert!(
                target.runtime.snapshot().await.builds.is_empty(),
                "publication preceded durable completion release"
            );
            if stage == PgBuildStage::Intent {
                let (system, _) = target
                    .application
                    .instance()
                    .control_identity()
                    .await
                    .unwrap();
                assert_eq!(Some(system), before);
            }
            let old_request = durable.native_build.unwrap().request;
            let processes = target.application.instance().is_running().await.then(|| {
                postgres_replicated::testing::ProcessProbe::postgres(
                    target.application.instance().data_dir(),
                )
            });
            target.runtime.abort();
            assert!(build.await.is_err());
            if let Some(processes) = processes {
                processes.assert_reaped();
            }
            old_request
        };
        let target = target.reopen().await;
        assert!(target.inject(&old_request).await.is_err());
        let authority = source.authorize(&target, "crash").await;
        source.build(&target, &authority).await.unwrap();
        target.refresh().await;
        assert_data(&target).await;
        assert_eq!(target.runtime.snapshot().await.builds.len(), 1);
    }
}

#[tokio::test]
async fn native_lineage_validation_precedes_scalar_boundary_comparison() {
    let source = PgLineage {
        system_identifier: "123".into(),
        timeline: 1,
        history: vec![],
        history_text: String::new(),
    };
    let other_system = PgLineage {
        system_identifier: "456".into(),
        ..source.clone()
    };
    assert!(!other_system.can_rewind_from(&source));
    let root = TestDataDir::new("nl");
    let source = PgPod::new(root.path().join("s"), native_identity(1, "s")).await;
    let target = PgPod::new(root.path().join("t"), native_identity(2, "t")).await;
    source.singleton().await;
    target.singleton().await;
    source
        .admit(postgres_replicated::testing::native_configuration(
            std::slice::from_ref(&source.identity),
            0,
            3,
        ))
        .await;
    let authority = source.authorize(&target, "incompatible").await;
    target.refresh().await;
    assert!(
        target.runtime.snapshot().await.builds.is_empty(),
        "an unrelated pre-existing primary's scalar progress is not build completion"
    );
    let before = target
        .application
        .instance()
        .control_identity()
        .await
        .unwrap();
    let error = source.build(&target, &authority).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("incompatible PostgreSQL system identity/timeline"),
        "{error}"
    );
    assert_eq!(
        target
            .application
            .instance()
            .control_identity()
            .await
            .unwrap(),
        before
    );
    assert!(
        target
            .application
            .native_driver()
            .durable_state()
            .await
            .native_build
            .is_none()
    );
    let evidence = PgNativeObserver::new(target.application.instance().clone())
        .snapshot()
        .await;
    // Rejection may stop an in-flight build, but must never replace its data.
    if let Ok(evidence) = evidence {
        assert_eq!(evidence.evidence.unwrap().system_identifier, before.0);
    }
    source
        .effect(RuntimeEffectAction::SetAccessStatus {
            read: AccessStatus::ReconfigurationPending,
            write: AccessStatus::ReconfigurationPending,
        })
        .await
        .unwrap();
}
