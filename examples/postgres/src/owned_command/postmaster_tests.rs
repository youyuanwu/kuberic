use super::*;
use crate::durable::{PgDurableIdentity, PgDurableStore, StorageMode};
use crate::instance::PgInstanceManager;
use crate::testing::{ProcessProbe, allocate_port, find_pg_bin, native_identity};
use kuberic_runtime::protocol::types::ResourceUid;
use rustix::process::{
    Pid, PidfdFlags, Signal, WaitOptions, pidfd_open, pidfd_send_signal, waitpid,
};
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const OWNER_ENTRY: &str =
    "owned_command::helper_receipt::postmaster_tests::postmaster_os_owner_entry";

fn root() -> PathBuf {
    let mut nonce = [0; 8];
    rustix::rand::getrandom(&mut nonce, rustix::rand::GetRandomFlags::empty()).unwrap();
    // Keep the retained path below PostgreSQL's Unix-socket path limit.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.paw/work/sf-hosting-m3/pm3")
        .join(format!("{}-{}", std::process::id(), hex(&nonce)));
    std::fs::create_dir_all(&root).unwrap();
    root.canonicalize().unwrap()
}

fn identity() -> PgDurableIdentity {
    PgDurableIdentity {
        resource_uid: ResourceUid::new("postmaster-owner"),
        replica: native_identity(1, "postmaster-owner"),
    }
}

async fn store(root: &Path, mode: StorageMode) -> Arc<PgDurableStore> {
    Arc::new(
        PgDurableStore::open(root.join("metadata"), identity(), mode)
            .await
            .unwrap(),
    )
}

struct LivePid {
    pid: u32,
    fd: OwnedFd,
}

impl LivePid {
    fn open(pid: u32) -> Self {
        Self {
            pid,
            fd: pidfd_open(Pid::from_raw(pid as i32).unwrap(), PidfdFlags::empty()).unwrap(),
        }
    }

    fn signal(&self, signal: Signal) {
        match pidfd_send_signal(&self.fd, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(error) => panic!("exact fixture pidfd {}: {error}", self.pid),
        }
    }

    fn reaped(&self) -> bool {
        let mut fds = [rustix::event::PollFd::new(
            &self.fd,
            rustix::event::PollFlags::IN,
        )];
        rustix::event::poll(
            &mut fds,
            Some(&rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )
        .unwrap();
        fds[0].revents().contains(rustix::event::PollFlags::HUP)
    }

    async fn reap_adopted(&self) {
        until(
            || match waitpid(Pid::from_raw(self.pid as i32), WaitOptions::NOHANG) {
                Ok(Some((_, status))) => {
                    assert!(status.exited() || status.signaled());
                    true
                }
                Err(rustix::io::Errno::CHILD) => self.reaped(),
                Ok(None) | Err(rustix::io::Errno::INTR) => false,
                Err(error) => panic!("actual fixture wait {}: {error}", self.pid),
            },
        )
        .await;
        assert!(self.reaped());
    }
}

impl Drop for LivePid {
    fn drop(&mut self) {
        if !self.reaped() {
            let _ = pidfd_send_signal(&self.fd, Signal::CONT);
            let _ = pidfd_send_signal(&self.fd, Signal::KILL);
        }
    }
}

async fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "bounded postmaster witness timed out"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn spawn_owner(root: &Path, port: u16, phase: &str) -> Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", OWNER_ENTRY, "--nocapture"])
        .env("KUBERIC_POSTMASTER_OWNER", root)
        .env("KUBERIC_POSTMASTER_PORT", port.to_string())
        .env("KUBERIC_POSTMASTER_PHASE", phase)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

fn kill_owner(owner: &mut Child) {
    let process = LivePid::open(owner.id());
    process.signal(Signal::KILL);
    assert_eq!(owner.wait().unwrap().signal(), Some(9));
    assert!(
        process.reaped(),
        "SIGKILLed application owner must actually be waited"
    );
}

async fn wait_owner(owner: &mut Child) {
    let mut status = None;
    until(|| {
        status = owner.try_wait().unwrap();
        status.is_some()
    })
    .await;
    assert!(status.unwrap().success());
}

async fn local_connection(
    root: &Path,
    port: u16,
) -> (tokio_postgres::Client, tokio::task::JoinHandle<()>) {
    let (client, connection) = tokio_postgres::connect(
        &format!(
            "host={} port={port} dbname=postgres",
            root.join("data/pg_stat_tmp").display()
        ),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap();
    (
        client,
        tokio::spawn(async move {
            let _ = connection.await;
        }),
    )
}

async fn acknowledged(client: &tokio_postgres::Client) {
    let row = client
        .query_one("SELECT value FROM retained_owner WHERE id = 1", &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "acknowledged-before-host-death");
}

#[test]
fn postmaster_os_owner_entry() {
    let Some(root) = std::env::var_os("KUBERIC_POSTMASTER_OWNER") else {
        return;
    };
    let root = PathBuf::from(root);
    let port = std::env::var("KUBERIC_POSTMASTER_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let phase = std::env::var("KUBERIC_POSTMASTER_PHASE").unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let store = store(&root, StorageMode::Established).await;
            let before = store.snapshot().await;
            let instance = Arc::new(PgInstanceManager::new(
                root.join("data"),
                find_pg_bin(),
                port,
            ));
            instance.bind_generation_store(&store).await.unwrap();
            if phase != "successor" {
                instance.init_db().await.unwrap();
            }
            let (faults, _receiver) = mpsc::channel(16);
            instance.start_native(faults.clone()).await.unwrap();
            let (client, driver) = instance.connect().await.unwrap();
            if phase == "successor" {
                acknowledged(&client).await;
                client
                    .execute(
                        "INSERT INTO retained_owner VALUES (2, 'new-local-process-only')",
                        &[],
                    )
                    .await
                    .unwrap();
                assert!(
                    instance.connect_application().await.is_err(),
                    "native initialization is not application authority"
                );
                let selected = store.snapshot().await;
                assert!(
                    selected
                        .postmaster_executions
                        .starts_with(&before.postmaster_executions)
                );
                assert!(
                    selected
                        .postmaster_executions
                        .last()
                        .unwrap()
                        .launch
                        .intent
                        .process_generation
                        > before
                            .postmaster_executions
                            .last()
                            .unwrap()
                            .launch
                            .intent
                            .process_generation
                );
                assert_eq!(selected.role, before.role);
                assert_eq!(selected.native_reformations, before.native_reformations);
                assert!(selected.external_access_closed);
                drop(client);
                driver.await.unwrap();
                instance.stop().await.unwrap();
                instance.join_tasks().await.unwrap();
                store.ensure_helpers_complete().await.unwrap();
                std::fs::write(
                    root.join("successor-local-read"),
                    b"retained SQL; no framework quorum claim",
                )
                .unwrap();
                return;
            }
            client
                .batch_execute(
                    "CREATE TABLE retained_owner(id integer PRIMARY KEY, value text NOT NULL);
             INSERT INTO retained_owner VALUES (1, 'acknowledged-before-host-death');
             CHECKPOINT",
                )
                .await
                .unwrap();
            // The current exact OwnedProcess must allow live probes and idempotent
            // start without interpreting its Pending cell as an old generation.
            instance.control_identity().await.unwrap();
            instance.start_native(faults.clone()).await.unwrap();
            let selected = store.snapshot().await;
            assert_eq!(selected.postmaster_executions.len(), 1);
            assert!(selected.postmaster_executions[0].completed().is_err());
            std::fs::write(
                root.join("old-ready"),
                b"actual PostgreSQL SQL acknowledged",
            )
            .unwrap();
            if phase == "failed-join" {
                until(|| root.join("request-failed-join").exists()).await;
                let first = instance.stop().await.unwrap_err().to_string();
                let second = instance.stop().await.unwrap_err().to_string();
                assert_eq!(first, second, "cleanup errors remain sticky");
                let restart = instance.start_native(faults).await.unwrap_err().to_string();
                assert!(restart.contains(&first), "{restart} must retain {first}");
                std::fs::write(root.join("failed-join"), first).unwrap();
            }
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
}

async fn old_owner(
    root: &Path,
    port: u16,
    phase: &str,
) -> (Child, PostmasterSelection, LivePid, LivePid) {
    drop(store(root, StorageMode::Fresh).await);
    let owner = spawn_owner(root, port, phase);
    until(|| root.join("old-ready").exists()).await;
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("metadata/state-v3.json")).unwrap())
            .unwrap();
    let selected: PostmasterSelection =
        serde_json::from_value(state["state"]["postmasterExecutions"][0].clone()).unwrap();
    let supervisor = LivePid::open(selected.supervisor.pid);
    let postmaster: u32 = std::fs::read_to_string(root.join("data/postmaster.pid"))
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    (owner, selected, supervisor, LivePid::open(postmaster))
}

fn retained_state(root: &Path) -> crate::durable::PgDurableState {
    let envelope: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("metadata/state-v3.json")).unwrap())
            .unwrap();
    serde_json::from_value(envelope["state"].clone()).unwrap()
}

async fn denied_successor(root: &Path) {
    let before = retained_state(root);
    let bytes = std::fs::read(root.join("metadata/state-v3.json")).unwrap();
    let clock = std::fs::read(root.join("metadata/process-generation-v2")).unwrap();
    let config = std::fs::read(root.join("data/postgresql.conf")).unwrap();
    assert!(
        matches!(PgDurableStore::open(root.join("metadata"), identity(), StorageMode::Established).await,
            Err(crate::durable::PgDurableError::Invalid(message)) if message.contains("old native postmaster exclusion unproven")),
        "no native owner for pre-bind repair or initialization before exclusion"
    );
    assert_eq!(
        std::fs::read(root.join("metadata/state-v3.json")).unwrap(),
        bytes,
        "no clock/selection/mutation before exclusion"
    );
    assert_eq!(
        std::fs::read(root.join("data/postgresql.conf")).unwrap(),
        config
    );
    assert_eq!(
        std::fs::read(root.join("metadata/process-generation-v2")).unwrap(),
        clock
    );
    assert_eq!(
        std::fs::metadata(root.join("data")).unwrap().ino(),
        before.postmaster_executions[0]
            .launch
            .intent
            .data_root
            .inode
    );
}

#[tokio::test]
async fn m3_pg_postmaster_real_host_death_requires_survivor_echild_before_fresh_native_read() {
    rustix::process::set_child_subreaper(Some(rustix::process::getpid())).unwrap();
    let root = root();
    let port = allocate_port().await;
    let (mut owner, selected, supervisor, postmaster) = old_owner(&root, port, "running").await;
    let (client, connection) = local_connection(&root, port).await;
    let backend = LivePid::open(
        client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0) as u32,
    );
    let lineage = ProcessProbe::postgres(&root.join("data"));
    supervisor.signal(Signal::STOP);
    until(|| {
        std::fs::read_to_string(format!("/proc/{}/stat", supervisor.pid))
            .unwrap()
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .next()
            == Some("T")
    })
    .await;
    kill_owner(&mut owner);
    assert!(!postmaster.reaped());
    assert!(!backend.reaped());
    acknowledged(&client).await;
    assert!(selected.completed().is_err());
    denied_successor(&root).await;
    let before = retained_state(&root);
    supervisor.signal(Signal::CONT);
    until(|| selected.completed().is_ok()).await;
    supervisor.reap_adopted().await;
    assert!(postmaster.reaped());
    assert!(backend.reaped());
    lineage.assert_reaped();
    drop(client);
    connection.await.unwrap();
    let outcome = selected.completed().unwrap();
    assert!(outcome.dispatched && outcome.cancelled);
    let mut successor = spawn_owner(&root, port, "successor");
    wait_owner(&mut successor).await;
    assert!(root.join("successor-local-read").exists());
    let after = store(&root, StorageMode::Established)
        .await
        .snapshot()
        .await;
    assert_eq!(after.postmaster_executions[0], selected);
    assert_eq!(after.native_reformations, before.native_reformations);
    assert_eq!(
        std::fs::metadata(root.join("data")).unwrap().ino(),
        selected.launch.intent.data_root.inode
    );
    assert!(
        after
            .postmaster_executions
            .iter()
            .all(|selection| selection.completed().is_ok())
    );
    eprintln!(
        "postmaster custody root={} old-host={} SIGKILL+wait supervisor={} ECHILD+wait postmaster={} backend={} pidfd-HUP successor-host={} actual native read; no original-quorum/framework claim cell={}",
        root.display(),
        owner.id(),
        supervisor.pid,
        postmaster.pid,
        backend.pid,
        successor.id(),
        selected.launch.cell_path().display(),
    );
}

#[tokio::test]
async fn m3_pg_postmaster_supervisor_loss_failed_join_and_pid_absence_remain_blocked() {
    rustix::process::set_child_subreaper(Some(rustix::process::getpid())).unwrap();
    let root = root();
    let port = allocate_port().await;
    let (mut owner, selected, supervisor, postmaster) = old_owner(&root, port, "failed-join").await;
    let (client, connection) = local_connection(&root, port).await;
    let backend = LivePid::open(
        client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0) as u32,
    );
    let lineage = ProcessProbe::postgres(&root.join("data"));
    supervisor.signal(Signal::KILL);
    std::fs::write(root.join("request-failed-join"), b"actual lost supervisor").unwrap();
    until(|| root.join("failed-join").exists()).await;
    assert!(
        supervisor.reaped(),
        "actual failed stop reaps killed supervisor but cannot certify its tree"
    );
    kill_owner(&mut owner);
    assert!(!postmaster.reaped());
    acknowledged(&client).await;
    denied_successor(&root).await;
    // Explicit fixture teardown uses only retained live pidfds; it cannot write
    // the lost supervisor's selected durable receipt.
    postmaster.signal(Signal::INT);
    postmaster.reap_adopted().await;
    assert!(backend.reaped());
    lineage.assert_reaped();
    drop(client);
    connection.await.unwrap();
    assert_eq!(
        std::fs::metadata(selected.launch.cell_path())
            .unwrap()
            .len(),
        0
    );
    denied_successor(&root).await;
    assert!(
        selected.completed().is_err(),
        "PID absence/fixture joins cannot replace ECHILD receipt"
    );
    eprintln!(
        "postmaster supervisor-loss custody root={} old-host={} SIGKILL+wait supervisor={} killed/reaped actual failed+sticky stop retained postmaster={} backend={} fixture-reaped; cell Pending",
        root.display(),
        owner.id(),
        supervisor.pid,
        postmaster.pid,
        backend.pid,
    );
}

#[tokio::test]
async fn m3_pg_postmaster_cancel_and_drop_complete_actual_owned_tree() {
    let root = root();
    let store = store(&root, StorageMode::Fresh).await;
    let port = allocate_port().await;
    let instance = Arc::new(PgInstanceManager::new(
        root.join("data"),
        find_pg_bin(),
        port,
    ));
    instance.bind_generation_store(&store).await.unwrap();
    instance.init_db().await.unwrap();
    let (faults, _receiver) = mpsc::channel(16);
    let cancellation = tokio_util::sync::CancellationToken::new();
    instance
        .start_native_with_cancellation(faults.clone(), cancellation.clone())
        .await
        .unwrap();
    let first = store.snapshot().await.postmaster_executions[0].clone();
    let processes = ProcessProbe::postgres(instance.data_dir());
    cancellation.cancel();
    until(|| first.completed().is_ok()).await;
    instance.stop().await.unwrap();
    instance.join_tasks().await.unwrap();
    processes.assert_reaped();
    drop(instance);
    let instance = PgInstanceManager::new(root.join("data"), find_pg_bin(), port);
    instance.bind_generation_store(&store).await.unwrap();
    instance.start_native(faults).await.unwrap();
    let processes = ProcessProbe::postgres(instance.data_dir());
    let selected = store
        .snapshot()
        .await
        .postmaster_executions
        .last()
        .unwrap()
        .clone();
    drop(instance);
    selected.completed().unwrap();
    processes.assert_reaped();
    store.ensure_helpers_complete().await.unwrap();
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn m3_pg_postmaster_cancelled_start_selection_is_undispatched_until_actual_stop_receipt() {
    let root = root();
    let store = store(&root, StorageMode::Fresh).await;
    let instance = PgInstanceManager::new(root.join("data"), find_pg_bin(), allocate_port().await);
    instance.bind_generation_store(&store).await.unwrap();
    instance.init_db().await.unwrap();
    let gate = store.pause_commit(crate::durable::CommitStage::CommittedBeforePublish, false);
    let (faults, _receiver) = mpsc::channel(16);
    let mut start = Box::pin(instance.start_native(faults.clone()));
    tokio::select! {
        _ = gate.entered.notified() => {}
        result = &mut start => panic!("start completed before selected commit boundary: {result:?}"),
    }
    assert!(!root.join("data/postmaster.pid").exists());
    drop(start);
    let selected = store.snapshot().await.postmaster_executions[0].clone();
    assert!(selected.completed().is_err());
    assert!(store.ensure_helpers_complete().await.is_err());
    instance.stop().await.unwrap();
    let outcome = selected.completed().unwrap();
    assert!(!outcome.dispatched);
    assert!(outcome.cancelled);
    assert!(!root.join("data/postmaster.pid").exists());
    instance.start_native(faults).await.unwrap();
    instance.stop().await.unwrap();
    instance.join_tasks().await.unwrap();
    store.ensure_helpers_complete().await.unwrap();
}

#[tokio::test]
async fn m3_pg_postmaster_stable_ancestor_alias_is_pinned_before_start() {
    let root = root();
    std::fs::create_dir(root.join("real")).unwrap();
    std::os::unix::fs::symlink(root.join("real"), root.join("a")).unwrap();
    let path = root.join("a/d");
    let store = store(&root, StorageMode::Fresh).await;
    let instance = PgInstanceManager::new(path.clone(), find_pg_bin(), allocate_port().await);
    instance.bind_generation_store(&store).await.unwrap();
    instance.init_db().await.unwrap();
    let (faults, _receiver) = mpsc::channel(16);
    instance.start_native(faults).await.unwrap();
    let selected = store.snapshot().await.postmaster_executions[0].clone();
    assert_eq!(selected.launch.intent.data_path, path);
    assert_eq!(selected.launch.intent.data_root.path, root.join("real/d"));
    instance.control_identity().await.unwrap();
    instance.stop().await.unwrap();
    instance.join_tasks().await.unwrap();
    selected.completed().unwrap();
    // Both directories and the original alias are retained. The selected
    // command cannot follow a newly substituted ancestor before dispatch.
    let command = Command::new(find_pg_bin().join("postgres"));
    let intent = PostmasterIntent::new(1, &path, &command).unwrap();
    std::fs::create_dir_all(root.join("other/d/global")).unwrap();
    std::fs::write(root.join("other/d/global/pg_control"), b"foreign").unwrap();
    std::fs::rename(root.join("a"), root.join("retained-alias")).unwrap();
    std::os::unix::fs::symlink(root.join("other"), root.join("a")).unwrap();
    assert!(intent.revalidate(&command).is_err());
    assert!(root.join("real/d/PG_VERSION").exists());
}

#[tokio::test]
async fn m3_postmaster_required_history_and_receipt_identity_denials() {
    for kind in [
        "pending",
        "missing",
        "foreign",
        "nonce",
        "process-generation",
        "execution-generation",
        "data-root",
        "data-path",
        "command",
        "owner",
        "supervisor",
        "echild",
    ] {
        let root = root();
        std::fs::create_dir_all(root.join("data/global")).unwrap();
        std::fs::write(
            root.join("data/global/pg_control"),
            b"receipt-validation-only-not-real-PG",
        )
        .unwrap();
        let store = store(&root, StorageMode::Fresh).await;
        let command = Command::new("true");
        let launch = store
            .postmaster_launch(PostmasterIntent::new(1, &root.join("data"), &command).unwrap())
            .await
            .unwrap();
        let mut cell = CompletionCell::create(launch).unwrap();
        let selection = cell.selection.clone();
        store.select_postmaster(selection.clone()).await.unwrap();
        let path = selection.launch.cell_path();
        if kind == "pending" {
            assert!(store.ensure_helpers_complete().await.is_err());
            let before = store.snapshot().await;
            let instance =
                PgInstanceManager::new(root.join("data"), root.join("unavailable-tools"), 1);
            assert!(instance.bind_generation_store(&store).await.is_err());
            assert!(instance.init_db().await.is_err());
            assert!(instance.control_identity().await.is_err());
            let (faults, _receiver) = mpsc::channel(16);
            assert!(instance.start_native(faults).await.is_err());
            assert_eq!(store.snapshot().await, before);
            assert!(!root.join("data/PG_VERSION").exists());
            continue;
        }

        let mut unrelated = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let unrelated_pid = LivePid::open(unrelated.id());
        let mut receipt = Completion {
            selection: selection.clone(),
            outcome: HelperOutcome {
                dispatched: true,
                cancelled: true,
                launcher_status: 9,
            },
            echild: true,
            checksum: String::new(),
        };
        match kind {
            "nonce" => receipt.selection.launch.nonce[0] ^= 1,
            "process-generation" => receipt.selection.launch.intent.process_generation += 1,
            "execution-generation" => receipt.selection.launch.execution_generation += 1,
            "data-root" => receipt.selection.launch.intent.data_root.inode += 1,
            "data-path" => receipt.selection.launch.intent.data_path = root.join("foreign"),
            "command" => receipt.selection.launch.intent.command_digest = "0".repeat(64),
            "owner" => {
                receipt.selection.launch.intent.owner = OsIdentity::read(unrelated.id()).unwrap()
            }
            "supervisor" => {
                receipt.selection.supervisor = OsIdentity::read(unrelated.id()).unwrap()
            }
            "echild" => receipt.echild = false,
            "missing" | "foreign" => {}
            _ => unreachable!(),
        }
        receipt.checksum = receipt.digest().unwrap();
        cell.file
            .write_all(&serde_json::to_vec(&receipt).unwrap())
            .unwrap();
        cell.file.sync_all().unwrap();
        if kind == "missing" || kind == "foreign" {
            std::fs::rename(&path, root.join("retained-receipt")).unwrap();
            if kind == "foreign" {
                std::fs::write(path, serde_json::to_vec(&receipt).unwrap()).unwrap();
            }
        }
        assert!(selection.completed().is_err(), "{kind}");
        assert!(store.ensure_helpers_complete().await.is_err(), "{kind}");
        let before = store.snapshot().await;
        assert!(
            store
                .update(|state| {
                    state.postmaster_executions.clear();
                    Ok(())
                })
                .await
                .is_err()
        );
        assert_eq!(store.snapshot().await, before);
        let mut body = serde_json::to_value(&before).unwrap();
        body.as_object_mut().unwrap().remove("postmasterExecutions");
        assert!(serde_json::from_value::<crate::durable::PgDurableState>(body).is_err());
        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "foreign stored identity was never signalled"
        );
        unrelated_pid.signal(Signal::KILL);
        unrelated.wait().unwrap();
    }
}

#[tokio::test]
async fn m3_postmaster_state_validates_without_any_selected_repair_helper() {
    let root = root();
    std::fs::create_dir_all(root.join("data/global")).unwrap();
    std::fs::write(root.join("data/global/pg_control"), b"validation-only").unwrap();
    let store = store(&root, StorageMode::Fresh).await;
    let command = Command::new("true");
    let launch = store
        .postmaster_launch(PostmasterIntent::new(1, &root.join("data"), &command).unwrap())
        .await
        .unwrap();
    let cell = CompletionCell::create(launch).unwrap();
    let selection = cell.selection.clone();
    store.select_postmaster(selection.clone()).await.unwrap();
    let before = store.snapshot().await;
    assert!(before.helper_executions.is_empty());
    for kind in [
        "generation",
        "process-generation",
        "owner",
        "supervisor",
        "cell",
        "bound",
    ] {
        let mut next = selection.clone();
        next.launch.execution_generation = 2;
        next.launch.intent.process_generation = 2;
        match kind {
            "generation" => next.launch.execution_generation = 3,
            "process-generation" => next.launch.intent.process_generation = 1,
            "owner" => next.launch.intent.owner.pid = 0,
            "supervisor" => next.supervisor.start = 0,
            "cell" => next.inode = 0,
            "bound" => {}
            _ => unreachable!(),
        }
        assert!(
            store
                .update(|state| {
                    state.postmaster_executions.push(next);
                    if kind == "bound" {
                        state
                            .postmaster_executions
                            .resize(MAX_EXECUTIONS + 1, selection.clone());
                    }
                    Ok(())
                })
                .await
                .is_err(),
            "{kind}"
        );
        assert_eq!(store.snapshot().await, before, "{kind}");
    }
}
