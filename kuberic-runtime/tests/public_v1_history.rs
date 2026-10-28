use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use kuberic_runtime::protocol::public_operations::{
    DataLossAttempt, DataLossAuthorization, DataLossRenewalDecision, DataLossReplay,
    ProcessAttemptContext, ProviderProgressReset, PublicV1Identity,
};
use kuberic_runtime::protocol::types::{
    AgentGeneration, EffectivePolicy, Epoch, InitializationId, OperationId, PodUid,
    ProcessSessionId, PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaStorageIdentity, ReplicaStorageIncarnationContext,
    ReplicatorCreationId, ResourceUid, StorageGeneration, derive_bootstrap_history,
    derive_data_loss_history, derive_history_initialization_id,
};
use kuberic_runtime::testing::sqlite_store::{PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION, SqliteStore};
use kuberic_runtime::testing::state::{SCHEMA_VERSION, StorageIdentity};

#[derive(Debug, PartialEq, Eq)]
struct FileTreeSnapshot {
    directories: BTreeSet<PathBuf>,
    files: BTreeMap<PathBuf, Vec<u8>>,
}

fn snapshot_file_tree(root: &Path) -> FileTreeSnapshot {
    fn visit(
        root: &Path,
        current: &Path,
        directories: &mut BTreeSet<PathBuf>,
        files: &mut BTreeMap<PathBuf, Vec<u8>>,
    ) {
        let relative = current.strip_prefix(root).unwrap().to_path_buf();
        if current.is_dir() {
            directories.insert(relative);
            let mut entries = std::fs::read_dir(current)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            entries.sort();
            for entry in entries {
                visit(root, &entry, directories, files);
            }
        } else {
            files.insert(relative, std::fs::read(current).unwrap());
        }
    }

    let mut directories = BTreeSet::new();
    let mut files = BTreeMap::new();
    visit(root, root, &mut directories, &mut files);
    FileTreeSnapshot { directories, files }
}

fn create_rejected_store(path: &Path, version: u32, partial_replacement: bool) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE agent_state (
                singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                state_json TEXT NOT NULL
             );",
        )
        .unwrap();
    if !partial_replacement {
        connection
            .execute(
                "INSERT INTO agent_state(singleton, state_json) VALUES(1, ?1)",
                [format!("legacy-schema-{version}")],
            )
            .unwrap();
    }
    connection
        .pragma_update(None, "user_version", version)
        .unwrap();
}

fn sqlite_sidecars(path: &Path) -> [PathBuf; 2] {
    [
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ]
}

fn replica(replica_id: i64, pod: &str, generation: &str) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(replica_id),
        instance_id: ReplicaInstanceId::new(pod),
        agent_generation: AgentGeneration::new(generation),
    }
}

fn storage(replica: ReplicaIdentity, pod: &str, pvc: &str) -> ReplicaStorageIncarnationContext {
    ReplicaStorageIncarnationContext {
        replica,
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new(pod),
            pvc_uid: PvcUid::new(pvc),
        },
        storage_generation: StorageGeneration::new(format!("storage-{pvc}")),
    }
}

fn attempt(
    authorization_id: &OperationId,
    attempt_id: &str,
    session: &str,
    creation: &str,
    epoch: Epoch,
    revision: u64,
    replay: DataLossReplay,
) -> DataLossAttempt {
    DataLossAttempt {
        authorization_id: authorization_id.clone(),
        attempt_id: OperationId::new(attempt_id),
        process: ProcessAttemptContext {
            process_session_id: ProcessSessionId::new(session),
            replicator_creation_id: ReplicatorCreationId::new(creation),
            catch_up_specific_quorum: false,
            data_loss_replay: replay,
            epoch,
            attempt_revision: revision,
        },
    }
}

#[test]
fn public_v1_history_partition_seed_is_shared() {
    let resource_uid = ResourceUid::new("resource-1");
    let generation = PublicProtocolGeneration::new(42);
    let accepted = derive_history_initialization_id(&resource_uid, generation);
    let first = derive_bootstrap_history(&resource_uid, generation, &accepted);
    let second = derive_bootstrap_history(&resource_uid, generation, &accepted);

    assert_eq!(first, second);
    assert_eq!(first.data_loss_number, 0);
    assert_ne!(
        accepted.as_str(),
        InitializationId::new("replica-local-init").as_str()
    );

    let other_resource = derive_bootstrap_history(
        &ResourceUid::new("resource-2"),
        generation,
        &derive_history_initialization_id(&ResourceUid::new("resource-2"), generation),
    );
    let other_generation = derive_bootstrap_history(
        &resource_uid,
        PublicProtocolGeneration::new(43),
        &derive_history_initialization_id(&resource_uid, PublicProtocolGeneration::new(43)),
    );
    assert_ne!(first.history_id, other_resource.history_id);
    assert_ne!(first.history_id, other_generation.history_id);
}

#[test]
fn public_v1_history_data_loss_attempt_renews_higher_epoch() {
    let resource_uid = ResourceUid::new("resource-1");
    let generation = PublicProtocolGeneration::new(42);
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    let predecessor = derive_bootstrap_history(&resource_uid, generation, &initialization);
    let provisional = storage(replica(1, "pod-1", "generation-1"), "pod-1", "pvc-1");
    let authorization_id = OperationId::new("data-loss-authorization");
    let proposed =
        derive_data_loss_history(&authorization_id, &predecessor, 1, &provisional).unwrap();
    let authorization = DataLossAuthorization {
        authorization_id: authorization_id.clone(),
        predecessor,
        proposed,
        provisional_primary: provisional,
        reset_scope: ProviderProgressReset::ResetToEmpty,
        replay_disposition: DataLossReplay::Convergent,
    };
    authorization.validate().unwrap();

    let first = attempt(
        &authorization_id,
        "attempt-1",
        "session-1",
        "creation-1",
        Epoch::new(1, 10),
        1,
        DataLossReplay::Convergent,
    );
    let renewed = attempt(
        &authorization_id,
        "attempt-2",
        "session-2",
        "creation-2",
        Epoch::new(1, 11),
        2,
        DataLossReplay::Convergent,
    );
    assert_eq!(
        DataLossAttempt::renewal_decision(&authorization, &first, &renewed, true),
        DataLossRenewalDecision::InvokeCallback
    );

    let same_epoch = attempt(
        &authorization_id,
        "attempt-3",
        "session-3",
        "creation-3",
        Epoch::new(1, 10),
        3,
        DataLossReplay::Convergent,
    );
    assert_eq!(
        DataLossAttempt::renewal_decision(&authorization, &first, &same_epoch, true),
        DataLossRenewalDecision::Terminal
    );

    let lower_epoch = attempt(
        &authorization_id,
        "attempt-lower",
        "session-lower",
        "creation-lower",
        Epoch::new(1, 9),
        3,
        DataLossReplay::Convergent,
    );
    assert_eq!(
        DataLossAttempt::renewal_decision(&authorization, &first, &lower_epoch, true),
        DataLossRenewalDecision::Terminal
    );

    let conservative = attempt(
        &authorization_id,
        "attempt-4",
        "session-4",
        "creation-4",
        Epoch::new(1, 12),
        4,
        DataLossReplay::ReplaceOnAmbiguity,
    );
    assert_eq!(
        DataLossAttempt::renewal_decision(&authorization, &first, &conservative, true),
        DataLossRenewalDecision::RetireWithoutCallback
    );

    let terminal_authorization = DataLossAuthorization {
        replay_disposition: DataLossReplay::ReplaceOnAmbiguity,
        ..authorization
    };
    assert_eq!(
        DataLossAttempt::renewal_decision(&terminal_authorization, &conservative, &renewed, true,),
        DataLossRenewalDecision::Terminal
    );
}

#[tokio::test]
async fn public_v1_history_store_uses_the_current_exact_schema() {
    assert_eq!(PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION, 23);
    let directory = tempfile::tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let preview = PublicV1Identity::new(42);
    let resource_uid = ResourceUid::new("resource-1");
    let local = replica(1, "pod-1", "generation-1");
    let store = SqliteStore::create_public_v1(
        &path,
        preview.clone(),
        StorageIdentity {
            schema_version: SCHEMA_VERSION,
            resource_uid: resource_uid.clone(),
            pod_uid: PodUid::new("pod-1"),
            pvc_uid: PvcUid::new("pvc-1"),
            initialization_id: InitializationId::new("local-initialization"),
            local_identity: local.clone(),
            effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
        },
    )
    .unwrap();
    let generation = PublicProtocolGeneration::new(preview.generation);
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
    let incarnation = storage(local.clone(), "pod-1", "pvc-1");
    store
        .initialize_public_v1_history(&initialization, &history, &incarnation)
        .await
        .unwrap();
    assert_eq!(
        store.public_v1_history_snapshot().await.unwrap().history,
        history
    );
    drop(store);

    let reopened = SqliteStore::open_public_v1(&path, &preview).unwrap();
    assert_eq!(
        reopened
            .public_v1_history_snapshot()
            .await
            .unwrap()
            .initialization_id,
        initialization
    );
    drop(reopened);

    let connection = rusqlite::Connection::open(&path).unwrap();
    let tables = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(tables, vec!["agent_state"]);
    drop(connection);

    for rejected_version in [6, 7, 11, 14, 15, 16, 17] {
        let directory = tempfile::tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(directory.path());
        create_rejected_store(&path, rejected_version, false);
        if rejected_version == 11 {
            for (index, sidecar) in sqlite_sidecars(&path).iter().enumerate() {
                std::fs::write(sidecar, format!("unchanged-sidecar-{index}")).unwrap();
            }
        } else {
            assert!(
                sqlite_sidecars(&path)
                    .iter()
                    .all(|sidecar| !sidecar.exists())
            );
        }
        let before = snapshot_file_tree(directory.path());
        assert!(SqliteStore::open_public_v1(&path, &preview).is_err());
        assert_eq!(snapshot_file_tree(directory.path()), before);
        if rejected_version != 11 {
            assert!(
                sqlite_sidecars(&path)
                    .iter()
                    .all(|sidecar| !sidecar.exists())
            );
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    create_rejected_store(&path, 12, true);
    assert!(
        sqlite_sidecars(&path)
            .iter()
            .all(|sidecar| !sidecar.exists())
    );
    let before = snapshot_file_tree(directory.path());
    assert!(SqliteStore::open_public_v1(&path, &preview).is_err());
    assert_eq!(snapshot_file_tree(directory.path()), before);
    assert!(
        sqlite_sidecars(&path)
            .iter()
            .all(|sidecar| !sidecar.exists())
    );
}
