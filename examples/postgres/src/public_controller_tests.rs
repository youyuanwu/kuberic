//! Actual production PostgreSQL engine, host and controller qualification.
use super::*;
use crate::service::{PgService, PgServiceConfig};
use kuberic_controller::test_util::{
    ApplicationFactory, Barrier, BarrierStage, CreatedApplication, ErrorKind, ReplicaContext,
    TestCluster, TestError,
};
use kuberic_runtime::protocol::public_operations::PublicV1CallKind;
use std::sync::Mutex as StdMutex;
use tokio_stream::wrappers::TcpListenerStream;

type TestResult<T = ()> = kuberic_controller::test_util::Result<T>;

fn error(source: impl std::error::Error + Send + Sync + 'static) -> TestError {
    let mut diagnostic = source.to_string();
    let mut cause = source.source();
    while let Some(next) = cause {
        diagnostic.push_str(&format!(": {next}"));
        cause = next.source();
    }
    TestError::with_source(
        ErrorKind::Application,
        format!("native controller qualification: {diagnostic}"),
        source,
    )
}
fn check(condition: bool, message: &str) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(TestError::new(ErrorKind::Application, message))
    }
}

struct Listener {
    service: Arc<PgService>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<std::result::Result<(), tonic::transport::Error>>,
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
        // PostgreSQL's Unix socket path is bounded by sockaddr_un. The harness
        // retains long incarnation paths; this owned alias names the same
        // declared backing storage, not a different storage incarnation.
        static NEXT_ALIAS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let parent = std::env::var_os("KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/postgres-public-v1")
            });
        std::fs::create_dir_all(&parent)?;
        let alias = parent.join(format!(
            "c{}-{}",
            std::process::id(),
            NEXT_ALIAS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
        let service = Arc::new(PgService::deferred(config));
        let cancel = CancellationToken::new();
        let stopped = cancel.clone();
        let native = crate::data_service::PgDataServiceImpl::new(service.clone(), String::new())
            .into_server();
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(native)
                .serve_with_incoming_shutdown(
                    TcpListenerStream::new(listener),
                    stopped.cancelled_owned(),
                )
                .await
        });
        self.0.lock().unwrap().push(Listener {
            service: service.clone(),
            cancel,
            task,
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
        if let Some(listener) = &listener {
            listener.cancel.cancel();
        }
        if let Err(source) = kuberic_runtime::StatefulServiceReplica::close(service.as_ref()).await
        {
            failures.push(source.to_string());
        }
        if let Some(listener) = listener {
            match listener.task.await {
                Ok(Ok(())) => {}
                Ok(Err(source)) => failures.push(source.to_string()),
                Err(source) => failures.push(source.to_string()),
            }
            if tokio::net::TcpStream::connect(listener.address)
                .await
                .is_ok()
            {
                failures.push("owned native coordination listener survived".into());
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
) -> TestResult<(tokio_postgres::Client, tokio::task::JoinHandle<()>)> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match service.instance().connect_application().await {
                Ok(connection) => return connection,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .map_err(error)
}

async fn administrative_database(
    service: &PgService,
) -> TestResult<(tokio_postgres::Client, tokio::task::JoinHandle<()>)> {
    // Native/admin evidence is deliberately separate from ordinary SQL access.
    let connection_string = service.instance().connection_string().replace(
        "dbname=postgres",
        &format!("dbname={}", crate::access::APPLICATION_DATABASE),
    );
    let (client, connection) = tokio_postgres::connect(&connection_string, tokio_postgres::NoTls)
        .await
        .map_err(error)?;
    let owner = tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok((client, owner))
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_controller_formation() {
    run(false, None).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_controller_cleanup_failure() {
    run(true, None).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_controller_delayed_build_invalidation() {
    run(false, Some(true)).await;
    run(false, Some(false)).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_controller_pre_effect_invalidation() {
    for source_cut in [true, false] {
        let factory = Factory::default();
        let mut cluster = TestCluster::builder(factory.clone())
            .initial_replicas(1)
            .scenario_timeout(Duration::from_secs(120))
            .start()
            .await
            .unwrap();
        let mut probes = Vec::new();
        let mut inspected = None;
        let result: TestResult = async {
            let topology = cluster.settle_members(1).await?;
            let source = cluster.service(topology.primary()).await?;
            cluster.scale_to(2).await?;
            let intent = cluster
                .pause_at(Barrier::intent(PublicV1CallKind::Build))
                .await?;
            let command = intent
                .evidence()
                .operation()
                .command()
                .map_err(|source| TestError::new(ErrorKind::Application, source.to_string()))?;
            let Some(PublicOperationProgram::Build(input)) = command.intent.program.as_ref() else {
                return Err(TestError::new(
                    ErrorKind::Application,
                    "pre-effect Build missing",
                ));
            };
            let observation = cluster.observe().await?;
            let target_ref = observation
                .replicas()
                .find(|reference| reference.identity() == &input.replica)
                .cloned()
                .ok_or_else(|| {
                    TestError::new(ErrorKind::Application, "pre-effect target missing")
                })?;
            let target = cluster.service(&target_ref).await?;
            let faulted = if source_cut {
                source.native_driver()
            } else {
                target.native_driver()
            };
            inspected = Some(faulted.clone());
            let gate = faulted.pause_public_build_effects();
            let attempt = input.attempt.clone();
            cluster.resume(intent).await?;
            cluster
                .drive(async {
                    gate.entered.notified().await;
                    Ok::<(), TestError>(())
                })
                .await?;
            let authority = source
                .native_driver()
                .public_owner()
                .host
                .get()
                .unwrap()
                .outbound_build_authority(&attempt)
                .await
                .map_err(error)?;
            check(
                faulted
                    .public_owner()
                    .effect_starts
                    .load(std::sync::atomic::Ordering::SeqCst)
                    == 0,
                "effects began before serialized admission",
            )?;
            probes.push(ProcessProbe::postgres(source.instance().data_dir()));
            faulted
                .public_owner()
                .host
                .get()
                .unwrap()
                .report_fault(FaultType::Transient)
                .await
                .map_err(error)?;
            check(
                faulted
                    .public_owner()
                    .host
                    .get()
                    .unwrap()
                    .validate_build_authority(&authority)
                    .await
                    .is_err(),
                "pre-effect authority remained live",
            )?;
            gate.release.notify_one();
            cluster
                .probe(async {
                    tokio::time::timeout(Duration::from_secs(15), async {
                        loop {
                            let target_settled = if source_cut {
                                target
                                    .native_driver()
                                    .public_owner()
                                    .inbound_owner
                                    .lock()
                                    .unwrap()
                                    .is_none()
                            } else {
                                target
                                    .native_driver()
                                    .public_owner()
                                    .inbound_owner
                                    .lock()
                                    .unwrap()
                                    .as_ref()
                                    .is_some_and(|owner| owner.result.borrow().is_some())
                            };
                            if target_settled
                                && source
                                    .native_driver()
                                    .public_owner()
                                    .host
                                    .get()
                                    .unwrap()
                                    .validate_build_authority(&authority)
                                    .await
                                    .is_err()
                            {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    })
                    .await
                    .map_err(error)
                })
                .await?;
            check(
                faulted
                    .public_owner()
                    .effect_starts
                    .load(std::sync::atomic::Ordering::SeqCst)
                    == 0,
                "fenced queued request acquired mutation/cleanup scope",
            )?;
            check(
                !target.instance().data_dir().join("PG_VERSION").exists()
                    && target
                        .native_driver()
                        .durable
                        .snapshot()
                        .await
                        .system_identifier
                        .is_none(),
                "fenced pre-effect request prepared native database/copy state",
            )?;
            if source_cut {
                check(
                    source
                        .instance()
                        .data_dir()
                        .join("pg_replslot")
                        .read_dir()
                        .map_err(error)?
                        .next()
                        .is_none(),
                    "dead source attempt created a persistent physical slot",
                )?;
                check(
                    target
                        .native_driver()
                        .public_owner()
                        .inbound_owner
                        .lock()
                        .unwrap()
                        .is_none(),
                    "dead source attempt launched a native target owner",
                )?;
            }
            Ok(())
        }
        .await;
        cluster.finish(result).await.unwrap_or_else(|failure| {
            panic!(
                "pre-effect source={source_cut}: {failure:#}; sources {:?}",
                failure
                    .sources()
                    .map(|source| source.to_string())
                    .collect::<Vec<_>>()
            )
        });
        assert_eq!(
            inspected
                .unwrap()
                .public_owner()
                .effect_starts
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        for probe in probes {
            probe.assert_gone().await;
        }
        assert!(factory.0.lock().unwrap().is_empty());
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_graceful_close_observes_acknowledged_tail() {
    // Direct protected-Close preservation, not a controller-formation witness.
    let directory = OracleDataDir::new();
    let hosted = production_service(directory.path().join("gc"), 301).await;
    let before = hosted.driver.durable.snapshot().await.policy_certified_lsn;
    let (sql, connection) = hosted.driver.instance.connect_application().await.unwrap();
    sql.batch_execute(
        "CREATE TABLE graceful_tail(id integer primary key);
        INSERT INTO graceful_tail SELECT value FROM generate_series(1,64) AS value",
    )
    .await
    .unwrap();
    drop(sql);
    connection.await.unwrap();
    let acknowledged = hosted.driver.current_progress().await.unwrap();
    assert!(acknowledged > before);
    assert!(
        hosted.driver.durable.snapshot().await.policy_certified_lsn < acknowledged,
        "test must not persist an explicit snapshot before graceful Close"
    );
    let process = ProcessProbe::postgres(hosted.driver.instance.data_dir());
    Replicator::close(hosted.driver.as_ref()).await.unwrap();
    let final_state = hosted.driver.durable.revalidate().await.unwrap();
    assert!(
        final_state.policy_certified_lsn >= acknowledged,
        "graceful Close skipped final durable native observation"
    );
    assert!(final_state.postgres_stopped && final_state.external_access_closed);
    process.assert_reaped();
    assert!(hosted.driver.public_owner().owner.lock().await.is_none());
    assert!(hosted.driver.fault_owner.lock().await.is_none());
    hosted.shutdown().await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn public_v1_postgres_graceful_close_cancels_source_inspect() {
    // Real controller dispatch/transport establishes the pending native Build;
    // the terminal action here is deliberately direct protected Close.
    for cached in [false, true] {
        let factory = Factory::default();
        let mut cluster = TestCluster::builder(factory.clone())
            .initial_replicas(1)
            .scenario_timeout(Duration::from_secs(120))
            .start()
            .await
            .unwrap();
        let mut gates = Vec::<Arc<BuildCompletionGate>>::new();
        let mut observer = None;
        let mut probes = Vec::new();
        let result: TestResult = async {
            let singleton = cluster.settle_members(1).await?;
            let source = cluster.service(singleton.primary()).await?;
            cluster.scale_to(2).await?;
            let intent = cluster
                .pause_at(Barrier::intent(PublicV1CallKind::Build))
                .await?;
            let command =
                intent.evidence().operation().command().map_err(|failure| {
                    TestError::new(ErrorKind::Application, failure.to_string())
                })?;
            let Some(PublicOperationProgram::Build(input)) = command.intent.program.as_ref() else {
                return Err(TestError::new(
                    ErrorKind::Application,
                    "Inspect-cut Build missing",
                ));
            };
            let observation = cluster.observe().await?;
            let reference = observation
                .replicas()
                .find(|replica| replica.identity() == &input.replica)
                .cloned()
                .ok_or_else(|| {
                    TestError::new(ErrorKind::Application, "Inspect-cut target missing")
                })?;
            let target = cluster.service(&reference).await?;
            let inspect = if cached {
                let completion = source.native_driver().pause_public_build_completion();
                gates.push(completion.clone());
                cluster.resume(intent).await?;
                cluster
                    .drive(async {
                        completion.entered.notified().await;
                        Ok::<(), TestError>(())
                    })
                    .await?;
                let binding = target
                    .native_driver()
                    .public_owner()
                    .state
                    .lock()
                    .await
                    .built
                    .as_ref()
                    .ok_or_else(|| {
                        TestError::new(ErrorKind::Application, "actual copied target missing")
                    })?
                    .binding
                    .clone();
                let inspect = source.native_driver().pause_public_source_inspect();
                gates.push(inspect.clone());
                let sender = source.native_driver().clone();
                observer = Some(tokio::spawn(async move {
                    sender
                        .public_rpc(
                            &binding.target.replication_address,
                            &Request::Build(binding.clone()),
                        )
                        .await
                }));
                inspect
            } else {
                let inspect = source.native_driver().pause_public_source_inspect();
                gates.push(inspect.clone());
                cluster.resume(intent).await?;
                inspect
            };
            cluster
                .drive(async {
                    inspect.entered.notified().await;
                    Ok::<(), TestError>(())
                })
                .await?;
            let effects = target
                .native_driver()
                .public_owner()
                .effect_starts
                .load(std::sync::atomic::Ordering::SeqCst);
            check(
                effects == usize::from(cached),
                "Inspect reader duplicated/acquired mutation ownership",
            )?;
            probes.push(ProcessProbe::postgres(source.instance().data_dir()));
            if cached {
                probes.push(ProcessProbe::postgres(target.instance().data_dir()));
            }
            // Do not release the genuine source response until Close completes.
            // An unrelated Abort must not be needed to release this reader.
            cluster
                .probe(async {
                    tokio::time::timeout(
                        Duration::from_secs(10),
                        Replicator::close(target.native_driver().as_ref()),
                    )
                    .await
                    .map_err(error)?
                    .map_err(error)
                })
                .await?;
            check(
                target.native_driver().public_closed(),
                "Close failed to join pending Inspect owner",
            )?;
            check(
                target
                    .native_driver()
                    .public_owner()
                    .effect_starts
                    .load(std::sync::atomic::Ordering::SeqCst)
                    == effects,
                "cancelled Inspect observer entered new transfer effects",
            )?;
            if !cached {
                check(
                    !target.instance().data_dir().join("PG_VERSION").exists()
                        && target
                            .native_driver()
                            .durable
                            .snapshot()
                            .await
                            .system_identifier
                            .is_none(),
                    "preflight cancellation mutated native copy state",
                )?;
            }
            if let Some(task) = observer.as_mut() {
                check(
                    tokio::time::timeout(Duration::from_secs(5), task)
                        .await
                        .map_err(error)?
                        .map_err(error)?
                        .is_err(),
                    "cached observer credited completion after Close",
                )?;
                observer.take();
            }
            Ok(())
        }
        .await;
        for gate in gates {
            gate.release.notify_one();
        }
        if let Some(task) = observer {
            task.abort();
            let _ = task.await;
        }
        cluster
            .finish(result)
            .await
            .unwrap_or_else(|failure| panic!("cached={cached}: {failure:#}"));
        for probe in probes {
            probe.assert_gone().await;
        }
        assert!(factory.0.lock().unwrap().is_empty());
    }
}

async fn run(fail_cleanup: bool, invalidate_source: Option<bool>) {
    let factory = Factory::default();
    let mut cluster = TestCluster::builder(factory.clone())
        .initial_replicas(1)
        .scenario_timeout(Duration::from_secs(120))
        .teardown_timeout(Duration::from_secs(40))
        .start()
        .await
        .expect("native controller harness starts");
    let mut probes = Vec::new();
    let mut retained = None;
    let mut queued_replay = None;
    let mut cancelled_replay = None;
    let mut pending_replay_gate = None;
    let result:TestResult=async {
        let open=cluster.pause_at(Barrier::completion(PublicV1CallKind::Open)).await?;
        let opened=cluster.observe().await?;
        let (fresh_ref,fresh_report)=opened.reports().next().ok_or_else(||TestError::new(ErrorKind::Application,"fresh Open report missing"))?;
        check(fresh_report.role==ReplicaRole::None
            && fresh_report.progress.current()==Some(kuberic_runtime::application::INVALID_LSN)
            && fresh_report.progress.capability()==Some(kuberic_runtime::application::INVALID_LSN)
            && fresh_report.has_current_open_endpoint(),
            "pre-Role native queries must truthfully preserve unknown history")?;
        let new_open=PublicOperationProgram::Open { replica:fresh_ref.identity().clone(),existing:false };
        check(fresh_report.completed_operations.iter().any(|completion|completion.call_kind==PublicV1CallKind::Open
            && completion.input_digest==new_open.digest() && completion.result==kuberic_runtime::protocol::public_operations::PublicV1OperationResult::Succeeded
            && completion.process_session_id==*fresh_ref.process_session_id()
            && completion.storage_incarnation==*fresh_ref.storage_incarnation() && completion.history==fresh_report.history),
            "initial authority lacked exact successful New-Open provenance")?;
        check(!cluster.service(fresh_ref).await?.instance().is_running().await,"first Role was not held before native initialization")?;
        cluster.resume(open).await?;
        let singleton=cluster.settle_members(1).await?;
        eprintln!("native controller witness: singleton installed");
        let primary=singleton.primary().clone();
        let source=cluster.service(&primary).await?;
        let (sql,connection)=cluster.drive(application_connection(&source)).await?;
        sql.batch_execute("CREATE TABLE controller_native(id integer primary key, value text);
            INSERT INTO controller_native VALUES (1,'before-controller-build')").await.map_err(error)?;
        drop(sql); connection.await.map_err(error)?;
        cluster.scale_to(2).await?;
        let build_intent=cluster.pause_at(Barrier::intent(PublicV1CallKind::Build)).await?;
        let frozen=build_intent.evidence().operation().command().map_err(|source|TestError::new(ErrorKind::Application,source.to_string()))?;
        let Some(PublicOperationProgram::Build(input))=frozen.intent.program.as_ref() else {
            return Err(TestError::new(ErrorKind::Application,"controller Build intent missing"));
        };
        let observation=cluster.observe().await?;
        let target_ref=observation.replicas().find(|replica|replica.identity()==&input.replica).cloned()
            .ok_or_else(||TestError::new(ErrorKind::Application,"prepared idle target missing"))?;
        let target=cluster.service(&target_ref).await?;
        let gate=target.native_driver().pause_public_build_completion();
        let source_gate=source.native_driver().pause_public_build_completion();
        let effects=target.native_driver().pause_public_build_effects();
        cluster.resume(build_intent).await?;
        cluster.drive(async { effects.entered.notified().await; Ok::<(),TestError>(()) }).await?;
        let binding=source.native_driver().public_owner().state.lock().await.grants.iter()
            .find_map(|grant|match &grant.binding { TransportBinding::Build(build) if build.target.replica==*target_ref.identity()=>Some(build.clone()),_=>None })
            .ok_or_else(||TestError::new(ErrorKind::Application,"live native source grant missing"))?;
        let live=binding.authority.as_ref().ok_or_else(||TestError::new(ErrorKind::Application,"host authority missing"))?;
        if !fail_cleanup && invalidate_source.is_none() {
            pending_replay_gate=Some(target.native_driver().pause_public_build_replay());
        }
        let replay_source=source.native_driver().clone(); let replay_binding=binding.clone();
        queued_replay=Some(tokio::spawn(async move {
            replay_source.public_rpc(&replay_binding.target.replication_address,&Request::Build(replay_binding.clone())).await
        }));
        cluster.probe(async {
            tokio::time::timeout(Duration::from_secs(5),async {
                while target.native_driver().public_build_waiters()<2 {tokio::task::yield_now().await;}
            }).await.map_err(error)
        }).await?;
        let replay_source=source.native_driver().clone(); let replay_binding=binding.clone();
        cancelled_replay=Some(tokio::spawn(async move {
            replay_source.public_rpc(&replay_binding.target.replication_address,&Request::Build(replay_binding.clone())).await
        }));
        cluster.probe(async {
            tokio::time::timeout(Duration::from_secs(5),async {
                while target.native_driver().public_build_waiters()<3 {tokio::task::yield_now().await;}
            }).await.map_err(error)
        }).await?;
        let cancelled=cancelled_replay.take().unwrap();
        cancelled.abort(); let _=cancelled.await;
        check(target.native_driver().public_owner().effect_starts.load(std::sync::atomic::Ordering::SeqCst)==0
            && !target.instance().data_dir().join("PG_VERSION").exists(),
            "queued response observer acquired transfer mutation/cleanup ownership")?;
        target.native_driver().public_owner().host.get().unwrap().validate_build_authority(live).await.map_err(error)?;
        effects.release.notify_one();
        cluster.drive(async { gate.entered.notified().await; Ok::<(),TestError>(()) }).await?;
        // The same retained native owner now performed real backup/replay.
        check(target.native_driver().public_owner().effect_starts.load(std::sync::atomic::Ordering::SeqCst)==1,
            "overlapping exact native requests duplicated mutation ownership")?;
        source.native_driver().public_owner().host.get().unwrap().validate_build_authority(live).await.map_err(error)?;
        target.native_driver().public_owner().host.get().unwrap().validate_build_authority(live).await.map_err(error)?;
        let generation=target.instance().generation_id();
        let durable_generation=target.native_driver().durable.snapshot().await.generation;
        let mut missing=binding.clone(); missing.authority=None;
        check(target.native_driver().public_request(Request::Build(missing)).await.is_err(),"hosted native ingress fell back to direct unbound Build")?;
        let encoded=serde_json::to_value(live).map_err(error)?;
        fn leaves(value:&serde_json::Value,path:Vec<String>,out:&mut Vec<Vec<String>>) {
            if let Some(object)=value.as_object() {
                for (key,value) in object { let mut next=path.clone(); next.push(key.clone()); leaves(value,next,out); }
            } else { out.push(path); }
        }
        let mut paths=Vec::new(); leaves(&encoded,vec![],&mut paths);
        for path in paths {
            let mut changed=encoded.clone(); let mut leaf=&mut changed;
            for key in &path { leaf=&mut leaf[key]; }
            *leaf=match leaf {
                serde_json::Value::String(value)=>serde_json::Value::String(format!("{value}-stale")),
                serde_json::Value::Number(value)=>serde_json::json!(value.as_i64().unwrap()+1),
                _=>continue,
            };
            if let Ok(authority)=serde_json::from_value::<PublicInboundBuildAuthority>(changed) {
                let mut changed=binding.clone(); changed.authority=Some(authority);
                let rejected=tokio::time::timeout(Duration::from_secs(2),target.native_driver().public_request(Request::Build(changed))).await.map_err(error)?;
                check(rejected.is_err(),"mutated live native authority reached copy effects/completion credit")?;
            }
        }
        check(target.instance().generation_id()==generation && target.native_driver().durable.snapshot().await.generation==durable_generation,
            "rejected native ingress mutated process or durable copy state")?;
        // Component-level stale-revoke negative, not a replacement for real
        // controller dispatch: a different prior token cannot cancel this live
        // native owner. The host/controller positives above installed it.
        let mut stale=live.clone();
        stale.operation_revision=stale.operation_revision.saturating_sub(1);
        stale.operation_id=kuberic_runtime::protocol::types::OperationId::new("stale-native-revoke");
        CustomReplicatorControl::revoke_inbound_build_authority(target.native_driver().as_ref(),&stale).await.map_err(error)?;
        target.native_driver().public_owner().host.get().unwrap().validate_build_authority(live).await.map_err(error)?;
        check(target.native_driver().public_owner().inbound.lock().unwrap().as_ref()==Some(live),
            "stale native revoke cancelled a different exact receiver owner")?;
        if let Some(source_fault)=invalidate_source {
            probes.push(ProcessProbe::postgres(source.instance().data_dir()));
            probes.push(ProcessProbe::postgres(target.instance().data_dir()));
            let faulted=if source_fault {source.native_driver()} else {target.native_driver()};
            if source_fault {
                // Helper-contract negative: force a retryable first round-trip
                // result, using this genuinely hosted source's live validator.
                // This is not fabricated SQL/transfer success evidence.
                let rounds=std::sync::atomic::AtomicUsize::new(0);
                let context=faulted.public_owner().host.get().unwrap();
                let checked=|| async {
                    context.validate_build_authority(live).await
                        .map_err(|failure|crate::instance::PgError::Process(failure.to_string()))
                };
                let result=crate::native::retire_slot_rounds(&checked,|| async {
                    rounds.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                    context.report_fault(FaultType::Transient).await
                        .map_err(|failure|crate::instance::PgError::Process(failure.to_string()))?;
                    Ok(false)
                }).await;
                check(result.is_err() && rounds.load(std::sync::atomic::Ordering::SeqCst)==1,
                    "slot retry helper issued a new mutation round after source invalidation")?;
            } else {
                faulted.public_owner().host.get().unwrap().report_fault(FaultType::Transient).await.map_err(error)?;
            }
            check(faulted.public_owner().host.get().unwrap().validate_build_authority(live).await.is_err(),
                "supported fault did not synchronously invalidate exact authority")?;
            gate.release.notify_one();
            source_gate.release.notify_one();
            cluster.probe(async {
                tokio::time::timeout(Duration::from_secs(15),async {
                    loop {
                        let source_live=source.native_driver().public_owner().host.get().unwrap().validate_build_authority(live).await.is_ok();
                        if !source_live && !target.instance().is_running().await { break; }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }).await.map_err(error)
            }).await?;
            check(target.native_driver().public_owner().state.lock().await.built.is_none(),
                "delayed native target completion survived authority invalidation")?;
            check(source.native_driver().public_owner().state.lock().await.built_idle.is_empty(),
                "cancelled source callback published built-idle success")?;
            check(source.native_driver().public_owner().host.get().unwrap().outbound_build_authority(&binding.build_id).await.is_err(),
                "cancelled source journal remained live")?;
            let stopped_generation=target.instance().generation_id();
            check(target.native_driver().public_request(Request::Build(binding.clone())).await.is_err(),
                "late old native attempt restarted after containment")?;
            check(target.instance().generation_id()==stopped_generation,"stale attempt mutated contained storage/process scope")?;
            check(queued_replay.take().unwrap().await.map_err(error)?.is_err(),
                "queued native observer credited invalidated completion")?;
            return Ok(());
        }
        gate.release.notify_one();
        cluster.drive(async { source_gate.entered.notified().await; Ok::<(),TestError>(()) }).await?;
        if let Some(gate)=pending_replay_gate.as_ref() {
            cluster.probe(async { gate.entered.notified().await; Ok::<(),TestError>(()) }).await?;
        } else {
            let concurrent=queued_replay.take().unwrap().await.map_err(error)?.map_err(error)?;
            check(matches!(concurrent,Response::Built(ref completion) if completion.binding==binding),
                "overlapping exact native observer did not attach to original completion")?;
        }
        let replay_generation=target.instance().generation_id();
        let replay_metadata=target.native_driver().durable.snapshot().await.generation;
        let replay=source.native_driver().public_rpc(&binding.target.replication_address,&Request::Build(binding.clone())).await.map_err(error)?;
        check(matches!(replay,Response::Built(ref completion) if completion.binding==binding),
            "exact live native retry did not retain its actual completion")?;
        check(target.instance().generation_id()==replay_generation && target.native_driver().durable.snapshot().await.generation==replay_metadata,
            "exact live native retry duplicated native mutation")?;
        source_gate.release.notify_one();
        let copy=cluster.pause_at(Barrier::completion(PublicV1CallKind::Build)).await?;
        eprintln!("native controller witness: source/target Build completed");
        let command=copy.evidence().operation().command().map_err(|source|TestError::new(ErrorKind::Application,source.to_string()))?;
        let Some(PublicOperationProgram::Build(input))=command.intent.program.as_ref() else {
            return Err(TestError::new(ErrorKind::Application,"controller did not dispatch Build"));
        };
        let observed=cluster.observe().await?;
        let target_ref=observed.replicas().find(|replica|replica.identity()==&input.replica)
            .cloned().ok_or_else(||TestError::new(ErrorKind::Application,"Build target report missing"))?;
        let target=cluster.service(&target_ref).await?;
        let native=target.native_driver().public_owner().state.lock().await.built.clone()
            .ok_or_else(||TestError::new(ErrorKind::Application,"actual target native Build completion missing"))?;
        let authority=native.binding.authority.as_ref().ok_or_else(||TestError::new(ErrorKind::Application,"hosted Build lacked exact authority"))?;
        let before=target.instance().generation_id();
        let metadata_before=target.native_driver().durable.snapshot().await.generation;
        check(target.native_driver().public_request(Request::Build(native.binding.clone())).await.is_err(),
            "historical copy completion recreated native ingress")?;
        check(target.instance().generation_id()==before && target.native_driver().durable.snapshot().await.generation==metadata_before,
            "historical native rejection mutated an already-built target")?;
        check(authority.operation_id==command.intent.operation_id && authority.operation_revision==command.intent.revision
            && authority.attempt.build_id==input.attempt
            && authority.attempt.source==*primary.identity() && authority.attempt.source_storage==*primary.storage_incarnation()
            && authority.attempt.source_session==*primary.process_session_id()
            && authority.attempt.target==*target_ref.identity() && authority.attempt.target_storage==*target_ref.storage_incarnation()
            && authority.attempt.target_session==*target_ref.process_session_id()
            && input.history.as_ref()==Some(&authority.history) && input.current_configuration_id.as_ref()==Some(&authority.current_configuration_id)
            && authority.source_address==input.source_address && authority.target_address==input.replication_address
            && authority.attempt.epoch==input.epoch && command.intent.input_digest==command.intent.program.as_ref().unwrap().digest(),
            "native Build differs from exact frozen controller authority")?;
        native.lineage.validate().map_err(|source| TestError::new(ErrorKind::Application, source))?;
        let report=observed.report(&target_ref).ok_or_else(||TestError::new(ErrorKind::Application,"target not observed"))?;
        check(report.role==ReplicaRole::IdleSecondary && report.read_status!=AccessStatus::Granted
            && report.write_status!=AccessStatus::Granted,"Build must remain idle/nonserving")?;
        check(target.instance().connect_application().await.is_err(),"ordinary SQL was open on built idle")?;
        let source_report=observed.report(&primary).ok_or_else(||TestError::new(ErrorKind::Application,"source report missing"))?;
        let old=source_report.configuration.as_ref().ok_or_else(||TestError::new(ErrorKind::Application,"installed source predecessor missing"))?.voting.current.clone();
        check(old.members.len()==1 && source_report.configuration.as_ref().unwrap().idle_replicas==vec![target_ref.identity().replica_id],
            "source must truthfully observe a built idle outside voting")?;
        let target_snapshot=target.native_driver().observer.snapshot().await.map_err(error)?;
        let evidence=target_snapshot.evidence.as_ref().ok_or_else(||TestError::new(ErrorKind::Application,"native target WAL evidence absent"))?;
        check(evidence.in_recovery && evidence.system_identifier==native.lineage.system_identifier
            && evidence.timeline_id==native.lineage.timeline && evidence.received_lsn.is_some_and(|lsn|lsn>=native.boundary)
            && evidence.replay_lsn.is_some_and(|lsn|lsn>=native.boundary),"actual backup/lineage/receive/replay did not cover Build boundary")?;
        let (admin,admin_connection)=administrative_database(&target).await?;
        check(admin.query_one("SELECT value FROM controller_native WHERE id=1",&[]).await.map_err(error)?.get::<_,String>(0)=="before-controller-build",
            "nonempty pre-Build state missing from native target")?;
        drop(admin); admin_connection.await.map_err(error)?;
        cluster.resume(copy).await?;
        let epoch=cluster.pause_at(Barrier::Operation { stage:BarrierStage::Completion,kind:Some(PublicV1CallKind::Epoch),participant:Some(primary.clone()) }).await?;
        let observed=cluster.observe().await?;
        check(observed.report(&primary).and_then(|report|report.configuration.as_ref()).is_some_and(|configuration|configuration.voting.current==old),
            "source Epoch erased truthful predecessor membership")?;
        cluster.resume(epoch).await?;
        let joint=cluster.pause_at(Barrier::Operation { stage:BarrierStage::Intent,kind:Some(PublicV1CallKind::CatchUp),participant:Some(primary.clone()) }).await?;
        let command=joint.evidence().operation().command().map_err(|source|TestError::new(ErrorKind::Application,source.to_string()))?;
        let Some(PublicOperationProgram::CatchUp {configuration:joint_configuration,..})=command.intent.program.as_ref() else {
            return Err(TestError::new(ErrorKind::Application,"controller skipped joint catch-up"));
        };
        let expected=kuberic_runtime::protocol::types::ConfigurationDescriptor::new(joint_configuration.current.epoch,primary.identity().replica_id,vec![
            kuberic_runtime::protocol::types::ConfigurationMember {identity:primary.identity().clone(),role:ReplicaRole::Primary},
            kuberic_runtime::protocol::types::ConfigurationMember {identity:target_ref.identity().clone(),role:ReplicaRole::ActiveSecondary},
        ],2);
        check(joint_configuration.previous.as_ref()==Some(&old) && joint_configuration.current==expected
            && expected.epoch.data_loss_number==old.epoch.data_loss_number && expected.epoch>old.epoch
            && joint_configuration.replicas.len()==1 && joint_configuration.replicas[0].identity==*target_ref.identity()
            && joint_configuration.replicas[0].process_session_id==*target_ref.process_session_id()
            && joint_configuration.replicas[0].storage_incarnation.as_ref()==Some(target_ref.storage_incarnation())
            && Some(joint_configuration.replicas[0].replication_address.as_str())==target_ref.replication_address()
            && joint_configuration.replicas[0].role==ReplicaRole::ActiveSecondary,
            "joint configuration lost exact old/current descriptor or peer binding")?;
        let joint_current=joint_configuration.current.clone();
        let joint_peers=joint_configuration.replicas.clone();
        cluster.resume(joint).await?;
        let joint_done=cluster.pause_at(Barrier::Operation {stage:BarrierStage::Completion,kind:Some(PublicV1CallKind::CatchUp),participant:Some(primary.clone())}).await?;
        let native_peer=source.native_driver().public_owner().state.lock().await.grants.iter().find_map(|grant|match &grant.binding {TransportBinding::Peer(peer)=>Some(peer.clone()),_=>None})
            .ok_or_else(||TestError::new(ErrorKind::Application,"actual native joint peer missing"))?;
        check(native_peer.epoch==joint_current.epoch && native_peer.current_configuration_id==joint_current.configuration_id
            && native_peer.previous_configuration_id.as_ref()==Some(&old.configuration_id)
            && native_peer.primary.replica==*primary.identity() && native_peer.primary.process_session_id==*primary.process_session_id()
            && native_peer.primary.role==ReplicaRole::Primary && native_peer.secondary.role==ReplicaRole::ActiveSecondary
            && Some(native_peer.primary.replication_address.as_str())==primary.replication_address()
            && native_peer.secondary.replica==*target_ref.identity() && native_peer.secondary.process_session_id==*target_ref.process_session_id()
            && Some(native_peer.secondary.replication_address.as_str())==target_ref.replication_address(),
            "native joint peer does not match frozen configuration/session endpoints")?;
        check(target.native_driver().public_owner().state.lock().await.receiver.as_ref()==Some(&TransportBinding::Peer(native_peer.clone())),
            "native target did not admit the actual joint peer")?;
        cluster.resume(joint_done).await?;
        let final_configuration=cluster.pause_at(Barrier::Operation { stage:BarrierStage::Completion,kind:Some(PublicV1CallKind::Configuration),participant:Some(primary.clone()) }).await?;
        let command=final_configuration.evidence().operation().command().map_err(|source|TestError::new(ErrorKind::Application,source.to_string()))?;
        let Some(PublicOperationProgram::Configuration(configuration))=command.intent.program.as_ref() else {
            return Err(TestError::new(ErrorKind::Application,"current-only configuration missing"));
        };
        check(configuration.previous.is_none() && configuration.current==joint_current
            && configuration.replicas.len()==joint_peers.len()
            && configuration.replicas.iter().zip(&joint_peers).all(|(current,joint)|current.identity==joint.identity
                && current.process_session_id==joint.process_session_id && current.storage_incarnation==joint.storage_incarnation
                && current.replication_address==joint.replication_address && current.role==joint.role),
            "current-only activation changed the joint current descriptor or exact peer scope")?;
        let mut expected_native=native_peer;
        expected_native.previous_configuration_id=None;
        let final_peer=source.native_driver().public_owner().state.lock().await.grants.iter()
            .find_map(|grant|match &grant.binding {TransportBinding::Peer(peer)=>Some(peer.clone()),_=>None});
        check(final_peer.as_ref()==Some(&expected_native)
            && target.native_driver().public_owner().state.lock().await.receiver.as_ref()==Some(&TransportBinding::Peer(expected_native)),
            "native current-only activation did not remove exactly previous participation")?;
        cluster.resume(final_configuration).await?;
        let topology=cluster.settle_members(2).await?;
        check(topology.primary()==&primary,"qualification unexpectedly replaced its source")?;
        let observed=cluster.observe().await?;
        for reference in topology.members() {
            let report=observed.report(reference).ok_or_else(||TestError::new(ErrorKind::Application,"admitted report missing"))?;
            check(report.history==authority.history && report.storage_incarnation==*reference.storage_incarnation()
                && report.process_session_id==*reference.process_session_id()
                && report.configuration.as_ref().is_some_and(|configuration| configuration.voting.current.members.len()==2
                    && configuration.voting.previous.is_none() && configuration.idle_replicas.is_empty()),
                "admission report scope/membership is not exact")?;
        }
        check(target.instance().connect_application().await.is_err(),"stable secondary replay was mistaken for ordinary read access")?;
        if let Some(gate)=pending_replay_gate.take() {
            let generation=target.instance().generation_id();
            let metadata=target.native_driver().durable.snapshot().await.generation;
            let receiver=target.native_driver().public_owner().state.lock().await.receiver.clone();
            gate.release.notify_one();
            check(queued_replay.take().unwrap().await.map_err(error)?.is_err(),
                "queued old observer survived copy-to-peer authority advancement")?;
            check(target.instance().generation_id()==generation
                && target.native_driver().durable.snapshot().await.generation==metadata
                && target.native_driver().public_owner().state.lock().await.receiver==receiver,
                "queued old observer cleaned up or rewrote the admitted peer owner")?;
        }
        let successor_receiver=target.native_driver().public_owner().state.lock().await.receiver.clone();
        let successor_generation=target.instance().generation_id();
        let successor_metadata=target.native_driver().durable.snapshot().await.generation;
        check(source.native_driver().public_rpc(&native.binding.target.replication_address,&Request::Build(native.binding.clone())).await.is_err(),
            "old copy authority was admitted after later role/epoch/configuration ownership")?;
        // Component-negative late old callback against the real admitted native
        // successor. Actual install/revoke/admission above used the controller.
        CustomReplicatorControl::revoke_inbound_build_authority(target.native_driver().as_ref(),authority).await.map_err(error)?;
        check(target.native_driver().public_owner().state.lock().await.receiver==successor_receiver
            && target.instance().generation_id()==successor_generation
            && target.native_driver().durable.snapshot().await.generation==successor_metadata,
            "stale copy revoke/result disturbed the actual admitted native successor")?;
        let (sql,connection)=cluster.drive(application_connection(&source)).await?;
        cluster.drive(async { sql.execute("INSERT INTO controller_native VALUES (2,'after-joint-admission')",&[]).await.map(|_|()).map_err(error) }).await?;
        let (admin,admin_connection)=source.instance().connect().await.map_err(error)?;
        check(admin.query_one("SELECT count(*)::bigint FROM pg_stat_replication WHERE sync_state IN ('sync','quorum') AND replay_lsn IS NOT NULL",&[]).await.map_err(error)?.get::<_,i64>(0)>0,
            "acknowledged post-admission write had no actual synchronous native participant")?;
        drop(admin); admin_connection.await.map_err(error)?;
        let (standby,standby_connection)=administrative_database(&target).await?;
        check(standby.query_one("SELECT value FROM controller_native WHERE id=2",&[]).await.map_err(error)?.get::<_,String>(0)=="after-joint-admission",
            "acknowledged post-admission write was not replayed")?;
        drop(standby); standby_connection.await.map_err(error)?;
        probes.push(ProcessProbe::postgres(source.instance().data_dir()));
        probes.push(ProcessProbe::postgres(target.instance().data_dir()));
        if fail_cleanup {
            let state=source.native_driver().public_owner().state.lock().await;
            check(!state.grants.is_empty(),"no real native gateway available for cleanup failure")?;
            state.grants[0].gateway.fail_task();
            retained=Some(source.clone());
            drop(sql); connection.await.map_err(error)?;
        } else {
            cluster.stop(&primary).await?;
            check(!source.instance().is_running().await,"native SQL fencing did not complete")?;
            check(sql.execute("INSERT INTO controller_native VALUES (999,'forbidden-after-completed-fence')",&[]).await.is_err(),
                "retained SQL socket wrote after completed native fence")?;
            drop(sql); connection.await.map_err(error)?;
            check(source.instance().connect_application().await.is_err(),"new SQL socket bypassed completed native fence")?;
        }
        Ok(())
    }.await;
    if let Some(gate) = pending_replay_gate.take() {
        gate.release.notify_one();
    }
    if let Some(observer) = queued_replay.take() {
        observer.abort();
        let _ = observer.await;
    }
    if let Some(observer) = cancelled_replay.take() {
        observer.abort();
        let _ = observer.await;
    }
    let finished = cluster.finish(result).await;
    if fail_cleanup {
        assert!(
            finished.is_err(),
            "failed native gateway cleanup must propagate through finish"
        );
        let source =
            retained.expect("native positive journey must finish before cleanup injection");
        assert!(!source.instance().is_running().await);
        assert!(
            source
                .native_driver()
                .public_owner()
                .owner
                .lock()
                .await
                .is_none(),
            "periodic native owner was not joined after cleanup error"
        );
        assert!(
            source.native_driver().fault_owner.lock().await.is_none(),
            "fault reporter was not joined after cleanup error"
        );
        for owner in source
            .native_driver()
            .public_owner()
            .gateway_drains
            .lock()
            .unwrap()
            .clone()
        {
            assert!(
                owner.lock().await.is_none(),
                "remaining gateway join ownership was lost"
            );
        }
    } else {
        finished.unwrap_or_else(|error| {
            panic!(
                "native controller journey: {error:#} sources: {:?}",
                error
                    .sources()
                    .map(|source| source.to_string())
                    .collect::<Vec<_>>()
            )
        });
    }
    for probe in probes {
        probe.assert_gone().await;
    }
    assert!(
        factory.0.lock().unwrap().is_empty(),
        "native listeners were not joined"
    );
}
