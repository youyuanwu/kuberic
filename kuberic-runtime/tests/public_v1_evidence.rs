use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use kuberic_runtime::application::OpenMode;
use kuberic_runtime::control::{
    WireError, normalize_public_v1_agent_status_report, proto, public_v1_agent_status_to_proto,
};
use kuberic_runtime::protocol::public_operations::{
    PublicProgress, PublicProgressRole, PublicV1Identity,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, AgentGeneration, EffectivePolicy, InitializationId, PodUid, ProcessSessionId,
    PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole,
    ReplicaStorageIdentity, ReplicaStorageIncarnationContext, ResourceUid, StorageGeneration,
    derive_bootstrap_history, derive_history_initialization_id,
};
use kuberic_runtime::testing::sqlite_store::{
    PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION, PublicV1ReportProjection, SqliteStore,
};
use kuberic_runtime::testing::state::{SCHEMA_VERSION, StorageIdentity};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

fn test_root(name: &str) -> PathBuf {
    let root = std::env::current_dir()
        .unwrap()
        .join("target")
        .join("public-v1-evidence")
        .join(format!(
            "{name}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn replica() -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(1),
        instance_id: ReplicaInstanceId::new("evidence-pod"),
        agent_generation: AgentGeneration::new("evidence-agent"),
    }
}

fn storage(replica: ReplicaIdentity) -> ReplicaStorageIncarnationContext {
    ReplicaStorageIncarnationContext {
        replica,
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new("evidence-pod-uid"),
            pvc_uid: PvcUid::new("evidence-pvc-uid"),
        },
        storage_generation: StorageGeneration::new("evidence-storage"),
    }
}

fn identity(resource_uid: ResourceUid, replica: ReplicaIdentity) -> StorageIdentity {
    StorageIdentity {
        schema_version: SCHEMA_VERSION,
        resource_uid,
        pod_uid: PodUid::new("evidence-pod-uid"),
        pvc_uid: PvcUid::new("evidence-pvc-uid"),
        initialization_id: InitializationId::new("evidence-initialization"),
        local_identity: replica,
        effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
    }
}

fn contains_forbidden_key(value: &serde_json::Value) -> Option<String> {
    const FORBIDDEN: &[&str] = &[
        "verifiedReplicationLsn",
        "committedLsn",
        "currentConfigurationQuorumProgress",
        "catchUpBoundary",
        "catchUpComplete",
        "replicationBoundaryLsn",
        "durableLsn",
        "lastSequence",
        "copySequence",
        "engineCursor",
        "topologyReceipt",
        "acknowledgements",
    ];
    match value {
        serde_json::Value::Array(values) => values.iter().find_map(contains_forbidden_key),
        serde_json::Value::Object(values) => values.iter().find_map(|(key, value)| {
            FORBIDDEN
                .contains(&key.as_str())
                .then(|| key.clone())
                .or_else(|| contains_forbidden_key(value))
        }),
        _ => None,
    }
}

#[test]
fn public_v1_source_uses_no_native_evidence() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for relative in [
        "src/host/public_lifecycle.rs",
        "src/host/public_lifecycle_program.rs",
        "src/protocol/public_operations.rs",
    ] {
        let source = std::fs::read_to_string(manifest.join(relative)).unwrap();
        for forbidden in [
            "ManagedReplicaObservation",
            "ReplicationEngineObservation",
            "NativeProgressStatus",
            "TopologyReceipt",
            "BuildAuthorityStore",
            "BuildProgressStore",
        ] {
            assert!(
                !source.contains(forbidden),
                "{relative} depends on private/native evidence: {forbidden}"
            );
        }
    }
    assert!(!manifest.join("src/replicator/stateless/mod.rs").exists());
    let production =
        std::fs::read_to_string(manifest.join("src/replicator/v1/service_adapter/production.rs"))
            .unwrap();
    for forbidden in [
        "ManagedReplicaStore",
        "ReplicationProgressStore",
        "LocalWriteJournal",
        "BuildAuthorityStore",
        "BuildProgressStore",
    ] {
        assert!(
            !production.contains(forbidden),
            "native composition depends on old engine state: {forbidden}"
        );
    }
    let hosting = std::fs::read_to_string(manifest.join("src/host/hosting.rs")).unwrap();
    let projection = hosting
        .split("pub(crate) async fn refresh_replacement_projection")
        .nth(1)
        .unwrap()
        .split("pub(crate) fn provider_writer_lease_count")
        .next()
        .unwrap();
    let completed = projection.find("PublicOperationStage::Completed").unwrap();
    let succeeded = projection
        .find("PublicOperationDisposition::Succeeded")
        .unwrap();
    let access = projection.find("v1_lifecycle::activated_access").unwrap();
    assert!(completed < access && succeeded < access);
    assert!(projection.contains("if ready && native_live && role == selected_role"));

    let build_store =
        std::fs::read_to_string(manifest.join("src/host/public_build_store.rs")).unwrap();
    let start = build_store
        .find("pub(crate) struct PublicV1BuildRecord")
        .unwrap();
    let end = build_store[start..]
        .find("impl PublicV1BuildRecord")
        .map(|offset| start + offset)
        .unwrap();
    let build_record = &build_store[start..end];
    for forbidden in [
        "cursor",
        "sequence",
        "boundary",
        "durable_lsn",
        "ack",
        "quorum",
        "BuildAuthority",
        "BuildProgress",
        "Replicator",
    ] {
        assert!(
            !build_record.contains(forbidden),
            "replacement-schema build record contains private evidence: {forbidden}"
        );
    }
}

#[tokio::test]
async fn public_v1_evidence_report_is_structurally_public_only() {
    assert_eq!(PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION, 19);
    let root = test_root("report");
    let path = SqliteStore::metadata_database_path(&root);
    let resource_uid = ResourceUid::new("evidence-resource");
    let generation = PublicProtocolGeneration::new(42);
    let preview = PublicV1Identity::new(generation.value());
    let replica = replica();
    let store = SqliteStore::create_public_v1(
        &path,
        preview.clone(),
        identity(resource_uid.clone(), replica.clone()),
    )
    .unwrap();
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
    store
        .initialize_public_v1_history(&initialization, &history, &storage(replica))
        .await
        .unwrap();
    let before = store.load_state().await.unwrap();
    let report = store
        .public_v1_agent_report(
            &preview,
            PublicV1ReportProjection {
                process_session_id: ProcessSessionId::new("evidence-session"),
                report_sequence: 1,
                role: ReplicaRole::Primary,
                read_status: AccessStatus::Granted,
                write_status: AccessStatus::Granted,
                progress: PublicProgress {
                    role: PublicProgressRole::Primary,
                    current_progress:
                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(1),
                    catch_up_capability:
                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(1),
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.load_state().await.unwrap(),
        before,
        "public-v1 reporting must be read-only"
    );
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(contains_forbidden_key(&json), None);
    assert_eq!(report.open_mode, OpenMode::Existing);
    let mut missing_mode = json;
    missing_mode.as_object_mut().unwrap().remove("openMode");
    assert!(
        serde_json::from_value::<
            kuberic_runtime::protocol::public_operations::PublicV1AgentReport,
        >(missing_mode)
        .is_err()
    );

    let mut report = report;
    for mode in [OpenMode::New, OpenMode::Existing] {
        report.open_mode = mode;
        let wire = public_v1_agent_status_to_proto(&report);
        assert_eq!(
            normalize_public_v1_agent_status_report(wire).unwrap(),
            report
        );
    }
    let wire = public_v1_agent_status_to_proto(&report);
    assert_eq!(
        wire.protocol_version,
        kuberic_runtime::protocol::public_operations::PUBLIC_V1_PROTOCOL_VERSION
    );
    assert!(wire.public_v1.is_some());
    assert!(wire.public_v1.is_some());
    assert_eq!(
        normalize_public_v1_agent_status_report(wire.clone()).unwrap(),
        report
    );
    for invalid in [proto::PublicV1OpenMode::Unknown as i32, -1, i32::MAX] {
        let mut invalid_wire = wire.clone();
        invalid_wire.public_v1.as_mut().unwrap().open_mode = invalid;
        assert_eq!(
            normalize_public_v1_agent_status_report(invalid_wire),
            Err(WireError::InvalidEnum {
                field: "public_v1.open_mode",
                value: invalid,
            })
        );
    }
    split_report_wire_cases(&mut report);
    std::fs::remove_dir_all(root).unwrap();
}

fn split_report_wire_cases(
    report: &mut kuberic_runtime::protocol::public_operations::PublicV1AgentReport,
) {
    use kuberic_runtime::protocol::public_operations::*;
    use kuberic_runtime::protocol::types::{
        ConfigurationDescriptor, ConfigurationMember, Epoch, OperationId,
    };
    let epoch = Epoch::new(report.history.data_loss_number, 1);
    let voting = PublicConfiguration {
        current: ConfigurationDescriptor::new(
            epoch,
            report.replica.replica_id,
            vec![ConfigurationMember {
                identity: report.replica.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        ),
        previous: None,
        replicas: Vec::new(),
    };
    report.configuration = Some(PublicV1InstalledConfiguration {
        voting: voting.clone(),
    });
    assert_eq!(
        normalize_public_v1_agent_status_report(public_v1_agent_status_to_proto(report)).unwrap(),
        *report
    );
    assert!(report.engine_idle.is_none());
    report.engine_idle = Some(PublicV1EngineIdleObservation {
        configuration: voting.clone(),
        history: report.history.clone(),
        storage_incarnation: report.storage_incarnation.clone(),
        process_session_id: report.process_session_id.clone(),
        replicator_creation_id: report.replicator_creation_id.clone(),
        idle_replicas: Vec::new(),
    });
    assert_eq!(
        normalize_public_v1_agent_status_report(public_v1_agent_status_to_proto(report)).unwrap(),
        *report
    );
    for case in [
        "configuration",
        "history",
        "storage",
        "process",
        "creation",
        "voter",
        "duplicate",
        "negative",
        "missing-install",
    ] {
        let mut invalid = report.clone();
        let sample = invalid.engine_idle.as_mut().unwrap();
        match case {
            "configuration" => sample.configuration.current.epoch.configuration_number += 1,
            "history" => sample.history.data_loss_number += 1,
            "storage" => {
                sample.storage_incarnation.storage_generation = StorageGeneration::new("other")
            }
            "process" => sample.process_session_id = ProcessSessionId::new("other"),
            "creation" => {
                sample.replicator_creation_id =
                    kuberic_runtime::protocol::types::ReplicatorCreationId::new("other")
            }
            "voter" => sample.idle_replicas = vec![report.replica.replica_id],
            "duplicate" => sample.idle_replicas = vec![ReplicaId::new(2), ReplicaId::new(2)],
            "negative" => sample.idle_replicas = vec![ReplicaId::new(-1)],
            _ => invalid.configuration = None,
        }
        assert!(invalid.validate().is_err(), "{case}");
        assert!(
            normalize_public_v1_agent_status_report(public_v1_agent_status_to_proto(&invalid))
                .is_err(),
            "{case}"
        );
    }
    let target = ReplicaIdentity {
        replica_id: ReplicaId::new(2),
        ..report.replica.clone()
    };
    let build = PublicBuildInput {
        attempt: OperationId::new("wire-build"),
        epoch,
        current_configuration_id: Some(voting.current.configuration_id),
        history: Some(report.history.clone()),
        target_storage: storage(target.clone()),
        replica: target,
        process_session_id: ProcessSessionId::new("target-session"),
        source_address: "http://source:7000".into(),
        replication_address: "http://target:7000".into(),
    };
    let program = PublicOperationProgram::Build(build.clone());
    let remote = PublicV1RemoteBuildObservation {
        operation_id: build.attempt.clone(),
        revision: 2,
        build: build.clone(),
        state: PublicV1RemoteBuildState::Accepted,
    };
    report.remote_builds = vec![remote];
    // Presence in a real query need not mean Build completion.
    report.engine_idle.as_mut().unwrap().idle_replicas = vec![ReplicaId::new(2)];
    assert_eq!(
        normalize_public_v1_agent_status_report(public_v1_agent_status_to_proto(report)).unwrap(),
        *report
    );
    for case in [
        "ready-without-success",
        "running-without-operation",
        "attempt",
        "revision",
        "history",
        "storage",
        "self",
        "duplicate",
    ] {
        let mut invalid = report.clone();
        let remote = &mut invalid.remote_builds[0];
        match case {
            "ready-without-success" => remote.state = PublicV1RemoteBuildState::Ready,
            "running-without-operation" => remote.state = PublicV1RemoteBuildState::InBuild,
            "attempt" => remote.operation_id = OperationId::new("other"),
            "revision" => remote.revision = 0,
            "history" => remote.build.history = None,
            "storage" => remote.build.target_storage.replica = report.replica.clone(),
            "self" => {
                remote.build.replica = report.replica.clone();
                remote.build.target_storage = report.storage_incarnation.clone();
            }
            _ => invalid.remote_builds.push(report.remote_builds[0].clone()),
        }
        assert!(invalid.validate().is_err(), "{case}");
        assert!(
            normalize_public_v1_agent_status_report(public_v1_agent_status_to_proto(&invalid))
                .is_err(),
            "{case}"
        );
    }
    report.remote_builds[0].state = PublicV1RemoteBuildState::InBuild;
    report.active_operations.push(PublicOperationIntent {
        identity: report.identity.clone(),
        operation_id: build.attempt.clone(),
        revision: 2,
        process_session_id: report.process_session_id.clone(),
        class: PublicOperationClass::Build {
            target: build.replica.replica_id,
        },
        input_digest: program.digest(),
        replay_disposition: program.replay_disposition(),
        lifecycle: None,
        program: Some(program.clone()),
    });
    assert_eq!(
        normalize_public_v1_agent_status_report(public_v1_agent_status_to_proto(report)).unwrap(),
        *report
    );
    report.active_operations.clear();
    report
        .completed_operations
        .push(PublicV1OperationCompletion {
            operation_id: build.attempt,
            revision: 2,
            call_kind: PublicV1CallKind::Build,
            process_session_id: report.process_session_id.clone(),
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            history: report.history.clone(),
            storage_incarnation: report.storage_incarnation.clone(),
            previous_configuration_id: None,
            current_configuration_id: build.current_configuration_id,
            catch_up_mode: None,
            catch_up_specific_quorum: None,
            result: PublicV1OperationResult::Succeeded,
        });
    for case in [
        "failed",
        "revision",
        "target-session",
        "configuration",
        "source-storage",
        "accepted",
        "running",
    ] {
        let mut invalid = report.clone();
        invalid.engine_idle = None;
        invalid.remote_builds[0].state = PublicV1RemoteBuildState::Ready;
        match case {
            "failed" => {
                invalid.completed_operations.last_mut().unwrap().result =
                    PublicV1OperationResult::Failed("copy failed".into())
            }
            "revision" => invalid.remote_builds[0].revision += 1,
            "target-session" => {
                invalid.remote_builds[0].build.process_session_id =
                    ProcessSessionId::new("changed-target")
            }
            "configuration" => {
                invalid.remote_builds[0].build.current_configuration_id = Some(
                    kuberic_runtime::protocol::types::ConfigurationId::new("changed-configuration"),
                )
            }
            "source-storage" => {
                invalid.storage_incarnation.storage_generation =
                    StorageGeneration::new("changed-source")
            }
            "accepted" => invalid.remote_builds[0].state = PublicV1RemoteBuildState::Accepted,
            _ => invalid.remote_builds[0].state = PublicV1RemoteBuildState::InBuild,
        }
        assert!(invalid.validate().is_err(), "{case}");
        assert!(
            normalize_public_v1_agent_status_report(public_v1_agent_status_to_proto(&invalid))
                .is_err(),
            "{case}"
        );
    }
    for state in [
        PublicV1RemoteBuildState::Ready,
        PublicV1RemoteBuildState::InDrop,
        PublicV1RemoteBuildState::Dropped,
    ] {
        report.remote_builds[0].state = state;
        assert_eq!(
            normalize_public_v1_agent_status_report(public_v1_agent_status_to_proto(report))
                .unwrap(),
            *report
        );
    }
    for (field, json) in [
        ("engine", "{}"),
        ("remote", "{}"),
        ("remote", r#"{"state":"Voting"}"#),
    ] {
        let mut invalid = public_v1_agent_status_to_proto(report);
        let public = invalid.public_v1.as_mut().unwrap();
        if field == "engine" {
            public.engine_idle_json = Some(json.into());
        } else {
            public.remote_builds_json = vec![json.into()];
        }
        assert!(normalize_public_v1_agent_status_report(invalid).is_err());
    }
}

#[test]
fn public_v1_replacement_schema_rejects_prior_preview_stores() {
    let preview = PublicV1Identity::new(42);
    for version in [8, 9, 10, 15, 16, 17] {
        let root = test_root(&format!("reject-{version}"));
        let path = SqliteStore::metadata_database_path(&root);
        let resource_uid = ResourceUid::new("evidence-resource");
        let store = SqliteStore::create_public_v1(
            &path,
            preview.clone(),
            identity(resource_uid, replica()),
        )
        .unwrap();
        drop(store);
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(connection);
        assert!(SqliteStore::open_public_v1(&path, &preview).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn initializing_application_bindings_report_new_without_mutating_storage() {
    let root = test_root("fresh-open-mode");
    let resource_uid = ResourceUid::new("fresh-open-resource");
    let generation = PublicProtocolGeneration::new(1);
    let preview = PublicV1Identity::new(generation.value());
    let application_root = root.join("application");
    let store = SqliteStore::create_public_v1_with_application_paths(
        SqliteStore::metadata_database_path(&root),
        preview.clone(),
        identity(resource_uid.clone(), replica()),
        std::collections::BTreeMap::from([("data".into(), application_root.clone())]),
    )
    .unwrap();
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
    store
        .initialize_public_v1_history(&initialization, &history, &storage(replica()))
        .await
        .unwrap();
    let before = store.load_state().await.unwrap();
    let report = store
        .public_v1_agent_report(
            &preview,
            PublicV1ReportProjection {
                process_session_id: ProcessSessionId::new("fresh-open-session"),
                report_sequence: 1,
                role: ReplicaRole::None,
                read_status: AccessStatus::NotPrimary,
                write_status: AccessStatus::NotPrimary,
                progress: PublicProgress {
                    role: PublicProgressRole::Recovery,
                    current_progress:
                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(0),
                    catch_up_capability:
                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(0),
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(report.open_mode, OpenMode::New);
    assert!(report.replication_address.is_none());
    assert!(report.completed_operations.is_empty());
    assert_eq!(store.load_state().await.unwrap(), before);
    assert!(!application_root.exists());
    assert_eq!(
        normalize_public_v1_agent_status_report(public_v1_agent_status_to_proto(&report)).unwrap(),
        report
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
