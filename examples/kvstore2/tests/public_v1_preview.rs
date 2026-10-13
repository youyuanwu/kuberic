#![cfg(feature = "testing")]

use std::sync::Arc;
use std::time::Duration;

use kuberic_runtime::engine::DurableState;
use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
};
use kuberic_runtime::protocol::public_operations::{
    PreviewLifecycleBinding, ProcessSessionRenewalRequired, PublicBuildInput, PublicConfiguration,
    PublicOperationClass, PublicOperationIntent, PublicOperationPreviewIdentity,
    PublicOperationProgram, PublicReplicaDescriptor, StatePersistence,
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
use kvstore2::persistence::KvPersistence;
use kvstore2::service::KvService;
use kvstore2::state::CopyGate;

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

#[tokio::test]
async fn kvstore2_fresh_restart_build_and_corrective_empty_replacement() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("kvstore2-public-v1-preview")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&root);
        let source_root = root.join("source");
        let replacement_root = root.join("replacement");
        let retired_root = root.join("retired-higher-progress");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&replacement_root).unwrap();

        let retired = KvPersistence::open(&retired_root).unwrap();
        retired
            .apply(kuberic_runtime::application::Operation {
                lsn: 9,
                committed_lsn: 0,
                data: KvPersistence::encode_put("stale".into(), "must-not-return".into()).unwrap(),
            })
            .await
            .unwrap();
        retired.commit(9).await.unwrap();
        drop(retired);

        let resource_uid = ResourceUid::new("kvstore2-public-v1-preview");
        let preview = PublicOperationPreviewIdentity::public_v1(51);
        let binding = PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let source_identity = identity(1, "kv-source");
        let replacement_identity = identity(2, "kv-replacement");
        let source_storage = storage(source_identity.clone(), "kv-source");
        let replacement_storage = storage(replacement_identity.clone(), "kv-replacement");
        for (path, identity, name) in [
            (&source_root, source_identity.clone(), "kv-source"),
            (
                &replacement_root,
                replacement_identity.clone(),
                "kv-replacement",
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
        let replacement_address = address();
        let source_persistence =
            Arc::new(KvPersistence::open(source_root.join("application")).unwrap());
        let replacement_persistence =
            Arc::new(KvPersistence::open(replacement_root.join("application")).unwrap());
        let copy_gate = CopyGate::enabled(source_root.join("copy-gate")).unwrap();
        let source_service = Arc::new(
            KvService::new(
                source_persistence.clone(),
                format!("http://{source_address}"),
                copy_gate.clone(),
            )
            .with_stateless_preview(),
        );
        let replacement_service = Arc::new(
            KvService::new(
                replacement_persistence.clone(),
                format!("http://{replacement_address}"),
                CopyGate::disabled(),
            )
            .with_stateless_preview(),
        );
        let mut source = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("kv-source-pod"),
                pvc_uid: PvcUid::new("kv-source-pvc"),
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
                pod_uid: PodUid::new("kv-replacement-pod"),
                pvc_uid: PvcUid::new("kv-replacement-pvc"),
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
        let source_handle = source.handle();
        let replacement_handle = replacement.handle();
        let generation = PublicProtocolGeneration::new(51);
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
                "kv-source-open",
                PublicOperationProgram::Open {
                    replica: source_identity.clone(),
                    existing: true,
                },
            ),
            (
                2,
                "kv-source-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::Primary,
                },
            ),
            (
                3,
                "kv-source-epoch",
                PublicOperationProgram::Epoch { epoch },
            ),
            (
                4,
                "kv-source-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: source_configuration,
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
                "kv-replacement-open",
                PublicOperationProgram::Open {
                    replica: replacement_identity.clone(),
                    existing: true,
                },
            ),
            (
                2,
                "kv-replacement-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
            (
                3,
                "kv-replacement-epoch",
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
            source_service
                .replicate_put("snapshot".into(), "v1".into())
                .await
                .unwrap(),
            1
        );
        copy_gate.hold().unwrap();
        let build_input = PublicBuildInput {
            attempt: OperationId::new("kv-corrective-build"),
            epoch,
            replica: replacement_identity.clone(),
            target_storage: replacement_storage.clone(),
            process_session_id: replacement_handle.process_session_id(),
            replication_address: format!("http://{replacement_address}"),
            history: Some(history.clone()),
        };
        let mut build = tokio::spawn({
            let handle = source_handle.clone();
            let preview = preview.clone();
            let input = build_input.clone();
            async move {
                handle
                    .execute_stateless_preview(intent(
                        &preview,
                        handle.process_session_id(),
                        4,
                        "kv-corrective-build",
                        PublicOperationProgram::Build(input),
                    ))
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!build.is_finished(), "build did not wait on the copy gate");
        assert_eq!(
            source_service
                .replicate_put("retained".into(), "v2".into())
                .await
                .unwrap(),
            2
        );
        copy_gate.release().unwrap();
        tokio::time::timeout(Duration::from_secs(20), &mut build)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            replacement_service.get("snapshot").await.is_err(),
            "idle replacement must remain access closed before admission"
        );
        assert_eq!(
            replacement_persistence.get("snapshot").as_deref(),
            Some("v1")
        );
        assert_eq!(
            replacement_persistence.get("retained").as_deref(),
            Some("v2")
        );
        assert_eq!(replacement_persistence.get("stale"), None);
        assert_eq!(
            replacement_persistence
                .durable_progress()
                .await
                .unwrap()
                .committed_lsn,
            2
        );

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
                4,
                "kv-replacement-admit-role",
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
                5,
                "kv-replacement-admit-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: admitted.clone(),
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: source_identity.clone(),
                        replication_address: format!("http://{source_address}"),
                        process_session_id: source_handle.process_session_id(),
                        role: ReplicaRole::Primary,
                        current_progress: 2,
                        catch_up_capability: 2,
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
                5,
                "kv-source-admit-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: admitted,
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: replacement_identity.clone(),
                        replication_address: format!("http://{replacement_address}"),
                        process_session_id: replacement_handle.process_session_id(),
                        role: ReplicaRole::ActiveSecondary,
                        current_progress: 2,
                        catch_up_capability: 2,
                        must_catchup: false,
                    }],
                }),
            ))
            .await
            .unwrap();
        assert_eq!(
            source_service
                .replicate_put("after-build".into(), "v3".into())
                .await
                .unwrap(),
            3
        );
        assert_eq!(
            replacement_service
                .get("after-build")
                .await
                .unwrap()
                .as_deref(),
            Some("v3")
        );
        source_handle
            .execute_stateless_preview(intent(
                &preview,
                source_handle.process_session_id(),
                5,
                "kv-retire-built-incarnation",
                PublicOperationProgram::Remove(build_input),
            ))
            .await
            .unwrap();
        source_handle
            .execute_stateless_preview(intent(
                &preview,
                source_handle.process_session_id(),
                6,
                "kv-exclude-retired-incarnation",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: ConfigurationDescriptor::new(
                        epoch,
                        source_identity.replica_id,
                        vec![ConfigurationMember {
                            identity: source_identity.clone(),
                            role: ReplicaRole::Primary,
                        }],
                        1,
                    ),
                    previous: None,
                    replicas: Vec::new(),
                }),
            ))
            .await
            .unwrap();

        let predecessor = source_handle
            .stateless_preview_process_endpoint(ReplicaRole::Primary)
            .await
            .unwrap();
        let predecessor_session = predecessor.process_session_id.clone();
        source.shutdown();
        source.wait().await.unwrap();
        let restarted_address = address();
        let restarted_service = Arc::new(
            KvService::new(
                Arc::new(KvPersistence::open(source_root.join("application")).unwrap()),
                format!("http://{restarted_address}"),
                CopyGate::disabled(),
            )
            .with_stateless_preview(),
        );
        let mut restarted = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("kv-source-pod"),
                pvc_uid: PvcUid::new("kv-source-pvc"),
                data_root: source_root.clone(),
                control_address: address(),
                replication_address: restarted_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            restarted_service.clone(),
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding)
        .start()
        .await
        .unwrap();
        let restarted_handle = restarted.handle();
        let renewal_epoch = Epoch::new(0, 2);
        let renewal_configuration = ConfigurationDescriptor::new(
            renewal_epoch,
            source_identity.replica_id,
            vec![ConfigurationMember {
                identity: source_identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        restarted_handle
            .authorize_stateless_preview_session_renewal(ProcessSessionRenewalRequired {
                operation_id: OperationId::new("kv-source-restart"),
                reason: ProcessSessionRenewalReason::PrimaryReplacement,
                resource_uid: resource_uid.clone(),
                history: history.clone(),
                previous_epoch: epoch,
                renewal_epoch,
                predecessor,
                replacement: Some(ProcessSessionEndpoint {
                    identity: source_identity.clone(),
                    storage_incarnation: source_storage.clone(),
                    process_session_id: restarted_handle.process_session_id(),
                    replicator_creation_id: ReplicatorCreationId::new("kv-source-restart-creation"),
                    role: ReplicaRole::None,
                }),
                surviving_sessions: Vec::new(),
                fresh_configuration: renewal_configuration.clone(),
                predecessor_contained: true,
                data_loss: None,
            })
            .await
            .unwrap();
        for (revision, name, program) in [
            (
                7,
                "kv-restart-open",
                PublicOperationProgram::Open {
                    replica: source_identity.clone(),
                    existing: true,
                },
            ),
            (
                8,
                "kv-restart-role",
                PublicOperationProgram::Role {
                    epoch: renewal_epoch,
                    role: ReplicaRole::Primary,
                },
            ),
            (
                9,
                "kv-restart-epoch",
                PublicOperationProgram::Epoch {
                    epoch: renewal_epoch,
                },
            ),
            (
                10,
                "kv-restart-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: renewal_configuration,
                    previous: None,
                    replicas: Vec::new(),
                }),
            ),
        ] {
            restarted_handle
                .execute_stateless_preview(intent(
                    &preview,
                    restarted_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        assert_eq!(
            restarted_service.get("snapshot").await.unwrap().as_deref(),
            Some("v1")
        );
        assert_eq!(
            restarted_service.get("retained").await.unwrap().as_deref(),
            Some("v2")
        );
        assert_eq!(restarted_service.get("stale").await.unwrap(), None);
        assert_eq!(
            restarted_service
                .replicate_put("after-restart".into(), "v4".into())
                .await
                .unwrap(),
            4
        );

        let before = restarted_service
            .persistence()
            .durable_progress()
            .await
            .unwrap();
        assert!(
            restarted_handle
                .execute_stateless_preview(intent(
                    &preview,
                    predecessor_session,
                    11,
                    "predecessor-session-progress",
                    PublicOperationProgram::Progress { capability: false },
                ))
                .await
                .is_err()
        );
        assert_eq!(
            restarted_service
                .persistence()
                .durable_progress()
                .await
                .unwrap(),
            before
        );

        restarted.shutdown();
        replacement.shutdown();
        restarted.wait().await.unwrap();
        replacement.wait().await.unwrap();
        assert_eq!(
            KvPersistence::open(&retired_root)
                .unwrap()
                .get("stale")
                .as_deref(),
            Some("must-not-return")
        );
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}
