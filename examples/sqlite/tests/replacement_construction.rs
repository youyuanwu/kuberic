use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kuberic_runtime::RuntimeError;
use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
    RunningReplica,
};
use kuberic_runtime::protocol::public_operations::*;
use kuberic_runtime::protocol::types::*;
use kuberic_runtime::testing::sqlite_store::SqliteStore;
use kuberic_runtime::testing::state::{SCHEMA_VERSION, StorageIdentity};
use sqlite_replicated::SqlitePersistence;
use sqlite_replicated::proto;
use sqlite_replicated::proto::sqlite_store_server::SqliteStore as _;
use sqlite_replicated::server::SqliteServer;
use sqlite_replicated::service::SqliteService;

struct Resolver;

impl ReplicaEndpointResolver for Resolver {
    fn control_endpoint(&self, _: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
    fn replication_endpoint(&self, _: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
}

struct Node {
    running: RunningReplica,
    service: Arc<SqliteService>,
    identity: ReplicaIdentity,
    storage: ReplicaStorageIncarnationContext,
    history: HistoryContext,
    preview: PublicV1Identity,
    revision: u64,
}

impl Node {
    async fn start(root: &Path, id: i64, revision: u64) -> Self {
        let resource = ResourceUid::new("sqlite-native-construction");
        let preview = PublicV1Identity::new(1);
        let name = root.file_name().unwrap().to_str().unwrap();
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(id),
            instance_id: ReplicaInstanceId::new(format!("{name}-pod")),
            agent_generation: AgentGeneration::new(format!("{name}-agent")),
        };
        let pod = PodUid::new(format!("{name}-pod"));
        let pvc = PvcUid::new(format!("{name}-pvc"));
        let application = root.join("application");
        let fresh = SqlitePersistence::is_fresh_empty(&application).unwrap();
        let paths = std::collections::BTreeMap::from([("application".into(), application.clone())]);
        if !SqliteStore::metadata_database_path(root).exists() {
            drop(
                SqliteStore::create_public_v1_with_application_paths(
                    SqliteStore::metadata_database_path(root),
                    preview.clone(),
                    StorageIdentity {
                        schema_version: SCHEMA_VERSION,
                        resource_uid: resource.clone(),
                        pod_uid: pod.clone(),
                        pvc_uid: pvc.clone(),
                        initialization_id: InitializationId::new(format!("{name}-initialization")),
                        local_identity: identity.clone(),
                        effective_policy: EffectivePolicy::fixed(2, 30).unwrap(),
                    },
                    paths.clone(),
                )
                .unwrap(),
            );
        }
        let service =
            Arc::new(SqliteService::deferred(application, "http://127.0.0.1:0".into()).unwrap());
        let running = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource.clone(),
                replica_id: identity.replica_id,
                pod_uid: pod,
                pvc_uid: pvc,
                data_root: root.to_path_buf(),
                replication_address: "127.0.0.1:0".parse().unwrap(),
                bearer_token: "sqlite-construction-token".into(),
                rpc_deadline: Duration::from_secs(5),
                transport_window_capacity: 8,
            },
            service.clone(),
            if fresh {
                ApplicationStorageState::FreshEmpty
            } else {
                ApplicationStorageState::Established
            },
            Arc::new(Resolver),
        )
        .with_application_storage_paths(paths)
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let generation = PublicProtocolGeneration::new(1);
        let initialization = derive_history_initialization_id(&resource, generation);
        let history = derive_bootstrap_history(&resource, generation, &initialization);
        let storage = ReplicaStorageIncarnationContext {
            replica: identity.clone(),
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new(format!("{name}-pod")),
                pvc_uid: PvcUid::new(format!("{name}-pvc")),
            },
            storage_generation: StorageGeneration::new(format!("{name}-storage")),
        };
        if revision == 0 {
            running
                .handle()
                .initialize_public_v1_history(&initialization, &history, &storage)
                .await
                .unwrap();
        }
        Self {
            running,
            service,
            identity,
            storage,
            history,
            preview,
            revision,
        }
    }

    async fn command(&mut self, program: PublicOperationProgram) {
        let label = format!("{program:?}");
        if !matches!(
            program,
            PublicOperationProgram::Build(_) | PublicOperationProgram::Remove(_)
        ) {
            self.revision += 1;
        }
        let handle = self.running.handle();
        let class = match &program {
            PublicOperationProgram::Build(input) => PublicOperationClass::Build {
                target: input.replica.replica_id,
            },
            PublicOperationProgram::Remove(input) => PublicOperationClass::Remove {
                target: input.replica.replica_id,
            },
            PublicOperationProgram::Close => PublicOperationClass::Close,
            PublicOperationProgram::Abort => PublicOperationClass::Abort,
            _ => PublicOperationClass::Authority,
        };
        let operation_id = match &program {
            PublicOperationProgram::Build(input) => input.attempt.clone(),
            PublicOperationProgram::Remove(input) => {
                OperationId::new(format!("remove-{}", input.attempt))
            }
            _ => OperationId::new(format!(
                "{}-{}-{}",
                self.identity.instance_id,
                handle.process_session_id(),
                self.revision
            )),
        };
        tokio::time::timeout(
            Duration::from_secs(20),
            handle.execute_public_v1(PublicOperationIntent {
                identity: self.preview.clone(),
                operation_id,
                revision: self.revision,
                process_session_id: handle.process_session_id(),
                class,
                input_digest: program.digest(),
                replay_disposition: program.replay_disposition(),
                lifecycle: None,
                program: Some(program),
            }),
        )
        .await
        .unwrap()
        .unwrap_or_else(|error| panic!("{}: {label}: {error}", self.identity.instance_id));
    }

    async fn open(&mut self, role: ReplicaRole, epoch: Epoch) {
        let existing = self
            .running
            .handle()
            .public_v1_report()
            .await
            .unwrap()
            .open_mode
            == kuberic_runtime::application::OpenMode::Existing;
        self.command(PublicOperationProgram::Open {
            replica: self.identity.clone(),
            existing,
        })
        .await;
        self.command(PublicOperationProgram::Role { epoch, role })
            .await;
        if role == ReplicaRole::Primary {
            self.command(PublicOperationProgram::Epoch { epoch }).await;
        }
    }

    async fn descriptor(&self, role: ReplicaRole) -> PublicReplicaDescriptor {
        let report = self.running.handle().public_v1_report().await.unwrap();
        PublicReplicaDescriptor {
            is_up: true,
            is_ready: true,
            build_id: OperationId::default(),
            identity: self.identity.clone(),
            replication_address: report.replication_address.unwrap(),
            process_session_id: report.process_session_id,
            storage_incarnation: Some(self.storage.clone()),
            role,
            current_progress: report.progress.current().unwrap(),
            catch_up_capability: report.progress.capability().unwrap(),
            must_catchup: false,
        }
    }

    async fn single(&mut self, epoch: Epoch) -> ConfigurationDescriptor {
        let current = ConfigurationDescriptor::new(
            epoch,
            self.identity.replica_id,
            vec![ConfigurationMember {
                identity: self.identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        self.command(PublicOperationProgram::Configuration(PublicConfiguration {
            current: current.clone(),
            previous: None,
            replicas: Vec::new(),
        }))
        .await;
        current
    }

    async fn copy(
        &mut self,
        target: &mut Self,
        epoch: Epoch,
        current: &ConfigurationDescriptor,
    ) -> PublicBuildInput {
        let source_session = self.running.handle().process_session_id();
        let source_address = self
            .running
            .handle()
            .public_v1_report()
            .await
            .unwrap()
            .replication_address
            .unwrap();
        let report = target.running.handle().public_v1_report().await.unwrap();
        let attempt = OperationId::new(format!(
            "copy-{}-{}",
            target.identity.instance_id,
            self.revision + 1
        ));
        let build = PublicBuildInput {
            attempt: attempt.clone(),
            epoch,
            source_address: source_address.clone(),
            current_configuration_id: Some(current.configuration_id.clone()),
            replica: target.identity.clone(),
            target_storage: target.storage.clone(),
            process_session_id: report.process_session_id.clone(),
            replication_address: report.replication_address.clone().unwrap(),
            history: Some(self.history.clone()),
        };
        let revision = self.revision;
        target
            .running
            .handle()
            .install_public_v1_inbound_build_authority(&PublicInboundBuildAuthority {
                operation_id: attempt.clone(),
                operation_revision: revision,
                attempt: PublicBuildAttemptReference {
                    resource_uid: self.history.resource_uid.clone(),
                    epoch,
                    build_id: attempt,
                    source: self.identity.clone(),
                    source_storage: self.storage.clone(),
                    source_session,
                    target: target.identity.clone(),
                    target_storage: target.storage.clone(),
                    target_session: report.process_session_id,
                },
                history: self.history.clone(),
                current_configuration_id: current.configuration_id.clone(),
                source_address,
                target_address: report.replication_address.unwrap(),
            })
            .await
            .unwrap();
        self.command(PublicOperationProgram::Build(build.clone()))
            .await;
        target
            .command(PublicOperationProgram::Role {
                epoch,
                role: ReplicaRole::ActiveSecondary,
            })
            .await;
        target
            .command(PublicOperationProgram::Epoch { epoch })
            .await;
        build
    }

    async fn pair(&mut self, target: &mut Self, epoch: Epoch) -> ConfigurationDescriptor {
        let current = ConfigurationDescriptor::new(
            epoch,
            self.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: self.identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: target.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        target
            .command(PublicOperationProgram::Configuration(PublicConfiguration {
                current: current.clone(),
                previous: None,
                replicas: vec![self.descriptor(ReplicaRole::Primary).await],
            }))
            .await;
        self.command(PublicOperationProgram::Configuration(PublicConfiguration {
            current: current.clone(),
            previous: None,
            replicas: vec![target.descriptor(ReplicaRole::ActiveSecondary).await],
        }))
        .await;
        current
    }

    async fn close(mut self) {
        self.command(PublicOperationProgram::Close).await;
        self.running.shutdown();
        self.running.wait().await.unwrap();
        self.service.shutdown_and_drain().await.unwrap();
    }

    async fn stop(mut self) -> (u64, RuntimeError) {
        self.running.shutdown();
        self.running.wait().await.unwrap();
        let diagnostic = self.service.shutdown_and_drain().await.unwrap_err();
        (self.revision, diagnostic)
    }
}

fn root() -> tempfile::TempDir {
    std::fs::create_dir_all(std::env::current_dir().unwrap().join("target")).unwrap();
    tempfile::Builder::new()
        .prefix("sqlite-native-")
        .tempdir_in(std::env::current_dir().unwrap().join("target"))
        .unwrap()
}

async fn write(server: &SqliteServer, sql: &str) -> Result<i64, tonic::Status> {
    server
        .execute(tonic::Request::new(proto::ExecuteRequest {
            sql: sql.into(),
            params: Vec::new(),
        }))
        .await
        .map(|response| response.into_inner().lsn)
}

async fn values(server: &SqliteServer) -> Result<Vec<String>, tonic::Status> {
    let response = server
        .query(tonic::Request::new(proto::QueryRequest {
            sql: "SELECT value FROM data ORDER BY id".into(),
            params: Vec::new(),
        }))
        .await?
        .into_inner();
    Ok(response
        .rows
        .into_iter()
        .map(|row| match row.values[0].kind.as_ref().unwrap() {
            proto::value::Kind::TextValue(value) => value.clone(),
            other => panic!("unexpected SQL result: {other:?}"),
        })
        .collect())
}

async fn baseline(source: &Node) -> i64 {
    let server = SqliteServer::new(source.service.clone());
    write(
        &server,
        "CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
    )
    .await
    .unwrap();
    write(&server, "INSERT INTO data VALUES(1,'acknowledged')")
        .await
        .unwrap()
}

#[tokio::test]
async fn sqlite_native_copy_waits_for_service_commit_and_copy_eof_before_activation() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let root = root();
        let epoch = Epoch::new(0, 1);
        let mut source = Node::start(&root.path().join("source"), 1, 0).await;
        let mut target = Node::start(&root.path().join("target"), 2, 0).await;
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source.copy(&mut target, epoch, &single).await;
        let current = source.pair(&mut target, epoch).await;
        let baseline = baseline(&source).await;
        let server = SqliteServer::new(source.service.clone());
        let committer = source.service.committer_for_test().unwrap();
        committer.before_commit().arm();
        let client = server.clone();
        let write =
            tokio::spawn(
                async move { write(&client, "INSERT INTO data VALUES(2,'isolated')").await },
            );
        committer.before_commit().wait_entered().await;
        assert!(!write.is_finished());
        assert!(values(&server).await.is_err());
        assert_eq!(
            source
                .service
                .persistence()
                .progress()
                .unwrap()
                .committed_lsn,
            baseline
        );
        let report = source.running.handle().public_v1_report().await.unwrap();
        assert_eq!(report.read_status, AccessStatus::Granted);
        assert!(report.progress.current().unwrap() > baseline);
        let mut replacement = Node::start(&root.path().join("replacement"), 3, 0).await;
        replacement.open(ReplicaRole::IdleSecondary, epoch).await;
        source.service.copy_snapshot_gate().arm();
        let service = source.service.clone();
        let committed = {
            let copy = source.copy(&mut replacement, epoch, &current);
            tokio::pin!(copy);
            assert!(
                tokio::time::timeout(Duration::from_millis(75), &mut copy)
                    .await
                    .is_err(),
                "copy cannot snapshot ahead of primary service-local commitment"
            );
            committer.before_commit().release();
            let committed = write.await.unwrap().unwrap();
            assert!(committed > baseline);
            assert!(
                tokio::time::timeout(Duration::from_millis(75), &mut copy)
                    .await
                    .is_err(),
                "copy must await the real snapshot stream and EOF"
            );
            service.copy_snapshot_gate().wait_entered().await;
            service.copy_snapshot_gate().release();
            copy.await;
            committed
        };
        assert_eq!(
            replacement
                .service
                .persistence()
                .progress()
                .unwrap()
                .committed_lsn,
            committed
        );
        assert_eq!(
            replacement
                .service
                .persistence()
                .snapshot(committed)
                .unwrap(),
            source.service.persistence().snapshot(committed).unwrap()
        );
        assert!(
            values(&SqliteServer::new(replacement.service.clone()))
                .await
                .is_err(),
            "stable native secondary reads remain NotPrimary"
        );
        assert_eq!(values(&server).await.unwrap(), ["acknowledged", "isolated"]);
        source.close().await;
        target.close().await;
        replacement.close().await;
    })
    .await
    .expect("bounded native SQLite copy/local commitment journey");
}

#[tokio::test]
async fn sqlite_fresh_restart_build_and_corrective_empty_replacement() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = root();
        let source_root = root.path().join("source");
        let retired_root = root.path().join("retired");
        let mut source = Node::start(&source_root, 1, 0).await;
        let mut retired = Node::start(&retired_root, 2, 0).await;
        let epoch = Epoch::new(0, 1);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        retired.open(ReplicaRole::IdleSecondary, epoch).await;
        source.copy(&mut retired, epoch, &single).await;
        source.pair(&mut retired, epoch).await;
        let acknowledged = baseline(&source).await;
        let old_service = retired.service.clone();
        let old_session = retired.running.handle().process_session_id();
        let (retired_revision, diagnostic) = retired.stop().await;
        assert!(
            matches!(diagnostic, RuntimeError::OperationCancelled),
            "{diagnostic}"
        );
        let (source_revision, diagnostic) = source.stop().await;
        assert!(
            matches!(diagnostic, RuntimeError::OperationCancelled),
            "{diagnostic}"
        );
        let mut source = Node::start(&source_root, 1, source_revision).await;
        let mut retired = Node::start(&retired_root, 2, retired_revision).await;
        assert_ne!(retired.running.handle().process_session_id(), old_session);
        let epoch = Epoch::new(0, 2);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        retired.open(ReplicaRole::IdleSecondary, epoch).await;
        source.copy(&mut retired, epoch, &single).await;
        source.pair(&mut retired, epoch).await;
        let server = SqliteServer::new(source.service.clone());
        assert_eq!(values(&server).await.unwrap(), ["acknowledged"]);
        assert!(
            write(
                &SqliteServer::new(old_service.clone()),
                "INSERT INTO data VALUES(99,'retired')"
            )
            .await
            .is_err()
        );
        assert!(
            values(&SqliteServer::new(old_service.clone()))
                .await
                .is_err()
        );
        let (_, diagnostic) = retired.stop().await;
        assert!(
            matches!(diagnostic, RuntimeError::OperationCancelled),
            "{diagnostic}"
        );
        let single = source.single(epoch).await;
        let mut replacement = Node::start(&root.path().join("fresh-replacement"), 2, 0).await;
        assert_ne!(replacement.identity, old_service_identity(&retired_root, 2));
        replacement.open(ReplicaRole::IdleSecondary, epoch).await;
        source.copy(&mut replacement, epoch, &single).await;
        source.pair(&mut replacement, epoch).await;
        assert_eq!(
            replacement
                .service
                .persistence()
                .progress()
                .unwrap()
                .committed_lsn,
            acknowledged
        );
        let later = write(&server, "INSERT INTO data VALUES(2,'replacement')")
            .await
            .unwrap();
        assert!(later > acknowledged);
        assert_eq!(
            replacement.service.persistence().snapshot(later).unwrap(),
            source.service.persistence().snapshot(later).unwrap()
        );
        assert!(
            write(
                &SqliteServer::new(old_service),
                "INSERT INTO data VALUES(100,'late-old-owner')"
            )
            .await
            .is_err()
        );
        source.close().await;
        replacement.close().await;
    })
    .await
    .expect("bounded restart and fresh storage replacement journey");
}

fn old_service_identity(root: &Path, id: i64) -> ReplicaIdentity {
    let name = root.file_name().unwrap().to_str().unwrap();
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("{name}-pod")),
        agent_generation: AgentGeneration::new(format!("{name}-agent")),
    }
}

#[tokio::test]
async fn sqlite_real_provider_restart_settlement_and_handoff() {
    use sqlite_replicated::v1_test_bridge::FailureCut;
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = root();
        let a = root.path().join("source");
        let b = root.path().join("secondary");
        let mut source = Node::start(&a, 1, 0).await;
        let mut target = Node::start(&b, 2, 0).await;
        let epoch = Epoch::new(0, 1);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source.copy(&mut target, epoch, &single).await;
        source.pair(&mut target, epoch).await;
        let acknowledged = baseline(&source).await;
        let old_service = source.service.clone();
        let (ar, diagnostic) = source.stop().await;
        assert!(
            matches!(diagnostic, RuntimeError::OperationCancelled),
            "{diagnostic}"
        );
        let (br, diagnostic) = target.stop().await;
        assert!(
            matches!(diagnostic, RuntimeError::OperationCancelled),
            "{diagnostic}"
        );
        let mut source = Node::start(&a, 1, ar).await;
        let mut target = Node::start(&b, 2, br).await;
        let epoch = Epoch::new(0, 2);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source.copy(&mut target, epoch, &single).await;
        source.pair(&mut target, epoch).await;
        assert_eq!(
            values(&SqliteServer::new(source.service.clone()))
                .await
                .unwrap(),
            ["acknowledged"]
        );
        assert_eq!(
            source
                .service
                .persistence()
                .progress()
                .unwrap()
                .committed_lsn,
            acknowledged
        );
        let epoch = Epoch::new(0, 3);
        source
            .command(PublicOperationProgram::Role {
                epoch,
                role: ReplicaRole::ActiveSecondary,
            })
            .await;
        source
            .command(PublicOperationProgram::Epoch { epoch })
            .await;
        target
            .command(PublicOperationProgram::Role {
                epoch,
                role: ReplicaRole::Primary,
            })
            .await;
        target
            .command(PublicOperationProgram::Epoch { epoch })
            .await;
        target.pair(&mut source, epoch).await;
        let server = SqliteServer::new(target.service.clone());
        let promoted = write(&server, "INSERT INTO data VALUES(2,'promoted')")
            .await
            .unwrap();
        assert!(promoted > acknowledged);
        assert_eq!(values(&server).await.unwrap(), ["acknowledged", "promoted"]);
        assert!(
            values(&SqliteServer::new(source.service.clone()))
                .await
                .is_err()
        );
        assert!(
            write(
                &SqliteServer::new(source.service.clone()),
                "INSERT INTO data VALUES(99,'old-primary')"
            )
            .await
            .is_err()
        );
        assert!(
            write(
                &SqliteServer::new(old_service),
                "INSERT INTO data VALUES(100,'retired-service')"
            )
            .await
            .is_err()
        );
        target
            .service
            .committer_for_test()
            .unwrap()
            .fail_once(FailureCut::Publication);
        assert!(
            write(&server, "INSERT INTO data VALUES(3,'ambiguous')")
                .await
                .is_err()
        );
        assert!(values(&server).await.is_err());
        assert!(
            target
                .service
                .persistence()
                .progress()
                .unwrap()
                .committed_lsn
                > promoted
        );
        assert!(
            write(&server, "INSERT INTO data VALUES(4,'late-success')")
                .await
                .is_err()
        );
        let (_, diagnostic) = target.stop().await;
        let RuntimeError::Application(diagnostic) = diagnostic else {
            panic!("publication failure lost its application diagnostic: {diagnostic}");
        };
        assert!(
            diagnostic.contains("injected SQLite service cut"),
            "{diagnostic}"
        );
        source.close().await;
    })
    .await
    .expect("bounded publication failure/restart/handoff journey");
}
