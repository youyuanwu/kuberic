use std::sync::Arc;
use std::time::Duration;

use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
    RunningReplica,
};
use kuberic_runtime::protocol::public_operations::{
    PublicConfiguration, PublicOperationClass, PublicOperationIntent, PublicOperationProgram,
    PublicV1Identity,
};
use kuberic_runtime::protocol::types::{
    AgentGeneration, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy, Epoch,
    InitializationId, PodUid, PvcUid, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole,
    ReplicaStorageIdentity, ReplicaStorageIncarnationContext, ResourceUid, StorageGeneration,
    derive_bootstrap_history, derive_history_initialization_id,
};
use kuberic_runtime::testing::sqlite_store::SqliteStore;
use kuberic_runtime::testing::state::{SCHEMA_VERSION, StorageIdentity};
use sqlite_replicated::SqlitePersistence;
use sqlite_replicated::proto;
use sqlite_replicated::proto::sqlite_store_server::SqliteStore as _;
use sqlite_replicated::server::SqliteServer;
use sqlite_replicated::service::SqliteService;

struct Resolver;

impl ReplicaEndpointResolver for Resolver {
    fn control_endpoint(&self, _identity: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }

    fn replication_endpoint(&self, _identity: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
}

struct Fixture {
    root: std::path::PathBuf,
    persistence: Arc<SqlitePersistence>,
    service: Arc<SqliteService>,
    server: SqliteServer,
    running: RunningReplica,
}

impl Fixture {
    async fn start(name: &str) -> Self {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/sqlite-publication-receipts")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let resource_uid = ResourceUid::new(format!("sqlite-{name}"));
        let preview = PublicV1Identity::new(1);
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(1),
            instance_id: ReplicaInstanceId::new(format!("{name}-pod")),
            agent_generation: AgentGeneration::new(format!("{name}-agent")),
        };
        let storage = ReplicaStorageIncarnationContext {
            replica: identity.clone(),
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new(format!("{name}-pod")),
                pvc_uid: PvcUid::new(format!("{name}-pvc")),
            },
            storage_generation: StorageGeneration::new(format!("{name}-storage")),
        };
        drop(
            SqliteStore::create_public_v1(
                SqliteStore::metadata_database_path(&root),
                preview.clone(),
                StorageIdentity {
                    schema_version: SCHEMA_VERSION,
                    resource_uid: resource_uid.clone(),
                    pod_uid: PodUid::new(format!("{name}-pod")),
                    pvc_uid: PvcUid::new(format!("{name}-pvc")),
                    initialization_id: InitializationId::new(format!("{name}-initialization")),
                    local_identity: identity.clone(),
                    effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
                },
            )
            .unwrap(),
        );

        let replication_address = address();
        let persistence = Arc::new(SqlitePersistence::open(root.join("application")).unwrap());
        let service = Arc::new(
            SqliteService::new(persistence.clone(), format!("http://{replication_address}"))
                .unwrap(),
        );
        let server = SqliteServer::new(service.clone());
        let running = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: identity.replica_id,
                pod_uid: PodUid::new(format!("{name}-pod")),
                pvc_uid: PvcUid::new(format!("{name}-pvc")),
                data_root: root.clone(),
                replication_address,
                bearer_token: "unused".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let handle = running.handle();
        let generation = kuberic_runtime::protocol::types::PublicProtocolGeneration::new(1);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        handle
            .initialize_public_v1_history(&initialization, &history, &storage)
            .await
            .unwrap();
        let epoch = Epoch::new(0, 1);
        let configuration = ConfigurationDescriptor::new(
            epoch,
            identity.replica_id,
            vec![ConfigurationMember {
                identity: identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        for (revision, name, program) in [
            (
                1,
                format!("{name}-open"),
                PublicOperationProgram::Open {
                    replica: identity.clone(),
                    existing: true,
                },
            ),
            (
                2,
                format!("{name}-role"),
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::Primary,
                },
            ),
            (
                3,
                format!("{name}-epoch"),
                PublicOperationProgram::Epoch { epoch },
            ),
            (
                4,
                format!("{name}-configuration"),
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: configuration,
                    previous: None,
                    replicas: Vec::new(),
                }),
            ),
        ] {
            handle
                .execute_public_v1(PublicOperationIntent {
                    identity: preview.clone(),
                    operation_id: kuberic_runtime::protocol::types::OperationId::new(&name),
                    revision,
                    process_session_id: handle.process_session_id(),
                    class: PublicOperationClass::Authority,
                    input_digest: program.digest(),
                    replay_disposition: program.replay_disposition(),
                    lifecycle: None,
                    program: Some(program),
                })
                .await
                .unwrap();
        }

        Self {
            root,
            persistence,
            service,
            server,
            running,
        }
    }

    async fn execute(&self, sql: &str) -> Result<proto::ExecuteResponse, tonic::Status> {
        self.server
            .execute(tonic::Request::new(proto::ExecuteRequest {
                sql: sql.into(),
                params: Vec::new(),
            }))
            .await
            .map(tonic::Response::into_inner)
    }

    async fn stop(mut self, expected: Option<&str>) {
        self.running.shutdown();
        self.running.wait().await.unwrap();
        let diagnostic = self.service.shutdown_and_drain().await;
        match expected {
            None => diagnostic.unwrap(),
            Some(message) => assert!(
                matches!(
                    diagnostic,
                    Err(kuberic_runtime::RuntimeError::Application(ref reason)) if reason.contains(message)
                ),
                "{diagnostic:?}"
            ),
        }
        drop(self.server);
        drop(self.service);
        drop(self.persistence);
        drop(self.running);
        std::fs::remove_dir_all(self.root).unwrap();
    }
}

fn address() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address
}

#[tokio::test]
async fn successful_noop_does_not_advance_or_fence_the_next_write() {
    let fixture = Fixture::start("noop").await;
    let created = fixture
        .execute("CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT)")
        .await
        .unwrap();

    let noop = fixture
        .execute("UPDATE data SET value = value WHERE id = -1")
        .await
        .unwrap();
    assert_eq!(noop.rows_affected, 0);
    assert_eq!(noop.lsn, created.lsn);
    assert_eq!(
        fixture.persistence.progress().unwrap().committed_lsn,
        created.lsn
    );

    let inserted = fixture
        .execute("INSERT INTO data VALUES(1, 'published')")
        .await
        .unwrap();
    assert_eq!(inserted.lsn, created.lsn + 1);
    assert_eq!(
        fixture.persistence.progress().unwrap().committed_lsn,
        inserted.lsn
    );

    fixture.stop(None).await;
}

#[tokio::test]
async fn failed_local_publication_returns_no_success_and_fences() {
    let fixture = Fixture::start("stale").await;
    fixture
        .execute("CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT)")
        .await
        .unwrap();
    fixture
        .service
        .committer_for_test()
        .unwrap()
        .fail_once(sqlite_replicated::v1_test_bridge::FailureCut::Publication);

    let error = fixture
        .execute("INSERT INTO data VALUES(1, 'ambiguous')")
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Unknown);
    assert!(error.message().contains("injected SQLite service cut"));
    assert!(
        fixture
            .execute("INSERT INTO data VALUES(2, 'must-not-succeed')")
            .await
            .is_err()
    );

    fixture.stop(Some("injected SQLite service cut")).await;
}
