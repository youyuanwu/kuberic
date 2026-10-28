use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::Command;

pub(crate) const MAX_EXECUTIONS: usize = 128;
const MAX_CELL_BYTES: u64 = 16 * 1024;
const PREFIX: &str = "state-v3.helper-";
const POSTMASTER_PREFIX: &str = "state-v3.postmaster-";
// Linux O_NOFOLLOW | O_NONBLOCK: do not follow a replaced cell or block on a FIFO.
const CELL_OPEN_FLAGS: i32 = 0o400000 | 0o4000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ObjectIdentity {
    path: PathBuf,
    device: u64,
    inode: u64,
    control: Option<String>,
}

impl ObjectIdentity {
    fn read(path: &Path) -> io::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_dir() || path.canonicalize()? != path {
            return Err(io::Error::other("helper object is not an exact directory"));
        }
        let control = path.join("global/pg_control");
        let control = match std::fs::symlink_metadata(&control) {
            Ok(metadata) if metadata.is_file() && metadata.nlink() == 1 => {
                Some(hex(&Sha256::digest(std::fs::read(control)?)))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            _ => return Err(io::Error::other("aliased helper control file")),
        };
        Ok(Self {
            path: path.to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
            control,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HelperIntent {
    process_generation: u64,
    candidate_nonce: [u8; 16],
    head: ObjectIdentity,
    candidate_path: PathBuf,
    candidate_parent: ObjectIdentity,
    candidate: Option<ObjectIdentity>,
    command_digest: String,
}

impl HelperIntent {
    pub(crate) fn new(
        process_generation: u64,
        candidate_nonce: [u8; 16],
        head: &Path,
        candidate: &Path,
        command: &Command,
    ) -> io::Result<Self> {
        if std::fs::symlink_metadata(head)?.file_type().is_symlink() {
            return Err(io::Error::other("aliased native helper head"));
        }
        let head = ObjectIdentity::read(&head.canonicalize()?)?;
        let parent = candidate
            .parent()
            .ok_or_else(|| io::Error::other("missing helper candidate parent"))?
            .canonicalize()?;
        let candidate_path = parent.join(
            candidate
                .file_name()
                .ok_or_else(|| io::Error::other("missing helper candidate name"))?,
        );
        let candidate = match std::fs::symlink_metadata(&candidate_path) {
            Ok(_) => Some(ObjectIdentity::read(&candidate_path)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let intent = Self {
            process_generation,
            candidate_nonce,
            head,
            candidate_path,
            candidate_parent: ObjectIdentity::read(&parent)?,
            candidate,
            command_digest: command_digest(command),
        };
        intent.validate()?;
        Ok(intent)
    }

    fn validate(&self) -> io::Result<()> {
        if self.candidate_nonce == [0; 16]
            || self.command_digest.len() != 64
            || !self.head.path.is_absolute()
            || self.head.inode == 0
            || self.candidate_parent.inode == 0
            || self.candidate_path.parent() != Some(self.candidate_parent.path.as_path())
            || self.candidate_path == self.head.path
            || self.candidate.as_ref().is_some_and(|candidate| {
                candidate.path != self.candidate_path
                    || (candidate.device == self.head.device && candidate.inode == self.head.inode)
            })
        {
            return Err(io::Error::other("invalid selected native helper intent"));
        }
        Ok(())
    }

    pub(crate) fn revalidate(&self, command: &Command) -> io::Result<()> {
        let actual = Self::new(
            self.process_generation,
            self.candidate_nonce,
            &self.head.path,
            &self.candidate_path,
            command,
        )?;
        if &actual != self {
            return Err(io::Error::other(
                "selected helper head/command/candidate changed",
            ));
        }
        Ok(())
    }
}

pub(crate) fn command_digest(command: &Command) -> String {
    let command = command.as_std();
    let mut digest = Sha256::new();
    let mut field = |value: &[u8]| {
        digest.update((value.len() as u64).to_le_bytes());
        digest.update(value);
    };
    field(command.get_program().as_bytes());
    field(&(command.get_args().len() as u64).to_le_bytes());
    for argument in command.get_args() {
        field(argument.as_bytes());
    }
    field(b"environment");
    let mut environment: BTreeMap<_, _> = std::env::vars_os().collect();
    for (key, value) in command.get_envs() {
        match value {
            Some(value) => {
                environment.insert(key.to_owned(), value.to_owned());
            }
            None => {
                environment.remove(key);
            }
        }
    }
    field(&(environment.len() as u64).to_le_bytes());
    for (key, value) in environment {
        field(key.as_bytes());
        field(value.as_bytes());
    }
    field(b"directory");
    field(
        command
            .get_current_dir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().expect("current directory"))
            .as_os_str()
            .as_bytes(),
    );
    hex(&digest.finalize())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct OsIdentity {
    pub(crate) pid: u32,
    boot: String,
    start: u64,
}

impl OsIdentity {
    pub(crate) fn read(pid: u32) -> io::Result<Self> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let fields = stat
            .rsplit_once(')')
            .ok_or_else(|| io::Error::other("invalid supervisor stat"))?
            .1;
        let start = fields
            .split_whitespace()
            .nth(19)
            .ok_or_else(|| io::Error::other("missing supervisor start time"))?
            .parse()
            .map_err(io::Error::other)?;
        Ok(Self {
            pid,
            boot: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
                .trim()
                .to_owned(),
            start,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HelperLaunch {
    pub(crate) root: PathBuf,
    pub(crate) execution_generation: u64,
    nonce: [u8; 16],
    pub(crate) intent: HelperIntent,
}

impl HelperLaunch {
    pub(crate) fn new(root: &Path, generation: u64, intent: HelperIntent) -> io::Result<Self> {
        let mut nonce = [0; 16];
        rustix::rand::getrandom(&mut nonce, rustix::rand::GetRandomFlags::empty())?;
        let launch = Self {
            root: root.canonicalize()?,
            execution_generation: generation,
            nonce,
            intent,
        };
        launch.validate()?;
        Ok(launch)
    }

    fn validate(&self) -> io::Result<()> {
        self.intent.validate()?;
        if self.nonce == [0; 16]
            || self.execution_generation == 0
            || self.execution_generation > MAX_EXECUTIONS as u64
            || !self.root.is_absolute()
        {
            return Err(io::Error::other("invalid native helper selection"));
        }
        Ok(())
    }

    fn cell_path(&self) -> PathBuf {
        self.root
            .join(format!("{PREFIX}{}.completion", hex(&self.nonce)))
    }
}

pub(crate) trait ExecutionLaunch: Clone + PartialEq + Serialize {
    const PREFIX: &'static str;
    fn validate(&self) -> io::Result<()>;
    fn root(&self) -> &Path;
    fn generation(&self) -> u64;
    fn cell_path(&self) -> PathBuf;
}

impl ExecutionLaunch for HelperLaunch {
    const PREFIX: &'static str = PREFIX;

    fn validate(&self) -> io::Result<()> {
        self.validate()
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn generation(&self) -> u64 {
        self.execution_generation
    }

    fn cell_path(&self) -> PathBuf {
        self.cell_path()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PostmasterIntent {
    pub(crate) process_generation: u64,
    owner: OsIdentity,
    data_path: PathBuf,
    data_root: ObjectIdentity,
    command_digest: String,
}

impl PostmasterIntent {
    pub(crate) fn new(
        process_generation: u64,
        data_root: &Path,
        command: &Command,
    ) -> io::Result<Self> {
        if !std::fs::symlink_metadata(data_root)?.is_dir() {
            return Err(io::Error::other("aliased postmaster data directory"));
        }
        let intent = Self {
            process_generation,
            owner: OsIdentity::read(std::process::id())?,
            data_path: data_root.to_owned(),
            data_root: ObjectIdentity::read(&data_root.canonicalize()?)?,
            command_digest: command_digest(command),
        };
        intent.validate()?;
        Ok(intent)
    }

    fn validate(&self) -> io::Result<()> {
        if self.process_generation == 0
            || self.owner.pid == 0
            || self.owner.boot.is_empty()
            || self.owner.start == 0
            || self.data_path.as_os_str().is_empty()
            || !self.data_root.path.is_absolute()
            || self.data_root.inode == 0
            || self.data_root.control.is_none()
            || self.command_digest.len() != 64
        {
            return Err(io::Error::other("invalid selected postmaster intent"));
        }
        Ok(())
    }

    pub(crate) fn revalidate(&self, command: &Command) -> io::Result<()> {
        if *self != Self::new(self.process_generation, &self.data_path, command)? {
            return Err(io::Error::other(
                "selected postmaster data/command/owner changed",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PostmasterLaunch {
    pub(crate) root: PathBuf,
    pub(crate) execution_generation: u64,
    nonce: [u8; 16],
    pub(crate) intent: PostmasterIntent,
}

impl PostmasterLaunch {
    pub(crate) fn new(root: &Path, generation: u64, intent: PostmasterIntent) -> io::Result<Self> {
        let mut nonce = [0; 16];
        rustix::rand::getrandom(&mut nonce, rustix::rand::GetRandomFlags::empty())?;
        let launch = Self {
            root: root.canonicalize()?,
            execution_generation: generation,
            nonce,
            intent,
        };
        launch.validate()?;
        Ok(launch)
    }
}

impl ExecutionLaunch for PostmasterLaunch {
    const PREFIX: &'static str = POSTMASTER_PREFIX;

    fn validate(&self) -> io::Result<()> {
        self.intent.validate()?;
        if self.nonce == [0; 16]
            || self.execution_generation == 0
            || self.execution_generation > MAX_EXECUTIONS as u64
            || !self.root.is_absolute()
        {
            return Err(io::Error::other("invalid postmaster execution selection"));
        }
        Ok(())
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn generation(&self) -> u64 {
        self.execution_generation
    }

    fn cell_path(&self) -> PathBuf {
        self.root.join(format!(
            "{POSTMASTER_PREFIX}{}.completion",
            hex(&self.nonce)
        ))
    }
}

pub(crate) type HelperSelection = ExecutionSelection<HelperLaunch>;
pub(crate) type PostmasterSelection = ExecutionSelection<PostmasterLaunch>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExecutionSelection<L> {
    pub(crate) launch: L,
    pub(crate) supervisor: OsIdentity,
    device: u64,
    inode: u64,
}

impl<L: ExecutionLaunch + serde::de::DeserializeOwned> ExecutionSelection<L> {
    pub(crate) fn validate(&self) -> io::Result<()> {
        self.launch.validate()?;
        if self.inode == 0
            || self.supervisor.pid == 0
            || self.supervisor.boot.is_empty()
            || self.supervisor.start == 0
        {
            return Err(io::Error::other("invalid native supervisor identity"));
        }
        Ok(())
    }

    pub(crate) fn validate_cell(&self) -> io::Result<File> {
        self.validate()?;
        let path = self.launch.cell_path();
        let named = std::fs::symlink_metadata(&path)?;
        if !named.is_file()
            || named.dev() != self.device
            || named.ino() != self.inode
            || named.nlink() != 1
        {
            return Err(io::Error::other(
                "foreign or aliased native helper completion cell",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(CELL_OPEN_FLAGS)
            .open(&path)?;
        let actual = file.metadata()?;
        if !actual.is_file()
            || actual.nlink() != 1
            || actual.dev() != self.device
            || actual.ino() != self.inode
            || actual.len() > MAX_CELL_BYTES
            || self.launch.root().canonicalize()? != self.launch.root()
        {
            return Err(io::Error::other(
                "foreign or aliased native helper completion cell",
            ));
        }
        Ok(file)
    }

    pub(crate) fn completed(&self) -> io::Result<HelperOutcome> {
        let mut file = self.validate_cell()?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_CELL_BYTES + 1)
            .read_to_end(&mut bytes)?;
        let receipt: Completion<L> = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if receipt.selection != *self || !receipt.echild || receipt.checksum != receipt.digest()? {
            return Err(io::Error::other(
                "wrong or incomplete native helper receipt",
            ));
        }
        // A successor also makes the observed complete bytes durable. This
        // closes the write/sync gap without accepting any caller Ready flag.
        file.sync_all()?;
        File::open(self.launch.root())?.sync_all()?;
        Ok(receipt.outcome)
    }
}

pub(crate) fn validate_cells<L: ExecutionLaunch + serde::de::DeserializeOwned>(
    root: &Path,
    selections: &[ExecutionSelection<L>],
) -> io::Result<()> {
    let root = root.canonicalize()?;
    if selections.len() > MAX_EXECUTIONS {
        return Err(io::Error::other(
            "native helper execution history exhausted",
        ));
    }
    for (index, selection) in selections.iter().enumerate() {
        if selection.launch.generation() != index as u64 + 1 || selection.launch.root() != root {
            return Err(io::Error::other("crossed native helper selection history"));
        }
        selection.validate_cell()?;
    }
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(L::PREFIX)
            && !selections
                .iter()
                .any(|selection| selection.launch.cell_path() == entry.path())
        {
            return Err(io::Error::other("unselected native helper completion cell"));
        }
    }
    Ok(())
}

pub(crate) fn is_cell_name(name: &str) -> bool {
    name.strip_prefix(PREFIX)
        .or_else(|| name.strip_prefix(POSTMASTER_PREFIX))
        .and_then(|value| value.strip_suffix(".completion"))
        .is_some_and(|nonce| {
            nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HelperOutcome {
    pub(crate) dispatched: bool,
    pub(crate) cancelled: bool,
    pub(crate) launcher_status: i32,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Completion<L> {
    selection: ExecutionSelection<L>,
    outcome: HelperOutcome,
    echild: bool,
    checksum: String,
}

impl<L: Serialize> Completion<L> {
    fn digest(&self) -> io::Result<String> {
        Ok(hex(&Sha256::digest(
            serde_json::to_vec(&(&self.selection, &self.outcome, self.echild))
                .map_err(io::Error::other)?,
        )))
    }
}

pub(crate) struct CompletionCell<L = HelperLaunch> {
    file: File,
    pub(crate) selection: ExecutionSelection<L>,
}

impl<L: ExecutionLaunch + serde::de::DeserializeOwned> CompletionCell<L> {
    pub(crate) fn create(launch: L) -> io::Result<Self> {
        launch.validate()?;
        if launch.root().canonicalize()? != launch.root() {
            return Err(io::Error::other("aliased native helper metadata directory"));
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(launch.cell_path())?;
        file.sync_all()?;
        File::open(launch.root())?.sync_all()?;
        let metadata = file.metadata()?;
        let selection = ExecutionSelection {
            launch,
            supervisor: OsIdentity::read(std::process::id())?,
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        selection.validate_cell()?;
        Ok(Self { file, selection })
    }

    pub(crate) fn complete(mut self, outcome: HelperOutcome) -> io::Result<()> {
        self.selection.validate_cell()?;
        let mut receipt = Completion {
            selection: self.selection.clone(),
            outcome,
            echild: true,
            checksum: String::new(),
        };
        receipt.checksum = receipt.digest()?;
        let bytes = serde_json::to_vec(&receipt).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_CELL_BYTES || self.file.metadata()?.len() != 0 {
            return Err(io::Error::other(
                "native helper completion cell is not pending",
            ));
        }
        self.file.write_all(&bytes)?;
        self.file.sync_all()?;
        File::open(self.selection.launch.root())?.sync_all()
    }
}

pub(crate) fn send_selection<L: Serialize>(
    channel: impl std::os::fd::AsFd,
    selection: &ExecutionSelection<L>,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(selection).map_err(io::Error::other)?;
    let mut frame = (bytes.len() as u32).to_le_bytes().to_vec();
    frame.extend(bytes);
    let mut pending = frame.as_slice();
    while !pending.is_empty() {
        let written = rustix::io::write(&channel, pending)?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "helper rendezvous",
            ));
        }
        pending = &pending[written..];
    }
    Ok(())
}

pub(crate) fn receive_selection<L: serde::de::DeserializeOwned>(
    mut channel: impl Read,
) -> io::Result<ExecutionSelection<L>> {
    let mut length = [0; 4];
    channel.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as u64;
    if length > MAX_CELL_BYTES {
        return Err(io::Error::other("oversized native helper rendezvous"));
    }
    let mut bytes = vec![0; length as usize];
    channel.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
#[path = "helper_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "postmaster_tests.rs"]
mod postmaster_tests;
