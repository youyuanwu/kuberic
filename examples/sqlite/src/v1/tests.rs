use super::*;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use futures::FutureExt;
use rusqlite::types::Value;

fn application_tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        let relative = path.strip_prefix(root).unwrap().to_owned();
        let metadata = fs::symlink_metadata(path).unwrap();
        if metadata.is_dir() {
            entries.insert(relative, None);
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), entries);
            }
        } else {
            assert!(metadata.is_file());
            entries.insert(relative, Some(fs::read(path).unwrap()));
        }
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

fn manifest(root: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(root.join("state-v2.json")).unwrap()).unwrap()
}

fn active_file(root: &Path, base: bool) -> PathBuf {
    let generation = manifest(root)["state"]["generation"].as_u64().unwrap();
    root.join(if base {
        format!("base-{generation}.sqlite")
    } else {
        format!("history-{generation}.log")
    })
}

fn assert_rejected_open_preserves_tree(root: &Path, expected: &str) {
    let before = application_tree(root);
    for _ in 0..2 {
        let result = Committer::open(
            root.into(),
            Arc::new(|_| panic!("rejected Open must not check serving access")),
            Arc::new(|_| panic!("rejected Open must not start a service worker")),
        );
        let error = result.err().expect("corrupt storage must reject Open");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(
            error.to_string().contains(expected),
            "unexpected Open error: {error}"
        );
        let after = application_tree(root);
        assert_eq!(
            after.keys().collect::<Vec<_>>(),
            before.keys().collect::<Vec<_>>()
        );
        for (path, bytes) in &before {
            assert!(after[path] == *bytes, "Open changed {path:?}");
        }
    }
}

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

#[tokio::test]
async fn native_cancellation_before_service_abort_is_typed_and_not_a_new_fault() {
    for cancelled in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let (service, faults) = open(root.path());
        service
            .promote(Arc::new(move |_| {
                Ok(Assignment {
                    lsn: 1,
                    completion: Box::pin(async move {
                        Err(if cancelled {
                            io::Error::other(kuberic_runtime::RuntimeError::OperationCancelled)
                        } else {
                            io::Error::new(io::ErrorKind::Interrupted, "ordinary worker failure")
                        })
                    }),
                })
            }))
            .unwrap();
        let mut write = execute(&service, "CREATE TABLE t (v INTEGER)");
        assert_eq!(is_abort(&write.wait().await.unwrap_err()), cancelled);
        assert_eq!(is_abort(&write.wait().await.unwrap_err()), cancelled);
        assert_eq!(service.core.store.progress().unwrap().committed_lsn, 0);
        assert_eq!(faults.lock().unwrap().is_empty(), cancelled);
        assert_eq!(is_abort(&service.close().await.unwrap_err()), cancelled);
    }
}

#[tokio::test]
async fn prior_fault_is_not_hidden_by_later_native_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let (service, faults) = open(root.path());
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (started, released) = (entered.clone(), release.clone());
    service
        .promote(Arc::new(move |_| {
            let (started, released) = (started.clone(), released.clone());
            Ok(Assignment {
                lsn: 1,
                completion: Box::pin(async move {
                    started.notify_one();
                    released.notified().await;
                    Err(io::Error::other(
                        kuberic_runtime::RuntimeError::OperationCancelled,
                    ))
                }),
            })
        }))
        .unwrap();
    let mut write = execute(&service, "CREATE TABLE t (v INTEGER)");
    entered.notified().await;
    service.core.fault("prior application failure".into());
    release.notify_one();
    let error = write.wait().await.unwrap_err();
    assert!(!is_abort(&error));
    assert!(error.to_string().contains("prior application failure"));
    assert!(!is_abort(&service.close().await.unwrap_err()));
    assert_eq!(faults.lock().unwrap().len(), 1);
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
async fn retained_secondary_requires_exact_native_boundary_and_preserves_sql_without_copy() {
    let root = tempfile::tempdir().unwrap();
    let (source, _) = open(root.path());
    standalone(&source, 1);
    create(&source).await;
    execute(&source, "INSERT INTO t VALUES (7)")
        .wait()
        .await
        .unwrap();
    source.close().await.unwrap();
    drop(source);
    let (restored, faults) = open(root.path());
    let before = application_tree(root.path());
    assert!(restored.restore_retained_secondary(-1).is_err());
    assert!(restored.restore_retained_secondary(1).is_err());
    assert!(restored.restore_retained_secondary(3).is_err());
    assert_eq!(application_tree(root.path()), before);
    restored.restore_retained_secondary(2).unwrap();
    assert_eq!(application_tree(root.path()), before);
    assert!(restored.restore_retained_secondary(2).is_err());
    assert_eq!(restored.progress().unwrap(), 2);
    assert_eq!(
        restored.query("SELECT v FROM t").unwrap(),
        vec![vec![Value::Integer(7)]]
    );
    assert!(faults.lock().unwrap().is_empty());
    restored.close().await.unwrap();
    assert!(restored.restore_retained_secondary(2).is_err());
    let root = tempfile::tempdir().unwrap();
    let (copy, _) = open(root.path());
    copy.begin_copy().unwrap();
    assert!(copy.restore_retained_secondary(0).is_err());
    assert!(copy.finish_copy().is_err());
    copy.close().await.unwrap();
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
            assert!(super::is_abort(&service.close().await.unwrap_err()));
            assert!(faults.lock().unwrap().is_empty());
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
    fs::write(
        active_file(root.path(), false),
        b"corrupt acknowledged history",
    )
    .unwrap();
    assert_rejected_open_preserves_tree(root.path(), "acknowledged history is missing");
}

#[tokio::test]
async fn corrupt_supported_base_and_history_reject_open_without_mutation() {
    for base in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (service, _) = open(root.path());
        standalone(&service, 1);
        create(&service).await;
        execute(&service, "INSERT INTO t VALUES (7)")
            .wait()
            .await
            .unwrap();
        service.close().await.unwrap();
        drop(service);
        if base {
            let (service, _) = open(root.path());
            service.close().await.unwrap();
            drop(service);
        }
        let state = manifest(root.path());
        assert_eq!(state["state"]["version"], 3);
        assert_eq!(state["state"]["applied_lsn"], 2);
        assert_eq!(state["state"]["committed_lsn"], 2);
        let path = active_file(root.path(), base);
        let mut bytes = fs::read(&path).unwrap();
        bytes[if base { 0 } else { 8 }] ^= 1;
        fs::write(path, bytes).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(active_file(root.path(), false))
            .unwrap()
            .write_all(b"unaccepted torn tail")
            .unwrap();
        fs::create_dir(root.path().join("retained")).unwrap();
        fs::write(root.path().join("retained/diagnostic"), b"preserve me").unwrap();
        assert_rejected_open_preserves_tree(
            root.path(),
            if base {
                "durable base corruption"
            } else {
                "acknowledged history checksum mismatch"
            },
        );
    }
}

#[tokio::test]
async fn invalid_supported_sqlite_images_reject_open_before_cleanup_or_base_publication() {
    use crate::framelog::durable::{Record, rewrite_manifest_for_test};

    for base in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (service, _) = open(root.path());
        standalone(&service, 1);
        create(&service).await;
        service.close().await.unwrap();
        drop(service);
        if base {
            let (service, _) = open(root.path());
            service.close().await.unwrap();
            drop(service);
            let path = active_file(root.path(), true);
            let mut image = fs::read(&path).unwrap();
            image[0] ^= 1;
            fs::write(path, &image).unwrap();
            rewrite_manifest_for_test(root.path(), |state| {
                state["base_checksum"] = crc32fast::hash(&image).into();
            });
        } else {
            let path = active_file(root.path(), false);
            let history = fs::read(&path).unwrap();
            let len = u32::from_le_bytes(history[..4].try_into().unwrap()) as usize;
            let mut record: Record = serde_json::from_slice(&history[4..4 + len]).unwrap();
            let mut frames: WalFrameSet = serde_json::from_slice(&record.data).unwrap();
            frames
                .frames
                .iter_mut()
                .find(|frame| frame.page_number == 1)
                .unwrap()
                .data[0] ^= 1;
            frames.checksum = WalFrameSet::compute_checksum(&frames.frames);
            record.data = serde_json::to_vec(&frames).unwrap();
            let payload = serde_json::to_vec(&record).unwrap();
            let mut history = Vec::new();
            history.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            history.extend_from_slice(&payload);
            history.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
            fs::write(path, &history).unwrap();
            rewrite_manifest_for_test(root.path(), |state| {
                state["history_len"] = history.len().into();
            });
        }
        fs::OpenOptions::new()
            .append(true)
            .open(active_file(root.path(), false))
            .unwrap()
            .write_all(b"unaccepted torn tail")
            .unwrap();
        assert_rejected_open_preserves_tree(root.path(), "not a database");
    }
}

#[tokio::test]
async fn healthy_and_reconciliation_restarts_preserve_committed_sql_and_ignore_torn_tails() {
    for reconcile in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (service, _) = open(root.path());
        standalone(&service, 1);
        create(&service).await;
        execute(&service, "INSERT INTO t VALUES (7)")
            .wait()
            .await
            .unwrap();
        if reconcile {
            service
                .core
                .store
                .require_reconciliation("publication outcome unresolved".into())
                .unwrap();
        }
        service.close().await.unwrap();
        drop(service);
        fs::OpenOptions::new()
            .append(true)
            .open(active_file(root.path(), false))
            .unwrap()
            .write_all(b"unaccepted torn tail")
            .unwrap();
        let (service, faults) = open(root.path());
        assert_eq!(service.progress().unwrap(), 2);
        assert_eq!(
            service.core.store.recovery_state().unwrap(),
            crate::RecoveryState::Healthy
        );
        standalone(&service, 3);
        assert_eq!(
            service.query("SELECT v FROM t").unwrap(),
            vec![vec![Value::Integer(7)]]
        );
        assert_eq!(
            execute(&service, "INSERT INTO t VALUES (8)")
                .wait()
                .await
                .unwrap(),
            3
        );
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
        service.close().await.unwrap();
    }
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
    assert!(faults.lock().unwrap().is_empty());
}
