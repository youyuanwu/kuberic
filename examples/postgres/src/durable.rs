use std::fs::File;
use std::path::{Path, PathBuf};

use crate::native::AcknowledgementPolicy;
use kuberic_protocol::types::{BuildAuthority, ReplicaIdentity, ResourceUid};
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
        for build in &self.outbound_builds {
            build.validate().map_err(PgDurableError::Invalid)?;
            if build.request.resource_uid != self.identity.resource_uid
                || build.request.authority.source != self.identity.replica
                || !ids.insert(build.request.authority.build_id.clone())
            {
                return Err(PgDurableError::Invalid(
                    "invalid outbound native build".into(),
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
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateEnvelope {
    state: PgDurableState,
    checksum: String,
}

pub struct PgDurableStore {
    root: PathBuf,
    state: Mutex<PgDurableState>,
}

impl PgDurableStore {
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
            state: Mutex::new(state),
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
        let mut state = self.state.lock().await;
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
        write_state(&self.root.join(STATE_FILE), &next).await?;
        *state = next.clone();
        Ok(next)
    }

    async fn persist(&self) -> Result<(), PgDurableError> {
        let state = self.state.lock().await.clone();
        write_state(&self.root.join(STATE_FILE), &state).await
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

async fn write_state(path: &Path, state: &PgDurableState) -> Result<(), PgDurableError> {
    let envelope = StateEnvelope {
        state: state.clone(),
        checksum: checksum(state)?,
    };
    let bytes = serde_json::to_vec_pretty(&envelope).map_err(PgDurableError::Json)?;
    let temporary = path.with_extension("json.tmp");
    tokio::fs::write(&temporary, bytes)
        .await
        .map_err(PgDurableError::Io)?;
    File::open(&temporary)
        .and_then(|file| file.sync_all())
        .map_err(PgDurableError::Io)?;
    tokio::fs::rename(&temporary, path)
        .await
        .map_err(PgDurableError::Io)?;
    if let Some(parent) = path.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(PgDurableError::Io)?;
    }
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
