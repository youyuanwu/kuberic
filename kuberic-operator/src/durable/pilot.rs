//! Immutable contract and admission gate for the durable switchover pilot.
//!
//! The pilot deliberately reuses the explicit switchover operation as its
//! workflow input. Per-turn workflow and reconciliation behavior is added by
//! later phases; this module owns only identity, bounds, and Kubernetes
//! lifecycle policy.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, OnceLock},
};

use async_trait::async_trait;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kuberic_durable_execution::{
    ActivityName, ActivityRecord, ActivitySequence, ActivitySpec, CasOutcome, CheckpointEnvelope,
    CheckpointLimits, CheckpointPayload, CheckpointStore, DispatchPermit, DurableActivity,
    DurableHost, ExactBytes, ExecutionContract, ExecutionId, ExecutionSpec, HostEpoch,
    InMemoryCheckpointStore, KubernetesCheckpointOwner, KubernetesCheckpointOwnerScope,
    KubernetesCheckpointStore, KubernetesCheckpointStoreOptions, LogicalActivityId,
    PreparedActivityError, PreparedActivityResolver, StorageRevision, StoreError, StoredCheckpoint,
    TerminalOutcome, Workflow, WorkflowContext, decode_activity_input, decode_activity_result,
    encode_activity_input, encode_activity_result,
};
use rand::random;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::crd::{
    DurableOperationPhase, DurableOperationStatus, DurableSwitchoverPilotStatus,
    PendingActionStatus, StablePartitionSnapshotStatus,
};

use super::effects::{
    LabelEffectCommand, PilotEffectPreparationError, ReplicaEffectCommand, exact_label_command,
    prepare_replica_effect_command, validate_pilot_replica_action_kind,
};
use super::pilot_store::MeasuredPilotCheckpointStore;
use super::{
    Decision, OperationObservations, decide, start_switchover,
    switchover::{
        is_legal_switchover_phase_transition, validate_switchover_operation,
        validate_switchover_terminal as validate_terminal,
    },
};

// COMPLEXITY-BOUNDARY: pilot-module:start
pub const PILOT_VERSION: u32 = 2;
pub const PILOT_MAX_REPLICAS: usize = 3;
pub const PILOT_MAX_ACTIVITY_RECORDS: usize = 32;
pub const PILOT_MAX_TRANSITION_FUEL: usize = 64;
pub const PILOT_MAX_OPERATION_BYTES: usize = 3_000;
pub const PILOT_MAX_ACTIVITY_INPUT_BYTES: usize = 8_192;
pub const PILOT_MAX_ACTIVITY_RESULT_BYTES: usize = 4_096;
pub const PILOT_MAX_TERMINAL_BYTES: u64 = 4_096;
pub const PILOT_MAX_ENCODED_CHECKPOINT_BYTES: usize = 752 * 1_024;

const PILOT_ACTIVITY_NAME: &str = "kuberic.switchover.effect-boundary";
const PILOT_ACTIVITY_VERSION: u32 = 1;
const MAX_COMPLETED_MEASUREMENT_SNAPSHOTS: usize = 64;

#[derive(Clone)]
pub enum PilotCheckpointStore {
    Kubernetes(Box<KubernetesCheckpointStore>),
    InMemory(InMemoryCheckpointStore),
}

#[async_trait]
impl CheckpointStore for PilotCheckpointStore {
    async fn load(
        &self,
        execution_id: ExecutionId,
    ) -> Result<Option<StoredCheckpoint>, StoreError> {
        match self {
            Self::Kubernetes(store) => store.load(execution_id).await,
            Self::InMemory(store) => store.load(execution_id).await,
        }
    }

    async fn compare_and_swap(
        &self,
        execution_id: ExecutionId,
        expected: Option<StorageRevision>,
        checkpoint: CheckpointEnvelope,
    ) -> Result<CasOutcome, StoreError> {
        match self {
            Self::Kubernetes(store) => {
                store
                    .compare_and_swap(execution_id, expected, checkpoint)
                    .await
            }
            Self::InMemory(store) => {
                store
                    .compare_and_swap(execution_id, expected, checkpoint)
                    .await
            }
        }
    }
}

#[derive(Clone)]
enum PilotStoreFactory {
    Kubernetes(kube::Client),
    InMemory(InMemoryCheckpointStore),
}

pub type PilotHost = DurableHost<MeasuredPilotCheckpointStore>;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PilotHostKey {
    namespace: String,
    set_name: String,
    set_uid: String,
    execution_id: String,
}

/// Process-local host cache. Checkpoints, rather than this cache, remain the
/// recovery authority; retaining hosts preserves monotonic attempt counters
/// within one process epoch.
pub struct DurableSwitchoverPilotRuntime {
    factory: PilotStoreFactory,
    host_epoch: HostEpoch,
    hosts: Mutex<HashMap<PilotHostKey, Arc<Mutex<PilotHost>>>>,
    completed_measurements:
        Mutex<HashMap<PilotHostKey, super::pilot_store::PilotCheckpointMeasurementsSnapshot>>,
}

impl DurableSwitchoverPilotRuntime {
    pub fn kubernetes(client: kube::Client) -> Self {
        Self {
            factory: PilotStoreFactory::Kubernetes(client),
            host_epoch: HostEpoch::from_bytes(random()),
            hosts: Mutex::new(HashMap::new()),
            completed_measurements: Mutex::new(HashMap::new()),
        }
    }

    pub fn in_memory(store: InMemoryCheckpointStore) -> Self {
        Self {
            factory: PilotStoreFactory::InMemory(store),
            host_epoch: HostEpoch::from_bytes(random()),
            hosts: Mutex::new(HashMap::new()),
            completed_measurements: Mutex::new(HashMap::new()),
        }
    }

    pub async fn host(
        &self,
        namespace: &str,
        set_name: &str,
        set_uid: &str,
        reference: &DurableSwitchoverPilotStatus,
    ) -> Result<Arc<Mutex<PilotHost>>, String> {
        let execution_id = execution_id(reference)?;
        let key = PilotHostKey {
            namespace: namespace.to_string(),
            set_name: set_name.to_string(),
            set_uid: set_uid.to_string(),
            execution_id: reference.execution_id.clone(),
        };
        let mut hosts = self.hosts.lock().await;
        if let Some(host) = hosts.get(&key) {
            return Ok(host.clone());
        }
        self.completed_measurements.lock().await.remove(&key);
        let store = match &self.factory {
            PilotStoreFactory::Kubernetes(client) => {
                let options = checkpoint_store_options(namespace, set_name, set_uid)?;
                PilotCheckpointStore::Kubernetes(Box::new(
                    KubernetesCheckpointStore::with_options(client.clone(), namespace, options)
                        .map_err(|error| {
                            format!("construct durable switchover checkpoint store: {error}")
                        })?,
                ))
            }
            PilotStoreFactory::InMemory(store) => PilotCheckpointStore::InMemory(store.clone()),
        };
        let host = Arc::new(Mutex::new(DurableHost::new(
            MeasuredPilotCheckpointStore::new(execution_id, store),
            self.host_epoch,
            checkpoint_limits(),
        )));
        hosts.insert(key.clone(), host.clone());
        let expected_name = KubernetesCheckpointStore::object_name(execution_id);
        if expected_name != reference.checkpoint_name {
            hosts.remove(&key);
            return Err("durable switchover checkpoint identity changed".to_string());
        }
        Ok(host)
    }

    pub async fn forget(&self, namespace: &str, set_name: &str, set_uid: &str, execution_id: &str) {
        let key = PilotHostKey {
            namespace: namespace.to_string(),
            set_name: set_name.to_string(),
            set_uid: set_uid.to_string(),
            execution_id: execution_id.to_string(),
        };
        let host = self.hosts.lock().await.remove(&key);
        if let Some(host) = host {
            let measurements = host.lock().await.store().measurements();
            let mut completed = self.completed_measurements.lock().await;
            if completed.len() == MAX_COMPLETED_MEASUREMENT_SNAPSHOTS {
                if let Some(eviction_candidate) = completed.keys().next().cloned() {
                    completed.remove(&eviction_candidate);
                }
            }
            completed.insert(key, measurements);
        }
    }

    pub async fn host_count(&self) -> usize {
        self.hosts.lock().await.len()
    }

    pub async fn measurements(
        &self,
        namespace: &str,
        set_name: &str,
        set_uid: &str,
        execution_id: &str,
    ) -> Option<super::pilot_store::PilotCheckpointMeasurementsSnapshot> {
        let key = PilotHostKey {
            namespace: namespace.to_string(),
            set_name: set_name.to_string(),
            set_uid: set_uid.to_string(),
            execution_id: execution_id.to_string(),
        };
        if let Some(host) = self.hosts.lock().await.get(&key).cloned() {
            return Some(host.lock().await.store().measurements());
        }
        self.completed_measurements.lock().await.get(&key).copied()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableSwitchoverPilotInput {
    pub version: u32,
    pub execution_id: String,
    pub initial_operation: DurableOperationStatus,
}

/// Compact mutable protocol projection replayed over the immutable initial
/// operation in the workflow input.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableSwitchoverState {
    pub phase: DurableOperationPhase,
    pub frozen_lsn: Option<i64>,
    pub next_secondary_index: u32,
    pub phase_deadline_unix_seconds: i64,
    pub pending_action: Option<PendingActionStatus>,
    pub last_error: Option<String>,
}

impl DurableSwitchoverState {
    pub fn from_operation(operation: &DurableOperationStatus) -> Self {
        Self {
            phase: operation.phase,
            frozen_lsn: operation.frozen_lsn,
            next_secondary_index: operation.next_secondary_index,
            phase_deadline_unix_seconds: operation.phase_deadline_unix_seconds,
            pending_action: operation.pending_action.clone(),
            last_error: operation.last_error.clone(),
        }
    }

    pub fn apply_to(
        &self,
        initial: &DurableOperationStatus,
    ) -> Result<DurableOperationStatus, String> {
        let mut operation = initial.clone();
        operation.phase = self.phase;
        operation.frozen_lsn = self.frozen_lsn;
        operation.next_secondary_index = self.next_secondary_index;
        operation.phase_deadline_unix_seconds = self.phase_deadline_unix_seconds;
        operation.pending_action = self.pending_action.clone();
        operation.last_error = self.last_error.clone();
        validate_transition(initial, &operation)?;
        Ok(operation)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableSwitchoverActivityInput {
    pub version: u32,
    pub state: DurableSwitchoverState,
    #[serde(default)]
    pub kind: PilotActivityKind,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PilotActivityKind {
    #[default]
    PassiveObservation,
    PreparedReplica {
        command: ReplicaEffectCommand,
    },
    PreparedLabel {
        command: LabelEffectCommand,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PilotActivityAccounting {
    pub external_effect_count: u64,
    pub passive_observation_count: u64,
}

impl PilotActivityAccounting {
    pub const fn new(external_effect_count: u64, passive_observation_count: u64) -> Self {
        Self {
            external_effect_count,
            passive_observation_count,
        }
    }

    pub fn matches_completed_activity_count(self, completed_activity_count: u64) -> bool {
        self.external_effect_count
            .checked_add(self.passive_observation_count)
            == Some(completed_activity_count)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum DurableSwitchoverStepResult {
    Advance {
        operation: DurableSwitchoverState,
    },
    ProvenNoAdmission {
        operation: DurableSwitchoverState,
        action_id: String,
        redelivery: u8,
    },
    Complete {
        operation: DurableSwitchoverState,
        snapshot: StablePartitionSnapshotStatus,
        compensated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accounting: Option<PilotActivityAccounting>,
    },
    Stopped {
        operation: DurableSwitchoverState,
        message: String,
    },
}

pub struct DurableSwitchoverActivity;

impl DurableActivity for DurableSwitchoverActivity {
    type Input = DurableSwitchoverActivityInput;
    type Output = DurableSwitchoverStepResult;

    const NAME: &'static str = PILOT_ACTIVITY_NAME;
    const VERSION: u32 = PILOT_ACTIVITY_VERSION;
    const MAX_INPUT_BYTES: u64 = PILOT_MAX_ACTIVITY_INPUT_BYTES as u64;
    const MAX_RESULT_BYTES: u64 = PILOT_MAX_ACTIVITY_RESULT_BYTES as u64;
}

#[derive(Clone, Debug, PartialEq)]
pub enum DurableSwitchoverPilotTerminal {
    Complete {
        operation: DurableOperationStatus,
        snapshot: StablePartitionSnapshotStatus,
        compensated: bool,
        accounting: PilotActivityAccounting,
    },
    Stopped {
        operation: Option<DurableOperationStatus>,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum DurableSwitchoverTerminalRecord {
    Complete {
        state: DurableSwitchoverState,
        snapshot: StablePartitionSnapshotStatus,
        compensated: bool,
        #[serde(default)]
        accounting: PilotActivityAccounting,
    },
    Stopped {
        state: Option<DurableSwitchoverState>,
        message: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum DurableSwitchoverTerminalAccountingRecord {
    Complete {
        #[serde(default)]
        compensated: bool,
        #[serde(default)]
        accounting: Option<PilotActivityAccounting>,
    },
    Stopped {},
}

// COMPLEXITY-BOUNDARY: pilot-workflow:start
pub struct DurableSwitchoverWorkflow;

// COMPLEXITY-BOUNDARY: pilot-workflow-body:start
#[async_trait]
impl Workflow for DurableSwitchoverWorkflow {
    async fn run(&self, context: &mut WorkflowContext<'_>, input: ExactBytes) -> TerminalOutcome {
        let input: DurableSwitchoverPilotInput = match serde_json::from_slice(input.as_slice()) {
            Ok(input) => input,
            Err(error) => {
                return terminal_failure(None, format!("decode pilot workflow input: {error}"));
            }
        };
        if input.version != PILOT_VERSION {
            return terminal_failure(
                Some(input.initial_operation),
                format!("unsupported pilot workflow version {}", input.version),
            );
        }
        if input.execution_id != encode_execution_id(context.execution_id()) {
            return terminal_failure(
                Some(input.initial_operation),
                "pilot workflow execution identity mismatch".to_string(),
            );
        }
        let initial = input.initial_operation;
        if let Err(error) = validate_pilot_operation(&initial) {
            return terminal_failure(Some(initial), error);
        }
        if let Err(error) = validate_switchover_operation(&initial) {
            return terminal_failure(Some(initial), error);
        }
        let mut state = DurableSwitchoverState::from_operation(&initial);
        let mut no_admission_redeliveries = std::collections::BTreeMap::<String, u8>::new();

        for _ in 0..PILOT_MAX_TRANSITION_FUEL {
            let operation = match state.apply_to(&initial) {
                Ok(operation) => operation,
                Err(error) => return terminal_failure(None, error),
            };
            match advance_effect_free(&operation) {
                Ok(Some(next)) => {
                    if let Err(error) = validate_phase_transition(&operation, &next) {
                        return terminal_failure(Some(operation), error);
                    }
                    if next.phase == DurableOperationPhase::Poisoned {
                        return terminal_failure(
                            Some(next.clone()),
                            next.last_error.clone().unwrap_or_else(|| {
                                "durable switchover entered poisoned state".to_string()
                            }),
                        );
                    }
                    state = DurableSwitchoverState::from_operation(&next);
                    continue;
                }
                Ok(None) => {}
                Err(error) => return terminal_failure(Some(operation), error),
            }
            let result = match context
                .call::<DurableSwitchoverActivity>(DurableSwitchoverActivityInput {
                    version: PILOT_VERSION,
                    state: state.clone(),
                    kind: PilotActivityKind::PassiveObservation,
                })
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    return terminal_failure(
                        Some(operation),
                        format!("decode pilot activity result: {error}"),
                    );
                }
            };
            match result {
                DurableSwitchoverStepResult::Advance {
                    operation: next_state,
                } => {
                    if next_state == state {
                        return terminal_failure(
                            Some(operation),
                            "durable switchover activity made no protocol progress".to_string(),
                        );
                    }
                    let next = match next_state.apply_to(&initial) {
                        Ok(next) => next,
                        Err(error) => return terminal_failure(Some(operation), error),
                    };
                    if let Err(error) = validate_phase_transition(&operation, &next) {
                        return terminal_failure(Some(operation), error);
                    }
                    state = next_state;
                }
                DurableSwitchoverStepResult::ProvenNoAdmission {
                    operation: next_state,
                    action_id,
                    redelivery,
                } => {
                    let next = match next_state.apply_to(&initial) {
                        Ok(next) => next,
                        Err(error) => return terminal_failure(Some(operation), error),
                    };
                    if let Err(error) =
                        validate_no_admission_transition(&operation, &next, &action_id)
                    {
                        return terminal_failure(Some(operation), error);
                    }
                    let expected_action_id = operation
                        .pending_action
                        .as_ref()
                        .map(|pending| pending.action_id.as_str());
                    if expected_action_id != Some(action_id.as_str()) || redelivery != 1 {
                        return terminal_failure(
                            Some(operation),
                            "invalid proven-no-admission redelivery evidence".to_string(),
                        );
                    }
                    let count = no_admission_redeliveries.entry(action_id).or_default();
                    *count = count.saturating_add(1);
                    if *count > 1 {
                        return terminal_failure(
                            Some(operation),
                            "correlated action exceeded one proven-no-admission redelivery"
                                .to_string(),
                        );
                    }
                    state = next_state;
                }
                DurableSwitchoverStepResult::Complete {
                    operation: completed_state,
                    snapshot,
                    compensated,
                    accounting,
                } => {
                    let completed = match completed_state.apply_to(&initial) {
                        Ok(completed) => completed,
                        Err(error) => return terminal_failure(Some(operation), error),
                    };
                    if let Err(error) =
                        validate_completion_transition(&operation, &completed, compensated)
                    {
                        return terminal_failure(Some(operation), error);
                    }
                    if let Err(error) = validate_terminal(&completed, &snapshot, compensated) {
                        return terminal_failure(Some(operation), error);
                    }
                    let Some(accounting) = accounting else {
                        return terminal_failure(
                            Some(operation),
                            "pilot completion is missing authoritative activity accounting"
                                .to_string(),
                        );
                    };
                    if let Err(error) = validate_terminal_activity_accounting(
                        &initial,
                        &completed,
                        compensated,
                        accounting,
                    ) {
                        return terminal_failure(Some(operation), error);
                    }
                    return terminal_success(DurableSwitchoverPilotTerminal::Complete {
                        operation: completed,
                        snapshot,
                        compensated,
                        accounting,
                    });
                }
                DurableSwitchoverStepResult::Stopped {
                    operation: stopped_state,
                    message,
                } => {
                    let stopped = match stopped_state.apply_to(&initial) {
                        Ok(stopped) => stopped,
                        Err(error) => return terminal_failure(Some(operation), error),
                    };
                    if stopped.phase != crate::crd::DurableOperationPhase::Poisoned
                        || stopped.pending_action.is_some()
                    {
                        return terminal_failure(
                            Some(operation),
                            "stopped durable switchover is not poisoned and effect-free"
                                .to_string(),
                        );
                    }
                    return terminal_failure(Some(stopped), message);
                }
            }
        }

        terminal_failure(
            state.apply_to(&initial).ok(),
            format!("durable switchover exhausted its {PILOT_MAX_TRANSITION_FUEL}-transition fuel"),
        )
    }
}
// COMPLEXITY-BOUNDARY: pilot-workflow-body:end

fn advance_effect_free(
    operation: &DurableOperationStatus,
) -> Result<Option<DurableOperationStatus>, String> {
    if operation.pending_action.is_some() {
        return Ok(None);
    }
    use DurableOperationPhase as Phase;
    if !matches!(
        operation.phase,
        Phase::Revoke
            | Phase::DemoteOldPrimary
            | Phase::PromoteTarget
            | Phase::DistributeEpoch
            | Phase::UpdateCatchUpConfiguration
            | Phase::WaitForCatchUpQuorum
            | Phase::UpdateCurrentConfiguration
            | Phase::LabelTargetPrimary
            | Phase::LabelOldSecondary
            | Phase::RestorePreviousConfiguration
            | Phase::CompensatePromoteOldPrimary
            | Phase::CompensateDistributeEpoch
            | Phase::CompensateCatchUpConfiguration
            | Phase::CompensateCurrentConfiguration
            | Phase::CompensateLabelOldPrimary
            | Phase::CompensateLabelTargetSecondary
    ) {
        return Ok(None);
    }
    let deterministic_now = operation
        .phase_deadline_unix_seconds
        .saturating_sub(super::ACTION_DEADLINE_SECONDS);
    match decide(operation, &OperationObservations::new(), deterministic_now)? {
        Decision::Persist(next) => Ok(Some(next)),
        Decision::Wait => Ok(None),
        other => Err(format!(
            "effect-free switchover transition produced external decision: {other:?}"
        )),
    }
}

pub fn encode_step_result(result: &DurableSwitchoverStepResult) -> Result<ExactBytes, String> {
    encode_activity_result::<DurableSwitchoverActivity>(result)
        .map_err(|error| format!("serialize pilot activity result: {error}"))
}

pub fn decode_activity_input_state(input: &ExactBytes) -> Result<DurableSwitchoverState, String> {
    Ok(decode_pilot_activity_input(input)?.state)
}

pub fn decode_pilot_activity_input(
    input: &ExactBytes,
) -> Result<DurableSwitchoverActivityInput, String> {
    let input = decode_activity_input::<DurableSwitchoverActivity>(input)
        .map_err(|error| format!("decode current durable switchover activity: {error}"))?;
    if input.version != PILOT_VERSION {
        return Err(format!(
            "unsupported durable switchover activity version {}",
            input.version
        ));
    }
    Ok(input)
}

pub fn validate_prepared_activity(
    operation: &DurableOperationStatus,
    kind: &PilotActivityKind,
) -> Result<(), String> {
    match kind {
        PilotActivityKind::PassiveObservation => Ok(()),
        PilotActivityKind::PreparedReplica { command } => {
            let pending = operation
                .pending_action
                .as_ref()
                .ok_or_else(|| "prepared replica activity has no pending action".to_string())?;
            let expected = ReplicaEffectCommand::from_pending(pending)?;
            if &expected != command {
                return Err(
                    "prepared replica activity differs from frozen pending action".to_string(),
                );
            }
            let action = kuberic_core::grpc::convert::decode_direct_correlated_action_payload(
                &command.action_payload,
            )
            .map_err(|error| format!("decode prepared replica activity: {error}"))?;
            let signature_matches = action.signature() == command.action_signature;
            let kind_matches = validate_pilot_replica_action_kind(pending.kind, &action);
            let fixed_semantics_match =
                action_matches_fixed_operation_semantics(operation, pending, &action);
            if !signature_matches || !kind_matches || !fixed_semantics_match {
                return Err(format!(
                    "prepared replica activity has invalid action identity \
                     (kind={:?}, signature_matches={signature_matches}, \
                     kind_matches={kind_matches}, \
                     fixed_semantics_match={fixed_semantics_match})",
                    pending.kind
                ));
            }
            Ok(())
        }
        PilotActivityKind::PreparedLabel { command } => {
            let pending = operation
                .pending_action
                .as_ref()
                .ok_or_else(|| "prepared label activity has no pending action".to_string())?;
            let expected_uid = operation
                .previous_snapshot
                .members
                .iter()
                .chain(operation.target_snapshot.members.iter())
                .find(|member| member.id == command.target_id)
                .map(|member| member.instance_id.as_str());
            let expected_role = match pending.kind {
                crate::crd::DurableActionKind::LabelTargetPrimary
                | crate::crd::DurableActionKind::CompensateLabelOldPrimary => Some("primary"),
                crate::crd::DurableActionKind::LabelOldSecondary
                | crate::crd::DurableActionKind::CompensateLabelTargetSecondary => {
                    Some("secondary")
                }
                _ => None,
            };
            if command.target_id != pending.target_id
                || command.expected_uid != pending.target_instance_id
                || expected_uid != Some(command.expected_uid.as_str())
                || command.pod_name.is_empty()
                || expected_role != Some(command.role.as_str())
                || !command.has_valid_identity_signature()
            {
                return Err("prepared label activity has invalid UID-fenced identity".to_string());
            }
            Ok(())
        }
    }
}

fn action_matches_fixed_operation_semantics(
    operation: &DurableOperationStatus,
    pending: &PendingActionStatus,
    action: &kuberic_core::types::DurableReplicaAction,
) -> bool {
    use kuberic_core::types::{DurableReplicaAction, Role};
    let target_epoch = &operation.target_snapshot.epoch;
    let has_target_epoch = |epoch: &kuberic_core::types::Epoch| {
        epoch.data_loss_number == target_epoch.data_loss_number
            && epoch.configuration_number == target_epoch.configuration_number
    };
    match action {
        DurableReplicaAction::ChangeRole { epoch, role } => {
            let expected_role =
                pending
                    .desired_postcondition
                    .role
                    .as_ref()
                    .map(|role| match role {
                        crate::crd::StableReplicaRoleStatus::Primary => Role::Primary,
                        crate::crd::StableReplicaRoleStatus::ActiveSecondary => {
                            Role::ActiveSecondary
                        }
                    });
            has_target_epoch(epoch) && expected_role.as_ref() == Some(role)
        }
        DurableReplicaAction::UpdateEpoch { epoch } => has_target_epoch(epoch),
        _ => true,
    }
}

pub fn decode_activity_step_result(
    result: &ExactBytes,
) -> Result<DurableSwitchoverStepResult, String> {
    decode_activity_result::<DurableSwitchoverActivity>(result)
        .map_err(|error| format!("decode completed durable switchover activity: {error}"))
}

pub fn decode_terminal(
    outcome: &TerminalOutcome,
    initial: &DurableOperationStatus,
) -> Result<DurableSwitchoverPilotTerminal, String> {
    let record: DurableSwitchoverTerminalRecord =
        serde_json::from_slice(outcome.payload().as_slice())
            .map_err(|error| format!("decode durable switchover terminal outcome: {error}"))?;
    Ok(match record {
        DurableSwitchoverTerminalRecord::Complete {
            state,
            snapshot,
            compensated,
            accounting,
        } => DurableSwitchoverPilotTerminal::Complete {
            operation: state.apply_to(initial)?,
            snapshot,
            compensated,
            accounting,
        },
        DurableSwitchoverTerminalRecord::Stopped { state, message } => {
            DurableSwitchoverPilotTerminal::Stopped {
                operation: state.map(|state| state.apply_to(initial)).transpose()?,
                message,
            }
        }
    })
}

pub fn decode_terminal_activity_accounting(
    outcome: &TerminalOutcome,
    completed_activity_count: u64,
) -> Result<Option<PilotActivityAccounting>, String> {
    let record: DurableSwitchoverTerminalAccountingRecord =
        serde_json::from_slice(outcome.payload().as_slice()).map_err(|error| {
            format!("decode durable switchover terminal activity accounting: {error}")
        })?;
    Ok(match (outcome, record) {
        (
            TerminalOutcome::Succeeded(_),
            DurableSwitchoverTerminalAccountingRecord::Complete {
                compensated,
                accounting: Some(accounting),
            },
        ) if validate_terminal_accounting_shape(
            None,
            compensated,
            accounting,
            completed_activity_count,
        )
        .is_ok() =>
        {
            Some(accounting)
        }
        _ => None,
    })
}

pub fn validate_loaded_terminal(
    reference: &DurableSwitchoverPilotStatus,
    outcome: &TerminalOutcome,
) -> Result<DurableSwitchoverPilotTerminal, String> {
    let initial = initial_operation(reference)?;
    validate_pilot_admission(&initial)?;
    let terminal = decode_terminal(outcome, &initial)?;
    match (outcome, &terminal) {
        (
            TerminalOutcome::Succeeded(_),
            DurableSwitchoverPilotTerminal::Complete {
                operation,
                snapshot,
                compensated,
                accounting,
            },
        ) => {
            validate_transition(&initial, operation)?;
            validate_terminal(operation, snapshot, *compensated)?;
            validate_terminal_activity_accounting(&initial, operation, *compensated, *accounting)?;
        }
        (
            TerminalOutcome::Failed(_),
            DurableSwitchoverPilotTerminal::Stopped {
                operation: Some(operation),
                ..
            },
        ) => {
            validate_transition(&initial, operation)?;
        }
        (TerminalOutcome::Failed(_), DurableSwitchoverPilotTerminal::Stopped { .. }) => {}
        _ => {
            return Err(
                "kernel terminal outcome kind does not match durable switchover payload"
                    .to_string(),
            );
        }
    }
    Ok(terminal)
}

pub enum PilotAdapterDecision {
    Observe(Box<DurableSwitchoverStepResult>),
    AwaitEvidence,
    External(Box<Decision>),
}

pub fn evaluate_adapter_step(
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
    now: i64,
) -> Result<PilotAdapterDecision, String> {
    Ok(match decide(operation, observations, now)? {
        Decision::Persist(operation)
            if operation.phase == crate::crd::DurableOperationPhase::Poisoned =>
        {
            let message = operation
                .last_error
                .clone()
                .unwrap_or_else(|| "durable switchover entered poisoned state".to_string());
            PilotAdapterDecision::Observe(Box::new(DurableSwitchoverStepResult::Stopped {
                operation: DurableSwitchoverState::from_operation(&operation),
                message,
            }))
        }
        Decision::Persist(operation) => {
            PilotAdapterDecision::Observe(Box::new(DurableSwitchoverStepResult::Advance {
                operation: DurableSwitchoverState::from_operation(&operation),
            }))
        }
        Decision::Complete {
            operation,
            snapshot,
            compensated,
        } => PilotAdapterDecision::Observe(Box::new(DurableSwitchoverStepResult::Complete {
            operation: DurableSwitchoverState::from_operation(&operation),
            snapshot,
            compensated,
            accounting: None,
        })),
        Decision::Wait => PilotAdapterDecision::AwaitEvidence,
        external @ (Decision::Execute { .. }
        | Decision::PatchPodRole { .. }
        | Decision::PatchPodRoleExactUid { .. }) => {
            PilotAdapterDecision::External(Box::new(external))
        }
        other => {
            return Err(format!(
                "unsupported explicit switchover decision reached pilot adapter: {other:?}"
            ));
        }
    })
}

pub struct PilotPreparedActivityResolver<'a> {
    initial: &'a DurableOperationStatus,
    observations: &'a OperationObservations,
    addressed_instances: &'a BTreeMap<i64, kuberic_core::types::ReplicaInstanceId>,
    now: i64,
}

impl<'a> PilotPreparedActivityResolver<'a> {
    pub fn new(
        initial: &'a DurableOperationStatus,
        observations: &'a OperationObservations,
        addressed_instances: &'a BTreeMap<i64, kuberic_core::types::ReplicaInstanceId>,
        now: i64,
    ) -> Self {
        Self {
            initial,
            observations,
            addressed_instances,
            now,
        }
    }

    fn prepare(
        &self,
        logical: &DurableSwitchoverActivityInput,
    ) -> Result<DurableSwitchoverActivityInput, PreparedActivityError> {
        if logical.version != PILOT_VERSION || logical.kind != PilotActivityKind::PassiveObservation
        {
            return Err(PreparedActivityError::Validation);
        }
        let operation = logical
            .state
            .apply_to(self.initial)
            .map_err(|_| PreparedActivityError::Validation)?;
        let decision = evaluate_adapter_step(&operation, self.observations, self.now)
            .map_err(|_| PreparedActivityError::Derivation)?;
        let mut prepared = logical.clone();
        match decision {
            PilotAdapterDecision::Observe(_) => {}
            PilotAdapterDecision::AwaitEvidence => {
                if operation.pending_action.is_some() {
                    return Err(PreparedActivityError::Derivation);
                }
            }
            PilotAdapterDecision::External(decision) => match *decision {
                Decision::Execute {
                    target_id,
                    action_id,
                    action,
                } => {
                    let pending = operation
                        .pending_action
                        .as_ref()
                        .ok_or(PreparedActivityError::Validation)?;
                    if pending.action_id != action_id || pending.target_id != target_id {
                        return Err(PreparedActivityError::Validation);
                    }
                    let observed = self
                        .observations
                        .get(&target_id)
                        .ok_or(PreparedActivityError::Derivation)?;
                    let addressed = self
                        .addressed_instances
                        .get(&target_id)
                        .ok_or(PreparedActivityError::Derivation)?;
                    let (planned, command) = prepare_replica_effect_command(
                        pending,
                        &observed.status,
                        addressed,
                        &action,
                    )
                    .map_err(preparation_error)?;
                    let mut operation = operation;
                    operation.pending_action = Some(planned);
                    prepared.state = DurableSwitchoverState::from_operation(&operation);
                    prepared.kind = PilotActivityKind::PreparedReplica { command };
                }
                Decision::PatchPodRole { target_id, role } => {
                    let command =
                        exact_label_command(&operation, target_id, &role, self.observations)
                            .map_err(|_| PreparedActivityError::Derivation)?;
                    prepared.kind = PilotActivityKind::PreparedLabel { command };
                }
                Decision::PatchPodRoleExactUid {
                    target_id,
                    expected_uid,
                    role,
                } => {
                    let command =
                        exact_label_command(&operation, target_id, &role, self.observations)
                            .map_err(|_| PreparedActivityError::Derivation)?;
                    if command.expected_uid != expected_uid {
                        return Err(PreparedActivityError::Validation);
                    }
                    prepared.kind = PilotActivityKind::PreparedLabel { command };
                }
                _ => return Err(PreparedActivityError::Validation),
            },
        }
        Ok(prepared)
    }

    fn validate_recorded(
        &self,
        logical: &DurableSwitchoverActivityInput,
        recorded: &DurableSwitchoverActivityInput,
    ) -> Result<(), PreparedActivityError> {
        if logical.version != PILOT_VERSION
            || recorded.version != PILOT_VERSION
            || logical.kind != PilotActivityKind::PassiveObservation
        {
            return Err(PreparedActivityError::Validation);
        }
        let mut logical_predecessor = recorded.state.clone();
        if let Some(pending) = logical_predecessor.pending_action.as_mut() {
            pending.dispatch_agent_generation = None;
            pending.dispatch_agent_control_version = None;
            pending.dispatch_observed_runtime_epoch = None;
            pending.dispatch_action_payload.clear();
        }
        if recorded.state != logical.state && logical_predecessor != logical.state {
            return Err(PreparedActivityError::Validation);
        }
        let operation = recorded
            .state
            .apply_to(self.initial)
            .map_err(|_| PreparedActivityError::Validation)?;
        if matches!(recorded.kind, PilotActivityKind::PassiveObservation)
            && recorded.state != logical.state
        {
            return Err(PreparedActivityError::Validation);
        }
        validate_prepared_activity(&operation, &recorded.kind)
            .map_err(|_| PreparedActivityError::Validation)
    }
}

impl PreparedActivityResolver for PilotPreparedActivityResolver<'_> {
    fn resolve(
        &self,
        logical: &ActivitySpec,
        recorded: Option<&ActivitySpec>,
    ) -> Result<ActivitySpec, PreparedActivityError> {
        let logical_input = decode_activity_input::<DurableSwitchoverActivity>(logical.input())
            .map_err(|_| PreparedActivityError::Encoding)?;
        if let Some(recorded) = recorded {
            if recorded.name() != logical.name()
                || recorded.max_result_bytes() != logical.max_result_bytes()
            {
                return Ok(logical.clone());
            }
            let Ok(recorded_input) =
                decode_activity_input::<DurableSwitchoverActivity>(recorded.input())
            else {
                return Ok(logical.clone());
            };
            if self
                .validate_recorded(&logical_input, &recorded_input)
                .is_err()
            {
                return Ok(logical.clone());
            }
            return Ok(recorded.clone());
        }
        let prepared = self.prepare(&logical_input)?;
        let spec = activity_spec(&prepared).map_err(|_| PreparedActivityError::Encoding)?;
        let actual_bytes = u64::try_from(spec.input().as_slice().len())
            .map_err(|_| PreparedActivityError::Encoding)?;
        if actual_bytes > DurableSwitchoverActivity::MAX_INPUT_BYTES {
            return Err(PreparedActivityError::InputTooLarge {
                actual_bytes,
                max_bytes: DurableSwitchoverActivity::MAX_INPUT_BYTES,
            });
        }
        Ok(spec)
    }
}

fn preparation_error(error: PilotEffectPreparationError) -> PreparedActivityError {
    match error {
        PilotEffectPreparationError::WaitForExactIncarnation
        | PilotEffectPreparationError::WaitForSupportedProtocol => {
            PreparedActivityError::Derivation
        }
        PilotEffectPreparationError::InvalidCommand => PreparedActivityError::Validation,
    }
}

pub struct PilotPermitGuard {
    permit: Option<DispatchPermit>,
}

impl PilotPermitGuard {
    pub fn new(permit: DispatchPermit) -> Self {
        Self {
            permit: Some(permit),
        }
    }

    pub fn consume_for(
        &mut self,
        operation: &DurableOperationStatus,
        prepared: &PilotActivityKind,
        expected_activity: &LogicalActivityId,
        attempt_id: kuberic_durable_execution::AttemptId,
    ) -> Result<DispatchPermit, String> {
        let expected = activity_spec(&DurableSwitchoverActivityInput {
            version: PILOT_VERSION,
            state: DurableSwitchoverState::from_operation(operation),
            kind: prepared.clone(),
        })?;
        let permit = self
            .permit
            .as_ref()
            .ok_or_else(|| "durable switchover dispatch permit was already consumed".to_string())?;
        if permit.attempt_id() != attempt_id
            || permit.activity() != expected_activity
            || permit.activity().spec() != &expected
        {
            return Err(
                "durable switchover dispatch permit does not match prepared operation or attempt"
                    .to_string(),
            );
        }
        Ok(self
            .permit
            .take()
            .expect("permit existence checked before consumption"))
    }

    pub fn activity(&self) -> Option<&LogicalActivityId> {
        self.permit.as_ref().map(DispatchPermit::activity)
    }
}

fn activity_spec(input: &DurableSwitchoverActivityInput) -> Result<ActivitySpec, String> {
    Ok(ActivitySpec::new(
        ActivityName::new(PILOT_ACTIVITY_NAME, PILOT_ACTIVITY_VERSION)
            .map_err(|error| format!("construct pilot activity name: {error}"))?,
        encode_activity_input::<DurableSwitchoverActivity>(input)
            .map_err(|error| format!("serialize pilot activity input: {error}"))?,
        PILOT_MAX_ACTIVITY_RESULT_BYTES as u64,
    ))
}

fn validate_transition(
    initial: &DurableOperationStatus,
    next: &DurableOperationStatus,
) -> Result<(), String> {
    if next.operation_id != initial.operation_id
        || next.execution_id != initial.execution_id
        || next.kind != initial.kind
        || next.previous_snapshot != initial.previous_snapshot
        || next.target_snapshot != initial.target_snapshot
        || next.old_primary_id != initial.old_primary_id
        || next.target_primary_id != initial.target_primary_id
    {
        return Err("durable switchover activity changed immutable operation identity".to_string());
    }
    validate_pilot_operation(next)
}

fn validate_phase_transition(
    current: &DurableOperationStatus,
    next: &DurableOperationStatus,
) -> Result<(), String> {
    if is_legal_switchover_phase_transition(current.phase, next.phase) {
        Ok(())
    } else {
        Err(format!(
            "invalid durable switchover phase transition {:?} -> {:?}",
            current.phase, next.phase
        ))
    }
}

fn validate_no_admission_transition(
    current: &DurableOperationStatus,
    next: &DurableOperationStatus,
    action_id: &str,
) -> Result<(), String> {
    if current.phase != next.phase {
        return Err("proven-no-admission changed switchover phase".to_string());
    }
    let Some(current_pending) = current.pending_action.as_ref() else {
        return Err("proven-no-admission has no current pending action".to_string());
    };
    let Some(next_pending) = next.pending_action.as_ref() else {
        return Err("proven-no-admission removed the pending action".to_string());
    };
    if current_pending.action_id != action_id || next_pending.action_id != action_id {
        return Err("proven-no-admission changed correlated action identity".to_string());
    }
    let mut normalized_current = current.clone();
    let mut normalized_next = next.clone();
    normalized_current.last_error = None;
    normalized_next.last_error = None;
    for pending in [
        normalized_current.pending_action.as_mut().unwrap(),
        normalized_next.pending_action.as_mut().unwrap(),
    ] {
        pending.last_error = None;
        pending.dispatch_agent_generation = None;
        pending.dispatch_agent_control_version = None;
        pending.dispatch_observed_runtime_epoch = None;
        pending.dispatch_action_payload.clear();
    }
    if normalized_current != normalized_next {
        return Err("proven-no-admission changed non-dispatch operation state".to_string());
    }
    Ok(())
}

fn validate_completion_transition(
    current: &DurableOperationStatus,
    completed: &DurableOperationStatus,
    compensated: bool,
) -> Result<(), String> {
    use crate::crd::DurableOperationPhase as Phase;
    let allowed = if compensated {
        matches!(
            (current.phase, completed.phase),
            (Phase::CompensateFinalize, Phase::Failed) | (Phase::Failed, Phase::Failed)
        )
    } else {
        (current.phase, completed.phase) == (Phase::Finalize, Phase::Completed)
    };
    if allowed {
        Ok(())
    } else {
        Err(format!(
            "invalid durable switchover completion transition {:?} -> {:?}",
            current.phase, completed.phase
        ))
    }
}

fn terminal_success(terminal: DurableSwitchoverPilotTerminal) -> TerminalOutcome {
    match encode_terminal(&terminal) {
        Ok(payload) => TerminalOutcome::succeeded(payload),
        Err(error) => terminal_failure(None, error),
    }
}

fn encode_terminal(terminal: &DurableSwitchoverPilotTerminal) -> Result<ExactBytes, String> {
    let record = match terminal {
        DurableSwitchoverPilotTerminal::Complete {
            operation,
            snapshot,
            compensated,
            accounting,
        } => DurableSwitchoverTerminalRecord::Complete {
            state: DurableSwitchoverState::from_operation(operation),
            snapshot: snapshot.clone(),
            compensated: *compensated,
            accounting: *accounting,
        },
        DurableSwitchoverPilotTerminal::Stopped { operation, message } => {
            DurableSwitchoverTerminalRecord::Stopped {
                state: operation
                    .as_ref()
                    .map(DurableSwitchoverState::from_operation),
                message: message.clone(),
            }
        }
    };
    encode_terminal_record(&record)
}

fn terminal_failure(operation: Option<DurableOperationStatus>, message: String) -> TerminalOutcome {
    let bounded_message: String = message.chars().take(512).collect();
    let terminal = DurableSwitchoverTerminalRecord::Stopped {
        state: operation
            .as_ref()
            .map(DurableSwitchoverState::from_operation),
        message: bounded_message,
    };
    let payload = encode_terminal_record(&terminal).unwrap_or_else(|_| {
        ExactBytes::new(br#"{"status":"stopped","state":null,"message":"pilot terminal payload exceeded its bound"}"#)
    });
    TerminalOutcome::failed(payload)
}

fn encode_terminal_record(
    terminal: &DurableSwitchoverTerminalRecord,
) -> Result<ExactBytes, String> {
    let encoded = serde_json::to_vec(terminal)
        .map_err(|error| format!("serialize durable switchover terminal outcome: {error}"))?;
    if encoded.len() > PILOT_MAX_TERMINAL_BYTES as usize {
        return Err(format!(
            "durable switchover terminal outcome is {} bytes; maximum is {}",
            encoded.len(),
            PILOT_MAX_TERMINAL_BYTES
        ));
    }
    Ok(ExactBytes::new(encoded))
}

// COMPLEXITY-BOUNDARY: pilot-workflow:end
pub fn new_pilot_reference(
    set_uid: &str,
    previous_snapshot: StablePartitionSnapshotStatus,
    target_primary_id: i64,
    now: i64,
) -> Result<DurableSwitchoverPilotStatus, String> {
    if set_uid.is_empty() {
        return Err("durable switchover pilot requires the KubericSet UID".to_string());
    }
    let execution_id = ExecutionId::from_bytes(random());
    let execution_hex = encode_execution_id(execution_id);
    let operation_identity = format!("{set_uid}:durable-pilot:{execution_hex}");
    let initial_operation = start_switchover(
        &operation_identity,
        previous_snapshot,
        target_primary_id,
        now,
    )?;
    validate_pilot_admission(&initial_operation)?;
    let initial_operation_json = serde_json::to_string(&initial_operation)
        .map_err(|error| format!("serialize initial durable switchover operation: {error}"))?;

    let reference = DurableSwitchoverPilotStatus {
        version: PILOT_VERSION,
        execution_id: execution_hex,
        checkpoint_name: KubernetesCheckpointStore::object_name(execution_id),
        initial_operation_json,
    };
    execution_spec(&reference)?;
    Ok(reference)
}

pub fn execution_id(reference: &DurableSwitchoverPilotStatus) -> Result<ExecutionId, String> {
    if reference.version != PILOT_VERSION {
        return Err(format!(
            "unsupported durable switchover pilot version {}",
            reference.version
        ));
    }
    let bytes = decode_execution_id(&reference.execution_id)?;
    let execution_id = ExecutionId::from_bytes(bytes);
    let expected_name = KubernetesCheckpointStore::object_name(execution_id);
    if reference.checkpoint_name != expected_name {
        return Err(format!(
            "durable switchover checkpoint name mismatch: expected {expected_name}, found {}",
            reference.checkpoint_name
        ));
    }
    Ok(execution_id)
}

pub fn execution_spec(reference: &DurableSwitchoverPilotStatus) -> Result<ExecutionSpec, String> {
    let execution_id = execution_id(reference)?;
    let initial_operation = initial_operation(reference)?;
    validate_pilot_admission(&initial_operation)?;
    let input = DurableSwitchoverPilotInput {
        version: reference.version,
        execution_id: reference.execution_id.clone(),
        initial_operation,
    };
    let input = serde_json::to_vec(&input)
        .map_err(|error| format!("serialize durable switchover pilot input: {error}"))?;
    if input.len() > PILOT_MAX_OPERATION_BYTES {
        return Err(format!(
            "durable switchover pilot input is {} bytes; maximum is {}",
            input.len(),
            PILOT_MAX_OPERATION_BYTES
        ));
    }
    Ok(ExecutionSpec::new(
        execution_id,
        ExactBytes::new(input),
        PILOT_MAX_TERMINAL_BYTES,
    ))
}

pub fn initial_operation(
    reference: &DurableSwitchoverPilotStatus,
) -> Result<DurableOperationStatus, String> {
    serde_json::from_str(&reference.initial_operation_json)
        .map_err(|error| format!("decode initial durable switchover operation: {error}"))
}

pub fn checkpoint_limits() -> CheckpointLimits {
    CheckpointLimits::new(
        PILOT_MAX_ACTIVITY_RECORDS,
        PILOT_MAX_ENCODED_CHECKPOINT_BYTES,
    )
    .expect("durable switchover pilot limits are nonzero")
}

pub fn checkpoint_store_options(
    namespace: &str,
    set_name: &str,
    set_uid: &str,
) -> Result<KubernetesCheckpointStoreOptions, String> {
    if namespace.is_empty() || set_name.is_empty() || set_uid.is_empty() {
        return Err(
            "durable switchover checkpoint owner requires namespace, name, and UID".to_string(),
        );
    }
    let owner = KubernetesCheckpointOwner::new(
        OwnerReference {
            api_version: "kuberic.io/v1".to_string(),
            kind: "KubericSet".to_string(),
            name: set_name.to_string(),
            uid: set_uid.to_string(),
            controller: Some(false),
            block_owner_deletion: Some(false),
        },
        KubernetesCheckpointOwnerScope::Namespaced(namespace.to_string()),
    );
    Ok(KubernetesCheckpointStoreOptions::default().with_owner(owner))
}

pub fn validate_pilot_admission(operation: &DurableOperationStatus) -> Result<(), String> {
    validate_pilot_operation(operation)?;
    validate_variant_bounds(operation)?;
    let previous_snapshot = operation
        .previous_snapshot
        .as_ref()
        .ok_or_else(|| "durable switchover pilot has no previous snapshot".to_string())?;
    let success_steps = projected_success_steps(previous_snapshot.members.len());
    let rollback_steps = projected_rollback_steps(previous_snapshot.members.len());
    let projected_steps = success_steps.len().max(rollback_steps.len());
    if projected_steps > PILOT_MAX_ACTIVITY_RECORDS {
        return Err(format!(
            "durable switchover requires {projected_steps} projected activities; maximum is {PILOT_MAX_ACTIVITY_RECORDS}"
        ));
    }
    let projected_transitions = (success_steps.len() + projected_success_pure_transitions())
        .max(rollback_steps.len() + projected_rollback_pure_transitions());
    if projected_transitions > PILOT_MAX_TRANSITION_FUEL {
        return Err(format!(
            "durable switchover requires {projected_transitions} projected transitions; maximum is {PILOT_MAX_TRANSITION_FUEL}"
        ));
    }
    let projected_bytes = maximum_projected_checkpoint_bytes()?;
    if projected_bytes > PILOT_MAX_ENCODED_CHECKPOINT_BYTES {
        return Err(format!(
            "durable switchover projected checkpoint is {projected_bytes} bytes; maximum is {PILOT_MAX_ENCODED_CHECKPOINT_BYTES}"
        ));
    }
    Ok(())
}

pub fn validate_pilot_operation(operation: &DurableOperationStatus) -> Result<(), String> {
    validate_switchover_operation(operation)?;
    let previous_snapshot = operation
        .previous_snapshot
        .as_ref()
        .ok_or_else(|| "durable switchover pilot has no previous snapshot".to_string())?;
    if previous_snapshot.members.len() > PILOT_MAX_REPLICAS {
        return Err(format!(
            "durable switchover pilot supports at most {PILOT_MAX_REPLICAS} replicas; found {}",
            previous_snapshot.members.len()
        ));
    }
    let operation_bytes = serde_json::to_vec(operation)
        .map_err(|error| format!("serialize durable switchover operation: {error}"))?
        .len();
    if operation_bytes > PILOT_MAX_OPERATION_BYTES {
        return Err(format!(
            "durable switchover operation is {operation_bytes} bytes; maximum is {PILOT_MAX_OPERATION_BYTES}"
        ));
    }
    Ok(())
}

fn maximum_projected_checkpoint_bytes() -> Result<usize, String> {
    static PROJECTED_BYTES: OnceLock<Result<usize, String>> = OnceLock::new();
    PROJECTED_BYTES
        .get_or_init(|| {
            maximum_active_checkpoint()?
                .encoded_len()
                .map_err(|error| format!("measure maximum pilot checkpoint: {error}"))
        })
        .clone()
}

fn projected_success_steps(member_count: usize) -> Vec<&'static str> {
    let mut steps = vec![
        "revoke.effect",
        "capture-lsn",
        "target-catch-up",
        "demote.effect",
        "promote.effect",
    ];
    steps.extend(std::iter::repeat_n(
        "epoch.effect",
        member_count.saturating_sub(2),
    ));

    steps.extend([
        "catch-up-config.effect",
        "quorum.effect",
        "current-config.effect",
        "target-label.effect",
        "old-label.effect",
        "final-attestation",
    ]);
    let redelivery_headroom = external_effect_count(&steps);
    steps.extend(std::iter::repeat_n(
        "proven-no-admission-redelivery",
        redelivery_headroom,
    ));
    steps
}

fn projected_passive_observation_count(steps: &[&str]) -> usize {
    steps
        .iter()
        .filter(|step| !step.ends_with(".effect") && **step != "proven-no-admission-redelivery")
        .count()
}

fn projected_activity_accounting(steps: &[&str]) -> PilotActivityAccounting {
    PilotActivityAccounting::new(
        u64::try_from(external_effect_count(steps))
            .expect("projected external-effect count fits u64"),
        u64::try_from(projected_passive_observation_count(steps))
            .expect("projected passive-observation count fits u64"),
    )
}

fn is_full_pilot_start(initial: &DurableOperationStatus) -> bool {
    initial.phase == DurableOperationPhase::Revoke
        && initial.pending_action.is_none()
        && initial.frozen_lsn.is_none()
        && initial.next_secondary_index == 0
        && initial.previous_snapshot.members.len() == PILOT_MAX_REPLICAS
}

fn validate_terminal_activity_accounting(
    initial: &DurableOperationStatus,
    terminal: &DurableOperationStatus,
    compensated: bool,
    accounting: PilotActivityAccounting,
) -> Result<(), String> {
    if !is_full_pilot_start(initial) {
        return Ok(());
    }
    validate_terminal_accounting_shape(
        Some(terminal.phase),
        compensated,
        accounting,
        accounting
            .external_effect_count
            .checked_add(accounting.passive_observation_count)
            .ok_or_else(|| "pilot terminal activity accounting overflowed".to_string())?,
    )
}

fn validate_terminal_accounting_shape(
    terminal_phase: Option<DurableOperationPhase>,
    compensated: bool,
    accounting: PilotActivityAccounting,
    completed_activity_count: u64,
) -> Result<(), String> {
    if !accounting.matches_completed_activity_count(completed_activity_count) {
        return Err(format!(
            "pilot terminal activity accounting {}/{} does not total the authoritative \
             completed activity count {completed_activity_count}",
            accounting.external_effect_count, accounting.passive_observation_count,
        ));
    }

    if !compensated {
        let expected = projected_activity_accounting(&projected_success_steps(PILOT_MAX_REPLICAS));
        if terminal_phase.is_some_and(|phase| phase != DurableOperationPhase::Completed)
            || accounting != expected
        {
            return Err(format!(
                "successful full pilot requires authoritative activity accounting {}/{}, got {}/{}",
                expected.external_effect_count,
                expected.passive_observation_count,
                accounting.external_effect_count,
                accounting.passive_observation_count,
            ));
        }
        return Ok(());
    }

    let rollback_steps = projected_rollback_steps(PILOT_MAX_REPLICAS);
    let rollback = projected_activity_accounting(&rollback_steps);
    let maximum_external_effects = rollback
        .external_effect_count
        .checked_mul(2)
        .ok_or_else(|| "pilot rollback activity accounting overflowed".to_string())?;
    let maximum_completed_activities =
        u64::try_from(rollback_steps.len()).expect("projected rollback activity count fits u64");
    let maximum_non_redelivery_activities = maximum_completed_activities
        .checked_sub(rollback.external_effect_count)
        .expect("rollback projection includes its redelivery headroom");
    let minimum_external_for_total = completed_activity_count
        .saturating_sub(maximum_non_redelivery_activities)
        .checked_mul(2)
        .ok_or_else(|| "pilot rollback redelivery accounting overflowed".to_string())?;
    if terminal_phase.is_some_and(|phase| phase != DurableOperationPhase::Failed)
        || accounting.external_effect_count == 0
        || accounting.external_effect_count > maximum_external_effects
        || accounting.passive_observation_count == 0
        || accounting.passive_observation_count > maximum_non_redelivery_activities
        || accounting.external_effect_count < minimum_external_for_total
        || completed_activity_count > maximum_completed_activities
    {
        return Err(format!(
            "compensated pilot activity accounting {}/{} is outside the authoritative rollback \
             projection (external 1..={maximum_external_effects}, passive <= \
             {maximum_non_redelivery_activities}, total <= {maximum_completed_activities}, \
             every redelivery requires its preceding external attempt)",
            accounting.external_effect_count, accounting.passive_observation_count,
        ));
    }
    Ok(())
}

#[cfg(test)]
fn projected_label_effect_count(steps: &[&str]) -> usize {
    steps
        .iter()
        .filter(|step| step.contains("label.effect"))
        .count()
}

fn validate_variant_bounds(operation: &DurableOperationStatus) -> Result<(), String> {
    let previous = operation
        .previous_snapshot
        .cloned()
        .ok_or_else(|| "durable switchover has no previous snapshot".to_string())?;
    let mut stopped = operation.clone();
    stopped.phase = crate::crd::DurableOperationPhase::Poisoned;
    stopped.pending_action = None;
    let state = DurableSwitchoverState::from_operation(operation);
    let variants = [
        DurableSwitchoverStepResult::Advance {
            operation: state.clone(),
        },
        DurableSwitchoverStepResult::ProvenNoAdmission {
            operation: state.clone(),
            action_id: operation
                .pending_action
                .as_ref()
                .map(|pending| pending.action_id.clone())
                .unwrap_or_else(|| format!("{}:maximum-action", operation.operation_id)),
            redelivery: 1,
        },
        DurableSwitchoverStepResult::Complete {
            operation: state.clone(),
            snapshot: operation.target_snapshot.clone(),
            compensated: false,
            accounting: Some(PilotActivityAccounting::new(u64::MAX, u64::MAX)),
        },
        DurableSwitchoverStepResult::Complete {
            operation: state,
            snapshot: previous.clone(),
            compensated: true,
            accounting: Some(PilotActivityAccounting::new(u64::MAX, u64::MAX)),
        },
        DurableSwitchoverStepResult::Stopped {
            operation: DurableSwitchoverState::from_operation(&stopped),
            message: "x".repeat(512),
        },
    ];
    for variant in &variants {
        encode_step_result(variant)?;
    }
    for terminal in [
        DurableSwitchoverTerminalRecord::Complete {
            state: DurableSwitchoverState::from_operation(operation),
            snapshot: operation.target_snapshot.clone(),
            compensated: false,
            accounting: PilotActivityAccounting::new(u64::MAX, u64::MAX),
        },
        DurableSwitchoverTerminalRecord::Complete {
            state: DurableSwitchoverState::from_operation(operation),
            snapshot: previous,
            compensated: true,
            accounting: PilotActivityAccounting::new(u64::MAX, u64::MAX),
        },
        DurableSwitchoverTerminalRecord::Stopped {
            state: Some(DurableSwitchoverState::from_operation(&stopped)),
            message: "x".repeat(512),
        },
    ] {
        encode_terminal_record(&terminal)?;
    }
    Ok(())
}

fn projected_rollback_steps(member_count: usize) -> Vec<&'static str> {
    let mut steps = vec![
        "revoke.effect",
        "capture-lsn",
        "target-catch-up",
        "demote.effect",
        "promote.effect",
        "rollback-promote.effect",
    ];
    steps.extend(std::iter::repeat_n(
        "rollback-epoch.effect",
        member_count.saturating_sub(1),
    ));
    steps.extend([
        "rollback-catch-up.effect",
        "rollback-current.effect",
        "rollback-old-label.effect",
        "rollback-target-label.effect",
        "rollback-final-attestation",
    ]);
    let redelivery_headroom = external_effect_count(&steps);
    steps.extend(std::iter::repeat_n(
        "proven-no-admission-redelivery",
        redelivery_headroom,
    ));
    steps
}

fn external_effect_count(steps: &[&str]) -> usize {
    steps
        .iter()
        .filter(|step| step.ends_with(".effect"))
        .count()
}

const fn projected_success_pure_transitions() -> usize {
    10
}

const fn projected_rollback_pure_transitions() -> usize {
    11
}

pub fn maximum_active_checkpoint() -> Result<CheckpointEnvelope, String> {
    let payload = maximum_active_payload(PILOT_MAX_ENCODED_CHECKPOINT_BYTES)?;
    CheckpointEnvelope::encode_with_limits(&payload, checkpoint_limits())
        .map_err(|error| format!("project maximum pilot checkpoint: {error}"))
}

fn maximum_active_payload(
    admitted_max_encoded_checkpoint_bytes: usize,
) -> Result<CheckpointPayload, String> {
    let execution_id = ExecutionId::from_bytes([u8::MAX; 16]);
    let execution = ExecutionSpec::new(
        execution_id,
        ExactBytes::new(vec![u8::MAX; PILOT_MAX_OPERATION_BYTES]),
        PILOT_MAX_TERMINAL_BYTES,
    );
    let contract = ExecutionContract::new(
        execution,
        u64::try_from(admitted_max_encoded_checkpoint_bytes)
            .map_err(|_| "pilot checkpoint limit does not fit u64".to_string())?,
    );
    let name = ActivityName::new(PILOT_ACTIVITY_NAME, PILOT_ACTIVITY_VERSION)
        .map_err(|error| format!("construct pilot activity name: {error}"))?;
    let activities = (0..PILOT_MAX_ACTIVITY_RECORDS)
        .map(|sequence| {
            let spec = ActivitySpec::new(
                name.clone(),
                ExactBytes::new(vec![u8::MAX; PILOT_MAX_ACTIVITY_INPUT_BYTES]),
                u64::try_from(PILOT_MAX_ACTIVITY_RESULT_BYTES)
                    .expect("pilot result bound fits u64"),
            );
            ActivityRecord::completed(
                ActivitySequence::new(
                    u64::try_from(sequence).expect("pilot activity count fits u64"),
                ),
                spec,
                ExactBytes::new(vec![u8::MAX; PILOT_MAX_ACTIVITY_RESULT_BYTES]),
            )
        })
        .collect();
    Ok(CheckpointPayload::active(contract, activities))
}

#[cfg(test)]
fn maximum_prepared_activity_input() -> Result<ExactBytes, String> {
    let maximum_fence = "x".repeat(512);
    let input = DurableSwitchoverActivityInput {
        version: PILOT_VERSION,
        state: DurableSwitchoverState {
            phase: DurableOperationPhase::Poisoned,
            frozen_lsn: Some(i64::MAX),
            next_secondary_index: u32::MAX,
            phase_deadline_unix_seconds: i64::MAX,
            pending_action: None,
            last_error: Some(maximum_fence.clone()),
        },
        kind: PilotActivityKind::PreparedReplica {
            command: ReplicaEffectCommand {
                action_id: maximum_fence.clone(),
                action_signature: maximum_fence.clone(),
                target_id: i64::MAX,
                target_instance_id: maximum_fence.clone(),
                expected_epoch: crate::crd::EpochStatus {
                    data_loss_number: i64::MAX,
                    configuration_number: i64::MAX,
                },
                desired_postcondition: crate::crd::DurablePostconditionStatus {
                    kind: crate::crd::DurablePostconditionKind::CurrentConfiguration,
                    role: Some(crate::crd::StableReplicaRoleStatus::Primary),
                },
                expected_agent_generation: maximum_fence.clone(),
                expected_control_version: u64::MAX,
                observed_runtime_epoch: crate::crd::EpochStatus {
                    data_loss_number: i64::MAX,
                    configuration_number: i64::MAX,
                },
                action_payload: "x".repeat(4_096),
            },
        },
    };
    let encoded = encode_activity_input::<DurableSwitchoverActivity>(&input)
        .map_err(|error| format!("encode maximum prepared pilot activity: {error}"))?;
    if encoded.as_slice().len() > PILOT_MAX_ACTIVITY_INPUT_BYTES {
        return Err(format!(
            "maximum prepared pilot activity is {} bytes; maximum is {}",
            encoded.as_slice().len(),
            PILOT_MAX_ACTIVITY_INPUT_BYTES
        ));
    }
    Ok(encoded)
}

fn encode_execution_id(execution_id: ExecutionId) -> String {
    let mut encoded = String::with_capacity(32);
    for byte in execution_id.as_bytes() {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn decode_execution_id(value: &str) -> Result<[u8; 16], String> {
    if value.len() != 32 {
        return Err(format!(
            "durable switchover execution ID must contain 32 lowercase hexadecimal characters; found {}",
            value.len()
        ));
    }
    let mut bytes = [0_u8; 16];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = decode_hex(pair[0])?;
        let low = decode_hex(pair[1])?;
        bytes[index] = (high << 4) | low;
    }
    Ok(bytes)
}

fn decode_hex(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err("durable switchover execution ID must be lowercase hexadecimal".to_string()),
    }
}

// COMPLEXITY-BOUNDARY: pilot-module:end
#[cfg(test)]
mod durable_switchover_pilot_tests {
    use super::*;
    use crate::crd::{EpochStatus, StableReplicaRoleStatus, StableReplicaSnapshotStatus};
    use crate::durable::{Decision, decide};
    use kuberic_durable_execution::ActivityObservation;
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn snapshot(member_count: usize) -> StablePartitionSnapshotStatus {
        StablePartitionSnapshotStatus {
            epoch: EpochStatus {
                data_loss_number: 4,
                configuration_number: 8,
            },
            primary_id: 1,
            members: (1..=member_count)
                .map(|id| StableReplicaSnapshotStatus {
                    id: i64::try_from(id).unwrap(),
                    instance_id: format!("pod-{id}-uid"),
                    role: if id == 1 {
                        StableReplicaRoleStatus::Primary
                    } else {
                        StableReplicaRoleStatus::ActiveSecondary
                    },
                    election_metadata: None,
                })
                .collect(),
            write_quorum: u32::try_from(member_count / 2 + 1).unwrap(),
        }
    }

    fn compact(operation: &DurableOperationStatus) -> DurableSwitchoverState {
        DurableSwitchoverState::from_operation(operation)
    }

    #[test]
    fn references_are_stable_but_repeated_requests_are_distinct() {
        let first = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let second = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        assert_ne!(first.execution_id, second.execution_id);
        assert_ne!(
            initial_operation(&first).unwrap().operation_id,
            initial_operation(&second).unwrap().operation_id
        );
        let first_action =
            match decide(&initial_operation(&first).unwrap(), &BTreeMap::new(), 100).unwrap() {
                Decision::Persist(operation) => operation.pending_action.unwrap().action_id,
                other => panic!("expected initial pending action, found {other:?}"),
            };
        let second_action =
            match decide(&initial_operation(&second).unwrap(), &BTreeMap::new(), 100).unwrap() {
                Decision::Persist(operation) => operation.pending_action.unwrap().action_id,
                other => panic!("expected initial pending action, found {other:?}"),
            };
        assert_ne!(first_action, second_action);
        assert_eq!(
            execution_spec(&first).unwrap(),
            execution_spec(&first).unwrap()
        );
        assert_eq!(
            first.checkpoint_name,
            KubernetesCheckpointStore::object_name(execution_id(&first).unwrap())
        );
    }

    #[test]
    fn owner_is_same_namespace_non_controlling_and_non_blocking() {
        let options = checkpoint_store_options("tenant-a", "database", "set-uid").unwrap();
        let owner = options.owner().unwrap();
        assert_eq!(
            owner.scope(),
            &KubernetesCheckpointOwnerScope::Namespaced("tenant-a".to_string())
        );
        assert_eq!(owner.reference().api_version, "kuberic.io/v1");
        assert_eq!(owner.reference().kind, "KubericSet");
        assert_eq!(owner.reference().name, "database");
        assert_eq!(owner.reference().uid, "set-uid");
        assert_eq!(owner.reference().controller, Some(false));
        assert_eq!(owner.reference().block_owner_deletion, Some(false));
    }

    #[test]
    fn admission_rejects_more_than_three_members() {
        let reference = new_pilot_reference("set-uid", snapshot(4), 2, 100).unwrap_err();
        assert!(reference.contains("at most 3 replicas"), "{reference}");
    }

    #[test]
    fn maximum_projected_history_fits_both_budgets() {
        let prepared_input = maximum_prepared_activity_input().unwrap();
        assert!(
            prepared_input.as_slice().len() > PILOT_MAX_ACTIVITY_INPUT_BYTES / 2,
            "prepared fixture must exercise large commands and fences"
        );
        assert!(matches!(
            decode_pilot_activity_input(&prepared_input).unwrap().kind,
            PilotActivityKind::PreparedReplica { .. }
        ));
        let checkpoint = maximum_active_checkpoint().unwrap();
        let encoded = checkpoint.encoded_len().unwrap();
        assert!(encoded <= PILOT_MAX_ENCODED_CHECKPOINT_BYTES);
        assert!(
            encoded + "checkpoint.json".len()
                <= usize::try_from(kuberic_durable_execution::DEFAULT_CONFIG_MAP_DATA_BUDGET_BYTES)
                    .unwrap()
        );

        let payload = maximum_active_payload(encoded).unwrap();
        let exact = CheckpointLimits::new(PILOT_MAX_ACTIVITY_RECORDS, encoded).unwrap();
        assert!(CheckpointEnvelope::encode_with_limits(&payload, exact).is_ok());
        let one_byte_short =
            CheckpointLimits::new(PILOT_MAX_ACTIVITY_RECORDS, encoded - 1).unwrap();
        assert!(CheckpointEnvelope::encode_with_limits(&payload, one_byte_short).is_err());
    }

    #[test]
    fn malformed_or_mismatched_execution_identity_is_rejected() {
        let mut reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        reference.execution_id = "ABC".to_string();
        assert!(execution_id(&reference).is_err());

        let mut reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        reference.checkpoint_name.push_str("-other");
        assert!(execution_id(&reference).is_err());
    }

    #[test]
    fn wrapped_workflow_input_is_bounded_before_checkpoint_creation() {
        let mut reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let mut operation = initial_operation(&reference).unwrap();
        let mut found_wrapped_overflow = false;
        for length in (1..=PILOT_MAX_OPERATION_BYTES).rev() {
            operation.last_error = Some("x".repeat(length));
            let operation_json = serde_json::to_string(&operation).unwrap();
            if operation_json.len() <= PILOT_MAX_OPERATION_BYTES {
                reference.initial_operation_json = operation_json;
                let error = execution_spec(&reference).unwrap_err();
                assert!(error.contains("pilot input is"), "{error}");
                found_wrapped_overflow = true;
                break;
            }
        }
        assert!(
            found_wrapped_overflow,
            "test must construct an operation that fits while its workflow wrapper does not"
        );
    }

    #[test]
    fn missing_previous_snapshot_is_rejected_without_panic() {
        let mut reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_str(&reference.initial_operation_json).unwrap();
        value.as_object_mut().unwrap().remove("previousSnapshot");
        reference.initial_operation_json = serde_json::to_string(&value).unwrap();
        let error = execution_spec(&reference).unwrap_err();
        assert!(error.contains("previous stable snapshot"), "{error}");

        let mut operation =
            initial_operation(&new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap())
                .unwrap();
        operation.previous_snapshot = Default::default();
        assert!(validate_pilot_admission(&operation).is_err());
    }

    #[tokio::test]
    async fn runtime_reuses_one_host_per_process_execution() {
        let runtime = DurableSwitchoverPilotRuntime::in_memory(InMemoryCheckpointStore::new());
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let first = runtime
            .host("tenant-a", "database", "set-uid", &reference)
            .await
            .unwrap();
        let second = runtime
            .host("tenant-a", "database", "set-uid", &reference)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        runtime
            .forget("tenant-a", "database", "set-uid", &reference.execution_id)
            .await;
        let replacement = runtime
            .host("tenant-a", "database", "set-uid", &reference)
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &replacement));
    }

    #[tokio::test]
    async fn host_cache_identity_includes_owner_coordinates() {
        let runtime = DurableSwitchoverPilotRuntime::in_memory(InMemoryCheckpointStore::new());
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let first = runtime
            .host("tenant-a", "database", "set-uid", &reference)
            .await
            .unwrap();
        let other_owner = runtime
            .host("tenant-b", "database", "other-uid", &reference)
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &other_owner));
    }

    struct PollCountingPilotWorkflow {
        polls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Workflow for PollCountingPilotWorkflow {
        async fn run(
            &self,
            context: &mut WorkflowContext<'_>,
            input: ExactBytes,
        ) -> TerminalOutcome {
            self.polls.fetch_add(1, Ordering::SeqCst);
            DurableSwitchoverWorkflow.run(context, input).await
        }
    }

    async fn expose_next<W: Workflow>(
        host: &mut PilotHost,
        workflow: &W,
        execution: &ExecutionSpec,
    ) -> kuberic_durable_execution::DispatchPermit {
        assert!(matches!(
            {
                let outcome = host.turn(workflow, execution.clone()).await;
                host.store().correlate_host_outcome(&outcome);
                outcome
            },
            kuberic_durable_execution::HostOutcome::ScheduleAccepted { .. }
        ));
        let outcome = host.turn(workflow, execution.clone()).await;
        host.store().correlate_host_outcome(&outcome);
        match outcome {
            kuberic_durable_execution::HostOutcome::DispatchPermitted { permit, .. } => permit,
            other => panic!("expected dispatch permit, found {other:?}"),
        }
    }

    #[tokio::test]
    async fn workflow_replays_steps_and_reloads_terminal_without_polling() {
        let mut reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let mut initial = initial_operation(&reference).unwrap();
        initial.phase = crate::crd::DurableOperationPhase::LabelOldSecondary;
        reference.initial_operation_json = serde_json::to_string(&initial).unwrap();
        let execution = execution_spec(&reference).unwrap();
        let store = InMemoryCheckpointStore::new();
        let measured = MeasuredPilotCheckpointStore::new(
            execution.execution_id(),
            PilotCheckpointStore::InMemory(store),
        );
        let mut host = DurableHost::new(
            measured,
            HostEpoch::from_bytes([7; 16]),
            checkpoint_limits(),
        );
        let polls = Arc::new(AtomicUsize::new(0));
        let workflow = PollCountingPilotWorkflow {
            polls: polls.clone(),
        };

        let first_permit = expose_next(&mut host, &workflow, &execution).await;
        let mut advanced = initial.clone();
        advanced.phase = crate::crd::DurableOperationPhase::Finalize;
        let outcome = host
            .observe(
                &execution,
                ActivityObservation::new(
                    first_permit.activity().clone(),
                    encode_step_result(&DurableSwitchoverStepResult::Advance {
                        operation: compact(&advanced),
                    })
                    .unwrap(),
                ),
            )
            .await;
        host.store().correlate_host_outcome(&outcome);
        assert!(matches!(
            outcome,
            kuberic_durable_execution::HostOutcome::ObservationAccepted { .. }
        ));

        let second_permit = expose_next(&mut host, &workflow, &execution).await;
        advanced.phase = crate::crd::DurableOperationPhase::Completed;
        let terminal_snapshot = advanced.target_snapshot.clone();
        let observed = host
            .observe(
                &execution,
                ActivityObservation::new(
                    second_permit.activity().clone(),
                    encode_step_result(&DurableSwitchoverStepResult::Complete {
                        operation: compact(&advanced),
                        snapshot: terminal_snapshot.clone(),
                        compensated: false,
                        accounting: Some(PilotActivityAccounting::new(1, 1)),
                    })
                    .unwrap(),
                ),
            )
            .await;
        host.store().correlate_host_outcome(&observed);
        assert!(matches!(
            observed,
            kuberic_durable_execution::HostOutcome::ObservationAccepted { .. }
        ));

        let completed = host.turn(&workflow, execution.clone()).await;
        host.store().correlate_host_outcome(&completed);
        let kuberic_durable_execution::HostOutcome::WorkflowCompleted { outcome, .. } = completed
        else {
            panic!("expected workflow completion, found {completed:?}");
        };
        assert_eq!(
            decode_terminal(&outcome, &initial).unwrap(),
            DurableSwitchoverPilotTerminal::Complete {
                operation: advanced,
                snapshot: terminal_snapshot,
                compensated: false,
                accounting: PilotActivityAccounting::new(1, 1),
            }
        );
        let polls_before_terminal_reload = polls.load(Ordering::SeqCst);
        assert!(matches!(
            host.turn(&workflow, execution).await,
            kuberic_durable_execution::HostOutcome::WorkflowCompleted { .. }
        ));
        assert_eq!(
            polls.load(Ordering::SeqCst),
            polls_before_terminal_reload,
            "terminal reload must not poll workflow code"
        );
        let measurements = host.store().measurements();
        assert_eq!(measurements.accepted_writes, 7);
        assert!(measurements.latest_authoritative_checkpoint_bytes.is_some());
        assert!(
            host.store()
                .collector()
                .events()
                .iter()
                .all(|event| event.boundary.is_some())
        );
    }

    #[tokio::test]
    async fn exposed_activity_is_quarantined_after_host_restart() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let execution = execution_spec(&reference).unwrap();
        let backend = InMemoryCheckpointStore::new();
        let mut first_host = DurableHost::new(
            MeasuredPilotCheckpointStore::new(
                execution.execution_id(),
                PilotCheckpointStore::InMemory(backend.clone()),
            ),
            HostEpoch::from_bytes([3; 16]),
            checkpoint_limits(),
        );
        let workflow = DurableSwitchoverWorkflow;
        let permit = expose_next(&mut first_host, &workflow, &execution).await;

        let mut restarted_host = DurableHost::new(
            MeasuredPilotCheckpointStore::new(
                execution.execution_id(),
                PilotCheckpointStore::InMemory(backend),
            ),
            HostEpoch::from_bytes([4; 16]),
            checkpoint_limits(),
        );
        match restarted_host.turn(&workflow, execution).await {
            kuberic_durable_execution::HostOutcome::Quarantined { activity, .. } => {
                assert_eq!(&activity, permit.activity());
            }
            other => panic!("expected quarantine after restart, found {other:?}"),
        }
    }

    #[tokio::test]
    async fn stopped_step_becomes_failed_terminal() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let execution = execution_spec(&reference).unwrap();
        let operation = initial_operation(&reference).unwrap();
        let store = InMemoryCheckpointStore::new();
        let mut host = DurableHost::new(
            MeasuredPilotCheckpointStore::new(
                execution.execution_id(),
                PilotCheckpointStore::InMemory(store),
            ),
            HostEpoch::from_bytes([8; 16]),
            checkpoint_limits(),
        );
        let workflow = DurableSwitchoverWorkflow;
        let permit = expose_next(&mut host, &workflow, &execution).await;
        let mut stopped = operation.clone();
        stopped.phase = crate::crd::DurableOperationPhase::Poisoned;
        stopped.pending_action = None;
        assert!(matches!(
            host.observe(
                &execution,
                ActivityObservation::new(
                    permit.activity().clone(),
                    encode_step_result(&DurableSwitchoverStepResult::Stopped {
                        operation: compact(&stopped),
                        message: "ambiguous effect remains quarantined".to_string(),
                    })
                    .unwrap(),
                ),
            )
            .await,
            kuberic_durable_execution::HostOutcome::ObservationAccepted { .. }
        ));
        let completed = host.turn(&workflow, execution).await;
        let kuberic_durable_execution::HostOutcome::WorkflowCompleted { outcome, .. } = completed
        else {
            panic!("expected stopped terminal, found {completed:?}");
        };
        assert!(matches!(outcome, TerminalOutcome::Failed(_)));
        assert_eq!(
            decode_terminal(&outcome, &operation).unwrap(),
            DurableSwitchoverPilotTerminal::Stopped {
                operation: Some(stopped),
                message: "ambiguous effect remains quarantined".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn permit_guard_consumes_matching_permit_once() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let execution = execution_spec(&reference).unwrap();
        let operation = initial_operation(&reference).unwrap();
        let mut host = DurableHost::new(
            MeasuredPilotCheckpointStore::new(
                execution.execution_id(),
                PilotCheckpointStore::InMemory(InMemoryCheckpointStore::new()),
            ),
            HostEpoch::from_bytes([12; 16]),
            checkpoint_limits(),
        );
        let permit = expose_next(&mut host, &DurableSwitchoverWorkflow, &execution).await;
        let prepared = decode_pilot_activity_input(permit.activity().input()).unwrap();
        let current = prepared.state.apply_to(&operation).unwrap();
        let activity = permit.activity().clone();
        let attempt_id = permit.attempt_id();
        let mut guard = PilotPermitGuard::new(permit);
        assert!(guard.activity().is_some());
        let wrong_attempt =
            kuberic_durable_execution::AttemptId::new(HostEpoch::from_bytes([99; 16]), 1).unwrap();
        assert!(
            guard
                .consume_for(&current, &prepared.kind, &activity, wrong_attempt)
                .is_err()
        );
        let wrong_kind = PilotActivityKind::PreparedLabel {
            command: LabelEffectCommand::new(
                1,
                "database-0".to_string(),
                "pod-1-uid".to_string(),
                "primary".to_string(),
            ),
        };
        assert!(
            guard
                .consume_for(&current, &wrong_kind, &activity, attempt_id)
                .is_err()
        );
        let consumed = guard
            .consume_for(&current, &prepared.kind, &activity, attempt_id)
            .unwrap();
        assert_eq!(
            consumed.activity().input().as_slice(),
            encode_activity_input::<DurableSwitchoverActivity>(&DurableSwitchoverActivityInput {
                version: PILOT_VERSION,
                state: compact(&current),
                kind: prepared.kind.clone(),
            })
            .unwrap()
            .as_slice()
        );
        assert!(
            guard
                .consume_for(&current, &prepared.kind, &activity, attempt_id)
                .is_err()
        );
    }

    #[test]
    fn prepared_replica_spec_binds_every_dispatch_fence() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let operation = initial_operation(&reference).unwrap();
        let command = ReplicaEffectCommand {
            action_id: "execution:1:RevokeWrite".to_string(),
            action_signature: kuberic_core::types::DurableReplicaAction::RevokeWriteStatus
                .signature(),
            target_id: 1,
            target_instance_id: "pod-1-uid".to_string(),
            expected_epoch: EpochStatus {
                data_loss_number: 4,
                configuration_number: 8,
            },
            desired_postcondition: crate::crd::DurablePostconditionStatus {
                kind: crate::crd::DurablePostconditionKind::WriteRevoked,
                role: None,
            },
            expected_agent_generation: "0123456789abcdef0123456789abcdef".to_string(),
            expected_control_version: 7,
            observed_runtime_epoch: EpochStatus {
                data_loss_number: 4,
                configuration_number: 8,
            },
            action_payload: kuberic_core::grpc::convert::encode_direct_correlated_action_payload(
                &kuberic_core::types::DurableReplicaAction::RevokeWriteStatus,
            )
            .unwrap(),
        };
        let input = |command| DurableSwitchoverActivityInput {
            version: PILOT_VERSION,
            state: compact(&operation),
            kind: PilotActivityKind::PreparedReplica { command },
        };
        let baseline = activity_spec(&input(command.clone())).unwrap();
        let mut variants = Vec::new();
        let mut changed = command.clone();
        changed.action_id.push_str("-other");
        variants.push(changed);
        let mut changed = command.clone();
        changed.action_signature.push_str("-other");
        variants.push(changed);
        let mut changed = command.clone();
        changed.target_id = 2;
        variants.push(changed);
        let mut changed = command.clone();
        changed.target_instance_id = "replacement-uid".to_string();
        variants.push(changed);
        let mut changed = command.clone();
        changed.expected_epoch.configuration_number += 1;
        variants.push(changed);
        let mut changed = command.clone();
        changed.desired_postcondition.kind = crate::crd::DurablePostconditionKind::Role;
        variants.push(changed);
        let mut changed = command.clone();
        changed.expected_agent_generation = "fedcba9876543210fedcba9876543210".to_string();
        variants.push(changed);
        let mut changed = command.clone();
        changed.expected_control_version += 1;
        variants.push(changed);
        let mut changed = command.clone();
        changed.observed_runtime_epoch.configuration_number += 1;
        variants.push(changed);
        let mut changed = command;
        changed.action_payload =
            kuberic_core::grpc::convert::encode_direct_correlated_action_payload(
                &kuberic_core::types::DurableReplicaAction::Close,
            )
            .unwrap();
        variants.push(changed);

        for changed in variants {
            assert_ne!(activity_spec(&input(changed)).unwrap(), baseline);
        }
    }

    #[test]
    fn restart_validation_rejects_consistently_rewritten_role_and_epoch_payloads() {
        use kuberic_core::types::{DurableReplicaAction, Epoch, Role};

        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let mut operation = initial_operation(&reference).unwrap();
        operation.phase = DurableOperationPhase::PromoteTarget;
        let target = operation
            .target_snapshot
            .members
            .iter()
            .find(|member| member.id == operation.target_primary_id)
            .unwrap();
        let target_epoch = Epoch {
            data_loss_number: operation.target_snapshot.epoch.data_loss_number,
            configuration_number: operation.target_snapshot.epoch.configuration_number,
        };
        let mut pending = PendingActionStatus {
            action_id: "execution:3:PromoteTarget".to_string(),
            sequence: 3,
            kind: crate::crd::DurableActionKind::PromoteTarget,
            target_id: target.id,
            target_instance_id: target.instance_id.clone(),
            expected_epoch: operation.previous_snapshot.epoch.clone(),
            desired_postcondition: crate::crd::DurablePostconditionStatus {
                kind: crate::crd::DurablePostconditionKind::Role,
                role: Some(crate::crd::StableReplicaRoleStatus::Primary),
            },
            attempts: 0,
            deadline_unix_seconds: 100,
            last_error: None,
            dispatch_authorized: true,
            dispatch_agent_generation: Some("0123456789abcdef0123456789abcdef".to_string()),
            dispatch_agent_control_version: Some(7),
            dispatch_observed_runtime_epoch: Some(operation.previous_snapshot.epoch.clone()),
            dispatch_action_payload:
                kuberic_core::grpc::convert::encode_direct_correlated_action_payload(
                    &DurableReplicaAction::ChangeRole {
                        epoch: target_epoch,
                        role: Role::Primary,
                    },
                )
                .unwrap(),
        };
        operation.pending_action = Some(pending.clone());
        validate_prepared_activity(
            &operation,
            &PilotActivityKind::PreparedReplica {
                command: ReplicaEffectCommand::from_pending(&pending).unwrap(),
            },
        )
        .unwrap();

        for rewritten in [
            DurableReplicaAction::ChangeRole {
                epoch: Epoch {
                    configuration_number: target_epoch.configuration_number + 1,
                    ..target_epoch
                },
                role: Role::Primary,
            },
            DurableReplicaAction::ChangeRole {
                epoch: target_epoch,
                role: Role::ActiveSecondary,
            },
        ] {
            pending.dispatch_action_payload =
                kuberic_core::grpc::convert::encode_direct_correlated_action_payload(&rewritten)
                    .unwrap();
            operation.pending_action = Some(pending.clone());
            assert!(
                validate_prepared_activity(
                    &operation,
                    &PilotActivityKind::PreparedReplica {
                        command: ReplicaEffectCommand::from_pending(&pending).unwrap(),
                    },
                )
                .is_err(),
                "restart validation accepted rewritten fixed role/epoch semantics"
            );
        }
    }

    #[test]
    fn replay_rejects_recorded_command_semantic_drift_as_activity_mismatch() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let initial = initial_operation(&reference).unwrap();
        let PilotAdapterDecision::Observe(result) =
            evaluate_adapter_step(&initial, &OperationObservations::new(), 100).unwrap()
        else {
            panic!("expected revoke intent");
        };
        let DurableSwitchoverStepResult::Advance { operation: state } = *result else {
            panic!("expected pending revoke state");
        };
        let logical = activity_spec(&DurableSwitchoverActivityInput {
            version: PILOT_VERSION,
            state: state.clone(),
            kind: PilotActivityKind::PassiveObservation,
        })
        .unwrap();
        let mut prepared_operation = state.apply_to(&initial).unwrap();
        let pending = prepared_operation.pending_action.as_mut().unwrap();
        pending.dispatch_agent_generation = Some("0123456789abcdef0123456789abcdef".to_string());
        pending.dispatch_agent_control_version = Some(7);
        pending.dispatch_observed_runtime_epoch = Some(EpochStatus {
            data_loss_number: 4,
            configuration_number: 8,
        });
        pending.dispatch_action_payload =
            kuberic_core::grpc::convert::encode_direct_correlated_action_payload(
                &kuberic_core::types::DurableReplicaAction::RevokeWriteStatus,
            )
            .unwrap();
        let command = ReplicaEffectCommand::from_pending(pending).unwrap();
        let prepared = activity_spec(&DurableSwitchoverActivityInput {
            version: PILOT_VERSION,
            state: compact(&prepared_operation),
            kind: PilotActivityKind::PreparedReplica {
                command: command.clone(),
            },
        })
        .unwrap();
        validate_prepared_activity(
            &prepared_operation,
            &PilotActivityKind::PreparedReplica {
                command: command.clone(),
            },
        )
        .unwrap();
        let observations = OperationObservations::new();
        let addressed = BTreeMap::new();
        let resolver = PilotPreparedActivityResolver::new(&initial, &observations, &addressed, 100);
        assert_eq!(
            resolver.resolve(&logical, Some(&prepared)).unwrap(),
            prepared
        );

        let execution = execution_spec(&reference).unwrap();
        let mut variants = Vec::new();
        let mut changed = command.clone();
        changed.action_id.push_str("-other");
        variants.push(changed);
        let mut changed = command.clone();
        changed.action_signature.push_str("-other");
        variants.push(changed);
        let mut changed = command.clone();
        changed.target_id = 2;
        variants.push(changed);
        let mut changed = command.clone();
        changed.target_instance_id = "replacement-uid".to_string();
        variants.push(changed);
        let mut changed = command.clone();
        changed.expected_epoch.configuration_number += 1;
        variants.push(changed);
        let mut changed = command.clone();
        changed.desired_postcondition.kind = crate::crd::DurablePostconditionKind::Role;
        variants.push(changed);
        let mut changed = command.clone();
        changed.expected_agent_generation = "fedcba9876543210fedcba9876543210".to_string();
        variants.push(changed);
        let mut changed = command.clone();
        changed.expected_control_version += 1;
        variants.push(changed);
        let mut changed = command.clone();
        changed.observed_runtime_epoch.configuration_number += 1;
        variants.push(changed);
        let mut changed = command;
        changed.action_payload =
            kuberic_core::grpc::convert::encode_direct_correlated_action_payload(
                &kuberic_core::types::DurableReplicaAction::Close,
            )
            .unwrap();
        variants.push(changed);

        for changed in variants {
            assert!(
                validate_prepared_activity(
                    &prepared_operation,
                    &PilotActivityKind::PreparedReplica {
                        command: changed.clone(),
                    },
                )
                .is_err(),
                "restart validation must reject any changed prepared command"
            );
            let drifted = activity_spec(&DurableSwitchoverActivityInput {
                version: PILOT_VERSION,
                state: compact(&prepared_operation),
                kind: PilotActivityKind::PreparedReplica { command: changed },
            })
            .unwrap();
            assert_eq!(resolver.resolve(&logical, Some(&drifted)).unwrap(), logical);
            let payload = CheckpointPayload::active(
                ExecutionContract::new(
                    execution.clone(),
                    u64::try_from(PILOT_MAX_ENCODED_CHECKPOINT_BYTES).unwrap(),
                ),
                vec![ActivityRecord::scheduled(ActivitySequence::new(0), drifted)],
            );
            let checkpoint =
                CheckpointEnvelope::encode_with_limits(&payload, checkpoint_limits()).unwrap();
            assert!(matches!(
                kuberic_durable_execution::evaluate_prepared(
                    &DurableSwitchoverWorkflow,
                    &execution,
                    Some(&checkpoint),
                    checkpoint_limits(),
                    &resolver,
                ),
                kuberic_durable_execution::Evaluation::Nondeterminism(
                    kuberic_durable_execution::Nondeterminism::ActivityMismatch { .. }
                )
            ));
        }
    }

    #[test]
    fn replay_rejects_changed_activity_name_version_and_result_bound() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let initial = initial_operation(&reference).unwrap();
        let logical = activity_spec(&DurableSwitchoverActivityInput {
            version: PILOT_VERSION,
            state: compact(&initial),
            kind: PilotActivityKind::PassiveObservation,
        })
        .unwrap();
        let observations = OperationObservations::new();
        let addressed = BTreeMap::new();
        let resolver = PilotPreparedActivityResolver::new(&initial, &observations, &addressed, 100);
        assert_eq!(resolver.resolve(&logical, Some(&logical)).unwrap(), logical);

        let changed_name = ActivitySpec::new(
            ActivityName::new("kuberic.switchover.changed-name", logical.name().version()).unwrap(),
            logical.input().clone(),
            logical.max_result_bytes(),
        );
        let changed_version = ActivitySpec::new(
            ActivityName::new(
                logical.name().name(),
                logical.name().version().saturating_add(1),
            )
            .unwrap(),
            logical.input().clone(),
            logical.max_result_bytes(),
        );
        let changed_result_bound = ActivitySpec::new(
            logical.name().clone(),
            logical.input().clone(),
            logical.max_result_bytes().saturating_add(1),
        );
        let execution = execution_spec(&reference).unwrap();

        for changed in [changed_name, changed_version, changed_result_bound] {
            assert_eq!(resolver.resolve(&logical, Some(&changed)).unwrap(), logical);
            let payload = CheckpointPayload::active(
                ExecutionContract::new(
                    execution.clone(),
                    u64::try_from(PILOT_MAX_ENCODED_CHECKPOINT_BYTES).unwrap(),
                ),
                vec![ActivityRecord::scheduled(ActivitySequence::new(0), changed)],
            );
            let checkpoint =
                CheckpointEnvelope::encode_with_limits(&payload, checkpoint_limits()).unwrap();
            assert!(matches!(
                kuberic_durable_execution::evaluate_prepared(
                    &DurableSwitchoverWorkflow,
                    &execution,
                    Some(&checkpoint),
                    checkpoint_limits(),
                    &resolver,
                ),
                kuberic_durable_execution::Evaluation::Nondeterminism(
                    kuberic_durable_execution::Nondeterminism::ActivityMismatch { .. }
                )
            ));
        }
    }

    #[test]
    fn legacy_activity_v1_without_kind_replays_with_exact_recorded_bytes() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let initial = initial_operation(&reference).unwrap();
        let logical = activity_spec(&DurableSwitchoverActivityInput {
            version: PILOT_VERSION,
            state: compact(&initial),
            kind: PilotActivityKind::PassiveObservation,
        })
        .unwrap();
        let mut legacy_json: serde_json::Value =
            serde_json::from_slice(logical.input().as_slice()).unwrap();
        legacy_json.as_object_mut().unwrap().remove("kind");
        let legacy = ActivitySpec::new(
            logical.name().clone(),
            ExactBytes::new(serde_json::to_vec(&legacy_json).unwrap()),
            logical.max_result_bytes(),
        );
        let observations = OperationObservations::new();
        let addressed = BTreeMap::new();
        let resolver = PilotPreparedActivityResolver::new(&initial, &observations, &addressed, 100);
        assert_eq!(resolver.resolve(&logical, Some(&legacy)).unwrap(), legacy);
    }

    #[test]
    fn adapter_invokes_existing_switchover_decision_engine() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let operation = initial_operation(&reference).unwrap();
        let observations = OperationObservations::new();
        let PilotAdapterDecision::Observe(result) =
            evaluate_adapter_step(&operation, &observations, 100).unwrap()
        else {
            panic!("initial explicit decision must persist the correlated revoke action");
        };
        let DurableSwitchoverStepResult::Advance { operation: pending } = *result else {
            panic!("initial explicit decision must advance compact state");
        };
        let pending = pending.apply_to(&operation).unwrap();
        assert!(pending.pending_action.is_some());
        assert!(matches!(
            evaluate_adapter_step(&pending, &observations, 100).unwrap(),
            PilotAdapterDecision::AwaitEvidence
        ));
    }

    #[test]
    fn adapter_turns_effect_free_poison_into_stopped_result() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let initial = initial_operation(&reference).unwrap();
        let PilotAdapterDecision::Observe(result) =
            evaluate_adapter_step(&initial, &OperationObservations::new(), 100).unwrap()
        else {
            panic!("expected persisted revoke intent");
        };
        let DurableSwitchoverStepResult::Advance { operation: pending } = *result else {
            panic!("expected compact revoke state");
        };
        let pending = pending.apply_to(&initial).unwrap();
        let PilotAdapterDecision::Observe(result) =
            evaluate_adapter_step(&pending, &OperationObservations::new(), 1_000).unwrap()
        else {
            panic!("expired operation did not produce an observation");
        };
        assert!(matches!(
            *result,
            DurableSwitchoverStepResult::Stopped { .. }
        ));
    }

    #[tokio::test]
    async fn proven_no_admission_redelivery_is_bounded_per_action() {
        let mut reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let initial = initial_operation(&reference).unwrap();
        let PilotAdapterDecision::Observe(result) =
            evaluate_adapter_step(&initial, &OperationObservations::new(), 100).unwrap()
        else {
            panic!("expected persisted revoke action");
        };
        let DurableSwitchoverStepResult::Advance { operation: pending } = *result else {
            panic!("expected compact revoke state");
        };
        let pending = pending.apply_to(&initial).unwrap();
        let action_id = pending.pending_action.as_ref().unwrap().action_id.clone();
        reference.initial_operation_json = serde_json::to_string(&pending).unwrap();
        let execution = execution_spec(&reference).unwrap();
        let mut host = DurableHost::new(
            MeasuredPilotCheckpointStore::new(
                execution.execution_id(),
                PilotCheckpointStore::InMemory(InMemoryCheckpointStore::new()),
            ),
            HostEpoch::from_bytes([13; 16]),
            checkpoint_limits(),
        );
        let workflow = DurableSwitchoverWorkflow;

        for _ in 0..2 {
            let permit = expose_next(&mut host, &workflow, &execution).await;
            let observed = host
                .observe(
                    &execution,
                    ActivityObservation::new(
                        permit.activity().clone(),
                        encode_step_result(&DurableSwitchoverStepResult::ProvenNoAdmission {
                            operation: compact(&pending),
                            action_id: action_id.clone(),
                            redelivery: 1,
                        })
                        .unwrap(),
                    ),
                )
                .await;
            assert!(matches!(
                observed,
                kuberic_durable_execution::HostOutcome::ObservationAccepted { .. }
            ));
        }
        let completed = host.turn(&workflow, execution).await;
        let kuberic_durable_execution::HostOutcome::WorkflowCompleted { outcome, .. } = completed
        else {
            panic!("second no-admission redelivery must stop, found {completed:?}");
        };
        assert!(matches!(outcome, TerminalOutcome::Failed(_)));
    }

    #[tokio::test]
    async fn compensation_branch_terminalizes_failed_old_primary_topology() {
        let mut reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let mut failed = initial_operation(&reference).unwrap();
        failed.phase = crate::crd::DurableOperationPhase::Failed;
        reference.initial_operation_json = serde_json::to_string(&failed).unwrap();
        let execution = execution_spec(&reference).unwrap();
        let mut host = DurableHost::new(
            MeasuredPilotCheckpointStore::new(
                execution.execution_id(),
                PilotCheckpointStore::InMemory(InMemoryCheckpointStore::new()),
            ),
            HostEpoch::from_bytes([14; 16]),
            checkpoint_limits(),
        );
        let workflow = DurableSwitchoverWorkflow;
        let permit = expose_next(&mut host, &workflow, &execution).await;
        let previous = failed.previous_snapshot.cloned().unwrap();
        assert!(matches!(
            host.observe(
                &execution,
                ActivityObservation::new(
                    permit.activity().clone(),
                    encode_step_result(&DurableSwitchoverStepResult::Complete {
                        operation: compact(&failed),
                        snapshot: previous.clone(),
                        compensated: true,
                        accounting: Some(PilotActivityAccounting::new(0, 1)),
                    })
                    .unwrap(),
                ),
            )
            .await,
            kuberic_durable_execution::HostOutcome::ObservationAccepted { .. }
        ));
        let kuberic_durable_execution::HostOutcome::WorkflowCompleted { outcome, .. } =
            host.turn(&workflow, execution).await
        else {
            panic!("expected compensated terminal");
        };
        assert_eq!(
            decode_terminal(&outcome, &failed).unwrap(),
            DurableSwitchoverPilotTerminal::Complete {
                operation: failed,
                snapshot: previous,
                compensated: true,
                accounting: PilotActivityAccounting::new(0, 1),
            }
        );
    }

    #[tokio::test]
    async fn malformed_workflow_input_fails_without_scheduling_activity() {
        let execution = ExecutionSpec::new(
            ExecutionId::from_bytes([11; 16]),
            ExactBytes::new(b"not-json"),
            PILOT_MAX_TERMINAL_BYTES,
        );
        let mut host = DurableHost::new(
            MeasuredPilotCheckpointStore::new(
                execution.execution_id(),
                PilotCheckpointStore::InMemory(InMemoryCheckpointStore::new()),
            ),
            HostEpoch::from_bytes([9; 16]),
            checkpoint_limits(),
        );
        let outcome = host.turn(&DurableSwitchoverWorkflow, execution).await;
        let kuberic_durable_execution::HostOutcome::WorkflowCompleted { outcome, .. } = outcome
        else {
            panic!("malformed input must fail terminally, found {outcome:?}");
        };
        assert!(matches!(outcome, TerminalOutcome::Failed(_)));
        assert!(
            serde_json::from_slice::<serde_json::Value>(outcome.payload().as_slice()).unwrap()["status"]
                == "stopped"
        );
    }

    #[test]
    fn structurally_invalid_initial_operation_fails_before_activity_schedule() {
        let mut reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let mut invalid = initial_operation(&reference).unwrap();
        invalid.version = u32::MAX;
        reference.initial_operation_json = serde_json::to_string(&invalid).unwrap();
        let error = execution_spec(&reference).unwrap_err();
        assert!(
            error.contains("unsupported durable operation version"),
            "{error}"
        );
    }

    #[test]
    fn codecs_reject_identity_drift_and_oversized_results() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let initial = initial_operation(&reference).unwrap();
        let mut drifted = initial.clone();
        drifted.operation_id.push_str("-other");
        assert!(validate_transition(&initial, &drifted).is_err());

        let mut oversized = initial;
        oversized.last_error = Some("x".repeat(PILOT_MAX_ACTIVITY_RESULT_BYTES));
        let error = encode_step_result(&DurableSwitchoverStepResult::Advance {
            operation: compact(&oversized),
        })
        .unwrap_err();
        assert!(error.contains("activity result is"), "{error}");
    }

    #[test]
    fn compact_activity_payloads_exclude_immutable_operation_snapshots() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let operation = initial_operation(&reference).unwrap();
        let state = compact(&operation);
        let input =
            encode_activity_input::<DurableSwitchoverActivity>(&DurableSwitchoverActivityInput {
                version: PILOT_VERSION,
                state: state.clone(),
                kind: PilotActivityKind::PassiveObservation,
            })
            .unwrap();
        let result =
            encode_step_result(&DurableSwitchoverStepResult::Advance { operation: state }).unwrap();
        for payload in [input, result] {
            let text = std::str::from_utf8(payload.as_slice()).unwrap();
            assert!(!text.contains("previousSnapshot"));
            assert!(!text.contains("targetSnapshot"));
            assert!(!text.contains("operationId"));
            assert!(!text.contains("executionId"));
        }
    }

    #[test]
    fn effect_free_pending_creation_advances_without_an_activity() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let operation = initial_operation(&reference).unwrap();
        let pending = advance_effect_free(&operation).unwrap().unwrap();
        assert_eq!(pending.phase, operation.phase);
        assert!(pending.pending_action.is_some());
        assert!(advance_effect_free(&pending).unwrap().is_none());
    }

    #[tokio::test]
    async fn same_state_activity_result_stops_before_consuming_history_capacity() {
        let mut reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let mut initial = initial_operation(&reference).unwrap();
        initial.phase = DurableOperationPhase::CaptureLsn;
        reference.initial_operation_json = serde_json::to_string(&initial).unwrap();
        let execution = execution_spec(&reference).unwrap();
        let mut host = DurableHost::new(
            MeasuredPilotCheckpointStore::new(
                execution.execution_id(),
                PilotCheckpointStore::InMemory(InMemoryCheckpointStore::new()),
            ),
            HostEpoch::from_bytes([31; 16]),
            checkpoint_limits(),
        );
        let permit = expose_next(&mut host, &DurableSwitchoverWorkflow, &execution).await;
        let current = decode_activity_input_state(permit.activity().input()).unwrap();
        assert!(matches!(
            host.observe(
                &execution,
                ActivityObservation::new(
                    permit.activity().clone(),
                    encode_step_result(&DurableSwitchoverStepResult::Advance {
                        operation: current,
                    })
                    .unwrap(),
                ),
            )
            .await,
            kuberic_durable_execution::HostOutcome::ObservationAccepted { .. }
        ));
        let kuberic_durable_execution::HostOutcome::WorkflowCompleted { outcome, .. } =
            host.turn(&DurableSwitchoverWorkflow, execution).await
        else {
            panic!("same-state transcript did not stop");
        };
        let DurableSwitchoverPilotTerminal::Stopped { message, .. } =
            decode_terminal(&outcome, &initial).unwrap()
        else {
            panic!("no-progress result did not produce stopped terminal");
        };
        assert!(message.contains("no protocol progress"));
    }

    #[test]
    fn compensated_terminal_round_trips_exactly() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let operation = initial_operation(&reference).unwrap();
        let terminal = DurableSwitchoverPilotTerminal::Complete {
            snapshot: operation.previous_snapshot.cloned().unwrap(),
            operation: operation.clone(),
            compensated: true,
            accounting: PilotActivityAccounting::new(10, 3),
        };
        let outcome = TerminalOutcome::succeeded(encode_terminal(&terminal).unwrap());
        assert_eq!(decode_terminal(&outcome, &operation).unwrap(), terminal);
    }

    #[test]
    fn loaded_terminal_revalidates_identity_topology_and_outcome_kind() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let mut operation = initial_operation(&reference).unwrap();
        operation.phase = crate::crd::DurableOperationPhase::Completed;
        let valid = DurableSwitchoverPilotTerminal::Complete {
            snapshot: operation.target_snapshot.clone(),
            operation: operation.clone(),
            compensated: false,
            accounting: PilotActivityAccounting::new(9, 3),
        };
        let success = TerminalOutcome::succeeded(encode_terminal(&valid).unwrap());
        assert_eq!(
            validate_loaded_terminal(&reference, &success).unwrap(),
            valid
        );
        for accounting in [
            PilotActivityAccounting::new(10, 2),
            PilotActivityAccounting::new(8, 3),
        ] {
            let invalid_accounting = TerminalOutcome::succeeded(
                encode_terminal(&DurableSwitchoverPilotTerminal::Complete {
                    snapshot: operation.target_snapshot.clone(),
                    operation: operation.clone(),
                    compensated: false,
                    accounting,
                })
                .unwrap(),
            );
            assert!(
                validate_loaded_terminal(&reference, &invalid_accounting).is_err(),
                "successful terminal accepted invalid accounting {accounting:?}"
            );
        }

        let failed_kind = TerminalOutcome::failed(encode_terminal(&valid).unwrap());
        assert!(validate_loaded_terminal(&reference, &failed_kind).is_err());

        let mut wrong_identity = operation.clone();
        wrong_identity.operation_id.push_str("-other");
        let wrong_identity = TerminalOutcome::succeeded(
            encode_terminal(&DurableSwitchoverPilotTerminal::Complete {
                snapshot: wrong_identity.target_snapshot.clone(),
                operation: wrong_identity,
                compensated: false,
                accounting: PilotActivityAccounting::new(9, 3),
            })
            .unwrap(),
        );
        let canonical = validate_loaded_terminal(&reference, &wrong_identity).unwrap();
        let DurableSwitchoverPilotTerminal::Complete {
            operation: canonical,
            ..
        } = canonical
        else {
            panic!("expected compact completed terminal");
        };
        assert_eq!(
            canonical.operation_id,
            initial_operation(&reference).unwrap().operation_id,
            "immutable identity comes only from the authoritative initial reference"
        );

        let mut wrong_snapshot = operation.target_snapshot.clone();
        wrong_snapshot.primary_id = operation.old_primary_id;
        let wrong_snapshot = TerminalOutcome::succeeded(
            encode_terminal(&DurableSwitchoverPilotTerminal::Complete {
                operation,
                snapshot: wrong_snapshot,
                compensated: false,
                accounting: PilotActivityAccounting::new(9, 3),
            })
            .unwrap(),
        );
        assert!(validate_loaded_terminal(&reference, &wrong_snapshot).is_err());

        let mut failed = initial_operation(&reference).unwrap();
        failed.phase = crate::crd::DurableOperationPhase::Failed;
        let compensated = failed.previous_snapshot.cloned().unwrap();
        let valid_compensation = DurableSwitchoverPilotTerminal::Complete {
            operation: failed.clone(),
            snapshot: compensated.clone(),
            compensated: true,
            accounting: PilotActivityAccounting::new(8, 5),
        };
        assert!(
            validate_loaded_terminal(
                &reference,
                &TerminalOutcome::succeeded(encode_terminal(&valid_compensation).unwrap()),
            )
            .is_ok()
        );
        let invalid_compensation = TerminalOutcome::succeeded(
            encode_terminal(&DurableSwitchoverPilotTerminal::Complete {
                operation: failed.clone(),
                snapshot: compensated.clone(),
                compensated: true,
                accounting: PilotActivityAccounting::new(21, 3),
            })
            .unwrap(),
        );
        assert!(validate_loaded_terminal(&reference, &invalid_compensation).is_err());
        let impossible_passive_split = TerminalOutcome::succeeded(
            encode_terminal(&DurableSwitchoverPilotTerminal::Complete {
                operation: failed.clone(),
                snapshot: compensated.clone(),
                compensated: true,
                accounting: PilotActivityAccounting::new(1, 22),
            })
            .unwrap(),
        );
        assert!(validate_loaded_terminal(&reference, &impossible_passive_split).is_err());
        let impossible_redelivery_split = TerminalOutcome::succeeded(
            encode_terminal(&DurableSwitchoverPilotTerminal::Complete {
                operation: failed.clone(),
                snapshot: compensated.clone(),
                compensated: true,
                accounting: PilotActivityAccounting::new(10, 13),
            })
            .unwrap(),
        );
        assert!(validate_loaded_terminal(&reference, &impossible_redelivery_split).is_err());

        let mut malformed = Vec::new();
        let mut missing = compensated.clone();
        missing.members.pop();
        malformed.push(missing);
        let mut unique_extra = compensated.clone();
        let mut foreign = unique_extra.members[0].clone();
        foreign.id = 99;
        foreign.instance_id = "foreign".to_string();
        unique_extra.members.push(foreign.clone());
        malformed.push(unique_extra);
        let mut same_length_duplicate = compensated.clone();
        same_length_duplicate.members[2] = same_length_duplicate.members[0].clone();
        malformed.push(same_length_duplicate);
        let mut same_length_unknown = compensated.clone();
        same_length_unknown.members[2] = foreign;
        malformed.push(same_length_unknown);
        let mut wrong_incarnation = compensated.clone();
        wrong_incarnation.members[0].instance_id = "replacement".to_string();
        malformed.push(wrong_incarnation);
        let mut wrong_quorum = compensated.clone();
        wrong_quorum.write_quorum = wrong_quorum.write_quorum.saturating_add(1);
        malformed.push(wrong_quorum);
        let mut wrong_epoch = compensated;
        wrong_epoch.epoch.configuration_number += 9;
        malformed.push(wrong_epoch);
        for snapshot in malformed {
            let outcome = TerminalOutcome::succeeded(
                encode_terminal(&DurableSwitchoverPilotTerminal::Complete {
                    operation: failed.clone(),
                    snapshot,
                    compensated: true,
                    accounting: PilotActivityAccounting::new(8, 5),
                })
                .unwrap(),
            );
            assert!(validate_loaded_terminal(&reference, &outcome).is_err());
        }
    }

    #[test]
    fn transition_and_terminal_validation_rejects_shortcuts() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let initial = initial_operation(&reference).unwrap();
        let mut skipped = initial.clone();
        skipped.phase = crate::crd::DurableOperationPhase::PromoteTarget;
        assert!(validate_phase_transition(&initial, &skipped).is_err());

        let mut completed = initial.clone();
        completed.phase = crate::crd::DurableOperationPhase::Completed;
        assert!(validate_completion_transition(&initial, &completed, false).is_err());
        assert!(
            validate_terminal(
                &completed,
                &completed.previous_snapshot.cloned().unwrap(),
                false
            )
            .is_err()
        );
        assert!(validate_terminal(&completed, &completed.target_snapshot.clone(), true).is_err());
    }

    #[tokio::test]
    async fn schedule_unknown_outcomes_reload_before_any_permit() {
        for (fault, applied) in [
            (
                kuberic_durable_execution::InMemoryFault::OutcomeUnknownWithoutApply,
                false,
            ),
            (
                kuberic_durable_execution::InMemoryFault::OutcomeUnknownAfterApply,
                true,
            ),
        ] {
            let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
            let execution = execution_spec(&reference).unwrap();
            let backend = InMemoryCheckpointStore::new();
            backend.fail_next_compare_and_swap(fault);
            let mut host = DurableHost::new(
                MeasuredPilotCheckpointStore::new(
                    execution.execution_id(),
                    PilotCheckpointStore::InMemory(backend),
                ),
                HostEpoch::from_bytes([15; 16]),
                checkpoint_limits(),
            );
            let first = host
                .turn(&DurableSwitchoverWorkflow, execution.clone())
                .await;
            assert!(matches!(
                first,
                kuberic_durable_execution::HostOutcome::ReloadRequired {
                    boundary: kuberic_durable_execution::PersistenceBoundary::Schedule,
                    reason: kuberic_durable_execution::ReloadReason::OutcomeUnknown,
                }
            ));
            let second = host.turn(&DurableSwitchoverWorkflow, execution).await;
            assert_eq!(
                matches!(
                    &second,
                    kuberic_durable_execution::HostOutcome::DispatchPermitted { .. }
                ),
                applied
            );
            assert_eq!(
                matches!(
                    &second,
                    kuberic_durable_execution::HostOutcome::ScheduleAccepted { .. }
                ),
                !applied
            );
        }
    }

    #[tokio::test]
    async fn observation_faults_reload_effect_free_steps_without_dispatch() {
        use kuberic_durable_execution::{PersistenceBoundary, StoreOperation};

        for (fault, applied) in [
            (
                kuberic_durable_execution::InMemoryFault::ConflictWithoutApply,
                false,
            ),
            (
                kuberic_durable_execution::InMemoryFault::OutcomeUnknownWithoutApply,
                false,
            ),
            (
                kuberic_durable_execution::InMemoryFault::OutcomeUnknownAfterApply,
                true,
            ),
            (
                kuberic_durable_execution::InMemoryFault::FailBeforeRequest(
                    kuberic_durable_execution::StoreErrorKind::Unavailable,
                ),
                false,
            ),
        ] {
            let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
            let execution = execution_spec(&reference).unwrap();
            let backend = InMemoryCheckpointStore::new();
            let mut host = DurableHost::new(
                MeasuredPilotCheckpointStore::new(
                    execution.execution_id(),
                    PilotCheckpointStore::InMemory(backend.clone()),
                ),
                HostEpoch::from_bytes([18; 16]),
                checkpoint_limits(),
            );
            let permit = expose_next(&mut host, &DurableSwitchoverWorkflow, &execution).await;
            let mut next = decode_activity_input_state(permit.activity().input()).unwrap();
            next.last_error = Some("authoritative observation advanced state".to_string());
            let result = DurableSwitchoverStepResult::Advance { operation: next };
            backend.fail_next_compare_and_swap(fault);
            let observed = host
                .observe(
                    &execution,
                    ActivityObservation::new(
                        permit.activity().clone(),
                        encode_step_result(&result).unwrap(),
                    ),
                )
                .await;
            assert!(matches!(
                observed,
                kuberic_durable_execution::HostOutcome::ReloadRequired {
                    boundary: PersistenceBoundary::Observation,
                    ..
                } | kuberic_durable_execution::HostOutcome::StoreFailed {
                    operation: StoreOperation::CompareAndSwap(PersistenceBoundary::Observation),
                    ..
                }
            ));

            let mut restarted = DurableHost::new(
                MeasuredPilotCheckpointStore::new(
                    execution.execution_id(),
                    PilotCheckpointStore::InMemory(backend),
                ),
                HostEpoch::from_bytes([19; 16]),
                checkpoint_limits(),
            );
            let recovered = restarted.turn(&DurableSwitchoverWorkflow, execution).await;
            assert_eq!(
                matches!(
                    recovered,
                    kuberic_durable_execution::HostOutcome::ScheduleAccepted { .. }
                ),
                applied
            );
            assert_eq!(
                matches!(
                    recovered,
                    kuberic_durable_execution::HostOutcome::Quarantined { .. }
                ),
                !applied
            );
        }
    }

    #[tokio::test]
    async fn exposure_faults_reload_without_false_permits() {
        use kuberic_durable_execution::{PersistenceBoundary, StoreOperation};

        for (fault, applied) in [
            (
                kuberic_durable_execution::InMemoryFault::ConflictWithoutApply,
                false,
            ),
            (
                kuberic_durable_execution::InMemoryFault::OutcomeUnknownWithoutApply,
                false,
            ),
            (
                kuberic_durable_execution::InMemoryFault::OutcomeUnknownAfterApply,
                true,
            ),
            (
                kuberic_durable_execution::InMemoryFault::FailBeforeRequest(
                    kuberic_durable_execution::StoreErrorKind::Unavailable,
                ),
                false,
            ),
        ] {
            let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
            let execution = execution_spec(&reference).unwrap();
            let backend = InMemoryCheckpointStore::new();
            let mut host = DurableHost::new(
                MeasuredPilotCheckpointStore::new(
                    execution.execution_id(),
                    PilotCheckpointStore::InMemory(backend.clone()),
                ),
                HostEpoch::from_bytes([20; 16]),
                checkpoint_limits(),
            );
            assert!(matches!(
                host.turn(&DurableSwitchoverWorkflow, execution.clone())
                    .await,
                kuberic_durable_execution::HostOutcome::ScheduleAccepted { .. }
            ));
            backend.fail_next_compare_and_swap(fault);
            let exposure = host
                .turn(&DurableSwitchoverWorkflow, execution.clone())
                .await;
            assert!(matches!(
                exposure,
                kuberic_durable_execution::HostOutcome::ReloadRequired {
                    boundary: PersistenceBoundary::Exposure,
                    ..
                } | kuberic_durable_execution::HostOutcome::StoreFailed {
                    operation: StoreOperation::CompareAndSwap(PersistenceBoundary::Exposure),
                    ..
                }
            ));

            let mut restarted = DurableHost::new(
                MeasuredPilotCheckpointStore::new(
                    execution.execution_id(),
                    PilotCheckpointStore::InMemory(backend),
                ),
                HostEpoch::from_bytes([21; 16]),
                checkpoint_limits(),
            );
            let recovered = restarted.turn(&DurableSwitchoverWorkflow, execution).await;
            assert_eq!(
                matches!(
                    recovered,
                    kuberic_durable_execution::HostOutcome::Quarantined { .. }
                ),
                applied
            );
            assert_eq!(
                matches!(
                    recovered,
                    kuberic_durable_execution::HostOutcome::DispatchPermitted { .. }
                ),
                !applied
            );
        }
    }

    #[derive(Clone)]
    struct ConflictOnceStore {
        inner: InMemoryCheckpointStore,
        conflict: Arc<AtomicBool>,
    }

    #[async_trait]
    impl CheckpointStore for ConflictOnceStore {
        async fn load(
            &self,
            execution_id: ExecutionId,
        ) -> Result<Option<StoredCheckpoint>, StoreError> {
            self.inner.load(execution_id).await
        }

        async fn compare_and_swap(
            &self,
            execution_id: ExecutionId,
            expected: Option<StorageRevision>,
            checkpoint: CheckpointEnvelope,
        ) -> Result<CasOutcome, StoreError> {
            if self.conflict.swap(false, Ordering::SeqCst) {
                Ok(CasOutcome::Conflict)
            } else {
                self.inner
                    .compare_and_swap(execution_id, expected, checkpoint)
                    .await
            }
        }
    }

    #[tokio::test]
    async fn schedule_conflict_reloads_before_any_permit() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let execution = execution_spec(&reference).unwrap();
        let mut host = DurableHost::new(
            ConflictOnceStore {
                inner: InMemoryCheckpointStore::new(),
                conflict: Arc::new(AtomicBool::new(true)),
            },
            HostEpoch::from_bytes([16; 16]),
            checkpoint_limits(),
        );
        assert!(matches!(
            host.turn(&DurableSwitchoverWorkflow, execution.clone())
                .await,
            kuberic_durable_execution::HostOutcome::ReloadRequired {
                boundary: kuberic_durable_execution::PersistenceBoundary::Schedule,
                reason: kuberic_durable_execution::ReloadReason::Conflict,
            }
        ));
        assert!(matches!(
            host.turn(&DurableSwitchoverWorkflow, execution).await,
            kuberic_durable_execution::HostOutcome::ScheduleAccepted { .. }
        ));
    }

    #[test]
    fn success_and_rollback_transcripts_fit_with_redelivery_headroom() {
        let success = projected_success_steps(PILOT_MAX_REPLICAS);
        let rollback = projected_rollback_steps(PILOT_MAX_REPLICAS);
        assert_eq!(success.len(), 21);
        assert_eq!(rollback.len(), 23);
        let success_external_effects = external_effect_count(&success);
        let rollback_external_effects = external_effect_count(&rollback);
        assert_eq!(success_external_effects, 9);
        let success_label_effects = projected_label_effect_count(&success);
        assert_eq!(success_label_effects, 2);
        assert_eq!(success_external_effects - success_label_effects, 7);
        assert_eq!(projected_passive_observation_count(&success), 3);
        assert_eq!(
            success.len() - success_external_effects,
            12,
            "success projection includes three passive observations and nine redeliveries"
        );
        assert_eq!(rollback_external_effects, 10);
        assert_eq!(projected_passive_observation_count(&rollback), 3);
        assert_eq!(
            rollback.len() - rollback_external_effects,
            13,
            "rollback projection retains three passive observations and ten redeliveries"
        );
        assert!(success.len() <= PILOT_MAX_ACTIVITY_RECORDS);
        assert!(rollback.len() <= PILOT_MAX_ACTIVITY_RECORDS);
        assert!(success.len() + projected_success_pure_transitions() <= PILOT_MAX_TRANSITION_FUEL);
        assert!(
            rollback.len() + projected_rollback_pure_transitions() <= PILOT_MAX_TRANSITION_FUEL
        );
        assert!(success.contains(&"proven-no-admission-redelivery"));
        assert!(rollback.contains(&"rollback-promote.effect"));
    }
}
