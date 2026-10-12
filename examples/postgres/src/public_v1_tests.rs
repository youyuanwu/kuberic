use super::*;
use crate::durable::{PgDurableIdentity, StorageMode};
use crate::testing::{allocate_port, find_pg_bin, native_configuration, native_identity};
use kuberic_runtime::protocol::types::ResourceUid;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn test_endpoint(
    replica: ReplicaIdentity,
    session: &str,
    address: &str,
    role: ReplicaRole,
) -> NativeEndpoint {
    NativeEndpoint {
        replica,
        process_session_id: ProcessSessionId::new(session),
        replication_address: address.into(),
        role,
    }
}

#[test]
fn public_v1_process_fence_rejects_same_epoch_session_and_address_substitution() {
    let source = native_identity(1, "source");
    let target = native_identity(2, "target");
    let epoch = Epoch::new(0, 7);
    let source_fence = LifecycleFence::new(
        source.clone(),
        ProcessSessionId::new("source-session"),
        "http://source".into(),
    );
    source_fence
        .update(|context| {
            context.role = ReplicaRole::Primary;
            context.epoch = epoch;
        })
        .unwrap();
    let target_fence = LifecycleFence::new(
        target.clone(),
        ProcessSessionId::new("target-session"),
        "http://target".into(),
    );
    target_fence
        .update(|context| {
            context.role = ReplicaRole::IdleSecondary;
            context.epoch = epoch;
        })
        .unwrap();
    let binding = TransportBinding::Build(BuildBinding {
        resource_uid: ResourceUid::new("authority-test"),
        epoch,
        build_id: OperationId::new("build"),
        source: test_endpoint(
            source,
            "source-session",
            "http://source",
            ReplicaRole::Primary,
        ),
        target: test_endpoint(
            target,
            "target-session",
            "http://target",
            ReplicaRole::IdleSecondary,
        ),
    });
    source_fence.validate_binding(&binding).unwrap();
    target_fence.validate_binding(&binding).unwrap();
    for substitute_source_session in [true, false] {
        let mut substituted = binding.clone();
        let TransportBinding::Build(build) = &mut substituted else {
            unreachable!()
        };
        if substitute_source_session {
            build.source.process_session_id = ProcessSessionId::new("other-source-session");
        } else {
            build.source.replication_address = "http://other-source".into();
        }
        assert!(matches!(
            source_fence.validate_binding(&substituted),
            Err(RuntimeError::AuthorityNotAdmitted)
        ));
    }
    for substitute_target_session in [true, false] {
        let mut substituted = binding.clone();
        let TransportBinding::Build(build) = &mut substituted else {
            unreachable!()
        };
        if substitute_target_session {
            build.target.process_session_id = ProcessSessionId::new("other-target-session");
        } else {
            build.target.replication_address = "http://other-target".into();
        }
        assert!(matches!(
            target_fence.validate_binding(&substituted),
            Err(RuntimeError::AuthorityNotAdmitted)
        ));
    }
}

#[test]
fn public_v1_secondary_admits_joint_then_current_only_authority() {
    let source = native_identity(1, "source");
    let target = native_identity(2, "target");
    let epoch = Epoch::new(0, 9);
    let fence = LifecycleFence::new(
        target.clone(),
        ProcessSessionId::new("target-session"),
        "http://target".into(),
    );
    fence
        .update(|context| {
            context.role = ReplicaRole::ActiveSecondary;
            context.epoch = epoch;
        })
        .unwrap();
    let joint = PeerBinding {
        epoch,
        previous_configuration_id: Some(ConfigurationId::new("cfg-8")),
        current_configuration_id: ConfigurationId::new("cfg-9"),
        primary: test_endpoint(
            source,
            "source-session",
            "http://source",
            ReplicaRole::Primary,
        ),
        secondary: test_endpoint(
            target,
            "target-session",
            "http://target",
            ReplicaRole::ActiveSecondary,
        ),
    };
    let admitted = fence.admit_peer(&joint).unwrap();
    assert_eq!(
        admitted.previous_configuration_id,
        joint.previous_configuration_id
    );
    assert_eq!(
        admitted.current_configuration_id.as_ref(),
        Some(&joint.current_configuration_id)
    );
    let current_only = PeerBinding {
        previous_configuration_id: None,
        ..joint
    };
    let admitted = fence.admit_peer(&current_only).unwrap();
    assert_eq!(admitted.previous_configuration_id, None);
    assert_eq!(
        admitted.current_configuration_id.as_ref(),
        Some(&current_only.current_configuration_id)
    );
}

struct BindingFactory;

impl BindingFactory {
    fn new() -> Self {
        Self
    }

    fn build(&self, source: &Replica, target: &Replica, id: &str) -> BuildBinding {
        let context = source.driver.public_owner().fence.context().unwrap();
        BuildBinding {
            resource_uid: ResourceUid::new("postgres-public-oracle"),
            epoch: context.epoch,
            build_id: OperationId::new(id),
            source: NativeEndpoint {
                replica: source.identity.clone(),
                process_session_id: source.session.clone(),
                replication_address: source.address.clone(),
                role: ReplicaRole::Primary,
            },
            target: NativeEndpoint {
                replica: target.identity.clone(),
                process_session_id: target.session.clone(),
                replication_address: target.address.clone(),
                role: ReplicaRole::IdleSecondary,
            },
        }
    }

    fn peer(
        &self,
        source: &Replica,
        target: &Replica,
        current: &ConfigurationDescriptor,
        previous: Option<&ConfigurationDescriptor>,
    ) -> PeerBinding {
        PeerBinding {
            epoch: current.epoch,
            current_configuration_id: current.configuration_id.clone(),
            previous_configuration_id: previous
                .map(|configuration| configuration.configuration_id.clone()),
            primary: NativeEndpoint {
                replica: source.identity.clone(),
                process_session_id: source.session.clone(),
                replication_address: source.address.clone(),
                role: ReplicaRole::Primary,
            },
            secondary: NativeEndpoint {
                replica: target.identity.clone(),
                process_session_id: target.session.clone(),
                replication_address: target.address.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        }
    }
}

struct Replica {
    driver: Arc<PgReplicator>,
    identity: ReplicaIdentity,
    address: String,
    session: ProcessSessionId,
    server: tokio::task::JoinHandle<()>,
    root: std::path::PathBuf,
}

struct OracleDataDir {
    path: std::path::PathBuf,
}

impl OracleDataDir {
    fn new() -> Self {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::var_os("KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                #[cfg(unix)]
                {
                    std::path::PathBuf::from("/tmp")
                }
                #[cfg(not(unix))]
                {
                    std::env::temp_dir()
                }
            });
        let path = base.join(format!(
            "kuberic-public-v1-postgres-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for OracleDataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl Replica {
    async fn new(root: std::path::PathBuf, id: i64) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let identity = native_identity(id, &format!("public-{id}"));
        let resource = ResourceUid::new("postgres-public-oracle");
        let durable = Arc::new(
            PgDurableStore::open(
                root.join("a"),
                PgDurableIdentity {
                    resource_uid: resource,
                    replica: identity.clone(),
                },
                StorageMode::Fresh,
            )
            .await
            .unwrap(),
        );
        let instance = Arc::new(PgInstanceManager::new(
            root.join("d"),
            find_pg_bin(),
            allocate_port().await,
        ));
        let (fault_tx, _fault_rx) = mpsc::channel(8);
        let driver = Arc::new(
            PgReplicator::new(
                instance,
                durable,
                fault_tx,
                Weak::new(),
                CancellationToken::new(),
                crate::data_service::PgCoordination {
                    local_endpoint: address.clone(),
                    bearer_token: String::new(),
                },
                true,
            )
            .await,
        );
        let session = ProcessSessionId::new(format!("session-{id}"));
        assert!(
            driver
                .public_v1
                .set(PublicPg::new(
                    identity.clone(),
                    session.clone(),
                    address.clone(),
                ))
                .is_ok()
        );
        driver.start_public_owner();
        Replicator::open(driver.as_ref()).await.unwrap();
        let service = TestService(driver.clone());
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    crate::proto::pg_data_service_server::PgDataServiceServer::new(service),
                )
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        Self {
            driver,
            identity,
            address,
            session,
            server,
            root,
        }
    }

    async fn change_role(&self, epoch: i64, role: ReplicaRole) {
        self.driver
            .public_change_role(Epoch::new(0, epoch), role)
            .await
            .unwrap();
    }

    async fn role(&self, epoch: i64, role: ReplicaRole) {
        self.change_role(epoch, role).await;
        self.epoch(epoch).await;
    }

    async fn epoch(&self, epoch: i64) {
        self.driver
            .public_update_epoch(Epoch::new(0, epoch))
            .await
            .unwrap();
    }

    async fn singleton(&self, epoch: i64) {
        self.change_role(epoch, ReplicaRole::Primary).await;
        self.driver
            .public_application_role(ReplicaRole::Primary)
            .await
            .unwrap();
        assert!(self.driver.instance.connect_application().await.is_err());
        let config = native_configuration(std::slice::from_ref(&self.identity), 0, epoch);
        self.driver
            .public_configuration(config.into(), None)
            .await
            .unwrap();
        self.driver
            .public_application_role(ReplicaRole::Primary)
            .await
            .unwrap();
    }

    fn persisted(&self) -> Vec<Vec<u8>> {
        ["a/state-v2.json", "d/pg_hba.conf", "d/postgresql.auto.conf"]
            .map(|path| std::fs::read(self.root.join(path)).unwrap_or_default())
            .into()
    }
}

impl Drop for Replica {
    fn drop(&mut self) {
        self.driver.abort();
        self.server.abort();
    }
}

struct TestService(Arc<PgReplicator>);

#[tonic::async_trait]
impl crate::proto::pg_data_service_server::PgDataService for TestService {
    async fn public_v1(
        &self,
        request: tonic::Request<crate::proto::RecoveryRequest>,
    ) -> std::result::Result<tonic::Response<crate::proto::RecoveryResponse>, tonic::Status> {
        let request =
            decode(&request.into_inner().envelope_json).map_err(tonic::Status::invalid_argument)?;
        let response = self
            .0
            .public_request(request)
            .await
            .map_err(|e| tonic::Status::failed_precondition(e.to_string()))?;
        Ok(tonic::Response::new(crate::proto::RecoveryResponse {
            envelope_json: encode(&response).unwrap(),
        }))
    }
    async fn build(
        &self,
        _: tonic::Request<crate::proto::NativeBuildRequest>,
    ) -> std::result::Result<tonic::Response<crate::proto::NativeBuildResponse>, tonic::Status>
    {
        Err(tonic::Status::unimplemented("legacy"))
    }
    async fn inspect_source(
        &self,
        _: tonic::Request<crate::proto::NativeBuildRequest>,
    ) -> std::result::Result<tonic::Response<crate::proto::NativeSourceResponse>, tonic::Status>
    {
        Err(tonic::Status::unimplemented("legacy"))
    }
    async fn recover(
        &self,
        _: tonic::Request<crate::proto::RecoveryRequest>,
    ) -> std::result::Result<tonic::Response<crate::proto::RecoveryResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("legacy"))
    }
}

fn build_replica(binding: &BuildBinding) -> ReplicaInformation {
    let target = &binding.target;
    let mut replica = ReplicaInformation::idle_build(
        binding.build_id.clone(),
        target.replica.clone(),
        target.replication_address.clone(),
    );
    replica.process_session_id = target.process_session_id.clone();
    replica
}

fn peer_replica(binding: &PeerBinding) -> ReplicaInformation {
    let target = &binding.secondary;
    ReplicaInformation::active_peer(
        target.replica.clone(),
        target.replication_address.clone(),
        target.process_session_id.clone(),
        0,
        0,
        false,
    )
    .unwrap()
}

async fn frame(stream: &mut tokio::net::TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let kind = stream.read_u8().await?;
    let length = stream.read_u32().await?;
    assert!((4..=65536).contains(&length));
    let mut bytes = vec![0; length as usize - 4];
    stream.read_exact(&mut bytes).await?;
    Ok((kind, bytes))
}

async fn native_socket(
    credential: &NativeCredential,
    name: &str,
) -> std::io::Result<tokio::net::TcpStream> {
    let mut stream = tokio::net::TcpStream::connect((&*credential.host, credential.port)).await?;
    let mut startup = 196608u32.to_be_bytes().to_vec();
    startup.extend(
        format!(
            "user\0{}\0application_name\0{name}\0replication\0true\0\0",
            credential.user
        )
        .as_bytes(),
    );
    stream.write_u32((startup.len() + 4) as u32).await?;
    stream.write_all(&startup).await?;
    if frame(&mut stream).await? != (b'R', 0u32.to_be_bytes().to_vec()) {
        return Err(std::io::Error::other("native gateway refused startup"));
    }
    loop {
        match frame(&mut stream).await?.0 {
            b'Z' => return Ok(stream),
            b'E' => return Err(std::io::Error::other("native gateway rejected startup")),
            _ => {}
        }
    }
}

async fn closed(stream: &mut tokio::net::TcpStream) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut buffer = [0; 1024];
        loop {
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await
    .expect("predecessor native socket survived drainage");
}

async fn identify_system(stream: &mut tokio::net::TcpStream) {
    let query = b"IDENTIFY_SYSTEM\0";
    stream.write_u8(b'Q').await.unwrap();
    stream.write_u32((query.len() + 4) as u32).await.unwrap();
    stream.write_all(query).await.unwrap();
    let mut data = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match frame(stream).await.unwrap().0 {
                b'D' => data = true,
                b'Z' => break,
                b'E' => panic!("authorized native source query failed"),
                _ => {}
            }
        }
    })
    .await
    .expect("authorized native source query timed out");
    assert!(data, "authorized native source returned no identity");
}

async fn stage<T>(name: &'static str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(90), future)
        .await
        .unwrap_or_else(|_| panic!("{name} exceeded its 90-second test bound"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OracleScenario {
    PrimaryLifecycle,
    BuildCancellation,
    DataLossHistory,
    ContainedClose,
}

pub(crate) async fn public_v1_postgres_primary_lifecycle_and_joint_configuration() {
    run_bounded_oracle_scenario(OracleScenario::PrimaryLifecycle).await;
}

pub(crate) async fn public_v1_postgres_build_cancellation_and_retirement() {
    run_bounded_oracle_scenario(OracleScenario::BuildCancellation).await;
}

pub(crate) async fn public_v1_postgres_data_loss_history_and_replay() {
    run_bounded_oracle_scenario(OracleScenario::DataLossHistory).await;
}

pub(crate) async fn public_v1_postgres_contained_close() {
    run_bounded_oracle_scenario(OracleScenario::ContainedClose).await;
}

async fn run_bounded_oracle_scenario(scenario: OracleScenario) {
    tokio::time::timeout(Duration::from_secs(270), run_oracle_scenario(scenario))
        .await
        .expect("PostgreSQL Public V1 scenario exceeded its 270-second test bound");
}

async fn run_oracle_scenario(scenario: OracleScenario) {
    let directory = OracleDataDir::new();
    let bindings = BindingFactory::new();
    let source = Replica::new(directory.path().join("s"), 41).await;
    let target = Replica::new(directory.path().join("t"), 42).await;
    let identity = source.identity.clone();
    let singleton = native_configuration(std::slice::from_ref(&identity), 0, 1);
    assert!(
        source
            .driver
            .public_configuration(singleton.clone().into(), None)
            .await
            .is_err(),
        "configuration before role/epoch was accepted"
    );
    source.singleton(1).await;
    let (sql, sql_connection) = source.driver.instance.connect_application().await.unwrap();
    sql.batch_execute(
        "CREATE TABLE public_oracle (id integer primary key); INSERT INTO public_oracle VALUES (1)",
    )
    .await
    .unwrap();
    let saved = source.persisted();
    let access = source.driver.instance.access_generation();
    for _ in 0..3 {
        let progress = source.driver.current_progress().await.unwrap();
        let observed = source.driver.observer.snapshot().await.unwrap();
        assert!(progress > 0 && progress <= observed.committed_lsn);
        assert!(source.driver.catch_up_capability().await.unwrap() >= 0);
        assert!(
            !PrimaryReplicator::on_data_loss(source.driver.as_ref())
                .await
                .unwrap()
        );
    }
    assert_eq!(
        source.persisted(),
        saved,
        "progress polling mutated lifecycle state"
    );
    assert_eq!(source.driver.instance.access_generation(), access);
    if scenario == OracleScenario::DataLossHistory {
        source
            .driver
            .public_change_role(Epoch::new(0, 1), ReplicaRole::None)
            .await
            .unwrap();
        assert_eq!(
            source.driver.current_progress().await.unwrap(),
            source.driver.durable.snapshot().await.flush_lsn
        );
        source
            .driver
            .durable
            .update(|state| {
                state.timeline_id = Some(state.timeline_id.unwrap() + 1);
                Ok(())
            })
            .await
            .unwrap();
        let saved = source.persisted();
        let error = PrimaryReplicator::on_data_loss(source.driver.as_ref())
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::ReplicaRebuildRequired(failure)
            if matches!(*failure, ReplicaRebuildRequired::LocalProvider(LocalProviderRebuildRequired {
                reason: ReplicaRebuildReason::IncompatibleRetainedHistory, ..
            })))
        );
        assert_eq!(
            saved,
            source.persisted(),
            "data loss callback repaired or persisted state"
        );
        drop(sql);
        let _ = sql_connection.await;
        target.driver.public_close().await.unwrap();
        source.driver.public_close().await.unwrap();
        return;
    }
    target.role(1, ReplicaRole::IdleSecondary).await;
    assert_eq!(target.driver.current_progress().await.unwrap(), -1);
    let build = bindings.build(&source, &target, "idle-build");
    let mut invalid = build_replica(&build);
    invalid.current_progress = 0;
    assert!(source.driver.build_replica(invalid).await.is_err());
    source
        .driver
        .build_replica(build_replica(&build))
        .await
        .unwrap();
    let mut substituted_source_session = build.clone();
    substituted_source_session.source.process_session_id =
        ProcessSessionId::new("substituted-source-session");
    assert!(matches!(
        source
            .driver
            .public_request(Request::Inspect(TransportBinding::Build(
                substituted_source_session
            )))
            .await,
        Err(RuntimeError::AuthorityNotAdmitted)
    ));
    let mut substituted_source_address = build.clone();
    substituted_source_address.source.replication_address = target.address.clone();
    assert!(matches!(
        source
            .driver
            .public_request(Request::Inspect(TransportBinding::Build(
                substituted_source_address
            )))
            .await,
        Err(RuntimeError::AuthorityNotAdmitted)
    ));
    let mut substituted_target_session = build.clone();
    substituted_target_session.target.process_session_id =
        ProcessSessionId::new("substituted-target-session");
    assert!(matches!(
        target
            .driver
            .public_request(Request::Build(substituted_target_session))
            .await,
        Err(RuntimeError::AuthorityNotAdmitted)
    ));
    let mut substituted_target_address = build.clone();
    substituted_target_address.target.replication_address = source.address.clone();
    assert!(matches!(
        target
            .driver
            .public_request(Request::Build(substituted_target_address))
            .await,
        Err(RuntimeError::AuthorityNotAdmitted)
    ));
    let build_credential = source.driver.public_owner().state.lock().await.grants[0]
        .credential
        .clone();
    assert!(target.driver.current_progress().await.unwrap() > 0);
    assert!(target.driver.instance.connect_application().await.is_err());
    assert!(
        native_socket(&build_credential, "unaffiliated_ack")
            .await
            .is_err()
    );
    let mut predecessor = native_socket(&build_credential, &build_credential.name)
        .await
        .unwrap();
    assert!(
        source
            .driver
            .public_owner()
            .state
            .lock()
            .await
            .grants
            .iter()
            .all(|g| matches!(g.binding, TransportBinding::Build(_)))
    );

    if scenario == OracleScenario::ContainedClose {
        source.driver.public_close().await.unwrap();
        closed(&mut predecessor).await;
        assert!(sql.simple_query("SELECT 1").await.is_err());
        drop(sql);
        let _ = sql_connection.await;
        target.driver.public_close().await.unwrap();
        return;
    }

    if scenario == OracleScenario::BuildCancellation {
        // A fresh build ID cannot overwrite data retained by another exact
        // build attempt, even while the target remains idle and outside PC/CC.
        let completed_binding = target
            .driver
            .public_owner()
            .state
            .lock()
            .await
            .built
            .as_ref()
            .unwrap()
            .binding
            .clone();
        let mut conflict = completed_binding.clone();
        conflict.build_id = OperationId::new("conflicting-build");
        target
            .driver
            .public_owner()
            .state
            .lock()
            .await
            .built
            .as_mut()
            .unwrap()
            .binding = conflict;
        let before = target.persisted();
        let error = source
            .driver
            .build_replica(build_replica(&build))
            .await
            .unwrap_err();
        match error {
            RuntimeError::ReplicaRebuildRequired(failure) => match *failure {
                ReplicaRebuildRequired::RemoteBuildTarget(failure) => {
                    assert_eq!(failure.epoch, build.epoch);
                    assert_eq!(failure.target, build.target.replica);
                    assert_eq!(failure.target_session, build.target.process_session_id);
                    assert_eq!(failure.reason, ReplicaRebuildReason::RemoteCopyConflict);
                }
                _ => panic!("remote target failure lost its scope"),
            },
            error => panic!("typed remote error was lost: {error:?}"),
        }
        assert_eq!(target.persisted(), before);
        target
            .driver
            .public_owner()
            .state
            .lock()
            .await
            .built
            .as_mut()
            .unwrap()
            .binding = completed_binding;

        let retained_digest = target
            .driver
            .durable
            .snapshot()
            .await
            .timeline_history_digest
            .unwrap();
        target
            .driver
            .durable
            .update(|state| {
                state.timeline_history_digest = Some("incompatible-retained-history".into());
                Ok(())
            })
            .await
            .unwrap();
        let error = source
            .driver
            .build_replica(build_replica(&build))
            .await
            .unwrap_err();
        match error {
            RuntimeError::ReplicaRebuildRequired(failure) => match *failure {
                ReplicaRebuildRequired::RemoteBuildTarget(failure) => {
                    assert_eq!(
                        failure.reason,
                        ReplicaRebuildReason::IncompatibleRetainedHistory
                    );
                    assert_eq!(failure.epoch, build.epoch);
                    assert_eq!(failure.target, build.target.replica);
                    assert_eq!(failure.target_session, build.target.process_session_id);
                }
                failure => panic!("unexpected rebuild scope: {failure:?}"),
            },
            error => panic!("typed remote rebuild was lost: {error:?}"),
        }
        target
            .driver
            .durable
            .update(|state| {
                state.timeline_history_digest = Some(retained_digest);
                Ok(())
            })
            .await
            .unwrap();

        let cancelled = bindings.build(&source, &target, "cancelled-build");
        let (cancelled_active, cancelled_credential) = {
            let mut state = source.driver.public_owner().state.lock().await;
            let (active, created) = source
                .driver
                .public_grant(&mut state, TransportBinding::Build(cancelled.clone()))
                .await
                .unwrap();
            assert!(created);
            let credential = state
                .grants
                .iter()
                .find(|grant| grant.binding == TransportBinding::Build(cancelled.clone()))
                .unwrap()
                .credential
                .clone();
            (active, credential)
        };
        let mut cancelled_socket = tokio::net::TcpStream::connect((
            &*cancelled_credential.host,
            cancelled_credential.port,
        ))
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        drop(BuildGrantGuard {
            active: cancelled_active,
            armed: true,
        });
        closed(&mut cancelled_socket).await;
        assert!(source.driver.instance.is_running().await);
        identify_system(&mut predecessor).await;
    } else {
        identify_system(&mut predecessor).await;
    }
    source
        .driver
        .public_application_role(ReplicaRole::None)
        .await
        .unwrap();
    assert!(sql.simple_query("SELECT 1").await.is_err());
    drop(sql);
    let _ = sql_connection.await;
    closed(&mut predecessor).await;
    assert!(
        tokio::net::TcpStream::connect((&*build_credential.host, build_credential.port))
            .await
            .is_err()
    );
    if scenario == OracleScenario::BuildCancellation {
        target.driver.public_close().await.unwrap();
        source.driver.public_close().await.unwrap();
        return;
    }
    target.role(1, ReplicaRole::ActiveSecondary).await;
    target
        .driver
        .public_application_role(ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    assert!(target.driver.instance.connect_application().await.is_err());
    let members = [identity.clone(), target.identity.clone()];
    let current = native_configuration(&members, 0, 1);
    let peer = bindings.peer(&source, &target, &current, None);
    stage(
        "peer configuration",
        source.driver.public_configuration(
            ReplicaSetConfiguration {
                configuration: current.clone(),
                replicas: vec![peer_replica(&peer)],
            },
            None,
        ),
    )
    .await
    .unwrap();
    let mut substituted_source_session = peer.clone();
    substituted_source_session.primary.process_session_id =
        ProcessSessionId::new("substituted-peer-source-session");
    assert!(
        target
            .driver
            .public_request(Request::Peer(substituted_source_session))
            .await
            .is_err()
    );
    let mut substituted_source_address = peer.clone();
    substituted_source_address.primary.replication_address = target.address.clone();
    assert!(
        target
            .driver
            .public_request(Request::Peer(substituted_source_address))
            .await
            .is_err()
    );
    let mut substituted_target_session = peer.clone();
    substituted_target_session.secondary.process_session_id =
        ProcessSessionId::new("substituted-peer-target-session");
    assert!(matches!(
        target
            .driver
            .public_request(Request::Peer(substituted_target_session))
            .await,
        Err(RuntimeError::AuthorityNotAdmitted)
    ));
    let mut substituted_target_address = peer.clone();
    substituted_target_address.secondary.replication_address = source.address.clone();
    assert!(matches!(
        target
            .driver
            .public_request(Request::Peer(substituted_target_address))
            .await,
        Err(RuntimeError::AuthorityNotAdmitted)
    ));
    stage(
        "all-replica catch-up",
        PrimaryReplicator::wait_for_catch_up_quorum(
            source.driver.as_ref(),
            ReplicaSetQuorumMode::All,
        ),
    )
    .await
    .unwrap();
    let (writer, writer_connection) = stage("primary owner SQL access", async {
        loop {
            if let Ok(connection) = source.driver.instance.connect_application().await {
                break connection;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    let (reader, reader_connection) = stage("secondary owner SQL access", async {
        loop {
            if let Ok(connection) = target.driver.instance.connect_application().await {
                break connection;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert_eq!(
        reader
            .query_one("SELECT count(*) FROM public_oracle", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    assert!(
        reader
            .simple_query("INSERT INTO public_oracle VALUES (999)")
            .await
            .is_err()
    );
    drop(reader);
    let _ = reader_connection.await;
    let before = source.driver.current_progress().await.unwrap();
    let (control, control_connection) = target.driver.instance.connect().await.unwrap();
    control
        .simple_query("SELECT pg_wal_replay_pause()")
        .await
        .unwrap();
    let write = writer.execute("INSERT INTO public_oracle VALUES (2)", &[]);
    tokio::pin!(write);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut write)
            .await
            .is_err()
    );
    let persisted = source.persisted();
    let (native, native_connection) = source.driver.instance.connect().await.unwrap();
    let native_flush = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let flush = crate::monitor::parse_pg_lsn(
                native
                    .query_one("SELECT pg_current_wal_flush_lsn()::text", &[])
                    .await
                    .unwrap()
                    .get(0),
            )
            .unwrap();
            if flush > before {
                break flush;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .ok();
    let progress = source.driver.current_progress().await.unwrap();
    match native_flush {
        Some(native_flush) => assert!(
            progress >= before && progress < native_flush,
            "primary exported unacknowledged local WAL: before={before}, public={progress}, local={native_flush}"
        ),
        None => assert_eq!(
            progress, before,
            "primary progress advanced before the blocked write reached local flush"
        ),
    }
    drop(native);
    let _ = native_connection.await;
    assert_eq!(persisted, source.persisted());
    control
        .simple_query("SELECT pg_wal_replay_resume()")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), &mut write)
        .await
        .unwrap()
        .unwrap();
    drop(control);
    let _ = control_connection.await;
    let singleton = native_configuration(std::slice::from_ref(&identity), 0, 1);
    let initial_joint = bindings.peer(&source, &target, &current, Some(&singleton));
    stage(
        "initial joint peer admission",
        source.driver.public_configuration(
            ReplicaSetConfiguration {
                configuration: current.clone(),
                replicas: vec![peer_replica(&initial_joint)],
            },
            Some(ReplicaSetConfiguration {
                configuration: singleton.clone(),
                replicas: Vec::new(),
            }),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        target.driver.public_owner().fence.context().unwrap(),
        LifecycleContext {
            identity: target.identity.clone(),
            role: ReplicaRole::ActiveSecondary,
            epoch: Epoch::new(0, 1),
            previous_configuration_id: Some(singleton.configuration_id.clone()),
            current_configuration_id: Some(current.configuration_id.clone()),
        }
    );
    let current_only = bindings.peer(&source, &target, &current, None);
    stage(
        "joint to current-only peer admission",
        source.driver.public_configuration(
            ReplicaSetConfiguration {
                configuration: current.clone(),
                replicas: vec![peer_replica(&current_only)],
            },
            None,
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        target.driver.public_owner().fence.context().unwrap(),
        LifecycleContext {
            identity: target.identity.clone(),
            role: ReplicaRole::ActiveSecondary,
            epoch: Epoch::new(0, 1),
            previous_configuration_id: None,
            current_configuration_id: Some(current.configuration_id.clone()),
        }
    );
    let previous_only = bindings.peer(&source, &target, &singleton, Some(&current));
    stage(
        "previous-only peer configuration",
        source.driver.public_configuration(
            ReplicaSetConfiguration {
                configuration: singleton.clone(),
                replicas: Vec::new(),
            },
            Some(ReplicaSetConfiguration {
                configuration: current.clone(),
                replicas: vec![peer_replica(&previous_only)],
            }),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        target.driver.public_owner().fence.context().unwrap(),
        LifecycleContext {
            identity: target.identity.clone(),
            role: ReplicaRole::ActiveSecondary,
            epoch: Epoch::new(0, 1),
            previous_configuration_id: Some(current.configuration_id.clone()),
            current_configuration_id: Some(singleton.configuration_id.clone()),
        }
    );
    stage(
        "previous-only all-replica catch-up",
        PrimaryReplicator::wait_for_catch_up_quorum(
            source.driver.as_ref(),
            ReplicaSetQuorumMode::All,
        ),
    )
    .await
    .unwrap();
    let peer_credential = source.driver.public_owner().state.lock().await.grants[0]
        .credential
        .clone();
    let mut old_peer = native_socket(&peer_credential, &peer_credential.name)
        .await
        .unwrap();
    let mut restricted = native_socket(&peer_credential, &peer_credential.name)
        .await
        .unwrap();
    let query = b"SET application_name = 'unaffiliated'\0";
    restricted.write_u8(b'Q').await.unwrap();
    restricted
        .write_u32((query.len() + 4) as u32)
        .await
        .unwrap();
    restricted.write_all(query).await.unwrap();
    closed(&mut restricted).await;

    // Withdrawal joins predecessor SQL and physical sockets before a renewed
    // epoch/session can install new gateways or synchronous eligibility.
    source
        .driver
        .public_application_role(ReplicaRole::None)
        .await
        .unwrap();
    target
        .driver
        .public_application_role(ReplicaRole::None)
        .await
        .unwrap();
    closed(&mut old_peer).await;
    assert!(writer.simple_query("SELECT 1").await.is_err());
    source.change_role(2, ReplicaRole::Primary).await;
    target.change_role(2, ReplicaRole::ActiveSecondary).await;
    let current = native_configuration(&members, 0, 2);
    assert!(
        target
            .driver
            .public_request(Request::Build(build))
            .await
            .is_err()
    );
    let renewed = bindings.peer(&source, &target, &current, None);
    assert!(
        source
            .driver
            .public_configuration(
                ReplicaSetConfiguration {
                    configuration: current.clone(),
                    replicas: vec![peer_replica(&renewed)],
                },
                None,
            )
            .await
            .is_err(),
        "renewed configuration was accepted before standalone epoch barriers"
    );
    source.epoch(2).await;
    assert!(
        source
            .driver
            .public_configuration(
                ReplicaSetConfiguration {
                    configuration: current.clone(),
                    replicas: vec![peer_replica(&renewed)],
                },
                None,
            )
            .await
            .is_err(),
        "renewed configuration was accepted before the secondary epoch barrier"
    );
    target.epoch(2).await;
    target
        .driver
        .public_application_role(ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    source
        .driver
        .public_configuration(
            ReplicaSetConfiguration {
                configuration: current,
                replicas: vec![peer_replica(&renewed)],
            },
            None,
        )
        .await
        .unwrap();
    let (fresh, fresh_connection) = stage("renewed primary owner SQL access", async {
        loop {
            if let Ok(connection) = source.driver.instance.connect_application().await {
                break connection;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    fresh
        .execute("INSERT INTO public_oracle VALUES (3)", &[])
        .await
        .unwrap();
    let next_credential = source.driver.public_owner().state.lock().await.grants[0]
        .credential
        .clone();
    assert_ne!(peer_credential.name, next_credential.name);
    // Native ownership advances without any progress/report poll.
    source
        .driver
        .public_owner()
        .fence
        .update(|context| context.role = ReplicaRole::None)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while source.driver.instance.is_running().await {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert!(fresh.simple_query("SELECT 1").await.is_err());
    drop(fresh);
    let _ = fresh_connection.await;
    target.driver.public_close().await.unwrap();
    source.driver.public_close().await.unwrap();
    drop(writer_connection);
}
