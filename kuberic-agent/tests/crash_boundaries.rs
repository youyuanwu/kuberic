use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::{env, process::Command};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream;
use kuberic_agent::command::admit_configuration;
use kuberic_agent::coordinator::Coordinator;
use kuberic_agent::hosting::PodRuntime;
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
    Epoch, InitializationId, OperationId, PodUid, ProcessSessionId, PvcUid, ReplicaId,
    ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ResourceUid, ScaleUpConfigurationEvidence,
    ScaleUpFailoverEvidence, ScaleUpIntent, ScaleUpStage, ScaleUpWitness, SwitchoverRequestId,
    TransitionKind,
};
use kuberic_runtime::application::{
    CopyChunk, DurableApplicationAck, DurableApplicationProgress, OpenContext, Operation,
    OperationDataStream, RoleChange, StateProvider, StatefulServiceReplica,
};
use kuberic_runtime::engine::{DurableState, RetainedOperationStream};
use kuberic_runtime::replicator::{DefaultReplicatorFactory, Replicator, ReplicatorSettings};
use kuberic_runtime::{Result as RuntimeResult, RuntimeError};
use kuberic_runtime_internal::authority::{
    AdmittedAuthority, BuildAuthority, BuildAuthorityKind, BuildAuthorityStore, BuildProgressStore,
    DurableBuildProgress, ReplicaAuthorityStore, ReplicationProgress, ReplicationProgressStore,
};
use kuberic_runtime_internal::effects::{
    BuildPostcondition, OpenMode, RuntimeEffect, RuntimeEffectAction, RuntimeEffectResult,
    RuntimePostcondition, RuntimeSnapshot,
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
}

struct ScaleUpCrashRuntime {
    state: Mutex<RuntimePostcondition>,
    crash_after: Option<&'static str>,
}

impl ScaleUpCrashRuntime {
    fn new(state: &AgentState, crash_after: Option<&'static str>) -> Self {
        let mut postcondition = result().postcondition;
        postcondition.open = true;
        postcondition.role = state.role;
        postcondition.read_status = state.read_status;
        postcondition.write_status = state.write_status;
        postcondition.current_progress = 9;
        postcondition.verified_replication_lsn = Some(9);
        postcondition.committed_lsn = 9;
        postcondition.current_configuration_quorum_progress = 9;
        postcondition.catch_up_complete = true;
        postcondition.authority =
            state
                .current_configuration
                .as_ref()
                .map(|current| AdmittedAuthority {
                    local_identity: state.identity.local_identity.clone(),
                    transition_kind: state
                        .previous_configuration
                        .as_ref()
                        .map(|_| TransitionKind::ScaleUp),
                    previous_configuration: state.previous_configuration.clone(),
                    current_configuration: current.clone(),
                    switchover_handoff: None,
                    secondary_removal: None,
                    scale_up: state.scale_up_evidence.clone(),
                });
        postcondition.builds = state
            .build_commands
            .values()
            .filter_map(|command| command.authority.clone())
            .map(|authority| BuildPostcondition {
                durable_lsn: 9,
                completed: true,
                catch_up_boundary_lsn: Some(9),
                last_sequence: 2,
                authority,
            })
            .collect();
        Self {
            state: Mutex::new(postcondition),
            crash_after,
        }
    }
}

#[async_trait]
impl RuntimeEffectExecutor for ScaleUpCrashRuntime {
    async fn apply_runtime_effect(&self, effect: RuntimeEffect) -> Result<RuntimeEffectResult> {
        let mut state = self.state.lock().unwrap();
        let boundary = match effect.action {
            RuntimeEffectAction::AdmitAuthority(authority) => {
                state.authority = Some(*authority);
                "admission"
            }
            RuntimeEffectAction::SetAccessStatus { read, write } => {
                state.read_status = read;
                state.write_status = write;
                "access"
            }
            RuntimeEffectAction::AuthorizeFailoverPrefix(lsn) => {
                state.verified_replication_lsn = Some(lsn);
                "failover-prefix"
            }
            RuntimeEffectAction::SetReadStatus(read) => {
                state.read_status = read;
                "read"
            }
            RuntimeEffectAction::SetWriteStatus(write) => {
                state.write_status = write;
                "write"
            }
            RuntimeEffectAction::ChangeReplicatorRole(role) => {
                state.role_transition = Some(kuberic_runtime_internal::effects::RoleTransition {
                    completed_role: state.role,
                    target_role: role,
                    replicator_completed: true,
                    epoch_completed: role != ReplicaRole::Primary,
                    application_completed: false,
                });
                "replicator-role"
            }
            RuntimeEffectAction::UpdateEpoch => {
                state
                    .role_transition
                    .as_mut()
                    .expect("primary role transition")
                    .epoch_completed = true;
                "epoch"
            }
            RuntimeEffectAction::ChangeApplicationRole(role) => {
                state.role = role;
                state.role_transition = None;
                "application-role"
            }
            RuntimeEffectAction::RefreshApplicationProgress => "progress",
            RuntimeEffectAction::WaitForCatchup => "catchup",
            RuntimeEffectAction::RetireBuild(build_id) => {
                state
                    .builds
                    .retain(|build| build.authority.build_id != build_id);
                "retirement"
            }
            action => panic!("unexpected scale-up recovery effect {action:?}"),
        };
        if self.crash_after == Some(boundary) {
            std::process::exit(0);
        }
        Ok(RuntimeEffectResult {
            operation_id: effect.operation_id,
            sequence: effect.sequence,
            postcondition: state.clone(),
        })
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
            std::process::exit(73);
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
            std::process::exit(73);
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
        if self.consume_replication {
            let mut stream = interfaces
                .state_replicator()
                .get_replication_stream()
                .await?;
            let application = Arc::downgrade(&self);
            tokio::spawn(async move {
                while let Ok(Some(operation)) = stream.get_operation().await {
                    let Some(application) = application.upgrade() else {
                        return;
                    };
                    let kuberic_runtime::replicator::stream::OperationMetadata::Replication {
                        lsn,
                        committed_lsn,
                    } = operation.metadata
                    else {
                        panic!("expected replication")
                    };
                    let ack = application
                        .apply(Operation {
                            lsn,
                            committed_lsn,
                            data: operation.data.clone(),
                        })
                        .await
                        .unwrap();
                    operation.acknowledge(ack).unwrap();
                }
            });
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
        _up_to_lsn: i64,
        _copy_context: OperationDataStream,
    ) -> RuntimeResult<OperationDataStream> {
        Ok(Box::pin(stream::empty()))
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
        _chunk: CopyChunk,
    ) -> RuntimeResult<()> {
        Ok(())
    }

    async fn verify_copy_chunk(
        &self,
        _build_id: &OperationId,
        _sequence: u64,
        _chunk: &CopyChunk,
    ) -> RuntimeResult<bool> {
        Ok(true)
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
    let candidate = ReplicaIdentity {
        replica_id: ReplicaId::new(2),
        instance_id: ReplicaInstanceId::new("scale-up-candidate"),
        agent_generation: AgentGeneration::new("scale-up-candidate-generation"),
    };
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
        build_id: OperationId::new("scale-up-crash-build"),
        kind: BuildAuthorityKind::Provisioning,
        source: primary.clone(),
        target: candidate.clone(),
        current_configuration: previous.clone(),
        replication_boundary_lsn: 4,
    };
    let mut intent = ScaleUpIntent {
        operation_id: OperationId::default(),
        resource_uid: ResourceUid::new("resource-1"),
        spec_generation: 2,
        desired_replicas: 2,
        previous_configuration: previous.clone(),
        current_configuration: current.clone(),
        previous_policy: previous_policy.clone(),
        current_policy: current_policy.clone(),
        primary: primary.clone(),
        target: candidate.clone(),
        build_id: build.build_id.clone(),
        snapshot_boundary_lsn: 4,
        catch_up_boundary_lsn: 9,
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

async fn ensure_scale_up_store_cut(path: &Path, cut: &str, after: bool, terminate: bool) {
    let (state, _, build) = scale_up_crash_fixture(false);
    if cut == "store-initialization" && terminate && !after {
        std::process::exit(73);
    }
    let store = if path.is_file() {
        Arc::new(SqliteStore::open_existing(path, None).unwrap())
    } else {
        Arc::new(SqliteStore::create_authorized(path, state).unwrap())
    };
    if cut == "store-initialization" {
        if terminate && after {
            std::process::exit(73);
        }
        return;
    }

    if store.load_build(&build.build_id).await.unwrap().is_none() {
        if cut == "build-authority-admission" && terminate && !after {
            std::process::exit(73);
        }
        store.admit_build(&build).await.unwrap();
        if cut == "build-authority-admission" {
            if terminate && after {
                std::process::exit(73);
            }
            return;
        }
    }
    if cut == "build-authority-admission" {
        return;
    }

    let progress = match cut {
        "snapshot-boundary-persistence" => DurableBuildProgress {
            authority: build,
            last_sequence: 0,
            durable_lsn: 4,
            completed: false,
            catch_up_boundary_lsn: Some(9),
        },
        "source-progress-persistence" => DurableBuildProgress {
            authority: build,
            last_sequence: 1,
            durable_lsn: 7,
            completed: false,
            catch_up_boundary_lsn: Some(9),
        },
        "target-progress-persistence" => DurableBuildProgress {
            authority: build,
            last_sequence: 2,
            durable_lsn: 9,
            completed: true,
            catch_up_boundary_lsn: Some(9),
        },
        other => panic!("unknown scale-up store cut {other}"),
    };
    if terminate && !after {
        std::process::exit(73);
    }
    store.record_build_progress(&progress).await.unwrap();
    if terminate && after {
        std::process::exit(73);
    }
}

async fn execute_scale_up_configuration_cut(path: &Path, cut: &str, terminate: bool) {
    let current_only = cut.starts_with("current-only")
        || cut.starts_with("build-retirement")
        || cut.starts_with("completion");
    let (initial, command, build) = scale_up_crash_fixture(current_only);
    let store = if path.is_file() {
        Arc::new(SqliteStore::open_existing(path, None).unwrap())
    } else {
        let store = Arc::new(SqliteStore::create_authorized(path, initial.clone()).unwrap());
        store.admit_build(&build).await.unwrap();
        store
            .record_build_progress(&DurableBuildProgress {
                authority: build.clone(),
                last_sequence: 2,
                durable_lsn: 9,
                completed: true,
                catch_up_boundary_lsn: Some(9),
            })
            .await
            .unwrap();
        let application = CrashState::open(crash_application_path(path));
        application.apply(seeded_operation()).await.unwrap();
        store
    };

    let stop_before = match cut {
        "pc-cc-authority-before" | "current-only-authority-before" => {
            Some(CoordinatorStage::AdmitAuthority)
        }
        "pc-cc-access-before" | "current-only-access-before" => Some(CoordinatorStage::Activate),
        "build-retirement-before" => Some(CoordinatorStage::RetireBuild),
        "completion-before" => Some(CoordinatorStage::Complete),
        _ => None,
    };
    if let Some(stop_before) = stop_before
        && store.load_state().await.unwrap().reconfiguration.is_none()
    {
        store.begin_configuration(&command).await.unwrap();
        loop {
            let state = store.load_state().await.unwrap();
            let record = state.reconfiguration.clone().unwrap();
            if record.stage == stop_before {
                break;
            }
            let runtime = Arc::new(ScaleUpCrashRuntime::new(&state, None));
            let adapter = RuntimeAdapter::new(store.clone(), runtime);
            match record.stage {
                CoordinatorStage::AdmitAuthority => {
                    let authority = admit_configuration(&command, &state).unwrap();
                    adapter
                        .execute(RuntimeEffect {
                            operation_id: OperationId::new(format!(
                                "{}:admit-authority",
                                command.operation_id
                            )),
                            sequence: state.next_effect_sequence,
                            action: RuntimeEffectAction::AdmitAuthority(Box::new(authority)),
                        })
                        .await
                        .unwrap();
                    store
                        .advance_configuration(
                            &command.operation_id,
                            CoordinatorStage::AdmitAuthority,
                            CoordinatorStage::Activate,
                            None,
                        )
                        .await
                        .unwrap();
                }
                CoordinatorStage::Activate => {
                    adapter
                        .execute(RuntimeEffect {
                            operation_id: OperationId::new(format!(
                                "{}:activate",
                                command.operation_id
                            )),
                            sequence: state.next_effect_sequence,
                            action: RuntimeEffectAction::SetAccessStatus {
                                read: AccessStatus::Granted,
                                write: AccessStatus::Granted,
                            },
                        })
                        .await
                        .unwrap();
                    store
                        .advance_configuration(
                            &command.operation_id,
                            CoordinatorStage::Activate,
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
                    adapter
                        .execute(RuntimeEffect {
                            operation_id: OperationId::new(format!(
                                "{}:retire-build-0",
                                command.operation_id
                            )),
                            sequence: state.next_effect_sequence,
                            action: RuntimeEffectAction::RetireBuild(build.build_id.clone()),
                        })
                        .await
                        .unwrap();
                    store
                        .advance_configuration(
                            &command.operation_id,
                            CoordinatorStage::RetireBuild,
                            CoordinatorStage::Complete,
                            None,
                        )
                        .await
                        .unwrap();
                }
                other => panic!("{cut}: unexpected preparatory stage {other:?}"),
            }
        }
        if terminate {
            std::process::exit(73);
        }
    }

    if terminate {
        if cut == "completion-after" {
            Coordinator::new(
                store.clone(),
                Arc::new(ScaleUpCrashRuntime::new(
                    &store.load_state().await.unwrap(),
                    None,
                )),
            )
            .ensure_configuration(command.clone())
            .await
            .unwrap();
            std::process::exit(73);
        }
        let crash_after = match cut {
            "pc-cc-authority-after" | "current-only-authority-after" => "admission",
            "pc-cc-access-after" | "current-only-access-after" => "access",
            "build-retirement-after" => "retirement",
            other => panic!("unknown scale-up configuration cut {other}"),
        };
        Coordinator::new(
            store.clone(),
            Arc::new(ScaleUpCrashRuntime::new(
                &store.load_state().await.unwrap(),
                Some(crash_after),
            )),
        )
        .ensure_configuration(command.clone())
        .await
        .unwrap();
    } else {
        Coordinator::new(
            store.clone(),
            Arc::new(ScaleUpCrashRuntime::new(
                &store.load_state().await.unwrap(),
                None,
            )),
        )
        .ensure_configuration(command.clone())
        .await
        .unwrap_or_else(|error| panic!("{cut}: {error:?}"));
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
    let application = CrashState::open(crash_application_path(path));
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

async fn execute_real_active_secondary_cut(path: &Path, after: bool, terminate: bool) {
    let (_, command, build) = scale_up_crash_fixture(false);
    let evidence = command.scale_up_evidence.clone().unwrap();
    let intent = evidence.intent().clone();
    let candidate = intent.target.clone();
    let store = if path.is_file() {
        Arc::new(SqliteStore::open_existing(path, None).unwrap())
    } else {
        let mut state = AgentState::new(StorageIdentity {
            schema_version: SCHEMA_VERSION,
            resource_uid: intent.resource_uid.clone(),
            pod_uid: PodUid::new(candidate.instance_id.as_str()),
            pvc_uid: PvcUid::new("active-secondary-pvc"),
            initialization_id: InitializationId::new("active-secondary-initialization"),
            local_identity: candidate.clone(),
            effective_policy: intent.current_policy.clone(),
        });
        state.admitted_policy = Some(intent.current_policy.clone());
        state.previous_policy = Some(intent.previous_policy.clone());
        state.highest_epoch = intent.current_configuration.epoch;
        state.previous_configuration = Some(intent.previous_configuration.clone());
        state.current_configuration = Some(intent.current_configuration.clone());
        state.scale_up_evidence = Some(evidence.clone());
        state.role = ReplicaRole::IdleSecondary;
        state.build_commands.insert(
            build.build_id.clone(),
            kuberic_protocol::command::EnsureReplicaBuild {
                operation_id: build.build_id.clone(),
                local_replica_id: candidate.replica_id,
                expected_instance_id: candidate.instance_id.clone(),
                expected_agent_generation: candidate.agent_generation.clone(),
                target: candidate.clone(),
                authority: Some(build.clone()),
                source_session_id: Some(ProcessSessionId::new("active-secondary-source")),
            },
        );
        let store = Arc::new(SqliteStore::create_authorized(path, state).unwrap());
        store
            .admit(&AdmittedAuthority {
                local_identity: candidate.clone(),
                transition_kind: Some(TransitionKind::ScaleUp),
                previous_configuration: Some(intent.previous_configuration.clone()),
                current_configuration: intent.current_configuration.clone(),
                switchover_handoff: None,
                secondary_removal: None,
                scale_up: Some(evidence.clone()),
            })
            .await
            .unwrap();
        store.admit_build(&build).await.unwrap();
        store
            .record_build_progress(&DurableBuildProgress {
                authority: build,
                last_sequence: 2,
                durable_lsn: intent.catch_up_boundary_lsn,
                completed: true,
                catch_up_boundary_lsn: Some(intent.catch_up_boundary_lsn),
            })
            .await
            .unwrap();
        store
    };
    let durable = store.load_state().await.unwrap();
    let application = Arc::new(CrashState::open(crash_application_path(path)));
    application
        .apply(Operation {
            lsn: 1,
            committed_lsn: 1,
            data: Bytes::from_static(b"acknowledged-before-active-secondary"),
        })
        .await
        .unwrap();
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
            let expected_exit = if cut.ends_with("-after") && cut != "completion-after" {
                0
            } else {
                73
            };
            assert_eq!(
                output.status.code(),
                Some(expected_exit),
                "{cut} {}: {}",
                if after { "after" } else { "before" },
                String::from_utf8_lossy(&output.stderr)
            );
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                ensure_scale_up_store_cut(&path, cut, after, false).await;
                let store = SqliteStore::open_existing(&path, None).unwrap();
                let (_, _, build) = scale_up_crash_fixture(false);
                match cut {
                    "store-initialization" => {
                        assert_eq!(
                            store.identity().await.unwrap().schema_version,
                            SCHEMA_VERSION
                        );
                    }
                    "build-authority-admission" => {
                        assert_eq!(
                            store.load_build(&build.build_id).await.unwrap(),
                            Some(build)
                        );
                    }
                    _ => {
                        let progress = store
                            .load_build_progress(&build.build_id)
                            .await
                            .unwrap()
                            .expect("durable build progress");
                        assert_eq!(progress.authority, build);
                        assert_eq!(progress.catch_up_boundary_lsn, Some(9));
                        let expected = match cut {
                            "snapshot-boundary-persistence" => (0, 4, false),
                            "source-progress-persistence" => (1, 7, false),
                            "target-progress-persistence" => (2, 9, true),
                            _ => unreachable!(),
                        };
                        assert_eq!(
                            (
                                progress.last_sequence,
                                progress.durable_lsn,
                                progress.completed
                            ),
                            expected
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
        let expected_exit = if cut.ends_with("-after") && cut != "completion-after" {
            0
        } else {
            73
        };
        assert_eq!(
            output.status.code(),
            Some(expected_exit),
            "{cut}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let interrupted = SqliteStore::open_existing(&path, None).unwrap();
        assert_eq!(
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(interrupted.load_state())
                .unwrap()
                .write_status,
            AccessStatus::Granted,
            "{cut}: same-primary durable cut intentionally closed healthy writes"
        );
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(execute_scale_up_configuration_cut(&path, cut, false));
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
        assert_eq!(
            output.status.code(),
            Some(73),
            "active-secondary {}: {}",
            if after { "after" } else { "before" },
            String::from_utf8_lossy(&output.stderr)
        );
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(execute_real_active_secondary_cut(&path, after, false));
    }
}

#[test]
#[ignore = "helper process for scale_up_exact_cut_matrix_survives_real_process_restart"]
fn scale_up_store_cut_writer_process() {
    let (Ok(path), Ok(cut), Ok(side)) = (
        env::var("KUBERIC_SCALE_UP_STORE_CUT_PATH"),
        env::var("KUBERIC_SCALE_UP_STORE_CUT"),
        env::var("KUBERIC_SCALE_UP_STORE_CUT_SIDE"),
    ) else {
        return;
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
        return;
    };
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(execute_scale_up_configuration_cut(
            Path::new(&path),
            &cut,
            true,
        ));
}

#[test]
#[ignore = "helper process for scale_up_exact_cut_matrix_survives_real_process_restart"]
fn scale_up_active_secondary_cut_writer_process() {
    let (Ok(path), Ok(side)) = (
        env::var("KUBERIC_SCALE_UP_ACTIVE_SECONDARY_PATH"),
        env::var("KUBERIC_SCALE_UP_ACTIVE_SECONDARY_SIDE"),
    ) else {
        return;
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
fn scale_up_durable_boundaries_survive_real_subprocess_termination() {
    for boundary in [
        "boundary-capture",
        "pc-cc-install",
        "current-only-install",
        "build-retirement",
        "completion-persistence",
    ] {
        let directory = tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(directory.path());
        let output = Command::new(env::current_exe().unwrap())
            .args(["--ignored", "--exact", "scale_up_crash_writer_process"])
            .env("KUBERIC_SCALE_UP_CRASH_PATH", &path)
            .env("KUBERIC_SCALE_UP_CRASH_BOUNDARY", boundary)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{boundary}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let store = Arc::new(SqliteStore::open_existing(&path, None).unwrap());
            let current_only = boundary != "pc-cc-install" && boundary != "boundary-capture";
            let (_, command, build) = scale_up_crash_fixture(current_only);
            let progress = store
                .load_build_progress(&build.build_id)
                .await
                .unwrap()
                .expect("frozen build progress");
            assert_eq!(progress.catch_up_boundary_lsn, Some(9));
            assert_eq!(progress.durable_lsn, 9);
            if boundary == "boundary-capture" {
                let mut changed = progress;
                changed.catch_up_boundary_lsn = Some(10);
                assert!(store.record_build_progress(&changed).await.is_err());
                return;
            }
            let state = store.load_state().await.unwrap();
            assert_eq!(state.write_status, AccessStatus::Granted);
            let runtime = Arc::new(ScaleUpCrashRuntime::new(&state, None));
            let completed = Coordinator::new(store.clone(), runtime)
                .ensure_configuration(command.clone())
                .await
                .unwrap();
            assert_eq!(completed.command, command);
            let recovered = store.load_state().await.unwrap();
            assert!(recovered.pending_effect.is_none());
            assert!(recovered.reconfiguration.is_none());
            assert_eq!(recovered.read_status, AccessStatus::Granted);
            assert_eq!(recovered.write_status, AccessStatus::Granted);
            assert_eq!(
                recovered.current_configuration,
                Some(command.current_configuration.clone())
            );
            assert_eq!(
                recovered.previous_configuration,
                (!current_only).then(|| {
                    command
                        .previous_configuration
                        .clone()
                        .expect("PC/CC command has previous")
                })
            );
            assert_eq!(
                recovered
                    .retained_command
                    .as_ref()
                    .map(|retained| &retained.command),
                Some(&command)
            );
            assert_eq!(
                recovered.retired_builds.contains(&build.build_id),
                current_only
            );
        });
    }
}

#[test]
#[ignore = "helper process for scale_up_durable_boundaries_survive_real_subprocess_termination"]
fn scale_up_crash_writer_process() {
    let (Ok(path), Ok(boundary)) = (
        env::var("KUBERIC_SCALE_UP_CRASH_PATH"),
        env::var("KUBERIC_SCALE_UP_CRASH_BOUNDARY"),
    ) else {
        return;
    };
    let current_only = boundary != "pc-cc-install" && boundary != "boundary-capture";
    let (state, command, build) = scale_up_crash_fixture(current_only);
    let store = Arc::new(SqliteStore::create_authorized(path, state.clone()).unwrap());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        store.admit_build(&build).await.unwrap();
        store
            .record_build_progress(&DurableBuildProgress {
                authority: build,
                last_sequence: 2,
                durable_lsn: 9,
                completed: true,
                catch_up_boundary_lsn: Some(9),
            })
            .await
            .unwrap();
        if boundary == "boundary-capture" {
            return;
        }
        let crash_after = match boundary.as_str() {
            "pc-cc-install" | "current-only-install" => Some("admission"),
            "build-retirement" => Some("retirement"),
            "completion-persistence" => None,
            _ => panic!("unknown scale-up crash boundary {boundary}"),
        };
        Coordinator::new(
            store,
            Arc::new(ScaleUpCrashRuntime::new(&state, crash_after)),
        )
        .ensure_configuration(command)
        .await
        .unwrap();
    });
    std::process::exit(0);
}

#[test]
fn scale_up_failover_replays_after_authority_and_completion_process_boundaries() {
    for boundary in ["after-authority", "after-completion"] {
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
        assert!(
            output.status.success(),
            "{boundary}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let store = Arc::new(SqliteStore::open_existing(&path, None).unwrap());
            let (_, command) = scale_up_failover_crash_fixture();
            let durable = store.load_state().await.unwrap();
            if boundary == "after-authority" {
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
            let runtime = Arc::new(ScaleUpCrashRuntime::new(&durable, None));
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
        return;
    };
    let (state, command) = scale_up_failover_crash_fixture();
    let store = Arc::new(SqliteStore::create_authorized(path, state.clone()).unwrap());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let runtime = Arc::new(ScaleUpCrashRuntime::new(&state, None));
        if boundary == "after-authority" {
            store.begin_configuration(&command).await.unwrap();
            let authority = admit_configuration(&command, &store.load_state().await.unwrap())
                .expect("first failover admission");
            let sequence = store.load_state().await.unwrap().next_effect_sequence;
            RuntimeAdapter::new(store, runtime)
                .execute(RuntimeEffect {
                    operation_id: OperationId::new(format!(
                        "{}:admit-authority",
                        command.operation_id
                    )),
                    sequence,
                    action: RuntimeEffectAction::AdmitAuthority(Box::new(authority)),
                })
                .await
                .unwrap();
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
