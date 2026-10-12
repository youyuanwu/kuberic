use super::*;
use crate::durable::{PgDurableIdentity, StorageMode};
use crate::testing::{allocate_port, find_pg_bin, native_configuration, native_identity};
use kuberic_runtime::protocol::authorization::{
    AuthorizationSignature, AuthorizationVerificationKey, BuildAuthorizationClaimsV1,
    BuildAuthorizationEndpointV1, PeerSessionAuthorizationClaimsV1, PeerSessionEndpointV1,
};
use kuberic_runtime::protocol::types::{
    HistoryContext, HistoryId, PodUid, PublicProtocolGeneration, PvcUid,
    ReplicaStorageIncarnationContext, ResourceUid, StorageGeneration,
};
use kuberic_runtime::replicator::PublicAuthorizationTrust;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// The public host intentionally does not export a lifecycle-admission mutator.
// This test-only owner models callback-boundary advancement; production always
// delegates verification to the opaque, creation-bound runtime handle.
pub(super) struct FixtureAdmission {
    current: std::sync::Mutex<Option<PublicAuthorizationContext>>,
    trust: PublicAuthorizationTrust,
    address: String,
}

impl FixtureAdmission {
    pub(super) fn context(&self) -> Result<PublicAuthorizationContext> {
        self.current
            .lock()
            .unwrap()
            .clone()
            .ok_or(RuntimeError::AuthorityNotAdmitted)
    }

    pub(super) fn verify(
        &self,
        authorization: &Authorization,
    ) -> Result<PublicAuthorizationContext> {
        let context = self.context()?;
        let (history, epoch, resource, generation, key, identity, session, address, role) =
            match authorization {
                Authorization::Build(build) => {
                    self.trust
                        .verification_key
                        .verify_build_authorization(build)
                        .map_err(application_error)?;
                    let c = &build.claims;
                    let local = if c.source.storage_incarnation.replica == context.identity {
                        &c.source
                    } else {
                        &c.target
                    };
                    if local.storage_incarnation != context.storage_incarnation {
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    }
                    (
                        &c.history,
                        c.epoch,
                        &c.resource_uid,
                        c.protocol_generation,
                        c.key_id,
                        &local.storage_incarnation.replica,
                        &local.process_session_id,
                        &local.replication_address,
                        local.role,
                    )
                }
                Authorization::Peer(peer) => {
                    self.trust
                        .verification_key
                        .verify_peer_session_authorization(peer)
                        .map_err(application_error)?;
                    let c = &peer.claims;
                    let local = if c.primary.replica == context.identity {
                        &c.primary
                    } else {
                        &c.secondary
                    };
                    if context.current_configuration_id.as_ref()
                        != Some(&c.current_configuration_id)
                        || context.previous_configuration_id != c.previous_configuration_id
                    {
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    }
                    (
                        &c.history,
                        c.epoch,
                        &c.resource_uid,
                        c.protocol_generation,
                        c.key_id,
                        &local.replica,
                        &local.process_session_id,
                        &local.replication_address,
                        local.role,
                    )
                }
            };
        if history != &context.history
            || epoch != context.epoch
            || resource != &self.trust.resource_uid
            || generation != self.trust.protocol_generation
            || key != self.trust.key_id
            || identity != &context.identity
            || session != &context.process_session_id
            || address != &self.address
            || role != context.role
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(context)
    }

    fn advance(&self, change: impl FnOnce(&mut PublicAuthorizationContext)) {
        change(self.current.lock().unwrap().as_mut().unwrap());
    }
}

struct Signer {
    root: std::path::PathBuf,
    key: AuthorizationVerificationKey,
}

impl Signer {
    fn new(root: &std::path::Path) -> Self {
        let mut der = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        der.extend([7; 32]);
        std::fs::write(root.join("fixture-key.der"), der).unwrap();
        let output = std::process::Command::new("openssl")
            .args(["pkey", "-inform", "DER", "-in"])
            .arg(root.join("fixture-key.der"))
            .args(["-pubout", "-outform", "DER"])
            .output()
            .unwrap();
        assert!(output.status.success(), "fixture signing tool failed");
        Self {
            root: root.to_owned(),
            key: AuthorizationVerificationKey::from_bytes(output.stdout[12..].try_into().unwrap())
                .unwrap(),
        }
    }

    fn sign(&self, bytes: Vec<u8>) -> AuthorizationSignature {
        std::fs::write(self.root.join("fixture-claims"), bytes).unwrap();
        let output = std::process::Command::new("openssl")
            .args(["pkeyutl", "-sign", "-rawin", "-keyform", "DER", "-inkey"])
            .arg(self.root.join("fixture-key.der"))
            .arg("-in")
            .arg(self.root.join("fixture-claims"))
            .output()
            .unwrap();
        assert!(output.status.success(), "fixture signing failed");
        AuthorizationSignature::from_bytes(output.stdout.try_into().unwrap())
    }

    fn build(&self, source: &Replica, target: &Replica, id: &str) -> BuildAuthorizationV1 {
        let s = source.admission.context().unwrap();
        let t = target.admission.context().unwrap();
        let claims = BuildAuthorizationClaimsV1 {
            resource_uid: s.history.resource_uid.clone(),
            protocol_generation: s.history.protocol_generation,
            key_id: self.key.key_id(),
            history: s.history.clone(),
            epoch: s.epoch,
            operation_id: OperationId::new(format!("operation-{id}")),
            build_id: OperationId::new(id),
            source: BuildAuthorizationEndpointV1 {
                storage_incarnation: s.storage_incarnation,
                process_session_id: s.process_session_id,
                replication_address: source.admission.address.clone(),
                role: ReplicaRole::Primary,
            },
            target: BuildAuthorizationEndpointV1 {
                storage_incarnation: t.storage_incarnation,
                process_session_id: t.process_session_id,
                replication_address: target.admission.address.clone(),
                role: ReplicaRole::IdleSecondary,
            },
        };
        let signature = self.sign(claims.signing_bytes().unwrap());
        BuildAuthorizationV1::from_signature(claims, signature).unwrap()
    }

    fn peer(
        &self,
        source: &Replica,
        target: &Replica,
        revision: u64,
    ) -> PeerSessionAuthorizationV1 {
        let s = source.admission.context().unwrap();
        let t = target.admission.context().unwrap();
        let claims = PeerSessionAuthorizationClaimsV1 {
            resource_uid: s.history.resource_uid.clone(),
            protocol_generation: s.history.protocol_generation,
            key_id: self.key.key_id(),
            history: s.history.clone(),
            epoch: s.epoch,
            current_configuration_id: s.current_configuration_id.unwrap(),
            previous_configuration_id: s.previous_configuration_id,
            primary: PeerSessionEndpointV1 {
                replica: s.identity,
                process_session_id: s.process_session_id,
                replication_address: source.admission.address.clone(),
                role: ReplicaRole::Primary,
            },
            secondary: PeerSessionEndpointV1 {
                replica: t.identity,
                process_session_id: t.process_session_id,
                replication_address: target.admission.address.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
            configuration_revision: revision,
        };
        let signature = self.sign(claims.signing_bytes().unwrap());
        PeerSessionAuthorizationV1::from_signature(claims, signature).unwrap()
    }
}

struct Replica {
    driver: Arc<PgReplicator>,
    admission: Arc<FixtureAdmission>,
    server: tokio::task::JoinHandle<()>,
    root: std::path::PathBuf,
}

struct OracleDataDir {
    path: std::path::PathBuf,
}

impl OracleDataDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("kuberic-public-v1-postgres-{}", std::process::id()));
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
    async fn new(root: std::path::PathBuf, id: i64, signer: &Signer) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let identity = native_identity(id, &format!("public-{id}"));
        let resource = ResourceUid::new("postgres-public-oracle");
        let trust = PublicAuthorizationTrust {
            resource_uid: resource.clone(),
            protocol_generation: PublicProtocolGeneration::new(11),
            verification_key: signer.key,
            key_id: signer.key.key_id(),
        };
        let admission = Arc::new(FixtureAdmission {
            trust,
            address: address.clone(),
            current: std::sync::Mutex::new(Some(PublicAuthorizationContext {
                identity: identity.clone(),
                history: HistoryContext {
                    resource_uid: resource.clone(),
                    protocol_generation: PublicProtocolGeneration::new(11),
                    history_id: HistoryId::new("postgres-public-history"),
                    data_loss_number: 0,
                },
                storage_incarnation: ReplicaStorageIncarnationContext {
                    replica: identity.clone(),
                    storage: ReplicaStorageIdentity::Persistent {
                        pod_uid: PodUid::new(format!("pod-{id}")),
                        pvc_uid: PvcUid::new(format!("pvc-{id}")),
                    },
                    storage_generation: StorageGeneration::new(format!("storage-{id}")),
                },
                process_session_id: ProcessSessionId::new(format!("session-{id}")),
                role: ReplicaRole::None,
                epoch: Epoch::default(),
                previous_configuration_id: None,
                current_configuration_id: None,
            })),
        });
        let durable = Arc::new(
            PgDurableStore::open(
                root.join("a"),
                PgDurableIdentity {
                    resource_uid: resource,
                    replica: identity,
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
                    local_endpoint: address,
                    bearer_token: "public-oracle".into(),
                },
                true,
            )
            .await,
        );
        assert!(
            driver
                .public_v1
                .set(PublicPg::new(Admission::Fixture(admission.clone())))
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
            admission,
            server,
            root,
        }
    }

    async fn change_role(&self, epoch: i64, role: ReplicaRole) {
        self.driver
            .public_change_role(Epoch::new(0, epoch), role)
            .await
            .unwrap();
        self.admission.advance(|c| {
            c.role = role;
            c.epoch = Epoch::new(0, epoch);
        });
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

    fn configuration(&self, configuration: &ConfigurationDescriptor) {
        self.admission.advance(|c| {
            c.current_configuration_id = Some(configuration.configuration_id.clone());
            c.previous_configuration_id = None;
        });
    }

    fn configuration_pair(
        &self,
        current: &ConfigurationDescriptor,
        previous: &ConfigurationDescriptor,
    ) {
        self.admission.advance(|context| {
            context.current_configuration_id = Some(current.configuration_id.clone());
            context.previous_configuration_id = Some(previous.configuration_id.clone());
        });
    }

    async fn singleton(&self, epoch: i64) {
        self.change_role(epoch, ReplicaRole::Primary).await;
        self.driver
            .public_application_role(ReplicaRole::Primary)
            .await
            .unwrap();
        assert!(self.driver.instance.connect_application().await.is_err());
        let identity = self.admission.context().unwrap().identity;
        let config = native_configuration(&[identity], 0, epoch);
        self.configuration(&config);
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
        if request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            != Some("Bearer public-oracle")
        {
            return Err(tonic::Status::unauthenticated("fixture credential"));
        }
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

fn build_replica(authorization: &BuildAuthorizationV1) -> ReplicaInformation {
    let target = &authorization.claims.target;
    let mut replica = ReplicaInformation::idle_build(
        authorization.claims.build_id.clone(),
        target.storage_incarnation.replica.clone(),
        target.replication_address.clone(),
    );
    replica.process_session_id = target.process_session_id.clone();
    replica.storage_incarnation = Some(target.storage_incarnation.clone());
    replica.build_authorization = Some(authorization.clone());
    replica
}

fn peer_replica(authorization: &PeerSessionAuthorizationV1) -> ReplicaInformation {
    let target = &authorization.claims.secondary;
    let mut replica = ReplicaInformation::active_peer(
        target.replica.clone(),
        target.replication_address.clone(),
        target.process_session_id.clone(),
        0,
        0,
        false,
    )
    .unwrap();
    replica.peer_authorization = Some(authorization.clone());
    replica.configuration_revision = Some(authorization.claims.configuration_revision);
    replica
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
            credential.name
        )
        .as_bytes(),
    );
    stream.write_u32((startup.len() + 4) as u32).await?;
    stream.write_all(&startup).await?;
    if frame(&mut stream).await? != (b'R', 3u32.to_be_bytes().to_vec()) {
        return Err(std::io::Error::other("native gateway refused challenge"));
    }
    stream.write_u8(b'p').await?;
    stream
        .write_u32((credential.secret.len() + 5) as u32)
        .await?;
    stream
        .write_all(format!("{}\0", credential.secret).as_bytes())
        .await?;
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

pub(crate) async fn oracle() {
    let directory = OracleDataDir::new();
    let signer = Signer::new(directory.path());
    let source = Replica::new(directory.path().join("s"), 41, &signer).await;
    let target = Replica::new(directory.path().join("t"), 42, &signer).await;
    let identity = source.admission.context().unwrap().identity;
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
    target.role(1, ReplicaRole::IdleSecondary).await;
    assert_eq!(target.driver.current_progress().await.unwrap(), -1);
    let build = signer.build(&source, &target, "signed-idle");
    let mut invalid = build_replica(&build);
    invalid.current_progress = 0;
    assert!(source.driver.build_replica(invalid).await.is_err());
    let mut forged = build_replica(&build);
    forged
        .build_authorization
        .as_mut()
        .unwrap()
        .claims
        .target
        .process_session_id = ProcessSessionId::new("forged");
    assert!(source.driver.build_replica(forged).await.is_err());
    source
        .driver
        .build_replica(build_replica(&build))
        .await
        .unwrap();
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
            .all(|g| matches!(g.authorization, Authorization::Build(_)))
    );

    // A fresh signed build ID cannot overwrite data retained by another exact
    // build attempt, even while the target remains idle and outside PC/CC.
    let conflict = signer.build(&source, &target, "signed-conflict");
    target
        .driver
        .public_owner()
        .state
        .lock()
        .await
        .built
        .as_mut()
        .unwrap()
        .authorization = conflict;
    let before = target.persisted();
    let error = source
        .driver
        .build_replica(build_replica(&build))
        .await
        .unwrap_err();
    match error {
        RuntimeError::ReplicaRebuildRequired(failure) => match *failure {
            ReplicaRebuildRequired::RemoteBuildTarget(failure) => {
                assert_eq!(failure.authorization, build_reference(&build).unwrap());
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
        .authorization = build.clone();

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
    assert!(matches!(
        error,
        RuntimeError::ReplicaRebuildRequired(failure)
            if matches!(&*failure, ReplicaRebuildRequired::RemoteBuildTarget(failure)
                if failure.reason == ReplicaRebuildReason::IncompatibleRetainedHistory
                    && failure.authorization == build_reference(&build).unwrap())
    ));
    target
        .driver
        .durable
        .update(|state| {
            state.timeline_history_digest = Some(retained_digest);
            Ok(())
        })
        .await
        .unwrap();

    let cancelled = signer.build(&source, &target, "cancelled-source-attempt");
    let (cancelled_active, cancelled_credential) = {
        let mut state = source.driver.public_owner().state.lock().await;
        let (active, created) = source
            .driver
            .public_grant(&mut state, Authorization::Build(cancelled.clone()))
            .await
            .unwrap();
        assert!(created);
        let credential = state
            .grants
            .iter()
            .find(|grant| grant.authorization == Authorization::Build(cancelled.clone()))
            .unwrap()
            .credential
            .clone();
        (active, credential)
    };
    let mut cancelled_socket =
        tokio::net::TcpStream::connect((&*cancelled_credential.host, cancelled_credential.port))
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
    target.role(1, ReplicaRole::ActiveSecondary).await;
    target
        .driver
        .public_application_role(ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    assert!(target.driver.instance.connect_application().await.is_err());
    let members = [
        identity.clone(),
        target.admission.context().unwrap().identity,
    ];
    let current = native_configuration(&members, 0, 1);
    source.configuration(&current);
    target.configuration(&current);
    let peer = signer.peer(&source, &target, 1);
    stage(
        "signed peer configuration",
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
    source.configuration_pair(&singleton, &current);
    target.configuration_pair(&singleton, &current);
    let previous_only = signer.peer(&source, &target, 2);
    stage(
        "previous-only signed peer configuration",
        source.driver.public_configuration(
            ReplicaSetConfiguration {
                configuration: singleton,
                replicas: Vec::new(),
            },
            Some(ReplicaSetConfiguration {
                configuration: current,
                replicas: vec![peer_replica(&previous_only)],
            }),
        ),
    )
    .await
    .unwrap();
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
    for replica in [&source, &target] {
        replica.admission.advance(|c| {
            c.process_session_id =
                ProcessSessionId::new(format!("renewed-{}", c.identity.replica_id))
        });
    }
    let current = native_configuration(&members, 0, 2);
    source.configuration(&current);
    target.configuration(&current);
    assert!(
        source
            .driver
            .public_owner()
            .admission
            .verify(&Authorization::Peer(peer))
            .is_err()
    );
    assert!(
        target
            .driver
            .public_request(Request::Build(build))
            .await
            .is_err()
    );
    let renewed = signer.peer(&source, &target, 2);
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
    let mut bad_revision = peer_replica(&renewed);
    bad_revision.configuration_revision = Some(1);
    assert!(
        source
            .driver
            .public_configuration(
                ReplicaSetConfiguration {
                    configuration: current.clone(),
                    replicas: vec![bad_revision],
                },
                None
            )
            .await
            .is_err()
    );
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
    assert_ne!(peer_credential.secret, next_credential.secret);
    // Native ownership advances without any progress/report poll.
    source
        .admission
        .advance(|c| c.process_session_id = ProcessSessionId::new("withdrawn-session"));
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

    source
        .driver
        .public_change_role(Epoch::new(0, 2), ReplicaRole::None)
        .await
        .unwrap();
    source.admission.advance(|c| c.role = ReplicaRole::None);
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
    source.driver.public_close().await.unwrap();
    drop(writer_connection);
}
