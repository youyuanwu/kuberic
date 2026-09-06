//! Immutable contract and admission gate for the durable switchover pilot.
//!
//! The pilot deliberately reuses the explicit switchover operation as its
//! workflow input. Per-turn workflow and reconciliation behavior is added by
//! later phases; this module owns only identity, bounds, and Kubernetes
//! lifecycle policy.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kuberic_durable_execution::{
    ActivityName, ActivityRecord, ActivitySequence, ActivitySpec, CasOutcome, CheckpointEnvelope,
    CheckpointLimits, CheckpointPayload, CheckpointStore, DispatchPermit, DurableHost, ExactBytes,
    ExecutionContract, ExecutionId, ExecutionSpec, HostEpoch, InMemoryCheckpointStore,
    KubernetesCheckpointOwner, KubernetesCheckpointOwnerScope, KubernetesCheckpointStore,
    KubernetesCheckpointStoreOptions, LogicalActivityId, StorageRevision, StoreError,
    StoredCheckpoint, TerminalOutcome, Workflow, WorkflowContext,
};
use rand::random;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::crd::{
    DurableOperationStatus, DurableSwitchoverPilotStatus, StablePartitionSnapshotStatus,
};

use super::pilot_store::MeasuredPilotCheckpointStore;
use super::{
    Decision, OperationObservations, decide, start_switchover,
    switchover::validate_switchover_operation,
};

pub const PILOT_VERSION: u32 = 1;
pub const PILOT_MAX_REPLICAS: usize = 3;
pub const PILOT_MAX_ACTIVITY_RECORDS: usize = 48;
pub const PILOT_MAX_OPERATION_BYTES: usize = 3_000;
pub const PILOT_MAX_ACTIVITY_RESULT_BYTES: usize = 4_096;
pub const PILOT_MAX_TERMINAL_BYTES: u64 = 4_096;
pub const PILOT_MAX_ENCODED_CHECKPOINT_BYTES: usize = 704 * 1_024;

const PILOT_ACTIVITY_NAME: &str = "kuberic.switchover.explicit-step";
const PILOT_ACTIVITY_VERSION: u32 = 1;

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
}

impl DurableSwitchoverPilotRuntime {
    pub fn kubernetes(client: kube::Client) -> Self {
        Self {
            factory: PilotStoreFactory::Kubernetes(client),
            host_epoch: HostEpoch::from_bytes(random()),
            hosts: Mutex::new(HashMap::new()),
        }
    }

    pub fn in_memory(store: InMemoryCheckpointStore) -> Self {
        Self {
            factory: PilotStoreFactory::InMemory(store),
            host_epoch: HostEpoch::from_bytes(random()),
            hosts: Mutex::new(HashMap::new()),
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
        self.hosts.lock().await.remove(&PilotHostKey {
            namespace: namespace.to_string(),
            set_name: set_name.to_string(),
            set_uid: set_uid.to_string(),
            execution_id: execution_id.to_string(),
        });
    }

    pub async fn host_count(&self) -> usize {
        self.hosts.lock().await.len()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableSwitchoverPilotInput {
    pub version: u32,
    pub execution_id: String,
    pub initial_operation: DurableOperationStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum DurableSwitchoverStepResult {
    Advance {
        operation: DurableOperationStatus,
    },
    ProvenNoAdmission {
        operation: DurableOperationStatus,
        action_id: String,
        redelivery: u8,
    },
    Complete {
        operation: DurableOperationStatus,
        snapshot: StablePartitionSnapshotStatus,
        compensated: bool,
    },
    Stopped {
        operation: DurableOperationStatus,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum DurableSwitchoverPilotTerminal {
    Complete {
        operation: DurableOperationStatus,
        snapshot: StablePartitionSnapshotStatus,
        compensated: bool,
    },
    Stopped {
        operation: Option<DurableOperationStatus>,
        message: String,
    },
}

pub struct DurableSwitchoverWorkflow;

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
        if let Err(error) = validate_pilot_admission(&initial) {
            return terminal_failure(Some(initial), error);
        }
        if let Err(error) = validate_switchover_operation(&initial) {
            return terminal_failure(Some(initial), error);
        }
        let mut operation = initial.clone();
        let mut no_admission_redeliveries = std::collections::BTreeMap::<String, u8>::new();

        for _ in 0..PILOT_MAX_ACTIVITY_RECORDS {
            let activity = match activity_spec(&operation) {
                Ok(activity) => activity,
                Err(error) => return terminal_failure(Some(operation), error),
            };
            let encoded = context.activity(activity).await;
            let result: DurableSwitchoverStepResult =
                match serde_json::from_slice(encoded.as_slice()) {
                    Ok(result) => result,
                    Err(error) => {
                        return terminal_failure(
                            Some(operation),
                            format!("decode pilot activity result: {error}"),
                        );
                    }
                };
            match result {
                DurableSwitchoverStepResult::Advance { operation: next } => {
                    if let Err(error) = validate_transition(&initial, &next) {
                        return terminal_failure(Some(operation), error);
                    }
                    if let Err(error) = validate_phase_transition(&operation, &next) {
                        return terminal_failure(Some(operation), error);
                    }
                    operation = next;
                }
                DurableSwitchoverStepResult::ProvenNoAdmission {
                    operation: next,
                    action_id,
                    redelivery,
                } => {
                    if let Err(error) = validate_transition(&initial, &next) {
                        return terminal_failure(Some(operation), error);
                    }
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
                    operation = next;
                }
                DurableSwitchoverStepResult::Complete {
                    operation: completed,
                    snapshot,
                    compensated,
                } => {
                    if let Err(error) = validate_transition(&initial, &completed) {
                        return terminal_failure(Some(operation), error);
                    }
                    if let Err(error) =
                        validate_completion_transition(&operation, &completed, compensated)
                    {
                        return terminal_failure(Some(operation), error);
                    }
                    if let Err(error) = validate_terminal(&completed, &snapshot, compensated) {
                        return terminal_failure(Some(operation), error);
                    }
                    return terminal_success(DurableSwitchoverPilotTerminal::Complete {
                        operation: completed,
                        snapshot,
                        compensated,
                    });
                }
                DurableSwitchoverStepResult::Stopped {
                    operation: stopped,
                    message,
                } => {
                    if let Err(error) = validate_transition(&initial, &stopped) {
                        return terminal_failure(Some(operation), error);
                    }
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
            Some(operation),
            format!(
                "durable switchover exhausted its {PILOT_MAX_ACTIVITY_RECORDS}-activity admission"
            ),
        )
    }
}

pub fn encode_step_result(result: &DurableSwitchoverStepResult) -> Result<ExactBytes, String> {
    let encoded = serde_json::to_vec(result)
        .map_err(|error| format!("serialize pilot activity result: {error}"))?;
    if encoded.len() > PILOT_MAX_ACTIVITY_RESULT_BYTES {
        return Err(format!(
            "durable switchover activity result is {} bytes; maximum is {}",
            encoded.len(),
            PILOT_MAX_ACTIVITY_RESULT_BYTES
        ));
    }
    Ok(ExactBytes::new(encoded))
}

pub fn decode_terminal(
    outcome: &TerminalOutcome,
) -> Result<DurableSwitchoverPilotTerminal, String> {
    serde_json::from_slice(outcome.payload().as_slice())
        .map_err(|error| format!("decode durable switchover terminal outcome: {error}"))
}

pub enum PilotAdapterDecision {
    Observe(DurableSwitchoverStepResult),
    AwaitEvidence,
    External(Decision),
}

pub fn evaluate_adapter_step(
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
    now: i64,
) -> Result<PilotAdapterDecision, String> {
    Ok(match decide(operation, observations, now)? {
        Decision::Persist(operation) => {
            PilotAdapterDecision::Observe(DurableSwitchoverStepResult::Advance { operation })
        }
        Decision::Complete {
            operation,
            snapshot,
            compensated,
        } => PilotAdapterDecision::Observe(DurableSwitchoverStepResult::Complete {
            operation,
            snapshot,
            compensated,
        }),
        Decision::Wait => PilotAdapterDecision::AwaitEvidence,
        external @ (Decision::Execute { .. } | Decision::PatchPodRole { .. }) => {
            PilotAdapterDecision::External(external)
        }
        other => {
            return Err(format!(
                "unsupported explicit switchover decision reached pilot adapter: {other:?}"
            ));
        }
    })
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
    ) -> Result<DispatchPermit, String> {
        let expected = activity_spec(operation)?;
        let permit = self
            .permit
            .as_ref()
            .ok_or_else(|| "durable switchover dispatch permit was already consumed".to_string())?;
        if permit.activity().spec() != &expected {
            return Err(
                "durable switchover dispatch permit does not match current operation".to_string(),
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

fn activity_spec(operation: &DurableOperationStatus) -> Result<ActivitySpec, String> {
    let input = serde_json::to_vec(operation)
        .map_err(|error| format!("serialize pilot activity input: {error}"))?;
    if input.len() > PILOT_MAX_OPERATION_BYTES {
        return Err(format!(
            "durable switchover activity input is {} bytes; maximum is {}",
            input.len(),
            PILOT_MAX_OPERATION_BYTES
        ));
    }
    Ok(ActivitySpec::new(
        ActivityName::new(PILOT_ACTIVITY_NAME, PILOT_ACTIVITY_VERSION)
            .map_err(|error| format!("construct pilot activity name: {error}"))?,
        ExactBytes::new(input),
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
    validate_pilot_admission(next)?;
    validate_switchover_operation(next)
}

fn validate_phase_transition(
    current: &DurableOperationStatus,
    next: &DurableOperationStatus,
) -> Result<(), String> {
    use crate::crd::DurableOperationPhase as Phase;
    let allowed = current.phase == next.phase
        || next.phase == Phase::Poisoned
        || matches!(
            (current.phase, next.phase),
            (Phase::Revoke, Phase::CaptureLsn | Phase::Failed)
                | (Phase::CaptureLsn, Phase::PreCatchUp)
                | (
                    Phase::PreCatchUp,
                    Phase::DemoteOldPrimary | Phase::RestorePreviousConfiguration
                )
                | (
                    Phase::DemoteOldPrimary,
                    Phase::PromoteTarget | Phase::RestorePreviousConfiguration
                )
                | (
                    Phase::PromoteTarget,
                    Phase::DistributeEpoch | Phase::CompensatePromoteOldPrimary
                )
                | (Phase::DistributeEpoch, Phase::UpdateCatchUpConfiguration)
                | (
                    Phase::UpdateCatchUpConfiguration,
                    Phase::WaitForCatchUpQuorum
                )
                | (
                    Phase::WaitForCatchUpQuorum,
                    Phase::UpdateCurrentConfiguration
                )
                | (Phase::UpdateCurrentConfiguration, Phase::LabelTargetPrimary)
                | (Phase::LabelTargetPrimary, Phase::LabelOldSecondary)
                | (Phase::LabelOldSecondary, Phase::Finalize)
                | (Phase::RestorePreviousConfiguration, Phase::Failed)
                | (
                    Phase::CompensatePromoteOldPrimary,
                    Phase::CompensateDistributeEpoch
                )
                | (
                    Phase::CompensateDistributeEpoch,
                    Phase::CompensateCatchUpConfiguration
                )
                | (
                    Phase::CompensateCatchUpConfiguration,
                    Phase::CompensateCurrentConfiguration
                )
                | (
                    Phase::CompensateCurrentConfiguration,
                    Phase::CompensateLabelOldPrimary
                )
                | (
                    Phase::CompensateLabelOldPrimary,
                    Phase::CompensateLabelTargetSecondary
                )
                | (
                    Phase::CompensateLabelTargetSecondary,
                    Phase::CompensateFinalize
                )
        );
    if allowed {
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

fn validate_terminal(
    operation: &DurableOperationStatus,
    snapshot: &StablePartitionSnapshotStatus,
    compensated: bool,
) -> Result<(), String> {
    use crate::crd::{DurableOperationPhase as Phase, StableReplicaRoleStatus};
    if !compensated {
        if operation.phase != Phase::Completed
            || operation.pending_action.is_some()
            || !same_topology(snapshot, &operation.target_snapshot)
        {
            return Err("successful durable switchover terminal is inconsistent".to_string());
        }

        return Ok(());
    }
    if operation.phase != Phase::Failed
        || operation.pending_action.is_some()
        || snapshot.primary_id != operation.old_primary_id
        || snapshot.members.iter().any(|member| {
            (member.id == operation.old_primary_id
                && member.role != StableReplicaRoleStatus::Primary)
                || (member.id != operation.old_primary_id
                    && member.role != StableReplicaRoleStatus::ActiveSecondary)
        })
    {
        return Err("compensated durable switchover terminal is inconsistent".to_string());
    }
    Ok(())
}

fn same_topology(
    actual: &StablePartitionSnapshotStatus,
    expected: &StablePartitionSnapshotStatus,
) -> bool {
    actual.epoch == expected.epoch
        && actual.primary_id == expected.primary_id
        && actual.write_quorum == expected.write_quorum
        && actual.members.len() == expected.members.len()
        && actual
            .members
            .iter()
            .zip(&expected.members)
            .all(|(actual_member, expected_member)| {
                actual_member.id == expected_member.id
                    && actual_member.instance_id == expected_member.instance_id
                    && actual_member.role == expected_member.role
            })
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

fn terminal_failure(operation: Option<DurableOperationStatus>, message: String) -> TerminalOutcome {
    let bounded_message: String = message.chars().take(512).collect();
    let terminal = DurableSwitchoverPilotTerminal::Stopped {
        operation,
        message: bounded_message,
    };
    let payload = encode_terminal(&terminal).unwrap_or_else(|_| {
        ExactBytes::new(br#"{"status":"stopped","operation":null,"message":"pilot terminal payload exceeded its bound"}"#)
    });
    TerminalOutcome::failed(payload)
}

fn encode_terminal(terminal: &DurableSwitchoverPilotTerminal) -> Result<ExactBytes, String> {
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
    if operation.previous_snapshot.members.len() > PILOT_MAX_REPLICAS {
        return Err(format!(
            "durable switchover pilot supports at most {PILOT_MAX_REPLICAS} replicas; found {}",
            operation.previous_snapshot.members.len()
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
    validate_variant_bounds(operation)?;
    let success_steps = projected_success_steps(operation.previous_snapshot.members.len());
    let rollback_steps = projected_rollback_steps(operation.previous_snapshot.members.len());
    let projected_steps = success_steps.len().max(rollback_steps.len());
    if projected_steps > PILOT_MAX_ACTIVITY_RECORDS {
        return Err(format!(
            "durable switchover requires {projected_steps} projected activities; maximum is {PILOT_MAX_ACTIVITY_RECORDS}"
        ));
    }
    let projected = maximum_active_checkpoint()?;
    let projected_bytes = projected
        .encoded_len()
        .map_err(|error| format!("measure maximum pilot checkpoint: {error}"))?;
    if projected_bytes > PILOT_MAX_ENCODED_CHECKPOINT_BYTES {
        return Err(format!(
            "durable switchover projected checkpoint is {projected_bytes} bytes; maximum is {PILOT_MAX_ENCODED_CHECKPOINT_BYTES}"
        ));
    }
    Ok(())
}

fn projected_success_steps(member_count: usize) -> Vec<&'static str> {
    let mut steps = vec![
        "revoke.prepare",
        "revoke.fence",
        "revoke.resolve",
        "capture-lsn",
        "target-catch-up",
        "demote.prepare",
        "demote.fence",
        "demote.resolve",
        "promote.prepare",
        "promote.fence",
        "promote.resolve",
    ];
    for _ in 0..member_count.saturating_sub(2) {
        steps.extend(["epoch.prepare", "epoch.fence", "epoch.resolve"]);
    }

    steps.extend([
        "catch-up-config.prepare",
        "catch-up-config.fence",
        "catch-up-config.resolve",
        "quorum.prepare",
        "quorum.fence",
        "quorum.resolve",
        "current-config.prepare",
        "current-config.fence",
        "current-config.resolve",
        "target-label.prepare",
        "target-label.resolve",
        "old-label.prepare",
        "old-label.resolve",
        "final-attestation",
    ]);
    let redelivery_headroom = external_effect_count(&steps);
    steps.extend(std::iter::repeat_n(
        "proven-no-admission-redelivery",
        redelivery_headroom,
    ));
    steps
}

fn validate_variant_bounds(operation: &DurableOperationStatus) -> Result<(), String> {
    let previous = operation
        .previous_snapshot
        .cloned()
        .ok_or_else(|| "durable switchover has no previous snapshot".to_string())?;
    let mut stopped = operation.clone();
    stopped.phase = crate::crd::DurableOperationPhase::Poisoned;
    stopped.pending_action = None;
    let variants = [
        DurableSwitchoverStepResult::Advance {
            operation: operation.clone(),
        },
        DurableSwitchoverStepResult::ProvenNoAdmission {
            operation: operation.clone(),
            action_id: operation
                .pending_action
                .as_ref()
                .map(|pending| pending.action_id.clone())
                .unwrap_or_else(|| format!("{}:maximum-action", operation.operation_id)),
            redelivery: 1,
        },
        DurableSwitchoverStepResult::Complete {
            operation: operation.clone(),
            snapshot: operation.target_snapshot.clone(),
            compensated: false,
        },
        DurableSwitchoverStepResult::Complete {
            operation: operation.clone(),
            snapshot: previous.clone(),
            compensated: true,
        },
        DurableSwitchoverStepResult::Stopped {
            operation: stopped.clone(),
            message: "x".repeat(512),
        },
    ];
    for variant in &variants {
        encode_step_result(variant)?;
    }
    for terminal in [
        DurableSwitchoverPilotTerminal::Complete {
            operation: operation.clone(),
            snapshot: operation.target_snapshot.clone(),
            compensated: false,
        },
        DurableSwitchoverPilotTerminal::Complete {
            operation: operation.clone(),
            snapshot: previous,
            compensated: true,
        },
        DurableSwitchoverPilotTerminal::Stopped {
            operation: Some(stopped),
            message: "x".repeat(512),
        },
    ] {
        encode_terminal(&terminal)?;
    }
    Ok(())
}

fn projected_rollback_steps(member_count: usize) -> Vec<&'static str> {
    let mut steps = vec![
        "revoke.prepare",
        "revoke.fence",
        "revoke.resolve",
        "capture-lsn",
        "target-catch-up",
        "demote.prepare",
        "demote.fence",
        "demote.resolve",
        "promote.prepare",
        "promote.fence",
        "promote.failure",
        "rollback-promote.prepare",
        "rollback-promote.fence",
        "rollback-promote.resolve",
    ];
    for _ in 0..member_count.saturating_sub(1) {
        steps.extend([
            "rollback-epoch.prepare",
            "rollback-epoch.fence",
            "rollback-epoch.resolve",
        ]);
    }
    steps.extend([
        "rollback-catch-up.prepare",
        "rollback-catch-up.fence",
        "rollback-catch-up.resolve",
        "rollback-current.prepare",
        "rollback-current.fence",
        "rollback-current.resolve",
        "rollback-old-label.prepare",
        "rollback-old-label.resolve",
        "rollback-target-label.prepare",
        "rollback-target-label.resolve",
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
        .filter(|step| step.ends_with(".resolve") || step.ends_with(".failure"))
        .count()
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
                ExactBytes::new(vec![u8::MAX; PILOT_MAX_OPERATION_BYTES]),
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
                        operation: advanced.clone(),
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
                        operation: advanced.clone(),
                        snapshot: terminal_snapshot.clone(),
                        compensated: false,
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
            decode_terminal(&outcome).unwrap(),
            DurableSwitchoverPilotTerminal::Complete {
                operation: advanced,
                snapshot: terminal_snapshot,
                compensated: false,
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
                        operation: stopped.clone(),
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
            decode_terminal(&outcome).unwrap(),
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
        let mut guard = PilotPermitGuard::new(permit);
        assert!(guard.activity().is_some());
        let consumed = guard.consume_for(&operation).unwrap();
        assert_eq!(
            consumed.activity().input().as_slice(),
            serde_json::to_vec(&operation).unwrap()
        );
        assert!(guard.consume_for(&operation).is_err());
    }

    #[test]
    fn adapter_invokes_existing_switchover_decision_engine() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let operation = initial_operation(&reference).unwrap();
        let observations = OperationObservations::new();
        let PilotAdapterDecision::Observe(DurableSwitchoverStepResult::Advance {
            operation: pending,
        }) = evaluate_adapter_step(&operation, &observations, 100).unwrap()
        else {
            panic!("initial explicit decision must persist the correlated revoke action");
        };
        assert!(pending.pending_action.is_some());
        assert!(matches!(
            evaluate_adapter_step(&pending, &observations, 100).unwrap(),
            PilotAdapterDecision::AwaitEvidence
        ));
    }

    #[tokio::test]
    async fn proven_no_admission_redelivery_is_bounded_per_action() {
        let mut reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let initial = initial_operation(&reference).unwrap();
        let PilotAdapterDecision::Observe(DurableSwitchoverStepResult::Advance {
            operation: pending,
        }) = evaluate_adapter_step(&initial, &OperationObservations::new(), 100).unwrap()
        else {
            panic!("expected persisted revoke action");
        };
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
                            operation: pending.clone(),
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
                        operation: failed.clone(),
                        snapshot: previous.clone(),
                        compensated: true,
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
            decode_terminal(&outcome).unwrap(),
            DurableSwitchoverPilotTerminal::Complete {
                operation: failed,
                snapshot: previous,
                compensated: true,
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
        assert!(matches!(
            decode_terminal(&outcome).unwrap(),
            DurableSwitchoverPilotTerminal::Stopped {
                operation: None,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn structurally_invalid_initial_operation_fails_before_activity_schedule() {
        let mut reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let mut invalid = initial_operation(&reference).unwrap();
        invalid.version = u32::MAX;
        reference.initial_operation_json = serde_json::to_string(&invalid).unwrap();
        let execution = execution_spec(&reference).unwrap();
        let backend = InMemoryCheckpointStore::new();
        let mut host = DurableHost::new(
            MeasuredPilotCheckpointStore::new(
                execution.execution_id(),
                PilotCheckpointStore::InMemory(backend.clone()),
            ),
            HostEpoch::from_bytes([17; 16]),
            checkpoint_limits(),
        );
        let outcome = host
            .turn(&DurableSwitchoverWorkflow, execution.clone())
            .await;
        assert!(matches!(
            outcome,
            kuberic_durable_execution::HostOutcome::WorkflowCompleted {
                outcome: TerminalOutcome::Failed(_),
                ..
            }
        ));
        let stored = backend
            .load(execution.execution_id())
            .await
            .unwrap()
            .unwrap();
        let payload = stored
            .checkpoint()
            .decode_and_validate(&execution, checkpoint_limits())
            .unwrap();
        assert!(payload.active_activities().is_none());
        assert_eq!(payload.terminal_outcome().unwrap().1, 0);
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
            operation: oversized,
        })
        .unwrap_err();
        assert!(error.contains("activity result is"), "{error}");
    }

    #[test]
    fn compensated_terminal_round_trips_exactly() {
        let reference = new_pilot_reference("set-uid", snapshot(3), 2, 100).unwrap();
        let operation = initial_operation(&reference).unwrap();
        let terminal = DurableSwitchoverPilotTerminal::Complete {
            snapshot: operation.previous_snapshot.cloned().unwrap(),
            operation,
            compensated: true,
        };
        let outcome = TerminalOutcome::succeeded(encode_terminal(&terminal).unwrap());
        assert_eq!(decode_terminal(&outcome).unwrap(), terminal);
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
        assert_eq!(success.len(), 37);
        assert_eq!(rollback.len(), 41);
        assert!(success.len() <= PILOT_MAX_ACTIVITY_RECORDS);
        assert!(rollback.len() <= PILOT_MAX_ACTIVITY_RECORDS);
        assert!(success.contains(&"proven-no-admission-redelivery"));
        assert!(rollback.contains(&"rollback-promote.resolve"));
    }
}
