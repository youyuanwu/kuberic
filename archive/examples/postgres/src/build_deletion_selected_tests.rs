use crate::instance::PgInstanceManager;
use crate::testing::{ProcessProbe, TestDataDir, find_pg_bin};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn instance(name: &str) -> (TestDataDir, PgInstanceManager) {
    let root = TestDataDir::new(name);
    let data = root.path().join("pgdata");
    std::fs::create_dir(&data).unwrap();
    std::fs::write(data.join("PG_VERSION"), b"16").unwrap();
    std::fs::create_dir(data.join("base")).unwrap();
    std::fs::write(data.join("base/old"), b"old").unwrap();
    let instance = PgInstanceManager::new(data, find_pg_bin(), 1);
    (root, instance)
}

fn deletion_gate(instance: &PgInstanceManager, entry: &str, root: &Path) -> PathBuf {
    let marker = root.join("deletion-entered");
    *instance.clear_pgdata_hook.lock().unwrap() = Some((entry.into(), marker.clone()));
    marker
}

async fn deletion_entered(marker: &Path) -> ProcessProbe {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(marker)
                && let Ok(pid) = pid.parse()
            {
                return ProcessProbe::process(pid);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("actual deletion helper did not enter")
}

#[tokio::test]
async fn pgdata_deletion_supersession_reaps_before_successor_configuration() {
    let (root, instance) = instance("delete-supersession");
    let marker = deletion_gate(&instance, "base", root.path());
    let old = {
        let work = instance.clear_pgdata();
        tokio::pin!(work);
        tokio::select! {
            result = &mut work => panic!("clear missed deletion gate: {result:?}"),
            helper = deletion_entered(&marker) => helper,
        }
    };
    old.assert_reaped();
    std::fs::remove_file(&marker).unwrap();
    let successor = instance.data_dir().join("successor-owned");
    std::fs::write(&successor, b"successor").unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(std::fs::read(successor).unwrap(), b"successor");
}

#[tokio::test]
async fn pgdata_deletion_timeout_reaps_before_same_build_retry() {
    let (root, instance) = instance("delete-timeout");
    let marker = deletion_gate(&instance, "base", root.path());
    let helper = {
        let work = instance.clear_pgdata();
        tokio::pin!(work);
        let helper = tokio::select! {
            result = &mut work => panic!("clear missed deletion gate: {result:?}"),
            helper = deletion_entered(&marker) => helper,
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut work)
                .await
                .is_err()
        );
        helper
    };
    helper.assert_reaped();
    std::fs::remove_file(marker).unwrap();
    instance.clear_pgdata().await.unwrap();
    assert_eq!(std::fs::read_dir(instance.data_dir()).unwrap().count(), 0);
}

#[tokio::test]
async fn pgdata_deletion_io_failure_is_reported_and_partial_copy_retried() {
    let (root, instance) = instance("delete-io");
    let marker = deletion_gate(&instance, "base", root.path());
    let work = instance.clear_pgdata();
    tokio::pin!(work);
    let helper = tokio::select! {
        result = &mut work => panic!("clear missed deletion gate: {result:?}"),
        helper = deletion_entered(&marker) => helper,
    };
    let original = instance.data_dir().join("base");
    let saved = root.path().join("removed-entry");
    std::fs::rename(&original, &saved).unwrap();
    std::fs::remove_file(&marker).unwrap();
    let error = work.await.unwrap_err();
    assert!(error.to_string().contains("clear PGDATA failed"), "{error}");
    helper.assert_reaped();
    std::fs::rename(saved, original).unwrap();
    instance.clear_pgdata().await.unwrap();
    assert_eq!(std::fs::read_dir(instance.data_dir()).unwrap().count(), 0);
}

#[tokio::test]
async fn pgdata_deletion_release_executes_removals_without_following_symlinks() {
    for directory in [false, true] {
        let root = TestDataDir::new("delete-release");
        let data = root.path().join("pgdata");
        let entry = data.join("remove-me");
        std::fs::create_dir(&data).unwrap();
        if directory {
            std::fs::create_dir(&entry).unwrap();
            std::fs::write(entry.join("nested"), b"old-data").unwrap();
        } else {
            std::fs::write(&entry, b"old-data").unwrap();
        }
        let outside = root.path().join("keep-me");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("retained"), b"not-pgdata").unwrap();
        std::os::unix::fs::symlink(&outside, data.join("external-link")).unwrap();
        let instance = PgInstanceManager::new(data.clone(), find_pg_bin(), 1);
        let marker = deletion_gate(&instance, "remove-me", root.path());
        let work = instance.clear_pgdata();
        tokio::pin!(work);
        let helper = tokio::select! {
            result = &mut work => panic!("clear missed deletion gate: {result:?}"),
            helper = deletion_entered(&marker) => helper,
        };
        assert!(entry.exists());
        std::fs::remove_file(marker).unwrap();
        work.await.unwrap();
        helper.assert_reaped();
        assert_eq!(std::fs::read_dir(&data).unwrap().count(), 0);
        assert_eq!(
            std::fs::read(outside.join("retained")).unwrap(),
            b"not-pgdata"
        );
    }
}
