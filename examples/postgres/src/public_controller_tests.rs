//! Native PostgreSQL through the production ordinary Reconciler.
use super::*;
use crate::instance::DriverExitGate;
use crate::service::{PgService, PgServiceConfig};
use kuberic_controller::test_util::{
    ApplicationFactory, CreatedApplication, ErrorKind, ReplicaContext, ReplicaRef, TestCluster,
    TestError,
};
use kuberic_runtime::protocol::public_operations::{
    RemoteReplicaState, ReplicaGoal, ReplicaGoalKind, ReplicaGoalStatus, ReplicaPublicationStage,
};
use std::sync::Mutex as StdMutex;

type TestResult<T = ()> = kuberic_controller::test_util::Result<T>;

struct ReleaseGate(Arc<BuildCompletionGate>);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release.notify_one();
    }
}

struct ReleaseNotify<'a>(&'a tokio::sync::Notify);
impl Drop for ReleaseNotify<'_> {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

fn error(source: impl std::error::Error + Send + Sync + 'static) -> TestError {
    let mut diagnostic = source.to_string();
    let mut cause = source.source();
    while let Some(next) = cause {
        diagnostic.push_str(&format!(": {next}"));
        cause = next.source();
    }
    TestError::with_source(ErrorKind::Application, diagnostic, source)
}

fn check(condition: bool, message: &str) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(TestError::new(ErrorKind::Application, message))
    }
}

fn finished(result: TestResult) {
    result.unwrap_or_else(|failure| {
        panic!(
            "{failure:?}; sources: {:?}",
            failure
                .sources()
                .map(|source| source.to_string())
                .collect::<Vec<_>>()
        )
    });
}

struct Listener {
    service: Arc<PgService>,
    address: std::net::SocketAddr,
    alias: std::path::PathBuf,
}

#[derive(Clone, Default)]
struct Factory(Arc<StdMutex<Vec<Listener>>>);

#[async_trait]
impl ApplicationFactory for Factory {
    type Service = PgService;

    async fn create(&self, context: ReplicaContext) -> TestResult<CreatedApplication<PgService>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        drop(listener);
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let parent = std::env::var_os("KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/postgres-public-v1")
            });
        std::fs::create_dir_all(&parent)?;
        let alias = parent.canonicalize()?.join(format!(
            "c{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::os::unix::fs::symlink(context.application_root(), &alias)?;
        let config = PgServiceConfig {
            resource_uid: ResourceUid::new(
                context
                    .environment()
                    .get("KUBERIC_RESOURCE_UID")
                    .ok_or_else(|| {
                        TestError::new(ErrorKind::InvalidConfiguration, "resource UID missing")
                    })?,
            ),
            application_root: context.application_root().join("metadata"),
            pg_data: alias.join("d"),
            pg_bin: find_pg_bin(),
            pg_port: allocate_port().await,
            replication_address: format!("http://{address}"),
        };
        let storage = PgService::storage_state(&config)?;
        let paths = vec![
            config.application_root.clone(),
            context.application_root().join("d"),
        ];
        let service = Arc::new(
            PgService::deferred(config)
                .with_coordination_token("pg-controller-fixture-coordination".into()),
        );
        self.0.lock().unwrap().push(Listener {
            service: service.clone(),
            address,
            alias,
        });
        Ok(CreatedApplication::new(service, storage).with_storage_paths(paths))
    }

    async fn shutdown_and_drain(&self, service: Arc<PgService>) -> TestResult {
        let listener = {
            let mut listeners = self.0.lock().unwrap();
            listeners
                .iter()
                .position(|listener| Arc::ptr_eq(&listener.service, &service))
                .map(|index| listeners.remove(index))
        };
        let mut failures = Vec::new();
        if let Err(source) = kuberic_runtime::StatefulServiceReplica::close(service.as_ref()).await
        {
            failures.push(source.to_string());
        }
        if let Some(listener) = listener {
            if tokio::net::TcpStream::connect(listener.address)
                .await
                .is_ok()
            {
                failures.push("ordinary engine endpoint survived Close".into());
            }
            if let Err(source) = std::fs::remove_file(listener.alias) {
                failures.push(source.to_string());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(TestError::new(ErrorKind::Cleanup, failures.join("; ")))
        }
    }
}

async fn application_connection(
    service: &PgService,
) -> TestResult<(tokio_postgres::Client, crate::owned_task::PgTask)> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(connection) = service.instance().connect_application().await {
                return connection;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(error)
}

async fn administrative_database(
    service: &PgService,
) -> TestResult<(tokio_postgres::Client, tokio::task::JoinHandle<()>)> {
    let connection_string = service.instance().connection_string().replace(
        "dbname=postgres",
        &format!("dbname={}", crate::access::APPLICATION_DATABASE),
    );
    let (client, connection) = tokio_postgres::connect(&connection_string, tokio_postgres::NoTls)
        .await
        .map_err(error)?;
    Ok((
        client,
        tokio::spawn(async move {
            let _ = connection.await;
        }),
    ))
}

async fn cluster(factory: &Factory) -> TestCluster<Factory> {
    TestCluster::builder(factory.clone())
        .initial_replicas(1)
        .scenario_timeout(Duration::from_secs(180))
        .teardown_timeout(Duration::from_secs(30))
        .start()
        .await
        .unwrap()
}

async fn gate(cluster: &mut TestCluster<Factory>, gate: &BuildCompletionGate) -> TestResult {
    cluster
        .drive(async {
            gate.entered.notified().await;
            Ok::<_, TestError>(())
        })
        .await
}

async fn check_value(sql: &tokio_postgres::Client) -> TestResult {
    let row = sql
        .query_one("SELECT value FROM m2_native_admission WHERE id = 1", &[])
        .await
        .map_err(error)?;
    check(
        row.get::<_, &str>(0) == "acknowledged-before-copy",
        "native copy lost acknowledged SQL",
    )?;
    Ok(())
}

async fn checked_data(service: &PgService, primary: bool) -> TestResult {
    if primary {
        let (sql, connection) = application_connection(service).await?;
        check_value(&sql).await?;
        drop(sql);
        connection.await.map_err(error)
    } else {
        check(
            service.instance().connect_application().await.is_err(),
            "generic host did not grant stable-secondary ordinary SQL",
        )?;
        let (sql, connection) = administrative_database(service).await?;
        check_value(&sql).await?;
        drop(sql);
        connection.await.map_err(error)
    }
}

async fn seed(service: &PgService) -> TestResult {
    let (sql, connection) = application_connection(service).await?;
    sql.batch_execute(
        "CREATE TABLE m2_native_admission(id integer primary key, value text);
        INSERT INTO m2_native_admission VALUES (1, 'acknowledged-before-copy')",
    )
    .await
    .map_err(error)?;
    drop(sql);
    connection.await.map_err(error)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CopyCut {
    None,
    SourcePreEffect,
    TargetPreEffect,
    Checkpoint,
    SourceCredit,
    LostInbound,
    Inspect,
    LostCancel,
}

async fn rejected_copy(
    cluster: &mut TestCluster<Factory>,
    ordinary: (&ReplicaRef, &ReplicaRef, &ReplicaGoal),
    native: (&PgReplicator, &PgReplicator, &session::PgSessionId),
    native_rejected: bool,
) -> TestResult {
    let (source_ref, target_ref, goal) = ordinary;
    let (source, target, id) = native;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let observation = cluster.observe().await?;
            let snapshot = observation.snapshot(source_ref).ok_or_else(|| {
                TestError::new(ErrorKind::Application, "retained source journal missing")
            })?;
            if let Some(result) = snapshot.goals.iter().find(|entry| entry.goal == *goal)
                && result.status != ReplicaGoalStatus::Pending
            {
                check(
                    matches!(result.status, ReplicaGoalStatus::Failed(_)),
                    &format!("released ordinary Build was not rejected: {result:?}"),
                )?;
                for reference in [source_ref, target_ref] {
                    let snapshot = observation.snapshot(reference).unwrap();
                    check(
                        !snapshot.remote_operations.iter().any(|operation| {
                            operation.selection.target == id.target
                                && operation.state == RemoteReplicaState::Ready
                        }),
                        "released stale Build published ordinary Ready",
                    )?;
                    check(
                        snapshot.publication.as_ref().is_none_or(|publication| {
                            publication.scope.configuration.current
                                == id.source.scope.configuration.current
                        }),
                        "failed copy installed a successor configuration",
                    )?;
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, TestError>(())
    })
    .await
    .map_err(error)??;
    if let Some(owner) = target.pg_native_inbounds().into_iter().next() {
        check(
            owner.wait().await.is_err() == native_rejected,
            "released native owner's result did not match the actual invalidation cut",
        )?;
        target
            .public_join_inbound_owner(&owner)
            .await
            .map_err(error)?;
    }
    source.pg_retire_session(id).await.map_err(error)?;
    check(
        !source
            .public_owner()
            .state
            .lock()
            .await
            .built_idle
            .contains_key(&target_ref.identity().replica_id)
            && target.public_owner().state.lock().await.built.is_none()
            && target.public_owner().state.lock().await.receiver.is_none()
            && !target.pg_session_ownership(id).1
            && !target.instance.is_running().await,
        "released invalidated work retained stale credit or installed a late receiver",
    )?;
    check(
        target
            .pg_session_request(session::SessionRequest {
                session: id.clone(),
                action: session::SessionAction::Reserve,
            })
            .await
            .is_err(),
        "retired copy revived its receiver after the released result",
    )
}

async fn formation(cut: CopyCut, fail_cleanup: bool) {
    let factory = Factory::default();
    let mut cluster = cluster(&factory).await;
    let mut expected_cleanup_failure = false;
    let result: TestResult = async {
        let topology = cluster.settle_members(1).await?;
        eprintln!("native {cut:?}: singleton achieved");
        let source = cluster.service(topology.primary()).await?;
        seed(&source).await?;
        let source_effect = source.native_driver().pause_public_build_effects();
        let source_credit = source.native_driver().pause_public_build_completion();
        let _release_source = ReleaseGate(source_effect.clone());
        let _release_credit = ReleaseGate(source_credit.clone());
        cluster.scale_to(2).await?;
        gate(&mut cluster, &source_effect).await?;
        let observation = cluster.observe().await?;
        let target_ref = observation
            .replicas()
            .find(|reference| reference.identity() != topology.primary().identity())
            .cloned()
            .ok_or_else(|| {
                TestError::new(ErrorKind::Application, "ordinary prepared target missing")
            })?;
        let target = cluster.service(&target_ref).await?;
        let source_driver = source.native_driver();
        let target_driver = target.native_driver();
        let build_goal = observation
            .snapshot(topology.primary())
            .unwrap()
            .goals
            .iter()
            .find(|entry| matches!(entry.goal.desired, ReplicaGoalKind::Build { .. }))
            .ok_or_else(|| TestError::new(ErrorKind::Application, "retained Build goal missing"))?
            .goal
            .clone();
        let id = source_driver
            .pg_session_ids_for_target(target_ref.identity().replica_id)
            .into_iter()
            .next()
            .ok_or_else(|| TestError::new(ErrorKind::Application, "source session missing"))?;
        check(
            id.source.scope.storage == *topology.primary().storage_incarnation()
                && id.target.scope.storage == *target_ref.storage_incarnation()
                && id.target.scope.process_session_id == *target_ref.process_session_id(),
            "native session lost actual production host identity",
        )?;
        let target_effect = target_driver.pause_public_build_effects();
        let checkpoint = target_driver.pause_public_build_completion();
        let _release_target = ReleaseGate(target_effect.clone());
        let _release_checkpoint = ReleaseGate(checkpoint.clone());
        let pre_effect_metadata = target_driver.durable.snapshot().await.generation;
        if cut == CopyCut::SourcePreEffect {
            source_driver
                .remove_replica(target_ref.identity().replica_id)
                .await
                .map_err(error)?;
            source_effect.release.notify_one();
            rejected_copy(
                &mut cluster,
                (topology.primary(), &target_ref, &build_goal),
                (source_driver, target_driver, &id),
                true,
            )
            .await?;
            check(
                !target.instance().data_dir().join("PG_VERSION").exists()
                    && target_driver.durable.snapshot().await.generation == pre_effect_metadata
                    && target_driver
                        .public_owner()
                        .effect_starts
                        .load(std::sync::atomic::Ordering::SeqCst)
                        == 0,
                "retired pre-effect source started a backup",
            )?;
            check(
                source_driver
                    .public_owner()
                    .fence
                    .context()
                    .map_err(error)?
                    .role
                    == ReplicaRole::Primary,
                "Remove used source role loss as a fence",
            )?;
            return Ok(());
        }
        source_effect.release.notify_one();
        gate(&mut cluster, &target_effect).await?;
        eprintln!("native {cut:?}: two-ended session and source gateway established");
        let binding = source_driver
            .public_owner()
            .state
            .lock()
            .await
            .grants
            .iter()
            .find_map(|grant| match &grant.binding {
                TransportBinding::Build(binding) if binding.session.as_deref() == Some(&id) => {
                    Some(binding.clone())
                }
                _ => None,
            })
            .ok_or_else(|| {
                TestError::new(
                    ErrorKind::Application,
                    "actual native gateway owner missing",
                )
            })?;
        let observer = source_driver.clone();
        let observed_binding = binding.clone();
        let lost_observer = tokio::spawn(async move {
            observer
                .public_rpc(
                    &observed_binding.target.replication_address,
                    &Request::Build(observed_binding.clone()),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(15), async {
            while target_driver.public_build_waiters() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(error)?;
        lost_observer.abort();
        let _ = lost_observer.await;
        let serialized = serde_json::to_value(&id).map_err(error)?;
        let before = target_driver.durable.snapshot().await.generation;
        for change in 0..8 {
            let mut stale = serialized.clone();
            match change {
                0 => stale["nonce"][0] = serde_json::json!(id.nonce[0] ^ 1),
                1 => stale["generation"] = serde_json::json!(id.generation + 1),
                2 => stale["target"]["scope"]["creationId"] = serde_json::json!("stale-creation"),
                3 => {
                    stale["target"]["scope"]["storage"]["storageGeneration"] =
                        serde_json::json!("stale-storage")
                }
                4 => {
                    stale["source"]["scope"]["processSessionId"] =
                        serde_json::json!("stale-process")
                }
                5 => {
                    stale["target"]["replicationAddress"] = serde_json::json!("http://127.0.0.1:1")
                }
                6 => {
                    stale["source"]["scope"]["history"]["historyId"] =
                        serde_json::json!("stale-history")
                }
                _ => {
                    stale["source"]["scope"]["epoch"]["configurationNumber"] = serde_json::json!(2)
                }
            }
            let mut changed = binding.clone();
            changed.session = Some(Box::new(serde_json::from_value(stale).map_err(error)?));
            check(
                target_driver
                    .public_request(Request::Build(changed))
                    .await
                    .is_err(),
                "crossed native session reached physical effects",
            )?;
        }
        check(
            target_driver.durable.snapshot().await.generation == before
                && !target.instance().data_dir().join("PG_VERSION").exists(),
            "rejected session mutated native data/metadata",
        )?;
        if cut == CopyCut::TargetPreEffect {
            let slot = crate::native::replication_slot_name(target_ref.identity());
            let slot_path = source.instance().data_dir().join("pg_replslot").join(slot);
            check(
                slot_path.exists(),
                "actual exact native slot was not reserved before transfer",
            )?;
            source_driver
                .remove_replica(target_ref.identity().replica_id)
                .await
                .map_err(error)?;
            check(
                !slot_path.exists(),
                "source local retirement retained an obsolete exact native slot",
            )?;
            target_effect.release.notify_one();
            rejected_copy(
                &mut cluster,
                (topology.primary(), &target_ref, &build_goal),
                (source_driver, target_driver, &id),
                true,
            )
            .await?;
            check(
                !target.instance().data_dir().join("PG_VERSION").exists()
                    && target_driver.durable.snapshot().await.generation == pre_effect_metadata
                    && target_driver
                        .public_owner()
                        .effect_starts
                        .load(std::sync::atomic::Ordering::SeqCst)
                        == 0,
                "released target pre-effect callback mutated PGDATA or metadata",
            )?;
            check(
                source_driver
                    .public_owner()
                    .fence
                    .context()
                    .map_err(error)?
                    .role
                    == ReplicaRole::Primary,
                "session retirement depended on source demotion",
            )?;
            return Ok(());
        }
        if cut == CopyCut::LostInbound {
            let owner = target_driver
                .pg_native_inbounds()
                .into_iter()
                .next()
                .ok_or_else(|| {
                    TestError::new(ErrorKind::Application, "native inbound owner missing")
                })?;
            owner.task.lock().await.task.as_ref().unwrap().abort();
            let first = target_driver.public_close().await.unwrap_err().to_string();
            let second = target_driver.public_close().await.unwrap_err().to_string();
            check(
                first == second && !target_driver.public_closed(),
                "lost native join became successful settlement",
            )?;
            let join = owner.task.lock().await;
            check(
                join.task.is_none() && join.result.as_ref().is_some_and(|result| result.is_err()),
                "native lost join was not retained",
            )?;
            expected_cleanup_failure = true;
            return Ok(());
        }
        target_effect.release.notify_one();
        gate(&mut cluster, &checkpoint).await?;
        eprintln!("native {cut:?}: actual backup/receive/replay/checkpoint reached");
        let snapshot = target_driver.observer.snapshot().await.map_err(error)?;
        let evidence = snapshot
            .evidence
            .ok_or_else(|| TestError::new(ErrorKind::Application, "native WAL evidence absent"))?;
        check(
            evidence.in_recovery
                && evidence.received_lsn.is_some()
                && evidence.replay_lsn.is_some(),
            "copy reached checkpoint without actual receive and replay",
        )?;
        check(
            target.instance().connect_application().await.is_err(),
            "physical copy admitted ordinary SQL before role/configuration",
        )?;
        let identity = {
            let state = source_driver.public_owner().state.lock().await;
            let grant = state
                .grants
                .iter()
                .find(|grant| grant.binding.session() == Some(&id))
                .unwrap();
            check(
                evidence
                    .received_lsn
                    .is_some_and(|lsn| lsn >= grant.boundary)
                    && evidence.replay_lsn.is_some_and(|lsn| lsn >= grant.boundary),
                "receive/replay did not cross captured source flush boundary",
            )?;
            check(
                tokio::time::timeout(
                    Duration::from_secs(5),
                    native_socket(&grant.credential, "stale_session_ack"),
                )
                .await
                .map_err(error)?
                .is_err(),
                "native gateway accepted stale ACK application name",
            )?;
            (
                grant.credential.host.clone(),
                grant.credential.port,
                grant.credential.name.clone(),
                grant.session.clone(),
            )
        };
        if cut == CopyCut::LostCancel {
            target_driver
                .public_owner()
                .retirement_unavailable
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let exit = Arc::new(DriverExitGate::default());
            let (admin, admin_driver) = target
                .instance()
                .connect_with_driver_exit(exit.clone())
                .await
                .map_err(error)?;
            admin.simple_query("SELECT 1").await.map_err(error)?;
            source_driver
                .remove_replica(target_ref.identity().replica_id)
                .await
                .map_err(error)?;
            check(
                source_driver.pg_retire_session(&id).await.is_err(),
                "lost target cancellation fabricated remote containment",
            )?;
            check(
                target_driver.pg_session_ownership(&id).1
                    && target.instance().is_running().await
                    && source_driver
                        .public_owner()
                        .fence
                        .context()
                        .map_err(error)?
                        .role
                        == ReplicaRole::Primary,
                "source Remove secretly used target settlement or source demotion",
            )?;
            let mut successor = id.clone();
            successor.nonce[0] ^= 1;
            check(
                target_driver
                    .public_request(Request::Session(Box::new(session::SessionRequest {
                        session: successor,
                        action: session::SessionAction::Reserve,
                    })))
                    .await
                    .is_err(),
                "uncertain native receiver admitted a successor",
            )?;
            checkpoint.release.notify_one();
            exit.entered.notified().await;
            check(
                target_driver.pg_session_ownership(&id).1,
                "issued native work released ownership before real driver settlement",
            )?;
            target_driver
                .public_owner()
                .retirement_unavailable
                .store(false, std::sync::atomic::Ordering::SeqCst);
            let retiring = source_driver.clone();
            let retired = id.clone();
            let join = tokio::spawn(async move { retiring.pg_retire_session(&retired).await });
            check(
                !join.is_finished(),
                "remote cancellation bypassed held native join",
            )?;
            exit.release.notify_one();
            admin_driver.await.map_err(error)?;
            drop(admin);
            join.await.map_err(error)?.map_err(error)?;
            check(
                !target_driver.pg_session_ownership(&id).1
                    && target.instance().data_dir().join("PG_VERSION").exists(),
                "retirement erased PGDATA or failed to retain actual join",
            )?;
            source_credit.release.notify_one();
            return Ok(());
        }
        if cut == CopyCut::Checkpoint {
            let exit = Arc::new(DriverExitGate::default());
            let _release_exit = ReleaseNotify(&exit.release);
            let (admin, admin_driver) = target
                .instance()
                .connect_with_driver_exit(exit.clone())
                .await
                .map_err(error)?;
            admin.simple_query("SELECT 1").await.map_err(error)?;
            let owner = target_driver
                .pg_native_inbounds()
                .into_iter()
                .next()
                .unwrap();
            source_driver
                .remove_replica(target_ref.identity().replica_id)
                .await
                .map_err(error)?;
            checkpoint.release.notify_one();
            exit.entered.notified().await;
            check(
                target_driver.pg_session_ownership(&id).1
                    && matches!(
                        target_driver.pg_session_ownership(&id).0,
                        session::SessionStage::Retiring | session::SessionStage::Quarantined
                    )
                    && tokio::time::timeout(Duration::from_millis(100), owner.wait())
                        .await
                        .is_err(),
                "issued checkpoint effect escaped its actual held native cleanup",
            )?;
            let mut successor = id.clone();
            successor.nonce[0] ^= 1;
            check(
                target_driver
                    .pg_session_request(session::SessionRequest {
                        session: successor,
                        action: session::SessionAction::Reserve,
                    })
                    .await
                    .is_err(),
                "held checkpoint cleanup admitted a successor receiver",
            )?;
            exit.release.notify_one();
            admin_driver.await.map_err(error)?;
            drop(admin);
            rejected_copy(
                &mut cluster,
                (topology.primary(), &target_ref, &build_goal),
                (source_driver, target_driver, &id),
                true,
            )
            .await?;
            check(
                target.instance().data_dir().join("PG_VERSION").exists(),
                "invalidated checkpoint cleanup erased issued native effects",
            )?;
            check(
                source_driver
                    .public_owner()
                    .fence
                    .context()
                    .map_err(error)?
                    .role
                    == ReplicaRole::Primary,
                "checkpoint fencing used source demotion",
            )?;
            return Ok(());
        }
        checkpoint.release.notify_one();
        gate(&mut cluster, &source_credit).await?;
        eprintln!("native {cut:?}: native copy completion held before source credit");
        let generation = target.instance().generation_id();
        let metadata = target_driver.durable.snapshot().await.generation;
        eprintln!("native {cut:?}: installing observational replay gate");
        let replay_gate = target_driver.pause_public_build_replay();
        let _release_replay = ReleaseGate(replay_gate.clone());
        let observer = source_driver.clone();
        let observed_binding = binding.clone();
        let mut lost_replay = tokio::spawn(async move {
            observer
                .public_rpc(
                    &observed_binding.target.replication_address,
                    &Request::Build(observed_binding.clone()),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(15), async {
            tokio::select! {
                _ = replay_gate.entered.notified() => Ok(()),
                result = &mut lost_replay => {
                    let diagnostic = match result {
                        Ok(Err(error)) => error.to_string(),
                        Ok(Ok(response)) => serde_json::to_string(&response).unwrap(),
                        Err(error) => error.to_string(),
                    };
                    Err(TestError::new(ErrorKind::Application,
                        format!("cached replay returned before observational gate: {diagnostic}")))
                }
            }
        })
        .await
        .map_err(error)??;
        eprintln!("native {cut:?}: cached replay observer reached retained result");
        lost_replay.abort();
        let _ = lost_replay.await;
        replay_gate.release.notify_one();
        if cut == CopyCut::Inspect {
            let inspect = source_driver.pause_public_source_inspect();
            let _release_inspect = ReleaseGate(inspect.clone());
            let driver = source_driver.clone();
            let replay_binding = binding.clone();
            let replay = tokio::spawn(async move {
                driver
                    .public_rpc(
                        &replay_binding.target.replication_address,
                        &Request::Build(replay_binding.clone()),
                    )
                    .await
            });
            inspect.entered.notified().await;
            let closing = target_driver.clone();
            let close = tokio::spawn(async move { closing.public_close().await });
            tokio::time::timeout(Duration::from_secs(10), async {
                while target.instance().is_running().await {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .map_err(error)?;
            inspect.release.notify_one();
            check(
                replay.await.map_err(error)?.is_err(),
                "cancelled cached source inspection credited a completion",
            )?;
            close.await.map_err(error)?.map_err(error)?;
            check(
                target.instance().data_dir().join("PG_VERSION").exists(),
                "read cancellation cleanup erased copied PGDATA",
            )?;
            source_credit.release.notify_one();
            return Ok(());
        } else {
            let replay = tokio::time::timeout(
                Duration::from_secs(15),
                source_driver.public_rpc(
                    &binding.target.replication_address,
                    &Request::Build(binding.clone()),
                ),
            )
            .await
            .map_err(error)?
            .map_err(error)?;
            check(
                matches!(replay, Response::Built(ref completed) if completed.binding == binding),
                "exact cached replay did not attach to retained native completion",
            )?;
        }
        eprintln!("native {cut:?}: exact cached result revalidated without mutation");
        check(
            target.instance().generation_id() == generation
                && target_driver.durable.snapshot().await.generation == metadata
                && target.instance().is_running().await,
            "read/replay acquired mutation or cleanup ownership",
        )?;
        if cut == CopyCut::SourceCredit {
            source_driver
                .remove_replica(target_ref.identity().replica_id)
                .await
                .map_err(error)?;
            source_credit.release.notify_one();
            rejected_copy(
                &mut cluster,
                (topology.primary(), &target_ref, &build_goal),
                (source_driver, target_driver, &id),
                false,
            )
            .await?;
            check(
                target_driver
                    .public_request(Request::Build(binding.clone()))
                    .await
                    .is_err()
                    && target.instance().data_dir().join("PG_VERSION").exists(),
                "released source final-credit callback replayed stale target completion",
            )?;
            return Ok(());
        }
        source_credit.release.notify_one();
        let topology = cluster.settle_members(2).await?;
        let observation = cluster.observe().await?;
        for reference in topology.members() {
            let publication = observation
                .snapshot(reference)
                .and_then(|snapshot| snapshot.publication.as_ref())
                .ok_or_else(|| {
                    TestError::new(
                        ErrorKind::Application,
                        "current ordinary publication absent",
                    )
                })?;
            check(
                publication.stage == ReplicaPublicationStage::AchievedScope
                    && publication.scope.configuration.previous.is_none(),
                "native admission preceded exact current-only receipts",
            )?;
            let service = cluster.service(reference).await?;
            checked_data(
                &service,
                reference.identity() == topology.primary().identity(),
            )
            .await?;
        }
        {
            let state = source_driver.public_owner().state.lock().await;
            let grant = state
                .grants
                .iter()
                .find(|grant| grant.binding.session() == Some(&id))
                .ok_or_else(|| {
                    TestError::new(
                        ErrorKind::Application,
                        "native handover replaced its session",
                    )
                })?;
            check(
                (
                    grant.credential.host.clone(),
                    grant.credential.port,
                    grant.credential.name.clone(),
                    grant.session.clone(),
                ) == identity,
                "copy/joint/current changed gateway or exact-session ACK owner",
            )?;
            check(
                matches!(grant.binding, TransportBinding::Peer(_)),
                "ordinary target callbacks never completed native peer handover",
            )?;
        }
        if fail_cleanup {
            source.fail_next_close();
            expected_cleanup_failure = true;
        }
        Ok(())
    }
    .await;
    let passed = result.is_ok();
    let outcome = cluster.finish(result).await;
    assert!(
        passed,
        "native formation {cut:?}: {outcome:?}; sources: {:?}",
        outcome.as_ref().err().map(|failure| failure
            .sources()
            .map(|source| source.to_string())
            .collect::<Vec<_>>())
    );
    if expected_cleanup_failure {
        assert!(outcome.is_err());
    } else {
        finished(outcome);
    }
    assert!(factory.0.lock().unwrap().is_empty());
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_ordinary_fresh_admission() {
    formation(CopyCut::None, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_controller_formation() {
    formation(CopyCut::None, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_controller_cleanup_failure() {
    formation(CopyCut::None, true).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_controller_pre_effect_invalidation() {
    formation(CopyCut::SourcePreEffect, false).await;
    formation(CopyCut::TargetPreEffect, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_lost_target_cancel_quarantines_receiver() {
    formation(CopyCut::LostCancel, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_native_copy_boundaries() {
    formation(CopyCut::None, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_cached_replay_is_nonmutating_and_reconfirmed() {
    formation(CopyCut::None, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_crossed_identity_and_stale_completion() {
    formation(CopyCut::TargetPreEffect, false).await;
    formation(CopyCut::SourceCredit, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_effect_fences_and_retained_native_join() {
    formation(CopyCut::Checkpoint, false).await;
    formation(CopyCut::LostInbound, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_slot_retirement_and_exact_replay_membership() {
    formation(CopyCut::TargetPreEffect, false).await;
    formation(CopyCut::None, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_delayed_first_establishment_after_retirement() {
    let factory = Factory::default();
    let mut cluster = cluster(&factory).await;
    let result: TestResult = async {
        let topology = cluster.settle_members(1).await?;
        let source = cluster.service(topology.primary()).await?;
        seed(&source).await?;
        let confirmation = Arc::new(BuildCompletionGate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let _release = ReleaseGate(confirmation.clone());
        *source
            .native_driver()
            .public_owner()
            .session_confirmation_gate
            .lock()
            .unwrap() = Some(confirmation.clone());
        cluster.scale_to(2).await?;
        gate(&mut cluster, &confirmation).await?;
        let observed = cluster.observe().await?;
        let reference = observed
            .replicas()
            .find(|reference| reference.identity() != topology.primary().identity())
            .cloned()
            .unwrap();
        let target = cluster.service(&reference).await?;
        let driver = source.native_driver();
        let id = driver
            .pg_session_ids_for_target(reference.identity().replica_id)
            .into_iter()
            .next()
            .unwrap();
        let before = target.native_driver().durable.snapshot().await;
        check(
            target.native_driver().pg_session_ownership(&id).1,
            "target did not reserve before delayed source activation",
        )?;
        driver
            .remove_replica(reference.identity().replica_id)
            .await
            .map_err(error)?;
        check(
            driver.public_owner().fence.context().map_err(error)?.role == ReplicaRole::Primary,
            "delayed source activation was fenced only by role loss",
        )?;
        confirmation.release.notify_one();
        check(
            driver
                .pg_session_request(session::SessionRequest {
                    session: id.clone(),
                    action: session::SessionAction::Activate,
                })
                .await
                .is_err(),
            "retired source activated delayed first establishment",
        )?;
        driver.pg_retire_session(&id).await.map_err(error)?;
        check(
            target.native_driver().durable.snapshot().await.generation == before.generation
                && !target.instance().data_dir().join("PG_VERSION").exists()
                && target
                    .native_driver()
                    .public_owner()
                    .effect_starts
                    .load(std::sync::atomic::Ordering::SeqCst)
                    == 0,
            "delayed first establishment mutated native target state",
        )?;
        Ok(())
    }
    .await;
    finished(cluster.finish(result).await);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_controller_delayed_build_invalidation() {
    formation(CopyCut::Checkpoint, false).await;
    formation(CopyCut::SourceCredit, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_graceful_close_cancels_source_inspect() {
    formation(CopyCut::Inspect, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_lost_inbound_join_is_sticky_across_close() {
    formation(CopyCut::LostInbound, false).await;
}

async fn terminal(fault: u8) {
    let factory = Factory::default();
    let mut cluster = cluster(&factory).await;
    let result: TestResult = async {
        let topology = cluster.settle_members(1).await?;
        let source = cluster.service(topology.primary()).await?;
        seed(&source).await?;
        let probe = ProcessProbe::postgres(source.instance().data_dir());
        if fault == 1 {
            source.fail_next_close();
        }
        if fault == 2 {
            source.instance().lose_monitor().await;
        }
        let goal = cluster
            .retain_ordinary_terminal(
                topology.primary(),
                kuberic_runtime::protocol::public_operations::PublicV1CallKind::Close,
            )
            .await?;
        let completion = loop {
            cluster.step().await?;
            let observed = cluster.observe().await?;
            let snapshot = observed.snapshot(topology.primary()).unwrap();
            if let Some(completion) = snapshot.goals.iter().find(|entry| entry.goal == goal)
                && completion.status != ReplicaGoalStatus::Pending
            {
                check(
                    snapshot.withdrawn
                        && snapshot.publication.is_none()
                        && snapshot.read_status != AccessStatus::Granted
                        && snapshot.write_status != AccessStatus::Granted,
                    "ordinary terminal retained publication or SQL access",
                )?;
                break completion.clone();
            }
        };
        check(
            (completion.status == ReplicaGoalStatus::Achieved) == (fault == 0),
            "ordinary terminal hid native/application join failure",
        )?;
        probe.assert_gone().await;
        let repeated = source.native_driver().public_close().await;
        check(
            repeated.is_ok() == (fault != 2),
            "repeated native Close lost sticky actual-join outcome",
        )?;
        cluster.recreate_controller().await?;
        for _ in 0..3 {
            cluster.step().await?;
        }
        let observed = cluster.observe().await?;
        check(
            observed
                .snapshot(topology.primary())
                .unwrap()
                .goals
                .iter()
                .any(|entry| entry == &completion),
            "terminal replay changed the retained actual result",
        )?;
        check(
            source.native_driver().fault_owners_joined(),
            "ordinary terminal lost the actual retained fault reporter join",
        )?;
        Ok(())
    }
    .await;
    let passed = result.is_ok();
    let outcome = cluster.finish(result).await;
    assert!(passed, "native terminal fault={fault}: {outcome:?}");
    if fault != 0 {
        assert!(outcome.is_err());
    } else {
        finished(outcome);
    }
    assert!(factory.0.lock().unwrap().is_empty());
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_controller_terminal_success() {
    terminal(0).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_controller_terminal_child_close_failure() {
    terminal(1).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_controller_terminal_lost_native_join() {
    terminal(2).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_controller_undeclared_role_failure_is_not_retried() {
    let factory = Factory::default();
    let mut cluster = cluster(&factory).await;
    let result: TestResult = async {
        let topology = cluster.settle_members(1).await?;
        let source = cluster.service(topology.primary()).await?;
        let callbacks = source.role_callback_count();
        source.fail_next_role();
        cluster.scale_to(2).await?;
        let effect = source.native_driver().pause_public_build_effects();
        gate(&mut cluster, &effect).await?;
        effect.release.notify_one();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let observation = cluster.observe().await?;
            if observation
                .snapshot(topology.primary())
                .is_some_and(|snapshot| {
                    snapshot.goals.iter().any(|goal| {
                        matches!(&goal.status, ReplicaGoalStatus::Failed(_))
                            && matches!(goal.goal.desired, ReplicaGoalKind::Reconfigure { .. })
                    })
                })
            {
                break;
            }
            check(
                tokio::time::Instant::now() < deadline,
                "role callback failure not reported",
            )?;
            cluster.step().await?;
        }
        check(
            source.role_callback_count() == callbacks + 1,
            "undeclared failed application Role was replayed",
        )?;
        Ok(())
    }
    .await;
    finished(cluster.finish(result).await);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_graceful_close_observes_acknowledged_tail() {
    let factory = Factory::default();
    let mut cluster = cluster(&factory).await;
    let result: TestResult = async {
        let topology = cluster.settle_members(1).await?;
        let source = cluster.service(topology.primary()).await?;
        seed(&source).await?;
        let acknowledged = source
            .native_driver()
            .current_progress()
            .await
            .map_err(error)?;
        source.native_driver().public_close().await.map_err(error)?;
        check(
            source
                .native_driver()
                .durable
                .revalidate()
                .await
                .map_err(error)?
                .policy_certified_lsn
                >= acknowledged,
            "graceful Close lost acknowledged native flush progress",
        )?;
        Ok(())
    }
    .await;
    finished(cluster.finish(result).await);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_provisional_gateway_is_joined_after_cancel() {
    let factory = Factory::default();
    let mut cluster = cluster(&factory).await;
    let result: TestResult = async {
        let topology = cluster.settle_members(1).await?;
        let source = cluster.service(topology.primary()).await?;
        let publication = source.native_driver().pause_public_grant_publication();
        cluster.scale_to(2).await?;
        cluster
            .drive(async {
                publication.entered.notified().await;
                Ok::<_, TestError>(())
            })
            .await?;
        let probe = publication.gateway.lock().unwrap().clone().ok_or_else(|| {
            TestError::new(ErrorKind::Application, "provisional native gateway absent")
        })?;
        check(
            !probe.abort.is_finished(),
            "provisional ownership did not retain a real native gateway task",
        )?;
        let driver = source.native_driver().clone();
        driver.abort();
        let close = tokio::spawn(async move { driver.public_close().await });
        check(
            !close.is_finished(),
            "Close abandoned before-await provisional gateway",
        )?;
        publication.release.notify_one();
        probe.exit.entered.notified().await;
        check(
            !close.is_finished(),
            "Close released an unjoined native gateway",
        )?;
        probe.exit.release.notify_one();
        close.await.map_err(error)?.map_err(error)?;
        check(
            tokio::net::TcpStream::connect(probe.address).await.is_err(),
            "provisional native listener survived actual join",
        )?;
        Ok(())
    }
    .await;
    finished(cluster.finish(result).await);
}

pub(super) async fn source_service_abort_reclaims_native_work() {
    let factory = Factory::default();
    let mut cluster = cluster(&factory).await;
    let result: TestResult = async {
        let topology = cluster.settle_members(1).await?;
        let source = cluster.service(topology.primary()).await?;
        seed(&source).await?;
        let publication = source.native_driver().pause_public_grant_publication();
        cluster.scale_to(2).await?;
        cluster
            .drive(async {
                publication.entered.notified().await;
                Ok::<_, TestError>(())
            })
            .await?;
        let probe = publication.gateway.lock().unwrap().clone().ok_or_else(|| {
            TestError::new(ErrorKind::Application, "ordinary source gateway missing")
        })?;
        let credential = {
            publication.release.notify_one();
            probe.exit.release.notify_one();
            let state = source.native_driver().public_owner().state.lock().await;
            state
                .grants
                .first()
                .ok_or_else(|| {
                    TestError::new(
                        ErrorKind::Application,
                        "ordinary source grant disappeared before Abort",
                    )
                })?
                .credential
                .clone()
        };
        let socket = tokio::net::TcpStream::connect((&*credential.host, credential.port)).await?;
        let process = ProcessProbe::postgres(source.instance().data_dir());
        kuberic_runtime::StatefulServiceReplica::abort(source.as_ref());
        assert_abort_reclaimed(source.native_driver(), process, &credential, vec![socket]).await;
        check(
            tokio::net::TcpStream::connect(probe.address).await.is_err(),
            "application Abort retained a native gateway listener",
        )?;
        std::hint::black_box(&source);
        Ok(())
    }
    .await;
    finished(cluster.finish(result).await);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_scope_withdrawal_waits_for_actual_settlement() {
    let factory = Factory::default();
    let mut cluster = cluster(&factory).await;
    let result: TestResult = async {
        let singleton = cluster.settle_members(1).await?;
        let source = cluster.service(singleton.primary()).await?;
        seed(&source).await?;
        cluster.scale_to(2).await?;
        let topology = cluster.settle_members(2).await?;
        let (sql, sql_driver) = application_connection(&source).await?;
        let exit = Arc::new(DriverExitGate::default());
        let _release = ReleaseNotify(&exit.release);
        let (unrelated, unrelated_driver) = source.instance().connect().await.map_err(error)?;
        unrelated.simple_query("SELECT 1").await.map_err(error)?;
        drop(unrelated);
        cluster
            .probe(async { unrelated_driver.await.map_err(error) })
            .await?;
        let (admin, admin_driver) = source
            .instance()
            .connect_with_driver_exit(exit.clone())
            .await
            .map_err(error)?;
        admin.simple_query("SELECT 1").await.map_err(error)?;
        let mut preauth = tokio::net::TcpStream::connect((
            source.instance().listen_host(),
            source.instance().port(),
        ))
        .await?;
        preauth.write_all(&[0, 0]).await?;
        let process = ProcessProbe::postgres(source.instance().data_dir());
        let terminal = cluster
            .retain_ordinary_terminal(
                topology.primary(),
                kuberic_runtime::protocol::public_operations::PublicV1CallKind::Abort,
            )
            .await?;
        cluster
            .drive(async {
                exit.entered.notified().await;
                Ok::<_, TestError>(())
            })
            .await?;
        let observed = cluster.observe().await?;
        let snapshot = observed.snapshot(topology.primary()).unwrap();
        check(
            snapshot.withdrawn
                && snapshot.publication.is_none()
                && snapshot.read_status != AccessStatus::Granted
                && snapshot.write_status != AccessStatus::Granted,
            "withdrawal waited for native joining before denying publication/access",
        )?;
        process.assert_gone().await;
        check(
            sql.simple_query("SELECT 1").await.is_err()
                && admin.simple_query("SELECT 1").await.is_err(),
            "held native Rust join left physical SQL/admin writable",
        )?;
        closed(&mut preauth).await;
        check(
            !source.native_driver().public_closed(),
            "held real driver join fabricated successful Close",
        )?;
        let metadata_root = source
            .instance()
            .data_dir()
            .parent()
            .unwrap()
            .join("metadata");
        let metadata = std::fs::read(metadata_root.join("state-v2.json"))?;
        let identity = source.native_driver().durable.snapshot().await.identity;
        check(
            PgDurableStore::open(&metadata_root, identity, StorageMode::Established)
                .await
                .is_err(),
            "unsettled ordinary creation released its native metadata writer",
        )?;
        check(
            std::fs::read(metadata_root.join("state-v2.json"))? == metadata,
            "rejected writer reuse mutated durable metadata",
        )?;
        let driver = source.native_driver().clone();
        let mut observer = tokio::spawn(async move { Replicator::close(driver.as_ref()).await });
        check(
            tokio::time::timeout(Duration::from_millis(100), &mut observer)
                .await
                .is_err(),
            "ordinary Close observer bypassed actual held settlement",
        )?;
        observer.abort();
        let _ = observer.await;
        exit.release.notify_one();
        admin_driver.await.map_err(error)?;
        sql_driver.await.map_err(error)?;
        drop(admin);
        drop(sql);
        let completion = loop {
            cluster.step().await?;
            let observed = cluster.observe().await?;
            let snapshot = observed.snapshot(topology.primary()).unwrap();
            if let Some(completion) = snapshot.goals.iter().find(|entry| entry.goal == terminal)
                && completion.status != ReplicaGoalStatus::Pending
            {
                break completion.clone();
            }
        };
        check(
            completion.status == ReplicaGoalStatus::Achieved
                && source.native_driver().public_closed(),
            "actual native joins did not settle retained ordinary Abort/Close",
        )?;
        check(
            source.instance().data_dir().join("PG_VERSION").exists(),
            "withdrawal reset acknowledged native storage",
        )?;
        Ok(())
    }
    .await;
    finished(cluster.finish(result).await);
}

async fn peer_receiver(
    driver: &PgReplicator,
    id: &session::PgSessionId,
    joint: bool,
) -> TestResult {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if driver
                .public_owner()
                .state
                .lock()
                .await
                .receiver
                .as_ref()
                .is_some_and(|binding| {
                    matches!(binding, TransportBinding::Peer(peer)
                        if peer.previous_configuration_id.is_some() == joint
                            && peer.session.as_deref() == Some(id))
                })
            {
                return Ok::<_, TestError>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(error)?
}

#[derive(Debug, PartialEq, Eq)]
struct NativeContinuity {
    receiver_generation: u64,
    receiver_pid: i32,
    sender_pid: i32,
    gateway: tokio::task::Id,
    slot_inode: u64,
}

async fn native_continuity(
    source: &PgService,
    target: &PgService,
    id: &session::PgSessionId,
) -> TestResult<NativeContinuity> {
    use std::os::unix::fs::MetadataExt;
    let driver = source.native_driver();
    let (gateway, ack) = {
        let state = driver.public_owner().state.lock().await;
        let grant = state
            .grants
            .iter()
            .find(|grant| grant.binding.session() == Some(id))
            .unwrap();
        (
            grant.gateway.task_abort_handle().id(),
            grant.credential.name.clone(),
        )
    };
    let slot = crate::native::replication_slot_name(&id.target.scope.storage.replica);
    let (sql, connection) = source.instance().connect().await.map_err(error)?;
    let row = sql
        .query_one(
            "SELECT r.pid FROM pg_stat_replication r JOIN pg_replication_slots s
             ON s.active_pid = r.pid WHERE s.slot_name = $1 AND r.application_name = $2",
            &[&slot, &ack],
        )
        .await
        .map_err(error)?;
    let sender_pid = row.get(0);
    drop(sql);
    connection.await.map_err(error)?;
    let (sql, connection) = target.instance().connect().await.map_err(error)?;
    let row = sql
        .query_one(
            "SELECT pid FROM pg_stat_wal_receiver WHERE status = 'streaming'",
            &[],
        )
        .await
        .map_err(error)?;
    let receiver_pid = row.get(0);
    drop(sql);
    connection.await.map_err(error)?;
    Ok(NativeContinuity {
        receiver_generation: target.instance().generation_id(),
        receiver_pid,
        sender_pid,
        gateway,
        slot_inode: std::fs::metadata(source.instance().data_dir().join("pg_replslot").join(slot))?
            .ino(),
    })
}

async fn pending_continuity(already_joint: bool) {
    let factory = Factory::default();
    let mut cluster = cluster(&factory).await;
    let result: TestResult = async {
        let topology = cluster.settle_members(1).await?;
        let source = cluster.service(topology.primary()).await?;
        seed(&source).await?;
        let driver = source.native_driver();
        let effect = driver.pause_public_build_effects();
        let copied = driver.pause_public_build_completion();
        let current_policy = driver.pause_native_cut("source-current-policy");
        let current_receipt = driver.pause_native_cut("source-current-receipt");
        let before_policy = already_joint
            .then(|| driver.pause_native_cut("source-current-before-policy"));
        let _release_before = before_policy.as_ref().map(|gate| ReleaseGate(gate.clone()));
        let _release_effect = ReleaseGate(effect.clone());
        let _release_copied = ReleaseGate(copied.clone());
        let _release_policy = ReleaseGate(current_policy.clone());
        let _release_receipt = ReleaseGate(current_receipt.clone());
        cluster.scale_to(2).await?;
        gate(&mut cluster, &effect).await?;
        let observation = cluster.observe().await?;
        let target_ref = observation
            .replicas()
            .find(|reference| reference.identity() != topology.primary().identity())
            .cloned()
            .ok_or_else(|| {
                TestError::new(ErrorKind::Application, "pending-continuation target absent")
            })?;
        let target = cluster.service(&target_ref).await?;
        let target_driver = target.native_driver();
        let first_observation = (!already_joint)
            .then(|| target_driver.pause_native_cut("membership-first-observation"));
        let target_role = target_driver.pause_native_cut("target-current-role");
        let _release_first = first_observation.as_ref().map(|gate| ReleaseGate(gate.clone()));
        let _release_role = ReleaseGate(target_role.clone());
        effect.release.notify_one();
        gate(&mut cluster, &copied).await?;
        let (id, identity) = {
            let state = driver.public_owner().state.lock().await;
            let grant = state.grants.first().ok_or_else(|| {
                TestError::new(ErrorKind::Application, "completed copy has no native owner")
            })?;
            (
                grant.binding.session().cloned().ok_or_else(|| {
                    TestError::new(
                        ErrorKind::Application,
                        "completed copy has no engine session",
                    )
                })?,
                (
                    grant.credential.host.clone(),
                    grant.credential.port,
                    grant.credential.name.clone(),
                    grant.session.clone(),
                    crate::native::replication_slot_name(grant.binding.target()),
                ),
            )
        };
        copied.release.notify_one();
        let joint_native = if let Some(before_policy) = &before_policy {
            gate(&mut cluster, before_policy).await?;
            cluster.probe(peer_receiver(target_driver, &id, true)).await?;
            let native = native_continuity(&source, &target, &id).await?;
            check(
                target_driver.pg_membership_worker(&id).is_some()
                    && !driver.pg_native_membership(&id).map_err(error)?.unwrap().current_only,
                "live joint receiver was not owned before current-only installation",
            )?;
            eprintln!("already-live joint native receiver: {native:?}");
            before_policy.release.notify_one();
            Some(native)
        } else {
            None
        };
        gate(&mut cluster, &current_policy).await?;
        if let Some(first_observation) = &first_observation {
            eprintln!("pending native: current-only policy installed before worker's first joint observation");
            cluster
                .probe(async {
                    first_observation.entered.notified().await;
                    Ok::<_, TestError>(())
                })
                .await?;
        }
        let source_publication = driver
            .public_owner()
            .host
            .get()
            .unwrap()
            .local_publication()
            .await
            .map_err(error)?
            .unwrap();
        check(
            source_publication.stage == ReplicaPublicationStage::DiscoveryPending
                && source_publication.scope.configuration.previous.is_none(),
            "source current policy did not precede its achieved receipt",
        )?;
        check(
            target_driver
                .public_owner()
                .fence
                .context()
                .map_err(error)?
                .role
                == ReplicaRole::ActiveSecondary,
            "ordinary target role callback never ran",
        )?;
        if let Some(first_observation) = &first_observation {
            check(
                matches!(
                    target_driver.public_owner().state.lock().await.receiver,
                    Some(TransportBinding::Build(_))
                ),
                "worker saw joint before the scheduler cut",
            )?;
            first_observation.release.notify_one();
        }
        cluster.probe(peer_receiver(target_driver, &id, false)).await?;
        if let Some(joint_native) = &joint_native {
            check(
                native_continuity(&source, &target, &id).await? == *joint_native,
                "joint-to-current changed the actual WAL receiver/gateway/slot/ACK backend",
            )?;
        }
        eprintln!("pending native: exact retained copy-to-peer handover joined");
        let receiver_generation = target.instance().generation_id();
        let (target_sql, target_connection) = administrative_database(&target).await?;
        target_sql
            .simple_query("SELECT pg_wal_replay_pause()")
            .await
            .map_err(error)?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let paused: String = target_sql
                    .query_one("SELECT pg_get_wal_replay_pause_state()", &[])
                    .await
                    .map_err(error)?
                    .get(0);
                if paused == "paused" {
                    return Ok::<_, TestError>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(error)??;
        let (source_sql, source_connection) = administrative_database(&source).await?;
        source_sql
            .batch_execute(
                "SET synchronous_commit = local;
            INSERT INTO m2_native_admission VALUES (2, 'native-pending-tail')",
            )
            .await
            .map_err(error)?;
        let flush_row = source_sql
            .query_one("SELECT pg_current_wal_flush_lsn()::text", &[])
            .await
            .map_err(error)?;
        let flush = crate::monitor::parse_pg_lsn(flush_row.get(0)).map_err(error)?;
        let policy = driver
            .durable
            .revalidate()
            .await
            .map_err(error)?
            .synchronous
            .ok_or_else(|| {
                TestError::new(ErrorKind::Application, "native current policy missing")
            })?;
        check(
            policy.valid
                && policy.configuration_id
                    == source_publication
                        .scope
                        .configuration
                        .current
                        .configuration_id
                && policy.eligible_standbys.len() == 1
                && policy.eligible_standbys[0].process_session_id == identity.3,
            "pending source retained joint or unrelated-session policy",
        )?;
        let readback: String = source_sql
            .query_one("SHOW synchronous_standby_names", &[])
            .await
            .map_err(error)?
            .get(0);
        check(
            readback.contains(&identity.2),
            "current-only native policy readback lost exact ACK name",
        )?;
        current_policy.release.notify_one();
        eprintln!("pending native: real replay held; current catch-up released");
        cluster
            .drive(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    while driver
                        .public_owner()
                        .native_boundary
                        .load(std::sync::atomic::Ordering::SeqCst)
                        < flush
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Ok::<_, TestError>(())
                })
                .await
                .map_err(error)?
            })
            .await?;
        let boundary = driver
            .public_owner()
            .native_boundary
            .load(std::sync::atomic::Ordering::SeqCst);
        let source_pending = driver
            .public_owner()
            .host
            .get()
            .unwrap()
            .local_publication()
            .await
            .map_err(error)?
            .unwrap();
        check(
            source_pending.stage == ReplicaPublicationStage::DiscoveryPending,
            "source fabricated achieved catch-up while real WAL replay was held",
        )?;
        check(
            source.instance().connect_application().await.is_err()
                && target.instance().connect_application().await.is_err(),
            "pending source/target native SQL opened before admission",
        )?;
        let held = target_driver
            .observer
            .snapshot()
            .await
            .map_err(error)?
            .evidence
            .unwrap();
        check(
            held.replay_lsn.is_some_and(|lsn| lsn < boundary),
            "test did not hold actual native replay below captured flush",
        )?;
        target_sql
            .simple_query("SELECT pg_wal_replay_resume()")
            .await
            .map_err(error)?;
        eprintln!("pending native: releasing actual replay below captured boundary {boundary}");
        gate(&mut cluster, &current_receipt).await?;
        eprintln!("pending native: actual WriteQuorum completed before achieved receipt");
        PrimaryReplicator::wait_for_catch_up_quorum(
            driver.as_ref(),
            ReplicaSetQuorumMode::WriteQuorum,
        )
        .await
        .map_err(error)?;
        let evidence = target_driver
            .observer
            .snapshot()
            .await
            .map_err(error)?
            .evidence
            .unwrap();
        check(
            evidence.received_lsn.is_some_and(|lsn| lsn >= boundary)
                && evidence.replay_lsn.is_some_and(|lsn| lsn >= boundary),
            "native pending continuation did not receive AND replay captured WAL",
        )?;
        check(
            driver
                .public_owner()
                .host
                .get()
                .unwrap()
                .local_publication()
                .await
                .map_err(error)?
                .is_some_and(|publication| {
                    publication.stage == ReplicaPublicationStage::DiscoveryPending
                }),
            "native quorum progress was mistaken for a host receipt",
        )?;
        let replay: Option<String> = source_sql
            .query_one(
                "SELECT replay_lsn::text FROM pg_stat_replication WHERE application_name = $1",
                &[&identity.2],
            )
            .await
            .map_err(error)?
            .get(0);
        check(
            replay
                .as_deref()
                .map(crate::monitor::parse_pg_lsn)
                .transpose()
                .map_err(error)?
                .is_some_and(|lsn| lsn >= boundary),
            "source did not observe exact-session native replay ACK",
        )?;
        current_receipt.release.notify_one();
        gate(&mut cluster, &target_role).await?;
        let target_pending = target_driver
            .public_owner()
            .host
            .get()
            .unwrap()
            .local_publication()
            .await
            .map_err(error)?
            .unwrap();
        check(
            target_pending.stage == ReplicaPublicationStage::DiscoveryPending
                && target_pending.scope.configuration.previous.is_none()
                && target.instance().connect_application().await.is_err()
                && source.instance().connect_application().await.is_err(),
            "target current role receipt hold did not fence SQL",
        )?;
        PrimaryReplicator::wait_for_catch_up_quorum(
            driver.as_ref(),
            ReplicaSetQuorumMode::WriteQuorum,
        )
        .await
        .map_err(error)?;
        check(
            target.instance().generation_id() == receiver_generation,
            "compatible pending current callback restarted the native receiver",
        )?;
        if let Some(joint_native) = &joint_native {
            check(
                native_continuity(&source, &target, &id).await? == *joint_native,
                "pending callbacks/WAL replaced the live joint native owners",
            )?;
        }
        target_role.release.notify_one();
        let topology = cluster.settle_members(2).await?;
        let observed = cluster.observe().await?;
        for reference in topology.members() {
            check(
                observed
                    .snapshot(reference)
                    .and_then(|snapshot| snapshot.publication.as_ref())
                    .is_some_and(|publication| {
                        publication.stage == ReplicaPublicationStage::AchievedScope
                            && publication.scope.configuration.previous.is_none()
                    }),
                "final admission lacks current-only achieved publication",
            )?;
            let service = cluster.service(reference).await?;
            checked_data(
                &service,
                reference.identity() == topology.primary().identity(),
            )
            .await?;
        }
        let state = driver.public_owner().state.lock().await;
        let grant = state
            .grants
            .iter()
            .find(|grant| grant.binding.session() == Some(&id))
            .ok_or_else(|| {
                TestError::new(ErrorKind::Application, "current session owner replaced")
            })?;
        check(
            (
                grant.credential.host.clone(),
                grant.credential.port,
                grant.credential.name.clone(),
                grant.session.clone(),
                crate::native::replication_slot_name(grant.binding.target()),
            ) == identity,
            "joint/current continuity changed native nonce/gateway/slot/ACK identity",
        )?;
        drop(state);
        let (sql, connection) = application_connection(&source).await?;
        sql.batch_execute("INSERT INTO m2_native_admission VALUES (3, 'ordinary-admitted-tail')")
            .await
            .map_err(error)?;
        drop(sql);
        connection.await.map_err(error)?;
        PrimaryReplicator::wait_for_catch_up_quorum(
            driver.as_ref(),
            ReplicaSetQuorumMode::WriteQuorum,
        )
        .await
        .map_err(error)?;
        check(
            target.instance().connect_application().await.is_err(),
            "standby native replay was treated as a host SQL grant",
        )?;
        let (sql, connection) = administrative_database(&target).await?;
        let count: i64 = sql
            .query_one("SELECT count(*) FROM m2_native_admission", &[])
            .await
            .map_err(error)?
            .get(0);
        check(
            count == 3,
            "admitted ordinary SQL lost acknowledged native WAL tail",
        )?;
        drop(sql);
        connection.await.map_err(error)?;
        drop(source_sql);
        source_connection.await.map_err(error)?;
        drop(target_sql);
        target_connection.await.map_err(error)?;
        Ok(())
    }
    .await;
    finished(cluster.finish(result).await);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_joint_to_current_retains_native_session_while_pending() {
    pending_continuity(false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_already_live_joint_receiver_survives_pending_current() {
    pending_continuity(true).await;
}

async fn membership_registration(retirement_first: bool, lost_join: bool) {
    let factory = Factory::default();
    let mut cluster = cluster(&factory).await;
    let result: TestResult = async {
        let topology = cluster.settle_members(1).await?;
        let source = cluster.service(topology.primary()).await?;
        seed(&source).await?;
        let driver = source.native_driver();
        let effect = driver.pause_public_build_effects();
        let copied = driver.pause_public_build_completion();
        let current = driver.pause_native_cut("source-current-before-policy");
        let _release_effect = ReleaseGate(effect.clone());
        let _release_copied = ReleaseGate(copied.clone());
        let _release_current = ReleaseGate(current.clone());
        cluster.scale_to(2).await?;
        gate(&mut cluster, &effect).await?;
        let observation = cluster.observe().await?;
        let reference = observation
            .replicas()
            .find(|reference| reference.identity() != topology.primary().identity())
            .cloned()
            .unwrap();
        let target = cluster.service(&reference).await?;
        let target_driver = target.native_driver();
        let registration = target_driver.pause_native_cut("membership-registration");
        let _release_registration = ReleaseGate(registration.clone());
        effect.release.notify_one();
        gate(&mut cluster, &copied).await?;
        let id = driver
            .pg_session_ids_for_target(reference.identity().replica_id)
            .into_iter()
            .next()
            .unwrap();
        copied.release.notify_one();
        gate(&mut cluster, &registration).await?;
        check(
            target_driver.pg_membership_worker(&id).is_none()
                && target_driver.pg_session_ownership(&id).1,
            "registration cut did not precede the actual child spawn",
        )?;
        let returned = target_driver.pause_native_cut("membership-tick-returned");
        let _release_returned = ReleaseGate(returned.clone());
        if retirement_first {
            driver.pg_retire_session(&id).await.map_err(error)?;
            check(
                !target_driver.pg_session_ownership(&id).1,
                "retirement did not join the actual native inbound owner",
            )?;
            let inbound = target_driver
                .pg_native_inbounds()
                .into_iter()
                .next()
                .unwrap();
            target_driver
                .public_join_inbound_owner(&inbound)
                .await
                .map_err(error)?;
            registration.release.notify_one();
            cluster
                .probe(async {
                    returned.entered.notified().await;
                    Ok::<_, TestError>(())
                })
                .await?;
            check(
                target_driver.pg_membership_worker(&id).is_none()
                    && target_driver.pg_session_ownership(&id).0 == session::SessionStage::Retired
                    && !target.instance().is_running().await,
                "a membership child was appended after retirement sealed its joins",
            )?;
            returned.release.notify_one();
        } else {
            let first = target_driver.pause_native_cut("membership-first-observation");
            let exit = target_driver.pause_native_cut("membership-worker-exit");
            let _release_first = ReleaseGate(first.clone());
            let _release_exit = ReleaseGate(exit.clone());
            registration.release.notify_one();
            cluster
                .probe(async {
                    returned.entered.notified().await;
                    first.entered.notified().await;
                    Ok::<_, TestError>(())
                })
                .await?;
            let worker = target_driver.pg_membership_worker(&id).ok_or_else(|| {
                TestError::new(ErrorKind::Application, "real membership child missing")
            })?;
            returned.release.notify_one();
            driver.pg_remove_native_session(&id).await.map_err(error)?;
            cluster
                .probe(async {
                    exit.entered.notified().await;
                    Ok::<_, TestError>(())
                })
                .await?;
            let retiring = driver.clone();
            let retired = id.clone();
            let mut observer =
                tokio::spawn(async move { retiring.pg_retire_session(&retired).await });
            check(
                tokio::time::timeout(Duration::from_millis(100), &mut observer)
                    .await
                    .is_err()
                    && target_driver.pg_session_ownership(&id).1
                    && !worker.settled_successfully(),
                "retirement acknowledged containment before its real membership child joined",
            )?;
            observer.abort();
            let _ = observer.await;
            let mut successor = id.clone();
            successor.nonce[0] ^= 1;
            check(
                target_driver
                    .pg_session_request(session::SessionRequest {
                        session: successor,
                        action: session::SessionAction::Reserve,
                    })
                    .await
                    .is_err(),
                "held membership child released the reserved receiver",
            )?;
            if lost_join {
                worker.lose().await;
                exit.release.notify_one();
                check(
                    driver.pg_retire_session(&id).await.is_err()
                        && target_driver.pg_session_ownership(&id).1
                        && target_driver.pg_session_ownership(&id).0
                            == session::SessionStage::Quarantined,
                    "lost membership join fabricated retirement or released quarantine",
                )?;
                let first = target_driver.public_close().await.unwrap_err().to_string();
                let second = target_driver.public_close().await.unwrap_err().to_string();
                check(
                    first == second && !target_driver.public_closed(),
                    "lost membership join was not sticky across Close",
                )?;
            } else {
                exit.release.notify_one();
                worker.clone().await.map_err(error)?;
                driver.pg_retire_session(&id).await.map_err(error)?;
                check(
                    worker.settled_successfully()
                        && !target_driver.pg_session_ownership(&id).1
                        && !target.instance().is_running().await,
                    "retirement did not observe the real registered child's successful join",
                )?;
            }
        }
        check(
            target.instance().data_dir().join("PG_VERSION").exists(),
            "membership retirement reset copied PGDATA",
        )?;
        current.release.notify_one();
        Ok(())
    }
    .await;
    let passed = result.is_ok();
    let outcome = cluster.finish(result).await;
    assert!(
        passed,
        "membership registration witness failed: {outcome:?}"
    );
    if lost_join {
        assert!(
            outcome.is_err(),
            "lost actual child join became successful teardown"
        );
    } else {
        finished(outcome);
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_membership_registration_retirement_orders_and_sticky_join() {
    membership_registration(true, false).await;
    membership_registration(false, false).await;
    membership_registration(false, true).await;
}

async fn remove_adoption(adoption_first: bool, lost_join: bool) {
    use std::os::unix::fs::MetadataExt;

    let factory = Factory::default();
    let mut cluster = cluster(&factory).await;
    let result: TestResult = async {
        let topology = cluster.settle_members(1).await?;
        let source = cluster.service(topology.primary()).await?;
        seed(&source).await?;
        let driver = source.native_driver();
        let copied = driver.pause_public_build_completion();
        let resolved = driver.pause_native_cut("source-adoption-before-state");
        let _release_copied = ReleaseGate(copied.clone());
        let _release_resolved = ReleaseGate(resolved.clone());
        cluster.scale_to(2).await?;
        gate(&mut cluster, &copied).await?;
        copied.release.notify_one();
        gate(&mut cluster, &resolved).await?;
        let observed = cluster.observe().await?;
        let reference = observed.replicas()
            .find(|reference| reference.identity() != topology.primary().identity())
            .cloned().unwrap();
        let target = cluster.service(&reference).await?;
        let target_driver = target.native_driver();
        let replica = reference.identity().replica_id;
        let id = driver.pg_session_ids_for_target(replica).into_iter().next().unwrap();
        let native = native_continuity(&source, &target, &id).await?;
        let source_generation = source.instance().generation_id();
        let (configuration, previous, credential) = {
            let state = driver.public_owner().state.lock().await;
            check(state.built_idle.get(&replica).is_some_and(|build| build.session.as_deref() == Some(&id)),
                "race did not start from actual completed idle Build")?;
            let grant = state.grants.iter().find(|grant| grant.binding.session() == Some(&id)).unwrap();
            (state.configuration.clone(), state.previous.clone(),
                (grant.credential.host.clone(), grant.credential.port, grant.credential.name.clone(), grant.session.clone()))
        };
        let context = driver.public_owner().fence.context().map_err(error)?;
        let policy = driver.durable.revalidate().await.map_err(error)?.synchronous;
        let inbound = target_driver.pg_native_inbounds().into_iter().next().unwrap();
        if adoption_first {
            let adopting = driver.pause_native_cut("source-adoption-with-state");
            let removing = driver.pause_native_cut("source-remove-before-state");
            let _release_adopting = ReleaseGate(adopting.clone());
            let _release_removing = ReleaseGate(removing.clone());
            resolved.release.notify_one();
            gate(&mut cluster, &adopting).await?;
            let remove_driver = driver.clone();
            let mut remove = tokio::spawn(async move { remove_driver.remove_replica(replica).await });
            removing.entered.notified().await;
            removing.release.notify_one();
            check(tokio::time::timeout(Duration::from_millis(100), &mut remove).await.is_err(),
                "Remove bypassed adoption's native state owner")?;
            adopting.release.notify_one();
            cluster.settle_members(2).await?;
            check(remove.await.map_err(error)?.is_err(), "adopted voter was removed")?;
            let native = native_continuity(&source, &target, &id).await?;
            let owners = (driver.pg_session_ownership(&id), target_driver.pg_session_ownership(&id));
            let membership = driver.pg_native_membership(&id).map_err(error)?;
            let installed = driver.durable.revalidate().await.map_err(error)?.synchronous;
            check(driver.remove_replica(replica).await.is_err(), "installed voter accepted Remove")?;
            check(
                native_continuity(&source, &target, &id).await? == native
                    && owners == (driver.pg_session_ownership(&id), target_driver.pg_session_ownership(&id))
                    && membership == driver.pg_native_membership(&id).map_err(error)?
                    && installed == driver.durable.revalidate().await.map_err(error)?.synchronous
                    && Arc::ptr_eq(&inbound, &target_driver.pg_native_inbounds()[0]),
                "rejected Remove changed native session/gateway/slot/receiver or SQL policy",
            )?;
            {
                let state = driver.public_owner().state.lock().await;
                let grant = state.grants.iter().find(|grant| grant.binding.session() == Some(&id)).unwrap();
                check(
                    matches!(&grant.binding, TransportBinding::Peer(peer) if peer.session.as_deref() == Some(&id))
                        && (grant.credential.host.clone(), grant.credential.port, grant.credential.name.clone(), grant.session.clone()) == credential,
                    "adoption or rejected Remove changed nonce/endpoint/ACK owner",
                )?;
            }
            let (target_sql, target_connection) = target.instance().connect().await.map_err(error)?;
            target_sql.simple_query("SELECT pg_wal_replay_pause()").await.map_err(error)?;
            let (sql, connection) = application_connection(&source).await?;
            let mut write = tokio::spawn(async move {
                let result = sql.batch_execute(
                    "SET synchronous_commit = remote_apply;
                     INSERT INTO m2_native_admission VALUES (2, 'remove-survivor')",
                ).await;
                drop(sql);
                connection.await.map_err(error)?;
                result.map_err(error)
            });
            let (admin, admin_connection) = source.instance().connect().await.map_err(error)?;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let waiting: bool = admin.query_one(
                        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                         WHERE wait_event = 'SyncRep' AND query LIKE '%remove-survivor%')", &[],
                    ).await.map_err(error)?.get(0);
                    if waiting { return Ok::<_, TestError>(()); }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }).await.map_err(error)??;
            check(tokio::time::timeout(Duration::from_millis(100), &mut write).await.is_err(),
                "SQL write did not depend on the surviving actual voter's replay ACK")?;
            target_sql.simple_query("SELECT pg_wal_replay_resume()").await.map_err(error)?;
            write.await.map_err(error)??;
            let boundary = source.instance().retained_wal_boundary().await.map_err(error)?;
            for mode in [ReplicaSetQuorumMode::WriteQuorum, ReplicaSetQuorumMode::All] {
                PrimaryReplicator::wait_for_catch_up_quorum(driver.as_ref(), mode).await.map_err(error)?;
            }
            let evidence = target_driver.observer.snapshot().await.map_err(error)?.evidence.unwrap();
            check(evidence.received_lsn.is_some_and(|lsn| lsn >= boundary)
                && evidence.replay_lsn.is_some_and(|lsn| lsn >= boundary),
                "surviving receiver did not receive AND replay acknowledged WAL")?;
            let (sql, connection) = administrative_database(&target).await?;
            let value: String = sql.query_one("SELECT value FROM m2_native_admission WHERE id = 2", &[])
                .await.map_err(error)?.get(0);
            check(value == "remove-survivor", "quorum SQL tail missing on surviving voter")?;
            drop(sql);
            connection.await.map_err(error)?;
            drop(admin);
            admin_connection.await.map_err(error)?;
            drop(target_sql);
            target_connection.await.map_err(error)?;
            check(native_continuity(&source, &target, &id).await? == native,
                "quorum/catch-up replaced the surviving native owners")?;
        } else {
            let cleanup = driver.pause_native_cut("source-retirement-cleanup");
            let rejected = driver.pause_native_cut("source-adoption-rejected");
            let _release_cleanup = ReleaseGate(cleanup.clone());
            let _release_rejected = ReleaseGate(rejected.clone());
            let remove_driver = driver.clone();
            let mut observer = tokio::spawn(async move { remove_driver.remove_replica(replica).await });
            cleanup.entered.notified().await;
            check(driver.pg_session_ownership(&id).0 == session::SessionStage::Retiring
                && target_driver.pg_session_ownership(&id).1,
                "legal Remove reached its first cleanup await without fencing the exact session")?;
            check(tokio::time::timeout(Duration::from_millis(100), &mut observer).await.is_err(),
                "Remove returned before actual native cleanup")?;
            observer.abort();
            let _ = observer.await;
            resolved.release.notify_one();
            rejected.entered.notified().await;
            // Observe the real native rejection before ordinary callback
            // failure containment can retire the entire source creation.
            {
                let state = driver.public_owner().state.lock().await;
                check(state.configuration == configuration && state.previous == previous
                    && state.grants.iter().any(|grant| matches!(&grant.binding,
                        TransportBinding::Build(build) if build.session.as_deref() == Some(&id))
                        && grant.gateway.task_abort_handle().id() == native.gateway
                        && (grant.credential.host.clone(), grant.credential.port, grant.credential.name.clone(), grant.session.clone()) == credential),
                    "prevalidated adoption installed a fenced session before rejecting")?;
            }
            check(driver.public_owner().fence.context().map_err(error)? == context
                && driver.durable.revalidate().await.map_err(error)?.synchronous == policy
                && driver.pg_native_membership(&id).is_err()
                && source.instance().is_running().await
                && source.instance().generation_id() == source_generation
                && target.instance().generation_id() == native.receiver_generation
                && std::fs::metadata(source.instance().data_dir().join("pg_replslot")
                    .join(crate::native::replication_slot_name(&id.target.scope.storage.replica)))?.ino() == native.slot_inode,
                "fenced adoption changed source/target native effects or SQL policy")?;
            if lost_join {
                target.instance().lose_monitor().await;
            }
            cleanup.release.notify_one();
            driver.remove_replica(replica).await.map_err(error)?;
            let retired = driver.pg_retire_session(&id).await;
            if lost_join {
                check(retired.is_err() && driver.pg_retire_session(&id).await.is_err()
                    && target_driver.pg_session_ownership(&id).0 == session::SessionStage::Quarantined,
                    "lost native monitor join fabricated successful removal")?;
                let first = target_driver.public_close().await.unwrap_err().to_string();
                let second = target_driver.public_close().await.unwrap_err().to_string();
                check(first == second && !target_driver.public_closed(),
                    "lost native join was not sticky after lost Remove observer")?;
            } else {
                retired.map_err(error)?;
                check(driver.pg_session_ownership(&id).0 == session::SessionStage::Retired
                    && driver.pg_session_ownership(&id).2
                    && target_driver.pg_session_ownership(&id).0 == session::SessionStage::Retired
                    && !target_driver.pg_session_ownership(&id).1
                    && !target.instance().is_running().await,
                    "reattached Remove did not join both actual native owners")?;
                let join = inbound.task.lock().await;
                check(join.task.is_none() && join.result.as_ref().is_some_and(|result| result.is_ok()),
                    "actual inbound task join was not retained")?;
                let state = driver.public_owner().state.lock().await;
                check(state.grants.iter().all(|grant| grant.binding.session() != Some(&id))
                    && !state.built_idle.contains_key(&replica)
                    && !source.instance().data_dir().join("pg_replslot")
                        .join(crate::native::replication_slot_name(&id.target.scope.storage.replica)).exists(),
                    "removed idle gateway/slot owner survived settlement")?;
            }
            check(target.instance().data_dir().join("PG_VERSION").exists()
                && driver.public_owner().fence.context().map_err(error)?.role == ReplicaRole::Primary,
                "idle removal reset PGDATA or depended on source role loss")?;
            rejected.release.notify_one();
        }
        Ok(())
    }.await;
    let passed = result.is_ok();
    let outcome = cluster.finish(result).await;
    assert!(
        passed,
        "Remove/adoption witness failed: {outcome:?}; sources: {:?}",
        outcome.as_ref().err().map(|failure| failure
            .sources()
            .map(|source| source.to_string())
            .collect::<Vec<_>>())
    );
    if lost_join {
        assert!(
            outcome.is_err(),
            "lost real native join became successful teardown"
        );
    } else {
        finished(outcome);
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_adoption_remove_keeps_voter_and_quorum() {
    remove_adoption(true, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_adoption_remove_retirement_first_rejects_and_joins() {
    remove_adoption(false, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_adoption_remove_lost_observer_keeps_sticky_join() {
    remove_adoption(false, true).await;
}

async fn incompatible_live_publication(endpoint: &str, dimension: &str) {
    let factory = Factory::default();
    let mut cluster = cluster(&factory).await;
    let result: TestResult = async {
        let topology = cluster.settle_members(1).await?;
        let source = cluster.service(topology.primary()).await?;
        seed(&source).await?;
        let driver = source.native_driver();
        let effect = driver.pause_public_build_effects();
        let copied = driver.pause_public_build_completion();
        let before_policy = driver.pause_native_cut("source-current-before-policy");
        let current = driver.pause_native_cut("source-current-policy");
        let _release_effect = ReleaseGate(effect.clone());
        let _release_copied = ReleaseGate(copied.clone());
        let _release_before = ReleaseGate(before_policy.clone());
        let _release_current = ReleaseGate(current.clone());
        cluster.scale_to(2).await?;
        gate(&mut cluster, &effect).await?;
        let observation = cluster.observe().await?;
        let reference = observation
            .replicas()
            .find(|reference| reference.identity() != topology.primary().identity())
            .cloned()
            .unwrap();
        let target = cluster.service(&reference).await?;
        let target_driver = target.native_driver();
        effect.release.notify_one();
        gate(&mut cluster, &copied).await?;
        let id = driver.pg_session_ids_for_target(reference.identity().replica_id)
            .into_iter().next().unwrap();
        copied.release.notify_one();
        gate(&mut cluster, &before_policy).await?;
        cluster.probe(peer_receiver(target_driver, &id, true)).await?;
        let native = native_continuity(&source, &target, &id).await?;
        let worker = target_driver.pg_membership_worker(&id).unwrap();
        let delayed = target_driver.pause_native_cut("membership-first-observation");
        let _release_delayed = ReleaseGate(delayed.clone());
        cluster.probe(async {
            delayed.entered.notified().await;
            Ok::<_, TestError>(())
        }).await?;
        before_policy.release.notify_one();
        gate(&mut cluster, &current).await?;
        let observed = cluster.observe().await?;
        let current_goal = observed.snapshot(topology.primary()).unwrap().goals.iter()
            .find(|entry| entry.status == ReplicaGoalStatus::Pending
                && matches!(entry.goal.desired, ReplicaGoalKind::Reconfigure { .. })
                && entry.goal.scope.configuration.previous.is_none())
            .ok_or_else(|| TestError::new(ErrorKind::Application, "pending current goal missing"))?
            .goal.clone();
        let host_publication = |driver: Arc<PgReplicator>| async move {
            driver.public_owner().host.get().unwrap().local_publication()
                .await.map_err(error)?.ok_or_else(|| {
                    TestError::new(ErrorKind::Application, "live publication missing before fault")
                })
        };
        let source_publication = host_publication(driver.clone()).await?;
        let target_publication = host_publication(target_driver.clone()).await?;
        check(
            source_publication.stage == ReplicaPublicationStage::DiscoveryPending
                && source_publication.scope.configuration.previous.is_none()
                && target_driver.pg_session_ownership(&id).0 == session::SessionStage::Completed,
            "live incompatibility did not start from an established pending continuation",
        )?;
        let pgdata = std::fs::read(target.instance().data_dir().join("PG_VERSION"))?;
        let before_reads = target_driver.public_owner().continuation_reads
            .load(std::sync::atomic::Ordering::SeqCst);
        let mut encoded = serde_json::json!({
            "source": source_publication,
            "target": target_publication,
        });
        let publication = &mut encoded[endpoint];
        let original_publication = publication.clone();
        match dimension {
            "replica" => publication["scope"]["storage"]["replica"]["instanceId"] = serde_json::json!("changed-replica"),
            "creationId" | "processSessionId" => publication["scope"][dimension] = serde_json::json!("changed-identity"),
            "storage" => publication["scope"]["storage"]["storageGeneration"] = serde_json::json!("changed-storage"),
            "history" => publication["scope"]["history"]["historyId"] = serde_json::json!("changed-history"),
            "epoch" => publication["scope"]["epoch"]["configurationNumber"] = serde_json::json!(2),
            "replicationAddress" => publication[dimension] = serde_json::json!("http://127.0.0.1:1"),
            "generation" => publication[dimension] = serde_json::json!(0),
            "role" => publication[dimension] = serde_json::json!("idleSecondary"),
            "configurationId" => publication["scope"]["configuration"]["current"][dimension] = serde_json::json!("changed-C"),
            "configurationEpoch" => publication["scope"]["configuration"]["current"]["epoch"]["configurationNumber"] = serde_json::json!(2),
            "members" => publication["scope"]["configuration"]["current"]["members"][0]["instanceId"] = serde_json::json!("changed-member"),
            "writeQuorum" => publication["scope"]["configuration"]["current"][dimension] = serde_json::json!(1),
            "primaryId" => {
                for member in publication["scope"]["configuration"]["current"]["members"]
                    .as_array_mut().unwrap()
                {
                    member["role"] = if member["replicaId"] == serde_json::json!(id.source.scope.storage.replica.replica_id) {
                        serde_json::json!("activeSecondary")
                    } else {
                        serde_json::json!("primary")
                    };
                }
            }
            "previous" => publication["scope"]["configuration"][dimension] = serde_json::to_value(&id.source.scope.configuration.current).map_err(error)?,
            "replicas" => publication["scope"]["configuration"][dimension] = serde_json::json!([]),
            _ => unreachable!("unregistered publication dimension"),
        }
        check(
            *publication != original_publication,
            "publication fault was a no-op",
        )?;
        let source_fault = serde_json::from_value(encoded["source"].clone()).map_err(error)?;
        let target_fault = serde_json::from_value(encoded["target"].clone()).map_err(error)?;
        let exit = Arc::new(DriverExitGate::default());
        let _release_exit = ReleaseNotify(&exit.release);
        let (admin, admin_driver) = target
            .instance()
            .connect_with_driver_exit(exit.clone())
            .await
            .map_err(error)?;
        admin.simple_query("SELECT 1").await.map_err(error)?;
        *target_driver.public_owner().session_publications.lock().unwrap() =
            Some((source_fault, target_fault));
        delayed.release.notify_one();
        cluster.probe(async {
            tokio::time::timeout(Duration::from_secs(20), exit.entered.notified()).await.map_err(error)?;
            Ok::<_, TestError>(())
        }).await?;
        check(
            target_driver.public_owner().continuation_reads.load(std::sync::atomic::Ordering::SeqCst) > before_reads
                && target_driver.pg_session_ownership(&id).0 == session::SessionStage::Quarantined
                && target_driver.pg_session_ownership(&id).1
                && target.instance().generation_id() == native.receiver_generation
                && !target.instance().is_running().await
                && admin.simple_query("SELECT 1").await.is_err()
                && !worker.settled_successfully(),
            "newest incompatible publication bypassed the same-session predicates/native fence/retained join",
        )?;
        // An old publication returning cannot revive the cancelled live owner.
        *target_driver.public_owner().session_publications.lock().unwrap() = None;
        check(
            target_driver.pg_session_request(session::SessionRequest {
                session: id.clone(),
                action: session::SessionAction::Complete,
            }).await.is_err()
                && target_driver.pg_native_membership_tick().await.is_ok()
                && target.instance().connect_application().await.is_err(),
            "restored discovery revived quarantined native continuation or completion credit",
        )?;
        let mut successor = id.clone();
        successor.nonce[0] ^= 1;
        check(
            target_driver.pg_session_request(session::SessionRequest {
                session: successor,
                action: session::SessionAction::Reserve,
            }).await.is_err(),
            "incompatible live publication released an unsettled receiver for a successor",
        )?;
        let (sql, connection) = administrative_database(&source).await?;
        sql.batch_execute(
            "SET synchronous_commit = local;
             INSERT INTO m2_native_admission VALUES (4, 'not-acked-after-incompatibility')",
        ).await.map_err(error)?;
        check(
            !matches!(
                tokio::time::timeout(Duration::from_millis(200),
                    PrimaryReplicator::wait_for_catch_up_quorum(
                        driver.as_ref(), ReplicaSetQuorumMode::WriteQuorum,
                    )
                ).await,
                Ok(Ok(()))
            ),
            "incompatible continuation received stale native quorum credit",
        )?;
        check(
            source.instance().connect_application().await.is_err()
                && !driver.pg_native_membership(&id).map_err(error)?.unwrap().achieved,
            "native failure became current ordinary SQL/admission credit",
        )?;
        driver.pg_remove_native_session(&id).await.map_err(error)?;
        let retiring = driver.clone();
        let retired = id.clone();
        let mut join = tokio::spawn(async move { retiring.pg_retire_session(&retired).await });
        check(
            tokio::time::timeout(Duration::from_millis(100), &mut join).await.is_err()
                && target_driver.pg_session_ownership(&id).1,
            "two-ended retirement bypassed the actual held incompatible-worker join",
        )?;
        exit.release.notify_one();
        admin_driver.await.map_err(error)?;
        drop(admin);
        join.await.map_err(error)?.map_err(error)?;
        worker.clone().await.map_err(error)?;
        check(
            worker.settled_successfully()
                && !target_driver.pg_session_ownership(&id).1
                && target_driver.pg_membership_worker(&id).is_some()
                && !target.instance().is_running().await
                && std::fs::read(target.instance().data_dir().join("PG_VERSION"))? == pgdata,
            "incompatible retained owner did not actually join without PGDATA reset",
        )?;
        drop(sql);
        connection.await.map_err(error)?;
        current.release.notify_one();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let observed = cluster.observe().await?;
                let snapshot = observed.snapshot(topology.primary()).unwrap();
                if let Some(outcome) = snapshot.goals.iter().find(|entry| entry.goal == current_goal)
                    && outcome.status != ReplicaGoalStatus::Pending
                {
                    check(matches!(outcome.status, ReplicaGoalStatus::Failed(_)),
                        "incompatible same-session continuation credited the released ordinary current goal")?;
                    check(snapshot.publication.is_none(),
                        "failed continuation published a late current-only achieved scope")?;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, TestError>(())
        }).await.map_err(error)??;
        eprintln!("same-session live publication fenced and joined: {endpoint}.{dimension}");
        Ok(())
    }.await;
    finished(cluster.finish(result).await);
}

macro_rules! live_publication_case {
    ($name:ident, $dimension:literal, $($endpoint:literal),+) => {
        #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
        #[serial_test::serial]
        async fn $name() {
            $(incompatible_live_publication($endpoint, $dimension).await;)+
        }
    };
}

live_publication_case!(
    m2_pg_live_publication_replica,
    "replica",
    "source",
    "target"
);
live_publication_case!(
    m2_pg_live_publication_creation,
    "creationId",
    "source",
    "target"
);
live_publication_case!(
    m2_pg_live_publication_process,
    "processSessionId",
    "source",
    "target"
);
live_publication_case!(
    m2_pg_live_publication_storage,
    "storage",
    "source",
    "target"
);
live_publication_case!(
    m2_pg_live_publication_history,
    "history",
    "source",
    "target"
);
live_publication_case!(m2_pg_live_publication_epoch, "epoch", "source", "target");
live_publication_case!(
    m2_pg_live_publication_endpoint,
    "replicationAddress",
    "source",
    "target"
);
live_publication_case!(
    m2_pg_live_publication_generation,
    "generation",
    "source",
    "target"
);
live_publication_case!(m2_pg_live_publication_role, "role", "source", "target");
live_publication_case!(
    m2_pg_live_publication_configuration,
    "configurationId",
    "source",
    "target"
);
live_publication_case!(
    m2_pg_live_publication_members,
    "members",
    "source",
    "target"
);
live_publication_case!(
    m2_pg_live_publication_write_quorum,
    "writeQuorum",
    "source",
    "target"
);
live_publication_case!(
    m2_pg_live_publication_primary,
    "primaryId",
    "source",
    "target"
);
live_publication_case!(
    m2_pg_live_publication_replica_set,
    "replicas",
    "source",
    "target"
);
live_publication_case!(m2_pg_live_publication_previous, "previous", "source");
live_publication_case!(
    m2_pg_live_publication_configuration_epoch,
    "configurationEpoch",
    "source",
    "target"
);

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m2_pg_incompatible_or_retired_session_cannot_continue_while_pending() {
    let factory = Factory::default();
    let mut cluster = cluster(&factory).await;
    let result: TestResult = async {
        let topology = cluster.settle_members(1).await?;
        let source = cluster.service(topology.primary()).await?;
        seed(&source).await?;
        let driver = source.native_driver();
        let effect = driver.pause_public_build_effects();
        let copied = driver.pause_public_build_completion();
        let current = driver.pause_native_cut("source-current-policy");
        let _release_effect = ReleaseGate(effect.clone());
        let _release_copied = ReleaseGate(copied.clone());
        let _release_current = ReleaseGate(current.clone());
        cluster.scale_to(2).await?;
        gate(&mut cluster, &effect).await?;
        let observation = cluster.observe().await?;
        let target_ref = observation
            .replicas()
            .find(|reference| reference.identity() != topology.primary().identity())
            .cloned()
            .unwrap();
        let target = cluster.service(&target_ref).await?;
        let target_driver = target.native_driver();
        let delayed = target_driver.pause_native_cut("membership-first-observation");
        let _release_delayed = ReleaseGate(delayed.clone());
        effect.release.notify_one();
        gate(&mut cluster, &copied).await?;
        let id = driver
            .pg_session_ids_for_target(target_ref.identity().replica_id)
            .into_iter()
            .next()
            .unwrap();
        copied.release.notify_one();
        gate(&mut cluster, &current).await?;
        cluster
            .probe(async {
                delayed.entered.notified().await;
                Ok::<_, TestError>(())
            })
            .await?;
        let pending = driver
            .public_owner()
            .host
            .get()
            .unwrap()
            .local_publication()
            .await
            .map_err(error)?
            .unwrap();
        check(
            pending.stage == ReplicaPublicationStage::DiscoveryPending
                && pending.scope.configuration.previous.is_none(),
            "negative continuation did not reach actual current-only pending",
        )?;
        let retained = target_driver.durable.revalidate().await.map_err(error)?;
        let pgdata = std::fs::read(target.instance().data_dir().join("PG_VERSION"))?;
        let encoded = serde_json::to_value(&id).map_err(error)?;
        for endpoint in ["source", "target"] {
            for dimension in [
                "creationId",
                "processSessionId",
                "storage",
                "history",
                "epoch",
                "replicationAddress",
                "configuration",
                "role",
            ] {
                let mut changed = encoded.clone();
                match dimension {
                    "replicationAddress" => {
                        changed[endpoint][dimension] = serde_json::json!("http://127.0.0.1:1")
                    }
                    "role" => changed[endpoint][dimension] = serde_json::json!("activeSecondary"),
                    "storage" => {
                        changed[endpoint]["scope"]["storage"]["storageGeneration"] =
                            serde_json::json!("incompatible-storage")
                    }
                    "history" => {
                        changed[endpoint]["scope"]["history"]["historyId"] =
                            serde_json::json!("incompatible-history")
                    }
                    "epoch" => {
                        changed[endpoint]["scope"]["epoch"]["configurationNumber"] =
                            serde_json::json!(2)
                    }
                    "configuration" => {
                        changed[endpoint]["scope"]["configuration"]["current"]["configurationId"] =
                            serde_json::json!("incompatible-C")
                    }
                    other => {
                        changed[endpoint]["scope"][other] =
                            serde_json::json!("incompatible-identity")
                    }
                }
                let changed: session::PgSessionId =
                    serde_json::from_value(changed).map_err(error)?;
                check(
                    driver
                        .public_request(Request::Membership(Box::new(changed)))
                        .await
                        .is_err(),
                    "incompatible prerequisite revived native continuation",
                )?;
            }
        }
        let installed = driver.pg_native_membership(&id).map_err(error)?.unwrap();
        let current_descriptor = installed.current.clone();
        for dimension in ["members", "writeQuorum", "epoch", "configurationId"] {
            let mut changed = serde_json::to_value(&current_descriptor).map_err(error)?;
            match dimension {
                "members" => {
                    changed["members"][0]["instanceId"] = serde_json::json!("changed-primary")
                }
                "writeQuorum" => changed[dimension] = serde_json::json!(1),
                "epoch" => changed[dimension]["configurationNumber"] = serde_json::json!(2),
                _ => changed[dimension] = serde_json::json!("changed-C"),
            }
            let changed =
                serde_json::from_value::<ConfigurationDescriptor>(changed).map_err(error)?;
            check(
                driver
                    .pg_install_membership(&id, &changed, None, true)
                    .is_err(),
                "current-only continuation accepted a changed C descriptor",
            )?;
        }
        check(
            driver
                .pg_install_membership(&id, &current_descriptor, Some(&installed.previous), true)
                .is_err(),
            "delayed joint request downgraded installed current-only native phase",
        )?;
        let mut fresh = id.clone();
        fresh.nonce[0] ^= 1;
        fresh.source = pending.clone();
        check(
            driver
                .pg_propose_session(fresh.source.clone(), fresh.target.clone())
                .await
                .is_err(),
            "source pending publication admitted a fresh session",
        )?;
        check(
            target_driver
                .public_request(Request::Session(Box::new(session::SessionRequest {
                    session: fresh,
                    action: session::SessionAction::Reserve,
                })))
                .await
                .is_err(),
            "pending source admitted a fresh target receiver",
        )?;
        check(
            target_driver.durable.snapshot().await.generation == retained.generation
                && std::fs::read(target.instance().data_dir().join("PG_VERSION"))? == pgdata,
            "incompatible/pending requests mutated copied bytes or metadata",
        )?;
        let exit = Arc::new(DriverExitGate::default());
        let (admin, admin_driver) = target
            .instance()
            .connect_with_driver_exit(exit.clone())
            .await
            .map_err(error)?;
        admin.simple_query("SELECT 1").await.map_err(error)?;
        driver.pg_remove_native_session(&id).await.map_err(error)?;
        exit.entered.notified().await;
        let (stage, receiver, _) = target_driver.pg_session_ownership(&id);
        check(
            receiver
                && matches!(
                    stage,
                    session::SessionStage::Retiring | session::SessionStage::Quarantined
                ),
            "retirement released a receiver before its actual native driver join",
        )?;
        check(
            driver.public_owner().fence.context().map_err(error)?.role == ReplicaRole::Primary,
            "retired-before-delayed continuation relied on source demotion",
        )?;
        check(
            driver
                .public_request(Request::Membership(Box::new(id.clone())))
                .await
                .is_err()
                && PrimaryReplicator::wait_for_catch_up_quorum(
                    driver.as_ref(),
                    ReplicaSetQuorumMode::WriteQuorum,
                )
                .await
                .is_err(),
            "retired source credited a stale exact-session native ACK",
        )?;
        check(
            target_driver
                .public_request(Request::Session(Box::new(session::SessionRequest {
                    session: id.clone(),
                    action: session::SessionAction::Reserve,
                })))
                .await
                .is_err(),
            "retired reservation was revived while actual join was held",
        )?;
        delayed.release.notify_one();
        check(
            target_driver.pg_session_ownership(&id).1,
            "delayed worker release bypassed actual native settlement",
        )?;
        exit.release.notify_one();
        admin_driver.await.map_err(error)?;
        drop(admin);
        driver.pg_retire_session(&id).await.map_err(error)?;
        check(
            !target_driver.pg_session_ownership(&id).1 && !target.instance().is_running().await,
            "two-ended retirement completed before native receiver settlement",
        )?;
        check(
            std::fs::read(target.instance().data_dir().join("PG_VERSION"))? == pgdata,
            "retirement erased or reinitialized copied PGDATA",
        )?;
        current.release.notify_one();
        Ok(())
    }
    .await;
    finished(cluster.finish(result).await);
}
