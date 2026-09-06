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
    CheckpointLimits, CheckpointPayload, CheckpointStore, DurableHost, ExactBytes,
    ExecutionContract, ExecutionId, ExecutionSpec, HostEpoch, InMemoryCheckpointStore,
    KubernetesCheckpointOwner, KubernetesCheckpointOwnerScope, KubernetesCheckpointStore,
    KubernetesCheckpointStoreOptions, StorageRevision, StoreError, StoredCheckpoint,
};
use rand::random;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::crd::{
    DurableOperationStatus, DurableSwitchoverPilotStatus, StablePartitionSnapshotStatus,
};

use super::start_switchover;

pub const PILOT_VERSION: u32 = 1;
pub const PILOT_MAX_REPLICAS: usize = 3;
pub const PILOT_MAX_ACTIVITY_RECORDS: usize = 48;
pub const PILOT_MAX_OPERATION_BYTES: usize = 4_096;
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

pub type PilotHost = DurableHost<PilotCheckpointStore>;

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
            store,
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableSwitchoverPilotInput {
    pub version: u32,
    pub execution_id: String,
    pub initial_operation: DurableOperationStatus,
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
                u64::try_from(PILOT_MAX_OPERATION_BYTES).expect("pilot operation bound fits u64"),
            );
            ActivityRecord::completed(
                ActivitySequence::new(
                    u64::try_from(sequence).expect("pilot activity count fits u64"),
                ),
                spec,
                ExactBytes::new(vec![u8::MAX; PILOT_MAX_OPERATION_BYTES]),
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
mod tests {
    use super::*;
    use crate::crd::{EpochStatus, StableReplicaRoleStatus, StableReplicaSnapshotStatus};
    use crate::durable::{Decision, decide};
    use std::collections::BTreeMap;

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
