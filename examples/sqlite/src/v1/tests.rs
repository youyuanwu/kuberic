use super::*;
use futures::FutureExt;
use rusqlite::types::Value;

fn open(path: &std::path::Path) -> (Committer, Arc<Mutex<Vec<String>>>) {
    let faults = Arc::new(Mutex::new(Vec::new()));
    let reported = faults.clone();
    let service = Committer::open(
        path.into(),
        Arc::new(|_| Ok(())),
        Arc::new(move |reason| {
            reported.lock().unwrap().push(reason);
        }),
    )
    .unwrap();
    (service, faults)
}

fn standalone(service: &Committer, start: i64) {
    let next = Arc::new(std::sync::atomic::AtomicI64::new(start));
    service
        .promote(Arc::new(move |_| {
            Ok(Assignment {
                lsn: next.fetch_add(1, Ordering::SeqCst),
                completion: Box::pin(async { Ok(()) }),
            })
        }))
        .unwrap();
}

fn execute(service: &Committer, sql: &str) -> Write {
    service.begin_execute(vec![sql.into()]).unwrap()
}

async fn create(service: &Committer) {
    assert_eq!(
        execute(service, "CREATE TABLE t (v INTEGER)")
            .wait()
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn real_sql_publication_client_drop_and_repeated_outcome() {
    let root = tempfile::tempdir().unwrap();
    let (service, faults) = open(root.path());
    standalone(&service, 1);
    create(&service).await;
    service.before_commit().arm();
    let mut first = execute(&service, "INSERT INTO t VALUES (7)");
    assert_eq!(first.assigned().await.unwrap(), 2);
    service.before_commit().wait_entered().await;
    assert_eq!(service.progress().unwrap(), 1);
    assert!(service.snapshot(2).now_or_never().is_none());
    assert_eq!(
        service.query("SELECT * FROM t").unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    let second = execute(&service, "INSERT INTO t VALUES (8)");
    drop(second);
    assert!(first.wait().now_or_never().is_none());
    service.before_commit().release();
    assert_eq!(first.wait().await.unwrap(), 2);
    assert_eq!(first.wait().await.unwrap(), 2);
    service.close().await.unwrap();
    service.close().await.unwrap();
    assert!(faults.lock().unwrap().is_empty());
    drop(service);
    let (service, _) = open(root.path());
    standalone(&service, 4);
    assert_eq!(service.progress().unwrap(), 3);
    assert_eq!(
        service.query("SELECT v FROM t ORDER BY v").unwrap(),
        vec![vec![Value::Integer(7)], vec![Value::Integer(8)]]
    );
    // A query-opened connection must still use the guarded VFS for later writes.
    assert_eq!(
        execute(&service, "INSERT INTO t VALUES (9)")
            .wait()
            .await
            .unwrap(),
        4
    );
    service.close().await.unwrap();
}

#[tokio::test]
async fn local_commit_publication_and_success_are_distinct() {
    let root = tempfile::tempdir().unwrap();
    let (service, _) = open(root.path());
    standalone(&service, 1);
    create(&service).await;
    service.before_publication().arm();
    service.before_success().arm();
    let mut write = execute(&service, "INSERT INTO t VALUES (7)");
    service.before_publication().wait_entered().await;
    assert_eq!(service.progress().unwrap(), 2);
    assert!(service.snapshot(2).await.is_ok());
    assert!(write.wait().now_or_never().is_none());
    assert!(service.query("SELECT * FROM t").is_err());
    service.before_publication().release();
    service.before_success().wait_entered().await;
    assert!(write.wait().now_or_never().is_none());
    service.before_success().release();
    assert_eq!(write.wait().await.unwrap(), 2);
    assert_eq!(
        service.query("SELECT * FROM t").unwrap(),
        vec![vec![Value::Integer(7)]]
    );
    service.close().await.unwrap();
}

#[tokio::test]
async fn definitive_sql_and_admission_errors_do_not_fence_or_advance() {
    let root = tempfile::tempdir().unwrap();
    let (service, faults) = open(root.path());
    service
        .promote(Arc::new(|_| {
            Err(io::Error::other("native admission rejected"))
        }))
        .unwrap();
    let mut rejected = execute(&service, "CREATE TABLE t (v INTEGER)");
    assert!(rejected.wait().await.is_err());
    assert!(rejected.assigned().await.is_err());
    assert_eq!(service.progress().unwrap(), 0);
    standalone(&service, 1);
    assert!(execute(&service, "not sql").wait().await.is_err());
    create(&service).await;
    assert!(
        execute(&service, "ATTACH DATABASE ':memory:' AS bypass")
            .wait()
            .await
            .is_err()
    );
    assert_eq!(service.progress().unwrap(), 1);
    assert!(faults.lock().unwrap().is_empty());
    service.close().await.unwrap();
}

#[tokio::test]
async fn admitted_cuts_fence_and_new_instance_recovers_only_local_commit() {
    for cut in [
        FailureCut::Prepare,
        FailureCut::Commit,
        FailureCut::Publication,
    ] {
        let root = tempfile::tempdir().unwrap();
        let (service, faults) = open(root.path());
        standalone(&service, 1);
        create(&service).await;
        service.fail_once(cut);
        let mut write = execute(&service, "INSERT INTO t VALUES (7)");
        assert!(write.wait().await.is_err());
        assert!(write.wait().await.is_err());
        assert!(service.query("SELECT * FROM t").is_err());
        assert!(
            service
                .begin_execute(vec!["INSERT INTO t VALUES (8)".into()])
                .is_err()
        );
        assert!(service.close().await.is_err());
        assert_eq!(faults.lock().unwrap().len(), 1);
        drop(service);
        let (service, _) = open(root.path());
        let committed = matches!(cut, FailureCut::Publication);
        standalone(&service, if committed { 3 } else { 2 });
        assert_eq!(service.progress().unwrap(), if committed { 2 } else { 1 });
        assert_eq!(
            service.query("SELECT * FROM t").unwrap().len(),
            usize::from(committed)
        );
        service.close().await.unwrap();
    }
}

#[tokio::test]
async fn cancelled_close_drains_and_abort_revokes_late_success() {
    for abort in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (service, faults) = open(root.path());
        standalone(&service, 1);
        service.before_commit().arm();
        let mut write = execute(&service, "CREATE TABLE t (v INTEGER)");
        service.before_commit().wait_entered().await;
        assert!(service.close().now_or_never().is_none());
        assert!(service.begin_execute(vec![]).is_err());
        if abort {
            service.abort();
            assert!(write.wait().await.is_err());
            assert!(service.close().await.is_err());
            assert_eq!(faults.lock().unwrap().len(), 1);
        } else {
            service.before_commit().release();
            write.wait().await.unwrap();
            service.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn abort_after_sql_publication_never_selects_client_success() {
    let root = tempfile::tempdir().unwrap();
    let (service, _) = open(root.path());
    standalone(&service, 1);
    service.before_success().arm();
    let mut write = execute(&service, "CREATE TABLE t (v INTEGER)");
    service.before_success().wait_entered().await;
    service.abort();
    assert!(write.wait().await.is_err());
    assert!(service.close().await.is_err());
    drop(service);
    let (service, _) = open(root.path());
    assert_eq!(service.progress().unwrap(), 1);
    service.close().await.unwrap();
}

#[tokio::test]
async fn copy_requires_installation_and_eof_rejects_regression_conflict_and_bad_payloads() {
    let root = tempfile::tempdir().unwrap();
    let (source, _) = open(root.path());
    standalone(&source, 1);
    create(&source).await;
    let snapshot = source.snapshot(1).await.unwrap();
    let empty = source.snapshot(0).await.unwrap();
    for data in [Bytes::from_static(b"bad"), empty, {
        let mut conflict: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
        conflict["image"][0] = serde_json::json!(0);
        Bytes::from(serde_json::to_vec(&conflict).unwrap())
    }] {
        let target_root = tempfile::tempdir().unwrap();
        let (target, faults) = open(target_root.path());
        target.begin_copy().unwrap();
        assert!(target.finish_copy().is_err());
        assert!(target.promote(Arc::new(|_| unreachable!())).is_err());
        target.install_copy(snapshot.clone()).unwrap();
        target.finish_copy().unwrap();
        target.begin_copy().unwrap();
        assert!(target.install_copy(data).is_err());
        assert!(target.close().await.is_err());
        assert_eq!(faults.lock().unwrap().len(), 1);
    }
    source.close().await.unwrap();
}

#[tokio::test]
async fn corruption_is_not_hidden_by_new_instance_recovery() {
    let root = tempfile::tempdir().unwrap();
    let (service, _) = open(root.path());
    standalone(&service, 1);
    create(&service).await;
    service.close().await.unwrap();
    drop(service);
    let history = std::fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("history-")
        })
        .unwrap();
    std::fs::write(history, b"corrupt acknowledged history").unwrap();
    assert!(Committer::open(root.path().into(), Arc::new(|_| Ok(())), Arc::new(|_| {})).is_err());
}

#[tokio::test]
async fn callback_and_completion_panics_are_fenced_inside_the_sqlite_ffi_boundary() {
    for admission_panic in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (service, faults) = open(root.path());
        service
            .promote(Arc::new(move |_| {
                assert!(!admission_panic, "admission panic");
                Ok(Assignment {
                    lsn: 1,
                    completion: Box::pin(async { panic!("completion panic") }),
                })
            }))
            .unwrap();
        let mut write = execute(&service, "CREATE TABLE t (v INTEGER)");
        assert!(write.wait().await.is_err());
        assert!(write.wait().await.is_err());
        assert!(service.query("SELECT 1").is_err());
        assert!(service.close().await.is_err());
        assert_eq!(faults.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn fault_callback_panic_is_diagnostic_not_a_successful_drain() {
    let root = tempfile::tempdir().unwrap();
    let service = Committer::open(
        root.path().into(),
        Arc::new(|_| Ok(())),
        Arc::new(|_| panic!("fault reporter")),
    )
    .unwrap();
    standalone(&service, 1);
    service.fail_once(FailureCut::Commit);
    assert!(
        execute(&service, "CREATE TABLE t (v INTEGER)")
            .wait()
            .await
            .is_err()
    );
    assert!(
        service
            .close()
            .await
            .unwrap_err()
            .to_string()
            .contains("fault callback panicked")
    );
}

#[tokio::test]
async fn fresh_copy_rejects_invalid_sqlite_images_and_representation_before_installation() {
    let source_root = tempfile::tempdir().unwrap();
    let (source, _) = open(source_root.path());
    standalone(&source, 1);
    create(&source).await;
    let snapshot = source.snapshot(1).await.unwrap();
    for field in ["version", "up_to_lsn", "page_size", "image"] {
        let root = tempfile::tempdir().unwrap();
        let (target, _) = open(root.path());
        target.begin_copy().unwrap();
        let mut data: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
        match field {
            "version" => data[field] = serde_json::json!(0),
            "up_to_lsn" => data[field] = serde_json::json!(-1),
            "page_size" => data[field] = serde_json::json!(512),
            "image" => data[field][0] = serde_json::json!(0),
            _ => unreachable!(),
        }
        assert!(
            target
                .install_copy(serde_json::to_vec(&data).unwrap().into())
                .is_err()
        );
        assert_eq!(target.core.store.progress().unwrap().committed_lsn, 0);
        assert!(target.finish_copy().is_err());
        assert!(target.close().await.is_err());
    }
    source.close().await.unwrap();
}

#[tokio::test]
async fn dropped_service_cancels_retained_native_waits_and_never_publishes() {
    let root = tempfile::tempdir().unwrap();
    let (service, faults) = open(root.path());
    service
        .promote(Arc::new(|_| {
            Ok(Assignment {
                lsn: 1,
                completion: Box::pin(std::future::pending()),
            })
        }))
        .unwrap();
    let mut write = execute(&service, "CREATE TABLE t (v INTEGER)");
    write.assigned().await.unwrap();
    let core = service.core.clone();
    drop(service);
    assert!(write.wait().await.is_err());
    assert!(write.wait().await.is_err());
    assert_eq!(core.store.progress().unwrap().committed_lsn, 0);
    assert_eq!(faults.lock().unwrap().len(), 1);
}
