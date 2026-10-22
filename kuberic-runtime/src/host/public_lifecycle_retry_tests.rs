use super::*;
use crate::host::state::{PublicCallbackFamily as Family, PublicOperationContainment};
use crate::host::store::BeginPublicOperation;

async fn configuration(fixture: &Fixture) -> PublicConfiguration {
    let local = fixture
        .store
        .load_state()
        .await
        .unwrap()
        .identity
        .local_identity;
    PublicConfiguration {
        current: ConfigurationDescriptor::new(
            Epoch::new(0, 2),
            local.replica_id,
            vec![ConfigurationMember {
                identity: local,
                role: ReplicaRole::Primary,
            }],
            1,
        ),
        previous: None,
        replicas: Vec::new(),
    }
}

fn declare(fixture: &mut Fixture, families: impl IntoIterator<Item = Family>) {
    fixture.callbacks.retry_proof = PublicCallbackRetryProof::declared(families);
}

#[test]
fn no_retry_family_certifies_build_data_loss_or_terminal_instructions() {
    for family in [
        Family::ReplicatorRole,
        Family::ApplicationRole,
        Family::Epoch,
        Family::Configuration,
        Family::CatchUp,
    ] {
        for instruction in [
            PublicInstruction::Build,
            PublicInstruction::DataLoss,
            PublicInstruction::Remove,
            PublicInstruction::ApplicationOpen,
            PublicInstruction::ReplicatorOpen,
            PublicInstruction::ReplicatorClose,
            PublicInstruction::ApplicationClose,
            PublicInstruction::Abort,
            PublicInstruction::Cleanup,
            PublicInstruction::Revoke,
            PublicInstruction::Access,
            PublicInstruction::HistoryAdmission,
        ] {
            assert!(
                !family.allows(instruction),
                "{family:?} cannot certify {instruction:?}"
            );
        }
    }
}

#[tokio::test]
async fn declared_application_role_retry_keeps_prefix_and_immutable_earlier_attempt() {
    let mut fixture = Fixture::new().await;
    declare(&mut fixture, [Family::ApplicationRole]);
    fixture
        .probe
        .fail_application_role
        .store(true, Ordering::Release);
    let intent = fixture.intent(
        PublicOperationProgram::Role {
            epoch: Epoch::new(0, 2),
            role: ReplicaRole::ActiveSecondary,
        },
        1,
    );
    let (operation, first_attempt, bound_observer) = launch_bound_attempt(
        &fixture.registry,
        fixture.store.clone(),
        intent.clone(),
        fixture.callbacks.clone(),
        |operation| {
            (
                operation.snapshot().attempt,
                operation.intent().input_digest,
            )
        },
    )
    .await
    .unwrap();
    let first = operation.wait_for_attempt(first_attempt).await.unwrap();
    assert_eq!(first.stage, PublicOperationStage::RetryPending);
    assert_eq!(first.lifecycle.outcomes, [PublicInstructionOutcome::Done]);
    assert_eq!(
        first.retry_failure.as_ref().unwrap().family,
        Family::ApplicationRole
    );
    assert_eq!(first.containment, PublicOperationContainment::NotRequired);
    assert_eq!(bound_observer, (first_attempt, intent.input_digest.clone()));
    // Do not poll this earlier observer until an immediate exact retry finishes.
    let earlier_waiter = operation.wait_for_attempt(first_attempt);
    fixture
        .probe
        .fail_application_role
        .store(false, Ordering::Release);
    let (retry, attached) = tokio::join!(
        launch_attempt(
            &fixture.registry,
            fixture.store.clone(),
            intent.clone(),
            fixture.callbacks.clone()
        ),
        launch_attempt(
            &fixture.registry,
            fixture.store.clone(),
            intent.clone(),
            fixture.callbacks.clone()
        ),
    );
    let (retry, second_attempt) = retry.unwrap();
    let (attached, attached_attempt) = attached.unwrap();
    assert!(Arc::ptr_eq(&retry, &attached));
    assert_eq!(second_attempt, first_attempt + 1);
    assert_eq!(attached_attempt, second_attempt);
    let second = retry.wait_for_attempt(second_attempt).await.unwrap();
    assert_eq!(second.stage, PublicOperationStage::Completed);
    assert_eq!(
        second.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    let earlier = earlier_waiter.await.unwrap();
    assert_eq!(earlier, first);
    assert_eq!(earlier.attempt, first_attempt);
    assert_eq!(earlier.stage, first.stage);
    assert_eq!(earlier.disposition, first.disposition);
    assert_eq!(earlier.lifecycle, first.lifecycle);
    assert_eq!(earlier.retry_failure, first.retry_failure);
    assert_eq!(second.intent, first.intent);
    assert_eq!(
        second.attempt_results[&first_attempt],
        first.attempt_results[&first_attempt]
    );
    assert_eq!(
        *fixture.probe.events.lock().unwrap(),
        [
            "replicator.role.ActiveSecondary",
            "application.role.ActiveSecondary",
            "application.role.ActiveSecondary",
        ]
    );
    let (_, completed_attempt) = launch_attempt(
        &fixture.registry,
        fixture.store.clone(),
        intent,
        fixture.callbacks.clone(),
    )
    .await
    .unwrap();
    assert_eq!(completed_attempt, second_attempt);
    assert_eq!(
        retry
            .wait_for_attempt(second_attempt)
            .await
            .unwrap()
            .disposition,
        second.disposition
    );
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn retry_pending_report_is_separate_exact_and_round_trips_without_completion() {
    use crate::control::convert::{
        normalize_public_v1_agent_status_report, public_v1_agent_status_to_proto,
    };
    use crate::protocol::public_operations::{ProgressQueryResult, PublicProgressRole};
    let mut fixture = Fixture::new().await;
    declare(&mut fixture, [Family::ApplicationRole]);
    fixture
        .probe
        .fail_application_role
        .store(true, Ordering::Release);
    let operation = fixture
        .launch(
            PublicOperationProgram::Role {
                epoch: Epoch::new(0, 2),
                role: ReplicaRole::ActiveSecondary,
            },
            1,
        )
        .await;
    assert_eq!(
        completed(&operation).await.stage,
        PublicOperationStage::RetryPending
    );
    let state = fixture.store.load_state().await.unwrap();
    let preview = state.public_v1_operation_state.unwrap();
    let report = public_v1_report(
        fixture.store.as_ref(),
        &preview.identity,
        PublicV1ReportProjection {
            catch_up_specific_quorum: None,
            process_session_id: ProcessSessionId::new("session"),
            replicator_creation_id: ReplicatorCreationId::new("creation"),
            report_sequence: 1,
            role: ReplicaRole::None,
            read_status: AccessStatus::ReconfigurationPending,
            write_status: AccessStatus::ReconfigurationPending,
            progress: PublicProgress {
                role: PublicProgressRole::Recovery,
                current_progress: ProgressQueryResult::NotReady,
                catch_up_capability: ProgressQueryResult::NotReady,
            },
            progress_history: preview.history.clone(),
            native_idle_replicas: None,
            native_configuration: None,
            observation_alive: None,
        },
    )
    .await
    .unwrap();
    assert!(report.active_operations.is_empty());
    assert!(report.completed_operations.is_empty());
    assert!(report.configuration.is_none());
    assert_eq!(report.retry_pending_operations.len(), 1);
    let pending = &report.retry_pending_operations[0];
    assert_eq!(pending.intent, operation.intent());
    assert_eq!(pending.attempt, 1);
    assert!(pending.failure.contains("application role injected"));
    let wire = public_v1_agent_status_to_proto(&report);
    assert_eq!(
        wire.public_v1
            .as_ref()
            .unwrap()
            .retry_pending_operations_json
            .len(),
        1
    );
    assert_eq!(
        normalize_public_v1_agent_status_report(wire).unwrap(),
        report
    );
    for changed in [
        "zero-attempt",
        "empty-failure",
        "wrong-session",
        "also-active",
    ] {
        let mut invalid = report.clone();
        match changed {
            "zero-attempt" => invalid.retry_pending_operations[0].attempt = 0,
            "empty-failure" => invalid.retry_pending_operations[0].failure.clear(),
            "wrong-session" => {
                invalid.retry_pending_operations[0]
                    .intent
                    .process_session_id = ProcessSessionId::new("other")
            }
            _ => invalid.active_operations.push(pending.intent.clone()),
        }
        assert!(invalid.validate().is_err(), "{changed}");
        assert!(
            normalize_public_v1_agent_status_report(public_v1_agent_status_to_proto(&invalid))
                .is_err(),
            "{changed}"
        );
    }
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn remote_build_projection_separates_admission_running_success_and_retirement() {
    use crate::host::public_build_store::{PublicV1BuildRecord, PublicV1BuildRetirement};
    use crate::host::state::PublicRemoteBuildState as Remote;
    use crate::protocol::public_operations::{PublicOperationClass, PublicV1OperationResult};
    let fixture = Fixture::new().await;
    let seed = fixture
        .launch(
            PublicOperationProgram::Epoch {
                epoch: Epoch::new(0, 2),
            },
            1,
        )
        .await;
    let mut operation = completed(&seed).await;
    let mut preview = fixture
        .store
        .load_state()
        .await
        .unwrap()
        .public_v1_operation_state
        .unwrap();
    preview.operations.clear();
    let source_storage = preview.storage_incarnation.clone().unwrap();
    let target = ReplicaIdentity {
        replica_id: ReplicaId::new(2),
        instance_id: ReplicaInstanceId::new("target"),
        agent_generation: AgentGeneration::new("target-generation"),
    };
    let target_storage = ReplicaStorageIncarnationContext {
        replica: target.clone(),
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new("target"),
            pvc_uid: PvcUid::new("target-pvc"),
        },
        storage_generation: StorageGeneration::new("target-storage"),
    };
    let build = PublicBuildInput {
        attempt: OperationId::new("sf06-2"),
        epoch: Epoch::new(0, 2),
        current_configuration_id: Some(ConfigurationId::new("configuration")),
        replica: target.clone(),
        target_storage: target_storage.clone(),
        process_session_id: ProcessSessionId::new("target-session"),
        source_address: "http://source".into(),
        replication_address: "http://target".into(),
        history: preview.history.clone(),
    };
    let mut intent = fixture.intent(PublicOperationProgram::Build(build.clone()), 2);
    let remove_program = PublicOperationProgram::Remove(build.clone());
    intent.class = PublicOperationClass::Build {
        target: target.replica_id,
    };
    operation.intent = intent.clone();
    operation.stage = PublicOperationStage::Ready;
    operation.disposition = None;
    operation.lifecycle = Default::default();
    operation.attempt_results.clear();
    preview
        .operations
        .insert(intent.operation_id.clone(), operation);
    preview.build_records.insert(
        intent.operation_id.clone(),
        PublicV1BuildRecord {
            operation_id: intent.operation_id.clone(),
            build_id: build.attempt,
            admission_revision: intent.revision,
            history: preview.history.clone().unwrap(),
            epoch: build.epoch,
            source: source_storage.replica.clone(),
            source_storage,
            source_session: intent.process_session_id.clone(),
            target,
            target_storage,
            target_session: build.process_session_id,
            target_address: build.replication_address,
            retirement: PublicV1BuildRetirement::Retain,
            result: None,
        },
    );
    let project = |preview: &crate::host::state::PublicV1OperationState| {
        preview.remote_build_states(&intent.process_session_id)[0].state
    };
    assert_eq!(project(&preview), Remote::Accepted);
    preview
        .operations
        .get_mut(&intent.operation_id)
        .unwrap()
        .stage = PublicOperationStage::Running;
    assert_eq!(project(&preview), Remote::InBuild);
    preview
        .build_records
        .get_mut(&intent.operation_id)
        .unwrap()
        .result = Some(PublicV1OperationResult::Succeeded);
    assert_eq!(project(&preview), Remote::InBuild);
    let record = preview.operations.get_mut(&intent.operation_id).unwrap();
    record.stage = PublicOperationStage::Completed;
    record.disposition = Some(PublicOperationDisposition::Succeeded);
    assert_eq!(project(&preview), Remote::Ready);
    let mut remove = preview.operations[&intent.operation_id].clone();
    remove.intent = fixture.intent(remove_program, 2);
    remove.intent.operation_id = OperationId::new("remove-build");
    remove.intent.class = PublicOperationClass::Remove {
        target: ReplicaId::new(2),
    };
    remove.stage = PublicOperationStage::Ready;
    remove.disposition = None;
    preview
        .operations
        .insert(remove.intent.operation_id.clone(), remove);
    assert_eq!(project(&preview), Remote::InDrop);
    preview.operations.remove(&OperationId::new("remove-build"));
    assert_eq!(project(&preview), Remote::Ready);
    preview
        .operations
        .get_mut(&intent.operation_id)
        .unwrap()
        .superseded_by = Some(OperationId::new("remove"));
    assert_eq!(project(&preview), Remote::InDrop);
    preview
        .build_records
        .get_mut(&intent.operation_id)
        .unwrap()
        .retirement = PublicV1BuildRetirement::Retired;
    assert_eq!(project(&preview), Remote::Dropped);
    assert!(
        preview
            .remote_build_states(&ProcessSessionId::new("other-session"))
            .is_empty()
    );
    assert!(preview.current_inbound_build_authority.is_none());
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn declared_configuration_and_epoch_failures_resume_only_exact_frozen_input() {
    for family in [Family::Configuration, Family::Epoch] {
        let mut fixture = Fixture::new().await;
        declare(&mut fixture, [family]);
        let program = if family == Family::Configuration {
            fixture
                .probe
                .fail_configuration
                .store(true, Ordering::Release);
            PublicOperationProgram::Configuration(configuration(&fixture).await)
        } else {
            fixture.probe.fail_epoch.store(true, Ordering::Release);
            PublicOperationProgram::Epoch {
                epoch: Epoch::new(0, 2),
            }
        };
        let intent = fixture.intent(program, 1);
        let (operation, attempt) = launch_attempt(
            &fixture.registry,
            fixture.store.clone(),
            intent.clone(),
            fixture.callbacks.clone(),
        )
        .await
        .unwrap();
        let failed = operation.wait_for_attempt(attempt).await.unwrap();
        assert_eq!(failed.stage, PublicOperationStage::RetryPending);
        assert!(failed.lifecycle.outcomes.is_empty());
        assert_eq!(failed.retry_failure.unwrap().family, family);
        let mut changed = intent.clone();
        changed.revision += 1;
        assert!(
            launch(
                &fixture.registry,
                fixture.store.clone(),
                changed,
                fixture.callbacks.clone()
            )
            .await
            .is_err()
        );
        assert_eq!(operation.snapshot().attempt, attempt);
        fixture
            .probe
            .fail_configuration
            .store(false, Ordering::Release);
        fixture.probe.fail_epoch.store(false, Ordering::Release);
        let (retry, next) = launch_attempt(
            &fixture.registry,
            fixture.store.clone(),
            intent.clone(),
            fixture.callbacks.clone(),
        )
        .await
        .unwrap();
        assert_eq!(next, attempt + 1);
        let complete = retry.wait_for_attempt(next).await.unwrap();
        assert_eq!(complete.intent, intent);
        assert_eq!(
            complete.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
        assert_eq!(fixture.probe.events.lock().unwrap().len(), 2);
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn engine_role_declaration_cannot_certify_application_failure_or_native_observer() {
    for native_observer in [false, true] {
        let mut fixture = Fixture::new().await;
        fixture.callbacks.native_observer_callbacks = native_observer;
        declare(&mut fixture, [Family::ReplicatorRole]);
        fixture
            .probe
            .fail_application_role
            .store(true, Ordering::Release);
        let program = PublicOperationProgram::Role {
            epoch: Epoch::new(0, 2),
            role: ReplicaRole::ActiveSecondary,
        };
        let operation = fixture.launch(program.clone(), 1).await;
        let failed = completed(&operation).await;
        assert_eq!(failed.stage, PublicOperationStage::ContainmentPending);
        assert!(failed.retry_failure.is_none());
        fixture
            .probe
            .fail_application_role
            .store(false, Ordering::Release);
        let retry = fixture.launch(program, 1).await;
        assert_eq!(retry.snapshot().attempt, 1);
        assert_eq!(completed(&retry).await.disposition, failed.disposition);
        assert_eq!(fixture.probe.events.lock().unwrap().len(), 2);
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn declared_replicator_role_and_catchup_resume_with_frozen_successful_prefix() {
    for family in [Family::ReplicatorRole, Family::CatchUp] {
        let mut fixture = Fixture::new().await;
        declare(&mut fixture, [family]);
        let program = if family == Family::ReplicatorRole {
            fixture.probe.fail_role.store(true, Ordering::Release);
            PublicOperationProgram::Role {
                epoch: Epoch::new(0, 2),
                role: ReplicaRole::ActiveSecondary,
            }
        } else {
            fixture.probe.fail_catchup.store(true, Ordering::Release);
            PublicOperationProgram::CatchUp {
                configuration: configuration(&fixture).await,
                mode: PublicCatchUpMode::WriteQuorum,
            }
        };
        let operation = fixture.launch(program.clone(), 1).await;
        let failed = completed(&operation).await;
        assert_eq!(failed.stage, PublicOperationStage::RetryPending);
        assert_eq!(failed.retry_failure.as_ref().unwrap().family, family);
        fixture.probe.fail_role.store(false, Ordering::Release);
        fixture.probe.fail_catchup.store(false, Ordering::Release);
        let retry = fixture.launch(program, 1).await;
        let successful = completed(&retry).await;
        assert_eq!(successful.attempt, 2);
        assert_eq!(
            successful.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
        assert_eq!(retry.wait_for_attempt(1).await.unwrap(), failed);
        assert_eq!(
            &successful.lifecycle.outcomes[..failed.lifecycle.outcomes.len()],
            failed.lifecycle.outcomes
        );
        assert_eq!(
            fixture.probe.events.lock().unwrap().len(),
            if family == Family::ReplicatorRole {
                3
            } else {
                4
            }
        );
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn native_observer_is_not_a_returned_replicator_failure_repeatability_contract() {
    let mut fixture = Fixture::new().await;
    fixture.callbacks.native_observer_callbacks = true;
    fixture.probe.fail_role.store(true, Ordering::Release);
    let program = PublicOperationProgram::Role {
        epoch: Epoch::new(0, 2),
        role: ReplicaRole::ActiveSecondary,
    };
    let operation = fixture.launch(program.clone(), 1).await;
    let failed = completed(&operation).await;
    assert_eq!(failed.stage, PublicOperationStage::Completed);
    assert!(matches!(
        failed.disposition,
        Some(PublicOperationDisposition::Failed(_))
    ));
    fixture.probe.fail_role.store(false, Ordering::Release);
    let replay = fixture.launch(program, 1).await;
    assert_eq!(replay.snapshot().attempt, 1);
    assert_eq!(completed(&replay).await.disposition, failed.disposition);
    assert_eq!(fixture.probe.events.lock().unwrap().len(), 1);
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn retrying_catchup_cannot_borrow_configuration_repeatability_for_nested_reinstall() {
    let mut fixture = Fixture::new().await;
    declare(&mut fixture, [Family::CatchUp]);
    fixture.probe.fail_catchup.store(true, Ordering::Release);
    let program = PublicOperationProgram::CatchUp {
        configuration: configuration(&fixture).await,
        mode: PublicCatchUpMode::WriteQuorum,
    };
    let operation = fixture.launch(program.clone(), 1).await;
    let failed = completed(&operation).await;
    assert_eq!(failed.stage, PublicOperationStage::RetryPending);
    assert_eq!(failed.lifecycle.outcomes, [PublicInstructionOutcome::Done]);
    fixture.probe.fail_catchup.store(false, Ordering::Release);
    fixture
        .probe
        .fail_configuration
        .store(true, Ordering::Release);
    let retry = fixture.launch(program.clone(), 1).await;
    let nested = completed(&retry).await;
    assert_eq!(nested.stage, PublicOperationStage::ContainmentPending);
    assert_eq!(nested.attempt, 2);
    assert!(nested.retry_failure.is_none());
    assert_eq!(nested.lifecycle.outcomes, [PublicInstructionOutcome::Done]);
    let replay = fixture.launch(program, 1).await;
    assert_eq!(replay.snapshot().attempt, 2);
    assert_eq!(
        *fixture.probe.events.lock().unwrap(),
        ["configuration", "catchup", "configuration"]
    );
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn declared_canonical_rebuild_takes_precedence_over_retry_pending() {
    let mut fixture = Fixture::new().await;
    fixture.callbacks.native_observer_callbacks = true;
    declare(&mut fixture, [Family::ReplicatorRole]);
    *fixture.probe.role_rebuild.lock().unwrap() = Some(
        fixture
            .store
            .load_state()
            .await
            .unwrap()
            .identity
            .local_identity,
    );
    let program = PublicOperationProgram::Role {
        epoch: Epoch::new(0, 2),
        role: ReplicaRole::ActiveSecondary,
    };
    let operation = fixture.launch(program.clone(), 1).await;
    let failed = completed(&operation).await;
    assert_eq!(failed.stage, PublicOperationStage::Completed);
    assert!(failed.retry_failure.is_none());
    assert!(matches!(
        failed.lifecycle.outcomes.as_slice(),
        [PublicInstructionOutcome::RebuildRequired(_)]
    ));
    let retry = fixture.launch(program, 1).await;
    assert_eq!(retry.snapshot().attempt, 1);
    assert_eq!(fixture.probe.events.lock().unwrap().len(), 1);
    fixture.registry.shutdown().await.unwrap();
}

struct FailingInstructionStore {
    store: Arc<SqliteStore>,
    fail_receipt: bool,
    fail_retry_receipt: bool,
}

#[async_trait::async_trait]
impl AgentStore for FailingInstructionStore {
    async fn identity(&self) -> Result<StorageIdentity> {
        self.store.identity().await
    }
    async fn load_state(&self) -> Result<AgentState> {
        self.store.load_state().await
    }
    async fn complete_application_initialization(&self) -> Result<()> {
        self.store.complete_application_initialization().await
    }
    async fn record_partition_reports(
        &self,
        metrics: Vec<LoadMetric>,
        fault: Option<FaultType>,
    ) -> Result<()> {
        self.store.record_partition_reports(metrics, fault).await
    }
    async fn begin_public_operation(
        &self,
        intent: &PublicOperationIntent,
        blockers: &[OperationId],
        superseded: &[OperationId],
    ) -> Result<BeginPublicOperation> {
        self.store
            .begin_public_operation(intent, blockers, superseded)
            .await
    }
    async fn advance_public_operation(
        &self,
        id: &OperationId,
        revision: u64,
        session: &ProcessSessionId,
        expected: PublicOperationStage,
        next: PublicOperationStage,
        disposition: Option<PublicOperationDisposition>,
    ) -> Result<PublicOperationRecord> {
        self.store
            .advance_public_operation(id, revision, session, expected, next, disposition)
            .await
    }
    async fn public_operation_records(&self) -> Result<Vec<PublicOperationRecord>> {
        self.store.public_operation_records().await
    }
    async fn public_instruction(
        &self,
        intent: &PublicOperationIntent,
        index: usize,
        instruction: PublicInstruction,
        outcome: Option<PublicInstructionOutcome>,
    ) -> Result<()> {
        if !self.fail_retry_receipt && outcome.is_some() == self.fail_receipt {
            Err(HostError::Corrupt(
                "injected host instruction persistence failure".into(),
            ))
        } else {
            self.store
                .public_instruction(intent, index, instruction, outcome)
                .await
        }
    }
    async fn record_public_retry_failure(
        &self,
        _: &PublicOperationIntent,
        _: u64,
        _: &PublicCallbackFailure,
    ) -> Result<PublicOperationRecord> {
        if self.fail_retry_receipt {
            Err(HostError::Corrupt("retry receipt failure injected".into()))
        } else {
            panic!("host receipt/selection failure is not a returned callback failure")
        }
    }
}

#[tokio::test]
async fn callback_success_receipt_failure_and_unknown_host_failure_never_enable_retry() {
    for fail_receipt in [false, true] {
        let mut fixture = Fixture::new().await;
        declare(
            &mut fixture,
            [Family::ReplicatorRole, Family::ApplicationRole],
        );
        let store: Arc<dyn AgentStore> = Arc::new(FailingInstructionStore {
            store: fixture.store.clone(),
            fail_receipt,
            fail_retry_receipt: false,
        });
        let registry = PartitionOperationRegistry::new(
            store.clone(),
            PublicV1Identity::new(6),
            ProcessSessionId::new("session"),
        )
        .unwrap();
        let intent = fixture.intent(
            PublicOperationProgram::Role {
                epoch: Epoch::new(0, 2),
                role: ReplicaRole::ActiveSecondary,
            },
            1,
        );
        let operation = launch(
            &registry,
            store.clone(),
            intent.clone(),
            fixture.callbacks.clone(),
        )
        .await
        .unwrap();
        let failed = completed(&operation).await;
        assert_eq!(failed.stage, PublicOperationStage::ContainmentPending);
        assert!(failed.retry_failure.is_none());
        let retry = launch(&registry, store, intent, fixture.callbacks.clone())
            .await
            .unwrap();
        assert_eq!(retry.snapshot().attempt, 1);
        assert_eq!(
            fixture.probe.events.lock().unwrap().len(),
            usize::from(fail_receipt)
        );
        registry.shutdown().await.unwrap();
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn retry_receipt_persistence_failure_is_ambiguous_not_permission_to_restart() {
    let mut fixture = Fixture::new().await;
    declare(&mut fixture, [Family::ApplicationRole]);
    fixture.callbacks.native_observer_callbacks = true;
    fixture
        .probe
        .fail_application_role
        .store(true, Ordering::Release);
    let store: Arc<dyn AgentStore> = Arc::new(FailingInstructionStore {
        store: fixture.store.clone(),
        fail_receipt: false,
        fail_retry_receipt: true,
    });
    let registry = PartitionOperationRegistry::new(
        store.clone(),
        PublicV1Identity::new(6),
        ProcessSessionId::new("session"),
    )
    .unwrap();
    let intent = fixture.intent(
        PublicOperationProgram::Role {
            epoch: Epoch::new(0, 2),
            role: ReplicaRole::ActiveSecondary,
        },
        1,
    );
    let operation = launch(
        &registry,
        store.clone(),
        intent.clone(),
        fixture.callbacks.clone(),
    )
    .await
    .unwrap();
    let failed = completed(&operation).await;
    assert_eq!(failed.stage, PublicOperationStage::ContainmentPending);
    assert!(
        matches!(failed.disposition, Some(PublicOperationDisposition::Ambiguous(ref error))
            if error.contains("retry receipt failure injected"))
    );
    assert!(failed.retry_failure.is_none());
    fixture
        .probe
        .fail_application_role
        .store(false, Ordering::Release);
    let retry = launch(&registry, store, intent, fixture.callbacks.clone())
        .await
        .unwrap();
    assert_eq!(retry.snapshot().attempt, 1);
    assert_eq!(fixture.probe.events.lock().unwrap().len(), 2);
    registry.shutdown().await.unwrap();
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn rehydrated_retry_pending_preserves_attempt_results_and_requires_live_declaration() {
    let mut fixture = Fixture::new().await;
    declare(&mut fixture, [Family::ApplicationRole]);
    fixture
        .probe
        .fail_application_role
        .store(true, Ordering::Release);
    let program = PublicOperationProgram::Role {
        epoch: Epoch::new(0, 2),
        role: ReplicaRole::ActiveSecondary,
    };
    let operation = fixture.launch(program.clone(), 1).await;
    let first = completed(&operation).await;
    assert_eq!(first.stage, PublicOperationStage::RetryPending);
    fixture.replace_registry(false, false);
    fixture.registry.recover_unowned().await.unwrap();
    fixture.callbacks.retry_proof = Default::default();
    fixture
        .probe
        .fail_application_role
        .store(false, Ordering::Release);
    let not_restarted = fixture.launch(program.clone(), 1).await;
    assert_eq!(not_restarted.snapshot().attempt, 1);
    assert_eq!(
        completed(&not_restarted).await.stage,
        PublicOperationStage::RetryPending
    );
    assert_eq!(fixture.probe.events.lock().unwrap().len(), 2);
    declare(&mut fixture, [Family::ApplicationRole]);
    let resumed = fixture.launch(program, 1).await;
    let second = completed(&resumed).await;
    assert_eq!(second.attempt, 2);
    assert_eq!(
        second.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    assert_eq!(resumed.wait_for_attempt(1).await.unwrap(), first);
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn explicit_retry_pending_is_not_resumed_by_recovery_or_after_cancellation_or_supersession() {
    for supersede in [false, true] {
        let mut fixture = Fixture::new().await;
        declare(&mut fixture, [Family::ApplicationRole]);
        fixture
            .probe
            .fail_application_role
            .store(true, Ordering::Release);
        let program = PublicOperationProgram::Role {
            epoch: Epoch::new(0, 2),
            role: ReplicaRole::ActiveSecondary,
        };
        let operation = fixture.launch(program.clone(), 1).await;
        assert_eq!(
            completed(&operation).await.stage,
            PublicOperationStage::RetryPending
        );
        fixture.registry.recover_unowned().await.unwrap();
        assert_eq!(
            operation.snapshot().stage,
            PublicOperationStage::RetryPending
        );
        assert_eq!(operation.snapshot().attempt, 1);
        fixture
            .probe
            .fail_application_role
            .store(false, Ordering::Release);
        if supersede {
            let successor = fixture
                .launch(
                    PublicOperationProgram::Role {
                        epoch: Epoch::new(0, 3),
                        role: ReplicaRole::ActiveSecondary,
                    },
                    2,
                )
                .await;
            assert_eq!(
                completed(&successor).await.disposition,
                Some(PublicOperationDisposition::Succeeded)
            );
        } else {
            operation.cancel_root().await.unwrap();
        }
        let replay = fixture.launch(program, 1).await;
        assert_eq!(replay.snapshot().attempt, 1);
        assert_ne!(replay.snapshot().stage, PublicOperationStage::RetryPending);
        assert_eq!(
            fixture.probe.events.lock().unwrap().len(),
            if supersede { 4 } else { 2 }
        );
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn declared_repeatability_never_restarts_failed_terminal_cleanup() {
    let mut fixture = Fixture::new().await;
    let (_live_owner, witness) = tokio::sync::watch::channel(false);
    fixture.callbacks.containment = Some(witness);
    declare(
        &mut fixture,
        [
            Family::ReplicatorRole,
            Family::ApplicationRole,
            Family::Epoch,
            Family::Configuration,
            Family::CatchUp,
        ],
    );
    fixture.probe.fail_drain.store(true, Ordering::Release);
    let operation = fixture.launch(PublicOperationProgram::Abort, 1).await;
    let failed = completed(&operation).await;
    assert_eq!(failed.stage, PublicOperationStage::ContainmentPending);
    assert!(failed.retry_failure.is_none());
    assert!(matches!(
        failed.disposition,
        Some(PublicOperationDisposition::Failed(_))
    ));
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn declared_stage_repeatability_never_restarts_failed_build() {
    use crate::host::public_build_store::PublicV1BuildRetirement;
    use crate::protocol::public_operations::PublicOperationClass;
    let mut fixture = Fixture::new().await;
    declare(
        &mut fixture,
        [
            Family::ReplicatorRole,
            Family::ApplicationRole,
            Family::Epoch,
            Family::Configuration,
            Family::CatchUp,
        ],
    );
    let role = fixture
        .launch(
            PublicOperationProgram::Role {
                epoch: Epoch::new(0, 2),
                role: ReplicaRole::Primary,
            },
            1,
        )
        .await;
    assert_eq!(
        completed(&role).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    let configuration = configuration(&fixture).await;
    let installed = fixture
        .launch(
            PublicOperationProgram::Configuration(configuration.clone()),
            2,
        )
        .await;
    assert_eq!(
        completed(&installed).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    let target = ReplicaIdentity {
        replica_id: ReplicaId::new(2),
        instance_id: ReplicaInstanceId::new("build-target"),
        agent_generation: AgentGeneration::new("build-target-generation"),
    };
    let build = PublicBuildInput {
        attempt: OperationId::new("failed-build"),
        epoch: configuration.current.epoch,
        current_configuration_id: Some(configuration.current.configuration_id),
        replica: target.clone(),
        target_storage: ReplicaStorageIncarnationContext {
            replica: target,
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new("build-target"),
                pvc_uid: PvcUid::new("build-target-pvc"),
            },
            storage_generation: StorageGeneration::new("build-target-storage"),
        },
        process_session_id: ProcessSessionId::new("build-target-session"),
        source_address: "http://127.0.0.1:1".into(),
        replication_address: "http://127.0.0.1:2".into(),
        history: fixture
            .store
            .load_state()
            .await
            .unwrap()
            .public_v1_operation_state
            .unwrap()
            .history,
    };
    let mut intent = fixture.intent(PublicOperationProgram::Build(build.clone()), 2);
    intent.operation_id = build.attempt;
    intent.class = PublicOperationClass::Build {
        target: build.replica.replica_id,
    };
    fixture.probe.fail_build.store(true, Ordering::Release);
    let operation = launch(
        &fixture.registry,
        fixture.store.clone(),
        intent.clone(),
        fixture.callbacks.clone(),
    )
    .await
    .unwrap();
    let failed = completed(&operation).await;
    assert_eq!(failed.stage, PublicOperationStage::ContainmentPending);
    assert!(failed.retry_failure.is_none());
    let state = fixture
        .store
        .load_state()
        .await
        .unwrap()
        .public_v1_operation_state
        .unwrap();
    assert_eq!(
        state.build_records[&intent.operation_id].retirement,
        PublicV1BuildRetirement::Required
    );
    fixture.probe.fail_build.store(false, Ordering::Release);
    let replay = launch(
        &fixture.registry,
        fixture.store.clone(),
        intent,
        fixture.callbacks.clone(),
    )
    .await
    .unwrap();
    assert_eq!(replay.snapshot().attempt, 1);
    assert_eq!(
        fixture
            .probe
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.as_str() == "build")
            .count(),
        1
    );
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn schema13_current_cancelled_and_partial_records_reject_without_changing_provider_bytes() {
    for boundary in ["current-only", "cancelled-catchup", "partial-role"] {
        let fixture = Fixture::new().await;
        match boundary {
            "current-only" => {
                let operation = fixture
                    .launch(
                        PublicOperationProgram::Configuration(configuration(&fixture).await),
                        1,
                    )
                    .await;
                assert_eq!(
                    completed(&operation).await.disposition,
                    Some(PublicOperationDisposition::Succeeded)
                );
            }
            "cancelled-catchup" => {
                fixture.probe.block_catchup.store(true, Ordering::Release);
                let operation = fixture
                    .launch(
                        PublicOperationProgram::CatchUp {
                            configuration: configuration(&fixture).await,
                            mode: PublicCatchUpMode::WriteQuorum,
                        },
                        1,
                    )
                    .await;
                fixture.probe.catchup_entered.notified().await;
                operation.cancel_root().await.unwrap();
            }
            _ => {
                fixture
                    .probe
                    .fail_application_role
                    .store(true, Ordering::Release);
                let operation = fixture
                    .launch(
                        PublicOperationProgram::Role {
                            epoch: Epoch::new(0, 2),
                            role: ReplicaRole::ActiveSecondary,
                        },
                        1,
                    )
                    .await;
                assert_eq!(
                    completed(&operation).await.lifecycle.outcomes,
                    [PublicInstructionOutcome::Done]
                );
            }
        }
        fixture.registry.shutdown().await.unwrap();
        let state = fixture.store.load_state().await.unwrap();
        let binding = state
            .public_v1_operation_state
            .as_ref()
            .unwrap()
            .binding
            .clone()
            .unwrap();
        let mut old = serde_json::to_value(&state).unwrap();
        old["identity"]["schemaVersion"] = serde_json::json!(13);
        for record in old["publicV1OperationState"]["operations"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            let record = record.as_object_mut().unwrap();
            record.remove("attempt");
            record.remove("attemptResults");
            record.remove("retryFailure");
        }
        let connection = rusqlite::Connection::open(fixture.store.path()).unwrap();
        connection
            .execute(
                "UPDATE agent_state SET state_json = ?1 WHERE singleton = 1",
                [old.to_string()],
            )
            .unwrap();
        connection.pragma_update(None, "user_version", 13).unwrap();
        drop(connection);
        let provider = fixture._root.path().join("provider.bytes");
        let application = fixture._root.path().join("application.bytes");
        std::fs::write(&provider, [0, 255, 17, 42, 0]).unwrap();
        std::fs::write(&application, b"application-owned retained state").unwrap();
        let database_before = std::fs::read(fixture.store.path()).unwrap();
        let provider_before = std::fs::read(&provider).unwrap();
        let application_before = std::fs::read(&application).unwrap();
        assert!(
            matches!(
                SqliteStore::open_replacement_bound_existing(
                    fixture.store.path(),
                    Some(&state.identity),
                    &binding
                ),
                Err(HostError::SchemaMismatch {
                    expected: 14,
                    observed: 13
                })
            ),
            "{boundary}"
        );
        assert_eq!(
            std::fs::read(fixture.store.path()).unwrap(),
            database_before
        );
        assert_eq!(std::fs::read(provider).unwrap(), provider_before);
        assert_eq!(std::fs::read(application).unwrap(), application_before);
    }
}
