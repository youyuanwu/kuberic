use kuberic_controller::cluster_api::PublicV1Scope;
use kuberic_controller::evaluator::testing::{
    PublicV1ElectionStatus, PublicV1HistoryApi, compare_public_v1_election_progress,
    compare_public_v1_election_reports, ensure_public_v1_bootstrap_history,
    execute_public_v1_history, invalidate_public_v1_for_data_loss,
    is_public_v1_provisional_primary, plan_public_v1_bootstrap_history,
    plan_public_v1_data_loss_recovery, plan_public_v1_recovery_report, public_v1_election_status,
};
use kuberic_runtime::protocol::command::ProtocolCommand;
use kuberic_runtime::protocol::public_operations::{
    DataLossAttempt, DataLossAuthorization, DataLossReplay, PossibleDataLossIntent,
    ProcessAttemptContext, ProviderProgressReset, PublicDataLossRecoveryInput,
    PublicLifecycleInput, PublicLifecycleRecipe, PublicOperationClass, PublicOperationIntent,
    PublicOperationProgram, PublicOperationReplayDisposition, PublicProgress, PublicProgressRole,
    PublicV1AcceptedStatus, PublicV1AgentReport, PublicV1CallKind, PublicV1Identity,
    PublicV1OperationCompletion, PublicV1OperationResult,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, AgentGeneration, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy,
    Epoch, InitializationId, PodUid, ProcessSessionId, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity, ReplicaStorageIncarnationContext,
    ReplicatorCreationId, StorageGeneration,
};
use kuberic_runtime::protocol::types::{
    OperationId, PublicProtocolGeneration, ResourceUid, derive_bootstrap_history,
    derive_data_loss_history, derive_history_initialization_id,
};
use kuberic_runtime::testing::sqlite_store::{
    PublicV1ReportProjection, SqliteStore as TestingSqliteStore,
};
use kuberic_runtime::testing::state::{SCHEMA_VERSION, StorageIdentity as TestingStorageIdentity};
use std::sync::{Arc, Mutex};

fn storage() -> ReplicaStorageIncarnationContext {
    ReplicaStorageIncarnationContext {
        replica: ReplicaIdentity {
            replica_id: ReplicaId::new(1),
            instance_id: ReplicaInstanceId::new("pod-1"),
            agent_generation: AgentGeneration::new("generation-1"),
        },
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new("pod-1"),
            pvc_uid: PvcUid::new("pvc-1"),
        },
        storage_generation: StorageGeneration::new("storage-1"),
    }
}

#[test]
fn public_v1_history_bootstrap_decision_is_partition_wide_and_stable() {
    let resource_uid = ResourceUid::new("resource-1");
    let identity = PublicV1Identity::new(42);
    let accepted = ensure_public_v1_bootstrap_history(None, &resource_uid, &identity).unwrap();
    let retried =
        ensure_public_v1_bootstrap_history(Some(accepted.clone()), &resource_uid, &identity)
            .unwrap();
    assert_eq!(accepted, retried);

    let changed = PublicV1Identity::new(43);
    assert!(ensure_public_v1_bootstrap_history(Some(accepted), &resource_uid, &changed).is_err());
}

#[test]
fn public_v1_history_election_rejects_incomparable_numeric_progress() {
    let resource_uid = ResourceUid::new("resource-1");
    let generation = PublicProtocolGeneration::new(42);
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    let first = derive_bootstrap_history(&resource_uid, generation, &initialization);
    let second =
        derive_data_loss_history(&OperationId::new("data-loss"), &first, 1, &storage()).unwrap();

    assert_eq!(
        compare_public_v1_election_progress(&first, 10, &first, 20),
        Some(std::cmp::Ordering::Less)
    );
    assert_eq!(
        compare_public_v1_election_progress(&first, 1000, &second, 1),
        None
    );
}

#[test]
fn public_v1_data_loss_invalidation_is_owner_local_and_exact() {
    let resource_uid = ResourceUid::new("resource-1");
    let identity = PublicV1Identity::new(42);
    let mut accepted = ensure_public_v1_bootstrap_history(None, &resource_uid, &identity).unwrap();
    let old_program = PublicOperationProgram::Abort;
    accepted.completed_operations.push(
        kuberic_runtime::protocol::public_operations::PublicV1AcceptedOperationCompletion::
            from_completion(&PublicV1OperationCompletion {
            operation_id: OperationId::new("old-completion"),
            revision: 1,
            call_kind: PublicV1CallKind::Abort,
            process_session_id: ProcessSessionId::new("session-1"),
            input_digest: old_program.digest(),
            replay_disposition: old_program.replay_disposition(),
            history: accepted.history.clone(),
            storage_incarnation: storage(),
            previous_configuration_id: None,
            current_configuration_id: None,
            catch_up_mode: None,
            catch_up_specific_quorum: None,
            result: PublicV1OperationResult::Succeeded,
        }),
    );
    let storage = storage();
    let report = PublicV1AgentReport {
        identity: identity.clone(),
        resource_uid: resource_uid.clone(),
        replica: storage.replica.clone(),
        process_session_id: ProcessSessionId::new("session-1"),
        replicator_creation_id: ReplicatorCreationId::new("creation-1"),
        report_sequence: 1,
        open_mode: kuberic_runtime::protocol::types::OpenMode::Existing,
        role: ReplicaRole::Primary,
        read_status: AccessStatus::Granted,
        write_status: AccessStatus::Granted,
        history: accepted.history.clone(),
        storage_incarnation: storage.clone(),
        progress: PublicProgress {
            role: PublicProgressRole::Primary,
            first: 10,
            last: 10,
        },
        replication_address: None,
        service_location: None,
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        extensions: Default::default(),
    };
    let authorization_id = OperationId::new("data-loss");
    let proposed =
        derive_data_loss_history(&authorization_id, &accepted.history, 1, &storage).unwrap();
    let authorization = DataLossAuthorization {
        authorization_id: authorization_id.clone(),
        predecessor: accepted.history.clone(),
        proposed,
        provisional_primary: storage,
        reset_scope: ProviderProgressReset::ResetToEmpty,
        replay_disposition: DataLossReplay::Convergent,
    };
    let attempt = DataLossAttempt {
        authorization_id,
        attempt_id: OperationId::new("attempt-1"),
        process: ProcessAttemptContext {
            process_session_id: report.process_session_id.clone(),
            replicator_creation_id: ReplicatorCreationId::new("creation-1"),
            catch_up_specific_quorum: true,
            data_loss_replay: DataLossReplay::Convergent,
            epoch: Epoch::new(1, 1),
            attempt_revision: 1,
        },
    };

    let invalidated =
        invalidate_public_v1_for_data_loss(&accepted, &report, &authorization, &attempt, 0)
            .unwrap();
    assert_eq!(invalidated.history, accepted.history);
    assert_eq!(
        invalidated.data_loss_recovery.as_ref().unwrap().attempt,
        attempt
    );
    assert!(invalidated.completed_operations.is_empty());

    let mut crossed = report.clone();
    crossed.process_session_id = ProcessSessionId::new("other-session");
    assert!(
        invalidate_public_v1_for_data_loss(&accepted, &crossed, &authorization, &attempt, 0)
            .is_err()
    );
    let mut stale_attempt = attempt;
    stale_attempt.process.epoch.configuration_number = 0;
    assert!(
        invalidate_public_v1_for_data_loss(&accepted, &report, &authorization, &stale_attempt, 0,)
            .is_err()
    );
}

#[test]
fn public_v1_recovery_progress_only_selects_access_closed_provisional_primary() {
    let resource_uid = ResourceUid::new("resource-1");
    let identity = PublicV1Identity::new(42);
    let accepted = ensure_public_v1_bootstrap_history(None, &resource_uid, &identity).unwrap();
    let storage = storage();
    let authorization_id = OperationId::new("data-loss");
    let proposed =
        derive_data_loss_history(&authorization_id, &accepted.history, 1, &storage).unwrap();
    let attempt = DataLossAttempt {
        authorization_id: authorization_id.clone(),
        attempt_id: OperationId::new("attempt-1"),
        process: ProcessAttemptContext {
            process_session_id: ProcessSessionId::new("session-1"),
            replicator_creation_id: ReplicatorCreationId::new("creation-1"),
            catch_up_specific_quorum: true,
            data_loss_replay: DataLossReplay::Convergent,
            epoch: Epoch::new(1, 1),
            attempt_revision: 1,
        },
    };
    let recovery = kuberic_runtime::protocol::public_operations::PublicDataLossRecoveryStatus {
        authorization_id,
        attempt,
        predecessor_history: accepted.history.clone(),
        proposed_history: proposed,
        provisional_primary: storage.clone(),
        reset_scope: ProviderProgressReset::ResetToEmpty,
        replay_disposition: DataLossReplay::Convergent,
        stage: kuberic_runtime::protocol::public_operations::PublicDataLossRecoveryStage::HistoryPending,
        observed_progress: Some(PublicProgress {
            role: PublicProgressRole::Recovery,
            first: 7,
            last: 7,
        }),
    };
    let mut report = PublicV1AgentReport {
        identity,
        resource_uid,
        replica: storage.replica.clone(),
        process_session_id: ProcessSessionId::new("session-1"),
        replicator_creation_id: ReplicatorCreationId::new("creation-1"),
        report_sequence: 2,
        open_mode: kuberic_runtime::protocol::types::OpenMode::Existing,
        role: ReplicaRole::Primary,
        read_status: AccessStatus::NotPrimary,
        write_status: AccessStatus::NotPrimary,
        history: accepted.history,
        storage_incarnation: storage,
        progress: PublicProgress {
            role: PublicProgressRole::Recovery,
            first: 7,
            last: 7,
        },
        replication_address: None,
        service_location: None,
        data_loss_recovery: Some(recovery),
        completed_operations: Vec::new(),
        extensions: Default::default(),
    };
    assert!(is_public_v1_provisional_primary(&report));
    assert_eq!(
        public_v1_election_status(&report, &[]),
        PublicV1ElectionStatus::ProvisionalPrimary
    );

    report.write_status = AccessStatus::Granted;
    assert!(!is_public_v1_provisional_primary(&report));
    assert_eq!(
        public_v1_election_status(&report, &[]),
        PublicV1ElectionStatus::Ineligible
    );
    report.write_status = AccessStatus::NotPrimary;
    report.role = ReplicaRole::ActiveSecondary;
    assert!(!is_public_v1_provisional_primary(&report));
    assert_eq!(
        public_v1_election_status(&report, &[]),
        PublicV1ElectionStatus::RecoveryPending
    );
    let admitted_storage = report.storage_incarnation.clone();
    report.progress.role = PublicProgressRole::Secondary;
    report
        .data_loss_recovery
        .as_mut()
        .unwrap()
        .stage = kuberic_runtime::protocol::public_operations::PublicDataLossRecoveryStage::ReplacementRequired;
    assert_eq!(
        public_v1_election_status(&report, std::slice::from_ref(&admitted_storage)),
        PublicV1ElectionStatus::Ineligible
    );
    report.data_loss_recovery.as_mut().unwrap().stage =
        kuberic_runtime::protocol::public_operations::PublicDataLossRecoveryStage::Retired;
    assert_eq!(
        public_v1_election_status(&report, std::slice::from_ref(&admitted_storage)),
        PublicV1ElectionStatus::Ineligible
    );
}

#[test]
fn public_v1_all_survivor_election_requires_history_and_storage_admission() {
    let resource_uid = ResourceUid::new("resource-1");
    let identity = PublicV1Identity::new(42);
    let accepted = ensure_public_v1_bootstrap_history(None, &resource_uid, &identity).unwrap();
    let first_storage = storage();
    let mut second_storage = first_storage.clone();
    second_storage.replica.replica_id = ReplicaId::new(2);
    second_storage.replica.instance_id = ReplicaInstanceId::new("pod-2");
    second_storage.replica.agent_generation = AgentGeneration::new("generation-2");
    second_storage.storage = ReplicaStorageIdentity::Persistent {
        pod_uid: PodUid::new("pod-2"),
        pvc_uid: PvcUid::new("pvc-2"),
    };
    second_storage.storage_generation = StorageGeneration::new("storage-2");
    let report = |storage: ReplicaStorageIncarnationContext, progress: i64| PublicV1AgentReport {
        identity: identity.clone(),
        resource_uid: resource_uid.clone(),
        replica: storage.replica.clone(),
        process_session_id: ProcessSessionId::new(format!(
            "session-{}",
            storage.replica.replica_id.value()
        )),
        replicator_creation_id: ReplicatorCreationId::new(format!(
            "creation-{}",
            storage.replica.replica_id.value()
        )),
        report_sequence: 1,
        open_mode: kuberic_runtime::protocol::types::OpenMode::Existing,
        role: ReplicaRole::ActiveSecondary,
        read_status: AccessStatus::NotPrimary,
        write_status: AccessStatus::NotPrimary,
        history: accepted.history.clone(),
        storage_incarnation: storage,
        progress: PublicProgress {
            role: PublicProgressRole::Secondary,
            first: progress,
            last: progress,
        },
        replication_address: None,
        service_location: None,
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        extensions: Default::default(),
    };
    let first = report(first_storage.clone(), 20);
    let second = report(second_storage.clone(), 1);
    assert_eq!(
        compare_public_v1_election_reports(
            &first,
            &second,
            &[first_storage.clone(), second_storage.clone()],
        ),
        Some(std::cmp::Ordering::Greater)
    );
    assert_eq!(
        compare_public_v1_election_reports(&first, &second, std::slice::from_ref(&first_storage)),
        None
    );

    let mut incompatible = second.clone();
    incompatible.history = derive_data_loss_history(
        &OperationId::new("other-history"),
        &accepted.history,
        1,
        &second_storage,
    )
    .unwrap();
    assert_eq!(
        compare_public_v1_election_reports(&first, &incompatible, &[first_storage, second_storage],),
        None
    );
}

struct HistoryApi {
    status: Mutex<Option<PublicV1AcceptedStatus>>,
    store: Arc<TestingSqliteStore>,
    dispatched: Mutex<Vec<ProtocolCommand>>,
}

#[async_trait::async_trait]
impl PublicV1HistoryApi for HistoryApi {
    async fn observe_public_v1_status(
        &self,
        _scope: &PublicV1Scope,
    ) -> Result<Option<PublicV1AcceptedStatus>, String> {
        Ok(self.status.lock().unwrap().clone())
    }

    async fn persist_public_v1_status(
        &self,
        _scope: &PublicV1Scope,
        expected: Option<&PublicV1AcceptedStatus>,
        next: &PublicV1AcceptedStatus,
    ) -> Result<(), String> {
        let mut status = self.status.lock().unwrap();
        if status.as_ref() != expected {
            return Err("stale status replacement".into());
        }
        *status = Some(next.clone());
        Ok(())
    }

    async fn dispatch_public_v1_history(
        &self,
        _scope: &PublicV1Scope,
        expected: &PublicV1AcceptedStatus,
        command: &ProtocolCommand,
    ) -> Result<(), String> {
        if self.status.lock().unwrap().as_ref() != Some(expected) {
            return Err("stale dispatch".into());
        }
        let ProtocolCommand::PublicV1History(command) = command else {
            return Err("unexpected command".into());
        };
        self.store
            .apply_public_v1_history_command(command)
            .await
            .map_err(|error| error.to_string())?;
        self.dispatched
            .lock()
            .unwrap()
            .push(ProtocolCommand::PublicV1History(command.clone()));
        Ok(())
    }
}

fn test_scope() -> PublicV1Scope {
    PublicV1Scope {
        namespace: "kuberic-system".into(),
        name: "resource-1".into(),
        resource_uid: ResourceUid::new("resource-1"),
    }
}

#[tokio::test]
async fn public_v1_history_controller_command_journal_and_report_round_trip() {
    let directory = tempfile::tempdir().unwrap();
    let path = TestingSqliteStore::metadata_database_path(directory.path());
    let resource_uid = ResourceUid::new("resource-1");
    let identity = PublicV1Identity::new(42);
    let storage = storage();
    let local = storage.replica.clone();
    let store = Arc::new(
        TestingSqliteStore::create_public_v1(
            &path,
            identity.clone(),
            TestingStorageIdentity {
                schema_version: SCHEMA_VERSION,
                resource_uid: resource_uid.clone(),
                pod_uid: PodUid::new("pod-1"),
                pvc_uid: PvcUid::new("pvc-1"),
                initialization_id: InitializationId::new("local-init"),
                local_identity: local.clone(),
                effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
            },
        )
        .unwrap(),
    );
    let api = HistoryApi {
        status: Mutex::new(None),
        store: store.clone(),
        dispatched: Mutex::new(Vec::new()),
    };

    execute_public_v1_history(
        &api,
        &test_scope(),
        plan_public_v1_bootstrap_history(None, &resource_uid, &identity, storage.clone()).unwrap(),
    )
    .await
    .unwrap();
    let accepted = api.status.lock().unwrap().clone().unwrap();
    execute_public_v1_history(
        &api,
        &test_scope(),
        plan_public_v1_bootstrap_history(
            Some(accepted.clone()),
            &resource_uid,
            &identity,
            storage.clone(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        store.public_v1_history_snapshot().await.unwrap().history,
        accepted.history
    );

    let process_session_id = ProcessSessionId::new("session-1");
    let initial_report = store
        .public_v1_agent_report(
            &identity,
            PublicV1ReportProjection {
                process_session_id: process_session_id.clone(),
                report_sequence: 1,
                role: ReplicaRole::Primary,
                read_status: AccessStatus::Granted,
                write_status: AccessStatus::Granted,
                progress: PublicProgress {
                    role: PublicProgressRole::Primary,
                    first: 10,
                    last: 10,
                },
            },
        )
        .await
        .unwrap();
    let authorization_id = OperationId::new("data-loss");
    let proposed =
        derive_data_loss_history(&authorization_id, &accepted.history, 1, &storage).unwrap();
    let authorization = DataLossAuthorization {
        authorization_id: authorization_id.clone(),
        predecessor: accepted.history.clone(),
        proposed,
        provisional_primary: storage,
        reset_scope: ProviderProgressReset::ResetToEmpty,
        replay_disposition: DataLossReplay::Convergent,
    };
    let attempt = DataLossAttempt {
        authorization_id,
        attempt_id: OperationId::new("attempt-1"),
        process: ProcessAttemptContext {
            process_session_id: process_session_id.clone(),
            replicator_creation_id: ReplicatorCreationId::new("creation-1"),
            catch_up_specific_quorum: true,
            data_loss_replay: DataLossReplay::Convergent,
            epoch: Epoch::new(1, 1),
            attempt_revision: 1,
        },
    };
    let members = vec![ConfigurationMember {
        identity: local.clone(),
        role: ReplicaRole::Primary,
    }];
    let configuration =
        ConfigurationDescriptor::new(attempt.process.epoch, local.replica_id, members.clone(), 1);
    let lifecycle = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::InitialPrimary,
        replica: local.clone(),
        epoch: attempt.process.epoch,
        possible_data_loss: PossibleDataLossIntent::Possible,
        current: configuration,
        previous: Some(ConfigurationDescriptor::new(
            Epoch::new(1, 0),
            local.replica_id,
            members,
            1,
        )),
        replicas: Vec::new(),
        data_loss_recovery: Some(PublicDataLossRecoveryInput {
            authorization: authorization.clone(),
            attempt: attempt.clone(),
        }),
    };
    let intent = PublicOperationIntent {
        identity: identity.clone(),
        operation_id: attempt.attempt_id.clone(),
        revision: attempt.process.attempt_revision,
        process_session_id: process_session_id.clone(),
        class: PublicOperationClass::Authority,
        input_digest: lifecycle.digest(),
        replay_disposition: PublicOperationReplayDisposition::DataLoss(DataLossReplay::Convergent),
        lifecycle: Some(lifecycle),
        program: None,
    };

    execute_public_v1_history(
        &api,
        &test_scope(),
        plan_public_v1_data_loss_recovery(
            &accepted,
            &initial_report,
            &authorization,
            &attempt,
            &intent,
            false,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let invalidated = api.status.lock().unwrap().clone().unwrap();
    execute_public_v1_history(
        &api,
        &test_scope(),
        plan_public_v1_data_loss_recovery(
            &invalidated,
            &initial_report,
            &authorization,
            &attempt,
            &intent,
            false,
        )
        .unwrap(),
    )
    .await
    .unwrap();

    let report = store
        .public_v1_agent_report(
            &identity,
            PublicV1ReportProjection {
                process_session_id,
                report_sequence: 2,
                role: ReplicaRole::Primary,
                read_status: AccessStatus::NotPrimary,
                write_status: AccessStatus::NotPrimary,
                progress: PublicProgress {
                    role: PublicProgressRole::Recovery,
                    first: 0,
                    last: 0,
                },
            },
        )
        .await
        .unwrap();
    let wire = kuberic_runtime::control::public_v1_agent_status_to_proto(&report);
    let normalized =
        kuberic_runtime::control::normalize_public_v1_agent_status_report(wire).unwrap();
    assert_eq!(normalized, report);
    assert!(matches!(
        plan_public_v1_recovery_report(&invalidated, &normalized).unwrap(),
        kuberic_controller::evaluator::testing::PublicV1HistoryPlan::Stable
    ));
    let mut changed_epoch = normalized.clone();
    changed_epoch
        .data_loss_recovery
        .as_mut()
        .unwrap()
        .attempt
        .process
        .epoch
        .configuration_number += 1;
    assert!(plan_public_v1_recovery_report(&invalidated, &changed_epoch).is_err());

    let mut changed_revision = normalized.clone();
    changed_revision
        .data_loss_recovery
        .as_mut()
        .unwrap()
        .attempt
        .process
        .attempt_revision += 1;
    assert!(plan_public_v1_recovery_report(&invalidated, &changed_revision).is_err());

    let mut changed_creation = normalized.clone();
    changed_creation
        .data_loss_recovery
        .as_mut()
        .unwrap()
        .attempt
        .process
        .replicator_creation_id = ReplicatorCreationId::new("other-creation");
    assert!(plan_public_v1_recovery_report(&invalidated, &changed_creation).is_err());

    let mut changed_quorum = normalized.clone();
    changed_quorum
        .data_loss_recovery
        .as_mut()
        .unwrap()
        .attempt
        .process
        .catch_up_specific_quorum = false;
    assert!(plan_public_v1_recovery_report(&invalidated, &changed_quorum).is_err());

    let mut changed_reset = normalized;
    changed_reset
        .data_loss_recovery
        .as_mut()
        .unwrap()
        .reset_scope = ProviderProgressReset::Preserve;
    assert!(plan_public_v1_recovery_report(&invalidated, &changed_reset).is_err());

    store
        .contain_public_v1_data_loss_attempt(&intent)
        .await
        .unwrap();
    let contained_report = store
        .public_v1_agent_report(
            &identity,
            PublicV1ReportProjection {
                process_session_id: ProcessSessionId::new("session-1"),
                report_sequence: 3,
                role: ReplicaRole::Primary,
                read_status: AccessStatus::NotPrimary,
                write_status: AccessStatus::NotPrimary,
                progress: PublicProgress {
                    role: PublicProgressRole::Recovery,
                    first: 0,
                    last: 0,
                },
            },
        )
        .await
        .unwrap();
    execute_public_v1_history(
        &api,
        &test_scope(),
        plan_public_v1_recovery_report(&invalidated, &contained_report).unwrap(),
    )
    .await
    .unwrap();
    let contained = api.status.lock().unwrap().clone().unwrap();

    let successor_attempt = DataLossAttempt {
        authorization_id: authorization.authorization_id.clone(),
        attempt_id: OperationId::new("attempt-2"),
        process: ProcessAttemptContext {
            process_session_id: ProcessSessionId::new("session-2"),
            replicator_creation_id: ReplicatorCreationId::new("creation-2"),
            catch_up_specific_quorum: true,
            data_loss_replay: DataLossReplay::Convergent,
            epoch: Epoch::new(1, 2),
            attempt_revision: 2,
        },
    };
    let successor_members = vec![ConfigurationMember {
        identity: authorization.provisional_primary.replica.clone(),
        role: ReplicaRole::Primary,
    }];
    let successor_lifecycle = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::InitialPrimary,
        replica: authorization.provisional_primary.replica.clone(),
        epoch: successor_attempt.process.epoch,
        possible_data_loss: PossibleDataLossIntent::Possible,
        current: ConfigurationDescriptor::new(
            successor_attempt.process.epoch,
            authorization.provisional_primary.replica.replica_id,
            successor_members.clone(),
            1,
        ),
        previous: Some(ConfigurationDescriptor::new(
            Epoch::new(1, 1),
            authorization.provisional_primary.replica.replica_id,
            successor_members,
            1,
        )),
        replicas: Vec::new(),
        data_loss_recovery: Some(PublicDataLossRecoveryInput {
            authorization: authorization.clone(),
            attempt: successor_attempt.clone(),
        }),
    };
    let successor_intent = PublicOperationIntent {
        identity: identity.clone(),
        operation_id: successor_attempt.attempt_id.clone(),
        revision: successor_attempt.process.attempt_revision,
        process_session_id: successor_attempt.process.process_session_id.clone(),
        class: PublicOperationClass::Authority,
        input_digest: successor_lifecycle.digest(),
        replay_disposition: PublicOperationReplayDisposition::DataLoss(DataLossReplay::Convergent),
        lifecycle: Some(successor_lifecycle),
        program: None,
    };
    let replacement_report = PublicV1AgentReport {
        identity: identity.clone(),
        resource_uid: resource_uid.clone(),
        replica: authorization.provisional_primary.replica.clone(),
        process_session_id: successor_attempt.process.process_session_id.clone(),
        replicator_creation_id: successor_attempt.process.replicator_creation_id.clone(),
        report_sequence: 1,
        open_mode: kuberic_runtime::protocol::types::OpenMode::Existing,
        role: ReplicaRole::ActiveSecondary,
        read_status: AccessStatus::NotPrimary,
        write_status: AccessStatus::NotPrimary,
        history: authorization.predecessor.clone(),
        storage_incarnation: authorization.provisional_primary.clone(),
        progress: PublicProgress {
            role: PublicProgressRole::Secondary,
            first: 0,
            last: 0,
        },
        replication_address: None,
        service_location: None,
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        extensions: Default::default(),
    };
    assert!(
        plan_public_v1_data_loss_recovery(
            &contained,
            &replacement_report,
            &authorization,
            &successor_attempt,
            &successor_intent,
            false,
        )
        .is_err()
    );
    execute_public_v1_history(
        &api,
        &test_scope(),
        plan_public_v1_data_loss_recovery(
            &contained,
            &replacement_report,
            &authorization,
            &successor_attempt,
            &successor_intent,
            true,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let renewed = api.status.lock().unwrap().clone().unwrap();
    assert_eq!(
        renewed.data_loss_recovery.as_ref().unwrap().attempt,
        successor_attempt
    );
    execute_public_v1_history(
        &api,
        &test_scope(),
        plan_public_v1_data_loss_recovery(
            &renewed,
            &replacement_report,
            &authorization,
            &successor_attempt,
            &successor_intent,
            true,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        store
            .public_v1_history_snapshot()
            .await
            .unwrap()
            .recovery
            .unwrap()
            .attempt,
        successor_attempt
    );
    let renewed_report = store
        .public_v1_agent_report(
            &identity,
            PublicV1ReportProjection {
                process_session_id: ProcessSessionId::new("session-2"),
                report_sequence: 2,
                role: ReplicaRole::Primary,
                read_status: AccessStatus::NotPrimary,
                write_status: AccessStatus::NotPrimary,
                progress: PublicProgress {
                    role: PublicProgressRole::Recovery,
                    first: 0,
                    last: 0,
                },
            },
        )
        .await
        .unwrap();
    let renewed_wire = kuberic_runtime::control::public_v1_agent_status_to_proto(&renewed_report);
    let renewed_normalized =
        kuberic_runtime::control::normalize_public_v1_agent_status_report(renewed_wire).unwrap();
    assert!(matches!(
        plan_public_v1_recovery_report(&renewed, &renewed_normalized).unwrap(),
        kuberic_controller::evaluator::testing::PublicV1HistoryPlan::Stable
    ));
    assert_eq!(api.dispatched.lock().unwrap().len(), 3);
}
