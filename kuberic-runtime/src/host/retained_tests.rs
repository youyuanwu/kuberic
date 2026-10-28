use super::*;
use crate::host::state::{PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION, StorageIdentity};

fn fixture() -> (AgentState, ReplicaGoal, PublicOperationIntent) {
    let resource = ResourceUid::new("retained-host-unit");
    let identity = |id| ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
        agent_generation: AgentGeneration::new(format!("generation-{id}")),
    };
    let local = identity(2);
    let storage = ReplicaStorageIncarnationContext {
        replica: local.clone(),
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new("pod-2"),
            pvc_uid: PvcUid::new("pvc-2"),
        },
        storage_generation: StorageGeneration::new("storage-2"),
    };
    let initialization =
        derive_history_initialization_id(&resource, PublicProtocolGeneration::new(1));
    let history =
        derive_bootstrap_history(&resource, PublicProtocolGeneration::new(1), &initialization);
    let configuration = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        ReplicaId::new(1),
        vec![
            ConfigurationMember {
                identity: identity(1),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: local.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    let old = ReplicaGoal {
        format: ORDINARY_ACTION_FORMAT,
        identity: PublicV1Identity::new(1),
        operation_id: OperationId::new("admitted-secondary"),
        revision: 10,
        scope: ReplicaScope {
            history: history.clone(),
            storage: storage.clone(),
            process_session_id: ProcessSessionId::new("old-process"),
            creation_id: ReplicatorCreationId::new("old-creation"),
            epoch: configuration.epoch,
            configuration: ReplicaConfigurationScope {
                current: configuration.clone(),
                previous: None,
                replicas: vec![ReplicaPeerDescription {
                    identity: identity(1),
                    replication_address: "http://127.0.0.1:31001".into(),
                    process_session_id: ProcessSessionId::new("primary-process"),
                    storage_incarnation: ReplicaStorageIncarnationContext {
                        replica: identity(1),
                        storage: ReplicaStorageIdentity::Persistent {
                            pod_uid: PodUid::new("pod-1"),
                            pvc_uid: PvcUid::new("pvc-1"),
                        },
                        storage_generation: StorageGeneration::new("storage-1"),
                    },
                    role: ReplicaRole::Primary,
                    current_progress: INVALID_LSN,
                    catch_up_capability: INVALID_LSN,
                    is_up: false,
                    is_ready: false,
                    must_catchup: false,
                }],
            },
        },
        desired: ReplicaGoalKind::Prepare {
            role: ReplicaRole::ActiveSecondary,
            rebuild: None,
        },
    };
    let mut state = AgentState::new(StorageIdentity {
        schema_version: PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION,
        resource_uid: resource.clone(),
        pod_uid: PodUid::new("pod-2"),
        pvc_uid: PvcUid::new("pvc-2"),
        initialization_id: InitializationId::new("init"),
        local_identity: local,
        effective_policy: EffectivePolicy::fixed(2, 30).unwrap(),
    });
    let mut preview = PublicV1OperationState::new_bound(PublicV1LifecycleBinding {
        identity: old.identity.clone(),
        resource_uid: resource,
        spec_generation: 1,
        state_persistence: StatePersistence::Persisted,
    });
    preview.history = Some(history.clone());
    preview.history_initialization_id = Some(initialization);
    preview.storage_incarnation = Some(storage.clone());
    let actions = super::super::super::public_lifecycle::operation_instructions(&old.intent());
    let outcomes = actions
        .iter()
        .map(|instruction| match instruction {
            PublicInstruction::ReplicatorOpen => PublicInstructionOutcome::BuiltinNativeEndpoint {
                endpoint: "http://127.0.0.1:31002".into(),
                process_id: std::process::id(),
                process_lifetime: process_lifetime_id().to_owned(),
                storage_owner: Some(old.scope.process_session_id.clone()),
            },
            PublicInstruction::ApplicationRole => PublicInstructionOutcome::ApplicationRole(None),
            _ => PublicInstructionOutcome::Done,
        })
        .collect();
    preview.operations.insert(
        old.operation_id.clone(),
        PublicOperationRecord {
            intent: old.intent(),
            ordinary: Some(OrdinaryGoalRecord {
                native_input: None,
                action_format: ORDINARY_ACTION_FORMAT,
                actions,
                open_existing: Some(true),
                already_open: false,
                accepted: true,
                logically_retired: false,
                host_settled: true,
                ready_admissions: Vec::new(),
                retained: None,
                reconstruction: None,
                creation_owner: Some(crate::host::state::OrdinaryCreationOwner {
                    process_id: std::process::id(),
                    process_lifetime: process_lifetime_id().to_owned(),
                    process_session_id: old.scope.process_session_id.clone(),
                    creation_id: old.scope.creation_id.clone(),
                    builtin_factory: None,
                }),
            }),
            attempt: 1,
            attempt_results: Default::default(),
            retry_failure: None,
            accepted_history: Some(history),
            accepted_storage: Some(storage),
            stage: PublicOperationStage::Completed,
            disposition: Some(PublicOperationDisposition::Succeeded),
            containment: PublicOperationContainment::Complete,
            superseded_by: None,
            blockers: Default::default(),
            lifecycle: PublicLifecycleRecord {
                in_flight: None,
                outcomes,
            },
        },
    );
    state.public_v1_operation_state = Some(preview);
    let qualification = RetainedMemberQualification {
        predecessor_process_session_id: old.scope.process_session_id.clone(),
        predecessor_creation_id: old.scope.creation_id.clone(),
        admitted_operation_id: old.operation_id.clone(),
        admitted_revision: old.revision,
        configuration,
    };
    let fault = PublicOperationIntent {
        identity: old.identity.clone(),
        operation_id: OperationId::new("hidden-fault"),
        revision: 11,
        process_session_id: old.scope.process_session_id.clone(),
        class: PublicOperationClass::TransientFault,
        input_digest: CanonicalOperationInputDigest::opaque_v1("retained-test-fault", b"fault"),
        replay_disposition: PublicOperationReplayDisposition::Containment,
        lifecycle: None,
        program: None,
    };
    let restore = ReplicaGoal {
        operation_id: OperationId::new("restore-member"),
        revision: 12,
        scope: ReplicaScope {
            process_session_id: ProcessSessionId::new("new-process"),
            creation_id: ReplicatorCreationId::new("new-creation"),
            ..old.scope
        },
        desired: ReplicaGoalKind::RestoreRetainedMember {
            qualification: Box::new(qualification),
        },
        ..old
    };
    (state, restore, fault)
}

fn contained_fixture() -> (AgentState, ReplicaGoal) {
    let (mut state, mut goal, _) = fixture();
    state.application_storage = Some(ApplicationStorageBinding {
        paths: [(
            "application".into(),
            std::path::PathBuf::from("retained-validator-only"),
        )]
        .into_iter()
        .collect(),
        initializing: false,
    });
    let preview = state.public_v1_operation_state.as_mut().unwrap();
    let old = preview.operations.values_mut().next().unwrap();
    old.lifecycle.outcomes.truncate(4);
    old.lifecycle.in_flight = Some(PublicInstruction::ProgramEpoch);
    old.stage = PublicOperationStage::ContainmentPending;
    old.containment = PublicOperationContainment::Pending;
    old.disposition = Some(PublicOperationDisposition::Ambiguous(
        "old native effect has no receipt".into(),
    ));
    goal.operation_id = OperationId::new("recover-contained");
    goal.desired = ReplicaGoalKind::RecoverContainedCreation {
        recovery: Box::new(ContainedCreationRecovery {
            addition: None,
            predecessor_process_session_id: old.intent.process_session_id.clone(),
            predecessor_creation_id: old.intent.goal().unwrap().scope.creation_id.clone(),
            predecessor_operations: vec![old.intent.operation_id.clone()],
            receiver_settlements: Vec::new(),
            role: ReplicaRole::ActiveSecondary,
            catch_up: None,
        }),
    };
    (state, goal)
}

#[test]
fn m3_contained_reconstruction_is_distinct_from_narrow_retained_restoration() {
    let (state, goal) = contained_fixture();
    let before = state.clone();
    super::super::reconstruction::validate(&state, &goal).unwrap();
    let frozen = super::super::freeze(&state, &goal).unwrap();
    assert_eq!(frozen.open_existing, Some(true));
    assert!(frozen.reconstruction.is_none());
    assert!(frozen.retained.is_none());
    assert_eq!(
        frozen.actions,
        [
            PublicInstruction::ApplicationOpen,
            PublicInstruction::ReplicatorOpen,
            PublicInstruction::ReplicatorRole,
            PublicInstruction::ApplicationRole,
            PublicInstruction::ProgramEpoch,
            PublicInstruction::Configuration,
            PublicInstruction::NativeComplete,
        ]
    );
    let (_, mut restore, _) = fixture();
    restore.scope = goal.scope.clone();
    assert!(validate_restore(&state, &restore).is_err());
    assert_eq!(
        state, before,
        "qualification cannot settle the old effect or mint a receipt"
    );
    assert!(
        super::super::publication(
            &state,
            &goal.scope.process_session_id,
            &goal.scope.creation_id,
            false,
        )
        .is_none()
    );
}

#[test]
fn m3_contained_reconstruction_rejects_crossed_missing_and_foreign_dependencies() {
    let (state, goal) = contained_fixture();
    for changed in 0..7 {
        let mut state = state.clone();
        let mut goal = goal.clone();
        match changed {
            0 => {
                let ReplicaGoalKind::RecoverContainedCreation { recovery } = &mut goal.desired
                else {
                    unreachable!()
                };
                recovery
                    .predecessor_operations
                    .push(OperationId::new("unobserved"));
            }
            1 => goal.scope.epoch.configuration_number += 1,
            2 => {
                state.reported_fault = Some(FaultType::Permanent);
            }
            3 => state.application_storage = None,
            4 => {
                let old = state
                    .public_v1_operation_state
                    .as_mut()
                    .unwrap()
                    .operations
                    .values_mut()
                    .next()
                    .unwrap();
                old.lifecycle.outcomes[1] = PublicInstructionOutcome::Endpoint("unproven".into());
            }
            5 => {
                let mut unrelated = state
                    .public_v1_operation_state
                    .as_ref()
                    .unwrap()
                    .operations
                    .values()
                    .next()
                    .unwrap()
                    .clone();
                unrelated.intent.operation_id = OperationId::new("other-live-owner");
                unrelated.intent.process_session_id = ProcessSessionId::new("other-process");
                state
                    .public_v1_operation_state
                    .as_mut()
                    .unwrap()
                    .operations
                    .insert(unrelated.intent.operation_id.clone(), unrelated);
            }
            _ => goal.scope.configuration.replicas[0].is_ready = true,
        }
        assert!(
            super::super::reconstruction::validate(&state, &goal).is_err(),
            "crossed reconstruction {changed} must fail closed"
        );
    }
}

#[test]
fn m3_contained_reconstruction_retains_frozen_pc_and_highest_epoch_without_sampling() {
    let (mut state, mut goal) = contained_fixture();
    let previous = goal.scope.configuration.current.clone();
    goal.scope.configuration.previous = Some(previous);
    let old = state
        .public_v1_operation_state
        .as_mut()
        .unwrap()
        .operations
        .values_mut()
        .next()
        .unwrap();
    let mut selected = old.intent.goal().unwrap().clone();
    selected.scope.configuration = goal.scope.configuration.clone();
    old.intent = selected.intent();
    super::super::reconstruction::validate(&state, &goal).unwrap();
    let mut dropped_pc = goal.clone();
    dropped_pc.scope.configuration.previous = None;
    assert!(super::super::reconstruction::validate(&state, &dropped_pc).is_err());
    let mut lowered = goal;
    lowered.scope.epoch.configuration_number -= 1;
    assert!(super::super::reconstruction::validate(&state, &lowered).is_err());
}

#[test]
fn m3_contained_claim_wire_cannot_carry_death_ready_or_progress_authority() {
    let (_, goal) = contained_fixture();
    let ReplicaGoalKind::RecoverContainedCreation { recovery } = &goal.desired else {
        unreachable!()
    };
    for extra in [
        "dead",
        "ready",
        "currentProgress",
        "catchUpCapability",
        "quorumGrant",
    ] {
        let mut encoded = serde_json::to_value(recovery).unwrap();
        encoded[extra] = serde_json::json!(true);
        assert!(serde_json::from_value::<ContainedCreationRecovery>(encoded).is_err());
    }
    for format in [16, 17, 18, 19] {
        let mut old = goal.clone();
        old.format = format;
        assert!(old.validate().is_err());
        old = goal.clone();
        old.identity.protocol_version = format;
        assert!(old.validate().is_err());
    }
}

#[test]
fn m3_contained_receiver_settlement_requires_exact_independent_successful_join() {
    let (state, mut terminal) = contained_fixture();
    terminal.desired = ReplicaGoalKind::Close;
    let claim = ContainedReceiverSettlement {
        source_operation_id: OperationId::new("source-build-is-not-a-receiver-join"),
        terminal_goal: Box::new(terminal.clone()),
    };
    let mut snapshot = super::super::snapshot(
        &state,
        terminal.scope.process_session_id.clone(),
        terminal.scope.creation_id.clone(),
        true,
    );
    snapshot.goals = vec![ReplicaGoalObservation {
        goal: terminal,
        status: ReplicaGoalStatus::Achieved,
        selected_call: None,
        receipts: 2,
        host_settled: true,
        completed_handoff: None,
    }];
    assert!(super::super::reconstruction::receiver_settled(
        &claim, &snapshot
    ));
    for changed in 0..7 {
        let mut current = snapshot.clone();
        match changed {
            0 => current.goals[0].host_settled = false,
            1 => current.goals[0].status = ReplicaGoalStatus::Uncertain("lost root".into()),
            2 => current.provider_writer_held = true,
            3 => current.withdrawn = false,
            4 => current.process_session_id = ProcessSessionId::new("successor-not-old-receiver"),
            5 => current.goals[0].selected_call = Some("Cleanup".into()),
            _ => current.goals[0].goal.operation_id = OperationId::new("source-remove"),
        }
        assert!(
            !super::super::reconstruction::receiver_settled(&claim, &current),
            "receiver join cannot be inferred from observation {changed}"
        );
    }
    let mut successor = snapshot.clone();
    successor.process_session_id = ProcessSessionId::new("unopened-successor");
    successor.creation_id = ReplicatorCreationId::new("unopened-successor-creation");
    successor.withdrawn = false;
    assert!(super::super::reconstruction::receiver_settled(
        &claim, &successor
    ));
    for changed in 0..8 {
        let mut current = successor.clone();
        match changed {
            0 => current.goals[0].host_settled = false,
            1 => current.goals[0].status = ReplicaGoalStatus::Failed("actual Close error".into()),
            2 => current.provider_writer_held = true,
            3 => current.read_status = AccessStatus::Granted,
            4 => current.write_status = AccessStatus::Granted,
            5 => current.creation_id = snapshot.creation_id.clone(),
            6 => current.goals[0].selected_call = Some("Cleanup".into()),
            _ => {
                let mut live = current.goals[0].clone();
                live.goal.scope.process_session_id = current.process_session_id.clone();
                live.goal.scope.creation_id = current.creation_id.clone();
                current.goals.push(live);
            }
        }
        assert!(
            !super::super::reconstruction::receiver_settled(&claim, &current),
            "an opened/unsettled successor cannot settle its predecessor: {changed}"
        );
    }
}

#[test]
fn m3_contained_dependency_relation_requires_exact_new_success_without_rewriting_old_failure() {
    let (mut state, goal) = contained_fixture();
    let mut ordinary = super::super::freeze(&state, &goal).unwrap();
    ordinary.reconstruction = Some(CreationReconstructionReceipt {
        predecessor_process_id: std::process::id(),
        predecessor_process_lifetime: process_lifetime_id().into(),
        owner_process_id: std::process::id(),
        owner_process_lifetime: process_lifetime_id().into(),
        exclusion: CreationExclusion::JoinedHost,
    });
    let preview = state.public_v1_operation_state.as_mut().unwrap();
    let old_id = preview.operations.keys().next().unwrap().clone();
    let original = preview.operations[&old_id].clone();
    // A journal-validator vector only; actual host exclusion/native callbacks
    // are independently required by the store and production constructor.
    let mut record = PublicOperationRecord {
        intent: goal.intent(),
        ordinary: Some(ordinary),
        attempt: 1,
        attempt_results: Default::default(),
        retry_failure: None,
        accepted_history: Some(goal.scope.history.clone()),
        accepted_storage: Some(goal.scope.storage.clone()),
        stage: PublicOperationStage::Running,
        disposition: None,
        containment: PublicOperationContainment::NotRequired,
        superseded_by: None,
        blockers: Default::default(),
        lifecycle: PublicLifecycleRecord {
            in_flight: None,
            outcomes: vec![
                PublicInstructionOutcome::Done,
                PublicInstructionOutcome::BuiltinNativeEndpoint {
                    endpoint: "http://127.0.0.1:31002".into(),
                    process_id: std::process::id(),
                    process_lifetime: process_lifetime_id().into(),
                    storage_owner: Some(goal.scope.process_session_id.clone()),
                },
            ],
        },
    };
    super::super::reconstruction::validate_receipt(&state, &record).unwrap();
    record
        .lifecycle
        .outcomes
        .push(PublicInstructionOutcome::Done);
    assert!(super::super::reconstruction::validate_receipt(&state, &record).is_err());
    record.lifecycle.outcomes[2] =
        PublicInstructionOutcome::ReconstructedNativeInitialized(Box::new(goal.scope.clone()));
    record.lifecycle.outcomes.extend([
        PublicInstructionOutcome::ApplicationRole(None),
        PublicInstructionOutcome::Done,
        PublicInstructionOutcome::Done,
        PublicInstructionOutcome::NativeUnavailable,
    ]);
    super::super::reconstruction::validate_receipt(&state, &record).unwrap();
    let preview = state.public_v1_operation_state.as_mut().unwrap();
    preview.operations.insert(goal.operation_id.clone(), record);
    preview.operations.get_mut(&old_id).unwrap().superseded_by = Some(goal.operation_id.clone());
    assert!(!super::super::reconstruction::dependency_replaced(
        preview,
        &preview.operations[&old_id],
        &goal.scope.process_session_id,
    ));
    let successor = preview.operations.get_mut(&goal.operation_id).unwrap();
    successor.stage = PublicOperationStage::Completed;
    successor.disposition = Some(PublicOperationDisposition::Succeeded);
    assert!(super::super::reconstruction::dependency_replaced(
        preview,
        &preview.operations[&old_id],
        &goal.scope.process_session_id,
    ));
    assert_eq!(preview.operations[&old_id].stage, original.stage);
    assert_eq!(
        preview.operations[&old_id].disposition,
        original.disposition
    );
    assert_eq!(preview.operations[&old_id].lifecycle, original.lifecycle);
    assert_eq!(
        preview.operations[&old_id].containment,
        original.containment
    );
}

fn faulted() -> (AgentState, ReplicaGoal, RetainedRecoveryExitNotice) {
    let (mut state, goal, intent) = fixture();
    let notice = recovery_cause(&state, &intent, std::process::id()).unwrap();
    let preview = state.public_v1_operation_state.as_mut().unwrap();
    preview.terminal = true;
    preview.writes_revoked = true;
    preview.operations.insert(
        intent.operation_id.clone(),
        PublicOperationRecord {
            intent,
            ordinary: None,
            attempt: 1,
            attempt_results: Default::default(),
            retry_failure: None,
            accepted_history: Some(notice.history.clone()),
            accepted_storage: Some(notice.storage.clone()),
            stage: PublicOperationStage::ContainmentPending,
            disposition: Some(PublicOperationDisposition::Failed(
                "actual cleanup failed".into(),
            )),
            containment: PublicOperationContainment::Pending,
            superseded_by: None,
            blockers: Default::default(),
            lifecycle: PublicLifecycleRecord {
                in_flight: None,
                outcomes: vec![PublicInstructionOutcome::RecoveryExitRequired(Box::new(
                    notice.clone(),
                ))],
            },
        },
    );
    state.reported_fault = Some(FaultType::Transient);
    (state, goal, notice)
}

#[test]
fn synthetic_higher_current_only_native_receipts_cannot_resolve_fault_without_original_round() {
    use crate::protocol::public_operations::{
        NativeDeactivationInfo, NativeLifecycleFacts, NativeReplicaState,
    };
    let (mut state, mut goal, notice) = faulted();
    goal.operation_id = OperationId::new("synthetic-unrelated-current-only");
    goal.revision += 1;
    goal.scope.epoch.configuration_number += 1;
    goal.scope.configuration.current.epoch = goal.scope.epoch;
    goal.scope.configuration.current.configuration_id =
        goal.scope.configuration.current.expected_id();
    goal.scope.configuration.previous = None;
    goal.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::ActiveSecondary,
        catch_up: None,
    };
    goal.validate().unwrap();
    let facts = NativeLifecycleFacts {
        operation_id: goal.operation_id.clone(),
        revision: goal.revision,
        scope: goal.scope.clone(),
        state: NativeReplicaState::Ready,
        native_role: ReplicaRole::ActiveSecondary,
        application_role: Some(ReplicaRole::ActiveSecondary),
        applied_primary_fence: Some(goal.scope.epoch),
        deactivation: Some(NativeDeactivationInfo {
            primary_fence: goal.scope.epoch,
            catch_up_lsn: 0,
        }),
    };
    let preview = state.public_v1_operation_state.as_mut().unwrap();
    let mut record = preview.operations[&notice.qualification.admitted_operation_id].clone();
    record.intent = goal.intent();
    record.superseded_by = None;
    let ordinary = record.ordinary.as_mut().unwrap();
    ordinary.actions = crate::host::public_lifecycle::operation_instructions(&record.intent);
    ordinary.accepted = true;
    ordinary.host_settled = true;
    record.lifecycle.in_flight = None;
    record.lifecycle.outcomes = ordinary
        .actions
        .iter()
        .map(|instruction| match instruction {
            PublicInstruction::NativeComplete => {
                PublicInstructionOutcome::NativeLifecycle(Box::new(facts.clone()))
            }
            PublicInstruction::ApplicationRole => PublicInstructionOutcome::ApplicationRole(None),
            _ => PublicInstructionOutcome::Done,
        })
        .collect();
    preview.operations.insert(goal.operation_id.clone(), record);
    preview
        .operations
        .get_mut(&notice.fault_operation_id)
        .unwrap()
        .superseded_by = Some(goal.operation_id.clone());
    let before = state.clone();
    assert!(!crate::host::original_member::successful_final(
        &state, &goal
    ));
    let preview = state.public_v1_operation_state.as_ref().unwrap();
    assert!(!crate::host::original_member::dependency_replaced(
        preview,
        &preview.operations[&notice.fault_operation_id],
        &goal.scope.process_session_id,
    ));
    assert_eq!(state, before);
}

#[test]
fn retained_historical_eligibility_is_not_physical_custody_or_new_native_authority() {
    let (state, goal, _) = fixture();
    let before = state.clone();
    let (pid, lifetime) = validate_restore(&state, &goal).unwrap();
    assert_eq!(pid, std::process::id());
    assert_eq!(lifetime, process_lifetime_id());
    let frozen = super::super::freeze(&state, &goal).unwrap();
    assert!(frozen.retained.is_none());
    assert_eq!(frozen.open_existing, Some(true));
    assert_eq!(
        frozen.actions,
        vec![
            PublicInstruction::ApplicationOpen,
            PublicInstruction::ReplicatorOpen,
            PublicInstruction::ReplicatorRole,
            PublicInstruction::ApplicationRole,
            PublicInstruction::ProgramEpoch,
            PublicInstruction::Configuration,
            PublicInstruction::NativeComplete,
        ]
    );
    assert!(
        super::super::publication(
            &state,
            &goal.scope.process_session_id,
            &goal.scope.creation_id,
            false
        )
        .is_none()
    );
    assert_eq!(state, before);
}

#[test]
fn retained_qualification_rejects_missing_builtin_unpaired_roles_and_unfinished_work() {
    let (state, goal, _) = fixture();
    for mutation in 0..7 {
        let mut changed = state.clone();
        let preview = changed.public_v1_operation_state.as_mut().unwrap();
        let record = preview
            .operations
            .get_mut(&OperationId::new("admitted-secondary"))
            .unwrap();
        match mutation {
            0 => {
                record.lifecycle.outcomes[1] =
                    PublicInstructionOutcome::Endpoint("independent".into())
            }
            1 => record.lifecycle.outcomes[3] = PublicInstructionOutcome::Done,
            2 => record.lifecycle.in_flight = Some(PublicInstruction::ProgramEpoch),
            3 => {
                record.disposition = Some(PublicOperationDisposition::Ambiguous("lost role".into()))
            }
            4 => record.ordinary.as_mut().unwrap().accepted = false,
            5 => {
                let mut old = record.intent.goal().unwrap().clone();
                old.scope.configuration.previous = Some(old.scope.configuration.current.clone());
                record.intent = old.intent();
            }
            _ => {
                if let PublicInstructionOutcome::BuiltinNativeEndpoint { storage_owner, .. } =
                    &mut record.lifecycle.outcomes[1]
                {
                    *storage_owner = None;
                }
            }
        }
        assert!(
            validate_restore(&changed, &goal).is_err(),
            "mutation {mutation}"
        );
    }
    let mut retired = state.clone();
    retired
        .public_v1_operation_state
        .as_mut()
        .unwrap()
        .retired_storage_incarnations
        .push(goal.scope.storage.clone());
    assert!(validate_restore(&retired, &goal).is_err());
    let mut live = goal.clone();
    if let ReplicaGoalKind::RestoreRetainedMember { qualification } = &live.desired {
        live.scope.process_session_id = qualification.predecessor_process_session_id.clone();
    }
    assert!(validate_restore(&state, &live).is_err());
    let mut crossed = goal.clone();
    crossed.scope.history.data_loss_number += 1;
    assert!(validate_restore(&state, &crossed).is_err());
}

#[test]
fn retained_recovery_notice_preserves_real_failure_and_does_not_settle_cleanup() {
    let (state, goal, notice) = faulted();
    let before = state.clone();
    validate_notice(&state, &notice).unwrap();
    validate_restore(&state, &goal).unwrap();
    assert_eq!(
        status(&state, &notice.qualification.predecessor_process_session_id),
        Some(notice.clone())
    );
    let fault = state
        .public_v1_operation_state
        .as_ref()
        .unwrap()
        .operations
        .get(&notice.fault_operation_id)
        .unwrap();
    assert!(supersedes_fault(&state, &goal, fault));
    assert_eq!(fault.stage, PublicOperationStage::ContainmentPending);
    assert_eq!(fault.containment, PublicOperationContainment::Pending);
    assert_eq!(
        fault.disposition,
        Some(PublicOperationDisposition::Failed(
            "actual cleanup failed".into()
        ))
    );
    assert!(super::super::terminal_for_process(
        state.public_v1_operation_state.as_ref().unwrap(),
        &goal.scope.process_session_id
    ));
    assert_eq!(state, before);
}

#[test]
fn retained_successful_fault_report_is_not_a_cleanup_or_live_reopen_certificate() {
    let (mut state, goal, notice) = faulted();
    let record = state
        .public_v1_operation_state
        .as_mut()
        .unwrap()
        .operations
        .get_mut(&notice.fault_operation_id)
        .unwrap();
    record.stage = PublicOperationStage::Completed;
    record.disposition = Some(PublicOperationDisposition::Succeeded);
    assert_eq!(record.containment, PublicOperationContainment::Pending);
    assert!(status(&state, &notice.qualification.predecessor_process_session_id).is_some());
    assert!(super::super::terminal_for_process(
        state.public_v1_operation_state.as_ref().unwrap(),
        &notice.qualification.predecessor_process_session_id,
    ));
    assert!(
        super::super::freeze(&state, &goal)
            .unwrap()
            .retained
            .is_none()
    );
}

#[test]
fn retained_notice_rejects_primary_explicit_close_opaque_fault_and_crossed_cause() {
    let (mut state, goal, notice) = faulted();
    let mut crossed = notice.clone();
    crossed.process_id = crossed.process_id.wrapping_add(1);
    assert!(validate_notice(&state, &crossed).is_err());
    let preview = state.public_v1_operation_state.as_mut().unwrap();
    preview
        .operations
        .get_mut(&notice.fault_operation_id)
        .unwrap()
        .lifecycle
        .outcomes
        .clear();
    assert!(validate_restore(&state, &goal).is_err());
    assert!(status(&state, &notice.qualification.predecessor_process_session_id).is_none());
    let (mut state, _, fault) = fixture();
    let record = state
        .public_v1_operation_state
        .as_mut()
        .unwrap()
        .operations
        .get_mut(&OperationId::new("admitted-secondary"))
        .unwrap();
    let mut old = record.intent.goal().unwrap().clone();
    old.desired = ReplicaGoalKind::Prepare {
        role: ReplicaRole::Primary,
        rebuild: None,
    };
    record.intent = old.intent();
    assert!(recovery_cause(&state, &fault, std::process::id()).is_none());
    let (mut state, goal, notice) = faulted();
    state
        .public_v1_operation_state
        .as_mut()
        .unwrap()
        .operations
        .get_mut(&notice.fault_operation_id)
        .unwrap()
        .intent
        .class = PublicOperationClass::Close;
    assert!(validate_restore(&state, &goal).is_err());
    assert!(status(&state, &notice.qualification.predecessor_process_session_id).is_none());
}

#[test]
fn retained_snapshot_surfaces_hidden_fault_revision_without_relabeling_it_as_a_goal() {
    let (mut state, mut goal, mut notice) = faulted();
    assert!(qualification_for_process(&state, &goal.scope.process_session_id).is_none());
    // This journal-validator fixture models a predecessor lifetime, not a
    // claimed physical exclusion. The composed process witnesses supply that.
    notice.process_lifetime = "historical-hidden-fault-owner".into();
    for outcome in state
        .public_v1_operation_state
        .as_mut()
        .unwrap()
        .operations
        .values_mut()
        .flat_map(|record| &mut record.lifecycle.outcomes)
    {
        match outcome {
            PublicInstructionOutcome::BuiltinNativeEndpoint {
                process_lifetime, ..
            } => {
                *process_lifetime = notice.process_lifetime.clone();
            }
            PublicInstructionOutcome::RecoveryExitRequired(stored) => {
                stored.process_lifetime = notice.process_lifetime.clone();
            }
            _ => {}
        }
    }
    let snapshot = super::super::snapshot(
        &state,
        goal.scope.process_session_id.clone(),
        goal.scope.creation_id.clone(),
        false,
    );
    assert_eq!(
        snapshot.accepted_revision_high_watermark,
        notice.fault_revision
    );
    assert_eq!(
        snapshot.retained_member_qualification,
        Some(notice.qualification.clone())
    );
    assert!(
        qualification_for_process(&state, &notice.qualification.predecessor_process_session_id)
            .is_none()
    );
    assert!(
        snapshot
            .goals
            .iter()
            .all(|goal| goal.goal.operation_id != notice.fault_operation_id)
    );
    goal.revision = notice.fault_revision;
    goal.desired = ReplicaGoalKind::Discover;
    goal.scope.configuration.replicas.clear();
    let error = super::super::validate_admission(&state, &goal)
        .unwrap_err()
        .to_string();
    assert!(error.contains("high-watermark"), "{error}");
}

#[test]
fn retained_goal_wire_is_final_matched23_and_carries_no_historical_progress_claim() {
    assert_eq!(
        ORDINARY_ACTION_FORMAT, 23,
        "requires the coordinated final matched format"
    );
    let (_, goal, _) = fixture();
    goal.validate().unwrap();
    let bytes = serde_json::to_vec(&goal).unwrap();
    assert_eq!(serde_json::from_slice::<ReplicaGoal>(&bytes).unwrap(), goal);
    let ReplicaGoalKind::RestoreRetainedMember { qualification } = &goal.desired else {
        unreachable!()
    };
    let mut json = serde_json::to_value(qualification).unwrap();
    assert!(json.get("currentProgress").is_none());
    assert!(json.get("catchUpCapability").is_none());
    json["currentProgress"] = serde_json::json!(37);
    assert!(serde_json::from_value::<RetainedMemberQualification>(json).is_err());
    for predecessor in [22, 21, 20, 19, 18, 17, 16] {
        let mut crossed = goal.clone();
        crossed.format = predecessor;
        assert!(crossed.validate().is_err());
    }
    let mut sampled = goal;
    sampled.scope.configuration.replicas[0].current_progress = 37;
    assert!(sampled.validate().is_err());
}

#[tokio::test]
async fn retained_store_refuses_same_physical_process_despite_new_session_and_real_locks() {
    use crate::host::sqlite_store::SqliteStore;
    use crate::host::store::AgentStore;
    assert_eq!(
        ORDINARY_ACTION_FORMAT, 23,
        "requires coordinated final matched format"
    );
    let (state, goal, _) = fixture();
    let root = crate::host::tests::tempdir().unwrap();
    let path = SqliteStore::metadata_database_path(root.path());
    let binding = state
        .public_v1_operation_state
        .as_ref()
        .unwrap()
        .binding
        .clone()
        .unwrap();
    let initial = SqliteStore::create_replacement_authorized(
        &path,
        AgentState::new(state.identity.clone()),
        binding.clone(),
    )
    .unwrap();
    drop(initial);
    // This is a journal-validator negative, not an executed historical native
    // registration or an invented physical-exclusion certificate.
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE agent_state SET state_json = ?1 WHERE singleton = 1",
            [serde_json::to_string(&state).unwrap()],
        )
        .unwrap();
    drop(connection);
    let observed = crate::host::provisioning::ObservedStorageIdentity {
        resource_uid: state.identity.resource_uid.clone(),
        pod_uid: state.identity.pod_uid.clone(),
        pvc_uid: state.identity.pvc_uid.clone(),
        instance_id: state.identity.local_identity.instance_id.clone(),
    };
    let owner = SqliteStore::open_replacement_for_process(
        &path,
        &observed,
        state.identity.local_identity.replica_id,
        None,
        &binding,
    )
    .unwrap();
    owner
        .bind_process_owner(&goal.scope.process_session_id)
        .unwrap();
    let error = owner
        .begin_public_operation(&goal.intent(), &[], &[])
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("different real process"), "{error}");
    assert_eq!(owner.load_state().await.unwrap(), state);
    owner.release_process_storage();
}

#[test]
fn retained_selection_does_not_redirect_clean_same_process_reopen_into_physical_reconstruction() {
    let (mut state, goal, _) = fixture();
    assert!(qualification_for_process(&state, &goal.scope.process_session_id).is_none());
    // Validator fixture only: this is the historical lifetime distinction,
    // not a claim that this unit test has excluded another physical owner.
    let preview = state.public_v1_operation_state.as_mut().unwrap();
    for outcome in preview
        .operations
        .values_mut()
        .flat_map(|record| &mut record.lifecycle.outcomes)
    {
        if let PublicInstructionOutcome::BuiltinNativeEndpoint {
            process_lifetime, ..
        } = outcome
        {
            *process_lifetime = "historical-other-os-lifetime".into();
        }
    }
    assert!(qualification_for_process(&state, &goal.scope.process_session_id).is_some());
}

#[test]
fn retained_election_attempt_requires_accepted_paired_original_member_and_exact_storage() {
    let (mut state, _, _) = fixture();
    let record = state
        .public_v1_operation_state
        .as_mut()
        .unwrap()
        .operations
        .get_mut(&OperationId::new("admitted-secondary"))
        .unwrap();
    let mut old = record.intent.goal().unwrap().clone();
    let mut peer = old.scope.configuration.replicas[0].clone();
    peer.identity = ReplicaIdentity {
        replica_id: ReplicaId::new(3),
        instance_id: ReplicaInstanceId::new("pod-3"),
        agent_generation: AgentGeneration::new("generation-3"),
    };
    peer.storage_incarnation.replica = peer.identity.clone();
    peer.storage_incarnation.storage = ReplicaStorageIdentity::Persistent {
        pod_uid: PodUid::new("pod-3"),
        pvc_uid: PvcUid::new("pvc-3"),
    };
    peer.storage_incarnation.storage_generation = StorageGeneration::new("storage-3");
    peer.role = ReplicaRole::ActiveSecondary;
    peer.process_session_id = ProcessSessionId::new("old-member-3");
    peer.replication_address = "http://127.0.0.1:31003".into();
    let previous = ConfigurationDescriptor::new(
        old.scope.epoch,
        ReplicaId::new(1),
        old.scope
            .configuration
            .current
            .members
            .iter()
            .cloned()
            .chain(std::iter::once(ConfigurationMember {
                identity: peer.identity.clone(),
                role: peer.role,
            }))
            .collect(),
        2,
    );
    old.scope.configuration.current = previous.clone();
    old.scope.configuration.replicas.push(peer.clone());
    record.intent = old.intent();
    let mut goal = old.clone();
    goal.operation_id = OperationId::new("survivor-election");
    goal.revision += 1;
    goal.scope.epoch = Epoch::new(0, 2);
    goal.scope.configuration.previous = Some(previous);
    goal.scope.configuration.current = ConfigurationDescriptor::new(
        goal.scope.epoch,
        ReplicaId::new(2),
        vec![
            ConfigurationMember {
                identity: old.scope.storage.replica.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: peer.identity.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    goal.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::Primary,
        catch_up: Some(PublicCatchUpMode::All),
    };
    peer.process_session_id = ProcessSessionId::new("fresh-member-3");
    goal.scope.configuration.replicas[1] = peer.clone();
    let permits = |state: &AgentState, goal: &ReplicaGoal, peer: &ReplicaPeerDescription| {
        super::super::retained_election_member(state, goal, peer)
    };
    assert!(permits(&state, &goal, &peer));
    let mut changed = peer.clone();
    changed.storage_incarnation.storage_generation = StorageGeneration::new("new-storage");
    assert!(!permits(&state, &goal, &changed));
    let mut changed = goal.clone();
    changed.scope.configuration.current.write_quorum = 1;
    assert!(!permits(&state, &changed, &peer));
    let mut changed = goal.clone();
    changed.scope.configuration.current.epoch = old.scope.epoch;
    assert!(!permits(&state, &changed, &peer));
    let mut changed = goal.clone();
    changed.scope.configuration.previous = None;
    assert!(!permits(&state, &changed, &peer));
    let mut changed = goal.clone();
    changed.scope.creation_id = ReplicatorCreationId::new("unproven-source-creation");
    assert!(!permits(&state, &changed, &peer));
    let mut unaccepted = state.clone();
    unaccepted
        .public_v1_operation_state
        .as_mut()
        .unwrap()
        .operations
        .values_mut()
        .next()
        .unwrap()
        .ordinary
        .as_mut()
        .unwrap()
        .accepted = false;
    assert!(!permits(&unaccepted, &goal, &peer));
    let mut unpaired = state.clone();
    for outcome in &mut unpaired
        .public_v1_operation_state
        .as_mut()
        .unwrap()
        .operations
        .values_mut()
        .next()
        .unwrap()
        .lifecycle
        .outcomes
    {
        if matches!(outcome, PublicInstructionOutcome::ApplicationRole(_)) {
            *outcome = PublicInstructionOutcome::Done;
        }
    }
    assert!(!permits(&unpaired, &goal, &peer));
}
