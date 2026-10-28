use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};
#[cfg(feature = "testing")]
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use crate::native::AcknowledgementPolicy;
use kuberic_runtime::protocol::types::{
    BuildAuthority, ConfigurationDescriptor, Epoch, OperationId, ProcessSessionId, ReplicaIdentity,
    ReplicaRole, ReplicaStorageIncarnationContext, ResourceUid,
};
use kuberic_runtime::replicator::ReplicaSetConfiguration;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

pub const STATE_VERSION: u32 = 3;
const STATE_FILE: &str = "state-v3.json";
pub const MAX_RETAINED_BUILD_IDS: usize = 64;
pub const MAX_METADATA_BYTES: usize = 4 * 1024 * 1024;
const MAX_BUILD_ID_BYTES: usize = 512;

fn is_false(value: &bool) -> bool {
    !value
}
fn is_zero(value: &u64) -> bool {
    *value == 0
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PgNativeParticipant {
    pub identity: ReplicaIdentity,
    pub process_session_id: ProcessSessionId,
    pub storage_incarnation: Option<ReplicaStorageIncarnationContext>,
    pub replication_address: String,
    pub role: ReplicaRole,
}

impl From<&kuberic_runtime::replicator::ReplicaInformation> for PgNativeParticipant {
    fn from(peer: &kuberic_runtime::replicator::ReplicaInformation) -> Self {
        Self {
            identity: peer.identity.clone(),
            process_session_id: peer.process_session_id.clone(),
            storage_incarnation: peer.storage_incarnation.clone(),
            replication_address: peer.replication_address.clone(),
            role: peer.role,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PgNativeConfiguration {
    pub current: ConfigurationDescriptor,
    pub previous: Option<ConfigurationDescriptor>,
    pub current_participants: Vec<PgNativeParticipant>,
    pub previous_participants: Vec<PgNativeParticipant>,
}

impl PgNativeConfiguration {
    fn validate(&self) -> Result<(), PgDurableError> {
        for descriptor in std::iter::once(&self.current).chain(self.previous.iter()) {
            kuberic_runtime::protocol::validation::validate_configuration(descriptor, None)
                .map_err(|error| PgDurableError::Invalid(error.to_string()))?;
        }
        for peers in [&self.current_participants, &self.previous_participants] {
            if peers.len() > crate::build::MAX_BUILDS * 2 {
                return Err(PgDurableError::Invalid(
                    "native participant bound exceeded".into(),
                ));
            }
            let mut identities = BTreeSet::new();
            for peer in peers {
                if peer.process_session_id.is_empty()
                    || peer.replication_address.is_empty()
                    || !identities.insert(&peer.identity)
                    || peer.storage_incarnation.as_ref().is_some_and(|storage| {
                        storage.replica != peer.identity || storage.validate().is_err()
                    })
                    || !std::iter::once(&self.current)
                        .chain(self.previous.iter())
                        .flat_map(|configuration| &configuration.members)
                        .any(|member| member.identity == peer.identity && member.role == peer.role)
                {
                    return Err(PgDurableError::Invalid(
                        "invalid exact native participant".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// Physical exclusion only, never running-standby or admission evidence.
pub struct PgNativeQuiescence {
    pub predecessor_epoch: Epoch,
    pub selected_epoch: Epoch,
    pub process_session_id: ProcessSessionId,
    pub stopped_process_generation: u64,
    pub lineage: crate::build::PgLineage,
    pub required_certified_lsn: i64,
    pub retained_flush_lsn: i64,
    pub configuration: Option<PgNativeConfiguration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgDurableState {
    pub version: u32,
    pub identity: PgDurableIdentity,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub process_clock_initialized: bool,
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
    #[serde(default, skip_serializing_if = "is_zero")]
    pub synchronous_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synchronous_process_generation: Option<u64>,
    pub accepted_build: Option<BuildAuthority>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_accepted_authority: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_build: Option<crate::build::PgBuildProgress>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outbound_builds: Vec<crate::build::PgBuildProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch_up: Option<(kuberic_runtime::protocol::types::ConfigurationId, i64)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suspended_builds: Vec<crate::build::PgBuildProgress>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub retired_builds: BTreeSet<OperationId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub outbound_attempts: BTreeMap<OperationId, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_epoch: Option<Epoch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_build_epoch: Option<Epoch>,
    pub native_configuration: Option<PgNativeConfiguration>,
    pub quiescence: Option<PgNativeQuiescence>,
    pub native_session_generation: u64,
    pub(crate) native_reformations: Vec<crate::adapter::public_v1::session::PgNativeReformation>,
    pub(crate) helper_executions: Vec<crate::owned_command::helper_receipt::HelperSelection>,
    pub(crate) postmaster_executions:
        Vec<crate::owned_command::helper_receipt::PostmasterSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) recovery: Option<crate::adapter::recovery::Recovery>,
}

impl PgDurableState {
    fn new(identity: PgDurableIdentity) -> Self {
        Self {
            version: STATE_VERSION,
            identity,
            generation: 1,
            process_clock_initialized: false,
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
            synchronous_generation: 0,
            synchronous_process_generation: None,
            accepted_build: None,
            has_accepted_authority: false,
            native_build: None,
            outbound_builds: Vec::new(),
            catch_up: None,
            suspended_builds: Vec::new(),
            retired_builds: BTreeSet::new(),
            outbound_attempts: BTreeMap::new(),
            build_epoch: None,
            retired_build_epoch: None,
            native_configuration: None,
            quiescence: None,
            native_session_generation: 0,
            native_reformations: Vec::new(),
            helper_executions: Vec::new(),
            postmaster_executions: Vec::new(),
            recovery: None,
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
            || self.synchronous_generation > self.generation
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
        if let Some(configuration) = &self.native_configuration {
            configuration.validate()?;
        }
        if let Some(quiescence) = &self.quiescence {
            quiescence
                .lineage
                .validate()
                .map_err(PgDurableError::Invalid)?;
            if quiescence.predecessor_epoch > quiescence.selected_epoch
                || quiescence.process_session_id.is_empty()
                || quiescence.required_certified_lsn < 0
                || quiescence.retained_flush_lsn < quiescence.required_certified_lsn
                || self.system_identifier.as_ref() != Some(&quiescence.lineage.system_identifier)
                || self.timeline_id != Some(quiescence.lineage.timeline)
                || !self.external_access_closed
                || !self.postgres_stopped
                || self.role != PgDurableRole::Primary
            {
                return Err(PgDurableError::Invalid(
                    "invalid physically quiescent native owner".into(),
                ));
            }
            if let Some(configuration) = &quiescence.configuration {
                configuration.validate()?;
            }
        }
        if self.native_reformations.len() > crate::adapter::public_v1::session::SESSION_LIMIT {
            return Err(PgDurableError::Invalid(
                "native reformation history exhausted".into(),
            ));
        }
        if self.helper_executions.len() > crate::owned_command::helper_receipt::MAX_EXECUTIONS {
            return Err(PgDurableError::Invalid(
                "native helper execution history exhausted".into(),
            ));
        }
        for (index, selection) in self.helper_executions.iter().enumerate() {
            selection
                .validate()
                .map_err(|error| PgDurableError::Invalid(error.to_string()))?;
            if selection.launch.execution_generation != index as u64 + 1 {
                return Err(PgDurableError::Invalid(
                    "native helper execution generations are not consecutive".into(),
                ));
            }
        }
        if self.postmaster_executions.len() > crate::owned_command::helper_receipt::MAX_EXECUTIONS {
            return Err(PgDurableError::Invalid(
                "native postmaster history exhausted".into(),
            ));
        }
        for (index, selection) in self.postmaster_executions.iter().enumerate() {
            selection
                .validate()
                .map_err(|error| PgDurableError::Invalid(error.to_string()))?;
            if selection.launch.execution_generation != index as u64 + 1
                || index > 0
                    && selection.launch.intent.process_generation
                        <= self.postmaster_executions[index - 1]
                            .launch
                            .intent
                            .process_generation
            {
                return Err(PgDurableError::Invalid(
                    "crossed postmaster execution history".into(),
                ));
            }
        }
        let mut reformation_ids = BTreeSet::new();
        for reformation in &self.native_reformations {
            reformation
                .validate(expected)
                .map_err(PgDurableError::Invalid)?;
            if !reformation_ids.insert(reformation.nonce()) {
                return Err(PgDurableError::Invalid(
                    "duplicate native reformation".into(),
                ));
            }
        }
        if let Some(recovery) = &self.recovery {
            recovery
                .validate(&expected.replica)
                .map_err(PgDurableError::Invalid)?;
        }
        if self.outbound_builds.len() + self.suspended_builds.len() > crate::build::MAX_BUILDS {
            return Err(PgDurableError::Invalid("too many native builds".into()));
        }
        if self.retired_builds.len() > MAX_RETAINED_BUILD_IDS
            || self.retired_builds.iter().any(|id| !bounded_build_id(id))
            || self
                .retired_build_epoch
                .is_some_and(|retired| self.build_epoch.is_none_or(|current| retired >= current))
        {
            return Err(PgDurableError::Invalid(
                "invalid bounded build history".into(),
            ));
        }
        let mut ids = std::collections::BTreeSet::new();
        for build in self.outbound_builds.iter().chain(&self.suspended_builds) {
            build.validate().map_err(PgDurableError::Invalid)?;
            if build.request.resource_uid != self.identity.resource_uid
                || build.request.authority.source != self.identity.replica
                || !ids.insert(build.request.authority.build_id.clone())
                || self.build_is_terminal(&build.request.authority)
            {
                return Err(PgDurableError::Invalid(
                    "invalid outbound native build".into(),
                ));
            }
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
        ids.extend(self.retired_builds.iter().cloned());
        if ids.len() > MAX_RETAINED_BUILD_IDS {
            return Err(PgDurableError::Invalid(
                "build history bound exceeded".into(),
            ));
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

    pub(crate) fn build_is_terminal(&self, authority: &BuildAuthority) -> bool {
        self.retired_builds.contains(&authority.build_id)
            || self
                .retired_build_epoch
                .is_some_and(|epoch| authority.current_configuration.epoch <= epoch)
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

    pub(crate) fn reconcile_builds(
        &mut self,
        current: &ReplicaSetConfiguration,
    ) -> Result<(), PgDurableError> {
        let epoch = current.configuration.epoch;
        if self.build_epoch.is_some_and(|previous| epoch < previous) {
            return Err(PgDurableError::Invalid(
                "build configuration is older than its durable epoch".into(),
            ));
        }
        if let Some(previous) = self.build_epoch
            && epoch > previous
        {
            self.retired_build_epoch = Some(previous);
            self.retired_builds.clear();
            self.outbound_builds
                .retain(|build| build.request.authority.current_configuration.epoch > previous);
            self.suspended_builds
                .retain(|build| build.request.authority.current_configuration.epoch > previous);
        }
        self.build_epoch = Some(epoch);
        let selected = self
            .native_build
            .iter()
            .chain(&self.outbound_builds)
            .chain(&self.suspended_builds)
            .filter(|build| crate::build::native_build_selected(&build.request, current))
            .map(|build| build.request.authority.build_id.clone())
            .collect::<BTreeSet<_>>();
        if selected.len() > crate::build::MAX_BUILDS
            || selected.iter().any(|id| !bounded_build_id(id))
        {
            return Err(PgDurableError::Invalid(
                "selected build descriptions exceed their bound".into(),
            ));
        }
        for build in &self.outbound_builds {
            if !selected.contains(&build.request.authority.build_id)
                && !self.retired_build_epoch.is_some_and(|floor| {
                    build.request.authority.current_configuration.epoch <= floor
                })
            {
                self.retired_builds
                    .insert(build.request.authority.build_id.clone());
            }
        }
        for build in &self.suspended_builds {
            if !selected.contains(&build.request.authority.build_id)
                && !self.retired_build_epoch.is_some_and(|floor| {
                    build.request.authority.current_configuration.epoch <= floor
                })
            {
                self.retired_builds
                    .insert(build.request.authority.build_id.clone());
            }
        }
        if let Some(build) = &self.native_build
            && !selected.contains(&build.request.authority.build_id)
            && self
                .retired_build_epoch
                .is_none_or(|floor| build.request.authority.current_configuration.epoch > floor)
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
        let mut known = self.retired_builds.clone();
        known.extend(selected);
        known.extend(
            self.outbound_builds
                .iter()
                .chain(&self.suspended_builds)
                .map(|build| build.request.authority.build_id.clone()),
        );
        if known.len() > MAX_RETAINED_BUILD_IDS {
            return Err(PgDurableError::Invalid(
                "build history bound reached; a newer admitted epoch is required".into(),
            ));
        }
        Ok(())
    }
}

fn bounded_build_id(id: &OperationId) -> bool {
    !id.is_empty()
        && id.as_str().len() <= MAX_BUILD_ID_BYTES
        && !id.as_str().chars().any(char::is_control)
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
    workers: StdMutex<Vec<Weak<CommitWorker>>>,
    ownership: Arc<File>,
    pub(crate) policy_lock: Arc<Mutex<()>>,
    pub(crate) policy_fence: std::sync::atomic::AtomicU64,
    pub(crate) helper_lock: Mutex<()>,
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
    state: Arc<GateState>,
}

#[cfg(feature = "testing")]
impl CommitGate {
    pub fn release(&self) {
        *self.state.released.lock().unwrap() = true;
        self.state.changed.notify_all();
    }
}

#[cfg(feature = "testing")]
impl Drop for CommitGate {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(feature = "testing")]
struct GateState {
    released: StdMutex<bool>,
    changed: Condvar,
}

#[cfg(feature = "testing")]
struct CommitHook {
    stage: CommitStage,
    catch_up_only: bool,
    entered: Arc<tokio::sync::Notify>,
    state: Arc<GateState>,
}

#[cfg(feature = "testing")]
impl CommitHook {
    fn checkpoint(&self, stage: CommitStage, control: &CommitControl) {
        if self.stage == stage {
            self.entered.notify_one();
            let mut released = self.state.released.lock().unwrap();
            while !*released && !control.cancel_requested.load(Ordering::Acquire) {
                released = self.state.changed.wait(released).unwrap();
            }
        }
    }
}

struct CommitControl {
    // Cancellation and the non-cancellable rename/publication section arbitrate
    // once: 0=pending, 1=committing, 2=cancelled.
    phase: AtomicU8,
    cancel_requested: AtomicBool,
    #[cfg(feature = "testing")]
    gate: Option<Arc<GateState>>,
}

impl CommitControl {
    fn cancel(&self) {
        self.cancel_requested.store(true, Ordering::Release);
        let _ = self
            .phase
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire);
        #[cfg(feature = "testing")]
        if let Some(gate) = &self.gate {
            let _released = gate.released.lock().unwrap();
            gate.changed.notify_all();
        }
    }

    fn begin_commit(&self) -> Result<(), PgDurableError> {
        self.phase
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| PgDurableError::Cancelled)
    }
}

struct CommitWorker {
    control: Arc<CommitControl>,
    thread: StdMutex<Option<std::thread::JoinHandle<()>>>,
}

impl CommitWorker {
    fn cancel_and_join(&self) {
        self.control.cancel();
        if let Some(thread) = self.thread.lock().unwrap().take() {
            let _ = thread.join();
        }
    }
}

impl Drop for CommitWorker {
    fn drop(&mut self) {
        self.cancel_and_join();
    }
}

struct CommitWaiter(Arc<CommitWorker>);

impl Drop for CommitWaiter {
    fn drop(&mut self) {
        self.0.cancel_and_join();
    }
}

impl Drop for PgDurableStore {
    fn drop(&mut self) {
        for worker in self
            .workers
            .get_mut()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
        {
            worker.cancel_and_join();
        }
    }
}

impl PgDurableStore {
    pub(crate) async fn prepare_process_clock(&self) -> Result<u64, crate::instance::PgError> {
        let initialized = self.snapshot().await.process_clock_initialized;
        let path = self.root.join("process-generation-v2");
        if !initialized && !path.exists() {
            self.write_process_clock(0)?;
        }
        let value = self.read_process_clock()?;
        if !initialized {
            self.update(|state| {
                state.process_clock_initialized = true;
                Ok(())
            })
            .await
            .map_err(|error| crate::instance::PgError::Process(error.to_string()))?;
        }
        Ok(value)
    }

    fn read_process_clock(&self) -> Result<u64, crate::instance::PgError> {
        use std::io::Read;
        let read = || -> std::io::Result<u64> {
            let mut file = File::open(self.root.join("process-generation-v2"))?;
            let mut data = [0; 40];
            file.read_exact(&mut data)?;
            let mut extra = [0];
            if file.read(&mut extra)? != 0 || Sha256::digest(&data[..8])[..] != data[8..] {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid process generation checksum/length",
                ));
            }
            Ok(u64::from_be_bytes(data[..8].try_into().unwrap()))
        };
        read().map_err(|error| {
            crate::instance::PgError::Process(format!("read process generation: {error}"))
        })
    }

    fn write_process_clock(&self, value: u64) -> Result<(), crate::instance::PgError> {
        use std::io::Write;
        let write = || -> std::io::Result<()> {
            let bytes = value.to_be_bytes();
            let path = self.root.join("process-generation-v2.next");
            let mut file = File::create(&path)?;
            file.write_all(&bytes)?;
            file.write_all(&Sha256::digest(bytes))?;
            file.sync_all()?;
            std::fs::rename(path, self.root.join("process-generation-v2"))?;
            File::open(&self.root)?.sync_all()
        };
        write().map_err(|error| {
            crate::instance::PgError::Process(format!("persist process generation: {error}"))
        })
    }

    pub(crate) fn advance_process_clock(
        &self,
        expected: u64,
        next: u64,
    ) -> Result<(), crate::instance::PgError> {
        if self.read_process_clock()? != expected || next <= expected {
            return Err(crate::instance::PgError::Process(
                "process generation changed outside its owner".into(),
            ));
        }
        self.write_process_clock(next)
    }

    #[cfg(feature = "testing")]
    pub fn pause_commit(&self, stage: CommitStage, catch_up_only: bool) -> CommitGate {
        let entered = Arc::new(tokio::sync::Notify::new());
        let state = Arc::new(GateState {
            released: StdMutex::new(false),
            changed: Condvar::new(),
        });
        *self.commit_hook.lock().unwrap() = Some(CommitHook {
            stage,
            catch_up_only,
            entered: entered.clone(),
            state: state.clone(),
        });
        CommitGate { entered, state }
    }

    pub async fn open(
        root: impl AsRef<Path>,
        expected: PgDurableIdentity,
        mode: StorageMode,
    ) -> Result<Self, PgDurableError> {
        let root = root.as_ref().to_path_buf();
        classify_metadata_root(&root, mode)?;
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
        crate::owned_command::helper_receipt::validate_cells(&root, &state.helper_executions)
            .map_err(|error| PgDurableError::Invalid(error.to_string()))?;
        // A newly opening durable owner has no live OwnedProcess capability.
        // Refuse before writable acquisition, including pre-bind repair moves.
        let postmasters_excluded = |state: &PgDurableState| -> Result<(), PgDurableError> {
            crate::owned_command::helper_receipt::validate_cells(
                &root,
                &state.postmaster_executions,
            )
            .map_err(|error| PgDurableError::Invalid(error.to_string()))?;
            for selected in &state.postmaster_executions {
                selected.completed().map_err(|error| {
                    PgDurableError::Invalid(format!(
                        "old native postmaster exclusion unproven: {error}"
                    ))
                })?;
            }
            Ok(())
        };
        postmasters_excluded(&state)?;
        let ownership = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("state-v3.owner"))
            .map_err(PgDurableError::Io)?;
        ownership.try_lock().map_err(|error| {
            let error: std::io::Error = error.into();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                PgDurableError::AlreadyOwned
            } else {
                PgDurableError::Io(error)
            }
        })?;
        let state = if mode == StorageMode::Established {
            let current = read_state(&path).await?;
            current.validate(&expected)?;
            postmasters_excluded(&current)?;
            current
        } else {
            state
        };
        let policy_generation = state.synchronous_generation;
        let store = Self {
            root,
            state: Arc::new(Mutex::new(state)),
            workers: StdMutex::new(Vec::new()),
            ownership: Arc::new(ownership),
            policy_lock: Arc::new(Mutex::new(())),
            policy_fence: std::sync::atomic::AtomicU64::new(policy_generation),
            helper_lock: Mutex::new(()),
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

    pub(crate) async fn helper_launch(
        &self,
        intent: crate::owned_command::helper_receipt::HelperIntent,
    ) -> Result<crate::owned_command::helper_receipt::HelperLaunch, crate::instance::PgError> {
        self.ensure_helpers_complete().await?;
        let state = self.snapshot().await;
        crate::owned_command::helper_receipt::HelperLaunch::new(
            &self.root,
            state.helper_executions.len() as u64 + 1,
            intent,
        )
        .map_err(|error| crate::instance::PgError::Process(error.to_string()))
    }

    pub(crate) async fn select_helper(
        &self,
        selection: crate::owned_command::helper_receipt::HelperSelection,
    ) -> Result<(), crate::instance::PgError> {
        selection
            .validate_cell()
            .map_err(|error| crate::instance::PgError::Process(error.to_string()))?;
        self.update(|state| {
            if selection.launch.root != self.root.canonicalize().map_err(PgDurableError::Io)?
                || selection.launch.execution_generation != state.helper_executions.len() as u64 + 1
            {
                return Err(PgDurableError::Invalid(
                    "crossed helper execution selection".into(),
                ));
            }
            for previous in &state.helper_executions {
                previous.completed().map_err(|error| {
                    PgDurableError::Invalid(format!(
                        "old native helper exclusion unproven: {error}"
                    ))
                })?;
            }
            for previous in &state.postmaster_executions {
                previous.completed().map_err(|error| {
                    PgDurableError::Invalid(format!(
                        "old native postmaster exclusion unproven: {error}"
                    ))
                })?;
            }
            state.helper_executions.push(selection);
            Ok(())
        })
        .await
        .map(|_| ())
        .map_err(|error| crate::instance::PgError::Process(error.to_string()))
    }

    pub(crate) async fn ensure_helpers_complete(&self) -> Result<(), crate::instance::PgError> {
        self.ensure_executions_complete(None).await
    }

    pub(crate) async fn postmaster_launch(
        &self,
        intent: crate::owned_command::helper_receipt::PostmasterIntent,
    ) -> Result<crate::owned_command::helper_receipt::PostmasterLaunch, crate::instance::PgError>
    {
        self.ensure_helpers_complete().await?;
        crate::owned_command::helper_receipt::PostmasterLaunch::new(
            &self.root,
            self.snapshot().await.postmaster_executions.len() as u64 + 1,
            intent,
        )
        .map_err(|error| crate::instance::PgError::Process(error.to_string()))
    }

    pub(crate) async fn select_postmaster(
        &self,
        selection: crate::owned_command::helper_receipt::PostmasterSelection,
    ) -> Result<(), crate::instance::PgError> {
        selection
            .validate_cell()
            .map_err(|error| crate::instance::PgError::Process(error.to_string()))?;
        self.update(|state| {
            if selection.launch.root != self.root.canonicalize().map_err(PgDurableError::Io)?
                || selection.launch.execution_generation
                    != state.postmaster_executions.len() as u64 + 1
            {
                return Err(PgDurableError::Invalid(
                    "crossed postmaster execution selection".into(),
                ));
            }
            for previous in &state.helper_executions {
                previous
                    .completed()
                    .map_err(|error| PgDurableError::Invalid(error.to_string()))?;
            }
            for previous in &state.postmaster_executions {
                previous
                    .completed()
                    .map_err(|error| PgDurableError::Invalid(error.to_string()))?;
            }
            state.postmaster_executions.push(selection);
            Ok(())
        })
        .await
        .map(|_| ())
        .map_err(|error| crate::instance::PgError::Process(error.to_string()))
    }

    pub(crate) async fn ensure_executions_complete(
        &self,
        current: Option<&crate::owned_command::helper_receipt::PostmasterSelection>,
    ) -> Result<(), crate::instance::PgError> {
        let state = self.revalidate().await.map_err(|error| {
            crate::instance::PgError::Process(format!("native helper selection: {error}"))
        })?;
        crate::owned_command::helper_receipt::validate_cells(&self.root, &state.helper_executions)
            .map_err(|error| crate::instance::PgError::Process(error.to_string()))?;
        for selection in &state.helper_executions {
            selection.completed().map_err(|error| {
                crate::instance::PgError::Process(format!(
                    "old native helper exclusion unproven: {error}"
                ))
            })?;
        }
        crate::owned_command::helper_receipt::validate_cells(
            &self.root,
            &state.postmaster_executions,
        )
        .map_err(|error| crate::instance::PgError::Process(error.to_string()))?;
        if current.is_some_and(|current| state.postmaster_executions.last() != Some(current)) {
            return Err(crate::instance::PgError::Process(
                "current postmaster selection changed".into(),
            ));
        }
        for selection in &state.postmaster_executions {
            if Some(selection) != current {
                selection.completed().map_err(|error| {
                    crate::instance::PgError::Process(format!(
                        "old native postmaster exclusion unproven: {error}"
                    ))
                })?;
            }
        }
        Ok(())
    }

    pub async fn update(
        &self,
        update: impl FnOnce(&mut PgDurableState) -> Result<(), PgDurableError>,
    ) -> Result<PgDurableState, PgDurableError> {
        let mut state = self.state.clone().lock_owned().await;
        let mut next = state.clone();
        update(&mut next)?;
        if next.helper_executions.len() < state.helper_executions.len()
            || !next.helper_executions.starts_with(&state.helper_executions)
        {
            return Err(PgDurableError::Invalid(
                "native helper selection history is immutable".into(),
            ));
        }
        if !next
            .postmaster_executions
            .starts_with(&state.postmaster_executions)
        {
            return Err(PgDurableError::Invalid(
                "native postmaster history is immutable".into(),
            ));
        }
        if next.native_reformations.len() < state.native_reformations.len()
            || next
                .native_reformations
                .iter()
                .zip(&state.native_reformations)
                .any(|(next, previous)| !next.continues(previous))
        {
            return Err(PgDurableError::Invalid(
                "native reformation history cannot regress or erase uncertainty".into(),
            ));
        }
        if next.native_session_generation < state.native_session_generation {
            return Err(PgDurableError::Invalid(
                "native session generation cannot regress".into(),
            ));
        }
        next.generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| PgDurableError::Invalid("durable generation overflow".into()))?;
        next.validate(&state.identity)?;
        if next.system_identifier == state.system_identifier
            && next.recovery_state != PgRecoveryState::Rebuilding
            && !next
                .native_reformations
                .iter()
                .any(|record| record.completes_installed_repair(&state, &next))
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
        let control = Arc::new(CommitControl {
            phase: AtomicU8::new(0),
            cancel_requested: AtomicBool::new(false),
            #[cfg(feature = "testing")]
            gate: hook.as_ref().map(|hook| hook.state.clone()),
        });
        let worker = Arc::new(CommitWorker {
            control: control.clone(),
            thread: StdMutex::new(None),
        });
        let waiter = CommitWaiter(worker.clone());
        let (completed, result) = tokio::sync::oneshot::channel();
        let ownership = self.ownership.clone();
        let thread = std::thread::Builder::new()
            .name("postgres-metadata-commit".into())
            .spawn(move || {
                let _ownership = ownership;
                let committed = (|| {
                    let _file_lock = lock_metadata(&path, Some(&control))?;
                    if read_state_sync(&path)? != *state {
                        return Err(PgDurableError::Invalid(
                            "metadata generation changed outside its owner".into(),
                        ));
                    }
                    let mut renamed = false;
                    let result = write_state(
                        &path,
                        &next,
                        &mut renamed,
                        || control.begin_commit(),
                        |stage| {
                            #[cfg(feature = "testing")]
                            if let Some(hook) = &hook {
                                hook.checkpoint(stage, &control);
                            }
                            #[cfg(not(feature = "testing"))]
                            let _ = stage;
                        },
                    );
                    if renamed {
                        *state = next.clone();
                        #[cfg(feature = "testing")]
                        if let Some(hook) = &hook {
                            hook.checkpoint(CommitStage::Published, &control);
                        }
                    }
                    result?;
                    Ok(next)
                })();
                drop(state);
                let _ = completed.send(committed);
            })
            .map_err(PgDurableError::Io)?;
        *worker.thread.lock().unwrap() = Some(thread);
        {
            let mut workers = self.workers.lock().unwrap();
            workers.retain(|worker| worker.strong_count() != 0);
            workers.push(Arc::downgrade(&worker));
        }
        let committed = result.await.map_err(|_| {
            PgDurableError::Invalid("metadata worker did not publish an outcome".into())
        })?;
        drop(waiter);
        committed
    }

    async fn persist(&self) -> Result<(), PgDurableError> {
        let state = self.state.lock().await.clone();
        let path = self.root.join(STATE_FILE);
        let _file_lock = lock_metadata(&path, None)?;
        if path.exists() {
            return Err(PgDurableError::AlreadyExists);
        }
        write_state(&path, &state, &mut false, || Ok(()), |_| {})
    }

    #[cfg(test)]
    pub(crate) async fn persist_unchecked_for_test(
        &self,
        state: PgDurableState,
    ) -> Result<(), PgDurableError> {
        let path = self.root.join(STATE_FILE);
        let _file_lock = lock_metadata(&path, None)?;
        write_state(&path, &state, &mut false, || Ok(()), |_| {})?;
        *self.state.lock().await = state;
        Ok(())
    }
}

async fn read_state(path: &Path) -> Result<PgDurableState, PgDurableError> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path)
        .await
        .map_err(PgDurableError::Io)?;
    let mut bytes = Vec::new();
    file.take(MAX_METADATA_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(PgDurableError::Io)?;
    decode_state(&bytes)
}

fn read_state_sync(path: &Path) -> Result<PgDurableState, PgDurableError> {
    use std::io::Read;
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(PgDurableError::Io)?
        .take(MAX_METADATA_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(PgDurableError::Io)?;
    decode_state(&bytes)
}

fn decode_state(bytes: &[u8]) -> Result<PgDurableState, PgDurableError> {
    if bytes.len() > MAX_METADATA_BYTES {
        return Err(PgDurableError::Invalid(
            "metadata byte bound exceeded".into(),
        ));
    }
    let header: serde_json::Value = serde_json::from_slice(bytes).map_err(PgDurableError::Json)?;
    let observed = header
        .get("state")
        .and_then(|state| state.get("version"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| PgDurableError::Invalid("native metadata has no explicit version".into()))?;
    if observed != STATE_VERSION {
        return Err(PgDurableError::Version {
            expected: STATE_VERSION,
            observed,
        });
    }
    let envelope: StateEnvelope = serde_json::from_slice(bytes).map_err(PgDurableError::Json)?;
    let checksum = checksum(&envelope.state)?;
    if checksum != envelope.checksum {
        return Err(PgDurableError::Checksum);
    }
    Ok(envelope.state)
}

fn lock_metadata(path: &Path, control: Option<&CommitControl>) -> Result<File, PgDurableError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
        .map_err(PgDurableError::Io)?;
    loop {
        if control.is_some_and(|control| control.cancel_requested.load(Ordering::Acquire)) {
            return Err(PgDurableError::Cancelled);
        }
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(error) => {
                let error: std::io::Error = error.into();
                if error.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(PgDurableError::Io(error));
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }
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
    begin_commit: impl FnOnce() -> Result<(), PgDurableError>,
    checkpoint: impl Fn(CommitStage),
) -> Result<(), PgDurableError> {
    let envelope = StateEnvelope {
        state: state.clone(),
        checksum: checksum(state)?,
    };
    let bytes = serde_json::to_vec_pretty(&envelope).map_err(PgDurableError::Json)?;
    if bytes.len() > MAX_METADATA_BYTES {
        return Err(PgDurableError::Invalid(
            "metadata byte bound exceeded".into(),
        ));
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes).map_err(PgDurableError::Io)?;
    File::open(&temporary)
        .and_then(|file| file.sync_all())
        .map_err(PgDurableError::Io)?;
    checkpoint(CommitStage::BeforeRename);
    if let Err(error) = begin_commit() {
        std::fs::remove_file(&temporary).map_err(PgDurableError::Io)?;
        return Err(error);
    }
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

fn classify_metadata_root(root: &Path, mode: StorageMode) -> Result<(), PgDurableError> {
    use std::os::unix::fs::MetadataExt;

    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(PgDurableError::Io(error)),
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            return Err(PgDurableError::Invalid(
                "native metadata root is not an owned directory".into(),
            ));
        }
        Ok(_) => {}
    }
    let mut nonempty = false;
    for entry in std::fs::read_dir(root).map_err(PgDurableError::Io)? {
        let entry = entry.map_err(PgDurableError::Io)?;
        nonempty = true;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(PgDurableError::Invalid(
                "native metadata filename is not UTF-8".into(),
            ));
        };
        let Some(version) = name.strip_prefix("state-v") else {
            continue;
        };
        if let Some(observed) = version
            .split('.')
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            && observed != STATE_VERSION
        {
            return Err(PgDurableError::Version {
                expected: STATE_VERSION,
                observed,
            });
        }
        let metadata = std::fs::symlink_metadata(entry.path()).map_err(PgDurableError::Io)?;
        if !matches!(
            name,
            STATE_FILE | "state-v3.owner" | "state-v3.lock" | "state-v3.json.tmp"
        ) && !crate::owned_command::helper_receipt::is_cell_name(name)
            || !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.nlink() != 1
        {
            return Err(PgDurableError::Invalid(
                "unrecognized or aliased native metadata object".into(),
            ));
        }
    }
    if mode == StorageMode::Fresh && nonempty {
        return Err(PgDurableError::AlreadyExists);
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
    #[error("PostgreSQL durable state already has a live owner")]
    AlreadyOwned,
    #[error("PostgreSQL metadata update cancelled before commit")]
    Cancelled,
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
    use kuberic_runtime::protocol::types::{AgentGeneration, ReplicaId, ReplicaInstanceId};

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

    #[cfg(feature = "testing")]
    fn metadata_tree(root: &Path) -> BTreeMap<std::ffi::OsString, Vec<u8>> {
        std::fs::read_dir(root)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let bytes = if entry.file_type().unwrap().is_symlink() {
                    std::fs::read_link(entry.path())
                        .unwrap()
                        .into_os_string()
                        .into_encoded_bytes()
                } else {
                    std::fs::read(entry.path()).unwrap()
                };
                (entry.file_name(), bytes)
            })
            .collect()
    }

    #[cfg(feature = "testing")]
    #[tokio::test]
    async fn m3_native_metadata_three_rejects_legacy_unknown_and_mixed_roots_before_mutation() {
        for observed in [1, 2, 9] {
            for mode in [StorageMode::Fresh, StorageMode::Established] {
                for mixed in [false, true] {
                    let directory = crate::testing::TestDataDir::new("native-three-incompatible");
                    let root = directory.path().join("metadata");
                    std::fs::create_dir(&root).unwrap();
                    std::fs::write(
                        root.join(format!("state-v{observed}.json")),
                        b"retained incompatible bytes, not a fresh-store invitation",
                    )
                    .unwrap();
                    std::fs::write(root.join("process-generation-v2"), b"73").unwrap();
                    if mixed {
                        write_state(
                            &root.join(STATE_FILE),
                            &PgDurableState::new(identity()),
                            &mut false,
                            || Ok(()),
                            |_| {},
                        )
                        .unwrap();
                    }
                    let before = metadata_tree(&root);
                    assert!(matches!(
                        PgDurableStore::open(&root, identity(), mode).await,
                        Err(PgDurableError::Version {
                            expected: 3,
                            observed: version
                        }) if version == observed
                    ));
                    assert_eq!(metadata_tree(&root), before);
                    assert!(!root.join("state-v3.owner").exists());
                    assert!(!root.join("state-v3.lock").exists());
                }
            }
        }
    }

    #[cfg(feature = "testing")]
    #[tokio::test]
    async fn m3_native_metadata_three_refuses_aliased_missing_and_unknown_heads() {
        use std::os::unix::fs::symlink;

        for kind in ["symlink", "dangling", "hardlink", "unknown", "missing"] {
            for mode in [StorageMode::Fresh, StorageMode::Established] {
                let directory = crate::testing::TestDataDir::new("native-three-head");
                let root = directory.path().join("metadata");
                std::fs::create_dir(&root).unwrap();
                let foreign = directory.path().join("foreign");
                write_state(
                    &foreign,
                    &PgDurableState::new(identity()),
                    &mut false,
                    || Ok(()),
                    |_| {},
                )
                .unwrap();
                let head = root.join(STATE_FILE);
                match kind {
                    "symlink" => symlink(&foreign, &head).unwrap(),
                    "dangling" => symlink(directory.path().join("absent"), &head).unwrap(),
                    "hardlink" => std::fs::hard_link(&foreign, &head).unwrap(),
                    "unknown" => {
                        std::fs::write(root.join("state-v3.unsupported"), b"unknown").unwrap();
                    }
                    "missing" => {
                        std::fs::write(root.join("process-generation-v2"), b"73").unwrap();
                    }
                    _ => unreachable!(),
                }
                let before = metadata_tree(&root);
                let foreign_before = std::fs::read(&foreign).unwrap();
                assert!(PgDurableStore::open(&root, identity(), mode).await.is_err());
                assert_eq!(metadata_tree(&root), before);
                assert_eq!(std::fs::read(&foreign).unwrap(), foreign_before);
                assert!(!root.join("state-v3.owner").exists());
                assert!(!root.join("state-v3.lock").exists());
            }
        }
    }

    #[cfg(feature = "testing")]
    #[tokio::test]
    async fn m3_pg_metadata_one_rejected_before_exclusive_owner_or_clock_mutation() {
        let directory = crate::testing::TestDataDir::new("native-metadata-one");
        let root = directory.path().join("metadata");
        std::fs::create_dir(&root).unwrap();
        let mut state = PgDurableState::new(identity());
        state.version = 1;
        let path = root.join(STATE_FILE);
        write_state(&path, &state, &mut false, || Ok(()), |_| {}).unwrap();
        std::fs::write(
            root.join("retained-native-input"),
            b"do not migrate or reset",
        )
        .unwrap();
        let tree = |root: &Path| {
            std::fs::read_dir(root)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), std::fs::read(entry.path()).unwrap())
                })
                .collect::<BTreeMap<_, _>>()
        };
        let before = tree(&root);
        assert!(matches!(
            PgDurableStore::open(&root, identity(), StorageMode::Established).await,
            Err(PgDurableError::Version {
                expected: 3,
                observed: 1
            }),
        ));
        assert_eq!(tree(&root), before);
        assert!(!root.join("state-v3.owner").exists());
        assert!(!root.join("process-generation-v2").exists());
    }

    #[cfg(feature = "testing")]
    #[tokio::test]
    async fn m2_pg_native_build_selection_preserves_durable_authority_without_host_descriptor_ids()
    {
        use crate::build::{
            PgBuildMethod, PgBuildProgress, PgBuildRequest, PgBuildStage, PgLineage,
        };
        use kuberic_runtime::protocol::types::{
            BuildAuthorityKind, ConfigurationDescriptor, ConfigurationMember, ProcessSessionId,
            ReplicaRole,
        };
        use kuberic_runtime::replicator::{ReplicaInformation, ReplicaSetConfiguration};

        let root = crate::testing::TestDataDir::new("native-selection");
        let source = identity().replica;
        let target = ReplicaIdentity {
            replica_id: ReplicaId::new(2),
            instance_id: ReplicaInstanceId::new("target-pod"),
            agent_generation: AgentGeneration::new("target-generation"),
        };
        let current = ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            source.replica_id,
            vec![ConfigurationMember {
                identity: source.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        let mut source_peer = ReplicaInformation::active_peer(
            source.clone(),
            "http://127.0.0.1:5001".into(),
            ProcessSessionId::new("source-process"),
            7,
            0,
            false,
        )
        .unwrap();
        source_peer.role = ReplicaRole::Primary;
        let mut target_peer =
            ReplicaInformation::idle_build(target.clone(), "http://127.0.0.1:5002".into());
        target_peer.process_session_id = ProcessSessionId::new("target-process");
        target_peer.current_progress = 7;
        let configuration = ReplicaSetConfiguration {
            configuration: current.clone(),
            replicas: vec![source_peer, target_peer],
        };
        let request = PgBuildRequest {
            version: crate::build::BUILD_PROTOCOL_VERSION,
            resource_uid: identity().resource_uid,
            authority: BuildAuthority {
                build_id: OperationId::new("independent-native-id"),
                kind: BuildAuthorityKind::Provisioning,
                source,
                target,
                current_configuration: current,
                replication_boundary_lsn: 7,
            },
            source_session: ProcessSessionId::new("source-process"),
            target_session: ProcessSessionId::new("target-process"),
            source_endpoint: "http://127.0.0.1:5001".into(),
            source_host: "127.0.0.1".into(),
            source_port: 5432,
            lineage: PgLineage {
                system_identifier: "1".into(),
                timeline: 1,
                history: Vec::new(),
                history_text: String::new(),
            },
        };
        request.validate().unwrap();
        assert!(crate::build::native_build_selected(
            &request,
            &configuration
        ));
        for dimension in ["source", "target", "epoch", "boundary", "role"] {
            let mut changed = configuration.clone();
            match dimension {
                "source" => {
                    changed.replicas[0].process_session_id = ProcessSessionId::new("old-source")
                }
                "target" => {
                    changed.replicas[1].process_session_id = ProcessSessionId::new("old-target")
                }
                "epoch" => changed.configuration.epoch.configuration_number += 1,
                "boundary" => changed.replicas[1].current_progress += 1,
                "role" => changed.replicas[1].role = ReplicaRole::ActiveSecondary,
                _ => unreachable!(),
            }
            assert!(
                !crate::build::native_build_selected(&request, &changed),
                "{dimension}"
            );
        }
        let store = PgDurableStore::open(root.path().join("meta"), identity(), StorageMode::Fresh)
            .await
            .unwrap();
        let progress = PgBuildProgress {
            request: request.clone(),
            stage: PgBuildStage::Intent,
            method: PgBuildMethod::Fresh,
            sequence: 1,
            evidence: None,
        };
        store
            .update(|state| {
                state.outbound_builds.push(progress.clone());
                state.reconcile_builds(&configuration)
            })
            .await
            .unwrap();
        let before = store.snapshot().await;
        assert_eq!(before.outbound_builds, vec![progress]);
        drop(store);
        let store = PgDurableStore::open(
            root.path().join("meta"),
            identity(),
            StorageMode::Established,
        )
        .await
        .unwrap();
        assert_eq!(store.snapshot().await, before);
        let mut successor = configuration;
        successor.replicas[1].process_session_id = ProcessSessionId::new("successor-process");
        store
            .update(|state| state.reconcile_builds(&successor))
            .await
            .unwrap();
        let retired = store.snapshot().await;
        assert!(retired.outbound_builds.is_empty());
        assert!(retired.retired_builds.contains(&request.authority.build_id));
        assert!(retired.build_is_terminal(&request.authority));
        assert_eq!(retired.version, before.version);
        assert_eq!(retired.identity, before.identity);
    }

    #[cfg(feature = "testing")]
    #[tokio::test]
    async fn exhausted_policy_and_metadata_generations_remain_fenced_after_reopen() {
        use crate::instance::PgInstanceManager;
        use crate::native::{PgNativeObserver, compile_synchronous_configuration};
        use crate::testing::{TestDataDir, allocate_port, find_pg_bin, native_configuration};
        let root = TestDataDir::new("policy-max");
        let store = Arc::new(
            PgDurableStore::open(root.path().join("meta"), identity(), StorageMode::Fresh)
                .await
                .unwrap(),
        );
        let instance = Arc::new(PgInstanceManager::new(
            root.path().join("pgdata"),
            find_pg_bin(),
            allocate_port().await,
        ));
        instance.init_db().await.unwrap();
        let (faults, _rx) = tokio::sync::mpsc::channel(8);
        instance.start_native(faults).await.unwrap();
        let policy = compile_synchronous_configuration(
            None,
            &native_configuration(std::slice::from_ref(&identity().replica), 0, 1),
            &identity().replica,
            &BTreeMap::new(),
            true,
        )
        .unwrap();
        let observer = PgNativeObserver::with_store(instance.clone(), store.clone()).await;
        observer.apply_synchronous(policy.clone()).await.unwrap();
        {
            let mut state = store.state.lock().await;
            state.generation = u64::MAX;
            state.synchronous_generation = u64::MAX;
            write_state(
                &store.root.join(STATE_FILE),
                &state,
                &mut false,
                || Ok(()),
                |_| {},
            )
            .unwrap();
        }
        assert!(observer.apply_synchronous(policy.clone()).await.is_err());
        assert!(
            !observer
                .snapshot()
                .await
                .unwrap()
                .evidence
                .unwrap()
                .synchronous
                .unwrap()
                .valid
        );
        assert_eq!(store.revalidate().await.unwrap().generation, u64::MAX);
        drop(observer);
        drop(store);
        let store = Arc::new(
            PgDurableStore::open(
                root.path().join("meta"),
                identity(),
                StorageMode::Established,
            )
            .await
            .unwrap(),
        );
        let observer = PgNativeObserver::with_store(instance.clone(), store.clone()).await;
        assert!(
            !observer
                .snapshot()
                .await
                .unwrap()
                .evidence
                .unwrap()
                .synchronous
                .unwrap()
                .valid
        );
        assert!(observer.apply_synchronous(policy).await.is_err());
        assert_eq!(
            store.revalidate().await.unwrap().synchronous_generation,
            u64::MAX
        );
        instance.stop().await.unwrap();
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
