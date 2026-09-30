use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::native::AcknowledgementPolicy;
use kuberic_protocol::types::{BuildAuthority, OperationId, ReplicaIdentity, ResourceUid};
use kuberic_runtime::replicator::ReplicaSetConfiguration;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

const STATE_VERSION: u32 = 1;
const STATE_FILE: &str = "state-v2.json";

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMode {
    Fresh,
    Established,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PgDurableRole {
    None,
    Primary,
    Standby,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PgRecoveryState {
    Ready,
    Rebuilding,
    Unsafe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgDurableIdentity {
    pub resource_uid: ResourceUid,
    pub replica: ReplicaIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgDurableState {
    pub version: u32,
    pub identity: PgDurableIdentity,
    pub generation: u64,
    pub system_identifier: Option<String>,
    pub timeline_id: Option<u32>,
    pub timeline_history_digest: Option<String>,
    pub role: PgDurableRole,
    pub recovery_state: PgRecoveryState,
    pub external_access_closed: bool,
    pub postgres_stopped: bool,
    pub current_lsn: i64,
    pub flush_lsn: i64,
    pub received_lsn: Option<i64>,
    pub replay_lsn: Option<i64>,
    pub policy_certified_lsn: i64,
    pub synchronous: Option<AcknowledgementPolicy>,
    pub accepted_build: Option<BuildAuthority>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_accepted_authority: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_build: Option<crate::build::PgBuildProgress>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outbound_builds: Vec<crate::build::PgBuildProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch_up: Option<(kuberic_protocol::types::ConfigurationId, i64)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suspended_builds: Vec<crate::build::PgBuildProgress>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub retired_builds: BTreeSet<OperationId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub outbound_attempts: BTreeMap<OperationId, u64>,
}

impl PgDurableState {
    fn new(identity: PgDurableIdentity) -> Self {
        Self {
            version: STATE_VERSION,
            identity,
            generation: 1,
            system_identifier: None,
            timeline_id: None,
            timeline_history_digest: None,
            role: PgDurableRole::None,
            recovery_state: PgRecoveryState::Ready,
            external_access_closed: true,
            postgres_stopped: true,
            current_lsn: 0,
            flush_lsn: 0,
            received_lsn: None,
            replay_lsn: None,
            policy_certified_lsn: 0,
            synchronous: None,
            accepted_build: None,
            has_accepted_authority: false,
            native_build: None,
            outbound_builds: Vec::new(),
            catch_up: None,
            suspended_builds: Vec::new(),
            retired_builds: BTreeSet::new(),
            outbound_attempts: BTreeMap::new(),
        }
    }

    fn validate(&self, expected: &PgDurableIdentity) -> Result<(), PgDurableError> {
        if self.version != STATE_VERSION {
            return Err(PgDurableError::Version {
                expected: STATE_VERSION,
                observed: self.version,
            });
        }
        if &self.identity != expected {
            return Err(PgDurableError::IdentityMismatch);
        }
        if self.generation == 0
            || self.current_lsn < 0
            || self.flush_lsn < 0
            || self.flush_lsn > self.current_lsn
            || self.policy_certified_lsn < 0
            || self.policy_certified_lsn > self.flush_lsn
            || self.received_lsn.is_some_and(|lsn| lsn < 0)
            || self.replay_lsn.is_some_and(|lsn| lsn < 0)
            || self
                .replay_lsn
                .zip(self.received_lsn)
                .is_some_and(|(replay, received)| replay > received)
        {
            return Err(PgDurableError::Invalid(
                "durable PostgreSQL progress is inconsistent".into(),
            ));
        }
        if let Some(synchronous) = &self.synchronous {
            synchronous.validate().map_err(PgDurableError::Invalid)?;
        }
        if self.outbound_builds.len() > crate::build::MAX_BUILDS {
            return Err(PgDurableError::Invalid("too many native builds".into()));
        }
        let mut ids = std::collections::BTreeSet::new();
        for build in self.outbound_builds.iter().chain(&self.suspended_builds) {
            build.validate().map_err(PgDurableError::Invalid)?;
            if build.request.resource_uid != self.identity.resource_uid
                || build.request.authority.source != self.identity.replica
                || !ids.insert(build.request.authority.build_id.clone())
                || self
                    .retired_builds
                    .contains(&build.request.authority.build_id)
            {
                return Err(PgDurableError::Invalid(
                    "invalid outbound native build".into(),
                ));
            }
            if self.outbound_attempts.iter().any(|(id, generation)| {
                *generation == 0
                    || *generation > self.generation
                    || !self
                        .outbound_builds
                        .iter()
                        .any(|b| &b.request.authority.build_id == id)
            }) {
                return Err(PgDurableError::Invalid(
                    "invalid outbound build attempt".into(),
                ));
            }
        }
        if let Some(build) = &self.native_build {
            build.validate().map_err(PgDurableError::Invalid)?;
            if build.request.resource_uid != self.identity.resource_uid
                || build.request.authority.target != self.identity.replica
                || self.accepted_build.as_ref() != Some(&build.request.authority)
                || !self.external_access_closed
                    && build.stage != crate::build::PgBuildStage::Complete
            {
                return Err(PgDurableError::Invalid(
                    "invalid target native build".into(),
                ));
            }
        }
        if self.postgres_stopped && !self.external_access_closed {
            return Err(PgDurableError::Invalid(
                "stopped PostgreSQL cannot have external access granted".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn suspend_build(&mut self, id: &OperationId) {
        if let Some(index) = self
            .outbound_builds
            .iter()
            .position(|b| &b.request.authority.build_id == id)
        {
            let build = self.outbound_builds.remove(index);
            self.suspended_builds
                .retain(|b| &b.request.authority.build_id != id);
            self.suspended_builds.push(build);
        }
        self.outbound_attempts.remove(id);
    }

    pub(crate) fn reconcile_builds(&mut self, current: &ReplicaSetConfiguration) {
        let selected = current
            .replicas
            .iter()
            .filter(|r| !r.build_id.is_empty())
            .map(|r| &r.build_id)
            .collect::<BTreeSet<_>>();
        for build in &self.outbound_builds {
            if !selected.contains(&build.request.authority.build_id) {
                self.retired_builds
                    .insert(build.request.authority.build_id.clone());
            }
        }
        for build in &self.suspended_builds {
            if !selected.contains(&build.request.authority.build_id) {
                self.retired_builds
                    .insert(build.request.authority.build_id.clone());
            }
        }
        if let Some(build) = &self.native_build
            && !selected.contains(&build.request.authority.build_id)
        {
            self.retired_builds
                .insert(build.request.authority.build_id.clone());
        }
        self.outbound_builds
            .retain(|b| !self.retired_builds.contains(&b.request.authority.build_id));
        self.suspended_builds
            .retain(|b| !self.retired_builds.contains(&b.request.authority.build_id));
        self.outbound_attempts.retain(|id, _| {
            self.outbound_builds
                .iter()
                .any(|b| &b.request.authority.build_id == id)
        });
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateEnvelope {
    state: PgDurableState,
    checksum: String,
}

pub struct PgDurableStore {
    root: PathBuf,
    state: Arc<Mutex<PgDurableState>>,
    #[cfg(feature = "testing")]
    commit_hook: std::sync::Mutex<Option<CommitHook>>,
}

#[cfg(feature = "testing")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitStage {
    BeforeRename,
    CommittedBeforePublish,
    Published,
}

#[cfg(feature = "testing")]
pub struct CommitGate {
    pub entered: Arc<tokio::sync::Notify>,
    resume: std::sync::mpsc::Sender<()>,
}

#[cfg(feature = "testing")]
impl CommitGate {
    pub fn release(&self) {
        let _ = self.resume.send(());
    }
}

#[cfg(feature = "testing")]
struct CommitHook {
    stage: CommitStage,
    catch_up_only: bool,
    entered: Arc<tokio::sync::Notify>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(feature = "testing")]
impl CommitHook {
    fn checkpoint(&self, stage: CommitStage) {
        if self.stage == stage {
            self.entered.notify_one();
            let _ = self.resume.recv();
        }
    }
}

impl PgDurableStore {
    #[cfg(feature = "testing")]
    pub fn pause_commit(&self, stage: CommitStage, catch_up_only: bool) -> CommitGate {
        let entered = Arc::new(tokio::sync::Notify::new());
        let (resume, receiver) = std::sync::mpsc::channel();
        *self.commit_hook.lock().unwrap() = Some(CommitHook {
            stage,
            catch_up_only,
            entered: entered.clone(),
            resume: receiver,
        });
        CommitGate { entered, resume }
    }

    pub async fn open(
        root: impl AsRef<Path>,
        expected: PgDurableIdentity,
        mode: StorageMode,
    ) -> Result<Self, PgDurableError> {
        let root = root.as_ref().to_path_buf();
        if mode == StorageMode::Fresh {
            tokio::fs::create_dir_all(&root)
                .await
                .map_err(PgDurableError::Io)?;
        }
        let path = root.join(STATE_FILE);
        let state = match (
            tokio::fs::try_exists(&path)
                .await
                .map_err(PgDurableError::Io)?,
            mode,
        ) {
            (true, StorageMode::Fresh) => return Err(PgDurableError::AlreadyExists),
            (false, StorageMode::Established) => return Err(PgDurableError::Missing),
            (false, StorageMode::Fresh) => PgDurableState::new(expected.clone()),
            (true, StorageMode::Established) => read_state(&path).await?,
        };
        state.validate(&expected)?;
        let store = Self {
            root,
            state: Arc::new(Mutex::new(state)),
            #[cfg(feature = "testing")]
            commit_hook: std::sync::Mutex::new(None),
        };
        if mode == StorageMode::Fresh {
            store.persist().await?;
        }
        Ok(store)
    }

    pub async fn snapshot(&self) -> PgDurableState {
        self.state.lock().await.clone()
    }

    pub async fn revalidate(&self) -> Result<PgDurableState, PgDurableError> {
        let expected = self.state.lock().await;
        let actual = read_state(&self.root.join(STATE_FILE)).await?;
        actual.validate(&expected.identity)?;
        if actual != *expected {
            return Err(PgDurableError::Invalid(
                "durable PostgreSQL evidence changed outside its owner".into(),
            ));
        }
        Ok(actual)
    }

    pub async fn update(
        &self,
        update: impl FnOnce(&mut PgDurableState) -> Result<(), PgDurableError>,
    ) -> Result<PgDurableState, PgDurableError> {
        let mut state = self.state.clone().lock_owned().await;
        let mut next = state.clone();
        update(&mut next)?;
        next.generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| PgDurableError::Invalid("durable generation overflow".into()))?;
        next.validate(&state.identity)?;
        if next.system_identifier == state.system_identifier
            && next.recovery_state != PgRecoveryState::Rebuilding
            && (next.current_lsn < state.current_lsn
                || next.flush_lsn < state.flush_lsn
                || next.received_lsn.zip(state.received_lsn).is_some_and(
                    |(next_received, previous_received)| next_received < previous_received,
                )
                || next
                    .replay_lsn
                    .zip(state.replay_lsn)
                    .is_some_and(|(next_replay, previous_replay)| next_replay < previous_replay)
                || next.policy_certified_lsn < state.policy_certified_lsn)
        {
            return Err(PgDurableError::Invalid(
                "durable PostgreSQL progress cannot regress outside rebuilding".into(),
            ));
        }
        let path = self.root.join(STATE_FILE);
        #[cfg(feature = "testing")]
        let hook = {
            let mut pending = self.commit_hook.lock().unwrap();
            if pending
                .as_ref()
                .is_some_and(|hook| !hook.catch_up_only || next.catch_up != state.catch_up)
            {
                pending.take()
            } else {
                None
            }
        };
        // The worker owns the state lock through publication. Dropping the
        // caller cannot leave the committed file ahead of the owner's memory.
        tokio::task::spawn_blocking(move || {
            let mut renamed = false;
            let result = write_state(&path, &next, &mut renamed, |stage| {
                #[cfg(feature = "testing")]
                if let Some(hook) = &hook {
                    hook.checkpoint(stage);
                }
                #[cfg(not(feature = "testing"))]
                let _ = stage;
            });
            if renamed {
                *state = next.clone();
                #[cfg(feature = "testing")]
                if let Some(hook) = &hook {
                    hook.checkpoint(CommitStage::Published);
                }
            }
            result?;
            Ok(next)
        })
        .await
        .map_err(|error| {
            PgDurableError::Io(std::io::Error::other(format!(
                "metadata commit worker: {error}"
            )))
        })?
    }

    async fn persist(&self) -> Result<(), PgDurableError> {
        let state = self.state.lock().await.clone();
        write_state(&self.root.join(STATE_FILE), &state, &mut false, |_| {})
    }
}

async fn read_state(path: &Path) -> Result<PgDurableState, PgDurableError> {
    let bytes = tokio::fs::read(path).await.map_err(PgDurableError::Io)?;
    let envelope: StateEnvelope = serde_json::from_slice(&bytes).map_err(PgDurableError::Json)?;
    let checksum = checksum(&envelope.state)?;
    if checksum != envelope.checksum {
        return Err(PgDurableError::Checksum);
    }
    Ok(envelope.state)
}

#[cfg(not(feature = "testing"))]
enum CommitStage {
    BeforeRename,
    CommittedBeforePublish,
}

fn write_state(
    path: &Path,
    state: &PgDurableState,
    renamed: &mut bool,
    checkpoint: impl Fn(CommitStage),
) -> Result<(), PgDurableError> {
    let envelope = StateEnvelope {
        state: state.clone(),
        checksum: checksum(state)?,
    };
    let bytes = serde_json::to_vec_pretty(&envelope).map_err(PgDurableError::Json)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes).map_err(PgDurableError::Io)?;
    File::open(&temporary)
        .and_then(|file| file.sync_all())
        .map_err(PgDurableError::Io)?;
    checkpoint(CommitStage::BeforeRename);
    std::fs::rename(&temporary, path).map_err(PgDurableError::Io)?;
    *renamed = true;
    if let Some(parent) = path.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(PgDurableError::Io)?;
    }
    checkpoint(CommitStage::CommittedBeforePublish);
    Ok(())
}

fn checksum(state: &PgDurableState) -> Result<String, PgDurableError> {
    let bytes = serde_json::to_vec(state).map_err(PgDurableError::Json)?;
    let digest = Sha256::digest(bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Debug, thiserror::Error)]
pub enum PgDurableError {
    #[error("PostgreSQL durable state already exists")]
    AlreadyExists,
    #[error("PostgreSQL durable state is missing")]
    Missing,
    #[error("PostgreSQL durable identity mismatch")]
    IdentityMismatch,
    #[error("PostgreSQL durable state checksum mismatch")]
    Checksum,
    #[error("PostgreSQL durable state version mismatch: expected {expected}, observed {observed}")]
    Version { expected: u32, observed: u32 },
    #[error("invalid PostgreSQL durable state: {0}")]
    Invalid(String),
    #[error("PostgreSQL durable state I/O failed: {0}")]
    Io(std::io::Error),
    #[error("PostgreSQL durable state JSON failed: {0}")]
    Json(serde_json::Error),
}

#[cfg(test)]
mod tests {
    use kuberic_protocol::types::{AgentGeneration, ReplicaId, ReplicaInstanceId};

    use super::*;

    fn identity() -> PgDurableIdentity {
        PgDurableIdentity {
            resource_uid: ResourceUid::new("resource"),
            replica: ReplicaIdentity {
                replica_id: ReplicaId::new(1),
                instance_id: ReplicaInstanceId::new("pod-1"),
                agent_generation: AgentGeneration::new("generation-1"),
            },
        }
    }

    #[test]
    fn durable_validation_rejects_version_progress_and_fence_mismatches() {
        let expected = identity();
        let mut state = PgDurableState::new(expected.clone());
        state.version = 0;
        assert!(matches!(
            state.validate(&expected),
            Err(PgDurableError::Version { .. })
        ));

        let mut state = PgDurableState::new(expected.clone());
        state.current_lsn = 1;
        state.flush_lsn = 2;
        assert!(matches!(
            state.validate(&expected),
            Err(PgDurableError::Invalid(_))
        ));

        let mut state = PgDurableState::new(expected.clone());
        state.postgres_stopped = true;
        state.external_access_closed = false;
        assert!(matches!(
            state.validate(&expected),
            Err(PgDurableError::Invalid(_))
        ));

        let mut state = PgDurableState::new(expected.clone());
        state.received_lsn = Some(2);
        state.replay_lsn = Some(3);
        assert!(matches!(
            state.validate(&expected),
            Err(PgDurableError::Invalid(_))
        ));
    }
}
