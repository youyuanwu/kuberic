use std::sync::Arc;
use std::time::Duration;

use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
};
use kuberic_runtime::protocol::public_operations::{
    PreviewLifecycleBinding, ProcessSessionRenewalRequired, PublicBuildAttemptReference,
    PublicBuildInput, PublicConfiguration, PublicInboundBuildAuthority, PublicOperationClass,
    PublicOperationIntent, PublicOperationPreviewIdentity, PublicOperationProgram,
    PublicReplicaDescriptor, StatePersistence,
};
use kuberic_runtime::protocol::types::{
    AgentGeneration, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy, Epoch,
    InitializationId, OperationId, PodUid, ProcessSessionEndpoint, ProcessSessionRenewalReason,
    PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole,
    ReplicaStorageIdentity, ReplicaStorageIncarnationContext, ReplicatorCreationId, ResourceUid,
    StorageGeneration, derive_bootstrap_history, derive_history_initialization_id,
};
use kuberic_runtime::testing::sqlite_store::SqliteStore;
use kuberic_runtime::testing::state::{SCHEMA_VERSION, StorageIdentity};
use sqlite_replicated::SqlitePersistence;
use sqlite_replicated::proto;
use sqlite_replicated::proto::sqlite_store_server::SqliteStore as _;
use sqlite_replicated::server::SqliteServer;
use sqlite_replicated::service::SqliteService;
use sqlite_replicated::testing::{SqlitePod, scratch};

struct Resolver;

impl ReplicaEndpointResolver for Resolver {
    fn control_endpoint(&self, _identity: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }

    fn replication_endpoint(&self, _identity: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
}

fn address() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address
}

fn identity(id: i64, name: &str) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("{name}-pod")),
        agent_generation: AgentGeneration::new(format!("{name}-agent")),
    }
}

fn storage(identity: ReplicaIdentity, name: &str) -> ReplicaStorageIncarnationContext {
    ReplicaStorageIncarnationContext {
        replica: identity,
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new(format!("{name}-pod")),
            pvc_uid: PvcUid::new(format!("{name}-pvc")),
        },
        storage_generation: StorageGeneration::new(format!("{name}-storage")),
    }
}

fn store_identity(
    resource_uid: ResourceUid,
    identity: ReplicaIdentity,
    name: &str,
) -> StorageIdentity {
    StorageIdentity {
        schema_version: SCHEMA_VERSION,
        resource_uid,
        pod_uid: PodUid::new(format!("{name}-pod")),
        pvc_uid: PvcUid::new(format!("{name}-pvc")),
        initialization_id: InitializationId::new(format!("{name}-initialization")),
        local_identity: identity,
        effective_policy: EffectivePolicy::fixed(2, 30).unwrap(),
    }
}

fn intent(
    preview: &PublicOperationPreviewIdentity,
    session: kuberic_runtime::protocol::types::ProcessSessionId,
    revision: u64,
    name: &str,
    program: PublicOperationProgram,
) -> PublicOperationIntent {
    let class = match &program {
        PublicOperationProgram::Build(build) => PublicOperationClass::Build {
            target: build.replica.replica_id,
        },
        PublicOperationProgram::Remove(build) => PublicOperationClass::Remove {
            target: build.replica.replica_id,
        },
        _ => PublicOperationClass::Authority,
    };
    PublicOperationIntent {
        preview: preview.clone(),
        operation_id: OperationId::new(name),
        revision,
        process_session_id: session,
        class,
        input_digest: program.digest(),
        replay_disposition: program.replay_disposition(),
        lifecycle: None,
        program: Some(program),
    }
}

async fn execute(server: &SqliteServer, sql: &str) -> i64 {
    server
        .execute(tonic::Request::new(proto::ExecuteRequest {
            sql: sql.into(),
            params: Vec::new(),
        }))
        .await
        .unwrap_or_else(|error| panic!("SQLite execute failed for {sql}: {error}"))
        .into_inner()
        .lsn
}

#[tokio::test]
async fn v2_data_loss_callback_reports_unchanged_and_preserves_sql_and_progress() {
    let root = scratch();
    let pod = SqlitePod::singleton(root.path().join("replica")).await;
    pod.execute("CREATE TABLE data(id INTEGER)").await.unwrap();
    pod.execute("INSERT INTO data VALUES(1)").await.unwrap();
    let before = pod.application.persistence().progress().unwrap();
    let snapshot = pod
        .application
        .persistence()
        .snapshot(before.committed_lsn)
        .unwrap();
    assert!(
        !pod.runtime
            .primary_replicator()
            .await
            .unwrap()
            .on_data_loss()
            .await
            .unwrap()
    );
    assert_eq!(pod.application.persistence().progress().unwrap(), before);
    assert_eq!(
        pod.application
            .persistence()
            .snapshot(before.committed_lsn)
            .unwrap(),
        snapshot
    );
    assert_eq!(pod.count().await, 1);
}

#[tokio::test]
async fn sqlite_fresh_restart_build_and_corrective_empty_replacement() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("target")
            .join("sqlite-public-v1-preview")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&root);
        let source_root = root.join("source");
        let replacement_root = root.join("replacement");
        let retired_root = root.join("retired-higher-progress");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&replacement_root).unwrap();

        let retired = SqlitePod::singleton(retired_root.clone()).await;
        retired
            .execute("CREATE TABLE stale(id INTEGER PRIMARY KEY, value TEXT)")
            .await
            .unwrap();
        for id in 1..=8 {
            retired
                .execute(&format!("INSERT INTO stale VALUES({id},'must-not-return')"))
                .await
                .unwrap();
        }
        let retired_progress = retired.application.persistence().progress().unwrap();
        drop(retired);
        std::fs::remove_dir_all(retired_root.join(".kuberic")).unwrap();

        let resource_uid = ResourceUid::new("sqlite-public-v1-preview");
        let preview = PublicOperationPreviewIdentity::public_v1(52);
        let binding = PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let source_identity = identity(1, "sqlite-source");
        let divergent_identity = identity(2, "sqlite-divergent");
        let replacement_identity = identity(2, "sqlite-replacement");
        let source_storage = storage(source_identity.clone(), "sqlite-source");
        let divergent_storage = storage(divergent_identity.clone(), "sqlite-divergent");
        let replacement_storage = storage(replacement_identity.clone(), "sqlite-replacement");
        for (path, identity, name) in [
            (&source_root, source_identity.clone(), "sqlite-source"),
            (
                &retired_root,
                divergent_identity.clone(),
                "sqlite-divergent",
            ),
            (
                &replacement_root,
                replacement_identity.clone(),
                "sqlite-replacement",
            ),
        ] {
            drop(
                SqliteStore::create_public_v1(
                    SqliteStore::metadata_database_path(path),
                    preview.clone(),
                    store_identity(resource_uid.clone(), identity, name),
                )
                .unwrap(),
            );
        }
        let source_address = address();
        let divergent_address = address();
        let replacement_address = address();
        let source_persistence =
            Arc::new(SqlitePersistence::open(source_root.join("application")).unwrap());
        let replacement_persistence =
            Arc::new(SqlitePersistence::open(replacement_root.join("application")).unwrap());
        let divergent_persistence =
            Arc::new(SqlitePersistence::open(retired_root.join("application")).unwrap());
        let source_service = Arc::new(
            SqliteService::new(
                source_persistence.clone(),
                format!("http://{source_address}"),
            )
            .unwrap()
            .with_stateless_preview(),
        );
        let replacement_service = Arc::new(
            SqliteService::new(
                replacement_persistence.clone(),
                format!("http://{replacement_address}"),
            )
            .unwrap()
            .with_stateless_preview(),
        );
        let divergent_service = Arc::new(
            SqliteService::new(
                divergent_persistence.clone(),
                format!("http://{divergent_address}"),
            )
            .unwrap()
            .with_stateless_preview(),
        );
        let source_server = SqliteServer::new(source_service.clone());
        let mut source = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("sqlite-source-pod"),
                pvc_uid: PvcUid::new("sqlite-source-pvc"),
                data_root: source_root.clone(),
                control_address: address(),
                replication_address: source_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            source_service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding.clone())
        .start()
        .await
        .unwrap();
        let mut replacement = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("sqlite-replacement-pod"),
                pvc_uid: PvcUid::new("sqlite-replacement-pvc"),
                data_root: replacement_root.clone(),
                control_address: address(),
                replication_address: replacement_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            replacement_service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding.clone())
        .start()
        .await
        .unwrap();
        let mut divergent = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("sqlite-divergent-pod"),
                pvc_uid: PvcUid::new("sqlite-divergent-pvc"),
                data_root: retired_root.clone(),
                control_address: address(),
                replication_address: divergent_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            divergent_service,
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding.clone())
        .start()
        .await
        .unwrap();
        let source_handle = source.handle();
        let divergent_handle = divergent.handle();
        let replacement_handle = replacement.handle();
        let generation = PublicProtocolGeneration::new(52);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        source_handle
            .initialize_stateless_preview_history(&initialization, &history, &source_storage)
            .await
            .unwrap();
        replacement_handle
            .initialize_stateless_preview_history(&initialization, &history, &replacement_storage)
            .await
            .unwrap();
        divergent_handle
            .initialize_stateless_preview_history(&initialization, &history, &divergent_storage)
            .await
            .unwrap();
        let epoch = Epoch::new(0, 1);
        let source_configuration = ConfigurationDescriptor::new(
            epoch,
            source_identity.replica_id,
            vec![ConfigurationMember {
                identity: source_identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        for (revision, name, program) in [
            (
                1,
                "sqlite-source-open",
                PublicOperationProgram::Open {
                    replica: source_identity.clone(),
                    existing: true,
                },
            ),
            (
                2,
                "sqlite-source-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::Primary,
                },
            ),
            (
                3,
                "sqlite-source-epoch",
                PublicOperationProgram::Epoch { epoch },
            ),
            (
                4,
                "sqlite-source-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: source_configuration.clone(),
                    previous: None,
                    replicas: Vec::new(),
                }),
            ),
        ] {
            source_handle
                .execute_stateless_preview(intent(
                    &preview,
                    source_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        for (revision, name, program) in [
            (
                1,
                "sqlite-divergent-open",
                PublicOperationProgram::Open {
                    replica: divergent_identity.clone(),
                    existing: true,
                },
            ),
            (
                2,
                "sqlite-divergent-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
            (
                3,
                "sqlite-divergent-epoch",
                PublicOperationProgram::Epoch { epoch },
            ),
        ] {
            divergent_handle
                .execute_stateless_preview(intent(
                    &preview,
                    divergent_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        for (revision, name, program) in [
            (
                1,
                "sqlite-replacement-open",
                PublicOperationProgram::Open {
                    replica: replacement_identity.clone(),
                    existing: true,
                },
            ),
            (
                2,
                "sqlite-replacement-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
            (
                3,
                "sqlite-replacement-epoch",
                PublicOperationProgram::Epoch { epoch },
            ),
        ] {
            replacement_handle
                .execute_stateless_preview(intent(
                    &preview,
                    replacement_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }

        assert_eq!(
            execute(
                &source_server,
                "CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT, padding BLOB)"
            )
            .await,
            1
        );
        assert_eq!(
            execute(
                &source_server,
                "INSERT INTO data VALUES(1,'snapshot',zeroblob(524288))"
            )
            .await,
            2
        );
        let divergent_build = PublicBuildInput {
            attempt: OperationId::new("sqlite-divergent-build"),
            epoch,
            current_configuration_id: Some(source_configuration.configuration_id.clone()),
            replica: divergent_identity.clone(),
            target_storage: divergent_storage.clone(),
            process_session_id: divergent_handle.process_session_id(),
            replication_address: format!("http://{divergent_address}"),
            history: Some(history.clone()),
        };
        divergent_handle
            .install_stateless_preview_inbound_build_authority(&PublicInboundBuildAuthority {
                attempt: PublicBuildAttemptReference {
                    resource_uid: resource_uid.clone(),
                    epoch,
                    build_id: divergent_build.attempt.clone(),
                    source: source_identity.clone(),
                    source_storage: source_storage.clone(),
                    source_session: source_handle.process_session_id(),
                    target: divergent_identity.clone(),
                    target_storage: divergent_storage.clone(),
                    target_session: divergent_handle.process_session_id(),
                },
                history: history.clone(),
                current_configuration_id: source_configuration.configuration_id.clone(),
                target_address: format!("http://{divergent_address}"),
            })
            .await
            .unwrap();
        assert!(
            source_handle
                .execute_stateless_preview(intent(
                    &preview,
                    source_handle.process_session_id(),
                    4,
                    "sqlite-divergent-build",
                    PublicOperationProgram::Build(divergent_build.clone()),
                ))
                .await
                .is_err()
        );
        assert!(
            source_handle
                .stateless_preview_report()
                .await
                .unwrap()
                .completed_operations
                .iter()
                .any(|completion| {
                    completion.operation_id == divergent_build.attempt
                        && matches!(
                        completion.result,
                        kuberic_runtime::protocol::public_operations::
                            PublicV1OperationResult::RebuildRequired(_)
                    )
                })
        );
        source_handle
            .execute_stateless_preview(intent(
                &preview,
                source_handle.process_session_id(),
                4,
                "sqlite-retire-divergent-build",
                PublicOperationProgram::Remove(divergent_build),
            ))
            .await
            .unwrap();
        divergent.shutdown();
        divergent.wait().await.unwrap();
        assert_eq!(divergent_persistence.progress().unwrap(), retired_progress);
        let source_predecessor = source_handle
            .stateless_preview_process_endpoint(ReplicaRole::Primary)
            .await
            .unwrap();
        source.shutdown();
        source.wait().await.unwrap();
        let source_service = Arc::new(
            SqliteService::new(
                source_persistence.clone(),
                format!("http://{source_address}"),
            )
            .unwrap()
            .with_stateless_preview(),
        );
        let source_server = SqliteServer::new(source_service.clone());
        let mut source = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("sqlite-source-pod"),
                pvc_uid: PvcUid::new("sqlite-source-pvc"),
                data_root: source_root.clone(),
                control_address: address(),
                replication_address: source_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            source_service.clone(),
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding.clone())
        .start()
        .await
        .unwrap();
        let source_handle = source.handle();
        let epoch = Epoch::new(0, 2);
        let source_configuration = ConfigurationDescriptor::new(
            epoch,
            source_identity.replica_id,
            vec![ConfigurationMember {
                identity: source_identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        source_handle
            .authorize_stateless_preview_session_renewal(ProcessSessionRenewalRequired {
                operation_id: OperationId::new("sqlite-source-after-divergent-retirement"),
                reason: ProcessSessionRenewalReason::PrimaryReplacement,
                resource_uid: resource_uid.clone(),
                history: history.clone(),
                previous_epoch: Epoch::new(0, 1),
                renewal_epoch: epoch,
                predecessor: source_predecessor,
                replacement: Some(ProcessSessionEndpoint {
                    identity: source_identity.clone(),
                    storage_incarnation: source_storage.clone(),
                    process_session_id: source_handle.process_session_id(),
                    replicator_creation_id: ReplicatorCreationId::new(
                        "sqlite-source-after-divergent-retirement-creation",
                    ),
                    role: ReplicaRole::None,
                }),
                surviving_sessions: Vec::new(),
                fresh_configuration: source_configuration.clone(),
                predecessor_contained: true,
                data_loss: None,
            })
            .await
            .unwrap();
        for (revision, name, program) in [
            (
                5,
                "sqlite-source-after-divergent-open",
                PublicOperationProgram::Open {
                    replica: source_identity.clone(),
                    existing: true,
                },
            ),
            (
                6,
                "sqlite-source-after-divergent-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::Primary,
                },
            ),
            (
                7,
                "sqlite-source-after-divergent-epoch",
                PublicOperationProgram::Epoch { epoch },
            ),
            (
                8,
                "sqlite-source-after-divergent-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: source_configuration.clone(),
                    previous: None,
                    replicas: Vec::new(),
                }),
            ),
        ] {
            source_handle
                .execute_stateless_preview(intent(
                    &preview,
                    source_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        for (revision, name, program) in [
            (
                4,
                "sqlite-replacement-recovery-epoch",
                PublicOperationProgram::Epoch { epoch },
            ),
            (
                5,
                "sqlite-replacement-recovery-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
        ] {
            replacement_handle
                .execute_stateless_preview(intent(
                    &preview,
                    replacement_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        let build_input = PublicBuildInput {
            attempt: OperationId::new("sqlite-corrective-build"),
            epoch,
            current_configuration_id: Some(source_configuration.configuration_id.clone()),
            replica: replacement_identity.clone(),
            target_storage: replacement_storage.clone(),
            process_session_id: replacement_handle.process_session_id(),
            replication_address: format!("http://{replacement_address}"),
            history: Some(history.clone()),
        };
        replacement_handle
            .install_stateless_preview_inbound_build_authority(&PublicInboundBuildAuthority {
                attempt: PublicBuildAttemptReference {
                    resource_uid: resource_uid.clone(),
                    epoch,
                    build_id: build_input.attempt.clone(),
                    source: source_identity.clone(),
                    source_storage: source_storage.clone(),
                    source_session: source_handle.process_session_id(),
                    target: replacement_identity.clone(),
                    target_storage: replacement_storage.clone(),
                    target_session: replacement_handle.process_session_id(),
                },
                history: history.clone(),
                current_configuration_id: source_configuration.configuration_id.clone(),
                target_address: format!("http://{replacement_address}"),
            })
            .await
            .unwrap();
        source_service.copy_snapshot_gate().arm();
        let mut build = tokio::spawn({
            let handle = source_handle.clone();
            let preview = preview.clone();
            let input = build_input.clone();
            async move {
                handle
                    .execute_stateless_preview(intent(
                        &preview,
                        handle.process_session_id(),
                        8,
                        "sqlite-corrective-build",
                        PublicOperationProgram::Build(input),
                    ))
                    .await
            }
        });
        source_service.copy_snapshot_gate().wait_entered().await;
        assert!(
            !build.is_finished(),
            "large SQLite copy finished before the retained write"
        );
        assert_eq!(
            execute(
                &source_server,
                "INSERT INTO data VALUES(2,'retained',zeroblob(1))"
            )
            .await,
            3
        );
        source_service.copy_snapshot_gate().release();
        let build_result = tokio::time::timeout(Duration::from_secs(30), &mut build)
            .await
            .unwrap()
            .unwrap();
        if let Err(error) = build_result {
            panic!(
                "SQLite build failed: {error}; target={:?}",
                replacement_persistence.inspect_for_test()
            );
        }
        assert_eq!(replacement_persistence.progress().unwrap().committed_lsn, 3);
        let inspection = replacement_persistence.inspect_for_test().unwrap();
        let inspection_path = root.join("replacement-inspection.sqlite");
        std::fs::write(&inspection_path, &inspection.image).unwrap();
        let database = rusqlite::Connection::open(&inspection_path).unwrap();
        let rows = database
            .query_row("SELECT count(*) FROM data", [], |row| row.get::<_, i64>(0))
            .unwrap();
        assert_eq!(rows, 2);
        drop(database);

        let admitted = ConfigurationDescriptor::new(
            epoch,
            source_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source_identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: replacement_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        replacement_handle
            .execute_stateless_preview(intent(
                &preview,
                replacement_handle.process_session_id(),
                6,
                "sqlite-replacement-role-active",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::ActiveSecondary,
                },
            ))
            .await
            .unwrap();
        replacement_handle
            .execute_stateless_preview(intent(
                &preview,
                replacement_handle.process_session_id(),
                7,
                "sqlite-replacement-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: admitted.clone(),
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: source_identity.clone(),
                        replication_address: format!("http://{source_address}"),
                        process_session_id: source_handle.process_session_id(),
                        storage_incarnation: Some(source_storage.clone()),
                        role: ReplicaRole::Primary,
                        current_progress: 3,
                        catch_up_capability: 3,
                        must_catchup: false,
                    }],
                }),
            ))
            .await
            .unwrap();
        source_handle
            .execute_stateless_preview(intent(
                &preview,
                source_handle.process_session_id(),
                9,
                "sqlite-source-configuration-admit",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: admitted,
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: replacement_identity.clone(),
                        replication_address: format!("http://{replacement_address}"),
                        process_session_id: replacement_handle.process_session_id(),
                        storage_incarnation: Some(replacement_storage.clone()),
                        role: ReplicaRole::ActiveSecondary,
                        current_progress: 3,
                        catch_up_capability: 3,
                        must_catchup: false,
                    }],
                }),
            ))
            .await
            .unwrap();
        assert_eq!(
            execute(
                &source_server,
                "INSERT INTO data VALUES(3,'after-build',zeroblob(1))"
            )
            .await,
            4
        );
        assert_eq!(replacement_persistence.progress().unwrap().committed_lsn, 4);
        let inspection = replacement_persistence.inspect_for_test().unwrap();
        std::fs::write(&inspection_path, &inspection.image).unwrap();
        let database = rusqlite::Connection::open(&inspection_path).unwrap();
        assert_eq!(
            database
                .query_row("SELECT count(*) FROM data", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            3
        );
        drop(database);

        let query = source_server
            .query(tonic::Request::new(proto::QueryRequest {
                sql: "SELECT value FROM data ORDER BY id".into(),
                params: Vec::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(query.rows.len(), 3);
        assert_eq!(
            execute(
                &source_server,
                "INSERT INTO data VALUES(4,'after-restart',zeroblob(1))"
            )
            .await,
            5
        );

        let replacement_predecessor = replacement_handle
            .stateless_preview_process_endpoint(ReplicaRole::ActiveSecondary)
            .await
            .unwrap();
        replacement.shutdown();
        replacement.wait().await.unwrap();
        let replacement_restart_address = address();
        let replacement_restart_persistence =
            Arc::new(SqlitePersistence::open(replacement_root.join("application")).unwrap());
        let replacement_restart_service = Arc::new(
            SqliteService::new(
                replacement_restart_persistence.clone(),
                format!("http://{replacement_restart_address}"),
            )
            .unwrap()
            .with_stateless_preview(),
        );
        let mut restarted_replacement = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("sqlite-replacement-pod"),
                pvc_uid: PvcUid::new("sqlite-replacement-pvc"),
                data_root: replacement_root.clone(),
                control_address: address(),
                replication_address: replacement_restart_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            replacement_restart_service,
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_stateless_preview(PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        })
        .start()
        .await
        .unwrap();
        let replacement_restart_handle = restarted_replacement.handle();
        let replacement_restart_epoch = Epoch::new(0, 4);
        let replacement_restart_configuration = ConfigurationDescriptor::new(
            replacement_restart_epoch,
            source_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source_identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: replacement_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        replacement_restart_handle
            .authorize_stateless_preview_session_renewal(ProcessSessionRenewalRequired {
                operation_id: OperationId::new("sqlite-replacement-restart"),
                reason: ProcessSessionRenewalReason::ActiveSecondaryReplacement,
                resource_uid: resource_uid.clone(),
                history: history.clone(),
                previous_epoch: epoch,
                renewal_epoch: replacement_restart_epoch,
                predecessor: replacement_predecessor,
                replacement: Some(ProcessSessionEndpoint {
                    identity: replacement_identity.clone(),
                    storage_incarnation: replacement_storage.clone(),
                    process_session_id: replacement_restart_handle.process_session_id(),
                    replicator_creation_id: ReplicatorCreationId::new(
                        "sqlite-replacement-restart-creation",
                    ),
                    role: ReplicaRole::None,
                }),
                surviving_sessions: vec![
                    source_handle
                        .stateless_preview_process_endpoint(ReplicaRole::Primary)
                        .await
                        .unwrap(),
                ],
                fresh_configuration: replacement_restart_configuration.clone(),
                predecessor_contained: true,
                data_loss: None,
            })
            .await
            .unwrap();
        source_handle
            .execute_stateless_preview(intent(
                &preview,
                source_handle.process_session_id(),
                14,
                "sqlite-source-replacement-restart-epoch",
                PublicOperationProgram::Epoch {
                    epoch: replacement_restart_epoch,
                },
            ))
            .await
            .unwrap();
        for (revision, name, program) in [
            (
                8,
                "sqlite-replacement-restart-open",
                PublicOperationProgram::Open {
                    replica: replacement_identity.clone(),
                    existing: true,
                },
            ),
            (
                9,
                "sqlite-replacement-restart-role",
                PublicOperationProgram::Role {
                    epoch: replacement_restart_epoch,
                    role: ReplicaRole::ActiveSecondary,
                },
            ),
            (
                10,
                "sqlite-replacement-restart-epoch",
                PublicOperationProgram::Epoch {
                    epoch: replacement_restart_epoch,
                },
            ),
        ] {
            replacement_restart_handle
                .execute_stateless_preview(intent(
                    &preview,
                    replacement_restart_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        source_handle
            .execute_stateless_preview(intent(
                &preview,
                source_handle.process_session_id(),
                15,
                "sqlite-source-replacement-restart-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: replacement_restart_configuration.clone(),
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: replacement_identity.clone(),
                        replication_address: format!("http://{replacement_restart_address}"),
                        process_session_id: replacement_restart_handle.process_session_id(),
                        storage_incarnation: Some(replacement_storage.clone()),
                        role: ReplicaRole::ActiveSecondary,
                        current_progress: 5,
                        catch_up_capability: 5,
                        must_catchup: false,
                    }],
                }),
            ))
            .await
            .unwrap();
        replacement_restart_handle
            .execute_stateless_preview(intent(
                &preview,
                replacement_restart_handle.process_session_id(),
                11,
                "sqlite-replacement-restart-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: replacement_restart_configuration.clone(),
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: source_identity.clone(),
                        replication_address: format!("http://{source_address}"),
                        process_session_id: source_handle.process_session_id(),
                        storage_incarnation: Some(source_storage.clone()),
                        role: ReplicaRole::Primary,
                        current_progress: 5,
                        catch_up_capability: 5,
                        must_catchup: false,
                    }],
                }),
            ))
            .await
            .unwrap();
        let replacement_image = replacement_restart_persistence
            .inspect_for_test()
            .unwrap()
            .image;
        let replacement_image_path = root.join("replacement-restart-inspection.sqlite");
        std::fs::write(&replacement_image_path, replacement_image).unwrap();
        let replacement_database = rusqlite::Connection::open(&replacement_image_path).unwrap();
        assert_eq!(
            replacement_database
                .query_row("SELECT count(*) FROM data", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            4
        );
        let readmit = ConfigurationDescriptor::new(
            replacement_restart_epoch,
            source_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source_identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: replacement_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: divergent_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        assert!(
            source_handle
                .execute_stateless_preview(intent(
                    &preview,
                    source_handle.process_session_id(),
                    16,
                    "sqlite-reject-retired-divergent-after-restart",
                    PublicOperationProgram::Configuration(PublicConfiguration {
                        current: readmit,
                        previous: None,
                        replicas: vec![
                            PublicReplicaDescriptor {
                                build_id: OperationId::default(),
                                identity: replacement_identity.clone(),
                                replication_address: format!(
                                    "http://{replacement_restart_address}"
                                ),
                                process_session_id: replacement_restart_handle.process_session_id(),
                                storage_incarnation: Some(replacement_storage.clone()),
                                role: ReplicaRole::ActiveSecondary,
                                current_progress: 5,
                                catch_up_capability: 5,
                                must_catchup: false,
                            },
                            PublicReplicaDescriptor {
                                build_id: OperationId::default(),
                                identity: divergent_identity.clone(),
                                replication_address: format!("http://{divergent_address}"),
                                process_session_id: divergent_handle.process_session_id(),
                                storage_incarnation: Some(divergent_storage.clone()),
                                role: ReplicaRole::ActiveSecondary,
                                current_progress: 99,
                                catch_up_capability: 99,
                                must_catchup: false,
                            },
                        ],
                    }),
                ))
                .await
                .is_err()
        );

        source.shutdown();
        restarted_replacement.shutdown();
        source.wait().await.unwrap();
        restarted_replacement.wait().await.unwrap();
        assert_eq!(
            SqlitePersistence::open(retired_root.join("application"))
                .unwrap()
                .progress()
                .unwrap(),
            retired_progress
        );
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}
