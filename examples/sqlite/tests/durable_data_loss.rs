use std::time::Duration;

use futures::StreamExt;
use kuberic_runtime::application::CopyChunk;
use kuberic_runtime::engine::DurableState;
use kuberic_runtime::protocol::types::OperationId;
use sqlite_replicated::SqlitePersistence;
use sqlite_replicated::testing::{SqlitePod, scratch};

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
    tokio::time::timeout(Duration::from_secs(120), async {
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
        let retired_root = root.join("retired");
        let replacement_root = root.join("replacement");

        let source = SqlitePod::singleton(source_root).await;
        source
            .execute("CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT)")
            .await
            .unwrap();
        source
            .execute("INSERT INTO data VALUES(1,'snapshot')")
            .await
            .unwrap();
        let boundary = source
            .application
            .persistence()
            .progress()
            .unwrap()
            .committed_lsn;
        let snapshot = source.application.persistence().snapshot(boundary).unwrap();
        let retained_lsn = source
            .execute("INSERT INTO data VALUES(2,'retained')")
            .await
            .unwrap()
            .lsn;

        let retired = SqlitePod::singleton(retired_root).await;
        retired
            .execute("CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT)")
            .await
            .unwrap();
        for id in 10..14 {
            retired
                .execute(&format!("INSERT INTO data VALUES({id},'stale')"))
                .await
                .unwrap();
        }
        assert!(
            retired
                .application
                .persistence()
                .progress()
                .unwrap()
                .committed_lsn
                > retained_lsn
        );

        let replacement = SqlitePersistence::open(replacement_root.clone()).unwrap();
        let build = OperationId::new("sqlite-corrective-replacement");
        replacement
            .apply_copy_chunk(
                &build,
                1,
                CopyChunk {
                    data: snapshot.into(),
                },
            )
            .await
            .unwrap();
        replacement
            .finish_copy(&build, boundary, boundary)
            .await
            .unwrap();
        let mut retained = source
            .application
            .persistence()
            .get_replication_operations(retained_lsn, retained_lsn)
            .await
            .unwrap();
        replacement
            .apply(retained.next().await.unwrap().unwrap())
            .await
            .unwrap();
        replacement.commit(retained_lsn).await.unwrap();
        drop(replacement);

        let reopened = SqlitePersistence::open(replacement_root).unwrap();
        let inspection = reopened.inspect_for_test().unwrap();
        assert_eq!(inspection.progress.committed_lsn, retained_lsn);
        let inspection_path = root.join("inspection.sqlite");
        std::fs::write(&inspection_path, &inspection.image).unwrap();
        let database = rusqlite::Connection::open(&inspection_path).unwrap();
        let rows = database
            .prepare("SELECT id,value FROM data ORDER BY id")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows, vec![(1, "snapshot".into()), (2, "retained".into())]);

        drop(database);
        drop(reopened);
        drop(retired);
        drop(source);
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}
