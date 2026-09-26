use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::{env, process::Command};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, stream};
use kuberic_agent::command::admit_configuration;
use kuberic_agent::coordinator::Coordinator;
use kuberic_agent::hosting::{OutboundReplication, PodRuntime};
use kuberic_agent::recovery::{RecoveryDecision, inspect_recovery, recover_pending};
use kuberic_agent::runtime_adapter::{RuntimeAdapter, RuntimeEffectExecutor};
use kuberic_agent::service::SessionRegistry;
use kuberic_agent::sqlite_store::SqliteStore;
use kuberic_agent::state::{
    AgentState, CoordinatorStage, EffectStage, SCHEMA_VERSION, StorageIdentity,
};
use kuberic_agent::store::{AgentStore, BeginConfiguration, BeginEffect};
use kuberic_agent::{AgentError, Result};
use kuberic_protocol::command::{EnsureConfiguration, PrepareSwitchover};
use kuberic_protocol::types::{
    AccessStatus, AgentGeneration, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy,
    Epoch, InitializationId, OperationId, PodUid, ProcessSessionId, ProvisioningIntent,
    ProvisioningPurpose, PvcUid, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole,
    ResourceUid, ScaleUpConfigurationEvidence, ScaleUpFailoverEvidence, ScaleUpIntent,
    ScaleUpProvisioning, ScaleUpStage, ScaleUpWitness, SwitchoverRequestId, TransitionKind,
};
use kuberic_runtime::application::{
    ClientWrite, CopyChunk, DurableApplicationAck, DurableApplicationProgress, OpenContext,
    Operation, OperationDataStream, RoleChange, StateProvider, StatefulServiceReplica,
};
use kuberic_runtime::engine::{DurableState, RetainedOperationStream};
use kuberic_runtime::replicator::copy::{BuildConfiguration, PrepareCopyRequest};
use kuberic_runtime::replicator::stream::{OperationMetadata, OperationStream};
use kuberic_runtime::replicator::{DefaultReplicatorFactory, Replicator, ReplicatorSettings};
use kuberic_runtime::{Result as RuntimeResult, RuntimeError};
use kuberic_runtime_internal::authority::{
    AdmittedAuthority, BuildAuthority, BuildAuthorityKind, BuildAuthorityStore, BuildProgressStore,
    ReplicaAuthorityStore, ReplicationProgress, ReplicationProgressStore,
};
use kuberic_runtime_internal::effects::{
    OpenMode, RuntimeEffect, RuntimeEffectAction, RuntimeEffectResult, RuntimePostcondition,
    RuntimeSnapshot,
};
use serde::{Deserialize, Serialize};
use tempfile::tempdir;

#[path = "support/removal_crashes.rs"]
mod removal_crashes;

struct FakeRuntime {
    calls: AtomicUsize,
    result: RuntimeEffectResult,
}

struct CancelledRuntime;

struct CrashAfterRealEffect {
    runtime: Arc<PodRuntime>,
    boundary: Option<String>,
    exit_code: i32,
}

struct ScaleUpProductionCutRuntime {
    runtime: Arc<PodRuntime>,
    candidate: Arc<PodRuntime>,
    acknowledgement_sent: std::sync::atomic::AtomicBool,
    write_label: String,
}

struct ScaleUpProductionCutStore {
    inner: Arc<SqliteStore>,
    cut: Option<String>,
    exit_code: i32,
}

struct ScaleUpFailoverRuntime {
    runtime: Arc<PodRuntime>,
    peers: Vec<Arc<PodRuntime>>,
    acknowledgement_sent: std::sync::atomic::AtomicBool,
    durable_operation: Operation,
}

impl ScaleUpProductionCutRuntime {
    async fn deliver_real_candidate_acknowledgement(&self) -> Result<()> {
        if self
            .acknowledgement_sent
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(());
        }
        eprintln!(
            "scale-up-production cut={} stage=ack-start",
            self.write_label
        );
        let source_progress = self.runtime.snapshot().await.current_progress;
        while self
            .candidate
            .snapshot()
            .await
            .verified_replication_lsn
            .unwrap_or_default()
            < source_progress
        {
            let outbound = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                self.runtime.data_plane().next_outbound(),
            )
            .await
            .expect("source did not emit retained production replication");
            let Some(OutboundReplication::Replication(item)) = outbound else {
                panic!(
                    "{}: expected retained replication for the configured candidate",
                    self.write_label
                );
            };
            let acknowledgement = self
                .candidate
                .data_plane()
                .receive_replication(item)
                .await?
                .applied()
                .await?;
            self.runtime
                .data_plane()
                .accept_acknowledgement(acknowledgement)
                .await?;
        }
        let pending = self
            .runtime
            .data_plane()
            .begin_write(ClientWrite {
                operation_id: OperationId::new(format!(
                    "scale-up-production-cut-write-{}",
                    self.write_label
                )),
                data: Bytes::from(format!(
                    "actual-candidate-runtime-acknowledgement-{}",
                    self.write_label
                )),
            })
            .await?;
        eprintln!(
            "scale-up-production cut={} stage=write-begun lsn={}",
            self.write_label, pending.lsn
        );
        assert_eq!(
            pending.replication_items.len(),
            1,
            "{}: expected the configured candidate to receive the production write",
            self.write_label
        );
        for item in pending.replication_items.clone() {
            let acknowledgement = self
                .candidate
                .data_plane()
                .receive_replication(item)
                .await?
                .applied()
                .await?;
            self.runtime
                .data_plane()
                .accept_acknowledgement(acknowledgement)
                .await?;
        }
        eprintln!(
            "scale-up-production cut={} stage=ack-accepted",
            self.write_label
        );
        pending.committed().await?;
        eprintln!(
            "scale-up-production cut={} stage=write-committed",
            self.write_label
        );
        Ok(())
    }
}

#[async_trait]
impl RuntimeEffectExecutor for ScaleUpProductionCutRuntime {
    async fn apply_runtime_effect(&self, effect: RuntimeEffect) -> Result<RuntimeEffectResult> {
        let grants_access = matches!(
            effect.action,
            RuntimeEffectAction::SetAccessStatus {
                read: AccessStatus::Granted,
                write: AccessStatus::Granted,
            }
        );
        match &effect.action {
            RuntimeEffectAction::AdmitAuthority(_) | RuntimeEffectAction::RetireBuild(_) => {}
            RuntimeEffectAction::SetAccessStatus {
                read: AccessStatus::Granted,
                write: AccessStatus::Granted,
            } if self.runtime.snapshot().await.write_status == AccessStatus::Granted => {
                self.deliver_real_candidate_acknowledgement().await?
            }
            RuntimeEffectAction::SetAccessStatus { .. } => {}
            RuntimeEffectAction::SetReadStatus(_) | RuntimeEffectAction::SetWriteStatus(_) => {
                panic!(
                    "{}: same-primary scale-up entered the ordinary write-closing path",
                    self.write_label
                )
            }
            RuntimeEffectAction::ChangeReplicatorRole(_)
            | RuntimeEffectAction::UpdateEpoch
            | RuntimeEffectAction::ChangeApplicationRole(_)
            | RuntimeEffectAction::WaitForCatchup
            | RuntimeEffectAction::RefreshApplicationProgress => {
                panic!(
                    "{}: same-primary scale-up entered the ordinary role/catch-up path",
                    self.write_label
                )
            }
            _ => {}
        }
        let result = self.runtime.apply_effect(effect).await?;
        if grants_access {
            self.deliver_real_candidate_acknowledgement().await?;
        }
        Ok(result)
    }
}

#[async_trait]
impl RuntimeEffectExecutor for ScaleUpFailoverRuntime {
    async fn apply_runtime_effect(&self, effect: RuntimeEffect) -> Result<RuntimeEffectResult> {
        if matches!(effect.action, RuntimeEffectAction::WaitForCatchup)
            && !self
                .acknowledgement_sent
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            eprintln!("scale-up-failover stage=peer-ack-start");
            let authority = self
                .runtime
                .snapshot()
                .await
                .authority
                .expect("failover authority before catch-up");
            for peer in &self.peers {
                let item = kuberic_wire::proto::ReplicationItem {
                    protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                    sender: Some(authority.local_identity.clone().into()),
                    receiver: Some(peer.snapshot().await.identity.into()),
                    epoch: Some(authority.current_configuration.epoch.into()),
                    previous_configuration_id: authority
                        .previous_configuration
                        .as_ref()
                        .map_or_else(String::new, |previous| {
                            previous.configuration_id.to_string()
                        }),
                    current_configuration_id: authority
                        .current_configuration
                        .configuration_id
                        .to_string(),
                    lsn: self.durable_operation.lsn,
                    committed_lsn: self.durable_operation.committed_lsn,
                    data: self.durable_operation.data.to_vec(),
                    ..Default::default()
                };
                let acknowledgement = peer
                    .data_plane()
                    .receive_replication(item)
                    .await?
                    .applied()
                    .await?;
                self.runtime
                    .data_plane()
                    .accept_acknowledgement(acknowledgement)
                    .await?;
            }
            eprintln!("scale-up-failover stage=peer-ack-accepted");
        }
        Ok(self.runtime.apply_effect(effect).await?)
    }
}

fn scale_up_runtime_effect_boundary(action: &RuntimeEffectAction) -> Option<&'static str> {
    match action {
        RuntimeEffectAction::AdmitAuthority(_) => Some("authority"),
        RuntimeEffectAction::SetAccessStatus {
            read: AccessStatus::Granted,
            write: AccessStatus::Granted,
        } => Some("access"),
        RuntimeEffectAction::RetireBuild(_) => Some("build-retirement"),
        _ => None,
    }
}

#[async_trait]
impl AgentStore for ScaleUpProductionCutStore {
    async fn identity(&self) -> Result<StorageIdentity> {
        self.inner.identity().await
    }

    async fn load_state(&self) -> Result<AgentState> {
        self.inner.load_state().await
    }

    async fn begin_effect(&self, effect: &RuntimeEffect) -> Result<BeginEffect> {
        if scale_up_runtime_effect_boundary(&effect.action).is_some_and(|boundary| {
            self.cut.as_deref() == Some(format!("{boundary}-before").as_str())
        }) {
            std::process::exit(self.exit_code);
        }
        self.inner.begin_effect(effect).await
    }

    async fn mark_effect_applied(&self, effect: &RuntimeEffect) -> Result<()> {
        self.inner.mark_effect_applied(effect).await
    }

    async fn complete_effect(&self, result: &RuntimeEffectResult) -> Result<()> {
        let boundary = self
            .inner
            .load_state()
            .await?
            .pending_effect
            .as_ref()
            .and_then(|pending| scale_up_runtime_effect_boundary(&pending.effect.action));
        self.inner.complete_effect(result).await?;
        if boundary.is_some_and(|boundary| {
            self.cut.as_deref() == Some(format!("{boundary}-after").as_str())
        }) {
            std::process::exit(self.exit_code);
        }
        Ok(())
    }

    async fn cancel_effect(&self, effect: &RuntimeEffect) -> Result<()> {
        self.inner.cancel_effect(effect).await
    }

    async fn begin_configuration(
        &self,
        command: &EnsureConfiguration,
    ) -> Result<BeginConfiguration> {
        self.inner.begin_configuration(command).await
    }

    async fn journal_build(
        &self,
        command: &kuberic_protocol::command::EnsureReplicaBuild,
    ) -> Result<kuberic_protocol::command::EnsureReplicaBuild> {
        self.inner.journal_build(command).await
    }

    async fn advance_configuration(
        &self,
        operation_id: &OperationId,
        expected: CoordinatorStage,
        next: CoordinatorStage,
        observed_lsn: Option<i64>,
    ) -> Result<kuberic_agent::state::ReconfigurationRecord> {
        self.inner
            .advance_configuration(operation_id, expected, next, observed_lsn)
            .await
    }

    async fn complete_configuration(
        &self,
        operation_id: &OperationId,
    ) -> Result<kuberic_agent::state::RetainedCommandResult> {
        if self.cut.as_deref() == Some("completion-before") {
            std::process::exit(self.exit_code);
        }
        let completed = self.inner.complete_configuration(operation_id).await?;
        if self.cut.as_deref() == Some("completion-after") {
            std::process::exit(self.exit_code);
        }
        Ok(completed)
    }

    async fn retained_result(&self) -> Result<Option<kuberic_agent::state::RetainedResult>> {
        self.inner.retained_result().await
    }

    async fn set_reconfiguration(&self, data: Option<String>) -> Result<()> {
        self.inner.set_reconfiguration(data).await
    }

    async fn clear_reconfiguration(&self) -> Result<()> {
        self.inner.clear_reconfiguration().await
    }

    async fn migrate_schema(&self, expected_version: u32, target_version: u32) -> Result<()> {
        self.inner
            .migrate_schema(expected_version, target_version)
            .await
    }

    async fn record_partition_reports(
        &self,
        load_metrics: Vec<kuberic_protocol::types::LoadMetric>,
        reported_fault: Option<kuberic_protocol::types::FaultType>,
    ) -> Result<()> {
        self.inner
            .record_partition_reports(load_metrics, reported_fault)
            .await
    }
}

#[async_trait]
impl RuntimeEffectExecutor for CrashAfterRealEffect {
    async fn apply_runtime_effect(&self, effect: RuntimeEffect) -> Result<RuntimeEffectResult> {
        let boundary = match &effect.action {
            RuntimeEffectAction::AdmitAuthority(_) => "authority",
            RuntimeEffectAction::ChangeRole(ReplicaRole::ActiveSecondary) => {
                "active-secondary-role"
            }
            RuntimeEffectAction::ChangeApplicationRole(_) => "application-role",
            RuntimeEffectAction::SetAccessStatus { .. } => "access",
            RuntimeEffectAction::RetireBuild(_) => "build-retirement",
            _ => "",
        };
        let before_boundary = format!("before-{boundary}");
        let after_boundary = format!("after-{boundary}");
        if self.boundary.as_deref() == Some(before_boundary.as_str()) {
            std::process::exit(self.exit_code);
        }
        if matches!(effect.action, RuntimeEffectAction::WaitForCatchup) {
            let snapshot = self.runtime.snapshot().await;
            let authority = snapshot.authority.unwrap();
            let receiver = authority
                .current_configuration
                .members
                .iter()
                .find(|member| member.identity != authority.local_identity)
                .unwrap();
            self.runtime
                .data_plane()
                .accept_acknowledgement(kuberic_wire::proto::ReplicationAck {
                    protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                    sender: Some(authority.primary_identity().clone().into()),
                    receiver: Some(receiver.identity.clone().into()),
                    epoch: Some(authority.current_configuration.epoch.into()),
                    previous_configuration_id: authority
                        .previous_configuration
                        .as_ref()
                        .map_or_else(String::new, |cc| cc.configuration_id.to_string()),
                    current_configuration_id: authority
                        .current_configuration
                        .configuration_id
                        .to_string(),
                    received_lsn: 7,
                    applied_lsn: 7,
                    committed_lsn: 7,
                    ..Default::default()
                })
                .await?;
        }
        let result = self.runtime.apply_effect(effect).await?;
        if self.boundary.as_deref() == Some(boundary)
            || self.boundary.as_deref() == Some(after_boundary.as_str())
        {
            // Exit before RuntimeAdapter records the result or advances the stage.
            std::process::exit(self.exit_code);
        }
        Ok(result)
    }
}

fn real_handoff_fixture(scenario: &str) -> (AgentState, EnsureConfiguration) {
    let fixture = match scenario {
        "target-current-only" => "retirement-after",
        "compensation-promotion" => "compensation-admission",
        other => other,
    };
    let (mut state, mut command) = switchover_recovery_fixture(fixture);
    if scenario == "target-current-only" {
        let target = switchover_prepare_command().target;
        state.identity.local_identity = target.clone();
        state.role = ReplicaRole::Primary;
        state.prepared_switchover = None;
        command.local_replica_id = target.replica_id;
        command.expected_instance_id = target.instance_id;
        command.expected_agent_generation = target.agent_generation;
        command.retire_switchover_preparation_ids.clear();
    }
    (state, command)
}

struct SwitchoverRecoveryRuntime {
    state: Mutex<RuntimePostcondition>,
    crash_after: Option<&'static str>,
}

impl SwitchoverRecoveryRuntime {
    fn new(state: &AgentState, crash_after: Option<&'static str>) -> Self {
        let mut snapshot = result().postcondition;
        snapshot.open = true;
        snapshot.role = state.role;
        snapshot.read_status = AccessStatus::ReconfigurationPending;
        snapshot.write_status = AccessStatus::ReconfigurationPending;
        snapshot.current_progress = 7;
        snapshot.verified_replication_lsn = Some(7);
        snapshot.committed_lsn = 7;
        snapshot.current_configuration_quorum_progress = 7;
        snapshot.catch_up_complete = true;
        snapshot.authority = Some(AdmittedAuthority {
            scale_up: None,
            secondary_removal: None,
            local_identity: state.identity.local_identity.clone(),
            transition_kind: state
                .previous_configuration
                .as_ref()
                .map(|_| TransitionKind::PlannedSwitchover),
            previous_configuration: state.previous_configuration.clone(),
            current_configuration: state.current_configuration.clone().unwrap(),
            switchover_handoff: (state.highest_epoch.configuration_number > 1)
                .then(|| {
                    state
                        .prepared_switchover
                        .clone()
                        .or(state.retired_switchover.clone())
                })
                .flatten(),
        });
        Self {
            state: Mutex::new(snapshot),
            crash_after,
        }
    }
}

#[async_trait]
impl RuntimeEffectExecutor for SwitchoverRecoveryRuntime {
    async fn apply_runtime_effect(&self, effect: RuntimeEffect) -> Result<RuntimeEffectResult> {
        let mut state = self.state.lock().unwrap();
        let stage = match effect.action {
            RuntimeEffectAction::AdmitAuthority(authority) => {
                state.authority = Some(*authority);
                "admission"
            }
            RuntimeEffectAction::ChangeApplicationRole(role) => {
                state.role = role;
                "role"
            }
            RuntimeEffectAction::SetReadStatus(read) => {
                state.read_status = read;
                "read"
            }
            RuntimeEffectAction::SetWriteStatus(write) => {
                state.write_status = write;
                "write"
            }
            RuntimeEffectAction::SetAccessStatus { read, write } => {
                state.read_status = read;
                state.write_status = write;
                "access"
            }
            RuntimeEffectAction::RefreshApplicationProgress
            | RuntimeEffectAction::ChangeReplicatorRole(_)
            | RuntimeEffectAction::UpdateEpoch
            | RuntimeEffectAction::WaitForCatchup => "other",
            action => panic!("unexpected recovery effect {action:?}"),
        };
        if self.crash_after == Some(stage) {
            std::process::exit(0);
        }
        Ok(RuntimeEffectResult {
            operation_id: effect.operation_id,
            sequence: effect.sequence,
            postcondition: state.clone(),
        })
    }
}

fn switchover_recovery_fixture(boundary: &str) -> (AgentState, EnsureConfiguration) {
    let starting = switchover_configuration();
    let prepare = switchover_prepare_command();
    let requested = ConfigurationDescriptor::new(
        Epoch::new(0, 2),
        prepare.target.replica_id,
        starting
            .members
            .iter()
            .map(|member| ConfigurationMember {
                identity: member.identity.clone(),
                role: if member.identity == prepare.target {
                    ReplicaRole::Primary
                } else {
                    ReplicaRole::ActiveSecondary
                },
            })
            .collect(),
        starting.write_quorum,
    );
    let handoff = kuberic_protocol::types::SwitchoverHandoff {
        preparation_generation: prepare.preparation_generation,
        preparation_operation_id: prepare.operation_id,
        request_id: prepare.request_id,
        source: prepare.source.clone(),
        target: prepare.target.clone(),
        starting_configuration_id: starting.configuration_id.clone(),
        starting_epoch: starting.epoch,
        handoff_lsn: 7,
    };
    let local = if boundary == "promotion" {
        prepare.target
    } else {
        prepare.source
    };
    let compensation = boundary.starts_with("compensation");
    let restoring = boundary.starts_with("restoration");
    let current_only = boundary.starts_with("retirement") || boundary == "compensation-completion";
    let current = if compensation {
        ConfigurationDescriptor::new(
            Epoch::new(0, 3),
            starting.primary_id,
            starting.members.clone(),
            starting.write_quorum,
        )
    } else if restoring {
        starting.clone()
    } else {
        requested.clone()
    };
    let previous = if compensation {
        requested.clone()
    } else {
        starting.clone()
    };
    let mut state = AgentState::new(StorageIdentity {
        local_identity: local.clone(),
        ..switchover_storage_identity()
    });
    state.current_configuration = Some(if current_only {
        current.clone()
    } else {
        previous.clone()
    });
    state.previous_configuration = current_only.then(|| previous.clone());
    state.highest_epoch = state.current_configuration.as_ref().unwrap().epoch;
    state.role = state
        .current_configuration
        .as_ref()
        .unwrap()
        .members
        .iter()
        .find(|member| member.identity == local)
        .unwrap()
        .role;
    state.write_status = AccessStatus::ReconfigurationPending;
    state.prepared_switchover =
        (local == handoff.source && !boundary.contains("unobserved")).then(|| handoff.clone());
    let command = EnsureConfiguration {
        previous_policy: None,
        secondary_removal_evidence: None,
        scale_up_evidence: None,
        operation_id: OperationId::new(format!("recover-{boundary}")),
        previous_configuration: (!current_only && !restoring).then(|| previous.clone()),
        previous_epoch: (!current_only && !restoring).then_some(previous.epoch),
        current_epoch: current.epoch,
        current_configuration: current,
        effective_policy: state.identity.effective_policy.clone(),
        local_replica_id: local.replica_id,
        expected_instance_id: local.instance_id,
        expected_agent_generation: local.agent_generation,
        transition_kind: TransitionKind::PlannedSwitchover,
        failover_safe_lsn: None,
        primary_write_status: AccessStatus::ReconfigurationPending,
        current_only,
        retire_build_ids: Vec::new(),
        retire_switchover_preparation_ids: if current_only || restoring {
            vec![handoff.preparation()]
        } else {
            Vec::new()
        },
        switchover_handoff: (!boundary.contains("unobserved")).then_some(handoff),
    };
    (state, command)
}

#[async_trait]
impl RuntimeEffectExecutor for CancelledRuntime {
    async fn apply_runtime_effect(&self, _effect: RuntimeEffect) -> Result<RuntimeEffectResult> {
        Err(RuntimeError::ReplicaRemoved(2).into())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CrashPersistedState {
    applied_lsn: i64,
    committed_lsn: i64,
    operations: BTreeMap<i64, Vec<u8>>,
    #[serde(default)]
    close_completions: u64,
    #[serde(default)]
    last_role: Option<ReplicaRole>,
}

struct CrashState {
    path: PathBuf,
    state: Mutex<CrashPersistedState>,
    opens: AtomicUsize,
    consume_replication: bool,
}

async fn consume_crash_stream(
    application: std::sync::Weak<CrashState>,
    mut stream: OperationStream,
) {
    while let Some(operation) = stream.get_operation().await.unwrap() {
        let Some(application) = application.upgrade() else {
            return;
        };
        let result = match &operation.metadata {
            OperationMetadata::Replication { lsn, committed_lsn } => {
                application
                    .apply(Operation {
                        lsn: *lsn,
                        committed_lsn: *committed_lsn,
                        data: operation.data.clone(),
                    })
                    .await
            }
            OperationMetadata::Copy { build_id, sequence } => {
                match application
                    .apply_copy_chunk(
                        build_id,
                        *sequence,
                        CopyChunk {
                            data: operation.data.clone(),
                        },
                    )
                    .await
                {
                    Ok(()) => application.durable_progress().await,
                    Err(error) => Err(error),
                }
            }
            OperationMetadata::CopyComplete {
                build_id,
                up_to_lsn,
                committed_lsn,
            } => {
                application
                    .finish_copy(build_id, *up_to_lsn, *committed_lsn)
                    .await
            }
        };
        match result {
            Ok(progress) => {
                let _ = operation.acknowledge(progress);
            }
            Err(error) => {
                let _ = operation.reject(error);
            }
        }
    }
}

impl CrashState {
    fn open(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let state = if path.is_file() {
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
        } else {
            CrashPersistedState::default()
        };
        Self {
            path,
            state: Mutex::new(state),
            opens: AtomicUsize::new(0),
            consume_replication: false,
        }
    }

    fn open_with_replication(path: impl AsRef<Path>) -> Self {
        let mut application = Self::open(path);
        application.consume_replication = true;
        application
    }

    fn persist(&self, state: &CrashPersistedState) -> RuntimeResult<()> {
        let temporary = self.path.with_extension("tmp");
        let bytes = serde_json::to_vec(state)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        std::fs::write(&temporary, bytes)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        std::fs::File::open(&temporary)
            .and_then(|file| file.sync_all())
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        std::fs::rename(&temporary, &self.path)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        std::fs::File::open(self.path.parent().expect("application state parent"))
            .and_then(|directory| directory.sync_all())
            .map_err(|error| RuntimeError::Application(error.to_string()))
    }

    fn progress(state: &CrashPersistedState) -> DurableApplicationProgress {
        DurableApplicationProgress {
            applied_lsn: state.applied_lsn,
            committed_lsn: state.committed_lsn,
        }
    }
}

#[async_trait]
impl StatefulServiceReplica for CrashState {
    async fn open(self: Arc<Self>, context: OpenContext) -> RuntimeResult<Arc<dyn Replicator>> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let partition = context
            .partition
            .with_factory(Arc::new(DefaultReplicatorFactory::new(self.clone())));
        let interfaces = partition
            .create_replicator(self.clone(), Some(ReplicatorSettings::default()))
            .await?;
        let copy = interfaces.state_replicator().get_copy_stream().await?;
        tokio::spawn(consume_crash_stream(Arc::downgrade(&self), copy));
        if self.consume_replication {
            let stream = interfaces
                .state_replicator()
                .get_replication_stream()
                .await?;
            tokio::spawn(consume_crash_stream(Arc::downgrade(&self), stream));
        }
        Ok(interfaces.replicator())
    }

    async fn change_role(&self, role: ReplicaRole) -> RuntimeResult<RoleChange> {
        let mut state = self.state.lock().unwrap();
        let mut candidate = state.clone();
        candidate.last_role = Some(role);
        self.persist(&candidate)?;
        *state = candidate;
        Ok(RoleChange {
            service_address: None,
        })
    }

    async fn close(&self) -> RuntimeResult<()> {
        if matches!(
            env::var("KUBERIC_REMOVAL_BOUNDARY").as_deref(),
            Ok("retire:application-close" | "retire:role-none-before-close")
        ) {
            assert_eq!(
                self.state.lock().unwrap().last_role,
                Some(ReplicaRole::None)
            );
            std::process::exit(73);
        }
        let mut state = self.state.lock().unwrap();
        let mut candidate = state.clone();
        candidate.close_completions += 1;
        self.persist(&candidate)?;
        *state = candidate;
        Ok(())
    }

    fn abort(&self) {}
}

#[async_trait]
impl StateProvider for CrashState {
    async fn update_epoch(
        &self,
        _epoch: Epoch,
        _previous_epoch_last_lsn: i64,
    ) -> RuntimeResult<()> {
        Ok(())
    }

    async fn last_committed_lsn(&self) -> RuntimeResult<i64> {
        Ok(self.state.lock().unwrap().committed_lsn)
    }

    async fn get_copy_context(&self) -> RuntimeResult<OperationDataStream> {
        Ok(Box::pin(stream::empty()))
    }

    async fn get_copy_state(
        &self,
        up_to_lsn: i64,
        mut copy_context: OperationDataStream,
    ) -> RuntimeResult<OperationDataStream> {
        if copy_context.next().await.is_some() {
            return Err(RuntimeError::Application(
                "crash fixture does not use copy context".into(),
            ));
        }
        let chunks = self
            .state
            .lock()
            .unwrap()
            .operations
            .range(..=up_to_lsn)
            .map(|(lsn, data)| {
                serde_json::to_vec(&(*lsn, data))
                    .map(Bytes::from)
                    .map_err(|error| RuntimeError::Application(error.to_string()))
            })
            .collect::<Vec<_>>();
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn on_data_loss(&self) -> RuntimeResult<bool> {
        Ok(false)
    }
}

#[async_trait]
impl DurableState for CrashState {
    async fn get_replication_operations(
        &self,
        from_lsn: i64,
        to_lsn: i64,
    ) -> RuntimeResult<RetainedOperationStream> {
        let operations = self
            .state
            .lock()
            .unwrap()
            .operations
            .range(from_lsn..=to_lsn)
            .map(|(lsn, data)| {
                Ok(Operation {
                    lsn: *lsn,
                    committed_lsn: *lsn,
                    data: Bytes::copy_from_slice(data),
                })
            })
            .collect::<Vec<_>>();
        Ok(Box::pin(stream::iter(operations)))
    }

    async fn apply_copy_chunk(
        &self,
        _build_id: &OperationId,
        _sequence: u64,
        chunk: CopyChunk,
    ) -> RuntimeResult<()> {
        let (lsn, data): (i64, Vec<u8>) = serde_json::from_slice(&chunk.data)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let mut state = self.state.lock().unwrap();
        if let Some(existing) = state.operations.get(&lsn)
            && existing != &data
        {
            return Err(RuntimeError::AuthorityMismatch(
                "copy chunk disagrees with existing durable bytes".into(),
            ));
        }
        let mut candidate = state.clone();
        candidate.operations.insert(lsn, data);
        candidate.applied_lsn = candidate.applied_lsn.max(lsn);
        self.persist(&candidate)?;
        *state = candidate;
        Ok(())
    }

    async fn verify_copy_chunk(
        &self,
        _build_id: &OperationId,
        _sequence: u64,
        chunk: &CopyChunk,
    ) -> RuntimeResult<bool> {
        let (lsn, data): (i64, Vec<u8>) = serde_json::from_slice(&chunk.data)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        Ok(self.state.lock().unwrap().operations.get(&lsn) == Some(&data))
    }

    async fn finish_copy(
        &self,
        _build_id: &OperationId,
        up_to_lsn: i64,
        committed_lsn: i64,
    ) -> RuntimeResult<DurableApplicationProgress> {
        let progress = DurableApplicationProgress {
            applied_lsn: up_to_lsn,
            committed_lsn,
        };
        let mut state = self.state.lock().unwrap();
        let mut candidate = state.clone();
        candidate.applied_lsn = up_to_lsn;
        candidate.committed_lsn = committed_lsn;
        self.persist(&candidate)?;
        *state = candidate;
        Ok(progress)
    }

    async fn apply(&self, operation: Operation) -> RuntimeResult<DurableApplicationAck> {
        let mut state = self.state.lock().unwrap();
        if let Some(existing) = state.operations.get(&operation.lsn) {
            if existing.as_slice() != operation.data.as_ref() {
                return Err(RuntimeError::AuthorityMismatch(
                    "crash fixture LSN reused with different data".into(),
                ));
            }
            return Ok(Self::progress(&state));
        }
        let mut candidate = state.clone();
        candidate
            .operations
            .insert(operation.lsn, operation.data.to_vec());
        candidate.applied_lsn = candidate.applied_lsn.max(operation.lsn);
        candidate.committed_lsn = candidate.committed_lsn.max(operation.committed_lsn);
        self.persist(&candidate)?;
        *state = candidate;
        Ok(Self::progress(&state))
    }

    async fn durable_progress(&self) -> RuntimeResult<DurableApplicationProgress> {
        Ok(Self::progress(&self.state.lock().unwrap()))
    }

    async fn verify_applied(&self, operation: &Operation) -> RuntimeResult<bool> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .operations
            .get(&operation.lsn)
            .is_some_and(|data| data.as_slice() == operation.data.as_ref()))
    }

    async fn commit(&self, committed_lsn: i64) -> RuntimeResult<DurableApplicationProgress> {
        let mut state = self.state.lock().unwrap();
        if committed_lsn > state.applied_lsn {
            return Err(RuntimeError::Application(
                "cannot commit beyond applied progress".into(),
            ));
        }
        let mut candidate = state.clone();
        candidate.committed_lsn = candidate.committed_lsn.max(committed_lsn);
        self.persist(&candidate)?;
        *state = candidate;
        if committed_lsn == 1
            && env::var("KUBERIC_LOCAL_RECOVERY_BOUNDARY").as_deref() == Ok("application-commit")
        {
            std::process::exit(73);
        }
        Ok(Self::progress(&state))
    }
}

#[async_trait]
impl RuntimeEffectExecutor for FakeRuntime {
    async fn apply_runtime_effect(&self, _effect: RuntimeEffect) -> Result<RuntimeEffectResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.result.clone())
    }
}

fn storage_identity() -> StorageIdentity {
    StorageIdentity {
        schema_version: SCHEMA_VERSION,
        resource_uid: ResourceUid::new("resource-1"),
        pod_uid: PodUid::new("pod-1"),
        pvc_uid: PvcUid::new("pvc-1"),
        initialization_id: InitializationId::new("init-1"),
        local_identity: ReplicaIdentity {
            replica_id: ReplicaId::new(1),
            instance_id: ReplicaInstanceId::new("instance-1"),
            agent_generation: AgentGeneration::new("generation-1"),
        },
        effective_policy: EffectivePolicy::fixed(3, 30).unwrap(),
    }
}

fn effect() -> RuntimeEffect {
    RuntimeEffect {
        operation_id: OperationId::new("effect-1"),
        sequence: 1,
        action: RuntimeEffectAction::Open(OpenMode::Existing),
    }
}

fn configuration_command() -> EnsureConfiguration {
    let local = storage_identity().local_identity;
    let effective_policy = EffectivePolicy::fixed(1, 30).unwrap();
    let current_configuration = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        local.replica_id,
        vec![ConfigurationMember {
            identity: local.clone(),
            role: ReplicaRole::Primary,
        }],
        effective_policy.write_quorum,
    );
    EnsureConfiguration {
        previous_policy: None,
        secondary_removal_evidence: None,
        scale_up_evidence: None,
        operation_id: OperationId::new("configuration-1"),
        previous_configuration: None,
        current_configuration,
        previous_epoch: None,
        current_epoch: Epoch::new(0, 1),
        effective_policy,
        local_replica_id: local.replica_id,
        expected_instance_id: local.instance_id,
        expected_agent_generation: local.agent_generation,
        transition_kind: TransitionKind::Bootstrap,
        failover_safe_lsn: None,
        primary_write_status: AccessStatus::ReconfigurationPending,
        current_only: false,
        retire_build_ids: Vec::new(),
        switchover_handoff: None,
        retire_switchover_preparation_ids: Vec::new(),
    }
}

fn single_storage_identity() -> StorageIdentity {
    StorageIdentity {
        effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
        ..storage_identity()
    }
}

fn scale_up_crash_fixture(current_only: bool) -> (AgentState, EnsureConfiguration, BuildAuthority) {
    let primary = single_storage_identity().local_identity;
    let resource_uid = ResourceUid::new("resource-1");
    let previous_policy = EffectivePolicy::fixed(1, 30).unwrap();
    let current_policy = EffectivePolicy::fixed(2, 30).unwrap();
    let previous = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        primary.replica_id,
        vec![ConfigurationMember {
            identity: primary.clone(),
            role: ReplicaRole::Primary,
        }],
        previous_policy.write_quorum,
    );
    let mut provisioning = ProvisioningIntent {
        purpose: ProvisioningPurpose::scale_up(ScaleUpProvisioning {
            resource_uid: resource_uid.clone(),
            spec_generation: 2,
            desired_replicas: 2,
            previous_configuration: previous.clone(),
            previous_policy: previous_policy.clone(),
            current_policy: current_policy.clone(),
            target_replica_id: ReplicaId::new(2),
        }),
        pod_uid: PodUid::new("scale-up-candidate"),
        pvc_uid: PvcUid::new("scale-up-cut-candidate-pvc"),
        operation_id: OperationId::default(),
    };
    provisioning.operation_id = provisioning.expected_operation_id();
    let candidate = provisioning.target_identity(&resource_uid);
    let current = ConfigurationDescriptor::new(
        Epoch::new(0, 2),
        primary.replica_id,
        vec![
            ConfigurationMember {
                identity: primary.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: candidate.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        current_policy.write_quorum,
    );
    let build = BuildAuthority {
        build_id: provisioning.scale_up_build_id(&resource_uid).unwrap(),
        kind: BuildAuthorityKind::Provisioning,
        source: primary.clone(),
        target: candidate.clone(),
        current_configuration: previous.clone(),
        replication_boundary_lsn: 1,
    };
    let mut intent = ScaleUpIntent {
        operation_id: OperationId::default(),
        resource_uid,
        spec_generation: 2,
        desired_replicas: 2,
        previous_configuration: previous.clone(),
        current_configuration: current.clone(),
        previous_policy: previous_policy.clone(),
        current_policy: current_policy.clone(),
        primary: primary.clone(),
        target: candidate.clone(),
        build_id: build.build_id.clone(),
        snapshot_boundary_lsn: 1,
        catch_up_boundary_lsn: 1,
    };
    intent.operation_id = intent.expected_operation_id();
    let evidence = ScaleUpConfigurationEvidence::Admission {
        intent: intent.clone(),
    };
    let mut state = AgentState::new(StorageIdentity {
        effective_policy: previous_policy.clone(),
        ..single_storage_identity()
    });
    state.admitted_policy = Some(if current_only {
        current_policy.clone()
    } else {
        previous_policy.clone()
    });
    state.current_configuration = Some(if current_only {
        current.clone()
    } else {
        previous.clone()
    });
    state.previous_configuration = current_only.then(|| previous.clone());
    state.highest_epoch = state.current_configuration.as_ref().unwrap().epoch;
    state.role = ReplicaRole::Primary;
    state.read_status = AccessStatus::Granted;
    state.write_status = AccessStatus::Granted;
    state.scale_up_evidence = current_only.then(|| Box::new(evidence.clone()));
    state.build_commands.insert(
        build.build_id.clone(),
        kuberic_protocol::command::EnsureReplicaBuild {
            operation_id: build.build_id.clone(),
            local_replica_id: primary.replica_id,
            expected_instance_id: primary.instance_id.clone(),
            expected_agent_generation: primary.agent_generation.clone(),
            target: candidate,
            authority: None,
            source_session_id: None,
        },
    );
    let stage = if current_only {
        ScaleUpStage::CurrentOnly
    } else {
        ScaleUpStage::PreviousCurrent
    };
    let command = EnsureConfiguration {
        operation_id: intent.command_operation_id(stage, &primary, &current),
        previous_configuration: (!current_only).then_some(previous.clone()),
        current_configuration: current.clone(),
        previous_epoch: (!current_only).then_some(previous.epoch),
        current_epoch: current.epoch,
        effective_policy: current_policy,
        previous_policy: Some(previous_policy),
        secondary_removal_evidence: None,
        scale_up_evidence: Some(Box::new(evidence)),
        local_replica_id: primary.replica_id,
        expected_instance_id: primary.instance_id,
        expected_agent_generation: primary.agent_generation,
        transition_kind: TransitionKind::ScaleUp,
        failover_safe_lsn: None,
        primary_write_status: AccessStatus::Granted,
        current_only,
        retire_build_ids: current_only
            .then_some(vec![build.build_id.clone()])
            .unwrap_or_default(),
        switchover_handoff: None,
        retire_switchover_preparation_ids: Vec::new(),
    };
    (state, command, build)
}

fn scale_up_candidate_provisioning(intent: &ScaleUpIntent) -> ProvisioningIntent {
    let mut provisioning = ProvisioningIntent {
        purpose: ProvisioningPurpose::scale_up(ScaleUpProvisioning {
            resource_uid: intent.resource_uid.clone(),
            spec_generation: intent.spec_generation,
            desired_replicas: intent.desired_replicas,
            previous_configuration: intent.previous_configuration.clone(),
            previous_policy: intent.previous_policy.clone(),
            current_policy: intent.current_policy.clone(),
            target_replica_id: intent.target.replica_id,
        }),
        pod_uid: PodUid::new(intent.target.instance_id.as_str()),
        pvc_uid: PvcUid::new("scale-up-cut-candidate-pvc"),
        operation_id: OperationId::default(),
    };
    provisioning.operation_id = provisioning.expected_operation_id();
    assert_eq!(
        provisioning.target_identity(&intent.resource_uid),
        intent.target,
        "scale-up crash fixture candidate must be derived from durable provisioning"
    );
    provisioning
}

fn scale_up_failover_crash_fixture() -> (AgentState, EnsureConfiguration) {
    let identities = (1..=3)
        .map(|id| ReplicaIdentity {
            replica_id: ReplicaId::new(id),
            instance_id: ReplicaInstanceId::new(format!("failover-pod-{id}")),
            agent_generation: AgentGeneration::new(format!("failover-generation-{id}")),
        })
        .collect::<Vec<_>>();
    let previous_policy = EffectivePolicy::fixed(2, 30).unwrap();
    let current_policy = EffectivePolicy::fixed(3, 30).unwrap();
    let previous = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        identities[0].replica_id,
        vec![
            ConfigurationMember {
                identity: identities[0].clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: identities[1].clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        previous_policy.write_quorum,
    );
    let expanded = ConfigurationDescriptor::new(
        Epoch::new(0, 2),
        identities[0].replica_id,
        vec![
            ConfigurationMember {
                identity: identities[0].clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: identities[1].clone(),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: identities[2].clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        current_policy.write_quorum,
    );
    let mut intent = ScaleUpIntent {
        operation_id: OperationId::default(),
        resource_uid: ResourceUid::new("resource-1"),
        spec_generation: 2,
        desired_replicas: 3,
        previous_configuration: previous.clone(),
        current_configuration: expanded.clone(),
        previous_policy: previous_policy.clone(),
        current_policy: current_policy.clone(),
        primary: identities[0].clone(),
        target: identities[2].clone(),
        build_id: OperationId::new("scale-up-failover-crash-build"),
        snapshot_boundary_lsn: 4,
        catch_up_boundary_lsn: 9,
    };
    intent.operation_id = intent.expected_operation_id();
    let witness = |identity: ReplicaIdentity, sequence: u64| ScaleUpWitness {
        resource_uid: intent.resource_uid.clone(),
        role: expanded
            .members
            .iter()
            .find(|member| member.identity == identity)
            .unwrap()
            .role,
        process_session_id: ProcessSessionId::new(format!("failover-session-{sequence}")),
        report_sequence: sequence,
        epoch: expanded.epoch,
        previous_configuration_id: Some(previous.configuration_id.clone()),
        current_configuration_id: expanded.configuration_id.clone(),
        verified_replication_lsn: 9,
        write_status: AccessStatus::ReconfigurationPending,
        pending_operation_id: None,
        retained_operation_id: Some(intent.command_operation_id(
            ScaleUpStage::PreviousCurrent,
            &identity,
            &expanded,
        )),
        identity,
    };
    let failover_evidence = ScaleUpFailoverEvidence {
        intent: intent.clone(),
        previous_read_quorum: vec![witness(identities[0].clone(), 1)],
        current_read_quorum: vec![
            witness(identities[1].clone(), 2),
            witness(identities[2].clone(), 3),
        ],
    };
    let failover = ConfigurationDescriptor::new(
        Epoch::new(0, 3),
        identities[1].replica_id,
        vec![
            ConfigurationMember {
                identity: identities[0].clone(),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: identities[1].clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: identities[2].clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        current_policy.write_quorum,
    );
    let mut state = AgentState::new(StorageIdentity {
        resource_uid: intent.resource_uid.clone(),
        local_identity: identities[1].clone(),
        pod_uid: PodUid::new(identities[1].instance_id.as_str()),
        effective_policy: current_policy.clone(),
        ..storage_identity()
    });
    state.admitted_policy = Some(current_policy.clone());
    state.previous_policy = Some(previous_policy.clone());
    state.highest_epoch = expanded.epoch;
    state.previous_configuration = Some(previous.clone());
    state.current_configuration = Some(expanded.clone());
    state.scale_up_evidence = Some(Box::new(ScaleUpConfigurationEvidence::Admission {
        intent: intent.clone(),
    }));
    state.role = ReplicaRole::ActiveSecondary;
    state.read_status = AccessStatus::Granted;
    state.write_status = AccessStatus::NotPrimary;
    let command = EnsureConfiguration {
        operation_id: intent.command_operation_id(
            ScaleUpStage::PreviousCurrent,
            &identities[1],
            &failover,
        ),
        previous_configuration: Some(previous.clone()),
        current_configuration: failover.clone(),
        previous_epoch: Some(previous.epoch),
        current_epoch: failover.epoch,
        effective_policy: current_policy,
        previous_policy: Some(previous_policy),
        secondary_removal_evidence: None,
        scale_up_evidence: Some(Box::new(ScaleUpConfigurationEvidence::Failover {
            evidence: failover_evidence,
        })),
        local_replica_id: identities[1].replica_id,
        expected_instance_id: identities[1].instance_id.clone(),
        expected_agent_generation: identities[1].agent_generation.clone(),
        transition_kind: TransitionKind::Failover,
        failover_safe_lsn: Some(9),
        primary_write_status: AccessStatus::Granted,
        current_only: false,
        retire_build_ids: Vec::new(),
        switchover_handoff: None,
        retire_switchover_preparation_ids: Vec::new(),
    };
    (state, command)
}

fn scale_up_failover_initial_authority(state: &AgentState) -> AdmittedAuthority {
    AdmittedAuthority {
        local_identity: state.identity.local_identity.clone(),
        transition_kind: Some(TransitionKind::ScaleUp),
        previous_configuration: state.previous_configuration.clone(),
        current_configuration: state.current_configuration.clone().unwrap(),
        switchover_handoff: None,
        secondary_removal: None,
        scale_up: state.scale_up_evidence.clone(),
    }
}

fn scale_up_failover_command_for_identity(
    command: &EnsureConfiguration,
    identity: ReplicaIdentity,
) -> EnsureConfiguration {
    let intent = command.scale_up_evidence.as_deref().unwrap().intent();
    EnsureConfiguration {
        operation_id: intent.command_operation_id(
            ScaleUpStage::PreviousCurrent,
            &identity,
            &command.current_configuration,
        ),
        local_replica_id: identity.replica_id,
        expected_instance_id: identity.instance_id,
        expected_agent_generation: identity.agent_generation,
        ..command.clone()
    }
}

fn scale_up_failover_peer_fixture() -> (AgentState, EnsureConfiguration) {
    let (source, command) = scale_up_failover_crash_fixture();
    let peer_identity = command
        .current_configuration
        .members
        .iter()
        .find(|member| {
            member.role == ReplicaRole::ActiveSecondary
                && member.identity != source.identity.local_identity
        })
        .unwrap()
        .identity
        .clone();
    let mut state = source;
    state.identity.local_identity = peer_identity.clone();
    state.identity.pod_uid = PodUid::new(peer_identity.instance_id.as_str());
    state.role = ReplicaRole::Primary;
    state.read_status = AccessStatus::ReconfigurationPending;
    state.write_status = AccessStatus::ReconfigurationPending;
    let command = scale_up_failover_command_for_identity(&command, peer_identity);
    (state, command)
}

fn scale_up_failover_operation(lsn: i64) -> Operation {
    Operation {
        lsn,
        committed_lsn: lsn,
        data: Bytes::from(format!("scale-up-failover-durable-history-{lsn}")),
    }
}

fn scale_up_store_cut_exit_code(cut: &str, after: bool) -> i32 {
    let index = match cut {
        "store-initialization" => 0,
        "build-authority-admission" => 1,
        "snapshot-boundary-persistence" => 2,
        "source-progress-persistence" => 3,
        "target-progress-persistence" => 4,
        other => panic!("unknown scale-up store cut {other}"),
    };
    160 + index * 2 + i32::from(after)
}

fn scale_up_configuration_cut_exit_code(cut: &str) -> i32 {
    let index = [
        "pc-cc-authority-before",
        "pc-cc-authority-after",
        "pc-cc-access-before",
        "pc-cc-access-after",
        "current-only-authority-before",
        "current-only-authority-after",
        "current-only-access-before",
        "current-only-access-after",
        "build-retirement-before",
        "build-retirement-after",
        "completion-before",
        "completion-after",
    ]
    .iter()
    .position(|candidate| *candidate == cut)
    .unwrap_or_else(|| panic!("unknown scale-up configuration cut {cut}"));
    180 + i32::try_from(index).unwrap()
}

fn assert_exact_scale_up_exit(status: std::process::ExitStatus, expected: i32, label: &str) {
    assert_eq!(status.code(), Some(expected), "{label}: wrong exit code");
    #[cfg(unix)]
    assert_eq!(status.signal(), None, "{label}: terminated by signal");
    assert_ne!(expected, 101, "{label}: Rust test panic is not a cut");
    assert_ne!(expected, 134, "{label}: process abort is not a cut");
}

fn is_scale_up_cut_exit_code(code: Option<i32>) -> bool {
    matches!(code, Some(160..=169 | 180..=195))
}

#[test]
fn scale_up_cut_adapter_matches_real_source_and_candidate_runtime_trace() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let directory = tempdir().unwrap();
        let source_root = directory.path().join("source-owner");
        let candidate_root = directory.path().join("candidate-owner");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&candidate_root).unwrap();

        let (source_state, command, build) = scale_up_crash_fixture(false);
        let evidence = command.scale_up_evidence.as_deref().unwrap();
        let intent = evidence.intent();
        let source_path = SqliteStore::metadata_database_path(&source_root);
        let source_store =
            Arc::new(SqliteStore::create_authorized(&source_path, source_state.clone()).unwrap());
        let source_authority = AdmittedAuthority {
            local_identity: intent.primary.clone(),
            transition_kind: None,
            previous_configuration: None,
            current_configuration: intent.previous_configuration.clone(),
            switchover_handoff: None,
            secondary_removal: None,
            scale_up: None,
        };
        source_store.admit(&source_authority).await.unwrap();
        source_store.admit_build(&build).await.unwrap();

        let candidate_path = SqliteStore::metadata_database_path(&candidate_root);
        let mut candidate_state = AgentState::new(StorageIdentity {
            schema_version: SCHEMA_VERSION,
            resource_uid: intent.resource_uid.clone(),
            pod_uid: PodUid::new(intent.target.instance_id.as_str()),
            pvc_uid: PvcUid::new("candidate-real-pvc"),
            initialization_id: InitializationId::new("candidate-real-initialization"),
            local_identity: intent.target.clone(),
            effective_policy: intent.current_policy.clone(),
        });
        candidate_state.role = ReplicaRole::IdleSecondary;
        let candidate_store =
            Arc::new(SqliteStore::create_authorized(&candidate_path, candidate_state).unwrap());
        candidate_store.admit_build(&build).await.unwrap();

        let source_application = Arc::new(CrashState::open(source_root.join("application.json")));
        let snapshot_operations = [
            Operation {
                lsn: 1,
                committed_lsn: 1,
                data: Bytes::from_static(b"snapshot-row-alpha"),
            },
            Operation {
                lsn: 2,
                committed_lsn: 2,
                data: Bytes::from_static(b"snapshot-row-beta-with-different-length"),
            },
            Operation {
                lsn: 4,
                committed_lsn: 4,
                data: Bytes::from_static(b"snapshot-boundary-row"),
            },
        ];
        for operation in snapshot_operations.iter().cloned() {
            source_application.apply(operation).await.unwrap();
        }
        let source_runtime = Arc::new(PodRuntime::new(
            intent.primary.clone(),
            source_application.clone(),
            source_store.clone(),
        ));
        source_runtime
            .reconstruct(
                OpenMode::Existing,
                ReplicaRole::Primary,
                AccessStatus::Granted,
                AccessStatus::Granted,
                None,
            )
            .await
            .unwrap();

        let candidate_application =
            Arc::new(CrashState::open(candidate_root.join("application.json")));
        let candidate_runtime = Arc::new(PodRuntime::new(
            intent.target.clone(),
            candidate_application.clone(),
            candidate_store.clone(),
        ));
        candidate_runtime
            .apply_effect(RuntimeEffect {
                operation_id: OperationId::new("candidate-open"),
                sequence: 1,
                action: RuntimeEffectAction::Open(OpenMode::New),
            })
            .await
            .unwrap();
        candidate_runtime
            .apply_effect(RuntimeEffect {
                operation_id: OperationId::new("candidate-idle"),
                sequence: 2,
                action: RuntimeEffectAction::ChangeRole(ReplicaRole::IdleSecondary),
            })
            .await
            .unwrap();
        candidate_runtime
            .apply_effect(RuntimeEffect {
                operation_id: OperationId::new("candidate-build-authority"),
                sequence: 3,
                action: RuntimeEffectAction::AdmitBuildAuthority(Box::new(build.clone())),
            })
            .await
            .unwrap();

        let mut enumeration = source_application
            .get_copy_state(4, Box::pin(stream::empty()))
            .await
            .unwrap();
        let post_enumeration_operations = [
            Operation {
                lsn: 5,
                committed_lsn: 5,
                data: Bytes::from_static(b"client-write-after-enumeration"),
            },
            Operation {
                lsn: 7,
                committed_lsn: 7,
                data: Bytes::from_static(b"client-write-with-lsn-gap"),
            },
            Operation {
                lsn: 9,
                committed_lsn: 9,
                data: Bytes::from_static(b"catch-up-boundary-value"),
            },
        ];
        for operation in post_enumeration_operations.iter().cloned() {
            source_application.apply(operation).await.unwrap();
        }

        let mut sequence = 0;
        while let Some(bytes) = enumeration.next().await {
            candidate_application
                .apply_copy_chunk(
                    &build.build_id,
                    sequence,
                    CopyChunk {
                        data: bytes.unwrap(),
                    },
                )
                .await
                .unwrap();
            sequence += 1;
        }
        candidate_application
            .finish_copy(&build.build_id, 4, 4)
            .await
            .unwrap();
        for operation in post_enumeration_operations.iter().cloned() {
            candidate_application.apply(operation).await.unwrap();
        }
        for operation in snapshot_operations
            .iter()
            .chain(post_enumeration_operations.iter())
        {
            assert!(
                source_application.verify_applied(operation).await.unwrap()
                    && candidate_application
                        .verify_applied(operation)
                        .await
                        .unwrap(),
                "source/candidate durable bytes differ at LSN {}",
                operation.lsn
            );
        }
        assert_eq!(
            candidate_application.durable_progress().await.unwrap(),
            DurableApplicationProgress {
                applied_lsn: 9,
                committed_lsn: 9,
            }
        );

        let authority =
            admit_configuration(&command, &source_store.load_state().await.unwrap()).unwrap();
        source_runtime
            .apply_effect(RuntimeEffect {
                operation_id: OperationId::new("real-source-refresh-progress"),
                sequence: 4,
                action: RuntimeEffectAction::RefreshApplicationProgress,
            })
            .await
            .unwrap();
        let real_result = source_runtime
            .apply_effect(RuntimeEffect {
                operation_id: OperationId::new("real-source-scale-up-authority"),
                sequence: 5,
                action: RuntimeEffectAction::AdmitAuthority(Box::new(authority.clone())),
            })
            .await
            .unwrap();
        assert_eq!(real_result.postcondition.authority, Some(authority));
        assert_eq!(real_result.postcondition.current_progress, 9);
        assert_eq!(
            candidate_application.durable_progress().await.unwrap(),
            DurableApplicationProgress {
                applied_lsn: 9,
                committed_lsn: 9,
            }
        );
    });
}

fn scale_up_candidate_root(path: &Path) -> PathBuf {
    path.parent()
        .expect("scale-up source metadata parent")
        .join("candidate-owner")
}

fn scale_up_candidate_store_path(path: &Path) -> PathBuf {
    SqliteStore::metadata_database_path(&scale_up_candidate_root(path))
}

fn replica_authority_table_bytes(path: &Path) -> Option<Vec<u8>> {
    use rusqlite::OptionalExtension;

    rusqlite::Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT authority_json FROM replica_authority WHERE singleton = 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .unwrap()
        .map(String::into_bytes)
}

fn durable_application_history_bytes(path: &Path) -> Vec<u8> {
    let raw = std::fs::read(path).expect("durable application file");
    let persisted: CrashPersistedState = serde_json::from_slice(&raw).unwrap();
    serde_json::to_vec(&persisted.operations).unwrap()
}

async fn open_real_scale_up_owners(
    path: &Path,
    terminate_before_initialization: bool,
) -> (
    Arc<SqliteStore>,
    Arc<PodRuntime>,
    Arc<CrashState>,
    Arc<SqliteStore>,
    Arc<PodRuntime>,
    Arc<CrashState>,
    BuildAuthority,
) {
    let (source_state, command, build) = scale_up_crash_fixture(false);
    let intent = command.scale_up_evidence.as_deref().unwrap().intent();
    let new_source = !path.is_file();
    if new_source && terminate_before_initialization {
        std::process::exit(scale_up_store_cut_exit_code("store-initialization", false));
    }
    let source_store = if new_source {
        Arc::new(SqliteStore::create_authorized(path, source_state).unwrap())
    } else {
        Arc::new(SqliteStore::open_existing(path, None).unwrap())
    };
    let source_application = Arc::new(CrashState::open(crash_application_path(path)));
    if new_source {
        source_store
            .admit(&AdmittedAuthority {
                local_identity: intent.primary.clone(),
                transition_kind: None,
                previous_configuration: None,
                current_configuration: intent.previous_configuration.clone(),
                switchover_handoff: None,
                secondary_removal: None,
                scale_up: None,
            })
            .await
            .unwrap();
    }
    let source_runtime = Arc::new(PodRuntime::new(
        intent.primary.clone(),
        source_application.clone(),
        source_store.clone(),
    ));
    let source_state = source_store.load_state().await.unwrap();
    let source_reconstruction = source_runtime
        .reconstruct(
            OpenMode::Existing,
            source_state.role,
            source_state.read_status,
            source_state.write_status,
            None,
        )
        .await;
    if let Err(error) = source_reconstruction {
        assert!(
            matches!(error, RuntimeError::ReconfigurationPending),
            "unexpected scale-up source reconstruction error: {error:?}"
        );
    }
    if new_source {
        let seeded = source_runtime
            .data_plane()
            .begin_write(ClientWrite {
                operation_id: OperationId::new("scale-up-seeded-production-write"),
                data: seeded_operation().data,
            })
            .await
            .unwrap();
        assert_eq!(seeded.lsn, seeded_operation().lsn);
        seeded.committed().await.unwrap();
    }

    let candidate_root = scale_up_candidate_root(path);
    std::fs::create_dir_all(&candidate_root).unwrap();
    let candidate_path = scale_up_candidate_store_path(path);
    let new_candidate = !candidate_path.is_file();
    let candidate_store = if new_candidate {
        let provisioning = scale_up_candidate_provisioning(intent);
        let mut state = AgentState::new(StorageIdentity {
            schema_version: SCHEMA_VERSION,
            resource_uid: intent.resource_uid.clone(),
            pod_uid: provisioning.pod_uid.clone(),
            pvc_uid: provisioning.pvc_uid.clone(),
            initialization_id: provisioning.initialization_id(&intent.resource_uid),
            local_identity: intent.target.clone(),
            effective_policy: intent.current_policy.clone(),
        });
        state.scale_up_initialization = Some(provisioning);
        state.role = ReplicaRole::IdleSecondary;
        state.build_commands.insert(
            build.build_id.clone(),
            kuberic_protocol::command::EnsureReplicaBuild {
                operation_id: build.build_id.clone(),
                local_replica_id: intent.target.replica_id,
                expected_instance_id: intent.target.instance_id.clone(),
                expected_agent_generation: intent.target.agent_generation.clone(),
                target: intent.target.clone(),
                authority: Some(build.clone()),
                source_session_id: Some(ProcessSessionId::new("scale-up-cut-source-session")),
            },
        );
        Arc::new(SqliteStore::create_authorized(&candidate_path, state).unwrap())
    } else {
        Arc::new(SqliteStore::open_existing(&candidate_path, None).unwrap())
    };
    let candidate_application = Arc::new(CrashState::open_with_replication(
        candidate_root.join("application.json"),
    ));
    let candidate_runtime = Arc::new(PodRuntime::new(
        intent.target.clone(),
        candidate_application.clone(),
        candidate_store.clone(),
    ));
    let candidate_state = candidate_store.load_state().await.unwrap();
    candidate_runtime
        .reconstruct(
            if new_candidate {
                OpenMode::New
            } else {
                OpenMode::Existing
            },
            candidate_state.role,
            candidate_state.read_status,
            candidate_state.write_status,
            None,
        )
        .await
        .unwrap();
    (
        source_store,
        source_runtime,
        source_application,
        candidate_store,
        candidate_runtime,
        candidate_application,
        build,
    )
}

async fn ensure_scale_up_store_cut(path: &Path, cut: &str, after: bool, terminate: bool) {
    if !terminate {
        eprintln!("scale-up-real-recovery cut={cut} stage=open-owners");
    }
    let (
        source_store,
        source_runtime,
        source_application,
        candidate_store,
        candidate_runtime,
        candidate_application,
        expected_build,
    ) = open_real_scale_up_owners(path, terminate && cut == "store-initialization" && !after).await;
    if !terminate {
        eprintln!("scale-up-real-recovery cut={cut} stage=owners-open");
    }
    if cut == "store-initialization" && terminate && after {
        std::process::exit(scale_up_store_cut_exit_code(cut, after));
    }
    if cut == "store-initialization" && !terminate {
        return;
    }

    if candidate_store
        .load_build(&expected_build.build_id)
        .await
        .unwrap()
        .is_none()
    {
        if cut == "build-authority-admission" && terminate && !after {
            std::process::exit(scale_up_store_cut_exit_code(cut, after));
        }
        let sequence = candidate_store
            .load_state()
            .await
            .unwrap()
            .next_effect_sequence;
        RuntimeAdapter::new(candidate_store.clone(), candidate_runtime.clone())
            .execute(RuntimeEffect {
                operation_id: OperationId::new("real-candidate-admit-build"),
                sequence,
                action: RuntimeEffectAction::AdmitBuildAuthority(Box::new(expected_build.clone())),
            })
            .await
            .unwrap();
        if cut == "build-authority-admission" && terminate && after {
            std::process::exit(scale_up_store_cut_exit_code(cut, after));
        }
    } else {
        candidate_runtime
            .apply_effect(RuntimeEffect {
                operation_id: OperationId::new("restore-real-candidate-build"),
                sequence: candidate_store
                    .load_state()
                    .await
                    .unwrap()
                    .next_effect_sequence,
                action: RuntimeEffectAction::AdmitBuildAuthority(Box::new(expected_build.clone())),
            })
            .await
            .unwrap();
    }
    if cut == "build-authority-admission" && !terminate {
        return;
    }

    if cut == "snapshot-boundary-persistence" && terminate && !after {
        std::process::exit(scale_up_store_cut_exit_code(cut, after));
    }
    let authority = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        source_runtime.authorize_build(
            expected_build.build_id.clone(),
            expected_build.target.clone(),
            BuildConfiguration::Current,
        ),
    )
    .await
    .expect("real source build authorization timed out")
    .unwrap();
    if !terminate {
        eprintln!("scale-up-real-recovery cut={cut} stage=build-authorized");
    }
    assert_eq!(authority, expected_build);
    let mut prepared = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        source_runtime
            .data_plane()
            .prepare_copy(PrepareCopyRequest {
                build_id: expected_build.build_id.clone(),
                target: expected_build.target.clone(),
                configuration: BuildConfiguration::Current,
                copy_context: Box::pin(stream::empty()),
            }),
    )
    .await
    .expect("real source copy preparation timed out")
    .unwrap();
    if !terminate {
        eprintln!("scale-up-real-recovery cut={cut} stage=copy-prepared");
    }
    let mut items = Vec::new();
    while let Some(item) =
        tokio::time::timeout(std::time::Duration::from_secs(5), prepared.items.next())
            .await
            .expect("real scale-up copy item timed out")
    {
        let item = item.unwrap();
        let final_item = item.final_item;
        items.push(item);
        if final_item {
            break;
        }
    }
    if cut == "snapshot-boundary-persistence" && terminate && after {
        std::process::exit(scale_up_store_cut_exit_code(cut, after));
    }

    for item in items {
        if item.final_item && cut == "target-progress-persistence" && terminate && !after {
            std::process::exit(scale_up_store_cut_exit_code(cut, after));
        }
        let acknowledgement = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            candidate_runtime
                .data_plane()
                .receive_copy_item(item.clone()),
        )
        .await
        .expect("real candidate copy delivery timed out")
        .unwrap();
        if item.final_item && cut == "target-progress-persistence" && terminate && after {
            std::process::exit(scale_up_store_cut_exit_code(cut, after));
        }
        if item.final_item && cut == "source-progress-persistence" && terminate && !after {
            std::process::exit(scale_up_store_cut_exit_code(cut, after));
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            source_runtime
                .data_plane()
                .accept_copy_acknowledgement(acknowledgement),
        )
        .await
        .expect("real source copy acknowledgement timed out")
        .unwrap();
        if item.final_item && cut == "source-progress-persistence" && terminate && after {
            std::process::exit(scale_up_store_cut_exit_code(cut, after));
        }
    }

    let source_progress = source_store
        .load_build_progress(&expected_build.build_id)
        .await
        .unwrap()
        .expect("real source progress");
    let target_progress = candidate_store
        .load_build_progress(&expected_build.build_id)
        .await
        .unwrap()
        .expect("real target progress");
    assert_eq!(source_progress.catch_up_boundary_lsn, Some(1));
    assert_eq!(target_progress.catch_up_boundary_lsn, Some(1));
    assert!(source_progress.completed && target_progress.completed);
    assert_eq!(
        source_application
            .durable_progress()
            .await
            .unwrap()
            .committed_lsn,
        1
    );
    assert_eq!(
        candidate_application
            .durable_progress()
            .await
            .unwrap()
            .committed_lsn,
        1
    );
    assert!(
        candidate_application
            .verify_applied(&seeded_operation())
            .await
            .unwrap()
    );
}

async fn reconstruct_scale_up_configuration_runtime(
    runtime: &Arc<PodRuntime>,
    state: &AgentState,
    command: &EnsureConfiguration,
    cut: &str,
) {
    let reconstruction = runtime
        .reconstruct(
            OpenMode::Existing,
            state.role,
            state.read_status,
            state.write_status,
            None,
        )
        .await;
    if let Err(error) = reconstruction {
        assert!(
            matches!(error, RuntimeError::ReconfigurationPending),
            "{cut}: unexpected reconstruction error: {error:?}"
        );
        let intent = command.scale_up_evidence.as_deref().unwrap().intent();
        runtime
            .data_plane()
            .accept_acknowledgement(kuberic_wire::proto::ReplicationAck {
                protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                sender: Some(intent.primary.clone().into()),
                receiver: Some(intent.target.clone().into()),
                epoch: Some(intent.current_configuration.epoch.into()),
                previous_configuration_id: state
                    .previous_configuration
                    .as_ref()
                    .map_or_else(String::new, |previous| {
                        previous.configuration_id.to_string()
                    }),
                current_configuration_id: intent.current_configuration.configuration_id.to_string(),
                received_lsn: intent.catch_up_boundary_lsn,
                applied_lsn: intent.catch_up_boundary_lsn,
                committed_lsn: intent.catch_up_boundary_lsn,
                ..Default::default()
            })
            .await
            .unwrap();
        runtime
            .reconstruct(
                OpenMode::Existing,
                state.role,
                state.read_status,
                state.write_status,
                None,
            )
            .await
            .unwrap();
    }
}

#[allow(dead_code)]
async fn execute_scale_up_configuration_cut_legacy(path: &Path, cut: &str, terminate: bool) {
    eprintln!("scale-up-config cut={cut} stage=start");
    let current_only = cut.starts_with("current-only")
        || cut.starts_with("build-retirement")
        || cut.starts_with("completion");
    if !path.is_file() {
        Box::pin(ensure_scale_up_store_cut(
            path,
            "source-progress-persistence",
            false,
            false,
        ))
        .await;
    }
    eprintln!("scale-up-config cut={cut} stage=copy-ready");
    let store = Arc::new(SqliteStore::open_existing(path, None).unwrap());
    let application = Arc::new(CrashState::open(crash_application_path(path)));
    let (_, pc_cc_command, build) = scale_up_crash_fixture(false);
    let (_, command, _) = scale_up_crash_fixture(current_only);
    let mut durable = store.load_state().await.unwrap();
    let mut runtime = Arc::new(PodRuntime::new(
        durable.identity.local_identity.clone(),
        application.clone(),
        store.clone(),
    ));
    let reconstruction = runtime
        .reconstruct(
            OpenMode::Existing,
            durable.role,
            durable.read_status,
            durable.write_status,
            None,
        )
        .await;
    if let Err(error) = reconstruction {
        assert!(
            matches!(error, RuntimeError::ReconfigurationPending),
            "{cut}: unexpected reconstruction error: {error:?}"
        );
        let intent = pc_cc_command.scale_up_evidence.as_deref().unwrap().intent();
        runtime
            .data_plane()
            .accept_acknowledgement(kuberic_wire::proto::ReplicationAck {
                protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                sender: Some(intent.primary.clone().into()),
                receiver: Some(intent.target.clone().into()),
                epoch: Some(intent.current_configuration.epoch.into()),
                previous_configuration_id: durable
                    .previous_configuration
                    .as_ref()
                    .map_or_else(String::new, |previous| {
                        previous.configuration_id.to_string()
                    }),
                current_configuration_id: intent.current_configuration.configuration_id.to_string(),
                received_lsn: intent.catch_up_boundary_lsn,
                applied_lsn: intent.catch_up_boundary_lsn,
                committed_lsn: intent.catch_up_boundary_lsn,
                ..Default::default()
            })
            .await
            .unwrap();
        let reconstruction = runtime
            .reconstruct(
                OpenMode::Existing,
                durable.role,
                durable.read_status,
                durable.write_status,
                None,
            )
            .await;
        if let Err(error) = reconstruction {
            assert!(
                matches!(error, RuntimeError::ReconfigurationPending),
                "{cut}: unexpected post-PC/CC reconstruction error: {error:?}"
            );
            let intent = command.scale_up_evidence.as_deref().unwrap().intent();
            runtime
                .data_plane()
                .accept_acknowledgement(kuberic_wire::proto::ReplicationAck {
                    protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                    sender: Some(intent.primary.clone().into()),
                    receiver: Some(intent.target.clone().into()),
                    epoch: Some(intent.current_configuration.epoch.into()),
                    previous_configuration_id: durable
                        .previous_configuration
                        .as_ref()
                        .map_or_else(String::new, |previous| {
                            previous.configuration_id.to_string()
                        }),
                    current_configuration_id: intent
                        .current_configuration
                        .configuration_id
                        .to_string(),
                    received_lsn: intent.catch_up_boundary_lsn,
                    applied_lsn: intent.catch_up_boundary_lsn,
                    committed_lsn: intent.catch_up_boundary_lsn,
                    ..Default::default()
                })
                .await
                .unwrap();
            let reconstruction = runtime
                .reconstruct(
                    OpenMode::Existing,
                    durable.role,
                    durable.read_status,
                    durable.write_status,
                    None,
                )
                .await;
            if let Err(error) = reconstruction {
                assert!(
                    matches!(error, RuntimeError::ReconfigurationPending),
                    "{cut}: unexpected seeded PC/CC reconstruction error: {error:?}"
                );
                let intent = command.scale_up_evidence.as_deref().unwrap().intent();
                runtime
                    .data_plane()
                    .accept_acknowledgement(kuberic_wire::proto::ReplicationAck {
                        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                        sender: Some(intent.primary.clone().into()),
                        receiver: Some(intent.target.clone().into()),
                        epoch: Some(intent.current_configuration.epoch.into()),
                        previous_configuration_id: durable
                            .previous_configuration
                            .as_ref()
                            .map_or_else(String::new, |previous| {
                                previous.configuration_id.to_string()
                            }),
                        current_configuration_id: intent
                            .current_configuration
                            .configuration_id
                            .to_string(),
                        received_lsn: intent.catch_up_boundary_lsn,
                        applied_lsn: intent.catch_up_boundary_lsn,
                        committed_lsn: intent.catch_up_boundary_lsn,
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                runtime
                    .reconstruct(
                        OpenMode::Existing,
                        durable.role,
                        durable.read_status,
                        durable.write_status,
                        None,
                    )
                    .await
                    .unwrap();
            }
        }
    }
    if current_only
        && durable.current_configuration != Some(pc_cc_command.current_configuration.clone())
    {
        eprintln!("scale-up-config cut={cut} stage=seed-pc-cc");
        Box::pin(execute_scale_up_configuration_cut(
            path,
            "pc-cc-access-after",
            false,
        ))
        .await;
        eprintln!("scale-up-config cut={cut} stage=pc-cc-seeded");
        durable = store.load_state().await.unwrap();
        runtime = Arc::new(PodRuntime::new(
            durable.identity.local_identity.clone(),
            application.clone(),
            store.clone(),
        ));
        reconstruct_scale_up_configuration_runtime(&runtime, &durable, &command, cut).await;
    }

    if store.load_state().await.unwrap().reconfiguration.is_none()
        && store
            .load_state()
            .await
            .unwrap()
            .retained_command
            .as_ref()
            .is_none_or(|retained| retained.command.operation_id != command.operation_id)
    {
        store.begin_configuration(&command).await.unwrap();
    }

    let (cut_stage, after) = match cut {
        "pc-cc-authority-before" | "current-only-authority-before" => {
            (CoordinatorStage::AdmitAuthority, false)
        }
        "pc-cc-authority-after" | "current-only-authority-after" => {
            (CoordinatorStage::AdmitAuthority, true)
        }
        "pc-cc-access-before" | "current-only-access-before" => (CoordinatorStage::Activate, false),
        "pc-cc-access-after" | "current-only-access-after" => (CoordinatorStage::Activate, true),
        "build-retirement-before" => (CoordinatorStage::RetireBuild, false),
        "build-retirement-after" => (CoordinatorStage::RetireBuild, true),
        "completion-before" => (CoordinatorStage::Complete, false),
        "completion-after" => (CoordinatorStage::Complete, true),
        other => panic!("unknown scale-up configuration cut {other}"),
    };

    loop {
        let state = store.load_state().await.unwrap();
        let Some(record) = state.reconfiguration.clone() else {
            break;
        };
        if terminate && record.stage == cut_stage && !after {
            std::process::exit(scale_up_configuration_cut_exit_code(cut));
        }
        match record.stage {
            CoordinatorStage::AdmitAuthority => {
                let operation_id =
                    OperationId::new(format!("{}:admit-authority", command.operation_id));
                let effect = if let Some(retained) = state
                    .retained_result
                    .as_ref()
                    .filter(|retained| retained.operation_id == operation_id)
                {
                    retained.effect.clone()
                } else {
                    let authority = admit_configuration(&command, &state).unwrap();
                    RuntimeEffect {
                        operation_id,
                        sequence: state.next_effect_sequence,
                        action: RuntimeEffectAction::AdmitAuthority(Box::new(authority)),
                    }
                };
                RuntimeAdapter::new(store.clone(), runtime.clone())
                    .execute(effect)
                    .await
                    .unwrap();
                if terminate && cut_stage == record.stage && after {
                    std::process::exit(scale_up_configuration_cut_exit_code(cut));
                }
                store
                    .advance_configuration(
                        &command.operation_id,
                        record.stage,
                        CoordinatorStage::Activate,
                        None,
                    )
                    .await
                    .unwrap();
            }
            CoordinatorStage::Activate => {
                let evidence = command.scale_up_evidence.as_deref().unwrap();
                let intent = evidence.intent();
                runtime
                    .data_plane()
                    .accept_acknowledgement(kuberic_wire::proto::ReplicationAck {
                        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                        sender: Some(intent.primary.clone().into()),
                        receiver: Some(intent.target.clone().into()),
                        epoch: Some(command.current_configuration.epoch.into()),
                        previous_configuration_id: command
                            .previous_configuration
                            .as_ref()
                            .map_or_else(String::new, |previous| {
                                previous.configuration_id.to_string()
                            }),
                        current_configuration_id: command
                            .current_configuration
                            .configuration_id
                            .to_string(),
                        received_lsn: intent.catch_up_boundary_lsn,
                        applied_lsn: intent.catch_up_boundary_lsn,
                        committed_lsn: intent.catch_up_boundary_lsn,
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                let operation_id = OperationId::new(format!("{}:activate", command.operation_id));
                let effect = state
                    .retained_result
                    .as_ref()
                    .filter(|retained| retained.operation_id == operation_id)
                    .map_or_else(
                        || RuntimeEffect {
                            operation_id,
                            sequence: state.next_effect_sequence,
                            action: RuntimeEffectAction::SetAccessStatus {
                                read: AccessStatus::Granted,
                                write: AccessStatus::Granted,
                            },
                        },
                        |retained| retained.effect.clone(),
                    );
                RuntimeAdapter::new(store.clone(), runtime.clone())
                    .execute(effect)
                    .await
                    .unwrap();
                if terminate && cut_stage == record.stage && after {
                    std::process::exit(scale_up_configuration_cut_exit_code(cut));
                }
                store
                    .advance_configuration(
                        &command.operation_id,
                        record.stage,
                        if current_only {
                            CoordinatorStage::RetireBuild
                        } else {
                            CoordinatorStage::Complete
                        },
                        None,
                    )
                    .await
                    .unwrap();
            }
            CoordinatorStage::RetireBuild => {
                let operation_id =
                    OperationId::new(format!("{}:retire-build-0", command.operation_id));
                let effect = state
                    .retained_result
                    .as_ref()
                    .filter(|retained| retained.operation_id == operation_id)
                    .map_or_else(
                        || RuntimeEffect {
                            operation_id,
                            sequence: state.next_effect_sequence,
                            action: RuntimeEffectAction::RetireBuild(build.build_id.clone()),
                        },
                        |retained| retained.effect.clone(),
                    );
                RuntimeAdapter::new(store.clone(), runtime.clone())
                    .execute(effect)
                    .await
                    .unwrap();
                if terminate && cut_stage == record.stage && after {
                    std::process::exit(scale_up_configuration_cut_exit_code(cut));
                }
                store
                    .advance_configuration(
                        &command.operation_id,
                        record.stage,
                        CoordinatorStage::Complete,
                        None,
                    )
                    .await
                    .unwrap();
            }
            CoordinatorStage::Complete => {
                if terminate && !after {
                    std::process::exit(scale_up_configuration_cut_exit_code(cut));
                }
                store
                    .complete_configuration(&command.operation_id)
                    .await
                    .unwrap();
                if terminate && after {
                    std::process::exit(scale_up_configuration_cut_exit_code(cut));
                }
            }
            other => panic!("{cut}: unexpected same-primary stage {other:?}"),
        }
    }
    let durable = store.load_state().await.unwrap();
    assert!(durable.pending_effect.is_none(), "{cut}");
    assert!(durable.reconfiguration.is_none(), "{cut}");
    assert_eq!(durable.write_status, AccessStatus::Granted, "{cut}");
    assert_eq!(
        durable.current_configuration,
        Some(command.current_configuration),
        "{cut}"
    );
    assert_eq!(
        durable.retired_builds.contains(&build.build_id),
        current_only,
        "{cut}"
    );
    let persisted = application.durable_progress().await.unwrap();
    assert_eq!(persisted.committed_lsn, 1, "{cut}");
    assert!(
        application
            .verify_applied(&seeded_operation())
            .await
            .unwrap(),
        "{cut}"
    );
}

fn scale_up_command_for_candidate(command: &EnsureConfiguration) -> EnsureConfiguration {
    let intent = command
        .scale_up_evidence
        .as_deref()
        .expect("scale-up configuration evidence")
        .intent();
    let candidate = intent.target.clone();
    EnsureConfiguration {
        operation_id: intent.command_operation_id(
            if command.current_only {
                ScaleUpStage::CurrentOnly
            } else {
                ScaleUpStage::PreviousCurrent
            },
            &candidate,
            &command.current_configuration,
        ),
        local_replica_id: candidate.replica_id,
        expected_instance_id: candidate.instance_id,
        expected_agent_generation: candidate.agent_generation,
        ..command.clone()
    }
}

fn normalized_scale_up_configuration_cut(cut: &str) -> &str {
    cut.strip_prefix("pc-cc-")
        .or_else(|| cut.strip_prefix("current-only-"))
        .unwrap_or(cut)
}

async fn run_scale_up_source_configuration(
    store: Arc<SqliteStore>,
    runtime: Arc<PodRuntime>,
    candidate: Arc<PodRuntime>,
    command: EnsureConfiguration,
    cut: Option<&str>,
) {
    let cut = cut.map(str::to_owned);
    let exit_code = cut
        .as_deref()
        .map(scale_up_configuration_cut_exit_code)
        .unwrap_or_default();
    let executor = Arc::new(ScaleUpProductionCutRuntime {
        runtime,
        candidate,
        acknowledgement_sent: std::sync::atomic::AtomicBool::new(false),
        write_label: command.operation_id.to_string(),
    });
    let coordinator_store = Arc::new(ScaleUpProductionCutStore {
        inner: store,
        cut: cut
            .as_deref()
            .map(normalized_scale_up_configuration_cut)
            .map(str::to_owned),
        exit_code,
    });
    Coordinator::new(coordinator_store, executor)
        .ensure_configuration(command)
        .await
        .unwrap();
}

async fn execute_scale_up_configuration_cut(path: &Path, cut: &str, terminate: bool) {
    eprintln!("scale-up-production cut={cut} stage=start");
    if !path.is_file() {
        ensure_scale_up_store_cut(path, "source-progress-persistence", false, false).await;
    }
    let current_only = cut.starts_with("current-only")
        || cut.starts_with("build-retirement")
        || cut.starts_with("completion");
    let (_, pc_cc_command, _) = scale_up_crash_fixture(false);
    let (_, command, build) = scale_up_crash_fixture(current_only);

    let (
        source_store,
        source_runtime,
        source_application,
        candidate_store,
        candidate_runtime,
        candidate_application,
        _,
    ) = open_real_scale_up_owners(path, false).await;
    eprintln!("scale-up-production cut={cut} stage=owners-open");

    let source_state = source_store.load_state().await.unwrap();
    if current_only
        && source_state.current_configuration.as_ref() != Some(&pc_cc_command.current_configuration)
    {
        let candidate_command = scale_up_command_for_candidate(&pc_cc_command);
        Coordinator::new(candidate_store.clone(), candidate_runtime.clone())
            .ensure_configuration(candidate_command)
            .await
            .unwrap();
        run_scale_up_source_configuration(
            source_store.clone(),
            source_runtime.clone(),
            candidate_runtime.clone(),
            pc_cc_command,
            None,
        )
        .await;
        eprintln!("scale-up-production cut={cut} stage=source-pc-cc-complete");
    }

    let candidate_command = scale_up_command_for_candidate(&command);
    let candidate_state = candidate_store.load_state().await.unwrap();
    let candidate_completed = candidate_state
        .retained_command
        .as_ref()
        .is_some_and(|retained| retained.command == candidate_command)
        || candidate_state
            .completed_scale_up
            .as_ref()
            .is_some_and(|retained| retained.command == candidate_command);
    if !candidate_completed {
        Coordinator::new(candidate_store.clone(), candidate_runtime.clone())
            .ensure_configuration(candidate_command.clone())
            .await
            .unwrap();
    }
    eprintln!("scale-up-production cut={cut} stage=candidate-complete");

    run_scale_up_source_configuration(
        source_store.clone(),
        source_runtime.clone(),
        candidate_runtime.clone(),
        command.clone(),
        terminate.then_some(cut),
    )
    .await;
    eprintln!("scale-up-production cut={cut} stage=source-complete");

    let source = source_store.load_state().await.unwrap();
    let candidate = candidate_store.load_state().await.unwrap();
    assert!(source.pending_effect.is_none() && source.reconfiguration.is_none());
    assert!(candidate.pending_effect.is_none() && candidate.reconfiguration.is_none());
    assert_eq!(
        source.current_configuration,
        Some(command.current_configuration.clone())
    );
    assert_eq!(
        candidate.current_configuration,
        Some(command.current_configuration.clone())
    );
    assert_eq!(source.write_status, AccessStatus::Granted);
    assert_eq!(candidate.write_status, AccessStatus::NotPrimary);
    assert_eq!(
        source.retired_builds.contains(&build.build_id),
        current_only
    );
    assert_eq!(
        candidate.retired_builds.contains(&build.build_id),
        current_only
    );
    let source_history = source_application.state.lock().unwrap().operations.clone();
    let candidate_history = candidate_application
        .state
        .lock()
        .unwrap()
        .operations
        .clone();
    assert_eq!(
        source_history, candidate_history,
        "{cut}: source and candidate durable application histories diverged"
    );
}

fn scale_up_failover_peer_root(path: &Path) -> PathBuf {
    path.parent()
        .and_then(Path::parent)
        .expect("agent database is under the failover data root")
        .join("failover-peer-owner")
}

fn scale_up_failover_peer_store_path(path: &Path) -> PathBuf {
    SqliteStore::metadata_database_path(&scale_up_failover_peer_root(path))
}

fn scale_up_failover_peer_application_path(path: &Path) -> PathBuf {
    scale_up_failover_peer_root(path).join("application.json")
}

fn scale_up_failover_witness_root(path: &Path) -> PathBuf {
    path.parent()
        .and_then(Path::parent)
        .expect("agent database is under the failover data root")
        .join("failover-witness-owner")
}

fn scale_up_failover_witness_store_path(path: &Path) -> PathBuf {
    SqliteStore::metadata_database_path(&scale_up_failover_witness_root(path))
}

fn scale_up_failover_witness_application_path(path: &Path) -> PathBuf {
    scale_up_failover_witness_root(path).join("application.json")
}

async fn initialize_scale_up_failover_owner(
    path: &Path,
    application_path: &Path,
    state: AgentState,
) {
    std::fs::create_dir_all(
        path.parent()
            .and_then(Path::parent)
            .expect("failover metadata parent"),
    )
    .unwrap();
    let store = Arc::new(SqliteStore::create_authorized(path, state.clone()).unwrap());
    let application = Arc::new(CrashState::open_with_replication(application_path));
    for lsn in 1..=9 {
        application
            .apply(scale_up_failover_operation(lsn))
            .await
            .unwrap();
    }
    application.commit(9).await.unwrap();
    let authority = scale_up_failover_initial_authority(&state);
    store
        .record_replication_progress(&ReplicationProgress {
            fence: authority.fence(),
            verified_lsn: 9,
        })
        .await
        .unwrap();
    let runtime = Arc::new(PodRuntime::new(
        state.identity.local_identity.clone(),
        application,
        store,
    ));
    for (sequence, action) in [
        RuntimeEffectAction::Open(OpenMode::Existing),
        RuntimeEffectAction::AdmitAuthority(Box::new(authority)),
        RuntimeEffectAction::ChangeRole(state.role),
        RuntimeEffectAction::SetAccessStatus {
            read: state.read_status,
            write: state.write_status,
        },
    ]
    .into_iter()
    .enumerate()
    {
        runtime
            .apply_effect(RuntimeEffect {
                operation_id: OperationId::new(format!("failover-initial-{sequence}")),
                sequence: u64::try_from(sequence + 1).unwrap(),
                action,
            })
            .await
            .unwrap();
    }
    runtime.abort();
}

async fn open_scale_up_failover_owner(
    path: &Path,
    application_path: &Path,
) -> (Arc<SqliteStore>, Arc<PodRuntime>, Arc<CrashState>) {
    let store = Arc::new(SqliteStore::open_existing(path, None).unwrap());
    let state = store.load_state().await.unwrap();
    let application = Arc::new(CrashState::open_with_replication(application_path));
    let runtime = Arc::new(PodRuntime::new(
        state.identity.local_identity.clone(),
        application.clone(),
        store.clone(),
    ));
    let reconstruction = runtime
        .reconstruct(
            OpenMode::Existing,
            state.role,
            state.read_status,
            state.write_status,
            None,
        )
        .await;
    if let Err(error) = reconstruction {
        assert!(
            matches!(error, RuntimeError::ReconfigurationPending),
            "unexpected failover owner reconstruction error: {error:?}"
        );
    }
    (store, runtime, application)
}

async fn execute_scale_up_failover_cut(path: &Path, boundary: &str, terminate: bool) {
    eprintln!("scale-up-failover cut={boundary} stage=start");
    let peer_path = scale_up_failover_peer_store_path(path);
    let peer_application_path = scale_up_failover_peer_application_path(path);
    let witness_path = scale_up_failover_witness_store_path(path);
    let witness_application_path = scale_up_failover_witness_application_path(path);
    if !path.is_file() {
        let (source_state, source_command) = scale_up_failover_crash_fixture();
        let (peer_state, _) = scale_up_failover_peer_fixture();
        initialize_scale_up_failover_owner(
            path,
            &crash_application_path(path),
            source_state.clone(),
        )
        .await;
        initialize_scale_up_failover_owner(&peer_path, &peer_application_path, peer_state).await;
        let witness_identity = source_command
            .current_configuration
            .members
            .iter()
            .find(|member| {
                member.role == ReplicaRole::ActiveSecondary
                    && member.identity != source_state.identity.local_identity
                    && member.identity != scale_up_failover_peer_fixture().0.identity.local_identity
            })
            .unwrap()
            .identity
            .clone();
        let mut witness_state = source_state;
        witness_state.identity.local_identity = witness_identity.clone();
        witness_state.identity.pod_uid = PodUid::new(witness_identity.instance_id.as_str());
        witness_state.role = ReplicaRole::ActiveSecondary;
        witness_state.read_status = AccessStatus::Granted;
        witness_state.write_status = AccessStatus::NotPrimary;
        initialize_scale_up_failover_owner(&witness_path, &witness_application_path, witness_state)
            .await;
        let (witness_store, witness_runtime, _) =
            open_scale_up_failover_owner(&witness_path, &witness_application_path).await;
        let witness_authority = AdmittedAuthority {
            local_identity: witness_identity,
            transition_kind: Some(TransitionKind::Failover),
            previous_configuration: source_command.previous_configuration.clone(),
            current_configuration: source_command.current_configuration.clone(),
            switchover_handoff: None,
            secondary_removal: None,
            scale_up: source_command.scale_up_evidence.clone(),
        };
        witness_runtime
            .apply_effect(RuntimeEffect {
                operation_id: OperationId::new("failover-witness-authority"),
                sequence: 1,
                action: RuntimeEffectAction::AdmitAuthority(Box::new(witness_authority.clone())),
            })
            .await
            .unwrap();
        witness_store
            .record_replication_progress(&ReplicationProgress {
                fence: witness_authority.fence(),
                verified_lsn: 9,
            })
            .await
            .unwrap();
        witness_runtime.abort();
    }
    let (peer_store, peer_runtime, _) =
        open_scale_up_failover_owner(&peer_path, &peer_application_path).await;
    let (_, peer_command) = scale_up_failover_peer_fixture();
    Coordinator::new(peer_store.clone(), peer_runtime.clone())
        .ensure_configuration(peer_command)
        .await
        .unwrap();
    eprintln!("scale-up-failover cut={boundary} stage=peer-complete");
    let (_, witness_runtime, _) =
        open_scale_up_failover_owner(&witness_path, &witness_application_path).await;

    let (source_store, source_runtime, source_application) =
        open_scale_up_failover_owner(path, &crash_application_path(path)).await;
    let (_, source_command) = scale_up_failover_crash_fixture();
    let executor = Arc::new(ScaleUpFailoverRuntime {
        runtime: source_runtime.clone(),
        peers: vec![peer_runtime.clone(), witness_runtime],
        acknowledgement_sent: std::sync::atomic::AtomicBool::new(false),
        durable_operation: scale_up_failover_operation(9),
    });
    let exit_code = if boundary == "authority-after" {
        194
    } else {
        195
    };
    let store = Arc::new(ScaleUpProductionCutStore {
        inner: source_store.clone(),
        cut: terminate.then(|| boundary.to_owned()),
        exit_code,
    });
    let completed = Coordinator::new(store, executor)
        .ensure_configuration(source_command.clone())
        .await
        .unwrap();
    eprintln!("scale-up-failover cut={boundary} stage=source-complete");
    assert_eq!(completed.command, source_command);
    let source = source_store.load_state().await.unwrap();
    assert!(source.reconfiguration.is_none() && source.pending_effect.is_none());
    assert_eq!(source.role, ReplicaRole::Primary);
    assert_eq!(source.read_status, AccessStatus::Granted);
    assert_eq!(source.write_status, AccessStatus::Granted);
    assert_eq!(
        source_application.state.lock().unwrap().operations,
        (1..=9)
            .map(|lsn| {
                let operation = scale_up_failover_operation(lsn);
                (lsn, operation.data.to_vec())
            })
            .collect()
    );
}

async fn execute_real_active_secondary_cut(path: &Path, after: bool, terminate: bool) {
    let (_, command, build) = scale_up_crash_fixture(false);
    let evidence = command.scale_up_evidence.clone().unwrap();
    let intent = evidence.intent().clone();
    let candidate = intent.target.clone();
    if !path.is_file() {
        ensure_scale_up_store_cut(path, "source-progress-persistence", false, false).await;
    }
    let candidate_path = scale_up_candidate_store_path(path);
    let store = Arc::new(SqliteStore::open_existing(&candidate_path, None).unwrap());
    let durable = store.load_state().await.unwrap();
    let application = Arc::new(CrashState::open(
        scale_up_candidate_root(path).join("application.json"),
    ));
    let seed = seeded_operation();
    assert!(
        application.verify_applied(&seed).await.unwrap(),
        "ActiveSecondary recovery must observe actual copied bytes before replay"
    );
    assert_eq!(
        application.durable_progress().await.unwrap().committed_lsn,
        intent.catch_up_boundary_lsn
    );
    let runtime = Arc::new(PodRuntime::new(
        candidate,
        application.clone(),
        store.clone(),
    ));
    runtime
        .reconstruct(
            OpenMode::Existing,
            durable.role,
            durable.read_status,
            durable.write_status,
            None,
        )
        .await
        .unwrap();
    let authority = AdmittedAuthority {
        local_identity: intent.target.clone(),
        transition_kind: Some(TransitionKind::ScaleUp),
        previous_configuration: Some(intent.previous_configuration.clone()),
        current_configuration: intent.current_configuration.clone(),
        switchover_handoff: None,
        secondary_removal: None,
        scale_up: Some(evidence),
    };
    if store.load().await.unwrap().as_ref() != Some(&authority) {
        let state = store.load_state().await.unwrap();
        RuntimeAdapter::new(store.clone(), runtime.clone())
            .execute(RuntimeEffect {
                operation_id: OperationId::new("active-secondary-admit-scale-up"),
                sequence: state.next_effect_sequence,
                action: RuntimeEffectAction::AdmitAuthority(Box::new(authority)),
            })
            .await
            .unwrap();
    }
    assert_eq!(
        store
            .load_build_progress(&build.build_id)
            .await
            .unwrap()
            .expect("actual candidate copy progress")
            .durable_lsn,
        intent.catch_up_boundary_lsn
    );
    let effect = RuntimeEffect {
        operation_id: OperationId::new("scale-up-active-secondary-role"),
        sequence: store.load_state().await.unwrap().next_effect_sequence,
        action: RuntimeEffectAction::ChangeRole(ReplicaRole::ActiveSecondary),
    };
    let executor = Arc::new(CrashAfterRealEffect {
        runtime: runtime.clone(),
        boundary: terminate.then(|| {
            if after {
                "after-active-secondary-role"
            } else {
                "before-active-secondary-role"
            }
            .to_string()
        }),
        exit_code: if after { 193 } else { 192 },
    });
    RuntimeAdapter::new(store.clone(), executor)
        .execute(effect)
        .await
        .unwrap();
    let access = RuntimeEffect {
        operation_id: OperationId::new("scale-up-active-secondary-access"),
        sequence: store.load_state().await.unwrap().next_effect_sequence,
        action: RuntimeEffectAction::SetAccessStatus {
            read: AccessStatus::Granted,
            write: AccessStatus::NotPrimary,
        },
    };
    RuntimeAdapter::new(store.clone(), runtime.clone())
        .execute(access)
        .await
        .unwrap();
    let durable = store.load_state().await.unwrap();
    assert_eq!(durable.role, ReplicaRole::ActiveSecondary);
    assert_eq!(durable.write_status, AccessStatus::NotPrimary);
    assert!(durable.pending_effect.is_none());
    assert_eq!(
        application.durable_progress().await.unwrap().committed_lsn,
        1
    );
}

fn real_configuration_command() -> EnsureConfiguration {
    let identity = single_storage_identity().local_identity;
    let policy = EffectivePolicy::fixed(1, 30).unwrap();
    let current_configuration = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        identity.replica_id,
        vec![ConfigurationMember {
            identity: identity.clone(),
            role: ReplicaRole::Primary,
        }],
        policy.write_quorum,
    );
    EnsureConfiguration {
        previous_policy: None,
        secondary_removal_evidence: None,
        scale_up_evidence: None,
        operation_id: OperationId::new("real-runtime-configuration"),
        previous_configuration: None,
        current_configuration,
        previous_epoch: None,
        current_epoch: Epoch::new(0, 1),
        effective_policy: policy,
        local_replica_id: identity.replica_id,
        expected_instance_id: identity.instance_id,
        expected_agent_generation: identity.agent_generation,
        transition_kind: TransitionKind::Bootstrap,
        failover_safe_lsn: None,
        primary_write_status: AccessStatus::ReconfigurationPending,
        current_only: false,
        retire_build_ids: Vec::new(),
        switchover_handoff: None,
        retire_switchover_preparation_ids: Vec::new(),
    }
}

fn grant_access_command() -> EnsureConfiguration {
    EnsureConfiguration {
        operation_id: OperationId::new("real-runtime-grant"),
        primary_write_status: AccessStatus::Granted,
        ..real_configuration_command()
    }
}

fn crash_application_path(database_path: &Path) -> PathBuf {
    database_path
        .parent()
        .and_then(Path::parent)
        .expect("agent database is under the data root")
        .join("crash-application.json")
}

fn real_runtime(
    store: Arc<SqliteStore>,
    application_path: &Path,
) -> (Arc<PodRuntime>, Arc<CrashState>) {
    let application = Arc::new(CrashState::open(application_path));
    (
        Arc::new(PodRuntime::new(
            single_storage_identity().local_identity,
            application.clone(),
            store,
        )),
        application,
    )
}

fn switchover_storage_identity() -> StorageIdentity {
    StorageIdentity {
        effective_policy: EffectivePolicy::fixed(2, 30).unwrap(),
        ..storage_identity()
    }
}

fn switchover_configuration() -> ConfigurationDescriptor {
    let source = switchover_storage_identity().local_identity;
    ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        source.replica_id,
        vec![
            ConfigurationMember {
                identity: source,
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: ReplicaIdentity {
                    replica_id: ReplicaId::new(2),
                    instance_id: ReplicaInstanceId::new("pod-2"),
                    agent_generation: AgentGeneration::new("generation-2"),
                },
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    )
}

fn switchover_prepare_command() -> PrepareSwitchover {
    let source = switchover_storage_identity().local_identity;
    let configuration = switchover_configuration();
    PrepareSwitchover {
        preparation_generation: 1,
        operation_id: kuberic_protocol::types::derive_switchover_preparation_operation_id(
            &switchover_storage_identity().resource_uid,
            &SwitchoverRequestId::new("real-switchover-request"),
            1,
            &configuration.configuration_id,
            &source,
            &configuration.members[1].identity,
        ),
        request_id: SwitchoverRequestId::new("real-switchover-request"),
        local_replica_id: source.replica_id,
        expected_instance_id: source.instance_id.clone(),
        expected_agent_generation: source.agent_generation.clone(),
        source,
        target: configuration.members[1].identity.clone(),
        current_configuration: configuration,
    }
}

fn switchover_runtime_effect(command: &PrepareSwitchover, sequence: u64) -> RuntimeEffect {
    RuntimeEffect {
        operation_id: command.operation_id.clone(),
        sequence,
        action: RuntimeEffectAction::PrepareSwitchover {
            preparation_generation: 1,
            request_id: command.request_id.clone(),
            source: command.source.clone(),
            target: command.target.clone(),
            starting_configuration_id: command.current_configuration.configuration_id.clone(),
            starting_epoch: command.current_configuration.epoch,
        },
    }
}

fn switchover_real_runtime(
    store: Arc<SqliteStore>,
    application_path: &Path,
) -> (Arc<PodRuntime>, Arc<CrashState>) {
    let application = Arc::new(CrashState::open(application_path));
    (
        Arc::new(PodRuntime::new(
            switchover_storage_identity().local_identity,
            application.clone(),
            store,
        )),
        application,
    )
}

fn seeded_operation() -> Operation {
    Operation {
        lsn: 1,
        committed_lsn: 1,
        data: Bytes::from_static(b"acknowledged-before-crash"),
    }
}

fn result() -> RuntimeEffectResult {
    RuntimeEffectResult {
        operation_id: OperationId::new("effect-1"),
        sequence: 1,
        postcondition: RuntimePostcondition {
            prepared_secondary_removal: None,
            retired_authority: None,
            accepted_secondary_removal: None,
            open: true,
            role: ReplicaRole::None,
            role_transition: None,
            read_status: AccessStatus::NotPrimary,
            write_status: AccessStatus::NotPrimary,
            authority: None,
            current_progress: 0,
            verified_replication_lsn: None,
            committed_lsn: 0,
            current_configuration_quorum_progress: 0,
            catch_up_boundary: None,
            catch_up_complete: false,
            builds: Vec::new(),
        },
    }
}

fn switchover_effect() -> RuntimeEffect {
    let authority = switchover_authority();
    RuntimeEffect {
        operation_id: OperationId::new("prepare-switchover-1"),
        sequence: 1,
        action: RuntimeEffectAction::PrepareSwitchover {
            preparation_generation: 1,
            request_id: SwitchoverRequestId::new("request-1"),
            source: storage_identity().local_identity,
            target: ReplicaIdentity {
                replica_id: ReplicaId::new(2),
                instance_id: ReplicaInstanceId::new("pod-2"),
                agent_generation: AgentGeneration::new("generation-2"),
            },
            starting_configuration_id: authority.current_configuration.configuration_id,
            starting_epoch: authority.current_configuration.epoch,
        },
    }
}

fn switchover_result() -> RuntimeEffectResult {
    let authority = switchover_authority();
    RuntimeEffectResult {
        operation_id: OperationId::new("prepare-switchover-1"),
        sequence: 1,
        postcondition: RuntimePostcondition {
            prepared_secondary_removal: None,
            retired_authority: None,
            accepted_secondary_removal: None,
            open: true,
            role: ReplicaRole::Primary,
            role_transition: None,
            read_status: AccessStatus::Granted,
            write_status: AccessStatus::ReconfigurationPending,
            authority: Some(authority),
            current_progress: 9,
            verified_replication_lsn: Some(9),
            committed_lsn: 7,
            current_configuration_quorum_progress: 9,
            catch_up_boundary: None,
            catch_up_complete: true,
            builds: Vec::new(),
        },
    }
}

fn switchover_authority() -> AdmittedAuthority {
    let source = storage_identity().local_identity;
    let target = ReplicaIdentity {
        replica_id: ReplicaId::new(2),
        instance_id: ReplicaInstanceId::new("pod-2"),
        agent_generation: AgentGeneration::new("generation-2"),
    };
    AdmittedAuthority {
        scale_up: None,
        secondary_removal: None,
        local_identity: source.clone(),
        transition_kind: None,
        previous_configuration: None,
        current_configuration: ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            source.replica_id,
            vec![
                ConfigurationMember {
                    identity: source,
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: target,
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        ),
        switchover_handoff: None,
    }
}

fn snapshot(write_status: AccessStatus) -> RuntimeSnapshot {
    RuntimeSnapshot {
        prepared_secondary_removal: None,
        retired_authority: None,
        accepted_secondary_removal: None,
        identity: storage_identity().local_identity,
        open: false,
        replication_address: None,
        role: ReplicaRole::None,
        role_transition: None,
        read_status: AccessStatus::NotPrimary,
        write_status,
        authority: None,
        current_progress: 0,
        verified_replication_lsn: None,
        committed_lsn: 0,
        current_configuration_quorum_progress: 0,
        catch_up_boundary: None,
        catch_up_complete: false,
        builds: Vec::new(),
    }
}

#[tokio::test]
async fn recovery_reissues_committed_intent_without_reporting_completion() {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let store = SqliteStore::create_authorized(&path, AgentState::new(storage_identity())).unwrap();
    assert_eq!(
        store.begin_effect(&effect()).await.unwrap(),
        BeginEffect::Execute(effect())
    );
    drop(store);

    let reopened = Arc::new(SqliteStore::open_existing(&path, None).unwrap());
    assert_eq!(
        inspect_recovery(reopened.as_ref(), &snapshot(AccessStatus::NotPrimary))
            .await
            .unwrap(),
        RecoveryDecision::Reissue(Box::new(effect()))
    );
    let runtime = Arc::new(FakeRuntime {
        calls: AtomicUsize::new(0),
        result: result(),
    });
    let adapter = RuntimeAdapter::new(reopened.clone(), runtime.clone());
    assert_eq!(
        recover_pending(&adapter, &snapshot(AccessStatus::NotPrimary))
            .await
            .unwrap(),
        Some(result())
    );
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    assert!(
        reopened
            .load_state()
            .await
            .unwrap()
            .pending_effect
            .is_none()
    );
}

#[tokio::test]
async fn recovery_reobserves_effect_applied_before_completion_persistence() {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let store = SqliteStore::create_authorized(&path, AgentState::new(storage_identity())).unwrap();
    store.begin_effect(&effect()).await.unwrap();
    store.mark_effect_applied(&effect()).await.unwrap();
    drop(store);

    let reopened = Arc::new(SqliteStore::open_existing(&path, None).unwrap());
    let runtime = Arc::new(FakeRuntime {
        calls: AtomicUsize::new(0),
        result: result(),
    });
    let adapter = RuntimeAdapter::new(reopened.clone(), runtime.clone());
    let recovered = recover_pending(&adapter, &snapshot(AccessStatus::NotPrimary))
        .await
        .unwrap();
    assert_eq!(recovered, Some(result()));
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        reopened.retained_result().await.unwrap().unwrap().result,
        result()
    );
}

#[tokio::test]
async fn bootstrap_terminal_result_survives_transition_clearing_and_reopen() {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let store = Arc::new(
        SqliteStore::create_authorized(&path, AgentState::new(storage_identity())).unwrap(),
    );
    let runtime = Arc::new(FakeRuntime {
        calls: AtomicUsize::new(0),
        result: result(),
    });
    let adapter = RuntimeAdapter::new(store.clone(), runtime);
    adapter.execute(effect()).await.unwrap();

    store
        .set_reconfiguration(Some("transition-stage".into()))
        .await
        .unwrap();
    store.clear_reconfiguration().await.unwrap();
    drop(adapter);
    drop(store);

    let reopened = SqliteStore::open_existing(&path, None).unwrap();
    let state = reopened.load_state().await.unwrap();
    assert!(state.reconfiguration_data.is_none());
    assert_eq!(state.retained_result.unwrap().result, result());
}

#[tokio::test]
async fn recovery_refuses_a_runtime_that_did_not_start_write_closed() {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let store = SqliteStore::create_authorized(&path, AgentState::new(storage_identity())).unwrap();
    assert!(matches!(
        inspect_recovery(&store, &snapshot(AccessStatus::Granted)).await,
        Err(AgentError::EffectConflict(_))
    ));
}

#[tokio::test]
async fn definitively_cancelled_build_effect_releases_durable_intent() {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let store = Arc::new(
        SqliteStore::create_authorized(&path, AgentState::new(storage_identity())).unwrap(),
    );
    let effect = RuntimeEffect {
        operation_id: OperationId::new("cancelled-build"),
        sequence: 1,
        action: RuntimeEffectAction::BuildReplica {
            build_id: OperationId::new("cancelled-build"),
            target: ReplicaIdentity {
                replica_id: ReplicaId::new(2),
                instance_id: ReplicaInstanceId::new("lost-target"),
                agent_generation: AgentGeneration::new("lost-generation"),
            },
            replication_address: String::new(),
        },
    };
    assert!(matches!(
        RuntimeAdapter::new(store.clone(), Arc::new(CancelledRuntime))
            .execute(effect)
            .await,
        Err(AgentError::Runtime(RuntimeError::ReplicaRemoved(2)))
    ));
    assert!(store.load_state().await.unwrap().pending_effect.is_none());
}

#[test]
fn process_session_changes_without_changing_durable_generation() {
    let first = kuberic_agent::session::ProcessSession::new();
    let second = kuberic_agent::session::ProcessSession::new();
    assert_ne!(first.id(), second.id());
    assert_eq!(first.next_report_sequence(), 1);
    assert_eq!(first.next_report_sequence(), 2);
    assert_eq!(
        storage_identity().local_identity.agent_generation,
        AgentGeneration::new("generation-1")
    );
    assert_eq!(Epoch::default(), Epoch::new(0, 0));
}

#[test]
fn sqlite_commits_survive_process_termination_without_destructors() {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let output = Command::new(env::current_exe().unwrap())
        .args(["--ignored", "--exact", "crash_writer_process"])
        .env("KUBERIC_CRASH_WRITER_PATH", &path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let reopened = SqliteStore::open_existing(&path, Some(&storage_identity())).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let state = runtime.block_on(reopened.load_state()).unwrap();
    assert_eq!(state.pending_effect.unwrap().effect, effect());
}

#[test]
fn configuration_boundaries_survive_process_termination_without_destructors() {
    for boundary in [
        "pending-effect",
        "effect-complete-before-stage-advance",
        "enclosing-command",
        "terminal-command",
    ] {
        let directory = tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(directory.path());
        let output = Command::new(env::current_exe().unwrap())
            .args(["--ignored", "--exact", "crash_boundary_writer_process"])
            .env("KUBERIC_CRASH_WRITER_PATH", &path)
            .env("KUBERIC_CRASH_BOUNDARY", boundary)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed at {boundary}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let reopened = SqliteStore::open_existing(&path, Some(&storage_identity())).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let state = runtime.block_on(reopened.load_state()).unwrap();
        match boundary {
            "pending-effect" => {
                assert_eq!(
                    state.pending_effect.unwrap().stage,
                    EffectStage::IntentCommitted
                );
            }

            "effect-complete-before-stage-advance" => {
                assert_eq!(
                    state.reconfiguration.unwrap().stage,
                    CoordinatorStage::AdmitAuthority
                );
                assert_eq!(state.retained_result.unwrap().result, result());
            }
            "enclosing-command" => {
                assert_eq!(
                    state.reconfiguration.unwrap().stage,
                    CoordinatorStage::AdmitAuthority
                );
                assert!(state.retained_command.is_none());
            }
            "terminal-command" => {
                assert!(state.reconfiguration.is_none());
                assert_eq!(
                    state.retained_command.unwrap().command,
                    configuration_command()
                );
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn switchover_recovery_boundaries_survive_process_termination() {
    for boundary in [
        "demotion",
        "promotion",
        "compensation-allocation",
        "compensation-admission",
        "compensation-completion",
        "retirement-before",
        "retirement-after",
        "restoration-before",
        "restoration-after",
        "restoration-unobserved-before",
        "restoration-unobserved-after",
    ] {
        let directory = tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(directory.path());
        let output = Command::new(env::current_exe().unwrap())
            .args(["--ignored", "--exact", "switchover_recovery_writer_process"])
            .env("KUBERIC_RECOVERY_PATH", &path)
            .env("KUBERIC_RECOVERY_BOUNDARY", boundary)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{boundary}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let store = Arc::new(SqliteStore::open_existing(&path, None).unwrap());
            let state = store.load_state().await.unwrap();
            let (_, command) = switchover_recovery_fixture(boundary);
            assert_ne!(state.write_status, AccessStatus::Granted);
            if boundary.ends_with("-after") || boundary == "compensation-completion" {
                assert!(state.prepared_switchover.is_none());
                assert_eq!(state.retired_switchover, command.switchover_handoff);
            } else if boundary != "promotion" {
                assert_eq!(state.prepared_switchover, command.switchover_handoff);
            }
            let runtime = Arc::new(SwitchoverRecoveryRuntime::new(&state, None));
            let coordinator = Coordinator::new(store.clone(), runtime);
            let completed = coordinator
                .ensure_configuration(command.clone())
                .await
                .unwrap();
            assert_eq!(completed.command, command);
            assert_eq!(
                coordinator
                    .ensure_configuration(command.clone())
                    .await
                    .unwrap(),
                completed
            );
            let state = store.load_state().await.unwrap();
            assert!(state.pending_effect.is_none() && state.reconfiguration.is_none());
            assert_eq!(state.highest_epoch, command.current_epoch);
            assert_ne!(state.write_status, AccessStatus::Granted);
            if !command.retire_switchover_preparation_ids.is_empty() {
                assert!(state.prepared_switchover.is_none());
                assert_eq!(state.retired_switchover, command.switchover_handoff);
                assert_eq!(
                    state
                        .preparation_retirement
                        .as_ref()
                        .map(|retired| retired.generation),
                    command
                        .retire_switchover_preparation_ids
                        .first()
                        .map(|id| id.generation)
                );
                let mut writable = state.clone();
                writable.write_status = AccessStatus::Granted;
                assert!(
                    kuberic_agent::command::admit_switchover_preparation(
                        &switchover_prepare_command(),
                        &writable
                    )
                    .is_err()
                );
            }
        });
    }
}

#[test]
fn scale_up_exact_cut_matrix_survives_real_process_restart() {
    let store_cuts = [
        "store-initialization",
        "build-authority-admission",
        "snapshot-boundary-persistence",
        "source-progress-persistence",
        "target-progress-persistence",
    ];
    for cut in store_cuts {
        for after in [false, true] {
            eprintln!(
                "scale-up-crash-cut cut={cut} side={}",
                if after { "after" } else { "before" }
            );
            let directory = tempdir().unwrap();
            let path = SqliteStore::metadata_database_path(directory.path());
            let output = Command::new(env::current_exe().unwrap())
                .args(["--ignored", "--exact", "scale_up_store_cut_writer_process"])
                .env("KUBERIC_SCALE_UP_STORE_CUT_PATH", &path)
                .env("KUBERIC_SCALE_UP_STORE_CUT", cut)
                .env(
                    "KUBERIC_SCALE_UP_STORE_CUT_SIDE",
                    if after { "after" } else { "before" },
                )
                .output()
                .unwrap();
            let expected_exit = scale_up_store_cut_exit_code(cut, after);
            assert_exact_scale_up_exit(
                output.status,
                expected_exit,
                &format!(
                    "{cut} {}: {}",
                    if after { "after" } else { "before" },
                    String::from_utf8_lossy(&output.stderr)
                ),
            );
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let (_, _, build) = scale_up_crash_fixture(false);
                match cut {
                    "store-initialization" if !after => {
                        assert!(
                            !path.exists(),
                            "before initialization unexpectedly created durable metadata"
                        );
                    }
                    "store-initialization" => {
                        let candidate_path = scale_up_candidate_store_path(&path);
                        let interrupted =
                            SqliteStore::open_existing(&candidate_path, None).unwrap();
                        assert_eq!(
                            interrupted.identity().await.unwrap().schema_version,
                            SCHEMA_VERSION
                        );
                    }
                    "build-authority-admission" => {
                        let candidate_path = scale_up_candidate_store_path(&path);
                        let interrupted =
                            SqliteStore::open_existing(&candidate_path, None).unwrap();
                        assert_eq!(
                            interrupted.load_build(&build.build_id).await.unwrap(),
                            after.then(|| build.clone()),
                            "{cut} side={} interrupted authority state",
                            if after { "after" } else { "before" }
                        );
                        assert!(
                            interrupted
                                .load_build_progress(&build.build_id)
                                .await
                                .unwrap()
                                .is_none(),
                            "authority cut fabricated build progress"
                        );
                    }
                    "snapshot-boundary-persistence" | "source-progress-persistence" => {
                        let interrupted = SqliteStore::open_existing(&path, None).unwrap();
                        assert_eq!(
                            interrupted.load_build(&build.build_id).await.unwrap(),
                            if cut == "snapshot-boundary-persistence" && !after {
                                None
                            } else {
                                Some(build.clone())
                            }
                        );
                        let progress = interrupted
                            .load_build_progress(&build.build_id)
                            .await
                            .unwrap();
                        if after {
                            let progress = progress.expect("after side persisted progress");
                            assert_eq!(progress.catch_up_boundary_lsn, Some(1));
                            assert_eq!(
                                (progress.durable_lsn, progress.completed),
                                if cut == "source-progress-persistence" {
                                    (1, true)
                                } else {
                                    (0, false)
                                }
                            );
                        } else {
                            assert!(
                                progress.as_ref().is_none_or(|progress| {
                                    progress.catch_up_boundary_lsn.is_none()
                                        || (cut == "source-progress-persistence"
                                            && !progress.completed)
                                }),
                                "{cut} before side crossed the cut operation: {progress:?}"
                            );
                        }
                    }
                    "target-progress-persistence" => {
                        let candidate_path = scale_up_candidate_store_path(&path);
                        let interrupted =
                            SqliteStore::open_existing(&candidate_path, None).unwrap();
                        let progress = interrupted
                            .load_build_progress(&build.build_id)
                            .await
                            .unwrap()
                            .expect("target snapshot progress");
                        assert_eq!(progress.completed, after);
                        assert_eq!(progress.catch_up_boundary_lsn, after.then_some(1));
                        let application = CrashState::open(
                            scale_up_candidate_root(&path).join("application.json"),
                        );
                        assert_eq!(
                            application.durable_progress().await.unwrap().committed_lsn,
                            i64::from(after)
                        );
                        assert!(
                            application
                                .verify_applied(&seeded_operation())
                                .await
                                .unwrap(),
                            "snapshot bytes must precede the final completion marker"
                        );
                    }
                    _ => unreachable!(),
                }

                // Recovery starts only after the parent has inspected the
                // interrupted durable side of the cut.
                ensure_scale_up_store_cut(&path, cut, after, false).await;
                let store = SqliteStore::open_existing(&path, None).unwrap();
                match cut {
                    "store-initialization" => {
                        assert_eq!(
                            store.identity().await.unwrap().schema_version,
                            SCHEMA_VERSION
                        );
                    }
                    "build-authority-admission" => {
                        let candidate =
                            SqliteStore::open_existing(scale_up_candidate_store_path(&path), None)
                                .unwrap();
                        assert_eq!(
                            candidate.load_build(&build.build_id).await.unwrap(),
                            Some(build)
                        );
                    }
                    _ => {
                        let source_progress = store
                            .load_build_progress(&build.build_id)
                            .await
                            .unwrap()
                            .expect("durable source build progress");
                        let candidate =
                            SqliteStore::open_existing(scale_up_candidate_store_path(&path), None)
                                .unwrap();
                        let target_progress = candidate
                            .load_build_progress(&build.build_id)
                            .await
                            .unwrap()
                            .expect("durable target build progress");
                        assert_eq!(source_progress.authority, build);
                        assert_eq!(target_progress.authority, build);
                        assert_eq!(source_progress.catch_up_boundary_lsn, Some(1));
                        assert_eq!(target_progress.catch_up_boundary_lsn, Some(1));
                        assert!(
                            source_progress.completed && target_progress.completed,
                            "{cut}: actual copy delivery did not complete on recovery"
                        );
                    }
                }
            });
        }
    }

    let configuration_cuts = [
        "pc-cc-authority-before",
        "pc-cc-authority-after",
        "pc-cc-access-before",
        "pc-cc-access-after",
        "current-only-authority-before",
        "current-only-authority-after",
        "current-only-access-before",
        "current-only-access-after",
        "build-retirement-before",
        "build-retirement-after",
        "completion-before",
        "completion-after",
    ];
    for cut in configuration_cuts {
        eprintln!("scale-up-crash-cut cut={cut}");
        let directory = tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(directory.path());
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "scale_up_configuration_cut_writer_process",
            ])
            .env("KUBERIC_SCALE_UP_CONFIGURATION_CUT_PATH", &path)
            .env("KUBERIC_SCALE_UP_CONFIGURATION_CUT", cut)
            .output()
            .unwrap();
        let expected_exit = scale_up_configuration_cut_exit_code(cut);
        assert_exact_scale_up_exit(
            output.status,
            expected_exit,
            &format!("{cut}: {}", String::from_utf8_lossy(&output.stderr)),
        );
        let interrupted = SqliteStore::open_existing(&path, None).unwrap();
        let interrupted_state = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(interrupted.load_state())
            .unwrap();
        assert_eq!(
            interrupted_state.write_status,
            AccessStatus::Granted,
            "{cut}: same-primary durable cut intentionally closed healthy writes"
        );
        let expected_stage = match cut {
            "pc-cc-authority-before"
            | "current-only-authority-before"
            | "pc-cc-authority-after"
            | "current-only-authority-after" => Some(CoordinatorStage::AdmitAuthority),
            "pc-cc-access-before"
            | "current-only-access-before"
            | "pc-cc-access-after"
            | "current-only-access-after" => Some(CoordinatorStage::Activate),
            "build-retirement-before" | "build-retirement-after" => {
                Some(CoordinatorStage::RetireBuild)
            }
            "completion-before" => Some(CoordinatorStage::Complete),
            "completion-after" => None,
            _ => unreachable!(),
        };
        assert_eq!(
            interrupted_state
                .reconfiguration
                .as_ref()
                .map(|record| record.stage),
            expected_stage,
            "{cut}: interrupted coordinator stage"
        );
        assert!(
            interrupted_state.pending_effect.is_none(),
            "{cut}: a completed real runtime effect left an ambiguous pending journal"
        );
        let (_, command, build) = scale_up_crash_fixture(
            cut.starts_with("current-only")
                || cut.starts_with("build-retirement")
                || cut.starts_with("completion"),
        );
        let candidate_path = scale_up_candidate_store_path(&path);
        let interrupted_candidate = SqliteStore::open_existing(&candidate_path, None).unwrap();
        let inspection_runtime = tokio::runtime::Runtime::new().unwrap();
        let (source_authority, candidate_authority, candidate_state) =
            inspection_runtime.block_on(async {
                let source_authority = interrupted
                    .load()
                    .await
                    .unwrap()
                    .expect("source replica_authority row");
                let candidate_authority = interrupted_candidate
                    .load()
                    .await
                    .unwrap()
                    .expect("candidate replica_authority row");
                let candidate_state = interrupted_candidate.load_state().await.unwrap();
                let source_progress = interrupted
                    .load_replication_progress(&source_authority.fence())
                    .await
                    .unwrap()
                    .or(interrupted
                        .load_configuration_progress(
                            source_authority.current_configuration.epoch,
                            &source_authority.current_configuration.configuration_id,
                        )
                        .await
                        .unwrap())
                    .expect("source durable replication progress");
                let candidate_progress = interrupted_candidate
                    .load_replication_progress(&candidate_authority.fence())
                    .await
                    .unwrap()
                    .or(interrupted_candidate
                        .load_configuration_progress(
                            candidate_authority.current_configuration.epoch,
                            &candidate_authority.current_configuration.configuration_id,
                        )
                        .await
                        .unwrap())
                    .expect("candidate durable replication progress");
                assert!(
                    source_progress.verified_lsn >= 1,
                    "{cut}: source durable progress lost acknowledged application bytes"
                );
                assert!(
                    candidate_progress.verified_lsn >= 1,
                    "{cut}: candidate durable progress lost copied application bytes"
                );
                (source_authority, candidate_authority, candidate_state)
            });
        let source_authority_bytes =
            replica_authority_table_bytes(&path).expect("source replica_authority table bytes");
        let candidate_authority_bytes = replica_authority_table_bytes(&candidate_path)
            .expect("candidate replica_authority table bytes");
        assert_eq!(
            serde_json::from_slice::<AdmittedAuthority>(&source_authority_bytes).unwrap(),
            source_authority,
            "{cut}: source table/store authority disagreement"
        );
        assert_eq!(
            serde_json::from_slice::<AdmittedAuthority>(&candidate_authority_bytes).unwrap(),
            candidate_authority,
            "{cut}: candidate table/store authority disagreement"
        );
        assert_eq!(
            candidate_authority.current_configuration, command.current_configuration,
            "{cut}: candidate did not execute the corresponding configuration"
        );
        assert_eq!(
            candidate_authority.scale_up, command.scale_up_evidence,
            "{cut}: candidate authority lost exact scale-up evidence"
        );
        assert_eq!(candidate_state.role, ReplicaRole::ActiveSecondary, "{cut}");
        assert_eq!(candidate_state.read_status, AccessStatus::Granted, "{cut}");
        assert_eq!(
            candidate_state.write_status,
            AccessStatus::NotPrimary,
            "{cut}"
        );
        assert_eq!(
            durable_application_history_bytes(&crash_application_path(&path)),
            durable_application_history_bytes(
                &scale_up_candidate_root(&path).join("application.json")
            ),
            "{cut}: source and candidate durable application bytes diverged before replay"
        );
        let source_crossed_authority = !cut.ends_with("authority-before");
        assert_eq!(
            source_authority.previous_configuration,
            if source_crossed_authority {
                command.previous_configuration.clone()
            } else if command.current_only {
                Some(
                    command
                        .scale_up_evidence
                        .as_deref()
                        .unwrap()
                        .intent()
                        .previous_configuration
                        .clone(),
                )
            } else {
                None
            },
            "{cut}: source replica_authority table crossed the wrong side of the cut"
        );
        if cut.ends_with("-after") {
            match cut {
                "pc-cc-authority-after" | "current-only-authority-after" => {
                    let authority = tokio::runtime::Runtime::new()
                        .unwrap()
                        .block_on(interrupted.load())
                        .expect("durable effect-side authority");
                    assert_eq!(
                        authority
                            .as_ref()
                            .map(|authority| &authority.current_configuration),
                        Some(&command.current_configuration),
                        "{cut}: current-only authority effect was not durable"
                    );
                    assert_eq!(
                        authority.and_then(|authority| authority.scale_up),
                        command.scale_up_evidence,
                        "{cut}: durable authority lost scale-up evidence"
                    );
                }
                "pc-cc-access-after" | "current-only-access-after" => {
                    assert_eq!(interrupted_state.read_status, AccessStatus::Granted);
                    assert_eq!(interrupted_state.write_status, AccessStatus::Granted);
                    assert_eq!(
                        interrupted_state.current_configuration,
                        Some(command.current_configuration.clone())
                    );
                }
                "build-retirement-after" => {
                    assert!(interrupted_state.retired_builds.contains(&build.build_id));
                    assert!(
                        tokio::runtime::Runtime::new()
                            .unwrap()
                            .block_on(interrupted.load_build(&build.build_id))
                            .unwrap()
                            .is_none(),
                        "{cut}: runtime build authority survived retirement"
                    );
                }
                "completion-after" => {
                    assert!(interrupted_state.reconfiguration.is_none());
                    assert_eq!(
                        interrupted_state
                            .retained_command
                            .as_ref()
                            .map(|retained| &retained.command),
                        Some(&command)
                    );
                }
                _ => unreachable!(),
            }
        }
        if cut == "completion-after" {
            assert!(
                interrupted_state.retained_command.is_some(),
                "completion-after did not persist terminal command evidence"
            );
        }
        let interrupted_application = CrashState::open(crash_application_path(&path));
        assert!(
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(interrupted_application.verify_applied(&seeded_operation()))
                .unwrap(),
            "{cut}: interrupted application lost initial durable bytes"
        );
        std::thread::Builder::new()
            .name(format!("scale-up-configuration-recovery-{cut}"))
            .stack_size(16 * 1024 * 1024)
            .spawn({
                let path = path.clone();
                let cut = cut.to_owned();
                move || {
                    tokio::runtime::Runtime::new()
                        .unwrap()
                        .block_on(execute_scale_up_configuration_cut(&path, &cut, false));
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    for after in [false, true] {
        eprintln!(
            "scale-up-crash-cut cut=active-secondary side={}",
            if after { "after" } else { "before" }
        );
        let directory = tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(directory.path());
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "scale_up_active_secondary_cut_writer_process",
            ])
            .env("KUBERIC_SCALE_UP_ACTIVE_SECONDARY_PATH", &path)
            .env(
                "KUBERIC_SCALE_UP_ACTIVE_SECONDARY_SIDE",
                if after { "after" } else { "before" },
            )
            .output()
            .unwrap();
        assert_exact_scale_up_exit(
            output.status,
            if after { 193 } else { 192 },
            &format!(
                "active-secondary {}: {}",
                if after { "after" } else { "before" },
                String::from_utf8_lossy(&output.stderr)
            ),
        );
        let candidate_path = scale_up_candidate_store_path(&path);
        let interrupted = SqliteStore::open_existing(&candidate_path, None).unwrap();
        let interrupted_state = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(interrupted.load_state())
            .unwrap();
        assert_eq!(interrupted_state.role, ReplicaRole::IdleSecondary);
        assert!(
            interrupted_state.pending_effect.is_some(),
            "ActiveSecondary cut did not persist pending role effect"
        );
        let interrupted_application =
            CrashState::open(scale_up_candidate_root(&path).join("application.json"));
        let application_state = interrupted_application.state.lock().unwrap().clone();
        assert_eq!(
            application_state.last_role,
            Some(if after {
                ReplicaRole::ActiveSecondary
            } else {
                ReplicaRole::IdleSecondary
            }),
            "ActiveSecondary application side was not distinguished before recovery"
        );
        assert_eq!(
            application_state.operations.get(&1).map(Vec::as_slice),
            Some(b"acknowledged-before-crash".as_slice())
        );
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(execute_real_active_secondary_cut(&path, after, false));
    }
}

#[test]
fn scale_up_crash_helpers_reject_disabled_hooks_and_missing_persistence() {
    let mut exact_codes = std::collections::BTreeSet::new();
    for cut in [
        "store-initialization",
        "build-authority-admission",
        "snapshot-boundary-persistence",
        "source-progress-persistence",
        "target-progress-persistence",
    ] {
        for after in [false, true] {
            assert!(exact_codes.insert(scale_up_store_cut_exit_code(cut, after)));
        }
    }
    for cut in [
        "pc-cc-authority-before",
        "pc-cc-authority-after",
        "pc-cc-access-before",
        "pc-cc-access-after",
        "current-only-authority-before",
        "current-only-authority-after",
        "current-only-access-before",
        "current-only-access-after",
        "build-retirement-before",
        "build-retirement-after",
        "completion-before",
        "completion-after",
    ] {
        assert!(exact_codes.insert(scale_up_configuration_cut_exit_code(cut)));
    }
    for code in [192, 193, 194, 195] {
        assert!(exact_codes.insert(code));
    }
    assert_eq!(exact_codes.len(), 26);

    for helper in [
        "scale_up_store_cut_writer_process",
        "scale_up_configuration_cut_writer_process",
        "scale_up_active_secondary_cut_writer_process",
        "scale_up_failover_crash_writer_process",
    ] {
        let output = Command::new(env::current_exe().unwrap())
            .args(["--ignored", "--exact", helper])
            .env_remove("KUBERIC_SCALE_UP_STORE_CUT_PATH")
            .env_remove("KUBERIC_SCALE_UP_STORE_CUT")
            .env_remove("KUBERIC_SCALE_UP_STORE_CUT_SIDE")
            .env_remove("KUBERIC_SCALE_UP_CONFIGURATION_CUT_PATH")
            .env_remove("KUBERIC_SCALE_UP_CONFIGURATION_CUT")
            .env_remove("KUBERIC_SCALE_UP_ACTIVE_SECONDARY_PATH")
            .env_remove("KUBERIC_SCALE_UP_ACTIVE_SECONDARY_SIDE")
            .env_remove("KUBERIC_SCALE_UP_FAILOVER_PATH")
            .env_remove("KUBERIC_SCALE_UP_FAILOVER_BOUNDARY")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(101),
            "{helper} disabled-hook control did not panic conventionally"
        );
        assert!(
            !is_scale_up_cut_exit_code(output.status.code()),
            "{helper} accepted Rust panic code 101 as a crash cut"
        );
        let diagnostic = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            diagnostic.contains("requires parent-provided"),
            "{helper} did not identify the disabled hook: {}",
            diagnostic
        );
    }

    let normal = Command::new(env::current_exe().unwrap())
        .args(["--exact", "no_such_scale_up_crash_helper"])
        .output()
        .unwrap();
    assert_eq!(normal.status.code(), Some(0));
    assert!(
        !is_scale_up_cut_exit_code(normal.status.code()),
        "normal test-harness completion was accepted as a crash cut"
    );

    let directory = tempdir().unwrap();
    let missing = SqliteStore::metadata_database_path(directory.path());
    assert!(
        SqliteStore::open_existing(&missing, None).is_err(),
        "missing durable metadata was reconstructed from expected test state"
    );
}

#[test]
#[ignore = "helper process for scale_up_exact_cut_matrix_survives_real_process_restart"]
fn scale_up_store_cut_writer_process() {
    let (Ok(path), Ok(cut), Ok(side)) = (
        env::var("KUBERIC_SCALE_UP_STORE_CUT_PATH"),
        env::var("KUBERIC_SCALE_UP_STORE_CUT"),
        env::var("KUBERIC_SCALE_UP_STORE_CUT_SIDE"),
    ) else {
        panic!("scale-up store cut helper requires parent-provided path, cut, and side");
    };
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(ensure_scale_up_store_cut(
            Path::new(&path),
            &cut,
            side == "after",
            true,
        ));
}

#[test]
#[ignore = "helper process for scale_up_exact_cut_matrix_survives_real_process_restart"]
fn scale_up_configuration_cut_writer_process() {
    let (Ok(path), Ok(cut)) = (
        env::var("KUBERIC_SCALE_UP_CONFIGURATION_CUT_PATH"),
        env::var("KUBERIC_SCALE_UP_CONFIGURATION_CUT"),
    ) else {
        panic!("scale-up configuration cut helper requires parent-provided path and cut");
    };
    std::thread::Builder::new()
        .name("scale-up-configuration-cut".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(execute_scale_up_configuration_cut(
                    Path::new(&path),
                    &cut,
                    true,
                ));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
#[ignore = "helper process for scale_up_exact_cut_matrix_survives_real_process_restart"]
fn scale_up_active_secondary_cut_writer_process() {
    let (Ok(path), Ok(side)) = (
        env::var("KUBERIC_SCALE_UP_ACTIVE_SECONDARY_PATH"),
        env::var("KUBERIC_SCALE_UP_ACTIVE_SECONDARY_SIDE"),
    ) else {
        panic!("scale-up ActiveSecondary cut helper requires parent-provided path and side");
    };
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(execute_real_active_secondary_cut(
            Path::new(&path),
            side == "after",
            true,
        ));
}

#[test]
fn scale_up_failover_replays_after_authority_and_completion_process_boundaries() {
    {
        let directory = tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(directory.path());
        let (state, command) = scale_up_failover_crash_fixture();
        let store = Arc::new(SqliteStore::create_authorized(&path, state.clone()).unwrap());
        assert!(
            replica_authority_table_bytes(&path).is_none(),
            "negative control unexpectedly had durable runtime authority"
        );
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let application = Arc::new(CrashState::open_with_replication(crash_application_path(
                &path,
            )));
            for lsn in 1..=9 {
                application
                    .apply(scale_up_failover_operation(lsn))
                    .await
                    .unwrap();
            }
            application.commit(9).await.unwrap();
            let runtime = Arc::new(PodRuntime::new(
                state.identity.local_identity,
                application,
                store.clone(),
            ));
            runtime
                .reconstruct(
                    OpenMode::Existing,
                    state.role,
                    state.read_status,
                    state.write_status,
                    None,
                )
                .await
                .unwrap();
            let error = Coordinator::new(store, runtime)
                .ensure_configuration(command)
                .await
                .expect_err("failover was admitted without durable runtime authority");
            assert!(
                matches!(
                    error,
                    AgentError::Runtime(RuntimeError::AuthorityNotAdmitted)
                ),
                "empty-authority negative control failed for the wrong reason: {error:?}"
            );
        });
    }

    for boundary in ["authority-after", "completion-after"] {
        let directory = tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(directory.path());
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "scale_up_failover_crash_writer_process",
            ])
            .env("KUBERIC_SCALE_UP_FAILOVER_PATH", &path)
            .env("KUBERIC_SCALE_UP_FAILOVER_BOUNDARY", boundary)
            .output()
            .unwrap();
        assert_exact_scale_up_exit(
            output.status,
            if boundary == "authority-after" {
                194
            } else {
                195
            },
            &format!("{boundary}: {}", String::from_utf8_lossy(&output.stderr)),
        );
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let store = Arc::new(SqliteStore::open_existing(&path, None).unwrap());
            let peer_path = scale_up_failover_peer_store_path(&path);
            let peer_store = Arc::new(SqliteStore::open_existing(&peer_path, None).unwrap());
            let (_, command) = scale_up_failover_crash_fixture();
            let durable = store.load_state().await.unwrap();
            let authority = store.load().await.unwrap().expect("source authority");
            let peer_authority = peer_store.load().await.unwrap().expect("peer authority");
            assert_eq!(
                serde_json::from_slice::<AdmittedAuthority>(
                    &replica_authority_table_bytes(&path).unwrap()
                )
                .unwrap(),
                authority
            );
            assert_eq!(
                serde_json::from_slice::<AdmittedAuthority>(
                    &replica_authority_table_bytes(&peer_path).unwrap()
                )
                .unwrap(),
                peer_authority
            );
            assert_eq!(
                authority.current_configuration,
                command.current_configuration
            );
            assert_eq!(authority.scale_up, command.scale_up_evidence);
            assert_eq!(
                durable_application_history_bytes(&crash_application_path(&path)),
                durable_application_history_bytes(&scale_up_failover_peer_application_path(&path)),
                "{boundary}: failover owners disagreed on byte-exact application history"
            );
            let source_progress = store
                .load_replication_progress(&authority.fence())
                .await
                .unwrap()
                .expect("source failover progress");
            let peer_progress = peer_store
                .load_replication_progress(&peer_authority.fence())
                .await
                .unwrap()
                .expect("peer failover progress");
            assert_eq!(
                source_progress.verified_lsn,
                if boundary == "authority-after" { 0 } else { 9 },
                "{boundary}: source durable failover prefix crossed the wrong side of the cut"
            );
            assert_eq!(peer_progress.verified_lsn, 9);
            let peer_state = peer_store.load_state().await.unwrap();
            assert_eq!(peer_state.role, ReplicaRole::ActiveSecondary);
            assert_eq!(peer_state.read_status, AccessStatus::Granted);
            assert_eq!(peer_state.write_status, AccessStatus::NotPrimary);
            if boundary == "authority-after" {
                assert!(durable.reconfiguration.is_some());
                assert!(durable.pending_effect.is_none());
                assert_eq!(
                    durable.current_configuration,
                    Some(command.current_configuration.clone())
                );
                assert_eq!(durable.scale_up_evidence, command.scale_up_evidence);
            } else {
                assert!(durable.reconfiguration.is_none());
                assert_eq!(
                    durable
                        .retained_command
                        .as_ref()
                        .map(|retained| &retained.command),
                    Some(&command)
                );
            }
        });
        std::thread::Builder::new()
            .name(format!("scale-up-failover-recovery-{boundary}"))
            .stack_size(16 * 1024 * 1024)
            .spawn({
                let path = path.clone();
                let boundary = boundary.to_owned();
                move || {
                    tokio::runtime::Runtime::new()
                        .unwrap()
                        .block_on(execute_scale_up_failover_cut(&path, &boundary, false));
                }
            })
            .unwrap()
            .join()
            .unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let store = Arc::new(SqliteStore::open_existing(&path, None).unwrap());
            let (_, command) = scale_up_failover_crash_fixture();
            let recovered = store.load_state().await.unwrap();
            assert!(recovered.reconfiguration.is_none());
            assert!(recovered.pending_effect.is_none());
            assert_eq!(
                recovered.current_configuration,
                Some(command.current_configuration)
            );
            assert_eq!(recovered.role, ReplicaRole::Primary);
            let evidence = command
                .scale_up_evidence
                .as_deref()
                .expect("carried failover evidence");
            let old_primary = evidence.intent().primary.clone();
            let new_primary = recovered
                .current_configuration
                .as_ref()
                .unwrap()
                .members
                .iter()
                .find(|member| member.role == ReplicaRole::Primary)
                .unwrap()
                .identity
                .clone();
            let registry = SessionRegistry::new(ProcessSessionId::new("receiver-current"));
            registry
                .register_peer(
                    new_primary.clone(),
                    ProcessSessionId::new("new-primary-current"),
                )
                .await;
            assert!(
                registry
                    .validate_peer(
                        &old_primary,
                        "old-primary-retired",
                        registry.local_session().as_str()
                    )
                    .await
                    .is_err()
            );
            assert!(
                registry
                    .validate_peer(&new_primary, "new-primary-current", "receiver-retired")
                    .await
                    .is_err()
            );
            assert!(
                registry
                    .validate_peer(
                        &new_primary,
                        "new-primary-current",
                        registry.local_session().as_str()
                    )
                    .await
                    .is_ok()
            );
        });
    }
}

#[test]
#[ignore = "helper process for scale_up_failover_replays_after_authority_and_completion_process_boundaries"]
fn scale_up_failover_crash_writer_process() {
    let (Ok(path), Ok(boundary)) = (
        env::var("KUBERIC_SCALE_UP_FAILOVER_PATH"),
        env::var("KUBERIC_SCALE_UP_FAILOVER_BOUNDARY"),
    ) else {
        panic!("scale-up failover cut helper requires parent-provided path and boundary");
    };
    std::thread::Builder::new()
        .name("scale-up-failover-cut".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(execute_scale_up_failover_cut(
                    Path::new(&path),
                    &boundary,
                    true,
                ));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
#[ignore = "helper process for switchover_recovery_boundaries_survive_process_termination"]
fn switchover_recovery_writer_process() {
    let (Ok(path), Ok(boundary)) = (
        env::var("KUBERIC_RECOVERY_PATH"),
        env::var("KUBERIC_RECOVERY_BOUNDARY"),
    ) else {
        return;
    };
    let (state, command) = switchover_recovery_fixture(&boundary);
    let crash_after = match boundary.as_str() {
        "demotion" | "promotion" => Some("role"),
        "compensation-admission" => Some("admission"),
        "retirement-before" | "restoration-before" | "restoration-unobserved-before" => {
            Some("access")
        }
        _ => None,
    };
    let runtime = Arc::new(SwitchoverRecoveryRuntime::new(&state, crash_after));
    let store = Arc::new(SqliteStore::create_authorized(path, state).unwrap());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        if boundary == "compensation-allocation" {
            store.begin_configuration(&command).await.unwrap();
        } else {
            Coordinator::new(store, runtime)
                .ensure_configuration(command)
                .await
                .unwrap();
        }
    });
    std::process::exit(0);
}

#[test]
fn switchover_preparation_boundaries_survive_process_termination() {
    for boundary in ["pending-effect", "effect-applied", "effect-completed"] {
        let directory = tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(directory.path());
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "switchover_preparation_writer_process",
            ])
            .env("KUBERIC_SWITCHOVER_PATH", &path)
            .env("KUBERIC_SWITCHOVER_BOUNDARY", boundary)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "switchover child failed at {boundary}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let store = Arc::new(SqliteStore::open_existing(&path, None).unwrap());
            if boundary != "effect-completed" {
                let adapter = RuntimeAdapter::new(
                    store.clone(),
                    Arc::new(FakeRuntime {
                        calls: AtomicUsize::new(0),
                        result: switchover_result(),
                    }),
                );
                adapter.resume_pending().await.unwrap();
            }
            let state = store.load_state().await.unwrap();
            assert!(state.pending_effect.is_none());
            let prepared = state.prepared_switchover.unwrap();
            assert_eq!(
                prepared.preparation_operation_id,
                switchover_effect().operation_id
            );
            assert_eq!(prepared.handoff_lsn, 9);
        });
    }
}

#[test]
fn real_runtime_switchover_preparation_recovers_after_process_termination() {
    for boundary in ["pending-effect", "effect-applied", "effect-completed"] {
        let directory = tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(directory.path());
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "real_switchover_preparation_writer_process",
            ])
            .env("KUBERIC_REAL_SWITCHOVER_PATH", &path)
            .env("KUBERIC_REAL_SWITCHOVER_BOUNDARY", boundary)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "real switchover child failed at {boundary}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let store = Arc::new(
                SqliteStore::open_existing(&path, Some(&switchover_storage_identity())).unwrap(),
            );
            let state = store.load_state().await.unwrap();
            let application_path = crash_application_path(&path);
            let (pod, application) = switchover_real_runtime(store.clone(), &application_path);
            assert!(
                application
                    .verify_applied(&seeded_operation())
                    .await
                    .unwrap()
            );
            pod.reconstruct(
                OpenMode::Existing,
                state.role,
                state.read_status,
                state.write_status,
                None,
            )
            .await
            .unwrap();
            let coordinator = Coordinator::new(store.clone(), pod);
            let prepared = coordinator
                .ensure_switchover_prepared(switchover_prepare_command())
                .await
                .unwrap();
            assert!(prepared.handoff_lsn >= 1);
            let recovered = store.load_state().await.unwrap();
            assert!(recovered.pending_effect.is_none());
            assert_eq!(recovered.write_status, AccessStatus::ReconfigurationPending);
            assert_eq!(recovered.prepared_switchover, Some(prepared));
        });
    }
}

async fn acknowledge_recovery_item(
    pod: &PodRuntime,
    peer: &CrashState,
    item: kuberic_wire::proto::ReplicationItem,
) {
    let progress = peer
        .apply(Operation {
            lsn: item.lsn,
            committed_lsn: item.committed_lsn,
            data: item.data.into(),
        })
        .await
        .unwrap();
    pod.data_plane()
        .accept_acknowledgement(kuberic_wire::proto::ReplicationAck {
            protocol_version: kuberic_protocol::PROTOCOL_VERSION,
            sender: item.sender,
            receiver: item.receiver,
            epoch: item.epoch,
            previous_configuration_id: item.previous_configuration_id,
            current_configuration_id: item.current_configuration_id,
            received_lsn: progress.applied_lsn,
            applied_lsn: progress.applied_lsn,
            committed_lsn: progress.committed_lsn,
            ..Default::default()
        })
        .await
        .unwrap();
}

fn generation_preparation(generation: u64) -> PrepareSwitchover {
    let mut command = switchover_prepare_command();
    command.preparation_generation = generation;
    command.request_id = SwitchoverRequestId::new(format!("restoration-{generation}"));
    command.operation_id = kuberic_protocol::types::derive_switchover_preparation_operation_id(
        &switchover_storage_identity().resource_uid,
        &command.request_id,
        generation,
        &command.current_configuration.configuration_id,
        &command.source,
        &command.target,
    );
    command
}

#[test]
fn repeated_preparation_retirement_fences_every_delayed_command_after_process_termination() {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let output = Command::new(env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "local_write_recovery_writer_process",
        ])
        .env("KUBERIC_LOCAL_RECOVERY_PATH", &path)
        .env("KUBERIC_LOCAL_RECOVERY_BOUNDARY", "retirements")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(73),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let store = Arc::new(SqliteStore::open_existing(&path, None).unwrap());
        let before = store.load_state().await.unwrap();
        assert_eq!(
            before.preparation_retirement.as_ref().unwrap().generation,
            3
        );
        let (pod, _) = switchover_real_runtime(store.clone(), &crash_application_path(&path));
        pod.reconstruct(
            OpenMode::Existing,
            ReplicaRole::Primary,
            before.read_status,
            before.write_status,
            None,
        )
        .await
        .unwrap();
        let runtime_before = pod.snapshot().await;
        let coordinator = Coordinator::new(store.clone(), pod.clone());
        for generation in 1..=3 {
            assert!(
                coordinator
                    .ensure_switchover_prepared(generation_preparation(generation))
                    .await
                    .is_err()
            );
            assert_eq!(store.load_state().await.unwrap(), before);
            assert_eq!(pod.snapshot().await, runtime_before);
        }
        assert_eq!(runtime_before.write_status, AccessStatus::Granted);
        assert!(
            coordinator
                .ensure_switchover_prepared(generation_preparation(4))
                .await
                .is_ok()
        );
    });
}

#[test]
fn local_write_recovery_boundaries_commit_fresh_writes_after_process_termination() {
    use kuberic_runtime_internal::authority::LocalWriteJournal;
    for boundary in ["registered", "application-commit", "grant"] {
        let directory = tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(directory.path());
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "local_write_recovery_writer_process",
            ])
            .env("KUBERIC_LOCAL_RECOVERY_PATH", &path)
            .env("KUBERIC_LOCAL_RECOVERY_BOUNDARY", boundary)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(73),
            "{boundary}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let store = Arc::new(SqliteStore::open_existing(&path, None).unwrap());
            let state = store.load_state().await.unwrap();
            assert_eq!(state.write_status, AccessStatus::ReconfigurationPending);
            let (pod, _) = switchover_real_runtime(store.clone(), &crash_application_path(&path));
            pod.reconstruct(OpenMode::Existing, ReplicaRole::Primary,
                AccessStatus::ReconfigurationPending, AccessStatus::ReconfigurationPending, None).await.unwrap();
            let adapter = Arc::new(RuntimeAdapter::new(store.clone(), pod.clone()));
            let recovery = adapter.clone();
            let mut task = tokio::spawn(async move { recovery.resume_pending().await });
            let plane = pod.data_plane();
            let peer = CrashState::open(path.with_extension("peer"));
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    tokio::select! {
                        result = &mut task => { result.unwrap().unwrap(); break; },
                        item = plane.next_outbound() => {
                            let Some(kuberic_agent::hosting::OutboundReplication::Replication(item)) = item else { panic!("recovery replication") };
                            assert_ne!(pod.snapshot().await.write_status, AccessStatus::Granted);
                            acknowledge_recovery_item(&pod, &peer, item).await;
                        }
                    }
                }
            }).await.unwrap_or_else(|_| panic!("local write recovery timed out at {boundary}"));
            let old = store.load_local_write(&OperationId::new("interrupted-before-crash")).await.unwrap().unwrap();
            assert_eq!(old.phase, kuberic_runtime_internal::authority::LocalWritePhase::Committed);
            assert_eq!(old.data, Bytes::from_static(b"original-before-crash"));
            let fresh = plane.begin_write(kuberic_runtime::application::ClientWrite {
                operation_id: OperationId::new("fresh-after-process-crash"),
                data: Bytes::from_static(b"different-after-crash"),
            }).await.unwrap();
            for item in &fresh.replication_items { acknowledge_recovery_item(&pod, &peer, item.clone()).await; }
            assert_eq!(fresh.committed().await.unwrap().committed_lsn, 2);
        });
    }
}

#[test]
#[ignore = "subprocess for local_write_recovery_boundaries_commit_fresh_writes_after_process_termination"]
fn local_write_recovery_writer_process() {
    let (Ok(path), Ok(boundary)) = (
        env::var("KUBERIC_LOCAL_RECOVERY_PATH"),
        env::var("KUBERIC_LOCAL_RECOVERY_BOUNDARY"),
    ) else {
        return;
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let configuration = switchover_configuration();
        let mut state = AgentState::new(switchover_storage_identity());
        state.highest_epoch = configuration.epoch;
        state.current_configuration = Some(configuration.clone());
        state.role = ReplicaRole::Primary;
        state.read_status = AccessStatus::Granted;
        state.write_status = AccessStatus::Granted;
        let store = Arc::new(SqliteStore::create_authorized(&path, state).unwrap());
        store
            .admit(&AdmittedAuthority {
                scale_up: None,
                secondary_removal: None,
                local_identity: switchover_storage_identity().local_identity,
                transition_kind: None,
                previous_configuration: None,
                current_configuration: configuration.clone(),
                switchover_handoff: None,
            })
            .await
            .unwrap();
        let (pod, _) =
            switchover_real_runtime(store.clone(), &crash_application_path(Path::new(&path)));
        pod.reconstruct(
            OpenMode::Existing,
            ReplicaRole::Primary,
            AccessStatus::Granted,
            AccessStatus::Granted,
            None,
        )
        .await
        .unwrap();
        if boundary == "retirements" {
            let coordinator = Coordinator::new(store.clone(), pod.clone());
            for generation in 1..=3 {
                let command = generation_preparation(generation);
                let handoff = if generation < 3 {
                    Some(
                        coordinator
                            .ensure_switchover_prepared(command.clone())
                            .await
                            .unwrap(),
                    )
                } else {
                    None
                };
                let restore = EnsureConfiguration {
                    previous_policy: None,
                    secondary_removal_evidence: None,
                    scale_up_evidence: None,
                    operation_id: OperationId::new("reused-restoration"),
                    previous_configuration: None,
                    previous_epoch: None,
                    current_epoch: configuration.epoch,
                    current_configuration: configuration.clone(),
                    effective_policy: switchover_storage_identity().effective_policy,
                    local_replica_id: command.source.replica_id,
                    expected_instance_id: command.source.instance_id,
                    expected_agent_generation: command.source.agent_generation,
                    transition_kind: TransitionKind::PlannedSwitchover,
                    failover_safe_lsn: None,
                    primary_write_status: AccessStatus::ReconfigurationPending,
                    current_only: false,
                    retire_build_ids: Vec::new(),
                    retire_switchover_preparation_ids: vec![
                        kuberic_protocol::types::SwitchoverPreparationId {
                            generation,
                            operation_id: command.operation_id,
                        },
                    ],
                    switchover_handoff: handoff,
                };
                coordinator
                    .ensure_configuration(restore.clone())
                    .await
                    .unwrap();
                coordinator
                    .ensure_configuration(EnsureConfiguration {
                        operation_id: OperationId::new("reused-grant"),
                        transition_kind: TransitionKind::Bootstrap,
                        primary_write_status: AccessStatus::Granted,
                        switchover_handoff: None,
                        retire_switchover_preparation_ids: Vec::new(),
                        ..restore
                    })
                    .await
                    .unwrap();
            }
            std::process::exit(73);
        }
        let pending = pod
            .data_plane()
            .begin_write(kuberic_runtime::application::ClientWrite {
                operation_id: OperationId::new("interrupted-before-crash"),
                data: Bytes::from_static(b"original-before-crash"),
            })
            .await
            .unwrap();
        let coordinator = Coordinator::new(store.clone(), pod.clone());
        let handoff = coordinator
            .ensure_switchover_prepared(switchover_prepare_command())
            .await
            .unwrap();
        assert!(pending.committed().await.is_err());
        let restore = EnsureConfiguration {
            previous_policy: None,
            secondary_removal_evidence: None,
            scale_up_evidence: None,
            operation_id: OperationId::new("recover-source"),
            previous_configuration: None,
            previous_epoch: None,
            current_epoch: configuration.epoch,
            current_configuration: configuration,
            effective_policy: switchover_storage_identity().effective_policy,
            local_replica_id: handoff.source.replica_id,
            expected_instance_id: handoff.source.instance_id.clone(),
            expected_agent_generation: handoff.source.agent_generation.clone(),
            transition_kind: TransitionKind::PlannedSwitchover,
            failover_safe_lsn: None,
            primary_write_status: AccessStatus::ReconfigurationPending,
            current_only: false,
            retire_build_ids: Vec::new(),
            retire_switchover_preparation_ids: vec![handoff.preparation()],
            switchover_handoff: Some(handoff),
        };
        coordinator.ensure_configuration(restore).await.unwrap();
        let grant = RuntimeEffect {
            operation_id: OperationId::new("grant-after-retirement"),
            sequence: store.load_state().await.unwrap().next_effect_sequence,
            action: RuntimeEffectAction::SetAccessStatus {
                read: AccessStatus::Granted,
                write: AccessStatus::Granted,
            },
        };
        store.begin_effect(&grant).await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(30),
                pod.apply_effect(grant.clone())
            )
            .await
            .is_err()
        );
        if boundary == "registered" {
            std::process::exit(73);
        }
        let granting = pod.clone();
        let task = tokio::spawn(async move { granting.apply_effect(grant).await });
        let Some(kuberic_agent::hosting::OutboundReplication::Replication(item)) =
            pod.data_plane().next_outbound().await
        else {
            panic!("recovery replication")
        };
        let peer = CrashState::open(Path::new(&path).with_extension("peer"));
        acknowledge_recovery_item(&pod, &peer, item).await;
        task.await.unwrap().unwrap();
        std::process::exit(73);
    });
}

#[test]
fn real_handoff_configuration_boundaries_survive_process_termination() {
    for scenario in [
        "demotion",
        "promotion",
        "retirement-after",
        "target-current-only",
        "compensation-promotion",
        "compensation-completion",
        "restoration-after",
    ] {
        for boundary in [
            "intent",
            "authority",
            "application-role",
            "access",
            "receipt",
        ] {
            // Current-only and restoration deliberately omit role callbacks.
            if boundary == "application-role"
                && matches!(
                    scenario,
                    "retirement-after"
                        | "target-current-only"
                        | "compensation-completion"
                        | "restoration-after"
                )
            {
                continue;
            }
            if boundary == "authority" && scenario == "restoration-after" {
                continue;
            }
            let directory = tempdir().unwrap();
            let path = SqliteStore::metadata_database_path(directory.path());
            let output = Command::new(env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "real_handoff_configuration_writer_process",
                ])
                .env("KUBERIC_HANDOFF_PATH", &path)
                .env("KUBERIC_HANDOFF_SCENARIO", scenario)
                .env("KUBERIC_HANDOFF_BOUNDARY", boundary)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(73),
                "{scenario}/{boundary}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let store = Arc::new(SqliteStore::open_existing(&path, None).unwrap());
                let state = store.load_state().await.unwrap();
                let (_, command) = real_handoff_fixture(scenario);
                assert_ne!(state.write_status, AccessStatus::Granted);
                if boundary == "receipt" {
                    assert!(state.reconfiguration.is_none());
                    assert_eq!(state.retained_command.as_ref().unwrap().command, command);
                } else {
                    assert_eq!(state.reconfiguration.as_ref().unwrap().command, command);
                    assert_eq!(state.pending_effect.is_some(), boundary != "intent");
                }
                let application = Arc::new(CrashState::open(crash_application_path(&path)));
                assert!(
                    application
                        .verify_applied(&seeded_operation())
                        .await
                        .unwrap()
                );
                assert_eq!(
                    application.durable_progress().await.unwrap().committed_lsn,
                    7
                );
                let pod = Arc::new(PodRuntime::new(
                    state.identity.local_identity.clone(),
                    application.clone(),
                    store.clone(),
                ));
                let executor = Arc::new(CrashAfterRealEffect {
                    runtime: pod.clone(),
                    boundary: None,
                    exit_code: 73,
                });
                let service = kuberic_agent::service::AgentService::new(
                    store.clone(),
                    pod.clone(),
                    executor.clone(),
                    Arc::<str>::from("crash-test"),
                )
                .unwrap();
                let address = || {
                    std::net::TcpListener::bind("127.0.0.1:0")
                        .unwrap()
                        .local_addr()
                        .unwrap()
                };
                let (ready_tx, mut ready_rx) = tokio::sync::watch::channel(false);
                let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
                let server =
                    tokio::spawn(service.serve(address(), address(), ready_tx, shutdown_rx));
                tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    ready_rx.wait_for(|ready| *ready),
                )
                .await
                .unwrap()
                .unwrap();
                let direct_client = pod.data_plane();
                assert!(
                    direct_client
                        .begin_write(kuberic_runtime::application::ClientWrite {
                            operation_id: OperationId::new("crash-retained-client"),
                            data: Bytes::from_static(b"forbidden"),
                        })
                        .await
                        .is_err()
                );
                let resumed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    while store.load_state().await.unwrap().reconfiguration.is_some() {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                })
                .await;
                assert!(resumed.is_ok(),
                    "{scenario}/{boundary}: startup recovery did not complete; durable={:?}; runtime={:?}",
                    store.load_state().await.unwrap(), pod.snapshot().await);
                let coordinator = Coordinator::new(store.clone(), executor);
                let completed = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    coordinator.ensure_configuration(command.clone()),
                )
                .await
                .unwrap()
                .unwrap_or_else(|error| panic!("{scenario}/{boundary}: {error}"));
                assert_eq!(
                    coordinator
                        .ensure_configuration(command.clone())
                        .await
                        .unwrap(),
                    completed
                );
                let recovered = store.load_state().await.unwrap();
                assert_eq!(recovered.highest_epoch, command.current_epoch);
                assert!(recovered.pending_effect.is_none() && recovered.reconfiguration.is_none());
                assert_ne!(pod.snapshot().await.write_status, AccessStatus::Granted);
                assert!(
                    application
                        .verify_applied(&seeded_operation())
                        .await
                        .unwrap()
                );
                assert_eq!(
                    application.durable_progress().await.unwrap().committed_lsn,
                    7
                );
                if !command.retire_switchover_preparation_ids.is_empty() {
                    assert!(recovered.prepared_switchover.is_none());
                    assert_eq!(recovered.retired_switchover, command.switchover_handoff);
                    assert_eq!(
                        recovered
                            .preparation_retirement
                            .as_ref()
                            .map(|retired| retired.generation),
                        command
                            .retire_switchover_preparation_ids
                            .first()
                            .map(|id| id.generation)
                    );
                }
                shutdown_tx.send_replace(true);
                server.await.unwrap().unwrap();
            });
        }
    }
}

#[test]
#[ignore = "helper process for real_handoff_configuration_boundaries_survive_process_termination"]
fn real_handoff_configuration_writer_process() {
    let (Ok(path), Ok(scenario), Ok(boundary)) = (
        env::var("KUBERIC_HANDOFF_PATH"),
        env::var("KUBERIC_HANDOFF_SCENARIO"),
        env::var("KUBERIC_HANDOFF_BOUNDARY"),
    ) else {
        return;
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (state, command) = real_handoff_fixture(&scenario);
        let authority = AdmittedAuthority {
            scale_up: None,
            secondary_removal: None,
            local_identity: state.identity.local_identity.clone(),
            transition_kind: state
                .previous_configuration
                .as_ref()
                .map(|_| TransitionKind::PlannedSwitchover),
            previous_configuration: state.previous_configuration.clone(),
            current_configuration: state.current_configuration.clone().unwrap(),
            switchover_handoff: (state.highest_epoch.configuration_number > 1)
                .then(|| command.switchover_handoff.clone())
                .flatten(),
        };
        let store = Arc::new(SqliteStore::create_authorized(&path, state.clone()).unwrap());
        store.admit(&authority).await.unwrap();
        store
            .record_replication_progress(&ReplicationProgress {
                fence: authority.fence(),
                verified_lsn: 7,
            })
            .await
            .unwrap();
        let starting = switchover_configuration();
        store
            .record_replication_progress(&ReplicationProgress {
                fence: kuberic_runtime_internal::authority::AuthorityFence {
                    epoch: starting.epoch,
                    previous_configuration_id: None,
                    current_configuration_id: starting.configuration_id,
                },
                verified_lsn: 7,
            })
            .await
            .unwrap();
        let application = Arc::new(CrashState::open(crash_application_path(Path::new(&path))));
        for lsn in 1..=7 {
            application
                .apply(if lsn == 1 {
                    seeded_operation()
                } else {
                    Operation {
                        lsn,
                        committed_lsn: lsn,
                        data: Bytes::from(format!("acknowledged-{lsn}")),
                    }
                })
                .await
                .unwrap();
        }
        application.commit(7).await.unwrap();
        let pod = Arc::new(PodRuntime::new(
            state.identity.local_identity,
            application,
            store.clone(),
        ));
        pod.reconstruct(
            OpenMode::Existing,
            state.role,
            state.read_status,
            state.write_status,
            None,
        )
        .await
        .unwrap();
        if boundary == "intent" {
            store.begin_configuration(&command).await.unwrap();
        } else {
            let executor = Arc::new(CrashAfterRealEffect {
                runtime: pod,
                boundary: Some(boundary.clone()),
                exit_code: 73,
            });
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                Coordinator::new(store, executor).ensure_configuration(command),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                boundary, "receipt",
                "requested effect boundary was not reached"
            );
        }
        std::process::exit(73);
    });
}

#[test]
fn real_runtime_effect_recovers_and_completes_after_process_termination() {
    for boundary in ["pending-effect", "effect-complete-before-stage-advance"] {
        let directory = tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(directory.path());
        let output = Command::new(env::current_exe().unwrap())
            .args(["--ignored", "--exact", "real_runtime_effect_writer_process"])
            .env("KUBERIC_REAL_RUNTIME_PATH", &path)
            .env("KUBERIC_REAL_RUNTIME_BOUNDARY", boundary)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "real runtime child failed at {boundary}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let store = Arc::new(
                SqliteStore::open_existing(&path, Some(&single_storage_identity())).unwrap(),
            );
            let state = store.load_state().await.unwrap();
            assert_eq!(
                state.reconfiguration.as_ref().map(|record| record.stage),
                Some(CoordinatorStage::AdmitAuthority)
            );
            match boundary {
                "pending-effect" => {
                    assert_eq!(
                        state.pending_effect.as_ref().map(|pending| pending.stage),
                        Some(EffectStage::IntentCommitted)
                    );
                    assert!(state.retained_result.is_some());
                }
                "effect-complete-before-stage-advance" => {
                    assert!(state.pending_effect.is_none());
                    assert!(state.retained_result.is_some());
                }
                _ => unreachable!(),
            }

            let application_path = crash_application_path(&path);
            let (pod, application) = real_runtime(store.clone(), &application_path);
            assert!(
                application
                    .verify_applied(&seeded_operation())
                    .await
                    .unwrap()
            );
            assert_eq!(
                application.durable_progress().await.unwrap(),
                DurableApplicationProgress {
                    applied_lsn: 1,
                    committed_lsn: 1,
                }
            );
            pod.reconstruct(
                OpenMode::Existing,
                state.role,
                state.read_status,
                state.write_status,
                None,
            )
            .await
            .unwrap();
            let coordinator = Coordinator::new(store.clone(), pod);
            coordinator
                .ensure_configuration(real_configuration_command())
                .await
                .unwrap();
            coordinator
                .ensure_configuration(grant_access_command())
                .await
                .unwrap();

            let recovered = store.load_state().await.unwrap();
            assert!(recovered.reconfiguration.is_none());
            assert_eq!(
                recovered.retained_command.unwrap().command,
                grant_access_command()
            );
            assert_eq!(recovered.role, ReplicaRole::Primary);
            assert_eq!(recovered.read_status, AccessStatus::Granted);
            assert_eq!(recovered.write_status, AccessStatus::Granted);
            assert!(
                application
                    .verify_applied(&seeded_operation())
                    .await
                    .unwrap()
            );
        });
    }
}

#[test]
#[ignore = "helper process for sqlite_commits_survive_process_termination_without_destructors"]
fn crash_writer_process() {
    let Ok(path) = env::var("KUBERIC_CRASH_WRITER_PATH") else {
        return;
    };
    let store = SqliteStore::create_authorized(path, AgentState::new(storage_identity())).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(store.begin_effect(&effect())).unwrap();
    std::process::exit(0);
}

#[test]
#[ignore = "helper process for configuration_boundaries_survive_process_termination_without_destructors"]
fn crash_boundary_writer_process() {
    let (Ok(path), Ok(boundary)) = (
        env::var("KUBERIC_CRASH_WRITER_PATH"),
        env::var("KUBERIC_CRASH_BOUNDARY"),
    ) else {
        return;
    };
    let store = SqliteStore::create_authorized(path, AgentState::new(storage_identity())).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        match boundary.as_str() {
            "pending-effect" => {
                assert_eq!(
                    store.begin_effect(&effect()).await.unwrap(),
                    BeginEffect::Execute(effect())
                );
            }

            "effect-complete-before-stage-advance" => {
                assert!(matches!(
                    store
                        .begin_configuration(&configuration_command())
                        .await
                        .unwrap(),
                    BeginConfiguration::Execute(_)
                ));
                store.begin_effect(&effect()).await.unwrap();
                store.mark_effect_applied(&effect()).await.unwrap();
                store.complete_effect(&result()).await.unwrap();
            }
            "enclosing-command" => {
                assert!(matches!(
                    store
                        .begin_configuration(&configuration_command())
                        .await
                        .unwrap(),
                    BeginConfiguration::Execute(_)
                ));
            }
            "terminal-command" => {
                let command = configuration_command();
                store.begin_configuration(&command).await.unwrap();
                store
                    .advance_configuration(
                        &command.operation_id,
                        CoordinatorStage::AdmitAuthority,
                        CoordinatorStage::Complete,
                        None,
                    )
                    .await
                    .unwrap();
                store
                    .complete_configuration(&command.operation_id)
                    .await
                    .unwrap();
            }
            _ => panic!("unknown crash boundary {boundary}"),
        }
    });
    std::process::exit(0);
}

#[test]
#[ignore = "helper process for switchover_preparation_boundaries_survive_process_termination"]
fn switchover_preparation_writer_process() {
    let (Ok(path), Ok(boundary)) = (
        env::var("KUBERIC_SWITCHOVER_PATH"),
        env::var("KUBERIC_SWITCHOVER_BOUNDARY"),
    ) else {
        return;
    };
    let store = SqliteStore::create_authorized(path, AgentState::new(storage_identity())).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let effect = switchover_effect();
        store.begin_effect(&effect).await.unwrap();
        match boundary.as_str() {
            "pending-effect" => {}
            "effect-applied" => {
                store.mark_effect_applied(&effect).await.unwrap();
            }
            "effect-completed" => {
                store.mark_effect_applied(&effect).await.unwrap();
                store.complete_effect(&switchover_result()).await.unwrap();
            }
            _ => panic!("unknown switchover boundary {boundary}"),
        }
    });
    std::process::exit(0);
}

#[test]
#[ignore = "helper process for real_runtime_switchover_preparation_recovers_after_process_termination"]
fn real_switchover_preparation_writer_process() {
    let (Ok(path), Ok(boundary)) = (
        env::var("KUBERIC_REAL_SWITCHOVER_PATH"),
        env::var("KUBERIC_REAL_SWITCHOVER_BOUNDARY"),
    ) else {
        return;
    };
    let configuration = switchover_configuration();
    let mut state = AgentState::new(switchover_storage_identity());
    state.highest_epoch = configuration.epoch;
    state.current_configuration = Some(configuration.clone());
    state.role = ReplicaRole::Primary;
    state.read_status = AccessStatus::Granted;
    state.write_status = AccessStatus::Granted;
    let store = Arc::new(SqliteStore::create_authorized(&path, state).unwrap());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let authority = AdmittedAuthority {
            scale_up: None,
            secondary_removal: None,
            local_identity: switchover_storage_identity().local_identity,
            transition_kind: None,
            previous_configuration: None,
            current_configuration: configuration,
            switchover_handoff: None,
        };
        store.admit(&authority).await.unwrap();
        let application_path = crash_application_path(Path::new(&path));
        let (pod, application) = switchover_real_runtime(store.clone(), &application_path);
        application.apply(seeded_operation()).await.unwrap();
        application.commit(1).await.unwrap();
        pod.reconstruct(
            OpenMode::Existing,
            ReplicaRole::Primary,
            AccessStatus::Granted,
            AccessStatus::Granted,
            None,
        )
        .await
        .unwrap();
        let command = switchover_prepare_command();
        let effect = switchover_runtime_effect(
            &command,
            store.load_state().await.unwrap().next_effect_sequence,
        );
        match boundary.as_str() {
            "pending-effect" => {
                store.begin_effect(&effect).await.unwrap();
            }
            "effect-applied" => {
                store.begin_effect(&effect).await.unwrap();
                pod.apply_effect(effect.clone()).await.unwrap();
                store.mark_effect_applied(&effect).await.unwrap();
            }
            "effect-completed" => {
                Coordinator::new(store.clone(), pod)
                    .ensure_switchover_prepared(command)
                    .await
                    .unwrap();
            }
            _ => panic!("unknown real switchover boundary {boundary}"),
        }
    });
    std::process::exit(0);
}

#[test]
#[ignore = "helper process for real_runtime_effect_recovers_and_completes_after_process_termination"]
fn real_runtime_effect_writer_process() {
    let (Ok(path), Ok(boundary)) = (
        env::var("KUBERIC_REAL_RUNTIME_PATH"),
        env::var("KUBERIC_REAL_RUNTIME_BOUNDARY"),
    ) else {
        return;
    };
    let store = Arc::new(
        SqliteStore::create_authorized(&path, AgentState::new(single_storage_identity())).unwrap(),
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let application_path = crash_application_path(Path::new(&path));
        let (pod, application) = real_runtime(store.clone(), &application_path);
        application.apply(seeded_operation()).await.unwrap();
        application.commit(1).await.unwrap();
        let coordinator = Coordinator::new(store.clone(), pod.clone());
        coordinator
            .open_runtime(
                OpenMode::Existing,
                &kuberic_protocol::types::ProcessSessionId::new("real-runtime-session"),
            )
            .await
            .unwrap();
        let command = real_configuration_command();
        store.begin_configuration(&command).await.unwrap();
        let authority = admit_configuration(&command, &store.load_state().await.unwrap()).unwrap();
        let sequence = store.load_state().await.unwrap().next_effect_sequence;
        let effect = RuntimeEffect {
            operation_id: OperationId::new("real-runtime-configuration:admit-authority"),
            sequence,
            action: RuntimeEffectAction::AdmitAuthority(Box::new(authority)),
        };
        match boundary.as_str() {
            "pending-effect" => {
                store.begin_effect(&effect).await.unwrap();
            }
            "effect-complete-before-stage-advance" => {
                RuntimeAdapter::new(store, pod)
                    .execute(effect)
                    .await
                    .unwrap();
            }
            _ => panic!("unknown real runtime boundary {boundary}"),
        }
    });
    std::process::exit(0);
}
