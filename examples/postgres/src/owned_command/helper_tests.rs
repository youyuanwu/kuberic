use super::*;
use crate::durable::{PgDurableIdentity, PgDurableStore, StorageMode};
use crate::owned_command::OwnedCommands;
use crate::owned_process::OwnedProcess;
use kuberic_runtime::protocol::types::{
    AgentGeneration, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ResourceUid,
};
use rustix::process::{
    Pid, PidfdFlags, Signal, WaitOptions, pidfd_open, pidfd_send_signal, waitpid,
};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const OWNER_ENTRY: &str = "owned_command::helper_receipt::tests::native_helper_os_owner_entry";

fn root(name: &str) -> PathBuf {
    let mut nonce = [0; 16];
    rustix::rand::getrandom(&mut nonce, rustix::rand::GetRandomFlags::empty()).unwrap();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.paw/work/sf-hosting-m3/native-helper-fixtures")
        .join(format!("{name}-{}-{}", std::process::id(), hex(&nonce)));
    std::fs::create_dir_all(root.join("head/global")).unwrap();
    std::fs::write(root.join("head/global/pg_control"), b"retained-control").unwrap();
    std::fs::write(root.join("head/acknowledged"), b"must-survive").unwrap();
    root.canonicalize().unwrap()
}

fn identity() -> PgDurableIdentity {
    PgDurableIdentity {
        resource_uid: ResourceUid::new("native-helper"),
        replica: ReplicaIdentity {
            replica_id: ReplicaId::new(1),
            instance_id: ReplicaInstanceId::new("native-helper"),
            agent_generation: AgentGeneration::new("native-helper"),
        },
    }
}

async fn fresh(root: &Path) -> Arc<PgDurableStore> {
    Arc::new(
        PgDurableStore::open(root.join("metadata"), identity(), StorageMode::Fresh)
            .await
            .unwrap(),
    )
}

fn intent(root: &Path, command: &Command) -> HelperIntent {
    HelperIntent::new(
        1,
        [7; 16],
        &root.join("head"),
        &root.join("candidate"),
        command,
    )
    .unwrap()
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "bounded native-helper OS witness timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
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
            Err(error) => panic!("pidfd signal {}: {error}", self.pid),
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

    fn reap_adopted(&self) {
        wait_until(
            || match waitpid(Pid::from_raw(self.pid as i32), WaitOptions::NOHANG) {
                Ok(Some(_)) => true,
                Err(rustix::io::Errno::CHILD) => self.reaped(),
                Ok(None) | Err(rustix::io::Errno::INTR) => false,
                Err(error) => panic!("wait exact adopted PID {}: {error}", self.pid),
            },
        );
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

fn spawn_owner(root: &Path, phase: &str) -> Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", OWNER_ENTRY, "--nocapture"])
        .env("KUBERIC_NATIVE_HELPER_OS_OWNER", root)
        .env("KUBERIC_NATIVE_HELPER_OS_PHASE", phase)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

fn kill_owner(owner: &mut Child) {
    let pid = LivePid::open(owner.id());
    pid.signal(Signal::KILL);
    let status = owner.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(9));
    assert!(pid.reaped(), "old host was actually waited/reaped");
}

async fn selected(root: &Path) -> HelperSelection {
    let path = root.join("metadata/state-v3.json");
    let mut selected = None;
    wait_until(|| {
        selected = std::fs::read(&path).ok().and_then(|bytes| {
            let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            serde_json::from_value(value["state"]["helperExecutions"][0].clone()).ok()
        });
        selected.is_some()
    });
    selected.unwrap()
}

fn helper_command(root: &Path, phase: &str) -> Command {
    let script = r#"
import os, pathlib, sys, time
root = pathlib.Path(sys.argv[1])
for fd in pathlib.Path('/proc/self/fd').iterdir():
    try:
        target = os.readlink(fd)
    except FileNotFoundError:
        continue
    assert 'state-v3.helper-' not in target, 'completion FD leaked into native tool'
(root / 'private-fd-checked').write_text('no completion FD inherited')
(root / 'effect').write_text('actual-native-effect')
(root / 'launcher').write_text(str(os.getpid()))
if os.fork() == 0:
    os.setsid()
    if os.fork() != 0:
        os._exit(0)
    (root / 'descendant').write_text(str(os.getpid()))
    while True:
        time.sleep(0.01)
if sys.argv[2] == 'effect-before-receipt':
    os._exit(37)
while True:
    time.sleep(0.01)
"#;
    let mut command = Command::new("python3");
    command.arg("-c").arg(script).arg(root).arg(phase);
    command
}

/// This body is also executed as an actual disposable application-owner OS
/// process. No in-process cancellation/death hook stands in for its SIGKILL.
#[test]
fn native_helper_os_owner_entry() {
    let Some(root) = std::env::var_os("KUBERIC_NATIVE_HELPER_OS_OWNER") else {
        return;
    };
    let root = PathBuf::from(root);
    let phase = std::env::var("KUBERIC_NATIVE_HELPER_OS_PHASE").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let store =
            PgDurableStore::open(root.join("metadata"), identity(), StorageMode::Established)
                .await
                .unwrap();
        let mut command = helper_command(&root, &phase);
        if phase == "before-start" {
            let launch = store.helper_launch(intent(&root, &command)).await.unwrap();
            let process = OwnedProcess::spawn_helper(&command, &launch).unwrap();
            store
                .select_helper(process.helper_selection().unwrap().clone())
                .await
                .unwrap();
            std::fs::write(root.join("owner-selected"), b"before-START").unwrap();
            loop {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        OwnedCommands::default()
            .output_selected(
                &mut command,
                &store,
                intent(&root, &helper_command(&root, &phase)),
            )
            .await
            .unwrap();
        panic!("held helper returned before the OS owner was killed");
    });
}

#[tokio::test]
async fn native_helper_real_owner_death_reaps_setsid_double_fork_before_successor_effects() {
    rustix::process::set_child_subreaper(Some(rustix::process::getpid())).unwrap();
    for phase in ["running", "effect-before-receipt", "before-start"] {
        let root = root(phase);
        drop(fresh(&root).await);
        let mut owner = spawn_owner(&root, phase);
        let selection = selected(&root).await;
        let supervisor = LivePid::open(selection.supervisor.pid);
        let children = if phase == "before-start" {
            wait_until(|| root.join("owner-selected").exists());
            Vec::new()
        } else {
            wait_until(|| root.join("descendant").exists());
            assert!(root.join("private-fd-checked").exists());
            let launcher: u32 = std::fs::read_to_string(root.join("launcher"))
                .unwrap()
                .parse()
                .unwrap();
            let descendant: u32 = std::fs::read_to_string(root.join("descendant"))
                .unwrap()
                .parse()
                .unwrap();
            let stat = std::fs::read_to_string(format!("/proc/{descendant}/stat")).unwrap();
            let fields: Vec<_> = stat
                .rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .collect();
            assert_eq!(
                fields[1].parse::<u32>().unwrap(),
                supervisor.pid,
                "double-fork orphan belongs to actual subreaper"
            );
            assert_ne!(
                fields[3].parse::<u32>().unwrap(),
                launcher,
                "descendant has an independent session"
            );
            let mut children = vec![LivePid::open(descendant)];
            if phase == "running" {
                children.push(LivePid::open(launcher));
            }
            children
        };
        supervisor.signal(Signal::STOP);
        kill_owner(&mut owner);
        eprintln!(
            "native-helper custody: host={} waited; supervisor={} phase={phase}; descendants={:?}; cell={}",
            owner.id(),
            supervisor.pid,
            children.iter().map(|pid| pid.pid).collect::<Vec<_>>(),
            selection.launch.cell_path().display()
        );
        assert!(
            selection.completed().is_err(),
            "no receipt while supervisor/lineage is held"
        );
        let store =
            PgDurableStore::open(root.join("metadata"), identity(), StorageMode::Established)
                .await
                .unwrap();
        assert!(store.ensure_helpers_complete().await.is_err());
        let mut successor = Command::new("sh");
        successor
            .args(["-c", "printf successor > \"$1\"", "sh"])
            .arg(root.join("successor-effect"));
        let successor_intent = intent(&root, &successor);
        assert!(
            OwnedCommands::default()
                .output_selected(&mut successor, &store, successor_intent.clone())
                .await
                .is_err()
        );
        assert!(!root.join("successor-effect").exists());
        let state_before = store.snapshot().await;
        let instance = crate::instance::PgInstanceManager::new(
            root.join("head"),
            root.join("unavailable-tools"),
            1,
        );
        assert!(
            instance
                .bind_generation_store(&Arc::new(store))
                .await
                .is_err()
        );
        supervisor.signal(Signal::CONT);
        wait_until(|| selection.completed().is_ok());
        supervisor.reap_adopted();
        for child in children {
            assert!(child.reaped(), "receipt requires actual descendant reaping");
        }
        let outcome = selection.completed().unwrap();
        assert!(outcome.cancelled);
        assert_eq!(outcome.dispatched, phase != "before-start");
        if phase == "effect-before-receipt" {
            assert_eq!(
                std::process::ExitStatus::from_raw(outcome.launcher_status).code(),
                Some(37)
            );
        }
        if phase == "before-start" {
            assert!(
                !root.join("effect").exists(),
                "undispatched EOF never launches"
            );
        }
        let store =
            PgDurableStore::open(root.join("metadata"), identity(), StorageMode::Established)
                .await
                .unwrap();
        store.ensure_helpers_complete().await.unwrap();
        let output = OwnedCommands::default()
            .output_selected(&mut successor, &store, successor_intent)
            .await
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            std::fs::read(root.join("successor-effect")).unwrap(),
            b"successor"
        );
        assert_eq!(
            std::fs::read(root.join("head/acknowledged")).unwrap(),
            b"must-survive"
        );
        let after = store.snapshot().await;
        assert_eq!(
            after.helper_executions[0], state_before.helper_executions[0],
            "old selected execution is immutable"
        );
        assert_eq!(after.native_reformations, state_before.native_reformations);
    }
}

use std::os::unix::process::ExitStatusExt;

#[tokio::test]
async fn native_helper_real_supervisor_loss_stays_blocked_even_after_pid_absence() {
    rustix::process::set_child_subreaper(Some(rustix::process::getpid())).unwrap();
    let root = root("supervisor-loss");
    drop(fresh(&root).await);
    let mut owner = spawn_owner(&root, "running");
    let selection = selected(&root).await;
    wait_until(|| root.join("descendant").exists());
    let supervisor = LivePid::open(selection.supervisor.pid);
    let children: Vec<_> = ["launcher", "descendant"]
        .iter()
        .map(|name| {
            LivePid::open(
                std::fs::read_to_string(root.join(name))
                    .unwrap()
                    .parse()
                    .unwrap(),
            )
        })
        .collect();
    supervisor.signal(Signal::STOP);
    kill_owner(&mut owner);
    supervisor.signal(Signal::KILL);
    supervisor.reap_adopted();
    assert!(children.iter().all(|child| !child.reaped()));
    let store = PgDurableStore::open(root.join("metadata"), identity(), StorageMode::Established)
        .await
        .unwrap();
    assert!(store.ensure_helpers_complete().await.is_err());
    for child in &children {
        child.signal(Signal::KILL);
        child.reap_adopted();
    }
    wait_until(|| match rustix::process::wait(WaitOptions::NOHANG) {
        Err(rustix::io::Errno::CHILD) => true,
        Ok(_) | Err(rustix::io::Errno::INTR) => false,
        Err(error) => panic!("reap fixture lineage: {error}"),
    });
    // Test cleanup is not the lost supervisor's ECHILD receipt.
    assert!(store.ensure_helpers_complete().await.is_err());
    assert_eq!(
        std::fs::metadata(selection.launch.cell_path())
            .unwrap()
            .len(),
        0
    );
    let mut command = Command::new("sh");
    command.arg("-c").arg("exit 0");
    let selected_intent = intent(&root, &command);
    assert!(
        OwnedCommands::default()
            .output_selected(&mut command, &store, selected_intent)
            .await
            .is_err()
    );
    eprintln!(
        "native-helper supervisor-loss custody: host={} waited; supervisor={} waited; helpers={:?} explicitly reaped by fixture; durable cell remains Pending",
        owner.id(),
        supervisor.pid,
        children.iter().map(|pid| pid.pid).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn native_helper_receipts_refuse_missing_torn_foreign_aliased_and_wrong_identity() {
    for kind in [
        "missing",
        "torn",
        "foreign",
        "symlink",
        "hardlink",
        "nonce",
        "head",
        "command",
        "supervisor",
    ] {
        let root = root(kind);
        let store = fresh(&root).await;
        let command = Command::new("true");
        let launch = store.helper_launch(intent(&root, &command)).await.unwrap();
        let cell = CompletionCell::create(launch).unwrap();
        let selection = cell.selection.clone();
        store.select_helper(selection.clone()).await.unwrap();
        let path = selection.launch.cell_path();
        if kind == "torn" {
            std::fs::write(&path, b"{\"selection\":").unwrap();
            assert!(selection.completed().is_err());
            assert!(store.ensure_helpers_complete().await.is_err());
            continue;
        }
        cell.complete(HelperOutcome {
            dispatched: true,
            cancelled: false,
            launcher_status: 0,
        })
        .unwrap();
        selection.completed().unwrap();
        let mut changed = selection.clone();
        match kind {
            "missing" => std::fs::rename(&path, root.join("retained-completion")).unwrap(),
            "foreign" => {
                std::fs::rename(&path, root.join("retained-completion")).unwrap();
                std::fs::write(&path, b"foreign").unwrap();
            }
            "symlink" => {
                std::fs::rename(&path, root.join("retained-completion")).unwrap();
                symlink(root.join("retained-completion"), &path).unwrap();
            }
            "hardlink" => std::fs::hard_link(&path, root.join("aliased-completion")).unwrap(),
            "nonce" => changed.launch.nonce[0] ^= 1,
            "head" => changed.launch.intent.head.inode += 1,
            "command" => changed.launch.intent.command_digest = "0".repeat(64),
            "supervisor" => changed.supervisor.pid = std::process::id() + 1,
            _ => unreachable!(),
        }
        assert!(changed.completed().is_err(), "{kind}");
        if matches!(kind, "missing" | "foreign" | "symlink" | "hardlink") {
            assert!(store.ensure_helpers_complete().await.is_err(), "{kind}");
        }
    }
}

#[tokio::test]
async fn native_helper_completion_is_not_success_or_a_mutable_historical_selection() {
    let root = root("terminal");
    let store = fresh(&root).await;
    let commands = OwnedCommands::default();
    let mut command = Command::new("sh");
    command.args(["-c", "printf diagnostic >&2; exit 37"]);
    let selected_intent = intent(&root, &command);
    let output = commands
        .output_selected(&mut command, &store, selected_intent)
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(37));
    assert_eq!(output.stderr, b"diagnostic");
    store.ensure_helpers_complete().await.unwrap();
    let before = store.snapshot().await;
    assert!(
        store
            .update(|state| {
                state.helper_executions.clear();
                Ok(())
            })
            .await
            .is_err()
    );
    assert_eq!(store.snapshot().await, before);
    let mut json = serde_json::to_value(before).unwrap();
    json.as_object_mut().unwrap().remove("helperExecutions");
    assert!(
        serde_json::from_value::<crate::durable::PgDurableState>(json).is_err(),
        "missing native3 helper facts must not reinterpret old records"
    );
}

#[tokio::test]
async fn native_helper_exact_receipt_fields_and_unrelated_live_pid_are_not_exclusion() {
    for kind in [
        "nonce",
        "head",
        "command",
        "supervisor",
        "echild",
        "checksum",
    ] {
        let root = root(&format!("receipt-{kind}"));
        let store = fresh(&root).await;
        let command = Command::new("true");
        let launch = store.helper_launch(intent(&root, &command)).await.unwrap();
        let mut cell = CompletionCell::create(launch).unwrap();
        let selection = cell.selection.clone();
        store.select_helper(selection.clone()).await.unwrap();
        let mut unrelated = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let unrelated_pid = LivePid::open(unrelated.id());
        let mut receipt = Completion {
            selection: selection.clone(),
            outcome: HelperOutcome {
                dispatched: true,
                cancelled: false,
                launcher_status: 0,
            },
            echild: true,
            checksum: String::new(),
        };
        match kind {
            "nonce" => receipt.selection.launch.nonce[0] ^= 1,
            "head" => receipt.selection.launch.intent.head.inode += 1,
            "command" => receipt.selection.launch.intent.command_digest = "0".repeat(64),
            "supervisor" => {
                receipt.selection.supervisor = OsIdentity::read(unrelated.id()).unwrap()
            }
            "echild" => receipt.echild = false,
            "checksum" => {}
            _ => unreachable!(),
        }
        receipt.checksum = receipt.digest().unwrap();
        if kind == "checksum" {
            receipt.checksum = "0".repeat(64);
        }
        cell.file
            .write_all(&serde_json::to_vec(&receipt).unwrap())
            .unwrap();
        cell.file.sync_all().unwrap();
        File::open(&selection.launch.root)
            .unwrap()
            .sync_all()
            .unwrap();
        assert!(selection.completed().is_err(), "{kind}");
        assert!(store.ensure_helpers_complete().await.is_err(), "{kind}");
        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "stored foreign PID must never be signalled"
        );
        unrelated_pid.signal(Signal::KILL);
        unrelated.wait().unwrap();
        assert!(unrelated_pid.reaped());
    }
}

#[tokio::test]
async fn native_helper_metadata_spelling_preserves_completion_subsequent_work_and_reopen() {
    for kind in ["relative", "parent-component"] {
        let root = root(kind);
        let canonical_metadata = root.join("metadata");
        let metadata = match kind {
            "relative" => {
                let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
                let common = cwd
                    .ancestors()
                    .find(|common| canonical_metadata.starts_with(common))
                    .unwrap();
                let mut relative = PathBuf::new();
                for _ in cwd.strip_prefix(common).unwrap().components() {
                    relative.push("..");
                }
                relative.push(canonical_metadata.strip_prefix(common).unwrap());
                assert!(relative.is_relative());
                relative
            }
            "parent-component" => root.join("head/../metadata"),
            _ => unreachable!(),
        };
        let store = PgDurableStore::open(&metadata, identity(), StorageMode::Fresh)
            .await
            .unwrap();
        assert_eq!(metadata.canonicalize().unwrap(), canonical_metadata);
        assert_eq!(root.join("head").canonicalize().unwrap(), root.join("head"));
        let commands = OwnedCommands::default();
        for candidate in ["candidate", "subsequent-candidate"] {
            let candidate = root.join(candidate);
            let mut command = Command::new("cp");
            command.arg("-a").arg(root.join("head")).arg(&candidate);
            let intent =
                HelperIntent::new(1, [7; 16], &root.join("head"), &candidate, &command).unwrap();
            let output = commands
                .output_selected(&mut command, &store, intent)
                .await
                .unwrap();
            assert!(output.status.success());
            assert_eq!(
                std::fs::read(candidate.join("acknowledged")).unwrap(),
                b"must-survive"
            );
            store.ensure_helpers_complete().await.unwrap();
        }
        let before = store.snapshot().await;
        assert!(before.helper_executions.iter().all(|selection| {
            selection.launch.root == canonical_metadata
                && selection
                    .completed()
                    .is_ok_and(|outcome| outcome.dispatched)
        }));
        drop(commands);
        drop(store);
        let reopened = PgDurableStore::open(&metadata, identity(), StorageMode::Established)
            .await
            .unwrap();
        assert_eq!(reopened.snapshot().await, before);
        reopened.ensure_helpers_complete().await.unwrap();
    }
}

#[tokio::test]
async fn native_helper_real_clone_and_native_start_gate_use_selected_metadata() {
    let root = root("actual-clone");
    let store = fresh(&root).await;
    std::fs::create_dir(root.join("tools")).unwrap();
    let initdb = root.join("tools/initdb");
    std::fs::write(
        &initdb,
        format!(
            "#!/bin/sh\nprintf forbidden > '{}'\n",
            root.join("forbidden-initdb").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&initdb, std::fs::Permissions::from_mode(0o700)).unwrap();
    let instance =
        crate::instance::PgInstanceManager::new(root.join("head"), root.join("tools"), 1);
    instance.bind_generation_store(&store).await.unwrap();
    let command = Command::new("true");
    let launch = store.helper_launch(intent(&root, &command)).await.unwrap();
    let mut pending = OwnedProcess::spawn_helper(&command, &launch).unwrap();
    store
        .select_helper(pending.helper_selection().unwrap().clone())
        .await
        .unwrap();
    let lease_store = store.clone();
    let valid: crate::adapter::public_transport::GatewayValidity = Arc::new(move || {
        let store = lease_store.clone();
        Box::pin(async move { store.revalidate().await.is_ok() })
    });
    let cancellation = tokio_util::sync::CancellationToken::new();
    assert!(
        instance
            .clone_stopped_repair_candidate(
                &root.join("candidate"),
                &[7; 16],
                valid.clone(),
                cancellation.clone()
            )
            .await
            .is_err()
    );
    assert!(!root.join("candidate").exists());
    assert!(instance.init_db().await.is_err());
    assert!(!root.join("forbidden-initdb").exists());
    pending.terminate().unwrap();
    let selection = pending.helper_selection().unwrap();
    let outcome = selection.completed().unwrap();
    assert!(!outcome.dispatched);
    assert!(outcome.cancelled);
    instance
        .clone_stopped_repair_candidate(&root.join("candidate"), &[7; 16], valid, cancellation)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(root.join("candidate/acknowledged")).unwrap(),
        b"must-survive"
    );
    let state = store.snapshot().await;
    assert_eq!(state.helper_executions[0], *selection);
    let clone = &state.helper_executions[1];
    assert_eq!(clone.launch.intent.candidate_path, root.join("candidate"));
    assert!(clone.launch.intent.candidate.is_none());
    assert!(clone.completed().unwrap().dispatched);
    assert!(!clone.completed().unwrap().cancelled);
    store.ensure_helpers_complete().await.unwrap();
}
