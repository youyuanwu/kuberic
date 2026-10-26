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
                action_format: ORDINARY_ACTION_FORMAT,
                actions,
                open_existing: Some(true),
                already_open: false,
                accepted: true,
                logically_retired: false,
                host_settled: true,
                ready_admissions: Vec::new(),
                retained: None,
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
fn retained_goal_wire_is_final_matched19_and_carries_no_historical_progress_claim() {
    assert_eq!(
        ORDINARY_ACTION_FORMAT, 19,
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
    let mut crossed = goal.clone();
    crossed.format = 18;
    assert!(crossed.validate().is_err());
    let mut sampled = goal;
    sampled.scope.configuration.replicas[0].current_progress = 37;
    assert!(sampled.validate().is_err());
}

#[tokio::test]
async fn retained_store_refuses_same_physical_process_despite_new_session_and_real_locks() {
    use crate::host::sqlite_store::SqliteStore;
    use crate::host::store::AgentStore;
    assert_eq!(
        ORDINARY_ACTION_FORMAT, 19,
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
