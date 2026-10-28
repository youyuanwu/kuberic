#[tokio::test]
async fn m2_application_reopen_role_address_waits_for_actual_callback_receipt() {
    ordinary_reopen_role_address_scenario().await;
}

#[tokio::test]
async fn m2_application_copy_failures_preserve_retained_bytes_and_never_ack() {
    m2_builtin::application_copy_failures().await;
}

async fn ordinary_reopen_role_address_scenario() {
    fn recorded(root: &Path, goal: &ReplicaGoal) -> serde_json::Value {
        let path =
            kuberic_runtime::testing::sqlite_store::SqliteStore::metadata_database_path(root);
        let connection =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let json: String = connection
            .query_row(
                "SELECT state_json FROM agent_state WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&json).unwrap();
        state["publicV1OperationState"]["operations"][goal.operation_id.as_str()].clone()
    }
    tokio::time::timeout(Duration::from_secs(45), async {
        for fail in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let epoch = Epoch::new(0, 1);
            let mut old = Node::<RoleAddressKvService>::start_sized(root.path(), 1, 0, 1).await;
            old.open(ReplicaRole::Primary, epoch).await;
            old.single(epoch).await;
            old.service.inner.replicate_put("retained".into(), "acknowledged".into()).await.unwrap();
            let predecessor = m2_builtin::snapshot(&old).await;
            let old_address = old.service.address.clone();
            old.close().await;
            let mut current = Node::<RoleAddressKvService>::start_sized(root.path(), 1, 0, 1).await;
            let fresh = m2_builtin::snapshot(&current).await;
            assert_ne!(fresh.process_session_id, predecessor.process_session_id);
            assert_ne!(fresh.creation_id, predecessor.creation_id);
            assert_ne!(current.service.address, old_address);
            current.service.block_primary.store(true, Ordering::Release);
            current.service.fail_primary.store(fail, Ordering::Release);
            let goal = ReplicaGoal {
                format: ORDINARY_ACTION_FORMAT, identity: fresh.identity.clone(),
                operation_id: OperationId::new("held-reopen-role"),
                revision: fresh.goals.iter().map(|entry| entry.goal.revision).max().unwrap() + 1,
                scope: ReplicaScope {
                    history: fresh.history, storage: fresh.storage,
                    process_session_id: fresh.process_session_id, creation_id: fresh.creation_id,
                    epoch, configuration: current.configuration(epoch),
                },
                desired: ReplicaGoalKind::Prepare { role: ReplicaRole::Primary, rebuild: None },
            };
            m2_builtin::submit(&current, &goal).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), current.service.primary_entered.notified()).await.unwrap();
            let pending = m2_builtin::snapshot(&current).await;
            assert_eq!(pending.publication.unwrap().stage, ReplicaPublicationStage::DiscoveryPending);
            assert_ne!(pending.write_status, AccessStatus::Granted);
            assert!(!recorded(root.path(), &goal).to_string().contains(&current.service.address));
            current.service.block_primary.store(false, Ordering::Release);
            current.service.primary_release.notify_one();
            let outcome = m2_builtin::settled(&current, &goal).await;
            let replay = m2_builtin::submit(&current, &goal).await;
            if fail { assert!(replay.is_err()); } else { replay.unwrap(); }
            assert_eq!(current.service.primary_calls.load(Ordering::Acquire), 1);
            if fail {
                assert!(matches!(outcome, ReplicaGoalStatus::Failed(ref detail) if detail.contains("replacement application role rejected")));
                let rejected = m2_builtin::snapshot(&current).await;
                assert!(rejected.withdrawn && rejected.publication.is_none());
                assert!(!recorded(root.path(), &goal).to_string().contains(&current.service.address));
                assert!(current.service.inner.get("retained").await.is_err());
                assert!(current.stop().await.is_none());
                let mut successor = Node::<RoleAddressKvService>::start_sized(root.path(), 1, 0, 1).await;
                let before = m2_builtin::snapshot(&successor).await;
                let mut retry = goal.clone();
                retry.operation_id = OperationId::new("fresh-process-after-actual-settlement");
                retry.revision = before.goals.iter().map(|entry| entry.goal.revision).max().unwrap() + 1;
                retry.scope.process_session_id = before.process_session_id.clone();
                retry.scope.creation_id = before.creation_id.clone();
                m2_builtin::submit(&successor, &retry).await.unwrap();
                assert_eq!(m2_builtin::settled(&successor, &retry).await, ReplicaGoalStatus::Achieved);
                let after = m2_builtin::snapshot(&successor).await;
                assert!(after.goals.iter().any(|entry| entry.goal == goal && matches!(entry.status, ReplicaGoalStatus::Failed(_))));
                assert!(m2_builtin::submit(&successor, &goal).await.is_err());
                assert_eq!(successor.service.primary_calls.load(Ordering::Acquire), 1);
                successor.scope = Some(retry.scope.clone());
                successor.revision = retry.revision;
                successor.single(epoch).await;
                assert_eq!(successor.service.inner.get("retained").await.unwrap().as_deref(), Some("acknowledged"));
                successor.close().await;
            } else {
                assert_eq!(outcome, ReplicaGoalStatus::Achieved);
                assert!(recorded(root.path(), &goal).to_string().contains(&current.service.address));
                current.scope = Some(goal.scope.clone());
                current.revision = goal.revision;
                current.single(epoch).await;
                assert_eq!(current.service.inner.get("retained").await.unwrap().as_deref(), Some("acknowledged"));
                let before = m2_builtin::snapshot(&current).await;
                assert!(m2_builtin::submit(&current, &predecessor.goals.last().unwrap().goal).await.is_err());
                assert_eq!(m2_builtin::snapshot(&current).await, before);
                current.close().await;
            }
        }
    }).await.expect("ordinary Role receipt owns the reopened application address");
}

#[tokio::test]
async fn m2_application_sqlite_abort_close_retains_actual_sql_worker() {
    tokio::time::timeout(Duration::from_secs(35), async {
        let root = tempfile::tempdir().unwrap();
        let mut node = Node::<SqliteService>::start_sized(root.path(), 1, 0, 1).await;
        let epoch = Epoch::new(0, 1);
        node.open(ReplicaRole::Primary, epoch).await;
        node.single(epoch).await;
        let server = sqlite_replicated::server::SqliteServer::new(node.service.clone());
        sql_write(
            &server,
            "CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
        )
        .await
        .unwrap();
        let baseline = sql_write(&server, "INSERT INTO data VALUES (1,'acknowledged')")
            .await
            .unwrap();
        let committer = node.service.committer_for_test().unwrap();
        committer.before_commit().arm();
        committer.worker_exit().arm();
        let write_server = server.clone();
        let write = tokio::spawn(async move {
            sql_write(
                &write_server,
                "INSERT INTO data VALUES (2,'must-not-commit')",
            )
            .await
        });
        committer.before_commit().wait_entered().await;
        let mut terminal = m2_builtin::snapshot(&node)
            .await
            .goals
            .last()
            .unwrap()
            .goal
            .clone();
        terminal.operation_id = OperationId::new("abort-held-sql-worker");
        terminal.revision = node.revision + 1;
        terminal.desired = ReplicaGoalKind::Abort;
        m2_builtin::submit(&node, &terminal).await.unwrap();
        committer.worker_exit().wait_entered().await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = m2_builtin::snapshot(&node).await;
                if snapshot.withdrawn {
                    assert!(snapshot.publication.is_none() && snapshot.provider_writer_held);
                    assert_ne!(snapshot.write_status, AccessStatus::Granted);
                    assert_eq!(
                        snapshot
                            .goals
                            .iter()
                            .find(|entry| entry.goal == terminal)
                            .unwrap()
                            .status,
                        ReplicaGoalStatus::Pending
                    );
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        committer.before_commit().release();
        committer.worker_exit().release();
        assert!(write.await.unwrap().is_err());
        assert_eq!(
            m2_builtin::settled(&node, &terminal).await,
            ReplicaGoalStatus::Achieved
        );
        assert!(!m2_builtin::snapshot(&node).await.provider_writer_held);
        assert_eq!(
            node.service.persistence().progress().unwrap().committed_lsn,
            baseline
        );
        drop(server);
        drop(committer);
        assert!(node.stop().await.is_none());
    })
    .await
    .expect("ordinary SQLite Close joins the actual held SQL worker");
}

#[tokio::test]
async fn m2_application_incompatible_goal_rejects_before_application_or_command_mutation() {
    let root = tempfile::tempdir().unwrap();
    let mut node = Node::<KvService>::start_sized(root.path(), 1, 0, 1).await;
    let epoch = Epoch::new(0, 1);
    node.open(ReplicaRole::Primary, epoch).await;
    node.single(epoch).await;
    node.service
        .replicate_put("retained".into(), "acknowledged".into())
        .await
        .unwrap();
    let before = m2_builtin::snapshot(&node).await;
    let application = std::fs::read(root.path().join("application/state.json")).unwrap();
    let mut incompatible = before.goals.last().unwrap().goal.clone();
    incompatible.operation_id = OperationId::new("incompatible-before-mutation");
    incompatible.revision += 1;
    incompatible.format -= 1;
    assert!(m2_builtin::submit(&node, &incompatible).await.is_err());
    assert_eq!(m2_builtin::snapshot(&node).await, before);
    assert_eq!(
        std::fs::read(root.path().join("application/state.json")).unwrap(),
        application
    );
    assert_eq!(
        node.service.get("retained").await.unwrap().as_deref(),
        Some("acknowledged")
    );
    node.close().await;
}

#[tokio::test]
async fn m2_application_kv_ordinary_copy_access_and_matching_format_reopen() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let root = tempfile::tempdir().unwrap();
        let source_root = root.path().join("source");
        let target_root = root.path().join("target");
        let mut source = Node::<KvService>::start_sized(&source_root, 1, 0, 1).await;
        let mut target = Node::<KvService>::start(&target_root, 2, 0).await;
        let epoch = Epoch::new(0, 1);
        source.open(ReplicaRole::Primary, epoch).await;
        assert!(
            source
                .service
                .replicate_put("denied".into(), "before-access".into())
                .await
                .is_err()
        );
        let single = source.single(epoch).await;
        let acknowledged = source
            .service
            .replicate_put("before-copy".into(), "durable".into())
            .await
            .unwrap();
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        let build = source.build_idle(&mut target, epoch, &single).await;
        let ready = m2_builtin::snapshot(&source).await;
        assert!(
            ready
                .remote_operations
                .iter()
                .any(|entry| entry.operation_id == build.operation_id
                    && entry.state == RemoteReplicaState::Ready
                    && !entry.accepted)
        );
        assert_eq!(
            target.service.persistence().get("before-copy").as_deref(),
            Some("durable")
        );
        assert!(target.service.get("before-copy").await.is_err());
        assert!(
            target
                .service
                .replicate_put("idle".into(), "forbidden".into())
                .await
                .is_err()
        );
        assert!(
            !single
                .members
                .iter()
                .any(|member| member.identity == target.identity)
        );
        // Replaying the same canonical ordinary Build observes its retained result.
        m2_builtin::submit(&source, &build).await.unwrap();
        assert_eq!(
            m2_builtin::settled(&source, &build).await,
            ReplicaGoalStatus::Achieved
        );
        let old = m2_builtin::snapshot(&source).await;
        tokio::join!(source.close(), target.close());
        let mut reopened = Node::<KvService>::start_sized(&source_root, 1, 0, 1).await;
        let fresh = m2_builtin::snapshot(&reopened).await;
        assert_eq!(fresh.storage, old.storage);
        assert_ne!(fresh.process_session_id, old.process_session_id);
        assert_ne!(fresh.creation_id, old.creation_id);
        assert!(fresh.publication.is_none());
        assert!(m2_builtin::submit(&reopened, &build).await.is_err());
        reopened.open(ReplicaRole::Primary, epoch).await;
        reopened.single(epoch).await;
        assert_eq!(
            reopened
                .service
                .get("before-copy")
                .await
                .unwrap()
                .as_deref(),
            Some("durable")
        );
        assert!(
            reopened
                .service
                .replicate_put("after-reopen".into(), "acknowledged".into())
                .await
                .unwrap()
                > acknowledged
        );
        reopened.close().await;
    })
    .await
    .expect("bounded KV ordinary application journey");
}

#[tokio::test]
async fn m2_application_sqlite_ordinary_copy_access_and_matching_format_reopen() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let root = tempfile::tempdir().unwrap();
        let source_root = root.path().join("source");
        let target_root = root.path().join("target");
        let mut source = Node::<SqliteService>::start_sized(&source_root, 1, 0, 1).await;
        let mut target = Node::<SqliteService>::start(&target_root, 2, 0).await;
        let epoch = Epoch::new(0, 1);
        source.open(ReplicaRole::Primary, epoch).await;
        let server = sqlite_replicated::server::SqliteServer::new(source.service.clone());
        assert!(
            sql_write(&server, "CREATE TABLE denied(id INTEGER)")
                .await
                .is_err()
        );
        let single = source.single(epoch).await;
        sql_write(
            &server,
            "CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
        )
        .await
        .unwrap();
        let acknowledged = sql_write(&server, "INSERT INTO data VALUES (1,'before-copy')")
            .await
            .unwrap();
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source.build(&mut target, epoch, &single).await;
        let idle = sqlite_replicated::server::SqliteServer::new(target.service.clone());
        assert!(sql_values(&idle).await.is_err());
        assert!(
            sql_write(&idle, "INSERT INTO data VALUES (2,'forbidden')")
                .await
                .is_err()
        );
        let later = sql_write(&server, "INSERT INTO data VALUES (2,'replicated')")
            .await
            .unwrap();
        assert!(later > acknowledged);
        assert_eq!(
            target
                .service
                .persistence()
                .progress()
                .unwrap()
                .committed_lsn,
            later
        );
        let old = m2_builtin::snapshot(&source).await;
        drop(server);
        drop(idle);
        tokio::join!(source.close(), target.close());
        let mut reopened = Node::<SqliteService>::start_sized(&source_root, 1, 0, 1).await;
        let fresh = m2_builtin::snapshot(&reopened).await;
        assert_eq!(fresh.storage, old.storage);
        assert_ne!(fresh.process_session_id, old.process_session_id);
        assert_ne!(fresh.creation_id, old.creation_id);
        reopened.open(ReplicaRole::Primary, epoch).await;
        reopened.single(epoch).await;
        let server = sqlite_replicated::server::SqliteServer::new(reopened.service.clone());
        assert_eq!(
            sql_values(&server).await.unwrap(),
            ["before-copy", "replicated"]
        );
        assert!(
            sql_write(&server, "INSERT INTO data VALUES (3,'reopened')")
                .await
                .unwrap()
                > later
        );
        drop(server);
        reopened.close().await;
    })
    .await
    .expect("bounded SQLite ordinary application journey");
}

#[tokio::test]
async fn m2_application_external_ordinary_copy_quorum_access_and_reopen() {
    use bytes::Bytes;
    tokio::time::timeout(Duration::from_secs(45), async {
        let root = tempfile::tempdir().unwrap();
        let source_root = root.path().join("source");
        let target_root = root.path().join("target");
        let mut source = Node::<ExternalService>::start_sized(&source_root, 1, 0, 1).await;
        let mut target = Node::<ExternalService>::start(&target_root, 2, 0).await;
        let epoch = Epoch::new(0, 1);
        source.open(ReplicaRole::Primary, epoch).await;
        assert!(
            source
                .service
                .begin_write(Bytes::from_static(b"forbidden"))
                .await
                .is_err()
        );
        let single = source.single(epoch).await;
        source
            .service
            .begin_write(Bytes::from_static(b"copied"))
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source.build(&mut target, epoch, &single).await;
        let tail = source
            .service
            .begin_write(Bytes::from_static(b"quorum-acknowledged"))
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(
            ExternalService::read_persisted_snapshot(&target_root.join("application"))
                .unwrap()
                .values,
            [
                Bytes::from_static(b"copied"),
                Bytes::from_static(b"quorum-acknowledged")
            ]
        );
        assert!(target.service.values().await.is_err());
        tokio::join!(source.close(), target.close());
        let mut reopened = Node::<ExternalService>::start_sized(&source_root, 1, 0, 1).await;
        reopened.open(ReplicaRole::Primary, epoch).await;
        reopened.single(epoch).await;
        assert_eq!(
            reopened.service.values().await.unwrap(),
            [
                Bytes::from_static(b"copied"),
                Bytes::from_static(b"quorum-acknowledged")
            ]
        );
        assert!(
            reopened
                .service
                .begin_write(Bytes::from_static(b"reopen"))
                .await
                .unwrap()
                .wait()
                .await
                .unwrap()
                > tail
        );
        reopened.close().await;
    })
    .await
    .expect("bounded external ordinary application journey");
}

#[tokio::test]
async fn m2_application_scope_withdrawal_waits_for_actual_kv_consumer_close() {
    tokio::time::timeout(Duration::from_secs(35), async {
        let root = tempfile::tempdir().unwrap();
        let mut source = Node::<KvService>::start(&root.path().join("source"), 1, 0).await;
        let mut target = Node::<KvService>::start(&root.path().join("target"), 2, 0).await;
        let epoch = Epoch::new(0, 1);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source.build(&mut target, epoch, &single).await;
        let exit = target.service.pause_next_stream_exit();
        let mut terminal = m2_builtin::snapshot(&target)
            .await
            .goals
            .last()
            .unwrap()
            .goal
            .clone();
        terminal.operation_id = OperationId::new("withdraw-held-application");
        terminal.revision = target.revision + 1;
        terminal.desired = ReplicaGoalKind::Abort;
        m2_builtin::submit(&target, &terminal).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), exit.entered())
            .await
            .unwrap();
        let (read, write) = target.service.abort_access_observation().unwrap();
        assert_ne!(read, AccessStatus::Granted);
        assert_ne!(write, AccessStatus::Granted);
        let pending = m2_builtin::snapshot(&target).await;
        assert!(pending.withdrawn && pending.publication.is_none() && pending.provider_writer_held);
        assert_ne!(pending.read_status, AccessStatus::Granted);
        assert_ne!(pending.write_status, AccessStatus::Granted);
        assert_eq!(
            pending
                .goals
                .iter()
                .find(|entry| entry.goal == terminal)
                .unwrap()
                .status,
            ReplicaGoalStatus::Pending
        );
        m2_builtin::submit(&target, &terminal).await.unwrap();
        let mut incompatible = terminal.clone();
        incompatible.operation_id = OperationId::new("no-successor-before-join");
        incompatible.revision += 1;
        incompatible.scope.epoch = Epoch::new(0, 9);
        incompatible.desired = ReplicaGoalKind::Prepare {
            role: ReplicaRole::IdleSecondary,
            rebuild: None,
        };
        assert!(m2_builtin::submit(&target, &incompatible).await.is_err());
        exit.release();
        assert_eq!(
            m2_builtin::settled(&target, &terminal).await,
            ReplicaGoalStatus::Achieved
        );
        assert!(!m2_builtin::snapshot(&target).await.provider_writer_held);
        assert_eq!(target.service.retained_consumer_count().await, 0);
        source.close().await;
        assert!(target.stop().await.is_none());
    })
    .await
    .expect("actual KV application Close is retained behind withdrawal");
}

#[tokio::test]
async fn m2_application_sqlite_close_joins_actual_consumers_and_retains_failure() {
    tokio::time::timeout(Duration::from_secs(35), async {
        let root = tempfile::tempdir().unwrap();
        let mut source = Node::<SqliteService>::start(&root.path().join("source"), 1, 0).await;
        let mut target = Node::<SqliteService>::start(&root.path().join("target"), 2, 0).await;
        let epoch = Epoch::new(0, 1);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source.build(&mut target, epoch, &single).await;
        target.service.consumer_exit_gate().arm();
        target.service.fail_next_consumer_exit();
        let mut terminal = m2_builtin::snapshot(&target).await.goals.last().unwrap().goal.clone();
        terminal.operation_id = OperationId::new("withdraw-failed-sqlite-application");
        terminal.revision = target.revision + 1;
        terminal.desired = ReplicaGoalKind::Abort;
        m2_builtin::submit(&target, &terminal).await.unwrap();
        target.service.consumer_exit_gate().wait_entered().await;
        let pending = m2_builtin::snapshot(&target).await;
        assert!(pending.withdrawn && pending.publication.is_none() && pending.provider_writer_held);
        assert_ne!(pending.write_status, AccessStatus::Granted);
        assert_eq!(pending.goals.iter().find(|entry| entry.goal == terminal).unwrap().status, ReplicaGoalStatus::Pending);
        target.service.consumer_exit_gate().release();
        let failure = m2_builtin::settled(&target, &terminal).await;
        assert!(matches!(&failure, ReplicaGoalStatus::Failed(detail) if detail.contains("injected SQLite consumer join failure")));
        m2_builtin::submit(&target, &terminal).await.unwrap();
        assert_eq!(m2_builtin::settled(&target, &terminal).await, failure);
        let retained = m2_builtin::snapshot(&target).await;
        assert!(retained.withdrawn && retained.publication.is_none() && retained.provider_writer_held);
        assert!(target.service.close().await.unwrap_err().to_string().contains("injected SQLite consumer join failure"));
        source.close().await;
        target.running.shutdown();
        let stopped = target.running.wait().await;
        assert!(stopped.is_err(), "failed application join must stay negative");
    }).await.expect("actual SQLite application failure never releases its writer");
}
