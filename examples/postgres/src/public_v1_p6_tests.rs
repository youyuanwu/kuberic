use super::*;

pub(crate) async fn public_v1_postgres_secondary_readiness() {
    stage("secondary authority and replay readiness", async {
        let directory = OracleDataDir::new();
        let source = Replica::new(directory.path().join("ready-source"), 151).await;
        let target = Replica::new(directory.path().join("ready-target"), 152).await;
        source.singleton(1).await;
        let writer = application(&source).await;
        writer
            .simple_query("CREATE TABLE readiness(id integer)")
            .await
            .unwrap();
        target.role(1, ReplicaRole::IdleSecondary).await;
        let binding = BindingFactory::new().build(&source, &target, "readiness-build");
        source
            .driver
            .public_build_native(binding.clone())
            .await
            .unwrap();
        target.role(1, ReplicaRole::ActiveSecondary).await;
        let config_path = target.driver.instance.data_dir().join("postgresql.conf");
        let mut settings = std::fs::read_to_string(&config_path).unwrap();
        settings.push_str("\nrecovery_min_apply_delay = '1h'\n");
        std::fs::write(config_path, settings).unwrap();
        writer
            .simple_query("INSERT INTO readiness VALUES (1)")
            .await
            .unwrap();
        drop(writer);
        let current =
            native_configuration(&[source.identity.clone(), target.identity.clone()], 0, 1);
        let peer = BindingFactory::new().peer(&source, &target, &current, None);
        source
            .driver
            .public_configuration(
                ReplicaSetConfiguration {
                    configuration: current,
                    replicas: vec![peer_replica(&peer)],
                },
                None,
            )
            .await
            .unwrap();
        let boundary = {
            let state = source.driver.public_owner().state.lock().await;
            state
                .grants
                .iter()
                .find(|grant| grant.binding.target() == &target.identity)
                .unwrap()
                .boundary
        };
        let (control, _connection) = target.driver.instance.connect().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let observation = target.driver.observer.snapshot().await.unwrap();
                let evidence = observation.evidence.unwrap();
                if evidence.received_lsn.is_some_and(|lsn| lsn >= boundary) {
                    assert!(evidence.replay_lsn.is_none_or(|lsn| lsn < boundary));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            target.driver.instance.connect_application().await.is_err(),
            "active secondary admitted SQL before required replay"
        );
        control
            .simple_query("ALTER SYSTEM SET recovery_min_apply_delay = '0'")
            .await
            .unwrap();
        control
            .simple_query("SELECT pg_reload_conf()")
            .await
            .unwrap();
        let reader = application(&target).await;
        assert_eq!(
            reader
                .query_one("SELECT count(*) FROM readiness", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            1
        );
        assert!(
            reader
                .simple_query("INSERT INTO readiness VALUES (2)")
                .await
                .is_err()
        );
        let context = target.driver.public_owner().fence.context().unwrap();
        *target.driver.public_owner().test_access.lock().unwrap() =
            (AccessStatus::NotPrimary, AccessStatus::NotPrimary);
        tokio::time::timeout(Duration::from_secs(10), async {
            while reader.simple_query("SELECT 1").await.is_ok() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            context,
            target.driver.public_owner().fence.context().unwrap()
        );
        assert!(target.driver.instance.connect_application().await.is_err());
        target.driver.public_close().await.unwrap();
        source.driver.public_close().await.unwrap();
    })
    .await;
}

pub(crate) async fn public_v1_postgres_partition_authority() {
    stage("live partition SQL authority", async {
        let directory = OracleDataDir::new();
        let hosted = production_service(directory.path().join("authority"), 121).await;
        let (sql, connection) = stage("authorized no-provider primary SQL", async {
            loop {
                if let Ok(client) = hosted.driver.instance.connect_application().await {
                    break client;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        sql.simple_query("CREATE TABLE authority_probe(id integer)")
            .await
            .unwrap();
        assert_eq!(hosted.running.handle().public_v1_provider_writer_count(), 0);
        let context = hosted.driver.public_owner().fence.context().unwrap();
        hosted.running.handle().fence_public_v1_access().await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if sql
                    .simple_query("INSERT INTO authority_probe VALUES (1)")
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("status-only partition revocation left an admitted writer usable");
        assert_eq!(
            context,
            hosted.driver.public_owner().fence.context().unwrap(),
            "status-only probe changed lifecycle authority"
        );
        assert!(hosted.driver.instance.connect_application().await.is_err());
        drop(sql);
        let _ = connection.await;
        hosted.shutdown().await;
    })
    .await;
}

pub(crate) async fn public_v1_postgres_orderly_progress() {
    stage("final orderly durable progress", async {
        for primary_none in [false, true] {
            let directory = OracleDataDir::new();
            let source = Replica::new(directory.path().join("progress-source"), 131).await;
            let target = Replica::new(directory.path().join("progress-target"), 132).await;
            source.singleton(1).await;
            let sql = application(&source).await;
            sql.simple_query("CREATE TABLE terminal_progress(id integer)")
                .await
                .unwrap();
            drop(sql);
            build_and_admit(&source, &target, 1, "terminal-progress").await;
            let primary_cached = source.driver.durable.snapshot().await.policy_certified_lsn;
            let secondary_cached = target
                .driver
                .durable
                .snapshot()
                .await
                .received_lsn
                .unwrap_or(-1);
            let writer = application(&source).await;
            writer
                .simple_query("INSERT INTO terminal_progress SELECT generate_series(1,100)")
                .await
                .unwrap();
            let primary = source.driver.current_progress().await.unwrap();
            let secondary = target.driver.current_progress().await.unwrap();
            assert!(
                primary > primary_cached,
                "primary regression never advanced past persisted admission"
            );
            assert!(
                secondary > secondary_cached,
                "secondary regression never advanced past admission"
            );
            assert_eq!(
                source.driver.durable.snapshot().await.policy_certified_lsn,
                primary_cached
            );
            if primary_none {
                source.change_role(2, ReplicaRole::None).await;
                target.epoch(2).await;
            } else {
                source.epoch(2).await;
                target.change_role(2, ReplicaRole::None).await;
            }
            let stopped_primary = source.driver.current_progress().await.unwrap();
            let stopped_secondary = target.driver.current_progress().await.unwrap();
            assert!(
                stopped_primary >= primary,
                "final certified primary position was lost"
            );
            assert!(
                stopped_secondary >= secondary,
                "final secondary receive/replay position was lost"
            );
            for replica in [&source, &target] {
                assert!(!replica.driver.instance.is_running().await);
                assert_eq!(replica.driver.catch_up_capability().await.unwrap(), -1);
                assert!(replica.driver.instance.connect_application().await.is_err());
            }
            assert!(
                writer
                    .simple_query("INSERT INTO terminal_progress VALUES (999)")
                    .await
                    .is_err()
            );
            let durable = source.driver.durable.snapshot().await;
            assert_eq!(
                stopped_primary, durable.policy_certified_lsn,
                "stopped primary/None exported local flush instead of its certificate"
            );
            let durable = target.driver.durable.snapshot().await;
            assert_eq!(
                stopped_secondary,
                durable
                    .received_lsn
                    .into_iter()
                    .chain(durable.replay_lsn)
                    .max()
                    .unwrap_or(-1),
                "stopped secondary/None was conflated with primary certification"
            );
            target.driver.public_close().await.unwrap();
            source.driver.public_close().await.unwrap();
        }
    })
    .await;
}

pub(crate) async fn public_v1_postgres_marked_catchup() {
    stage("marked WriteQuorum unsupported", async {
        let directory = OracleDataDir::new();
        let source = Replica::new(directory.path().join("marked-source"), 141).await;
        let required = Replica::new(directory.path().join("marked-required"), 142).await;
        let healthy = Replica::new(directory.path().join("marked-healthy"), 143).await;
        source.singleton(1).await;
        let sql = application(&source).await;
        sql.simple_query("CREATE TABLE marked_catchup(id integer)")
            .await
            .unwrap();
        drop(sql);
        for target in [&required, &healthy] {
            target.role(1, ReplicaRole::IdleSecondary).await;
            let binding = BindingFactory::new().build(&source, target, "marked-build");
            source
                .driver
                .public_build_native(binding.clone())
                .await
                .unwrap();
            target.role(1, ReplicaRole::ActiveSecondary).await;
        }
        let config = native_configuration(
            &[
                source.identity.clone(),
                required.identity.clone(),
                healthy.identity.clone(),
            ],
            0,
            1,
        );
        let peers = [&required, &healthy].map(|target| {
            peer_replica(&BindingFactory::new().peer(&source, target, &config, None))
        });
        let mut marked = peers.to_vec();
        marked[0].must_catchup = true;
        source
            .driver
            .public_configuration(
                ReplicaSetConfiguration {
                    configuration: config.clone(),
                    replicas: marked,
                },
                None,
            )
            .await
            .unwrap();
        source
            .driver
            .public_wait(ReplicaSetQuorumMode::All)
            .await
            .unwrap();
        let writer = application(&source).await;
        let (control, _connection) = required.driver.instance.connect().await.unwrap();
        control
            .simple_query("SELECT pg_wal_replay_pause()")
            .await
            .unwrap();
        let before = source.driver.current_progress().await.unwrap();
        writer
            .simple_query("INSERT INTO marked_catchup SELECT generate_series(1,100)")
            .await
            .unwrap();
        let certified = source.driver.current_progress().await.unwrap();
        assert!(certified > before);
        let replay = crate::monitor::parse_pg_lsn(
            control
                .query_one("SELECT pg_last_wal_replay_lsn()::text", &[])
                .await
                .unwrap()
                .get(0),
        )
        .unwrap();
        assert!(
            replay < certified,
            "marked peer did not lag actual ordinary quorum"
        );
        let error = source
            .driver
            .public_wait(ReplicaSetQuorumMode::WriteQuorum)
            .await
            .expect_err("aggregate-only WriteQuorum silently ignored a lagging marked successor");
        assert!(
            error.to_string().contains("unsupported"),
            "request did not reject unsupported marked semantics explicitly: {error}"
        );
        writer
            .simple_query("SELECT 1")
            .await
            .expect("unsupported query destructively stopped primary");
        control
            .simple_query("SELECT pg_wal_replay_resume()")
            .await
            .unwrap();
        source
            .driver
            .public_wait(ReplicaSetQuorumMode::All)
            .await
            .unwrap();
        source
            .driver
            .public_configuration(
                ReplicaSetConfiguration {
                    configuration: config,
                    replicas: peers.to_vec(),
                },
                None,
            )
            .await
            .unwrap();
        source
            .driver
            .public_wait(ReplicaSetQuorumMode::WriteQuorum)
            .await
            .unwrap();
        let mut controls = Vec::new();
        for target in [&required, &healthy] {
            let (control, _connection) = target.driver.instance.connect().await.unwrap();
            control
                .simple_query("SELECT pg_wal_replay_pause()")
                .await
                .unwrap();
            controls.push(control);
        }
        writer
            .simple_query("SET synchronous_commit = off; INSERT INTO marked_catchup VALUES (999)")
            .await
            .unwrap();
        let (admin, _connection) = source.driver.instance.connect().await.unwrap();
        admin.simple_query("CHECKPOINT").await.unwrap();
        let driver = source.driver.clone();
        let mut pending =
            tokio::spawn(async move { driver.public_wait(ReplicaSetQuorumMode::All).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut pending)
                .await
                .is_err()
        );
        *source.driver.public_owner().test_access.lock().unwrap() =
            (AccessStatus::NotPrimary, AccessStatus::NotPrimary);
        tokio::time::timeout(Duration::from_secs(10), async {
            while writer.simple_query("SELECT 1").await.is_ok() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(pending.await.unwrap().is_err());
        })
        .await
        .expect(
            "pending catch-up monopolized the lifecycle and blocked status-only SQL revocation",
        );
        healthy.driver.public_close().await.unwrap();
        required.driver.public_close().await.unwrap();
        source.driver.public_close().await.unwrap();
    })
    .await;
}
