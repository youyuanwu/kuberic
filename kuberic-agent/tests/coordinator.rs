use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kuberic_agent::command::admit_configuration;
use kuberic_agent::coordinator::Coordinator;
use kuberic_agent::runtime_adapter::{RuntimeAdapter, RuntimeEffectExecutor};
use kuberic_agent::sqlite_store::SqliteStore;
use kuberic_agent::state::{AgentState, SCHEMA_VERSION, StorageIdentity};
use kuberic_agent::store::AgentStore;
use kuberic_agent::{AgentError, Result};
use kuberic_protocol::command::EnsureConfiguration;
use kuberic_protocol::types::{
    AccessStatus, AgentGeneration, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy,
    Epoch, InitializationId, OperationId, PodUid, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaRole, ResourceUid, TransitionKind,
};
use kuberic_runtime::RuntimeError;
use kuberic_runtime_internal::effects::{
    RoleTransition, RuntimeEffect, RuntimeEffectAction, RuntimeEffectResult, RuntimePostcondition,
};
use tempfile::tempdir;

struct FakeRuntime {
    state: Mutex<RuntimePostcondition>,
    calls: Mutex<Vec<&'static str>>,
    fail_once: Mutex<Option<&'static str>>,
}

impl FakeRuntime {
    fn new() -> Self {
        Self {
            state: Mutex::new(RuntimePostcondition {
                open: true,
                role: ReplicaRole::None,
                role_transition: None,
                read_status: AccessStatus::NotPrimary,
                write_status: AccessStatus::NotPrimary,
                authority: None,
                current_progress: 7,
                verified_replication_lsn: Some(7),
                committed_lsn: 7,
                current_configuration_quorum_progress: 7,
                catch_up_boundary: None,
                catch_up_complete: true,
                builds: Vec::new(),
            }),
            calls: Mutex::new(Vec::new()),
            fail_once: Mutex::new(None),
        }
    }

    fn fail_once(&self, stage: &'static str) {
        *self.fail_once.lock().unwrap() = Some(stage);
    }
}

#[async_trait]
impl RuntimeEffectExecutor for FakeRuntime {
    async fn apply_runtime_effect(&self, effect: RuntimeEffect) -> Result<RuntimeEffectResult> {
        let stage = match &effect.action {
            RuntimeEffectAction::AdmitAuthority(_) => "admit",
            RuntimeEffectAction::SetReadStatus(_) => "read",
            RuntimeEffectAction::SetAccessStatus { .. } => "access",
            RuntimeEffectAction::RefreshApplicationProgress => "get-lsn",
            RuntimeEffectAction::WaitForCatchup => "catchup",
            RuntimeEffectAction::SetWriteStatus(_) => "write",
            RuntimeEffectAction::ChangeReplicatorRole(_) => "replicator-role",
            RuntimeEffectAction::UpdateEpoch => "epoch",
            RuntimeEffectAction::ChangeApplicationRole(_) => "application-role",
            action => panic!("unexpected coordinator action {action:?}"),
        };
        self.calls.lock().unwrap().push(stage);
        let mut fail_once = self.fail_once.lock().unwrap();
        if *fail_once == Some(stage) {
            *fail_once = None;
            return Err(RuntimeError::Application("injected failure".into()).into());
        }
        drop(fail_once);
        let mut state = self.state.lock().unwrap();
        match effect.action {
            RuntimeEffectAction::AdmitAuthority(authority) => {
                state.authority = Some(*authority);
                state.read_status = AccessStatus::ReconfigurationPending;
                state.write_status = AccessStatus::ReconfigurationPending;
            }
            RuntimeEffectAction::SetReadStatus(status) => state.read_status = status,
            RuntimeEffectAction::SetAccessStatus { read, write } => {
                state.read_status = read;
                state.write_status = write;
            }
            RuntimeEffectAction::SetWriteStatus(status) => state.write_status = status,
            RuntimeEffectAction::RefreshApplicationProgress
            | RuntimeEffectAction::WaitForCatchup => {}
            RuntimeEffectAction::ChangeReplicatorRole(role) => {
                state.role_transition = Some(RoleTransition {
                    completed_role: state.role,
                    target_role: role,
                    replicator_completed: true,
                    epoch_completed: role != ReplicaRole::Primary,
                    application_completed: false,
                });
            }
            RuntimeEffectAction::UpdateEpoch => {
                state
                    .role_transition
                    .as_mut()
                    .expect("replicator role stage")
                    .epoch_completed = true;
            }
            RuntimeEffectAction::ChangeApplicationRole(role) => {
                state.role = role;
                state.role_transition = None;
            }
            _ => unreachable!(),
        }
        Ok(RuntimeEffectResult {
            operation_id: effect.operation_id,
            sequence: effect.sequence,
            postcondition: state.clone(),
        })
    }
}

fn identity() -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(1),
        instance_id: ReplicaInstanceId::new("pod-1"),
        agent_generation: AgentGeneration::new("generation-1"),
    }
}

fn storage_identity() -> StorageIdentity {
    StorageIdentity {
        schema_version: SCHEMA_VERSION,
        resource_uid: ResourceUid::new("resource-1"),
        pod_uid: PodUid::new("pod-1"),
        pvc_uid: PvcUid::new("pvc-1"),
        initialization_id: InitializationId::new("init-1"),
        local_identity: identity(),
        effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
    }
}

fn command(operation_id: &str, epoch: Epoch) -> EnsureConfiguration {
    let local = identity();
    let policy = EffectivePolicy::fixed(1, 30).unwrap();
    let current_configuration = ConfigurationDescriptor::new(
        epoch,
        local.replica_id,
        vec![ConfigurationMember {
            identity: local.clone(),
            role: ReplicaRole::Primary,
        }],
        policy.write_quorum,
    );
    EnsureConfiguration {
        operation_id: OperationId::new(operation_id),
        previous_configuration: None,
        current_configuration,
        previous_epoch: None,
        current_epoch: epoch,
        effective_policy: policy,
        local_replica_id: local.replica_id,
        expected_instance_id: local.instance_id,
        expected_agent_generation: local.agent_generation,
        transition_kind: TransitionKind::Bootstrap,
    }
}

fn store() -> (tempfile::TempDir, Arc<SqliteStore>) {
    let directory = tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(directory.path());
    let store = SqliteStore::create_authorized(path, AgentState::new(storage_identity())).unwrap();
    (directory, Arc::new(store))
}

#[tokio::test]
async fn coordinator_converges_duplicates_and_retains_terminal_result() {
    let (_directory, store) = store();
    let runtime = Arc::new(FakeRuntime::new());
    let coordinator = Coordinator::new(store.clone(), runtime.clone());
    let command = command("configuration-1", Epoch::new(0, 1));

    let first = coordinator
        .ensure_configuration(command.clone())
        .await
        .unwrap();
    let calls = runtime.calls.lock().unwrap().clone();
    let duplicate = coordinator.ensure_configuration(command).await.unwrap();
    assert_eq!(duplicate, first);
    assert_eq!(*runtime.calls.lock().unwrap(), calls);
    let state = store.load_state().await.unwrap();
    assert!(state.reconfiguration.is_none());
    assert_eq!(state.read_status, AccessStatus::Granted);
    assert_eq!(state.write_status, AccessStatus::Granted);
}

#[tokio::test]
async fn coordinator_rejects_operation_reuse_and_stale_authority() {
    let (_directory, store) = store();
    let runtime = Arc::new(FakeRuntime::new());
    let coordinator = Coordinator::new(store.clone(), runtime);
    coordinator
        .ensure_configuration(command("same-operation", Epoch::new(0, 2)))
        .await
        .unwrap();

    assert!(matches!(
        coordinator
            .ensure_configuration(command("same-operation", Epoch::new(0, 3)))
            .await,
        Err(AgentError::EffectConflict(_))
    ));
    assert!(matches!(
        coordinator
            .ensure_configuration(command("stale-operation", Epoch::new(0, 1)))
            .await,
        Err(AgentError::CommandRejected(_))
    ));
}

#[tokio::test]
async fn coordinator_resumes_from_durable_stage_without_repeating_completed_effects() {
    let (directory, store) = store();
    let runtime = Arc::new(FakeRuntime::new());
    runtime.fail_once("epoch");
    let command = command("restart-operation", Epoch::new(0, 1));
    let coordinator = Coordinator::new(store.clone(), runtime.clone());
    assert!(
        coordinator
            .ensure_configuration(command.clone())
            .await
            .is_err()
    );
    let calls_before = runtime.calls.lock().unwrap().clone();
    assert_eq!(
        calls_before,
        [
            "admit",
            "read",
            "get-lsn",
            "write",
            "replicator-role",
            "epoch"
        ]
    );
    drop(coordinator);
    drop(store);

    let reopened = Arc::new(
        SqliteStore::open_existing(
            SqliteStore::metadata_database_path(directory.path()),
            Some(&storage_identity()),
        )
        .unwrap(),
    );
    let resumed = Coordinator::new(reopened, runtime.clone());
    resumed.ensure_configuration(command).await.unwrap();
    assert_eq!(
        runtime.calls.lock().unwrap().as_slice(),
        [
            "admit",
            "read",
            "get-lsn",
            "write",
            "replicator-role",
            "epoch",
            "epoch",
            "application-role",
            "catchup",
            "access",
        ]
    );
}

#[tokio::test]
async fn coordinator_observes_completed_effect_before_stage_advance() {
    let (_directory, store) = store();
    let runtime = Arc::new(FakeRuntime::new());
    let command = command("lost-stage-advance", Epoch::new(0, 1));
    store.begin_configuration(&command).await.unwrap();
    let authority = admit_configuration(&command, &store.load_state().await.unwrap()).unwrap();
    RuntimeAdapter::new(store.clone(), runtime.clone())
        .execute(RuntimeEffect {
            operation_id: OperationId::new("lost-stage-advance:admit-authority"),
            sequence: 1,
            action: RuntimeEffectAction::AdmitAuthority(Box::new(authority)),
        })
        .await
        .unwrap();

    Coordinator::new(store, runtime.clone())
        .ensure_configuration(command)
        .await
        .unwrap();
    assert_eq!(
        runtime
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|stage| **stage == "admit")
            .count(),
        1
    );
}

#[test]
fn same_epoch_new_operation_cannot_replace_durable_membership() {
    let local = identity();
    let policy = EffectivePolicy::fixed(3, 30).unwrap();
    let member = |id: i64, instance: &str, role: ReplicaRole| ConfigurationMember {
        identity: ReplicaIdentity {
            replica_id: ReplicaId::new(id),
            instance_id: ReplicaInstanceId::new(instance),
            agent_generation: AgentGeneration::new(format!("generation-{instance}")),
        },
        role,
    };
    let existing = ConfigurationDescriptor::new(
        Epoch::new(0, 5),
        local.replica_id,
        vec![
            ConfigurationMember {
                identity: local.clone(),
                role: ReplicaRole::Primary,
            },
            member(2, "secondary", ReplicaRole::ActiveSecondary),
            member(3, "old", ReplicaRole::ActiveSecondary),
        ],
        policy.write_quorum,
    );
    let conflicting = ConfigurationDescriptor::new(
        existing.epoch,
        local.replica_id,
        vec![
            ConfigurationMember {
                identity: local.clone(),
                role: ReplicaRole::Primary,
            },
            member(2, "secondary", ReplicaRole::ActiveSecondary),
            member(3, "replacement", ReplicaRole::ActiveSecondary),
        ],
        policy.write_quorum,
    );
    let mut state = AgentState::new(StorageIdentity {
        effective_policy: policy.clone(),
        ..storage_identity()
    });
    state.highest_epoch = existing.epoch;
    state.current_configuration = Some(existing);
    let command = EnsureConfiguration {
        operation_id: OperationId::new("conflicting-same-epoch"),
        previous_configuration: None,
        current_configuration: conflicting,
        previous_epoch: None,
        current_epoch: Epoch::new(0, 5),
        effective_policy: policy,
        local_replica_id: local.replica_id,
        expected_instance_id: local.instance_id,
        expected_agent_generation: local.agent_generation,
        transition_kind: TransitionKind::Bootstrap,
    };
    assert!(matches!(
        admit_configuration(&command, &state),
        Err(AgentError::CommandRejected(_))
    ));
}

#[tokio::test]
async fn concurrent_matching_commands_coalesce_without_panicking() {
    let (_directory, store) = store();
    let runtime = Arc::new(FakeRuntime::new());
    let coordinator = Arc::new(Coordinator::new(store, runtime.clone()));
    let command = command("concurrent-command", Epoch::new(0, 1));
    let first_coordinator = coordinator.clone();
    let first_command = command.clone();
    let first =
        tokio::spawn(async move { first_coordinator.ensure_configuration(first_command).await });
    let second_coordinator = coordinator.clone();
    let second =
        tokio::spawn(async move { second_coordinator.ensure_configuration(command).await });
    assert_eq!(
        first.await.unwrap().unwrap(),
        second.await.unwrap().unwrap()
    );
    assert_eq!(
        runtime
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|stage| **stage == "admit")
            .count(),
        1
    );
}
