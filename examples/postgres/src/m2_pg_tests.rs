use super::super::session::{PgSessionId, SessionAction, SessionRequest, SessionStage};
use super::*;
use crate::instance::DriverExitGate;
use kuberic_runtime::protocol::public_operations::{
    ORDINARY_ACTION_FORMAT, ReplicaConfigurationScope, ReplicaPublication, ReplicaPublicationStage,
    ReplicaScope,
};
use kuberic_runtime::protocol::types::{
    HistoryContext, HistoryId, PodUid, PublicProtocolGeneration, PvcUid, ReplicaStorageIdentity,
    ReplicaStorageIncarnationContext, ReplicatorCreationId, StorageGeneration,
};

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn m2_pg_matching_format_reopen_preserves_data() {
    let directory = OracleDataDir::new();
    let root = directory.path().join("matching");
    let source = Replica::new(root.clone(), 320).await;
    source.singleton(1).await;
    let (sql, connection) = source.driver.instance.connect_application().await.unwrap();
    sql.batch_execute(
        "CREATE TABLE m2_reopen(id integer primary key, value text);
        INSERT INTO m2_reopen VALUES (1, 'acknowledged-matching-format')",
    )
    .await
    .unwrap();
    drop(sql);
    connection.await.unwrap();
    let acknowledged = source.driver.current_progress().await.unwrap();
    source.driver.public_close().await.unwrap();
    let identity = source.driver.durable.snapshot().await.identity;
    drop(source);
    let metadata = std::fs::read(root.join("a/state-v2.json")).unwrap();
    let clock = std::fs::read(root.join("a/process-generation-v2")).unwrap();
    let control = std::fs::read(root.join("d/global/pg_control")).unwrap();
    let version = std::fs::read(root.join("d/PG_VERSION")).unwrap();
    let mut incompatible = identity.clone();
    incompatible.resource_uid = ResourceUid::new("different-resource");
    assert!(
        PgDurableStore::open(root.join("a"), incompatible, StorageMode::Established)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(root.join("a/state-v2.json")).unwrap(),
        metadata
    );
    assert_eq!(
        std::fs::read(root.join("a/process-generation-v2")).unwrap(),
        clock
    );
    assert_eq!(
        std::fs::read(root.join("d/global/pg_control")).unwrap(),
        control
    );
    assert_eq!(std::fs::read(root.join("d/PG_VERSION")).unwrap(), version);
    let reopened =
        Replica::new_incarnation(root, 320, "public-320", "reopened-process", find_pg_bin()).await;
    assert_eq!(reopened.driver.durable.snapshot().await.identity, identity);
    assert!(
        reopened
            .driver
            .durable
            .snapshot()
            .await
            .policy_certified_lsn
            >= acknowledged
    );
    reopened.singleton(1).await;
    let (sql, connection) = reopened
        .driver
        .instance
        .connect_application()
        .await
        .unwrap();
    assert_eq!(
        sql.query_one("SELECT value FROM m2_reopen WHERE id = 1", &[])
            .await
            .unwrap()
            .get::<_, &str>(0),
        "acknowledged-matching-format"
    );
    drop(sql);
    connection.await.unwrap();
    reopened.driver.public_close().await.unwrap();
}

fn publication(
    replica: &Replica,
    role: ReplicaRole,
    configuration: &ConfigurationDescriptor,
) -> ReplicaPublication {
    ReplicaPublication {
        format: ORDINARY_ACTION_FORMAT,
        scope: ReplicaScope {
            history: HistoryContext {
                resource_uid: ResourceUid::new("postgres-public-oracle"),
                protocol_generation: PublicProtocolGeneration::new(1),
                history_id: HistoryId::new("m2-pg-history"),
                data_loss_number: 0,
            },
            storage: ReplicaStorageIncarnationContext {
                replica: replica.identity.clone(),
                storage: ReplicaStorageIdentity::Persistent {
                    pod_uid: PodUid::new(format!("pod-{}", replica.identity.replica_id)),
                    pvc_uid: PvcUid::new(format!("pvc-{}", replica.identity.replica_id)),
                },
                storage_generation: StorageGeneration::new("storage-generation"),
            },
            process_session_id: replica.session.clone(),
            creation_id: ReplicatorCreationId::new(format!(
                "creation-{}",
                replica.identity.replica_id
            )),
            epoch: configuration.epoch,
            configuration: ReplicaConfigurationScope {
                current: configuration.clone(),
                previous: None,
                replicas: Vec::new(),
            },
        },
        replication_address: replica.address.clone(),
        generation: 1,
        stage: ReplicaPublicationStage::AchievedScope,
        role,
    }
}

async fn proposed(source: &Replica, target: &Replica) -> PgSessionId {
    let config = native_configuration(std::slice::from_ref(&source.identity), 0, 1);
    let source_publication = publication(source, ReplicaRole::Primary, &config);
    let target_publication = publication(target, ReplicaRole::IdleSecondary, &config);
    // Only this direct native fixture supplies read-only discovery observations.
    // Production obtains both publications from the ordinary host context.
    for driver in [&source.driver, &target.driver] {
        *driver.public_owner().session_publications.lock().unwrap() =
            Some((source_publication.clone(), target_publication.clone()));
    }
    source
        .driver
        .pg_propose_session(source_publication, target_publication)
        .await
        .unwrap()
}

async fn message(
    replica: &Replica,
    id: &PgSessionId,
    action: SessionAction,
) -> Result<SessionStage> {
    match replica
        .driver
        .public_rpc(
            &replica.address,
            &Request::Session(Box::new(SessionRequest {
                session: id.clone(),
                action,
            })),
        )
        .await?
    {
        Response::Session(reply) if reply.session == *id && reply.action == action => {
            Ok(reply.stage)
        }
        _ => Err(RuntimeError::AuthorityNotAdmitted),
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn m2_pg_open_owns_authenticated_endpoint_before_role_recovery() {
    let directory = OracleDataDir::new();
    let replica = Replica::new(directory.path().join("open"), 301).await;
    assert!(
        !replica
            .driver
            .instance
            .data_dir()
            .join("PG_VERSION")
            .exists()
    );
    assert_eq!(
        replica.driver.public_owner().fence.context().unwrap().role,
        ReplicaRole::None
    );
    let gate = Arc::new(BuildCompletionGate {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    *replica.driver.public_owner().role_gate.lock().unwrap() = Some(gate.clone());
    let driver = replica.driver.clone();
    let role = tokio::spawn(async move {
        Replicator::change_role(driver.as_ref(), Epoch::new(0, 1), ReplicaRole::Primary).await
    });
    gate.entered.notified().await;
    assert!(!role.is_finished());
    let mut client =
        crate::proto::pg_data_service_client::PgDataServiceClient::connect(replica.address.clone())
            .await
            .unwrap();
    for credential in [None, Some("Bearer incorrect")] {
        let mut request = tonic::Request::new(crate::proto::RecoveryRequest {
            envelope_json: b"not-json".to_vec(),
        });
        if let Some(credential) = credential {
            request
                .metadata_mut()
                .insert("authorization", credential.parse().unwrap());
        }
        assert_eq!(
            client.public_v1(request).await.unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
    }
    let mut request = tonic::Request::new(crate::proto::RecoveryRequest {
        envelope_json: b"not-json".to_vec(),
    });
    request.metadata_mut().insert(
        "authorization",
        "Bearer pg-native-fixture-coordination".parse().unwrap(),
    );
    assert_eq!(
        client.public_v1(request).await.unwrap_err().code(),
        tonic::Code::InvalidArgument
    );
    let interfaces = crate::service::PgReplicatorFactory(replica.driver.clone()).interfaces();
    assert!(interfaces.state_replicator().is_none());
    assert!(interfaces.primary_replicator().is_some());
    let mut preauth = tokio::net::TcpStream::connect(replica.address.trim_start_matches("http://"))
        .await
        .unwrap();
    gate.release.notify_one();
    role.await.unwrap().unwrap();
    let process = ProcessProbe::postgres(replica.driver.instance.data_dir());
    replica.driver.abort();
    Replicator::close(replica.driver.as_ref()).await.unwrap();
    process.assert_gone().await;
    closed(&mut preauth).await;
    assert!(
        tokio::net::TcpStream::connect(replica.address.trim_start_matches("http://"))
            .await
            .is_err()
    );
    assert!(
        replica
            .driver
            .instance
            .data_dir()
            .join("PG_VERSION")
            .exists()
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn m2_pg_exact_session_replay_and_retirement() {
    let directory = OracleDataDir::new();
    let source = Replica::new(directory.path().join("source"), 302).await;
    let target = Replica::new(directory.path().join("target"), 303).await;
    source.singleton(1).await;
    target.role(1, ReplicaRole::IdleSecondary).await;
    let id = proposed(&source, &target).await;
    source.driver.pg_establish_session(&id).await.unwrap();
    assert_eq!(
        message(&source, &id, SessionAction::Confirm).await.unwrap(),
        SessionStage::Active
    );
    assert_eq!(
        message(&target, &id, SessionAction::Reserve).await.unwrap(),
        SessionStage::Receiving
    );
    assert_eq!(
        message(&target, &id, SessionAction::Complete)
            .await
            .unwrap(),
        SessionStage::Completed
    );
    assert_eq!(
        message(&target, &id, SessionAction::Reserve).await.unwrap(),
        SessionStage::Completed
    );
    let snapshot = target.persisted();
    for change in 0..8 {
        let mut stale = id.clone();
        match change {
            0 => stale.nonce[0] ^= 1,
            1 => stale.generation += 1,
            2 => stale.source.scope.creation_id = ReplicatorCreationId::new("other-creation"),
            3 => {
                stale.target.scope.storage.storage_generation =
                    StorageGeneration::new("other-storage")
            }

            4 => stale.source.scope.process_session_id = ProcessSessionId::new("other-process"),
            5 => stale.target.replication_address = "http://127.0.0.1:1".into(),
            6 => stale.target.scope.history.history_id = HistoryId::new("other-history"),
            _ => stale.source.scope.epoch = Epoch::new(0, 2),
        }
        assert!(
            message(&target, &stale, SessionAction::Reserve)
                .await
                .is_err(),
            "accepted identity substitution {change}"
        );
    }
    assert_eq!(target.persisted(), snapshot);
    let retirement = Arc::new(BuildCompletionGate {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    *target
        .driver
        .public_owner()
        .session_retirement_gate
        .lock()
        .unwrap() = Some(retirement.clone());
    let driver = source.driver.clone();
    let retiring = id.clone();
    let observer = tokio::spawn(async move { driver.pg_retire_session(&retiring).await });
    retirement.entered.notified().await;
    observer.abort();
    let _ = observer.await;
    assert!(message(&source, &id, SessionAction::Confirm).await.is_err());
    assert!(
        source
            .driver
            .pg_propose_session(id.source.clone(), id.target.clone())
            .await
            .is_err()
    );
    retirement.release.notify_one();
    source.driver.pg_retire_session(&id).await.unwrap();
    assert_eq!(
        source.driver.public_owner().fence.context().unwrap().role,
        ReplicaRole::Primary
    );
    assert!(message(&source, &id, SessionAction::Confirm).await.is_err());
    assert!(message(&target, &id, SessionAction::Reserve).await.is_err());
    assert!(
        message(&target, &id, SessionAction::Complete)
            .await
            .is_err()
    );
    source.driver.pg_retire_session(&id).await.unwrap();
    Replicator::close(source.driver.as_ref()).await.unwrap();
    Replicator::close(target.driver.as_ref()).await.unwrap();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn m2_pg_confirmation_retirement_race_and_lost_target_quarantine() {
    let directory = OracleDataDir::new();
    let source = Replica::new(directory.path().join("source"), 304).await;
    let target = Replica::new(directory.path().join("target"), 305).await;
    source.singleton(1).await;
    target.role(1, ReplicaRole::IdleSecondary).await;
    let id = proposed(&source, &target).await;
    let gate = Arc::new(BuildCompletionGate {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    *source
        .driver
        .public_owner()
        .session_confirmation_gate
        .lock()
        .unwrap() = Some(gate.clone());
    let driver = source.driver.clone();
    let pending_id = id.clone();
    let pending = tokio::spawn(async move { driver.pg_establish_session(&pending_id).await });
    gate.entered.notified().await;
    source.driver.pg_retire_session(&id).await.unwrap();
    gate.release.notify_one();
    assert!(pending.await.unwrap().is_err());
    assert!(message(&target, &id, SessionAction::Reserve).await.is_err());
    let id = proposed(&source, &target).await;
    source.driver.pg_establish_session(&id).await.unwrap();
    let final_gate = Arc::new(BuildCompletionGate {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    *source
        .driver
        .public_owner()
        .session_confirmation_gate
        .lock()
        .unwrap() = Some(final_gate.clone());
    let driver = target.driver.clone();
    let completing = id.clone();
    let address = target.address.clone();
    let final_observer = tokio::spawn(async move {
        driver
            .public_rpc(
                &address,
                &Request::Session(Box::new(SessionRequest {
                    session: completing,
                    action: SessionAction::Complete,
                })),
            )
            .await
    });
    final_gate.entered.notified().await;
    source.driver.pg_retire_session(&id).await.unwrap();
    final_gate.release.notify_one();
    assert!(final_observer.await.unwrap().is_err());
    assert!(
        message(&target, &id, SessionAction::Complete)
            .await
            .is_err()
    );
    let id = proposed(&source, &target).await;
    source.driver.pg_establish_session(&id).await.unwrap();
    target.driver.public_owner().endpoint_cancel.cancel();
    {
        let endpoint = target
            .driver
            .public_owner()
            .endpoint
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        endpoint.lock().await.join().await.unwrap();
    }
    assert!(source.driver.pg_retire_session(&id).await.is_err());
    assert!(message(&source, &id, SessionAction::Confirm).await.is_err());
    assert!(source.driver.pg_establish_session(&id).await.is_err());
    assert!(
        source
            .driver
            .pg_propose_session(id.source.clone(), id.target.clone())
            .await
            .is_err()
    );
    assert_eq!(
        source.driver.public_owner().fence.context().unwrap().role,
        ReplicaRole::Primary
    );
    assert!(Replicator::close(source.driver.as_ref()).await.is_err());
    assert!(Replicator::close(source.driver.as_ref()).await.is_err());
    // Local cleanup still executes; unreachable peer settlement is never success.
    let target_failure = Replicator::close(target.driver.as_ref())
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        Replicator::close(target.driver.as_ref())
            .await
            .unwrap_err()
            .to_string(),
        target_failure
    );
    assert!(!source.driver.instance.is_running().await);
    assert!(!target.driver.instance.is_running().await);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn m2_pg_abort_close_retains_all_joins() {
    let directory = OracleDataDir::new();
    let source = Replica::new(directory.path().join("lifetime"), 306).await;
    source.singleton(1).await;
    let (sql, sql_driver) = source.driver.instance.connect_application().await.unwrap();
    sql.batch_execute(
        "CREATE TABLE m2_lifetime(id integer primary key); INSERT INTO m2_lifetime VALUES (1)",
    )
    .await
    .unwrap();
    let exit = Arc::new(DriverExitGate::default());
    let (admin, admin_driver) = source
        .driver
        .instance
        .connect_with_driver_exit(exit.clone())
        .await
        .unwrap();
    admin.simple_query("CHECKPOINT").await.unwrap();
    let mut preauth = tokio::net::TcpStream::connect((
        source.driver.instance.listen_host(),
        source.driver.instance.port(),
    ))
    .await
    .unwrap();
    preauth.write_all(&[0, 0]).await.unwrap();
    let process = ProcessProbe::postgres(source.driver.instance.data_dir());
    source.driver.abort();
    process.assert_gone().await;
    exit.entered.notified().await;
    let driver = source.driver.clone();
    let mut first = tokio::spawn(async move { Replicator::close(driver.as_ref()).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut first)
            .await
            .is_err()
    );
    first.abort();
    let _ = first.await;
    let driver = source.driver.clone();
    let mut second = tokio::spawn(async move { Replicator::close(driver.as_ref()).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut second)
            .await
            .is_err()
    );
    assert!(sql.simple_query("SELECT 1").await.is_err());
    assert!(admin.simple_query("SELECT 1").await.is_err());
    let mut byte = [0];
    let result = tokio::time::timeout(Duration::from_secs(5), preauth.read(&mut byte))
        .await
        .unwrap();
    assert!(!matches!(result, Ok(1..)));
    exit.release.notify_one();
    second.await.unwrap().unwrap();
    sql_driver.await.unwrap();
    admin_driver.await.unwrap();
    Replicator::close(source.driver.as_ref()).await.unwrap();
    assert!(
        source
            .driver
            .instance
            .data_dir()
            .join("PG_VERSION")
            .exists()
    );
    assert!(source.driver.instance.data_dir().join("base").exists());
    assert!(
        source
            .driver
            .durable
            .snapshot()
            .await
            .system_identifier
            .is_some()
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn m2_pg_close_fences_local_sql_before_remote_retirement_join() {
    let directory = OracleDataDir::new();
    let source = Replica::new(directory.path().join("source"), 312).await;
    let target = Replica::new(directory.path().join("target"), 313).await;
    source.singleton(1).await;
    target.role(1, ReplicaRole::IdleSecondary).await;
    let id = proposed(&source, &target).await;
    source.driver.pg_establish_session(&id).await.unwrap();
    let (sql, sql_driver) = source.driver.instance.connect_application().await.unwrap();
    sql.batch_execute(
        "CREATE TABLE m2_local_fence(id integer); INSERT INTO m2_local_fence VALUES (1)",
    )
    .await
    .unwrap();
    let (admin, admin_driver) = source.driver.instance.connect().await.unwrap();
    let mut preauth = tokio::net::TcpStream::connect((
        source.driver.instance.listen_host(),
        source.driver.instance.port(),
    ))
    .await
    .unwrap();
    preauth.write_all(&[0, 0]).await.unwrap();
    let process = ProcessProbe::postgres(source.driver.instance.data_dir());
    let gate = Arc::new(BuildCompletionGate {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    *target
        .driver
        .public_owner()
        .session_retirement_gate
        .lock()
        .unwrap() = Some(gate.clone());
    let driver = source.driver.clone();
    let mut observer = tokio::spawn(async move { Replicator::close(driver.as_ref()).await });
    gate.entered.notified().await;
    process.assert_gone().await;
    assert!(sql.simple_query("SELECT 1").await.is_err());
    assert!(admin.simple_query("SELECT 1").await.is_err());
    closed(&mut preauth).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut observer)
            .await
            .is_err()
    );
    observer.abort();
    let _ = observer.await;
    let driver = source.driver.clone();
    let mut repeated = tokio::spawn(async move { Replicator::close(driver.as_ref()).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut repeated)
            .await
            .is_err()
    );
    gate.release.notify_one();
    repeated.await.unwrap().unwrap();
    sql_driver.await.unwrap();
    admin_driver.await.unwrap();
    Replicator::close(target.driver.as_ref()).await.unwrap();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn m2_pg_lost_monitor_join_is_sticky() {
    lost_rust_join(false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn m2_pg_lost_connection_driver_join_is_sticky() {
    lost_rust_join(true).await;
}

async fn lost_rust_join(driver_loss: bool) {
    let directory = OracleDataDir::new();
    let source = Replica::new(directory.path().join("lost-monitor"), 307).await;
    source.singleton(1).await;
    let process = ProcessProbe::postgres(source.driver.instance.data_dir());
    if driver_loss {
        let (client, driver) = source.driver.instance.connect().await.unwrap();
        client.simple_query("SELECT 1").await.unwrap();
        driver.lose().await;
        drop(client);
    } else {
        source.driver.instance.lose_monitor().await;
    }
    source.driver.abort();
    let first = Replicator::close(source.driver.as_ref())
        .await
        .unwrap_err()
        .to_string();
    let second = Replicator::close(source.driver.as_ref())
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(first, second);
    assert!(first.contains("lost PostgreSQL Rust task"), "{first}");
    process.assert_gone().await;
    let retained = source.persisted();
    let generation = source.driver.instance.generation_id();
    assert!(
        source
            .driver
            .instance
            .start_native(source.driver.fault_tx.clone())
            .await
            .is_err()
    );
    assert_eq!(source.driver.instance.generation_id(), generation);
    assert_eq!(source.persisted(), retained);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn m2_pg_native_gateway_join_survives_lost_close_observer() {
    for fail_join in [false, true] {
        let directory = OracleDataDir::new();
        let source = Replica::new(directory.path().join("source"), 310).await;
        let target = Replica::new(directory.path().join("target"), 311).await;
        source.singleton(1).await;
        target.role(1, ReplicaRole::IdleSecondary).await;
        let binding = BindingFactory::new().build(&source, &target, "m2-gateway");
        source
            .driver
            .build_replica(build_replica(&binding))
            .await
            .unwrap();
        let (credential, exit, abort) = {
            let state = source.driver.public_owner().state.lock().await;
            let grant = state
                .grants
                .iter()
                .find(|grant| grant.binding.target() == &target.identity)
                .unwrap();
            (
                grant.credential.clone(),
                grant.gateway.hold_exit(),
                grant.gateway.task_abort_handle(),
            )
        };
        let mut authenticated = native_socket(&credential, &credential.name).await.unwrap();
        identify_system(&mut authenticated).await;
        let mut preauth = tokio::net::TcpStream::connect((&*credential.host, credential.port))
            .await
            .unwrap();
        let process = ProcessProbe::postgres(source.driver.instance.data_dir());
        source.driver.abort();
        process.assert_gone().await;
        exit.entered.notified().await;
        let driver = source.driver.clone();
        let mut first = tokio::spawn(async move { Replicator::close(driver.as_ref()).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut first)
                .await
                .is_err()
        );
        first.abort();
        let _ = first.await;
        let driver = source.driver.clone();
        let mut second = tokio::spawn(async move { Replicator::close(driver.as_ref()).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut second)
                .await
                .is_err()
        );
        closed(&mut authenticated).await;
        closed(&mut preauth).await;
        assert!(
            tokio::net::TcpStream::connect((&*credential.host, credential.port))
                .await
                .is_err()
        );
        if fail_join {
            abort.abort();
        } else {
            exit.release.notify_one();
        }
        let outcome = second.await.unwrap();
        assert_eq!(outcome.is_err(), fail_join);
        let repeated = Replicator::close(source.driver.as_ref()).await;
        assert_eq!(
            repeated.err().map(|error| error.to_string()),
            outcome.err().map(|error| error.to_string())
        );
        Replicator::close(target.driver.as_ref()).await.unwrap();
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn m2_pg_session_bounds_tombstones_and_pending_are_closed() {
    let directory = OracleDataDir::new();
    let source = Replica::new(directory.path().join("source"), 308).await;
    let target = Replica::new(directory.path().join("target"), 309).await;
    source.singleton(1).await;
    target.role(1, ReplicaRole::IdleSecondary).await;
    let first = proposed(&source, &target).await;
    let observed = (first.source.clone(), first.target.clone());
    for driver in [&source.driver, &target.driver] {
        let mut pending = observed.clone();
        pending.1.stage = ReplicaPublicationStage::DiscoveryPending;
        pending.1.role = ReplicaRole::None;
        *driver.public_owner().session_publications.lock().unwrap() = Some(pending);
    }
    assert!(source.driver.pg_establish_session(&first).await.is_err());
    assert!(
        message(&target, &first, SessionAction::Reserve)
            .await
            .is_err()
    );
    for driver in [&source.driver, &target.driver] {
        *driver.public_owner().session_publications.lock().unwrap() = Some(observed.clone());
    }
    source.driver.pg_retire_session(&first).await.unwrap();
    for _ in 1..super::super::session::SESSION_LIMIT {
        let id = source
            .driver
            .pg_propose_session(observed.0.clone(), observed.1.clone())
            .await
            .unwrap();
        source.driver.pg_retire_session(&id).await.unwrap();
    }
    assert!(
        source
            .driver
            .pg_propose_session(observed.0.clone(), observed.1.clone())
            .await
            .is_err()
    );
    assert!(
        message(&target, &first, SessionAction::Reserve)
            .await
            .is_err()
    );
    Replicator::close(source.driver.as_ref()).await.unwrap();
    Replicator::close(target.driver.as_ref()).await.unwrap();
}
