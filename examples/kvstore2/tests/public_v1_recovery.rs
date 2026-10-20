#![cfg(feature = "testing")]

use std::sync::Arc;
use std::time::Duration;

use kuberic_runtime::engine::DurableState;
use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
};
use kuberic_runtime::protocol::public_operations::{
    PossibleDataLossIntent, ProcessSessionRenewalRequired, PublicBuildAttemptReference,
    PublicBuildInput, PublicCatchUpMode, PublicConfiguration, PublicInboundBuildAuthority,
    PublicLifecycleInput, PublicLifecycleRecipe, PublicOperationClass, PublicOperationIntent,
    PublicOperationProgram, PublicOperationReplayDisposition, PublicReplicaDescriptor,
    PublicV1Identity, PublicV1LifecycleBinding, StatePersistence,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, AgentGeneration, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy,
    Epoch, InitializationId, OperationId, PodUid, ProcessSessionEndpoint,
    ProcessSessionRenewalReason, PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity, ReplicaStorageIncarnationContext,
    ReplicatorCreationId, ResourceUid, StorageGeneration, derive_bootstrap_history,
    derive_history_initialization_id,
};
use kuberic_runtime::testing::sqlite_store::SqliteStore;
use kuberic_runtime::testing::state::{SCHEMA_VERSION, StorageIdentity};
use kvstore2::persistence::KvPersistence;
use kvstore2::service::KvService;
use kvstore2::state::CopyGate;
use kvstore2::v1_test_bridge::{Assignment, Committer};

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

fn test_root(name: &str) -> std::path::PathBuf {
    let current = std::env::current_dir().unwrap();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| {
            current
                .ancestors()
                .find(|path| path.join("Cargo.toml").is_file() && path.join("Cargo.lock").is_file())
                .unwrap_or(&current)
                .join("target")
        });
    target.join(name).join(std::process::id().to_string())
}

fn application_paths(
    root: &std::path::Path,
) -> std::collections::BTreeMap<String, std::path::PathBuf> {
    std::collections::BTreeMap::from([("application".into(), root.join("application"))])
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
        // These journeys bootstrap/recover a singleton; later native configurations
        // independently require two-replica ACKs before admitting committed writes.
        effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
    }
}

fn intent(
    preview: &PublicV1Identity,
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
        identity: preview.clone(),
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

fn lifecycle_intent(
    preview: &PublicV1Identity,
    session: kuberic_runtime::protocol::types::ProcessSessionId,
    revision: u64,
    name: &str,
    lifecycle: PublicLifecycleInput,
) -> PublicOperationIntent {
    PublicOperationIntent {
        identity: preview.clone(),
        operation_id: OperationId::new(name),
        revision,
        process_session_id: session,
        class: PublicOperationClass::Authority,
        input_digest: lifecycle.digest(),
        replay_disposition: PublicOperationReplayDisposition::Convergent,
        lifecycle: Some(lifecycle),
        program: None,
    }
}

#[tokio::test]
async fn kvstore2_fresh_restart_build_and_corrective_empty_replacement() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let root = test_root("kvstore2-public-v1-preview");
        let _ = std::fs::remove_dir_all(&root);
        let source_root = root.join("source");
        let replacement_root = root.join("replacement");
        let retired_root = root.join("retired-higher-progress");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&replacement_root).unwrap();

        let retired = Committer::open(retired_root.join("application"), Arc::new(|_| {})).unwrap();
        retired.promote().unwrap();
        for lsn in 1..=9 {
            let mut write = retired
                .begin_put("stale".into(), "must-not-return".into(), |_| {
                    Ok(Assignment {
                        lsn,
                        completion: Box::pin(async { Ok(()) }),
                    })
                })
                .unwrap();
            assert_eq!(write.wait().await.unwrap(), lsn);
        }
        retired.close().await.unwrap();
        drop(retired);

        let resource_uid = ResourceUid::new("kvstore2-public-v1-preview");
        let preview = PublicV1Identity::new(51);
        let binding = PublicV1LifecycleBinding {
            identity: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let source_identity = identity(1, "kv-source");
        let divergent_identity = identity(2, "kv-divergent");
        let replacement_identity = identity(2, "kv-replacement");
        let source_storage = storage(source_identity.clone(), "kv-source");
        let divergent_storage = storage(divergent_identity.clone(), "kv-divergent");
        let replacement_storage = storage(replacement_identity.clone(), "kv-replacement");
        for (path, identity, name) in [
            (&source_root, source_identity.clone(), "kv-source"),
            (&retired_root, divergent_identity.clone(), "kv-divergent"),
            (
                &replacement_root,
                replacement_identity.clone(),
                "kv-replacement",
            ),
        ] {
            drop(
                SqliteStore::create_public_v1_with_application_paths(
                    SqliteStore::metadata_database_path(path),
                    preview.clone(),
                    store_identity(resource_uid.clone(), identity, name),
                    application_paths(path),
                )
                .unwrap(),
            );
        }

        let source_address = address();
        let divergent_address = address();
        let replacement_address = address();
        let source_persistence =
            Arc::new(KvPersistence::open(source_root.join("application")).unwrap());
        let replacement_persistence =
            Arc::new(KvPersistence::open(replacement_root.join("application")).unwrap());
        let divergent_persistence =
            Arc::new(KvPersistence::open(retired_root.join("application")).unwrap());
        let copy_gate = CopyGate::enabled(source_root.join("copy-gate")).unwrap();
        let source_service = Arc::new(KvService::new(
            source_persistence.clone(),
            format!("http://{source_address}"),
            copy_gate.clone(),
        ));
        let replacement_service = Arc::new(KvService::new(
            replacement_persistence.clone(),
            format!("http://{replacement_address}"),
            CopyGate::disabled(),
        ));
        let divergent_service = Arc::new(KvService::new(
            divergent_persistence.clone(),
            format!("http://{divergent_address}"),
            CopyGate::disabled(),
        ));
        let mut source = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("kv-source-pod"),
                pvc_uid: PvcUid::new("kv-source-pvc"),
                data_root: source_root.clone(),
                replication_address: source_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            source_service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_application_storage_paths(application_paths(&source_root))
        .with_public_v1_binding(binding.clone())
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let mut replacement = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("kv-replacement-pod"),
                pvc_uid: PvcUid::new("kv-replacement-pvc"),
                data_root: replacement_root.clone(),
                replication_address: replacement_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            replacement_service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_application_storage_paths(application_paths(&replacement_root))
        .with_public_v1_binding(binding.clone())
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let mut divergent = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("kv-divergent-pod"),
                pvc_uid: PvcUid::new("kv-divergent-pvc"),
                data_root: retired_root.clone(),
                replication_address: divergent_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            divergent_service.clone(),
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_application_storage_paths(application_paths(&retired_root))
        .with_public_v1_binding(binding.clone())
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let source_handle = source.handle();
        let divergent_handle = divergent.handle();
        let replacement_handle = replacement.handle();
        let generation = PublicProtocolGeneration::new(51);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        source_handle
            .initialize_public_v1_history(&initialization, &history, &source_storage)
            .await
            .unwrap();
        replacement_handle
            .initialize_public_v1_history(&initialization, &history, &replacement_storage)
            .await
            .unwrap();
        divergent_handle
            .initialize_public_v1_history(&initialization, &history, &divergent_storage)
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
                    existing: false,
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
                    current: source_configuration.clone(),
                    previous: None,
                    replicas: Vec::new(),
                }),
            ),
        ] {
            source_handle
                .execute_public_v1(intent(
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
                "kv-divergent-open",
                PublicOperationProgram::Open {
                    replica: divergent_identity.clone(),
                    existing: false,
                },
            ),
            (
                2,
                "kv-divergent-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
        ] {
            divergent_handle
                .execute_public_v1(intent(
                    &preview,
                    divergent_handle.process_session_id(),
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
        let divergent_build = PublicBuildInput {
            attempt: OperationId::new("kv-divergent-build"),
            source_address: format!("http://{source_address}"),
            epoch,
            current_configuration_id: Some(source_configuration.configuration_id.clone()),
            replica: divergent_identity.clone(),
            target_storage: divergent_storage.clone(),
            process_session_id: divergent_handle.process_session_id(),
            replication_address: format!("http://{divergent_address}"),
            history: Some(history.clone()),
        };
        divergent_handle
            .install_public_v1_inbound_build_authority(&PublicInboundBuildAuthority {
                operation_id: OperationId::new("kv-divergent-build"),
                source_address: divergent_build.source_address.clone(),
                operation_revision: 4,
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
        let divergent_attempt = divergent_build.attempt.clone();
        let divergent_build_observer = tokio::spawn({
            let handle = source_handle.clone();
            let preview = preview.clone();
            let build = divergent_build.clone();
            async move {
                handle
                    .execute_public_v1(intent(
                        &preview,
                        handle.process_session_id(),
                        4,
                        "kv-divergent-build",
                        PublicOperationProgram::Build(build),
                    ))
                    .await
            }
        });
        // Native copy rejection belongs to the target service, not a source-side
        // persistence preflight. It must remain rejected through explicit retirement.
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if divergent_service
                    .replicate_put("must-not-write".into(), "bad".into())
                    .await
                    .is_err_and(|error| {
                        error
                            .to_string()
                            .contains("invalid or regressing native KV snapshot")
                    })
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the target service did not reject its regressing native checkpoint");
        assert!(
            divergent_service
                .shutdown_and_drain()
                .await
                .unwrap_err()
                .to_string()
                .contains("invalid or regressing native KV snapshot")
        );
        assert!(
            !source_handle
                .public_v1_report()
                .await
                .unwrap()
                .completed_operations
                .iter()
                .any(|completion| {
                    completion.operation_id == divergent_attempt
                        && matches!(
                            completion.result,
                            kuberic_runtime::protocol::public_operations::
                                PublicV1OperationResult::Succeeded
                        )
                }),
            "a rejected native checkpoint must never complete the build successfully"
        );
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                4,
                "kv-retire-divergent-build",
                PublicOperationProgram::Remove(divergent_build),
            ))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(15), divergent_build_observer)
                .await
                .expect("retirement did not settle the failed native build observer")
                .unwrap()
                .is_err()
        );
        divergent.shutdown();
        divergent.wait().await.unwrap();
        assert_eq!(
            divergent_persistence
                .durable_progress()
                .await
                .unwrap()
                .committed_lsn,
            9
        );
        let source_predecessor = source_handle
            .public_v1_process_endpoint(ReplicaRole::Primary)
            .await
            .unwrap();
        source.shutdown();
        source.wait().await.unwrap();
        let source_service = Arc::new(KvService::new(
            source_persistence.clone(),
            format!("http://{source_address}"),
            copy_gate.clone(),
        ));
        let mut source = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("kv-source-pod"),
                pvc_uid: PvcUid::new("kv-source-pvc"),
                data_root: source_root.clone(),
                replication_address: source_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            source_service.clone(),
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_application_storage_paths(application_paths(&source_root))
        .with_public_v1_binding(binding.clone())
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
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
            .authorize_public_v1_session_renewal(ProcessSessionRenewalRequired {
                operation_id: OperationId::new("kv-source-after-divergent-retirement"),
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
                        "kv-source-after-divergent-retirement-creation",
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
                "kv-source-after-divergent-open",
                PublicOperationProgram::Open {
                    replica: source_identity.clone(),
                    existing: true,
                },
            ),
            (
                6,
                "kv-source-after-divergent-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::Primary,
                },
            ),
            (
                7,
                "kv-source-after-divergent-epoch",
                PublicOperationProgram::Epoch { epoch },
            ),
            (
                8,
                "kv-source-after-divergent-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: source_configuration.clone(),
                    previous: None,
                    replicas: Vec::new(),
                }),
            ),
        ] {
            source_handle
                .execute_public_v1(intent(
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
                    existing: false,
                },
            ),
            (
                2,
                "kv-replacement-recovery-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
        ] {
            replacement_handle
                .execute_public_v1(intent(
                    &preview,
                    replacement_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        copy_gate.hold().unwrap();
        let build_input = PublicBuildInput {
            attempt: OperationId::new("kv-corrective-build"),
            source_address: format!("http://{source_address}"),
            epoch,
            current_configuration_id: Some(source_configuration.configuration_id.clone()),
            replica: replacement_identity.clone(),
            target_storage: replacement_storage.clone(),
            process_session_id: replacement_handle.process_session_id(),
            replication_address: format!("http://{replacement_address}"),
            history: Some(history.clone()),
        };
        replacement_handle
            .install_public_v1_inbound_build_authority(&PublicInboundBuildAuthority {
                operation_id: OperationId::new("kv-corrective-build"),
                source_address: build_input.source_address.clone(),
                operation_revision: 8,
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
        let mut build = tokio::spawn({
            let handle = source_handle.clone();
            let preview = preview.clone();
            let input = build_input.clone();
            async move {
                handle
                    .execute_public_v1(intent(
                        &preview,
                        handle.process_session_id(),
                        8,
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
            .execute_public_v1(intent(
                &preview,
                replacement_handle.process_session_id(),
                6,
                "kv-replacement-admit-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::ActiveSecondary,
                },
            ))
            .await
            .unwrap();
        replacement_handle
            .execute_public_v1(intent(
                &preview,
                replacement_handle.process_session_id(),
                7,
                "kv-replacement-admit-configuration",
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
                        current_progress: 2,
                        catch_up_capability: 2,
                        must_catchup: false,
                    }],
                }),
            ))
            .await
            .unwrap();
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                9,
                "kv-source-admit-configuration",
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
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if replacement_persistence
                    .durable_progress()
                    .await
                    .unwrap()
                    .committed_lsn
                    == 3
                    && replacement_persistence.get("after-build").as_deref() == Some("v3")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement did not observe committed post-build write");
        assert!(matches!(
            replacement_service.get("after-build").await,
            Err(kuberic_runtime::RuntimeError::ReadClosed(
                AccessStatus::NotPrimary
            ))
        ));
        assert_eq!(
            source_service.get("after-build").await.unwrap().as_deref(),
            Some("v3")
        );
        let replacement_predecessor = replacement_handle
            .public_v1_process_endpoint(ReplicaRole::ActiveSecondary)
            .await
            .unwrap();
        replacement.shutdown();
        replacement.wait().await.unwrap();
        let replacement_restart_address = address();
        let replacement_restart_persistence =
            Arc::new(KvPersistence::open(replacement_root.join("application")).unwrap());
        let replacement_restart_service = Arc::new(KvService::new(
            replacement_restart_persistence.clone(),
            format!("http://{replacement_restart_address}"),
            CopyGate::disabled(),
        ));
        let mut restarted_replacement = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("kv-replacement-pod"),
                pvc_uid: PvcUid::new("kv-replacement-pvc"),
                data_root: replacement_root.clone(),
                replication_address: replacement_restart_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            replacement_restart_service.clone(),
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_application_storage_paths(application_paths(&replacement_root))
        .with_public_v1_binding(binding)
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let replacement_restart_handle = restarted_replacement.handle();
        let restart_epoch = Epoch::new(0, 3);
        let restart_configuration = ConfigurationDescriptor::new(
            restart_epoch,
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
            .authorize_public_v1_session_renewal(ProcessSessionRenewalRequired {
                operation_id: OperationId::new("kv-replacement-restart"),
                reason: ProcessSessionRenewalReason::ActiveSecondaryReplacement,
                resource_uid: resource_uid.clone(),
                history: history.clone(),
                previous_epoch: epoch,
                renewal_epoch: restart_epoch,
                predecessor: replacement_predecessor,
                replacement: Some(ProcessSessionEndpoint {
                    identity: replacement_identity.clone(),
                    storage_incarnation: replacement_storage.clone(),
                    process_session_id: replacement_restart_handle.process_session_id(),
                    replicator_creation_id: ReplicatorCreationId::new(
                        "kv-replacement-restart-creation",
                    ),
                    role: ReplicaRole::None,
                }),
                surviving_sessions: vec![
                    source_handle
                        .public_v1_process_endpoint(ReplicaRole::Primary)
                        .await
                        .unwrap(),
                ],
                fresh_configuration: restart_configuration.clone(),
                predecessor_contained: true,
                data_loss: None,
            })
            .await
            .unwrap();
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                10,
                "kv-source-replacement-restart-epoch",
                PublicOperationProgram::Epoch {
                    epoch: restart_epoch,
                },
            ))
            .await
            .unwrap();
        let restart_source_only = ConfigurationDescriptor::new(
            restart_epoch,
            source_identity.replica_id,
            vec![ConfigurationMember {
                identity: source_identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                11,
                "kv-source-replacement-restart-bootstrap",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: restart_source_only.clone(),
                    previous: None,
                    replicas: Vec::new(),
                }),
            ))
            .await
            .unwrap();
        for (revision, name, program) in [
            (
                8,
                "kv-replacement-restart-open",
                PublicOperationProgram::Open {
                    replica: replacement_identity.clone(),
                    existing: true,
                },
            ),
            (
                9,
                "kv-replacement-restart-role",
                PublicOperationProgram::Role {
                    epoch: restart_epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
        ] {
            replacement_restart_handle
                .execute_public_v1(intent(
                    &preview,
                    replacement_restart_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        let restart_build = PublicBuildInput {
            attempt: OperationId::new("kv-replacement-restart-build"),
            source_address: format!("http://{source_address}"),
            epoch: restart_epoch,
            current_configuration_id: Some(restart_source_only.configuration_id.clone()),
            replica: replacement_identity.clone(),
            target_storage: replacement_storage.clone(),
            process_session_id: replacement_restart_handle.process_session_id(),
            replication_address: format!("http://{replacement_restart_address}"),
            history: Some(history.clone()),
        };
        replacement_restart_handle
            .install_public_v1_inbound_build_authority(&PublicInboundBuildAuthority {
                operation_id: restart_build.attempt.clone(),
                operation_revision: 11,
                attempt: PublicBuildAttemptReference {
                    resource_uid: resource_uid.clone(),
                    epoch: restart_epoch,
                    build_id: restart_build.attempt.clone(),
                    source: source_identity.clone(),
                    source_storage: source_storage.clone(),
                    source_session: source_handle.process_session_id(),
                    target: replacement_identity.clone(),
                    target_storage: replacement_storage.clone(),
                    target_session: replacement_restart_handle.process_session_id(),
                },
                history: history.clone(),
                current_configuration_id: restart_source_only.configuration_id.clone(),
                source_address: restart_build.source_address.clone(),
                target_address: restart_build.replication_address.clone(),
            })
            .await
            .unwrap();
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                11,
                restart_build.attempt.as_str(),
                PublicOperationProgram::Build(restart_build.clone()),
            ))
            .await
            .unwrap();
        for (revision, name, program) in [
            (
                10,
                "kv-replacement-restart-admit",
                PublicOperationProgram::Role {
                    epoch: restart_epoch,
                    role: ReplicaRole::ActiveSecondary,
                },
            ),
            (
                11,
                "kv-replacement-restart-epoch",
                PublicOperationProgram::Epoch {
                    epoch: restart_epoch,
                },
            ),
        ] {
            replacement_restart_handle
                .execute_public_v1(intent(
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
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                12,
                "kv-source-replacement-restart-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: restart_configuration.clone(),
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: replacement_identity.clone(),
                        replication_address: format!("http://{replacement_restart_address}"),
                        process_session_id: replacement_restart_handle.process_session_id(),
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
        replacement_restart_handle
            .execute_public_v1(intent(
                &preview,
                replacement_restart_handle.process_session_id(),
                12,
                "kv-replacement-restart-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: restart_configuration.clone(),
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
        assert_eq!(
            replacement_restart_persistence.get("snapshot").as_deref(),
            Some("v1")
        );
        assert_eq!(
            replacement_restart_persistence.get("retained").as_deref(),
            Some("v2")
        );
        assert_eq!(replacement_restart_persistence.get("stale"), None);
        assert_eq!(
            source_service
                .replicate_put("after-restart".into(), "v4".into())
                .await
                .unwrap(),
            4
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if replacement_restart_persistence
                    .get("after-restart")
                    .as_deref()
                    == Some("v4")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("restarted replacement did not receive committed write");
        let readmit = ConfigurationDescriptor::new(
            restart_epoch,
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
                .execute_public_v1(intent(
                    &preview,
                    source_handle.process_session_id(),
                    13,
                    "kv-reject-retired-divergent-after-restart",
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
                                current_progress: 4,
                                catch_up_capability: 4,
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
            KvPersistence::open(retired_root.join("application"))
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

#[tokio::test]
async fn kvstore2_real_provider_restart_settlement_and_handoff() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let root = test_root("kvstore2-public-v1-composite");
        let _ = std::fs::remove_dir_all(&root);
        let source_root = root.join("source");
        let secondary_root = root.join("secondary");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&secondary_root).unwrap();

        let resource_uid = ResourceUid::new("kvstore2-public-v1-composite");
        let preview = PublicV1Identity::new(51);
        let binding = PublicV1LifecycleBinding {
            identity: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let source_identity = identity(1, "kv-composite-source");
        let secondary_identity = identity(2, "kv-composite-secondary");
        let source_storage = storage(source_identity.clone(), "kv-composite-source");
        let secondary_storage = storage(secondary_identity.clone(), "kv-composite-secondary");
        for (path, identity, name) in [
            (&source_root, source_identity.clone(), "kv-composite-source"),
            (
                &secondary_root,
                secondary_identity.clone(),
                "kv-composite-secondary",
            ),
        ] {
            drop(
                SqliteStore::create_public_v1_with_application_paths(
                    SqliteStore::metadata_database_path(path),
                    preview.clone(),
                    store_identity(resource_uid.clone(), identity, name),
                    application_paths(path),
                )
                .unwrap(),
            );
        }

        let source_address = address();
        let secondary_address = address();
        let source_persistence =
            Arc::new(KvPersistence::open(source_root.join("application")).unwrap());
        let secondary_persistence =
            Arc::new(KvPersistence::open(secondary_root.join("application")).unwrap());
        let source_service = Arc::new(KvService::new(
            source_persistence.clone(),
            format!("http://{source_address}"),
            CopyGate::disabled(),
        ));
        let secondary_service = Arc::new(KvService::new(
            secondary_persistence.clone(),
            format!("http://{secondary_address}"),
            CopyGate::disabled(),
        ));
        let mut source = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("kv-composite-source-pod"),
                pvc_uid: PvcUid::new("kv-composite-source-pvc"),
                data_root: source_root.clone(),
                replication_address: source_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            source_service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_application_storage_paths(application_paths(&source_root))
        .with_public_v1_binding(binding.clone())
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let mut secondary = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("kv-composite-secondary-pod"),
                pvc_uid: PvcUid::new("kv-composite-secondary-pvc"),
                data_root: secondary_root.clone(),
                replication_address: secondary_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            secondary_service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_application_storage_paths(application_paths(&secondary_root))
        .with_public_v1_binding(binding.clone())
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let source_handle = source.handle();
        let secondary_handle = secondary.handle();
        let generation = PublicProtocolGeneration::new(51);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        source_handle
            .initialize_public_v1_history(&initialization, &history, &source_storage)
            .await
            .unwrap();
        secondary_handle
            .initialize_public_v1_history(&initialization, &history, &secondary_storage)
            .await
            .unwrap();

        let initial_epoch = Epoch::new(0, 1);
        let initial_configuration = ConfigurationDescriptor::new(
            initial_epoch,
            source_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source_identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: secondary_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        let source_only = ConfigurationDescriptor::new(
            initial_epoch,
            source_identity.replica_id,
            vec![ConfigurationMember {
                identity: source_identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        for (handle, identity, role) in [
            (
                &secondary_handle,
                secondary_identity.clone(),
                ReplicaRole::IdleSecondary,
            ),
            (
                &source_handle,
                source_identity.clone(),
                ReplicaRole::Primary,
            ),
        ] {
            for (revision, name, program) in [
                (
                    1,
                    format!("{}-open", identity.instance_id),
                    PublicOperationProgram::Open {
                        replica: identity.clone(),
                        existing: false,
                    },
                ),
                (
                    2,
                    format!("{}-role", identity.instance_id),
                    PublicOperationProgram::Role {
                        epoch: initial_epoch,
                        role,
                    },
                ),
                (
                    3,
                    format!("{}-epoch", identity.instance_id),
                    PublicOperationProgram::Epoch {
                        epoch: initial_epoch,
                    },
                ),
            ] {
                if role == ReplicaRole::IdleSecondary
                    && matches!(program, PublicOperationProgram::Epoch { .. })
                {
                    continue;
                }
                handle
                    .execute_public_v1(intent(
                        &preview,
                        handle.process_session_id(),
                        revision,
                        &name,
                        program,
                    ))
                    .await
                    .unwrap();
            }
        }
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                4,
                "kv-composite-source-bootstrap",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: source_only.clone(),
                    previous: None,
                    replicas: Vec::new(),
                }),
            ))
            .await
            .unwrap();
        let build = PublicBuildInput {
            attempt: OperationId::new("kv-composite-initial-build"),
            source_address: format!("http://{source_address}"),
            epoch: initial_epoch,
            current_configuration_id: Some(source_only.configuration_id.clone()),
            replica: secondary_identity.clone(),
            target_storage: secondary_storage.clone(),
            process_session_id: secondary_handle.process_session_id(),
            replication_address: format!("http://{secondary_address}"),
            history: Some(history.clone()),
        };
        secondary_handle
            .install_public_v1_inbound_build_authority(&PublicInboundBuildAuthority {
                operation_id: build.attempt.clone(),
                operation_revision: 4,
                attempt: PublicBuildAttemptReference {
                    resource_uid: resource_uid.clone(),
                    epoch: initial_epoch,
                    build_id: build.attempt.clone(),
                    source: source_identity.clone(),
                    source_storage: source_storage.clone(),
                    source_session: source_handle.process_session_id(),
                    target: secondary_identity.clone(),
                    target_storage: secondary_storage.clone(),
                    target_session: secondary_handle.process_session_id(),
                },
                history: history.clone(),
                current_configuration_id: source_only.configuration_id.clone(),
                source_address: build.source_address.clone(),
                target_address: build.replication_address.clone(),
            })
            .await
            .unwrap();
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                4,
                build.attempt.as_str(),
                PublicOperationProgram::Build(build.clone()),
            ))
            .await
            .unwrap();
        secondary_handle
            .execute_public_v1(intent(
                &preview,
                secondary_handle.process_session_id(),
                4,
                "kv-composite-secondary-admit",
                PublicOperationProgram::Role {
                    epoch: initial_epoch,
                    role: ReplicaRole::ActiveSecondary,
                },
            ))
            .await
            .unwrap();
        for (handle, name, peer, peer_address, peer_session, peer_storage, peer_role) in [
            (
                &secondary_handle,
                "kv-composite-secondary-configuration",
                source_identity.clone(),
                source_address,
                source_handle.process_session_id(),
                source_storage.clone(),
                ReplicaRole::Primary,
            ),
            (
                &source_handle,
                "kv-composite-source-configuration",
                secondary_identity.clone(),
                secondary_address,
                secondary_handle.process_session_id(),
                secondary_storage.clone(),
                ReplicaRole::ActiveSecondary,
            ),
        ] {
            handle
                .execute_public_v1(intent(
                    &preview,
                    handle.process_session_id(),
                    5,
                    name,
                    PublicOperationProgram::Configuration(PublicConfiguration {
                        current: initial_configuration.clone(),
                        previous: None,
                        replicas: vec![PublicReplicaDescriptor {
                            build_id: OperationId::default(),
                            identity: peer,
                            replication_address: format!("http://{peer_address}"),
                            process_session_id: peer_session,
                            storage_incarnation: Some(peer_storage),
                            role: peer_role,
                            current_progress: 0,
                            catch_up_capability: 0,
                            must_catchup: false,
                        }],
                    }),
                ))
                .await
                .unwrap();
        }
        assert_eq!(
            source_service
                .replicate_put("baseline".into(), "v1".into())
                .await
                .unwrap(),
            1
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while secondary_persistence
                .durable_progress()
                .await
                .unwrap()
                .committed_lsn
                < 1
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("baseline did not reach the real secondary provider");
        let source_predecessor = source_handle
            .public_v1_process_endpoint(ReplicaRole::Primary)
            .await
            .unwrap();
        let secondary_predecessor = secondary_handle
            .public_v1_process_endpoint(ReplicaRole::ActiveSecondary)
            .await
            .unwrap();
        source.shutdown();
        secondary.shutdown();
        source.wait().await.unwrap();
        secondary.wait().await.unwrap();

        // Only native preparations are speculative. Reopen must discard them;
        // host catch-up cannot publish an application-local uncommitted suffix.
        for path in [&source_root, &secondary_root] {
            let committer = Committer::open(path.join("application"), Arc::new(|_| {})).unwrap();
            committer.promote().unwrap();
            let mut unsettled = committer
                .begin_put("unsettled".into(), "v2".into(), |_| {
                    Ok(Assignment {
                        lsn: 2,
                        completion: Box::pin(std::future::pending()),
                    })
                })
                .unwrap();
            committer.abort();
            assert!(unsettled.wait().await.is_err());
            assert!(committer.close().await.is_err());
        }
        let source_unsettled = KvPersistence::open(source_root.join("application")).unwrap();
        let secondary_unsettled = KvPersistence::open(secondary_root.join("application")).unwrap();
        assert_eq!(
            source_unsettled.durable_progress().await.unwrap(),
            kuberic_runtime::application::DurableApplicationProgress {
                applied_lsn: 2,
                committed_lsn: 1,
            }
        );
        assert_eq!(
            secondary_unsettled.durable_progress().await.unwrap(),
            kuberic_runtime::application::DurableApplicationProgress {
                applied_lsn: 2,
                committed_lsn: 1,
            }
        );
        drop(source_service);
        drop(secondary_service);
        drop(source_persistence);
        drop(secondary_persistence);
        drop(source_unsettled);
        drop(secondary_unsettled);

        let source_address = address();
        let secondary_address = address();
        let source_persistence =
            Arc::new(KvPersistence::open(source_root.join("application")).unwrap());
        let secondary_persistence =
            Arc::new(KvPersistence::open(secondary_root.join("application")).unwrap());
        let source_service = Arc::new(KvService::new(
            source_persistence.clone(),
            format!("http://{source_address}"),
            CopyGate::disabled(),
        ));
        let secondary_service = Arc::new(KvService::new(
            secondary_persistence.clone(),
            format!("http://{secondary_address}"),
            CopyGate::disabled(),
        ));
        let mut source = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("kv-composite-source-pod"),
                pvc_uid: PvcUid::new("kv-composite-source-pvc"),
                data_root: source_root.clone(),
                replication_address: source_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            source_service.clone(),
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_application_storage_paths(application_paths(&source_root))
        .with_public_v1_binding(binding.clone())
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let mut secondary = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("kv-composite-secondary-pod"),
                pvc_uid: PvcUid::new("kv-composite-secondary-pvc"),
                data_root: secondary_root.clone(),
                replication_address: secondary_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            secondary_service.clone(),
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_application_storage_paths(application_paths(&secondary_root))
        .with_public_v1_binding(binding)
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let source_handle = source.handle();
        let secondary_handle = secondary.handle();
        assert!(source_service.get("unsettled").await.is_err());
        assert!(secondary_service.get("unsettled").await.is_err());

        let restart_epoch = Epoch::new(0, 2);
        let source_only_configuration = ConfigurationDescriptor::new(
            restart_epoch,
            source_identity.replica_id,
            vec![ConfigurationMember {
                identity: source_identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        source_handle
            .authorize_public_v1_session_renewal(ProcessSessionRenewalRequired {
                operation_id: OperationId::new("kv-composite-renew-source"),
                reason: ProcessSessionRenewalReason::PrimaryReplacement,
                resource_uid: resource_uid.clone(),
                history: history.clone(),
                previous_epoch: initial_epoch,
                renewal_epoch: restart_epoch,
                predecessor: source_predecessor,
                replacement: Some(ProcessSessionEndpoint {
                    identity: source_identity.clone(),
                    storage_incarnation: source_storage.clone(),
                    process_session_id: source_handle.process_session_id(),
                    replicator_creation_id: ReplicatorCreationId::new(
                        "kv-composite-renew-source-creation",
                    ),
                    role: ReplicaRole::None,
                }),
                surviving_sessions: Vec::new(),
                fresh_configuration: source_only_configuration.clone(),
                predecessor_contained: true,
                data_loss: None,
            })
            .await
            .unwrap();
        for (revision, name, program) in [
            (
                6,
                "kv-composite-source-reopen",
                PublicOperationProgram::Open {
                    replica: source_identity.clone(),
                    existing: true,
                },
            ),
            (
                7,
                "kv-composite-source-rerole",
                PublicOperationProgram::Role {
                    epoch: restart_epoch,
                    role: ReplicaRole::Primary,
                },
            ),
            (
                8,
                "kv-composite-source-reepoch",
                PublicOperationProgram::Epoch {
                    epoch: restart_epoch,
                },
            ),
        ] {
            source_handle
                .execute_public_v1(intent(
                    &preview,
                    source_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        assert_ne!(
            source_handle.public_v1_report().await.unwrap().write_status,
            AccessStatus::Granted
        );
        assert!(source_service.get("unsettled").await.is_err());
        assert!(
            source_service
                .replicate_put("must-stay-closed".into(), "bad".into())
                .await
                .is_err()
        );
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                9,
                "kv-composite-source-rebootstrap",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: source_only_configuration.clone(),
                    previous: None,
                    replicas: Vec::new(),
                }),
            ))
            .await
            .unwrap();
        let renewed_source = source_handle
            .public_v1_process_endpoint(ReplicaRole::Primary)
            .await
            .unwrap();
        let restart_configuration = ConfigurationDescriptor::new(
            restart_epoch,
            source_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source_identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: secondary_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        secondary_handle
            .authorize_public_v1_session_renewal(ProcessSessionRenewalRequired {
                operation_id: OperationId::new("kv-composite-renew-secondary"),
                reason: ProcessSessionRenewalReason::ActiveSecondaryReplacement,
                resource_uid: resource_uid.clone(),
                history: history.clone(),
                previous_epoch: initial_epoch,
                renewal_epoch: restart_epoch,
                predecessor: secondary_predecessor,
                replacement: Some(ProcessSessionEndpoint {
                    identity: secondary_identity.clone(),
                    storage_incarnation: secondary_storage.clone(),
                    process_session_id: secondary_handle.process_session_id(),
                    replicator_creation_id: ReplicatorCreationId::new(
                        "kv-composite-renew-secondary-creation",
                    ),
                    role: ReplicaRole::None,
                }),
                surviving_sessions: vec![renewed_source],
                fresh_configuration: restart_configuration.clone(),
                predecessor_contained: true,
                data_loss: None,
            })
            .await
            .unwrap();
        for (revision, name, program) in [
            (
                6,
                "kv-composite-secondary-reopen",
                PublicOperationProgram::Open {
                    replica: secondary_identity.clone(),
                    existing: true,
                },
            ),
            (
                7,
                "kv-composite-secondary-rerole",
                PublicOperationProgram::Role {
                    epoch: restart_epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
        ] {
            secondary_handle
                .execute_public_v1(intent(
                    &preview,
                    secondary_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        for persistence in [&source_persistence, &secondary_persistence] {
            assert_eq!(
                persistence.durable_progress().await.unwrap(),
                kuberic_runtime::application::DurableApplicationProgress {
                    applied_lsn: 1,
                    committed_lsn: 1,
                }
            );
            assert_eq!(persistence.get("baseline").as_deref(), Some("v1"));
            assert_eq!(persistence.get("unsettled"), None);
        }
        let rebuild = PublicBuildInput {
            attempt: OperationId::new("kv-composite-restart-build"),
            source_address: format!("http://{source_address}"),
            epoch: restart_epoch,
            current_configuration_id: Some(source_only_configuration.configuration_id.clone()),
            replica: secondary_identity.clone(),
            target_storage: secondary_storage.clone(),
            process_session_id: secondary_handle.process_session_id(),
            replication_address: format!("http://{secondary_address}"),
            history: Some(history.clone()),
        };
        secondary_handle
            .install_public_v1_inbound_build_authority(&PublicInboundBuildAuthority {
                operation_id: rebuild.attempt.clone(),
                operation_revision: 9,
                attempt: PublicBuildAttemptReference {
                    resource_uid: resource_uid.clone(),
                    epoch: restart_epoch,
                    build_id: rebuild.attempt.clone(),
                    source: source_identity.clone(),
                    source_storage: source_storage.clone(),
                    source_session: source_handle.process_session_id(),
                    target: secondary_identity.clone(),
                    target_storage: secondary_storage.clone(),
                    target_session: secondary_handle.process_session_id(),
                },
                history: history.clone(),
                current_configuration_id: source_only_configuration.configuration_id.clone(),
                source_address: rebuild.source_address.clone(),
                target_address: rebuild.replication_address.clone(),
            })
            .await
            .unwrap();
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                9,
                rebuild.attempt.as_str(),
                PublicOperationProgram::Build(rebuild.clone()),
            ))
            .await
            .unwrap();
        secondary_handle
            .execute_public_v1(intent(
                &preview,
                secondary_handle.process_session_id(),
                9,
                "kv-composite-secondary-readmit",
                PublicOperationProgram::Role {
                    epoch: restart_epoch,
                    role: ReplicaRole::ActiveSecondary,
                },
            ))
            .await
            .unwrap();
        let source_configuration = PublicConfiguration {
            current: restart_configuration.clone(),
            previous: Some(source_only_configuration.clone()),
            replicas: vec![PublicReplicaDescriptor {
                build_id: OperationId::default(),
                identity: secondary_identity.clone(),
                replication_address: format!("http://{secondary_address}"),
                process_session_id: secondary_handle.process_session_id(),
                storage_incarnation: Some(secondary_storage.clone()),
                role: ReplicaRole::ActiveSecondary,
                current_progress: 1,
                catch_up_capability: 1,
                must_catchup: true,
            }],
        };
        let secondary_configuration = PublicConfiguration {
            current: restart_configuration.clone(),
            previous: Some(source_only_configuration),
            replicas: vec![PublicReplicaDescriptor {
                build_id: OperationId::default(),
                identity: source_identity.clone(),
                replication_address: format!("http://{source_address}"),
                process_session_id: source_handle.process_session_id(),
                storage_incarnation: Some(source_storage.clone()),
                role: ReplicaRole::Primary,
                current_progress: 1,
                catch_up_capability: 1,
                must_catchup: false,
            }],
        };
        secondary_handle
            .execute_public_v1(intent(
                &preview,
                secondary_handle.process_session_id(),
                10,
                "kv-composite-secondary-reconfiguration",
                PublicOperationProgram::Configuration(secondary_configuration),
            ))
            .await
            .unwrap();
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                10,
                "kv-composite-source-reconfiguration",
                PublicOperationProgram::Configuration(source_configuration.clone()),
            ))
            .await
            .unwrap();
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                11,
                "kv-composite-settle-suffix",
                PublicOperationProgram::CatchUp {
                    configuration: source_configuration,
                    mode: PublicCatchUpMode::WriteQuorum,
                },
            ))
            .await
            .unwrap();
        assert_eq!(
            source_persistence
                .durable_progress()
                .await
                .unwrap()
                .committed_lsn,
            1
        );
        assert_eq!(
            secondary_persistence
                .durable_progress()
                .await
                .unwrap()
                .committed_lsn,
            1
        );
        assert_eq!(
            source_service.get("unsettled").await.unwrap().as_deref(),
            None
        );
        assert_eq!(
            source_service
                .replicate_put("unsettled".into(), "v2".into())
                .await
                .unwrap(),
            2
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while secondary_persistence
                .durable_progress()
                .await
                .unwrap()
                .committed_lsn
                < 2
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("re-admitted write did not reach the rebuilt secondary");
        assert_eq!(
            source_service.get("unsettled").await.unwrap().as_deref(),
            Some("v2")
        );
        let source_report = source_handle.public_v1_report().await.unwrap();
        let secondary_report = secondary_handle.public_v1_report().await.unwrap();
        assert_eq!(source_report.write_status, AccessStatus::Granted);
        assert_ne!(secondary_report.write_status, AccessStatus::Granted);

        let handoff_epoch = Epoch::new(0, 3);
        let handoff_configuration = ConfigurationDescriptor::new(
            handoff_epoch,
            secondary_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: secondary_identity.clone(),
                    role: ReplicaRole::Primary,
                },
            ],
            2,
        );
        for (handle, revision, name, role) in [
            (
                &source_handle,
                12,
                "kv-composite-source-handoff-role",
                ReplicaRole::ActiveSecondary,
            ),
            (
                &secondary_handle,
                11,
                "kv-composite-secondary-handoff-role",
                ReplicaRole::Primary,
            ),
        ] {
            handle
                .execute_public_v1(intent(
                    &preview,
                    handle.process_session_id(),
                    revision,
                    &format!("{name}-epoch"),
                    PublicOperationProgram::Epoch {
                        epoch: handoff_epoch,
                    },
                ))
                .await
                .unwrap();
            handle
                .execute_public_v1(intent(
                    &preview,
                    handle.process_session_id(),
                    revision + 1,
                    name,
                    PublicOperationProgram::Role {
                        epoch: handoff_epoch,
                        role,
                    },
                ))
                .await
                .unwrap();
        }
        let source_handoff = PublicConfiguration {
            current: handoff_configuration.clone(),
            previous: None,
            replicas: vec![PublicReplicaDescriptor {
                build_id: OperationId::default(),
                identity: secondary_identity.clone(),
                replication_address: format!("http://{secondary_address}"),
                process_session_id: secondary_handle.process_session_id(),
                storage_incarnation: Some(secondary_storage.clone()),
                role: ReplicaRole::Primary,
                current_progress: 2,
                catch_up_capability: 2,
                must_catchup: false,
            }],
        };
        let secondary_handoff = PublicConfiguration {
            current: handoff_configuration,
            previous: None,
            replicas: vec![PublicReplicaDescriptor {
                build_id: OperationId::default(),
                identity: source_identity.clone(),
                replication_address: format!("http://{source_address}"),
                process_session_id: source_handle.process_session_id(),
                storage_incarnation: Some(source_storage.clone()),
                role: ReplicaRole::ActiveSecondary,
                current_progress: 2,
                catch_up_capability: 2,
                must_catchup: false,
            }],
        };
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                14,
                "kv-composite-source-handoff-configuration",
                PublicOperationProgram::Configuration(source_handoff),
            ))
            .await
            .unwrap();
        secondary_handle
            .execute_public_v1(intent(
                &preview,
                secondary_handle.process_session_id(),
                13,
                "kv-composite-secondary-handoff-configuration",
                PublicOperationProgram::Configuration(secondary_handoff.clone()),
            ))
            .await
            .unwrap();
        secondary_handle
            .execute_public_v1(intent(
                &preview,
                secondary_handle.process_session_id(),
                14,
                "kv-composite-secondary-handoff-catch-up",
                PublicOperationProgram::CatchUp {
                    configuration: secondary_handoff,
                    mode: PublicCatchUpMode::WriteQuorum,
                },
            ))
            .await
            .unwrap();
        let source_report = source_handle.public_v1_report().await.unwrap();
        let secondary_report = secondary_handle.public_v1_report().await.unwrap();
        assert_ne!(source_report.write_status, AccessStatus::Granted);
        assert_eq!(secondary_report.write_status, AccessStatus::Granted);
        assert!(
            source_service
                .replicate_put("old-primary".into(), "bad".into())
                .await
                .is_err()
        );
        assert_eq!(
            secondary_service
                .replicate_put("after-handoff".into(), "v3".into())
                .await
                .unwrap(),
            3
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while source_persistence
                .durable_progress()
                .await
                .unwrap()
                .committed_lsn
                < 3
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("post-handoff write did not reach the old primary provider");
        for persistence in [&source_persistence, &secondary_persistence] {
            assert_eq!(persistence.get("baseline").as_deref(), Some("v1"));
            assert_eq!(persistence.get("unsettled").as_deref(), Some("v2"));
            assert_eq!(persistence.get("after-handoff").as_deref(), Some("v3"));
        }

        source.shutdown();
        secondary.shutdown();
        source.wait().await.unwrap();
        secondary.wait().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn kvstore2_original_primary_loss_promotes_survivor_and_fences_return() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = test_root("kvstore2-public-v1-failover");
        let _ = std::fs::remove_dir_all(&root);
        let source_root = root.join("source");
        let survivor_root = root.join("survivor");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&survivor_root).unwrap();

        let resource_uid = ResourceUid::new("kvstore2-public-v1-failover");
        let preview = PublicV1Identity::new(54);
        let binding = PublicV1LifecycleBinding {
            identity: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let source_identity = identity(1, "kv-failover-source");
        let survivor_identity = identity(2, "kv-failover-survivor");
        let source_storage = storage(source_identity.clone(), "kv-failover-source");
        let survivor_storage = storage(survivor_identity.clone(), "kv-failover-survivor");
        for (path, identity, name) in [
            (&source_root, source_identity.clone(), "kv-failover-source"),
            (
                &survivor_root,
                survivor_identity.clone(),
                "kv-failover-survivor",
            ),
        ] {
            drop(
                SqliteStore::create_public_v1_with_application_paths(
                    SqliteStore::metadata_database_path(path),
                    preview.clone(),
                    store_identity(resource_uid.clone(), identity, name),
                    application_paths(path),
                )
                .unwrap(),
            );
        }
        let bind_address = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
        let source_persistence =
            Arc::new(KvPersistence::open(source_root.join("application")).unwrap());
        let survivor_persistence =
            Arc::new(KvPersistence::open(survivor_root.join("application")).unwrap());
        let source_service = Arc::new(KvService::new(
            source_persistence,
            format!("http://{bind_address}"),
            CopyGate::disabled(),
        ));
        let survivor_service = Arc::new(KvService::new(
            survivor_persistence.clone(),
            format!("http://{bind_address}"),
            CopyGate::disabled(),
        ));
        let mut source = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("kv-failover-source-pod"),
                pvc_uid: PvcUid::new("kv-failover-source-pvc"),
                data_root: source_root.clone(),
                replication_address: bind_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            source_service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_application_storage_paths(application_paths(&source_root))
        .with_public_v1_binding(binding.clone())
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let mut survivor = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("kv-failover-survivor-pod"),
                pvc_uid: PvcUid::new("kv-failover-survivor-pvc"),
                data_root: survivor_root.clone(),
                replication_address: bind_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            survivor_service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_application_storage_paths(application_paths(&survivor_root))
        .with_public_v1_binding(binding.clone())
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let source_handle = source.handle();
        let survivor_handle = survivor.handle();
        let generation = PublicProtocolGeneration::new(54);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        source_handle
            .initialize_public_v1_history(&initialization, &history, &source_storage)
            .await
            .unwrap();
        survivor_handle
            .initialize_public_v1_history(&initialization, &history, &survivor_storage)
            .await
            .unwrap();
        for (handle, identity) in [
            (&source_handle, &source_identity),
            (&survivor_handle, &survivor_identity),
        ] {
            handle
                .execute_public_v1(intent(
                    &preview,
                    handle.process_session_id(),
                    1,
                    &format!("{}-open", identity.instance_id),
                    PublicOperationProgram::Open {
                        replica: identity.clone(),
                        existing: false,
                    },
                ))
                .await
                .unwrap();
        }
        let source_address = source_handle
            .public_v1_report()
            .await
            .unwrap()
            .replication_address
            .unwrap();
        let survivor_address = survivor_handle
            .public_v1_report()
            .await
            .unwrap()
            .replication_address
            .unwrap();
        assert_ne!(source_address, survivor_address);
        let initial_epoch = Epoch::new(0, 1);
        let initial = ConfigurationDescriptor::new(
            initial_epoch,
            source_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source_identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: survivor_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        for (handle, identity, role) in [
            (
                &source_handle,
                source_identity.clone(),
                ReplicaRole::Primary,
            ),
            (
                &survivor_handle,
                survivor_identity.clone(),
                ReplicaRole::IdleSecondary,
            ),
        ] {
            for (revision, name, program) in [
                (
                    2,
                    format!("{}-role", identity.instance_id),
                    PublicOperationProgram::Role {
                        epoch: initial_epoch,
                        role,
                    },
                ),
                (
                    3,
                    format!("{}-epoch", identity.instance_id),
                    PublicOperationProgram::Epoch {
                        epoch: initial_epoch,
                    },
                ),
            ] {
                if role == ReplicaRole::IdleSecondary
                    && matches!(program, PublicOperationProgram::Epoch { .. })
                {
                    continue;
                }
                handle
                    .execute_public_v1(intent(
                        &preview,
                        handle.process_session_id(),
                        revision,
                        &name,
                        program,
                    ))
                    .await
                    .unwrap();
            }
        }
        let source_only = ConfigurationDescriptor::new(
            initial_epoch,
            source_identity.replica_id,
            vec![ConfigurationMember {
                identity: source_identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                4,
                "kv-failover-source-bootstrap",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: source_only.clone(),
                    previous: None,
                    replicas: Vec::new(),
                }),
            ))
            .await
            .unwrap();
        let build = PublicBuildInput {
            attempt: OperationId::new("kv-failover-initial-build"),
            source_address: source_address.clone(),
            epoch: initial_epoch,
            current_configuration_id: Some(source_only.configuration_id.clone()),
            replica: survivor_identity.clone(),
            target_storage: survivor_storage.clone(),
            process_session_id: survivor_handle.process_session_id(),
            replication_address: survivor_address.clone(),
            history: Some(history.clone()),
        };
        survivor_handle
            .install_public_v1_inbound_build_authority(&PublicInboundBuildAuthority {
                operation_id: build.attempt.clone(),
                operation_revision: 4,
                attempt: PublicBuildAttemptReference {
                    resource_uid: resource_uid.clone(),
                    epoch: initial_epoch,
                    build_id: build.attempt.clone(),
                    source: source_identity.clone(),
                    source_storage: source_storage.clone(),
                    source_session: source_handle.process_session_id(),
                    target: survivor_identity.clone(),
                    target_storage: survivor_storage.clone(),
                    target_session: survivor_handle.process_session_id(),
                },
                history: history.clone(),
                current_configuration_id: source_only.configuration_id.clone(),
                source_address: build.source_address.clone(),
                target_address: build.replication_address.clone(),
            })
            .await
            .unwrap();
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                4,
                build.attempt.as_str(),
                PublicOperationProgram::Build(build.clone()),
            ))
            .await
            .unwrap();
        survivor_handle
            .execute_public_v1(intent(
                &preview,
                survivor_handle.process_session_id(),
                4,
                "kv-failover-survivor-admit",
                PublicOperationProgram::Role {
                    epoch: initial_epoch,
                    role: ReplicaRole::ActiveSecondary,
                },
            ))
            .await
            .unwrap();
        for (handle, name, peer, peer_address, peer_session, peer_storage, peer_role) in [
            (
                &survivor_handle,
                "kv-failover-survivor-configuration",
                source_identity.clone(),
                source_address.clone(),
                source_handle.process_session_id(),
                source_storage.clone(),
                ReplicaRole::Primary,
            ),
            (
                &source_handle,
                "kv-failover-source-configuration",
                survivor_identity.clone(),
                survivor_address.clone(),
                survivor_handle.process_session_id(),
                survivor_storage.clone(),
                ReplicaRole::ActiveSecondary,
            ),
        ] {
            handle
                .execute_public_v1(intent(
                    &preview,
                    handle.process_session_id(),
                    5,
                    name,
                    PublicOperationProgram::Configuration(PublicConfiguration {
                        current: initial.clone(),
                        previous: None,
                        replicas: vec![PublicReplicaDescriptor {
                            build_id: OperationId::default(),
                            identity: peer,
                            replication_address: peer_address,
                            process_session_id: peer_session,
                            storage_incarnation: Some(peer_storage),
                            role: peer_role,
                            current_progress: 0,
                            catch_up_capability: 0,
                            must_catchup: false,
                        }],
                    }),
                ))
                .await
                .unwrap();
        }
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(15),
                source_service.replicate_put("committed".into(), "v1".into()),
            )
            .await
            .expect("native quorum baseline did not commit to the survivor")
            .unwrap(),
            1
        );
        tokio::time::timeout(
            Duration::from_secs(15),
            source_handle.execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                6,
                "kv-failover-catch-up",
                PublicOperationProgram::CatchUp {
                    configuration: PublicConfiguration {
                        current: initial.clone(),
                        previous: Some(source_only),
                        replicas: vec![PublicReplicaDescriptor {
                            build_id: OperationId::default(),
                            identity: survivor_identity.clone(),
                            replication_address: survivor_address.clone(),
                            process_session_id: survivor_handle.process_session_id(),
                            storage_incarnation: Some(survivor_storage.clone()),
                            role: ReplicaRole::ActiveSecondary,
                            current_progress: 0,
                            catch_up_capability: 0,
                            must_catchup: true,
                        }],
                    },
                    mode: PublicCatchUpMode::WriteQuorum,
                },
            )),
        )
        .await
        .expect("failover baseline catchup did not complete")
        .unwrap();
        assert_eq!(survivor_persistence.get("committed").as_deref(), Some("v1"));
        source.shutdown();
        tokio::time::timeout(Duration::from_secs(15), source.wait())
            .await
            .expect("failed primary did not drain")
            .unwrap();
        assert!(
            source_service
                .replicate_put("lost-primary".into(), "bad".into())
                .await
                .is_err()
        );

        let failover_epoch = Epoch::new(0, 2);
        let failover_previous = ConfigurationDescriptor::new(
            initial_epoch,
            survivor_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: survivor_identity.clone(),
                    role: ReplicaRole::Primary,
                },
            ],
            1,
        );
        tokio::time::timeout(
            Duration::from_secs(15),
            survivor_handle.execute_public_v1(lifecycle_intent(
                &preview,
                survivor_handle.process_session_id(),
                6,
                "kv-original-primary-loss",
                PublicLifecycleInput {
                    recipe: PublicLifecycleRecipe::FailoverPromotion,
                    replica: survivor_identity.clone(),
                    epoch: failover_epoch,
                    possible_data_loss: PossibleDataLossIntent::NotPossible,
                    current: ConfigurationDescriptor::new(
                        failover_epoch,
                        survivor_identity.replica_id,
                        vec![ConfigurationMember {
                            identity: survivor_identity.clone(),
                            role: ReplicaRole::Primary,
                        }],
                        1,
                    ),
                    previous: Some(failover_previous),
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: source_identity.clone(),
                        replication_address: source_address,
                        process_session_id: source_handle.process_session_id(),
                        storage_incarnation: Some(source_storage),
                        role: ReplicaRole::ActiveSecondary,
                        current_progress: 1,
                        catch_up_capability: 1,
                        must_catchup: false,
                    }],
                    data_loss_recovery: None,
                },
            )),
        )
        .await
        .expect("survivor native failover promotion did not complete")
        .unwrap();
        survivor_handle
            .execute_public_v1(intent(
                &preview,
                survivor_handle.process_session_id(),
                7,
                "kv-failover-final-current",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: ConfigurationDescriptor::new(
                        failover_epoch,
                        survivor_identity.replica_id,
                        vec![ConfigurationMember {
                            identity: survivor_identity.clone(),
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
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(15),
                survivor_service.replicate_put("after-failover".into(), "v2".into()),
            )
            .await
            .expect("promoted survivor did not commit its singleton write")
            .unwrap(),
            2
        );
        assert_eq!(survivor_persistence.get("committed").as_deref(), Some("v1"));
        assert_eq!(
            survivor_persistence.get("after-failover").as_deref(),
            Some("v2")
        );

        let returning_service = Arc::new(KvService::new(
            Arc::new(KvPersistence::open(source_root.join("application")).unwrap()),
            format!("http://{bind_address}"),
            CopyGate::disabled(),
        ));
        let mut returning = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid,
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("kv-failover-source-pod"),
                pvc_uid: PvcUid::new("kv-failover-source-pvc"),
                data_root: source_root.clone(),
                replication_address: bind_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            returning_service.clone(),
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_application_storage_paths(application_paths(&source_root))
        .with_public_v1_binding(binding)
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        assert_ne!(
            returning
                .handle()
                .public_v1_report()
                .await
                .unwrap()
                .write_status,
            AccessStatus::Granted
        );
        assert!(
            returning_service
                .replicate_put("returning-predecessor".into(), "bad".into())
                .await
                .is_err()
        );
        returning.shutdown();
        survivor.shutdown();
        returning.wait().await.unwrap();
        survivor.wait().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}
