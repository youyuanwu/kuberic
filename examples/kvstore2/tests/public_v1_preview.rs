#![cfg(feature = "testing")]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kuberic_runtime::application::{CopyChunk, Operation, StateProvider};
use kuberic_runtime::engine::DurableState;
use kuberic_runtime::protocol::types::OperationId;
use kvstore2::persistence::KvPersistence;
use kvstore2::state::{CopyGate, KvStateProvider};

fn root(name: &str) -> std::path::PathBuf {
    std::env::current_dir()
        .unwrap()
        .join("target")
        .join("kvstore2-public-v1-preview")
        .join(format!("{name}-{}", std::process::id()))
}

#[tokio::test]
async fn kvstore2_fresh_restart_build_and_corrective_empty_replacement() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let source_root = root("source");
        let retired_root = root("retired");
        let replacement_root = root("replacement");
        for path in [&source_root, &retired_root, &replacement_root] {
            let _ = std::fs::remove_dir_all(path);
        }

        let source = Arc::new(KvPersistence::open(&source_root).unwrap());
        source
            .apply(Operation {
                lsn: 1,
                committed_lsn: 0,
                data: KvPersistence::encode_put("snapshot".into(), "v1".into()).unwrap(),
            })
            .await
            .unwrap();
        source.commit(1).await.unwrap();
        let provider = KvStateProvider::new(source.clone(), CopyGate::disabled());
        let copy_context = provider.get_copy_context().await.unwrap();
        let mut copy = provider.get_copy_state(1, copy_context).await.unwrap();

        source
            .apply(Operation {
                lsn: 2,
                committed_lsn: 1,
                data: KvPersistence::encode_put("retained".into(), "v2".into()).unwrap(),
            })
            .await
            .unwrap();
        source.commit(2).await.unwrap();

        let retired = KvPersistence::open(&retired_root).unwrap();
        retired
            .apply(Operation {
                lsn: 9,
                committed_lsn: 0,
                data: KvPersistence::encode_put("stale".into(), "must-not-return".into()).unwrap(),
            })
            .await
            .unwrap();
        retired.commit(9).await.unwrap();
        drop(retired);

        let replacement = KvPersistence::open(&replacement_root).unwrap();
        let build = OperationId::new("kvstore2-corrective-replacement");
        let mut sequence = 0;
        while let Some(chunk) = copy.next().await {
            replacement
                .apply_copy_chunk(
                    &build,
                    sequence,
                    CopyChunk {
                        data: chunk.unwrap(),
                    },
                )
                .await
                .unwrap();
            sequence += 1;
        }
        replacement.finish_copy(&build, 1, 1).await.unwrap();
        let mut retained = source.get_replication_operations(2, 2).await.unwrap();
        replacement
            .apply(retained.next().await.unwrap().unwrap())
            .await
            .unwrap();
        replacement.commit(2).await.unwrap();
        drop(replacement);

        let reopened = KvPersistence::open(&replacement_root).unwrap();
        assert_eq!(reopened.get("snapshot").as_deref(), Some("v1"));
        assert_eq!(reopened.get("retained").as_deref(), Some("v2"));
        assert_eq!(reopened.get("stale"), None);
        assert_eq!(reopened.durable_progress().await.unwrap().committed_lsn, 2);
        assert!(
            KvPersistence::open(&retired_root)
                .unwrap()
                .get("stale")
                .is_some(),
            "retired storage remains distinct evidence and was not reused"
        );

        drop(reopened);
        for path in [&source_root, &retired_root, &replacement_root] {
            std::fs::remove_dir_all(path).unwrap();
        }
    })
    .await
    .unwrap();
}
