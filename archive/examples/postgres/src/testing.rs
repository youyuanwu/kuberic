use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use kuberic_runtime::protocol::types::{
    AgentGeneration, ConfigurationDescriptor, ConfigurationMember, Epoch, ReplicaId,
    ReplicaIdentity, ReplicaInstanceId, ReplicaRole,
};

pub fn native_identity(id: i64, incarnation: &str) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(incarnation),
        agent_generation: AgentGeneration::new(format!("generation-{incarnation}")),
    }
}

pub fn native_configuration(
    members: &[ReplicaIdentity],
    primary: usize,
    epoch: i64,
) -> ConfigurationDescriptor {
    ConfigurationDescriptor::new(
        Epoch::new(0, epoch),
        members[primary].replica_id,
        members
            .iter()
            .enumerate()
            .map(|(index, identity)| ConfigurationMember {
                identity: identity.clone(),
                role: if index == primary {
                    ReplicaRole::Primary
                } else {
                    ReplicaRole::ActiveSecondary
                },
            })
            .collect(),
        members.len() as u32 / 2 + 1,
    )
}

pub fn find_pg_bin() -> PathBuf {
    for path in [
        "/usr/lib/postgresql/16/bin",
        "/usr/lib/postgresql/17/bin",
        "/usr/lib/postgresql/15/bin",
        "/usr/pgsql-16/bin",
        "/usr/local/pgsql/bin",
    ] {
        let path = PathBuf::from(path);
        if path.join("initdb").exists() {
            return path;
        }
    }
    if let Some(path) = std::process::Command::new("pg_config")
        .arg("--bindir")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|path| !path.is_empty())
    {
        return PathBuf::from(path);
    }
    panic!("PostgreSQL binaries not found");
}

pub fn wrapped_pg_bin(root: &std::path::Path, command: &str, script: &str) -> PathBuf {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let wrappers = root.join("bin");
    std::fs::create_dir(&wrappers).unwrap();
    for entry in std::fs::read_dir(find_pg_bin()).unwrap() {
        let entry = entry.unwrap();
        symlink(entry.path(), wrappers.join(entry.file_name())).unwrap();
    }
    let wrapper = wrappers.join(command);
    std::fs::remove_file(&wrapper).unwrap();
    std::fs::write(&wrapper, script).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    wrappers
}

pub struct ProcessProbe(Vec<(u32, std::os::fd::OwnedFd)>);

impl ProcessProbe {
    pub fn process(pid: u32) -> Self {
        use rustix::process::{Pid, PidfdFlags, pidfd_open};
        Self(vec![(
            pid,
            pidfd_open(Pid::from_raw(pid as i32).unwrap(), PidfdFlags::empty()).unwrap(),
        )])
    }

    pub fn postgres(data_dir: &std::path::Path) -> Self {
        use rustix::process::{Pid, PidfdFlags, pidfd_open};
        let pid = std::fs::read_to_string(data_dir.join("postmaster.pid"))
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .parse::<u32>()
            .unwrap();
        let mut probe = Self::descendants(pid);
        probe.0.push((
            pid,
            pidfd_open(Pid::from_raw(pid as i32).unwrap(), PidfdFlags::empty()).unwrap(),
        ));
        probe
    }

    pub fn descendants(parent: u32) -> Self {
        use rustix::process::{Pid, PidfdFlags, pidfd_open};
        let mut processes = Vec::new();
        let mut parents = vec![parent];
        while let Some(parent) = parents.pop() {
            let Ok(tasks) = std::fs::read_dir(format!("/proc/{parent}/task")) else {
                continue;
            };
            for task in tasks.flatten() {
                let Ok(children) = std::fs::read_to_string(task.path().join("children")) else {
                    continue;
                };
                for child in children.split_whitespace() {
                    let pid = child.parse::<u32>().unwrap();
                    if processes.iter().any(|(known, _)| *known == pid) {
                        continue;
                    }
                    if let Ok(fd) =
                        pidfd_open(Pid::from_raw(pid as i32).unwrap(), PidfdFlags::empty())
                    {
                        processes.push((pid, fd));
                        parents.push(pid);
                    }
                }
            }
        }
        Self(processes)
    }

    pub fn assert_reaped(&self) {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        for (pid, fd) in &self.0 {
            let mut fds = [PollFd::new(fd, PollFlags::IN)];
            poll(
                &mut fds,
                Some(&Timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                }),
            )
            .unwrap();
            assert!(
                fds[0].revents().contains(PollFlags::HUP),
                "owned descendant {pid} was not reaped before successful completion"
            );
        }
    }

    pub async fn assert_gone(&self) {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if self.0.iter().all(|(_, fd)| {
                    let mut fds = [PollFd::new(fd, PollFlags::IN)];
                    poll(
                        &mut fds,
                        Some(&Timespec {
                            tv_sec: 0,
                            tv_nsec: 0,
                        }),
                    )
                    .unwrap();
                    fds[0].revents().contains(PollFlags::HUP)
                }) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "owned descendants survived: {:?}",
                self.0.iter().map(|process| process.0).collect::<Vec<_>>()
            )
        });
    }
}

impl Drop for ProcessProbe {
    fn drop(&mut self) {
        use rustix::process::{Signal, pidfd_send_signal};
        for (_, fd) in &self.0 {
            let _ = pidfd_send_signal(fd, Signal::QUIT);
            let _ = pidfd_send_signal(fd, Signal::CONT);
        }
    }
}

pub struct TestDataDir {
    path: PathBuf,
}

impl TestDataDir {
    pub fn new(name: &str) -> Self {
        Self {
            path: temp_data_dir(name),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TestDataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn temp_data_dir(name: &str) -> PathBuf {
    static DIRECTORY_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target")
        .join("pgv1-tests");
    std::fs::create_dir_all(&root).expect("create PostgreSQL test root");
    let root = std::fs::canonicalize(root).expect("resolve PostgreSQL test root");
    loop {
        let counter = DIRECTORY_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert!(counter < 4096, "PostgreSQL fixture directory IDs exhausted");
        let directory = root.join(format!("{:x}{counter:x}", std::process::id()));
        match std::fs::create_dir(&directory) {
            Ok(()) => {
                tracing::debug!(name, path = ?directory, "created PostgreSQL fixture");
                return directory;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("create PostgreSQL fixture {name}: {error}"),
        }
    }
}

pub async fn allocate_port() -> u16 {
    static PORTS: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();
    loop {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral PostgreSQL port");
        let port = listener.local_addr().unwrap().port();
        let inserted = PORTS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap()
            .insert(port);
        drop(listener);
        if inserted {
            return port;
        }
    }
}
