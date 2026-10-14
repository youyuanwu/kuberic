use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use kuberic_runtime::control::{
    normalize_public_v1_agent_status_report, public_v1_agent_status_to_proto,
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

        let stateless =
            std::fs::read_to_string(manifest.join("src/replicator/stateless/mod.rs")).unwrap();
        for forbidden in [
            "ManagedReplicaStore",
            "ReplicationProgressStore",
            "LocalWriteJournal",
            "BuildAuthorityStore",
            "BuildProgressStore",
            "authorize_request",
            "bearer_token",
        ] {
            assert!(
                !stateless.contains(forbidden),
                "stateless preview depends on managed state or authorization: {forbidden}"
            );
        }
        let hosting = std::fs::read_to_string(manifest.join("src/host/hosting.rs")).unwrap();
        let projection = hosting
            .split("refresh_replacement_projection")
            .nth(1)
            .unwrap()
            .split("pub(crate) fn provider_writer_lease_count")
            .next()
            .unwrap();
        let completed = projection.find("PublicOperationStage::Completed").unwrap();
        let succeeded = projection
            .find("PublicOperationDisposition::Succeeded")
            .unwrap();
        let granted = projection.find("AccessStatus::Granted").unwrap();
        assert!(completed < granted && succeeded < granted);
    }

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
    assert_eq!(PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION, 12);
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
                    first: 1,
                    last: 1,
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

    let wire = public_v1_agent_status_to_proto(&report);
    assert_eq!(
        wire.protocol_version,
        kuberic_runtime::protocol::public_operations::PUBLIC_V1_PROTOCOL_VERSION
    );
    assert!(wire.public_v1.is_some());
    assert!(wire.public_v1.is_some());
    assert_eq!(
        normalize_public_v1_agent_status_report(wire).unwrap(),
        report
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn public_v1_replacement_schema_rejects_prior_preview_stores() {
    let preview = PublicV1Identity::new(42);
    for version in [8, 9, 10] {
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
