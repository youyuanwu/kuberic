//! Reusable replica-process hosting for stateful applications.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::StatefulServiceReplica;
use crate::protocol::types::{
    FaultType, PodUid, PvcUid, ReplicaId, ReplicaInstanceId, ResourceUid,
};
use serde::Serialize;
use tokio::sync::{Mutex, watch};

use crate::host::Result;
use crate::host::hosting::PodRuntime;
use crate::host::provisioning::{ObservedStorageIdentity, validate_established_identity};
use crate::host::service::{AgentService, InitializationService, SHUTDOWN_TIMEOUT};
use crate::host::sqlite_store::SqliteStore;
use crate::host::store::AgentStore;
use crate::host::transport::{
    GrpcOutboundDispatcher, ReliableTransport, ReplicaEndpointResolver, run_outbound,
    run_peer_discovery,
};

#[cfg(all(feature = "testing", kuberic_workspace_tests))]
use crate::protocol::public_operations::{
    PublicFaultAction, RestartActionRecord, RestartActionStage,
};
#[cfg(all(feature = "testing", kuberic_workspace_tests))]
use crate::protocol::types::ProcessSessionId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationStorageState {
    FreshEmpty,
    Established,
}

#[derive(Debug, Clone)]
pub struct ReplicaProcessConfig {
    pub resource_uid: ResourceUid,
    pub replica_id: ReplicaId,
    pub pod_uid: PodUid,
    pub pvc_uid: PvcUid,
    pub data_root: PathBuf,
    pub control_address: SocketAddr,
    pub replication_address: SocketAddr,
    pub bearer_token: String,
    pub rpc_deadline: Duration,
    pub transport_window_capacity: usize,
}

#[cfg(all(feature = "testing", kuberic_workspace_tests))]
#[allow(dead_code)]
pub(crate) const PREVIEW_RESTART_DISPOSITION: i32 = 75;

#[cfg(all(feature = "testing", kuberic_workspace_tests))]
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct PreviewChildCommand {
    pub(crate) executable: PathBuf,
    pub(crate) arguments: Vec<String>,
    pub(crate) environment: BTreeMap<String, String>,
}

#[cfg(all(feature = "testing", kuberic_workspace_tests))]
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreviewChildEvidence {
    pub(crate) process_session: ProcessSessionId,
    pub(crate) child_pid: u32,
    pub(crate) data_root: PathBuf,
    pub(crate) pod_uid: PodUid,
    pub(crate) pvc_uid: PvcUid,
    pub(crate) provider_sentinel: String,
    pub(crate) application_instance_id: String,
    pub(crate) replicator_instance_id: String,
    pub(crate) callbacks: Vec<String>,
    pub(crate) launch_nonce: String,
}

#[cfg(all(feature = "testing", kuberic_workspace_tests))]
#[allow(dead_code)]
pub(crate) struct PreviewChildProcess {
    pub(crate) child: tokio::process::Child,
    pub(crate) evidence: PreviewChildEvidence,
    pub(crate) restart_signal: PathBuf,
}

#[cfg(all(feature = "testing", kuberic_workspace_tests))]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviewRestartCut {
    Accepted,
    PredecessorContained,
    SuccessorLaunched,
    SuccessorStarted,
}

#[cfg(all(feature = "testing", kuberic_workspace_tests))]
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreviewRestartResult {
    pub(crate) record: RestartActionRecord,
    pub(crate) successor: PreviewChildEvidence,
}

#[cfg(all(feature = "testing", kuberic_workspace_tests))]
#[derive(serde::Serialize, serde::Deserialize)]
struct PreviewSupervisorIdentity {
    pid: u32,
    start_time: String,
}

#[cfg(all(feature = "testing", kuberic_workspace_tests))]
#[derive(serde::Serialize, serde::Deserialize)]
struct PreviewLaunchOwner {
    nonce: String,
    supervisor_id: String,
    pid: u32,
    start_time: String,
}

#[cfg(all(feature = "testing", kuberic_workspace_tests))]
#[allow(dead_code)]
pub(crate) struct ReplicaProcessSupervisor {
    store: Arc<SqliteStore>,
    data_root: PathBuf,
    child: PreviewChildCommand,
    active_child: Mutex<Option<tokio::process::Child>>,
    launch: Mutex<()>,
    container_restart_proven: bool,
    instance_id: String,
}

#[cfg(all(feature = "testing", kuberic_workspace_tests))]
#[allow(dead_code)]
impl ReplicaProcessSupervisor {
    pub(crate) fn new(
        store: Arc<SqliteStore>,
        data_root: PathBuf,
        child: PreviewChildCommand,
    ) -> Result<Self> {
        let marker = data_root.join(".kuberic").join("supervisor-session");
        let container_restart_proven = std::fs::read(&marker)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PreviewSupervisorIdentity>(&bytes).ok())
            .is_some_and(|previous| !process_identity_is_alive(&previous));
        Self::record_parent_identity(&data_root)?;
        Ok(Self {
            store,
            data_root,
            child,
            active_child: Mutex::new(None),
            launch: Mutex::new(()),
            container_restart_proven,
            instance_id: uuid::Uuid::new_v4().to_string(),
        })
    }

    pub(crate) fn record_parent_identity(data_root: &Path) -> Result<()> {
        let marker = data_root.join(".kuberic").join("supervisor-session");
        if let Some(parent) = marker.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let identity = PreviewSupervisorIdentity {
            pid: std::process::id(),
            start_time: process_start_time(std::process::id()).ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "cannot read supervisor process identity".into(),
                )
            })?,
        };
        std::fs::write(
            marker,
            serde_json::to_vec(&identity)
                .map_err(|error| crate::host::HostError::Corrupt(error.to_string()))?,
        )?;
        Ok(())
    }

    pub(crate) async fn restart_with_child(
        &self,
        action: &PublicFaultAction,
        predecessor: &mut PreviewChildProcess,
        cut: Option<PreviewRestartCut>,
    ) -> Result<PreviewRestartResult> {
        let record = self.store.begin_restart_action(action).await?;
        if cut == Some(PreviewRestartCut::Accepted) {
            return Err(crate::host::HostError::CommandRejected(
                "injected crash after durable restart acceptance".into(),
            ));
        }

        if record.stage == RestartActionStage::Accepted {
            if predecessor.evidence.process_session != action.predecessor_session
                || predecessor.evidence.child_pid != action.predecessor_process_id
                || predecessor.child.id() != Some(predecessor.evidence.child_pid)
            {
                return Err(crate::host::HostError::IdentityMismatch(
                    "restart action does not name the supervised predecessor child".into(),
                ));
            }
            std::fs::write(&predecessor.restart_signal, b"restart")?;
            let status = predecessor.child.wait().await?;
            if status.code() != Some(PREVIEW_RESTART_DISPOSITION) {
                return Err(crate::host::HostError::CommandRejected(format!(
                    "predecessor did not exit with restart disposition {PREVIEW_RESTART_DISPOSITION}"
                )));
            }
        }
        self.resume_existing(action, cut).await
    }

    pub(crate) async fn resume_after_container_restart(
        &self,
        action: &PublicFaultAction,
        cut: Option<PreviewRestartCut>,
    ) -> Result<PreviewRestartResult> {
        let record = self.store.restart_action().await?.ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "container recovery cannot admit a new restart action".into(),
            )
        })?;
        if record.action != *action {
            return Err(crate::host::HostError::IdentityMismatch(
                "container recovery action differs from durable acceptance".into(),
            ));
        }
        if record.stage == RestartActionStage::Accepted
            && (!self.container_restart_proven
                || Path::new(&format!("/proc/{}", action.predecessor_process_id)).exists())
        {
            return Err(crate::host::HostError::CommandRejected(
                "durable supervisor marker does not prove a parent/container restart".into(),
            ));
        }
        self.resume_existing(action, cut).await
    }

    async fn resume_existing(
        &self,
        action: &PublicFaultAction,
        cut: Option<PreviewRestartCut>,
    ) -> Result<PreviewRestartResult> {
        let _launch = self.launch.lock().await;
        let mut record = self.store.restart_action().await?.ok_or_else(|| {
            crate::host::HostError::CommandRejected("restart action is not accepted".into())
        })?;
        if record.action != *action {
            return Err(crate::host::HostError::IdentityMismatch(
                "restart action changed during recovery".into(),
            ));
        }
        if cut == Some(PreviewRestartCut::Accepted) && record.stage == RestartActionStage::Accepted
        {
            return Err(crate::host::HostError::CommandRejected(
                "injected crash after durable restart acceptance".into(),
            ));
        }
        if record.stage == RestartActionStage::Accepted {
            record = self
                .store
                .advance_restart_action(
                    action,
                    RestartActionStage::Accepted,
                    RestartActionStage::PredecessorContained,
                    None,
                    None,
                    None,
                )
                .await?;
        }
        if cut == Some(PreviewRestartCut::PredecessorContained)
            && record.stage == RestartActionStage::PredecessorContained
        {
            return Err(crate::host::HostError::CommandRejected(
                "injected crash after predecessor containment".into(),
            ));
        }
        let (launch_nonce, may_spawn) = self.claim_successor_launch(action)?;
        if record.stage == RestartActionStage::PredecessorContained {
            record = self
                .store
                .advance_restart_action(
                    action,
                    RestartActionStage::PredecessorContained,
                    RestartActionStage::SuccessorLaunching,
                    None,
                    None,
                    Some(&launch_nonce),
                )
                .await?;
        }
        if record.stage == RestartActionStage::SuccessorStarted {
            let successor = self.read_successor_evidence(action)?;
            return Ok(PreviewRestartResult { record, successor });
        }
        if record.stage != RestartActionStage::SuccessorLaunching
            || record.launch_nonce.as_deref() != Some(launch_nonce.as_str())
        {
            return Err(crate::host::HostError::DurableEffectConflict(
                "successor launch claim differs from durable restart state".into(),
            ));
        }
        let successor = self
            .launch_successor(action, &launch_nonce, may_spawn)
            .await?;
        if cut == Some(PreviewRestartCut::SuccessorLaunched) {
            return Err(crate::host::HostError::CommandRejected(
                "injected crash after successor launch".into(),
            ));
        }
        record = self
            .store
            .advance_restart_action(
                action,
                RestartActionStage::SuccessorLaunching,
                RestartActionStage::SuccessorStarted,
                Some(&successor.process_session),
                Some(successor.child_pid),
                None,
            )
            .await?;
        if cut == Some(PreviewRestartCut::SuccessorStarted) {
            return Err(crate::host::HostError::CommandRejected(
                "injected crash after successor start".into(),
            ));
        }
        Ok(PreviewRestartResult { record, successor })
    }

    fn evidence_path(&self, action: &PublicFaultAction) -> PathBuf {
        self.data_root
            .join(".kuberic")
            .join(format!("{}.successor.json", action.action_id))
    }

    fn shutdown_path(&self, action: &PublicFaultAction) -> PathBuf {
        self.data_root
            .join(".kuberic")
            .join(format!("{}.successor.shutdown", action.action_id))
    }

    fn launch_owner_path(&self, action: &PublicFaultAction) -> PathBuf {
        self.data_root
            .join(".kuberic")
            .join(format!("{}.launch-owner.json", action.action_id))
    }

    fn claim_successor_launch(&self, action: &PublicFaultAction) -> Result<(String, bool)> {
        let path = self.launch_owner_path(action);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let nonce = format!("{}-successor", action.action_id);
        let current = PreviewLaunchOwner {
            nonce: nonce.clone(),
            supervisor_id: self.instance_id.clone(),
            pid: std::process::id(),
            start_time: process_start_time(std::process::id()).ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "cannot read launch-owner process identity".into(),
                )
            })?,
        };
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    file.write_all(
                        &serde_json::to_vec(&current)
                            .map_err(|error| crate::host::HostError::Corrupt(error.to_string()))?,
                    )?;
                    file.sync_all()?;
                    return Ok((nonce, true));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let owner: PreviewLaunchOwner = serde_json::from_slice(&std::fs::read(&path)?)
                        .map_err(|error| crate::host::HostError::Corrupt(error.to_string()))?;
                    if owner.nonce != nonce {
                        return Err(crate::host::HostError::DurableEffectConflict(
                            "successor launch nonce changed".into(),
                        ));
                    }
                    let identity = PreviewSupervisorIdentity {
                        pid: owner.pid,
                        start_time: owner.start_time.clone(),
                    };
                    if process_identity_is_alive(&identity) {
                        return Ok((nonce, owner.supervisor_id == current.supervisor_id));
                    }
                    std::fs::remove_file(&path)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(crate::host::HostError::CommandRejected(
            "cannot claim successor launch".into(),
        ))
    }

    fn read_successor_evidence(&self, action: &PublicFaultAction) -> Result<PreviewChildEvidence> {
        let bytes = std::fs::read(self.evidence_path(action))?;
        let evidence: PreviewChildEvidence = serde_json::from_slice(&bytes)
            .map_err(|error| crate::host::HostError::Corrupt(error.to_string()))?;
        self.validate_successor(action, &evidence)?;
        Ok(evidence)
    }

    async fn launch_successor(
        &self,
        action: &PublicFaultAction,
        launch_nonce: &str,
        may_spawn: bool,
    ) -> Result<PreviewChildEvidence> {
        let evidence_path = self.evidence_path(action);
        if evidence_path.exists() {
            let evidence = self.read_successor_evidence(action)?;
            if Path::new(&format!("/proc/{}", evidence.child_pid)).exists() {
                return Ok(evidence);
            }
            std::fs::remove_file(&evidence_path)?;
        }
        if self.active_child.lock().await.is_some() {
            return self.await_successor_evidence(action, launch_nonce).await;
        }
        if !may_spawn {
            return self.await_successor_evidence(action, launch_nonce).await;
        }
        if let Some(parent) = evidence_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let shutdown_path = self.shutdown_path(action);
        if shutdown_path.exists() {
            std::fs::remove_file(&shutdown_path)?;
        }
        let mut command = tokio::process::Command::new(&self.child.executable);
        command
            .args(&self.child.arguments)
            .envs(&self.child.environment)
            .env("KUBERIC_PREVIEW_CHILD_OUTPUT", &evidence_path)
            .env("KUBERIC_PREVIEW_DATA_ROOT", &self.data_root)
            .env("KUBERIC_PREVIEW_CHILD_MODE", "successor")
            .env("KUBERIC_PREVIEW_SHUTDOWN", &shutdown_path)
            .env("KUBERIC_PREVIEW_POD_UID", action.resources.pod_uid.as_str())
            .env("KUBERIC_PREVIEW_PVC_UID", action.resources.pvc_uid.as_str())
            .env("KUBERIC_PREVIEW_LAUNCH_NONCE", launch_nonce);
        let child = command.spawn()?;
        *self.active_child.lock().await = Some(child);
        self.await_successor_evidence(action, launch_nonce).await
    }

    async fn await_successor_evidence(
        &self,
        action: &PublicFaultAction,
        launch_nonce: &str,
    ) -> Result<PreviewChildEvidence> {
        let evidence_path = self.evidence_path(action);
        for _ in 0..500 {
            if evidence_path.exists() {
                let evidence = self.read_successor_evidence(action)?;
                if evidence.launch_nonce != launch_nonce {
                    return Err(crate::host::HostError::IdentityMismatch(
                        "successor evidence launch nonce changed".into(),
                    ));
                }
                return Ok(evidence);
            }
            let status = {
                let mut child = self.active_child.lock().await;
                match child.as_mut() {
                    Some(child) => child.try_wait()?,
                    None => None,
                }
            };
            if let Some(status) = status {
                self.active_child.lock().await.take();
                return Err(crate::host::HostError::CommandRejected(format!(
                    "successor child exited before readiness with {status}"
                )));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(crate::host::HostError::CommandRejected(
            "successor child readiness timed out".into(),
        ))
    }

    pub(crate) async fn shutdown_successor(&self, action: &PublicFaultAction) -> Result<()> {
        let shutdown_path = self.shutdown_path(action);
        if let Some(parent) = shutdown_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&shutdown_path, b"shutdown")?;
        if let Some(mut child) = self.active_child.lock().await.take() {
            let status = child.wait().await?;
            if !status.success() {
                return Err(crate::host::HostError::CommandRejected(format!(
                    "successor child shutdown failed with {status}"
                )));
            }
        } else if let Some(process_id) = self
            .store
            .restart_action()
            .await?
            .and_then(|record| record.successor_process_id)
        {
            for _ in 0..500 {
                if !Path::new(&format!("/proc/{process_id}")).exists() {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            return Err(crate::host::HostError::CommandRejected(
                "adopted successor did not stop".into(),
            ));
        }
        Ok(())
    }

    fn validate_successor(
        &self,
        action: &PublicFaultAction,
        evidence: &PreviewChildEvidence,
    ) -> Result<()> {
        if evidence.process_session.is_empty()
            || evidence.process_session == action.predecessor_session
            || evidence.child_pid == 0
            || evidence.data_root != self.data_root
            || evidence.pod_uid != action.resources.pod_uid
            || evidence.pvc_uid != action.resources.pvc_uid
            || evidence.provider_sentinel.is_empty()
            || evidence.application_instance_id.is_empty()
            || evidence.replicator_instance_id.is_empty()
            || evidence.callbacks
                != [
                    "replicator.open",
                    "replicator.change_role.none",
                    "application.change_role.none",
                ]
        {
            return Err(crate::host::HostError::IdentityMismatch(
                "successor child evidence changed storage, process, or construction identity"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[cfg(all(feature = "testing", kuberic_workspace_tests))]
fn process_start_time(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    fields.split_whitespace().nth(19).map(str::to_string)
}

#[cfg(all(feature = "testing", kuberic_workspace_tests))]
fn process_identity_is_alive(identity: &PreviewSupervisorIdentity) -> bool {
    process_start_time(identity.pid).as_deref() == Some(identity.start_time.as_str())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaDiagnostics {
    pub replica_id: i64,
    pub instance_id: String,
    pub agent_generation: String,
    pub process_session: String,
    pub role: String,
    pub epoch: String,
    pub previous_configuration: Option<String>,
    pub current_configuration: Option<String>,
    pub current_progress: i64,
    pub verified_replication_lsn: Option<i64>,
    pub committed_lsn: i64,
    pub read_status: String,
    pub write_status: String,
    pub catch_up_boundary_lsn: Option<i64>,
    pub catch_up_complete: bool,
    pub scale_up_operation: Option<String>,
    pub retired: bool,
    pub pending_operation: Option<String>,
    pub blocking: Option<String>,
    pub builds: Vec<ReplicaBuildDiagnostics>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaBuildDiagnostics {
    pub build_id: String,
    pub target_instance: String,
    pub replication_boundary_lsn: i64,
    pub durable_lsn: i64,
    pub completed: bool,
    pub catch_up_boundary_lsn: Option<i64>,
}

#[derive(Clone)]
pub struct ReplicaHandle {
    runtime: Arc<PodRuntime>,
    store: Arc<SqliteStore>,
    process_session: Arc<str>,
}

impl ReplicaHandle {
    pub async fn diagnostics(&self) -> Result<ReplicaDiagnostics> {
        let state = self.store.load_state().await?;
        let snapshot = self.runtime.snapshot().await;
        Ok(ReplicaDiagnostics {
            replica_id: state.identity.local_identity.replica_id.value(),
            instance_id: state.identity.local_identity.instance_id.to_string(),
            agent_generation: state.identity.local_identity.agent_generation.to_string(),
            process_session: self.process_session.to_string(),
            role: format!("{:?}", snapshot.role),
            epoch: format!(
                "{}.{}",
                state.highest_epoch.data_loss_number, state.highest_epoch.configuration_number
            ),
            previous_configuration: state
                .previous_configuration
                .map(|configuration| configuration.configuration_id.to_string()),
            current_configuration: state
                .current_configuration
                .map(|configuration| configuration.configuration_id.to_string()),
            current_progress: snapshot.current_progress,
            verified_replication_lsn: snapshot.verified_replication_lsn,
            committed_lsn: snapshot.committed_lsn,
            read_status: format!("{:?}", snapshot.read_status),
            write_status: format!("{:?}", snapshot.write_status),
            catch_up_boundary_lsn: snapshot.catch_up_boundary,
            catch_up_complete: snapshot.catch_up_complete,
            scale_up_operation: state
                .scale_up_evidence
                .as_deref()
                .map(|evidence| evidence.intent().operation_id.to_string()),
            retired: state.retired_authority.is_some() || snapshot.retired_authority.is_some(),
            pending_operation: state
                .reconfiguration
                .as_ref()
                .map(|record| record.command.operation_id.to_string())
                .or_else(|| {
                    state
                        .pending_effect
                        .as_ref()
                        .map(|pending| pending.effect.operation_id.to_string())
                }),
            blocking: state
                .reconfiguration
                .as_ref()
                .map(|record| {
                    format!(
                        "configuration:{:?}:{}",
                        record.stage, record.command.operation_id
                    )
                })
                .or_else(|| {
                    state.pending_effect.as_ref().map(|pending| {
                        format!("effect:{:?}:{}", pending.stage, pending.effect.operation_id)
                    })
                }),
            builds: snapshot
                .builds
                .into_iter()
                .map(|build| ReplicaBuildDiagnostics {
                    build_id: build.authority.build_id.to_string(),
                    target_instance: build.authority.target.instance_id.to_string(),
                    replication_boundary_lsn: build.authority.replication_boundary_lsn,
                    durable_lsn: build.durable_lsn,
                    completed: build.completed,
                    catch_up_boundary_lsn: build.catch_up_boundary_lsn,
                })
                .collect(),
        })
    }
}

pub struct RunningReplica {
    handle: ReplicaHandle,
    shutdown: watch::Sender<bool>,
    completion: tokio::task::JoinHandle<Result<()>>,
}

impl RunningReplica {
    pub fn handle(&self) -> ReplicaHandle {
        self.handle.clone()
    }

    pub fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    pub async fn wait(&mut self) -> Result<()> {
        (&mut self.completion)
            .await
            .map_err(|error| crate::host::HostError::CommandRejected(error.to_string()))?
    }
}

impl Drop for RunningReplica {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
    }
}

pub struct ReplicaHost<A, R> {
    config: ReplicaProcessConfig,
    application: Arc<A>,
    application_storage: ApplicationStorageState,
    application_storage_paths: Option<BTreeMap<String, PathBuf>>,
    resolver: Arc<R>,
}

impl<A, R> ReplicaHost<A, R>
where
    A: StatefulServiceReplica + 'static,
    R: ReplicaEndpointResolver + 'static,
{
    pub fn new(
        config: ReplicaProcessConfig,
        application: Arc<A>,
        application_storage: ApplicationStorageState,
        resolver: Arc<R>,
    ) -> Self {
        Self {
            config,
            application,
            application_storage,
            application_storage_paths: None,
            resolver,
        }
    }

    /// Bind application paths at authorized initialization. Only an unfinished
    /// first open receives `OpenMode::New`; missing bindings on existing stores reject.
    pub fn with_application_storage_paths(mut self, paths: BTreeMap<String, PathBuf>) -> Self {
        self.application_storage_paths = Some(paths);
        self
    }

    pub async fn start(self) -> Result<RunningReplica> {
        let (_shutdown, receiver) = watch::channel(false);
        self.start_with_shutdown(receiver)
            .await?
            .ok_or(crate::host::HostError::Runtime(
                crate::RuntimeError::OperationCancelled,
            ))
    }

    /// Cooperatively cancel startup and await its acknowledgement and task cleanup.
    /// `None` means cancellation completed before readiness. A readiness race may
    /// return a replica instead; the caller must shut it down and await `wait`.
    /// Keep this future alive until completion, including after requesting shutdown.
    pub async fn start_with_shutdown(
        self,
        mut startup_shutdown: watch::Receiver<bool>,
    ) -> Result<Option<RunningReplica>> {
        if *startup_shutdown.borrow() {
            return Ok(None);
        }
        if self.config.replica_id.value() <= 0 {
            return Err(crate::host::HostError::CommandRejected(
                "replica ID must be positive".into(),
            ));
        }
        if self.config.transport_window_capacity == 0 {
            return Err(crate::host::HostError::Backpressure(
                "transport window capacity must be positive".into(),
            ));
        }
        let observed = ObservedStorageIdentity {
            resource_uid: self.config.resource_uid.clone(),
            pod_uid: self.config.pod_uid.clone(),
            pvc_uid: self.config.pvc_uid.clone(),
            instance_id: ReplicaInstanceId::new(self.config.pod_uid.as_str()),
        };
        let paths = self
            .application_storage_paths
            .map(|paths| {
                paths
                    .into_iter()
                    .map(|(name, path)| resolve_storage_path(&path).map(|path| (name, path)))
                    .collect::<std::io::Result<BTreeMap<_, _>>>()
            })
            .transpose()?;
        let database_path = SqliteStore::metadata_database_path(&self.config.data_root);
        if !database_path.is_file() {
            serve_initialization(
                &self.config,
                observed.clone(),
                database_path.clone(),
                self.application_storage == ApplicationStorageState::FreshEmpty,
                paths.clone(),
                startup_shutdown.clone(),
            )
            .await?;
        }
        if *startup_shutdown.borrow() || startup_shutdown.has_changed().is_err() {
            return Ok(None);
        }

        let store = Arc::new(SqliteStore::open_existing(&database_path, None)?);
        let identity = store.identity().await?;
        validate_established_identity(&identity, &observed, self.config.replica_id)?;
        let state = store.load_state().await?;
        if state
            .application_storage
            .as_ref()
            .map(|binding| &binding.paths)
            != paths.as_ref()
        {
            store
                .record_partition_reports(state.load_metrics, Some(FaultType::Permanent))
                .await?;
            return Err(crate::host::HostError::InitializationNotAuthorized(
                "application storage paths differ from the authorized agent binding".into(),
            ));
        }
        let runtime = Arc::new(PodRuntime::new(
            identity.local_identity.clone(),
            self.application,
            store.clone(),
        ));
        let agent = AgentService::new(
            store.clone(),
            runtime.clone(),
            runtime.clone(),
            self.config.bearer_token.clone(),
        )?;
        let process_session: Arc<str> = Arc::from(agent.sessions().local_session().as_str());
        let sessions = agent.sessions().clone();
        let transport = Arc::new(Mutex::new(ReliableTransport::new(
            agent.sessions().local_session().clone(),
            self.config.transport_window_capacity,
        )?));
        let build_runtime = runtime.build_runtime();
        let outbound_runtime = Arc::new(runtime.outbound_runtime());
        let peer_runtime = Arc::new(runtime.peer_discovery_runtime());
        let dispatcher = Arc::new(GrpcOutboundDispatcher::new(
            build_runtime,
            transport.clone(),
            self.resolver,
            self.config.resource_uid.to_string(),
            self.config.bearer_token,
            self.config.rpc_deadline,
        )?);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (ready, mut ready_rx) = watch::channel(false);
        let mut agent_task = tokio::spawn(agent.serve(
            self.config.control_address,
            self.config.replication_address,
            ready,
            shutdown_rx.clone(),
        ));
        let mut outbound_task = tokio::spawn(run_outbound(
            outbound_runtime,
            transport.clone(),
            dispatcher.clone(),
            shutdown_rx.clone(),
        ));
        let mut peer_task = tokio::spawn(run_peer_discovery(
            identity.local_identity,
            peer_runtime,
            store.clone(),
            transport,
            dispatcher,
            sessions,
            shutdown_rx,
        ));
        let mut agent_finished = false;
        let mut outbound_finished = false;
        let mut peer_finished = false;
        let startup_result = tokio::select! {
            biased;
            result = &mut agent_task => {
                agent_finished = true;
                match result {
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(crate::host::HostError::CommandRejected(error.to_string())),
                    Ok(Ok(())) => Err(crate::host::HostError::CommandRejected(
                        "agent service stopped before becoming ready".into(),
                    )),
                }
            }
            result = &mut outbound_task => {
                outbound_finished = true;
                match result {
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(crate::host::HostError::CommandRejected(error.to_string())),
                    Ok(Ok(())) => Err(crate::host::HostError::CommandRejected(
                        "outbound progress stopped before agent readiness".into(),
                    )),
                }
            }
            result = &mut peer_task => {
                peer_finished = true;
                match result {
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(crate::host::HostError::CommandRejected(error.to_string())),
                    Ok(Ok(())) => Err(crate::host::HostError::CommandRejected(
                        "peer discovery stopped before agent readiness".into(),
                    )),
                }
            }
            result = async { ready_rx.wait_for(|ready| *ready).await.map(|_| ()) } => {
                match result {
                    Ok(_) => Ok(true),
                    Err(_) => {
                        agent_finished = true;
                        match (&mut agent_task).await {
                            Ok(Err(error)) => Err(error),
                            Err(error) => Err(crate::host::HostError::CommandRejected(error.to_string())),
                            Ok(Ok(())) => Err(crate::host::HostError::CommandRejected(
                                "agent readiness channel closed".into(),
                            )),
                        }
                    }
                }
            }
            _ = async { let _ = startup_shutdown.wait_for(|stopped| *stopped).await; } => Ok(false),
        };
        if !matches!(startup_result, Ok(true)) {
            shutdown.send_replace(true);
            let outbound = abort_progress(&mut outbound_task, outbound_finished).await;
            let peer = abort_progress(&mut peer_task, peer_finished).await;
            let cleanup = if agent_finished {
                Ok(())
            } else {
                finish_agent(&mut agent_task).await
            };
            runtime.abort();
            let cleanup =
                match cleanup {
                    Err(crate::host::HostError::Runtime(
                        crate::RuntimeError::OperationCancelled,
                    )) if matches!(startup_result, Ok(false)) => Ok(()),
                    result => result,
                };
            let result = with_shutdown_error(startup_result.map(|_| ()), cleanup, "agent shutdown");
            let result = with_shutdown_error(result, outbound, "outbound shutdown");
            with_shutdown_error(result, peer, "peer discovery shutdown")?;
            return Ok(None);
        }
        let supervisor_shutdown = shutdown.clone();
        let supervisor_runtime = runtime.clone();
        let completion = tokio::spawn(async move {
            let (first, finished) = tokio::select! {
                result = &mut agent_task => (result, 0),
                result = &mut outbound_task => (result, 1),
                result = &mut peer_task => (result, 2),
            };
            supervisor_shutdown.send_replace(true);
            let outbound = abort_progress(&mut outbound_task, finished == 1).await;
            let peer = abort_progress(&mut peer_task, finished == 2).await;
            // Joining the agent is the persistence acknowledgement. Aborting its
            // wrapper task can otherwise discard an accepted fault during shutdown.
            let cleanup = if finished == 0 {
                Ok(())
            } else {
                finish_agent(&mut agent_task).await
            };
            supervisor_runtime.abort();
            let first = first
                .map_err(|error| crate::host::HostError::CommandRejected(error.to_string()))
                .and_then(|result| result);
            let result = with_shutdown_error(cleanup, first, "replica task");
            let result = with_shutdown_error(result, outbound, "outbound shutdown");
            with_shutdown_error(result, peer, "peer discovery shutdown")
        });

        Ok(Some(RunningReplica {
            handle: ReplicaHandle {
                runtime,
                store,
                process_session,
            },
            shutdown,
            completion,
        }))
    }
}

async fn abort_progress(
    task: &mut tokio::task::JoinHandle<Result<()>>,
    finished: bool,
) -> Result<()> {
    if finished {
        return Ok(());
    }
    task.abort();
    match task.await {
        Ok(result) => result,
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => Err(crate::host::HostError::CommandRejected(error.to_string())),
    }
}

fn with_shutdown_error(primary: Result<()>, cleanup: Result<()>, context: &str) -> Result<()> {
    match (primary, cleanup) {
        (Err(primary), Err(cleanup)) => Err(crate::host::HostError::CommandRejected(format!(
            "{primary}; {context}: {cleanup}"
        ))),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn finish_agent(task: &mut tokio::task::JoinHandle<Result<()>>) -> Result<()> {
    match tokio::time::timeout(SHUTDOWN_TIMEOUT + Duration::from_secs(1), &mut *task).await {
        Ok(result) => {
            result.map_err(|error| crate::host::HostError::CommandRejected(error.to_string()))?
        }
        Err(_) => {
            task.abort();
            let _ = task.await;
            Err(crate::host::HostError::CommandRejected(
                "agent shutdown acknowledgement timed out".into(),
            ))
        }
    }
}

async fn serve_initialization(
    config: &ReplicaProcessConfig,
    observed: ObservedStorageIdentity,
    database_path: PathBuf,
    fresh_application_state: bool,
    application_storage_paths: Option<BTreeMap<String, PathBuf>>,
    mut startup_shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let (initialized, mut initialized_rx) = watch::channel(false);
    let service = InitializationService::new(
        observed,
        config.replica_id,
        database_path,
        config.bearer_token.clone(),
        initialized,
        fresh_application_state,
    )?
    .with_application_storage_paths(application_storage_paths);
    let (shutdown, shutdown_rx) = watch::channel(false);
    let (ready, _) = watch::channel(false);
    let stop = tokio::spawn(async move {
        tokio::select! {
            _ = async { let _ = initialized_rx.wait_for(|initialized| *initialized).await; } => {}
            _ = async { let _ = startup_shutdown.wait_for(|stopped| *stopped).await; } => {}
        }
        shutdown.send_replace(true);
    });
    let result = service
        .serve(config.control_address, ready, shutdown_rx)
        .await;
    stop.abort();
    let _ = stop.await;
    result
}

// Resolve existing ancestors without creating even empty application directories.
fn resolve_storage_path(path: &Path) -> std::io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let absolute = std::path::absolute(path)?;
            let parent = absolute.parent().ok_or(error)?;
            let name = absolute.file_name().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid application path")
            })?;
            Ok(resolve_storage_path(parent)?.join(name))
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    #[tokio::test]
    async fn cancelling_progress_joins_and_retains_completed_failures() {
        let mut pending = tokio::spawn(std::future::pending::<Result<()>>());
        abort_progress(&mut pending, false).await.unwrap();
        assert!(pending.is_finished());
        let mut failed = tokio::spawn(async {
            Err(crate::host::HostError::CommandRejected(
                "outbound failed".into(),
            ))
        });
        while !failed.is_finished() {
            tokio::task::yield_now().await;
        }
        let cleanup = abort_progress(&mut failed, false).await;
        let result = with_shutdown_error(
            Err(crate::host::HostError::CommandRejected(
                "startup failed".into(),
            )),
            cleanup,
            "outbound shutdown",
        );
        let error = result.unwrap_err().to_string();
        assert!(error.find("startup failed").unwrap() < error.find("outbound failed").unwrap());
    }

    #[tokio::test]
    async fn agent_shutdown_waits_for_acknowledgement_and_propagates_persistence_failure() {
        let (release, wait) = tokio::sync::oneshot::channel();
        let mut agent = tokio::spawn(async move {
            wait.await.unwrap();
            Err(crate::host::HostError::CommandRejected(
                "injected persistence failure".into(),
            ))
        });
        let completion = finish_agent(&mut agent);
        tokio::pin!(completion);
        tokio::select! {
            biased;
            result = &mut completion => panic!("returned before acknowledgement: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        release.send(()).unwrap();
        assert!(
            completion
                .await
                .unwrap_err()
                .to_string()
                .contains("injected persistence failure")
        );
    }

    #[tokio::test]
    async fn agent_shutdown_rejects_an_already_stopped_consumer_without_deadlock() {
        let mut agent = tokio::spawn(std::future::pending::<Result<()>>());
        agent.abort();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), finish_agent(&mut agent))
                .await
                .unwrap()
                .is_err()
        );
    }
}
