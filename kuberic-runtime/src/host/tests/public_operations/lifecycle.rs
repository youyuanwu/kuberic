use super::*;
use crate::host::state::{DataLossOutcome, PublicInstruction, PublicInstructionOutcome};
use crate::protocol::public_operations::{
    PossibleDataLossIntent, PublicLifecycleInput, PublicLifecycleRecipe, PublicLifecycleReport,
};
use kuberic_controller::evaluator::test_bridge::{
    PreviewAcceptedStatus, PreviewServiceApi, PreviewServiceLocationPlan,
    PreviewServiceLocationStage, PreviewTransition, PreviewWriteService,
    PublicOperationPreviewEvaluationConfig, evaluate_service_location,
    execute_preview_service_location, plan_public_lifecycle, preview_service_matches,
    preview_service_update,
};

fn wire<T: serde::Serialize, U: serde::de::DeserializeOwned>(value: T) -> U {
    serde_json::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap()
}

struct Fixture {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    store: Arc<SqliteStore>,
    preview: PublicOperationPreviewIdentity,
    owner: PublicOperationPreviewRuntime,
    host: Arc<PodRuntime>,
    trace: Arc<Trace>,
}

impl Fixture {
    async fn new(role: ReplicaRole) -> Self {
        let preview = PublicOperationPreviewIdentity::new(420);
        let (directory, path, store) = durable_preview_store(&preview);
        let (trace, application, _, _) = trace_fixture();
        let host = Arc::new(PodRuntime::new(
            replica_identity(),
            application,
            store.clone(),
        ));
        let owner = PublicOperationPreviewRuntime::start(
            store.clone(),
            preview.clone(),
            ProcessSessionId::new("session-1"),
        )
        .unwrap();
        host.bind_public_fault_preview(
            store.clone(),
            owner.registry(),
            preview.clone(),
            ProcessSessionId::new("session-1"),
        )
        .await
        .unwrap();
        let opening = host.clone();
        owner
            .run_root(
                intent(
                    &preview,
                    "open",
                    1,
                    PublicOperationClass::Authority,
                    "session-1",
                ),
                CallbackContainment::ObjectOwnedOnInterruption,
                async move {
                    opening
                        .reconstruct(
                            crate::application::OpenMode::New,
                            role,
                            AccessStatus::NotPrimary,
                            AccessStatus::NotPrimary,
                            None,
                        )
                        .await
                },
            )
            .await
            .unwrap();
        trace.events.lock().unwrap().clear();
        trace.arguments.lock().unwrap().clear();
        Self {
            _directory: directory,
            path,
            store,
            preview,
            owner,
            host,
            trace,
        }
    }

    fn config(&self) -> PublicOperationPreviewEvaluationConfig {
        PublicOperationPreviewEvaluationConfig::new(wire(&self.preview), Default::default())
    }

    fn command(
        &self,
        recipe: PublicLifecycleRecipe,
        revision: u64,
        transition: PreviewTransition,
    ) -> PublicOperationIntent {
        let primary = if recipe == PublicLifecycleRecipe::SecondaryEpochAdvance {
            2
        } else {
            1
        };
        let mut members = vec![ConfigurationMember {
            identity: replica_identity(),
            role: if primary == 1 {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
        }];
        if primary == 2 {
            let mut other = replica_identity();
            other.replica_id = ReplicaId::new(2);
            members.push(ConfigurationMember {
                identity: other,
                role: ReplicaRole::Primary,
            });
        }
        let epoch = Epoch::new(57, revision as i64);
        let current = ConfigurationDescriptor::new(epoch, ReplicaId::new(primary), members, 1);
        let previous = Some(ConfigurationDescriptor::new(
            Epoch::new(56, revision as i64 - 1),
            ReplicaId::new(primary),
            current.members.clone(),
            1,
        ));
        wire(
            plan_public_lifecycle(
                &self.config(),
                wire(OperationId::new(format!("role-{revision}"))),
                revision,
                wire(ProcessSessionId::new("session-1")),
                wire(PublicLifecycleInput {
                    recipe,
                    replica: replica_identity(),
                    epoch,
                    // The planner must replace even a malicious ordinary caller's value.
                    possible_data_loss: PossibleDataLossIntent::Possible,
                    current,
                    previous,
                }),
                transition,
            )
            .unwrap(),
        )
    }

    async fn run(
        &self,
        command: PublicOperationIntent,
    ) -> crate::host::state::PublicOperationRecord {
        let operation = self
            .owner
            .launch_lifecycle(command, &self.host)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), operation.wait_for_terminal())
            .await
            .unwrap()
            .unwrap()
    }

    fn begins(&self) -> Vec<String> {
        self.trace
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.ends_with(".begin") && !event.starts_with("provider."))
            .cloned()
            .collect()
    }

    async fn reopened_report(&self, session: &str) -> PublicLifecycleReport {
        let store = SqliteStore::open_preview_existing(&self.path, None, &self.preview).unwrap();
        crate::host::public_lifecycle::report(
            &store,
            &self.preview,
            &ProcessSessionId::new(session),
        )
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn exact_initial_failover_and_secondary_recipes() {
    use PublicLifecycleRecipe::*;
    for (recipe, role, expected) in [
        (
            InitialPrimary,
            ReplicaRole::None,
            vec![
                "replicator.change_role.begin",
                "application.change_role.begin",
                "primary.update_current_configuration.begin",
            ],
        ),
        (
            FailoverPromotion,
            ReplicaRole::ActiveSecondary,
            vec![
                "replicator.change_role.begin",
                "replicator.update_epoch.begin",
                "application.change_role.begin",
                "primary.update_catch_up_configuration.begin",
                "primary.wait_for_catch_up.begin",
            ],
        ),
        (
            SecondaryEpochAdvance,
            ReplicaRole::ActiveSecondary,
            vec!["replicator.update_epoch.begin"],
        ),
    ] {
        let fixture = Fixture::new(role).await;
        *fixture.trace.service_address.lock().unwrap() = Some("opaque://app/ α?x=1".into());
        let command = fixture.command(recipe, 2, PreviewTransition::Ordinary);
        assert_eq!(
            command.lifecycle.as_ref().unwrap().possible_data_loss,
            PossibleDataLossIntent::NotPossible
        );
        let result = fixture.run(command.clone()).await;
        assert_eq!(
            result.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
        assert_eq!(fixture.begins(), expected, "{recipe:?}");
        if recipe == SecondaryEpochAdvance {
            assert_eq!(
                result.lifecycle.outcomes,
                vec![PublicInstructionOutcome::Done]
            );
        }

        fixture.run(command.clone()).await;
        assert_eq!(
            fixture.begins(),
            expected,
            "exact replay reinvoked callbacks"
        );
        let report = fixture.owner.lifecycle_report().await.unwrap();
        assert_eq!(report.write_access, recipe != SecondaryEpochAdvance);
        assert_eq!(report, fixture.reopened_report("session-1").await);
        if recipe != SecondaryEpochAdvance {
            let address = report.service_location.unwrap();
            assert_eq!(address.address, "opaque://app/ α?x=1");
            assert_ne!(address.address, "trace://replicator");
            assert_eq!(address.epoch, command.lifecycle.as_ref().unwrap().epoch);
            assert_eq!(address.operation_id, command.operation_id);
        }
        let arguments = fixture.trace.arguments.lock().unwrap().clone();
        let epochs = arguments
            .iter()
            .filter(|arg| arg.starts_with("replicator.epoch:"))
            .collect::<Vec<_>>();
        assert_eq!(epochs.len(), usize::from(recipe != InitialPrimary));
        if let Some(epoch) = epochs.first() {
            assert!(epoch.contains(&format!("{:?}", command.lifecycle.unwrap().epoch)));
        }
        let fresh = fixture.reopened_report("fresh-session").await;
        assert_eq!(fresh.role, ReplicaRole::None);
        assert!(!fresh.write_access && fresh.service_location.is_none());
        let fresh_registry = PartitionOperationRegistry::new(
            fixture.store.clone(),
            fixture.preview.clone(),
            ProcessSessionId::new("fresh-session"),
        )
        .unwrap();
        let mut fresh_command = fixture.command(recipe, 3, PreviewTransition::Ordinary);
        fresh_command.process_session_id = ProcessSessionId::new("fresh-session");
        assert!(fresh_registry.admit(fresh_command).await.is_err());
        fresh_registry.shutdown().await.unwrap();
        // Preview publication never activates the legacy/custom authority or access path.
        assert_ne!(
            fixture.host.snapshot().await.write_status,
            AccessStatus::Granted
        );
        assert!(
            fixture
                .store
                .load_admitted_authority()
                .await
                .unwrap()
                .is_none()
        );
        fixture.owner.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn public_fault_reporting_closes_access_and_location_before_returning() {
    let fixture = Fixture::new(ReplicaRole::None).await;
    *fixture.trace.service_address.lock().unwrap() = Some("opaque://fault-serving".into());
    fixture
        .run(fixture.command(
            PublicLifecycleRecipe::InitialPrimary,
            2,
            PreviewTransition::Ordinary,
        ))
        .await;
    let serving = crate::host::public_lifecycle::report(
        fixture.store.as_ref(),
        &fixture.preview,
        &ProcessSessionId::new("session-1"),
    )
    .await
    .unwrap();
    assert_eq!(serving.role, ReplicaRole::Primary);
    assert!(serving.write_access);
    assert!(serving.service_location.is_some());

    let partition = fixture
        .host
        .public_open_context(crate::application::OpenMode::Existing)
        .await
        .partition;
    partition
        .report_fault(crate::protocol::types::FaultType::Transient)
        .await
        .unwrap();

    let closed = crate::host::public_lifecycle::report(
        fixture.store.as_ref(),
        &fixture.preview,
        &ProcessSessionId::new("session-1"),
    )
    .await
    .unwrap();
    assert_eq!(closed.role, ReplicaRole::None);
    assert!(!closed.write_access);
    assert!(closed.service_location.is_none());
    let state = fixture.store.load_state().await.unwrap();
    assert_eq!(
        state.reported_fault,
        Some(crate::protocol::types::FaultType::Transient)
    );
    let preview = state.public_operation_preview.unwrap();
    assert!(preview.writes_revoked);
    assert!(preview.terminal);

    assert_eq!(
        partition.get_read_status().await.unwrap(),
        AccessStatus::NotPrimary
    );
    assert_eq!(
        partition.get_write_status().await.unwrap(),
        AccessStatus::NotPrimary
    );
    let transient = preview.current_operation.clone().unwrap();
    fixture
        .owner
        .registry()
        .operation(&transient)
        .await
        .unwrap()
        .wait_for_terminal()
        .await
        .unwrap();
    let operation_count = preview.operations.len();
    partition
        .report_fault(crate::protocol::types::FaultType::Transient)
        .await
        .unwrap();
    let repeated = fixture.store.load_state().await.unwrap();
    let repeated = repeated.public_operation_preview.unwrap();
    assert_eq!(repeated.current_operation.as_ref(), Some(&transient));
    assert_eq!(repeated.operations.len(), operation_count);
    partition
        .report_fault(crate::protocol::types::FaultType::Permanent)
        .await
        .unwrap();
    let state = fixture.store.load_state().await.unwrap();
    let preview = state.public_operation_preview.unwrap();
    let permanent = preview.current_operation.clone().unwrap();
    fixture
        .owner
        .registry()
        .operation(&permanent)
        .await
        .unwrap()
        .wait_for_terminal()
        .await
        .unwrap();
    let rejected = fixture
        .owner
        .registry()
        .admit(intent(
            &fixture.preview,
            "authority-after-fault",
            5,
            PublicOperationClass::Authority,
            "session-1",
        ))
        .await;
    assert!(rejected.is_err());
    let current = preview
        .operations
        .get(preview.current_operation.as_ref().unwrap())
        .unwrap();
    assert_eq!(current.intent.class, PublicOperationClass::PermanentFault);
    fixture.owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn possible_data_loss_outcomes_and_barriers_survive_reopen() {
    for recipe in [
        PublicLifecycleRecipe::InitialPrimary,
        PublicLifecycleRecipe::FailoverPromotion,
    ] {
        for outcome in [Ok(false), Ok(true), Err("provider failed".to_string())] {
            let fixture = Fixture::new(ReplicaRole::ActiveSecondary).await;
            *fixture.trace.data_loss.lock().unwrap() = Some(outcome.clone());
            *fixture.trace.service_address.lock().unwrap() = Some("must-not-publish".into());
            let command = fixture.command(recipe, 2, PreviewTransition::PossibleDataLoss);
            let result = fixture.run(command.clone()).await;
            let expected = match outcome {
                Ok(false) => DataLossOutcome::False,
                Ok(true) => DataLossOutcome::True,
                Err(ref error) => {
                    assert_eq!(result.stage, PublicOperationStage::ContainmentPending);
                    DataLossOutcome::Error(
                        crate::RuntimeError::Application(error.clone()).to_string(),
                    )
                }
            };
            let reopened = Arc::new(
                SqliteStore::open_preview_existing(&fixture.path, None, &fixture.preview).unwrap(),
            );
            let state = reopened.load_state().await.unwrap();
            let preview = state.public_operation_preview.unwrap();
            assert_eq!(
                preview.history_barriers[&command.operation_id].outcome,
                expected
            );
            assert_eq!(
                preview.history_barriers[&command.operation_id].intent,
                command
            );
            assert_eq!(
                fixture.begins().last().unwrap(),
                "primary.on_data_loss.begin"
            );
            let mut expected_trace = vec!["replicator.change_role.begin"];
            if recipe == PublicLifecycleRecipe::FailoverPromotion {
                expected_trace.push("replicator.update_epoch.begin");
            }
            expected_trace.extend([
                "application.change_role.begin",
                "primary.on_data_loss.begin",
            ]);
            assert_eq!(fixture.begins(), expected_trace);
            assert!(
                !fixture
                    .begins()
                    .iter()
                    .any(|name| name.contains("configuration") || name.contains("catch_up"))
            );
            let report = fixture.reopened_report("session-1").await;
            assert!(!report.write_access && report.service_location.is_none());
            let registry = PartitionOperationRegistry::new(
                reopened.clone(),
                fixture.preview.clone(),
                ProcessSessionId::new("session-1"),
            )
            .unwrap();
            let ordinary = fixture.command(recipe, 3, PreviewTransition::Ordinary);
            assert!(registry.admit(ordinary.clone()).await.is_err());
            // The durable store rejects bypasses of the registry too.
            assert!(
                reopened
                    .begin_public_operation(&ordinary, &[], &[])
                    .await
                    .is_err()
            );
            let mut changed = command.clone();
            changed.lifecycle.as_mut().unwrap().possible_data_loss =
                PossibleDataLossIntent::NotPossible;
            changed.input_digest = changed.lifecycle.as_ref().unwrap().digest();
            assert!(registry.admit(changed).await.is_err());
            for class in [
                PublicOperationClass::Authority,
                PublicOperationClass::PlannedSwap,
                PublicOperationClass::Build {
                    target: ReplicaId::new(2),
                },
            ] {
                assert!(
                    registry
                        .admit(intent(
                            &fixture.preview,
                            &format!("bypass-{class:?}"),
                            10,
                            class,
                            "session-1"
                        ))
                        .await
                        .is_err()
                );
            }
            assert_eq!(state.role, ReplicaRole::None);
            assert!(
                state.current_configuration.is_none() && state.previous_configuration.is_none()
            );
            assert!(state.build_progress.is_empty());
            registry.shutdown().await.unwrap();
            fixture.owner.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn success_before_journal_and_live_cancellation_are_ambiguous_not_history_admission() {
    for journal_loss in [false, true] {
        let fixture = Fixture::new(ReplicaRole::ActiveSecondary).await;
        let release = fixture.trace.block("provider.on_data_loss");
        *fixture.trace.data_loss.lock().unwrap() = Some(Ok(true));
        let command = fixture.command(
            PublicLifecycleRecipe::FailoverPromotion,
            2,
            PreviewTransition::PossibleDataLoss,
        );
        let operation = fixture
            .owner
            .launch_lifecycle(command.clone(), &fixture.host)
            .await
            .unwrap();
        fixture.trace.wait_for("provider.on_data_loss.begin").await;
        assert!(!fixture.owner.lifecycle_report().await.unwrap().write_access);
        if journal_loss {
            fixture.store.fail_next_public_operation_advance();
            release.notify_one();
            fixture.trace.wait_for("provider.on_data_loss.end").await;
        } else {
            let ordinary = fixture.command(
                PublicLifecycleRecipe::FailoverPromotion,
                3,
                PreviewTransition::Ordinary,
            );
            assert!(fixture.owner.registry().admit(ordinary).await.is_err());
        }
        wait_for_stage(&operation, PublicOperationStage::ContainmentPending).await;
        let reopened =
            SqliteStore::open_preview_existing(&fixture.path, None, &fixture.preview).unwrap();
        let preview = reopened
            .load_state()
            .await
            .unwrap()
            .public_operation_preview
            .unwrap();
        assert_eq!(
            preview.history_barriers[&command.operation_id].outcome,
            DataLossOutcome::Ambiguous
        );
        assert!(
            fixture
                .begins()
                .iter()
                .all(|name| !name.contains("configuration") && !name.contains("catch_up"))
        );
        assert!(!fixture.reopened_report("new-session").await.write_access);
        fixture.owner.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn terminal_operations_retire_but_never_clear_history_barriers() {
    for class in [
        PublicOperationClass::Close,
        PublicOperationClass::Abort,
        PublicOperationClass::TransientFault,
        PublicOperationClass::PermanentFault,
    ] {
        let fixture = Fixture::new(ReplicaRole::None).await;
        let command = fixture.command(
            PublicLifecycleRecipe::InitialPrimary,
            2,
            PreviewTransition::PossibleDataLoss,
        );
        fixture.run(command.clone()).await;
        let terminal = intent(&fixture.preview, "terminal", 3, class, "session-1");
        fixture
            .owner
            .registry()
            .admit(terminal.clone())
            .await
            .unwrap();
        let state = fixture
            .store
            .load_state()
            .await
            .unwrap()
            .public_operation_preview
            .unwrap();
        let barrier = &state.history_barriers[&command.operation_id];
        assert_eq!(barrier.retired_by.as_ref(), Some(&terminal.operation_id));
        assert_eq!(barrier.outcome, DataLossOutcome::False);
        assert!(!fixture.reopened_report("session-1").await.write_access);
        assert!(
            fixture
                .owner
                .registry()
                .admit(fixture.command(
                    PublicLifecycleRecipe::InitialPrimary,
                    4,
                    PreviewTransition::Ordinary
                ))
                .await
                .is_err()
        );
        fixture.owner.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn stale_failed_and_incomplete_role_results_cannot_publish() {
    for case in 0..3 {
        let fixture = Fixture::new(ReplicaRole::None).await;
        *fixture.trace.service_address.lock().unwrap() = Some("late-address".into());
        fixture.trace.fail_role.store(case == 1, Ordering::Release);
        let release = fixture.trace.block("application.change_role");
        let command = fixture.command(
            PublicLifecycleRecipe::InitialPrimary,
            2,
            PreviewTransition::Ordinary,
        );
        let operation = fixture
            .owner
            .launch_lifecycle(command.clone(), &fixture.host)
            .await
            .unwrap();
        fixture
            .trace
            .wait_for("application.change_role.begin")
            .await;
        assert!(
            fixture
                .owner
                .lifecycle_report()
                .await
                .unwrap()
                .service_location
                .is_none()
        );
        if case == 2 {
            fixture.store.fail_next_public_operation_advance();
            release.notify_one();
        } else if case == 1 {
            release.notify_one();
        } else {
            fixture
                .owner
                .registry()
                .admit(intent(
                    &fixture.preview,
                    "close",
                    3,
                    PublicOperationClass::Close,
                    "session-1",
                ))
                .await
                .unwrap();
        }
        wait_for_stage(&operation, PublicOperationStage::ContainmentPending).await;
        assert!(
            fixture
                .store
                .public_instruction(
                    &command,
                    1,
                    PublicInstruction::ApplicationPrimary,
                    Some(PublicInstructionOutcome::ApplicationRole(Some(
                        "late-address".into()
                    )))
                )
                .await
                .is_err()
        );
        assert!(
            fixture
                .reopened_report("session-1")
                .await
                .service_location
                .is_none()
        );
        fixture.owner.shutdown().await.unwrap();
    }
}

struct UnavailableKubernetes;

#[async_trait]
impl PreviewServiceApi for UnavailableKubernetes {
    async fn observe_preview(
        &self,
    ) -> std::result::Result<(PreviewAcceptedStatus, PreviewWriteService), String> {
        Err("Kubernetes unavailable".into())
    }
    async fn persist_preview_status(
        &self,
        _: &PreviewAcceptedStatus,
        _: &PreviewAcceptedStatus,
    ) -> std::result::Result<(), String> {
        Err("Kubernetes unavailable".into())
    }
    async fn replace_preview_service(
        &self,
        _: PreviewWriteService,
    ) -> std::result::Result<(), String> {
        Err("Kubernetes unavailable".into())
    }
}

#[tokio::test]
async fn public_operation_role_address_terminal_fencing_does_not_wait_for_kubernetes() {
    for class in [
        PublicOperationClass::Close,
        PublicOperationClass::Abort,
        PublicOperationClass::TransientFault,
        PublicOperationClass::PermanentFault,
    ] {
        let fixture = Fixture::new(ReplicaRole::None).await;
        *fixture.trace.service_address.lock().unwrap() = Some("serving-address".into());
        let command = fixture.command(
            PublicLifecycleRecipe::InitialPrimary,
            2,
            PreviewTransition::Ordinary,
        );
        fixture.run(command.clone()).await;
        let live = fixture.owner.lifecycle_report().await.unwrap();
        let mut status = PreviewAcceptedStatus::default();
        let mut service = service();
        converge(&fixture, &command, &live, &mut status, &mut service);
        let terminal = intent(&fixture.preview, "terminal", 3, class, "session-1");
        tokio::time::timeout(
            Duration::from_secs(1),
            fixture.owner.registry().admit(terminal.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        let closed = fixture.owner.lifecycle_report().await.unwrap();
        assert!(!closed.write_access && closed.service_location.is_none());
        let plan = evaluate_service_location(
            &fixture.config(),
            &wire(ResourceUid::new("resource-1")),
            &wire(&terminal),
            Some(&wire(&closed)),
            &status,
            &service,
        )
        .unwrap();
        assert!(
            execute_preview_service_location(&UnavailableKubernetes, plan)
                .await
                .is_err()
        );
        assert!(
            fixture
                .reopened_report("session-1")
                .await
                .service_location
                .is_none()
        );
        converge(&fixture, &terminal, &closed, &mut status, &mut service);
        let PreviewServiceLocationStage::Published(projection) =
            &status.service_location_projection
        else {
            panic!("not cleared")
        };
        assert!(projection.location.is_none() && preview_service_matches(&service, projection));
        fixture.owner.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn frozen_recipe_and_instruction_order_cannot_be_forged() {
    let fixture = Fixture::new(ReplicaRole::None).await;
    let command = fixture.command(
        PublicLifecycleRecipe::InitialPrimary,
        2,
        PreviewTransition::Ordinary,
    );
    let gate = fixture.trace.block("replicator.change_role");
    let operation = fixture
        .owner
        .launch_lifecycle(command.clone(), &fixture.host)
        .await
        .unwrap();
    fixture.trace.wait_for("replicator.change_role.begin").await;
    for instruction in [
        PublicInstruction::CurrentConfiguration,
        PublicInstruction::Access,
        PublicInstruction::DataLoss,
    ] {
        assert!(
            fixture
                .store
                .public_instruction(&command, 0, instruction, None)
                .await
                .is_err()
        );
    }
    for case in 0..5 {
        let mut changed = command.clone();
        match case {
            0 => changed.preview.generation += 1,
            1 => changed.revision += 1,
            2 => changed.process_session_id = ProcessSessionId::new("other"),
            3 => {
                changed.lifecycle.as_mut().unwrap().recipe =
                    PublicLifecycleRecipe::FailoverPromotion
            }
            4 => changed.lifecycle.as_mut().unwrap().epoch = Epoch::new(999, 1),
            _ => unreachable!(),
        }
        changed.input_digest = changed.lifecycle.as_ref().unwrap().digest();
        assert!(
            fixture
                .owner
                .registry()
                .admit(changed.clone())
                .await
                .is_err()
        );
        assert!(
            fixture
                .store
                .public_instruction(
                    &changed,
                    0,
                    PublicInstruction::ReplicatorPrimary,
                    Some(PublicInstructionOutcome::Done)
                )
                .await
                .is_err()
        );
    }
    assert!(
        fixture
            .store
            .advance_public_operation(
                &command.operation_id,
                command.revision,
                &command.process_session_id,
                PublicOperationStage::Running,
                PublicOperationStage::CallbackApplied,
                Some(PublicOperationDisposition::Succeeded)
            )
            .await
            .is_err()
    );
    gate.notify_one();
    operation.wait_for_terminal().await.unwrap();
    assert!(fixture.owner.lifecycle_report().await.unwrap().write_access);
    fixture.owner.shutdown().await.unwrap();
}

fn service() -> PreviewWriteService {
    serde_json::from_value(serde_json::json!({
        "metadata":{"name":"write","uid":"service-1","resourceVersion":"1","annotations":{"unrelated":"keep"}},
        "spec":{"selector":{"operator.kuberic.io/instance":"disabled"}}
    })).unwrap()
}

fn converge(
    fixture: &Fixture,
    authority: &PublicOperationIntent,
    report: &PublicLifecycleReport,
    status: &mut PreviewAcceptedStatus,
    service: &mut PreviewWriteService,
) {
    for _ in 0..8 {
        match evaluate_service_location(
            &fixture.config(),
            &wire(ResourceUid::new("resource-1")),
            &wire(authority),
            Some(&wire(report)),
            status,
            service,
        )
        .unwrap()
        {
            PreviewServiceLocationPlan::PersistStatus { next, .. } => *status = next,
            PreviewServiceLocationPlan::WriteService { projection, .. } => {
                *service = preview_service_update(service, &projection);
                service.metadata.resource_version = Some(format!(
                    "{}+",
                    service.metadata.resource_version.as_ref().unwrap()
                ));
            }
            PreviewServiceLocationPlan::Stable => return,
        }
    }
    panic!("projection did not converge");
}

#[tokio::test]
async fn public_operation_role_address_runtime_to_controller_publish_replace_clear() {
    let fixture = Fixture::new(ReplicaRole::None).await;
    let mut status = PreviewAcceptedStatus::default();
    let mut service = service();
    for (revision, address) in [
        (2, Some("opaque://one")),
        (3, Some("")),
        (4, Some("opaque://two")),
        (5, None),
    ] {
        *fixture.trace.service_address.lock().unwrap() = address.map(str::to_string);
        let command = fixture.command(
            PublicLifecycleRecipe::InitialPrimary,
            revision,
            PreviewTransition::Ordinary,
        );
        fixture.run(command.clone()).await;
        let report = fixture.owner.lifecycle_report().await.unwrap();
        assert_eq!(
            report
                .service_location
                .as_ref()
                .map(|location| location.address.as_str()),
            address
        );
        // The bridge consumes a serialized runtime report, not a manually invented address.
        let wire = serde_json::to_vec(&report).unwrap();
        let report = serde_json::from_slice(&wire).unwrap();
        converge(&fixture, &command, &report, &mut status, &mut service);
        let PreviewServiceLocationStage::Published(projection) =
            &status.service_location_projection
        else {
            panic!("not published")
        };
        assert!(preview_service_matches(&service, projection));
        assert_eq!(
            projection
                .location
                .as_ref()
                .map(|location| location.address.as_str()),
            address
        );
        assert_eq!(
            service.metadata.annotations.as_ref().unwrap()["unrelated"],
            "keep"
        );
    }
    // A pending higher role revision fences both local and accepted publication.
    let command = fixture.command(
        PublicLifecycleRecipe::InitialPrimary,
        6,
        PreviewTransition::Ordinary,
    );
    let _release = fixture.trace.block("application.change_role");
    fixture
        .owner
        .launch_lifecycle(command.clone(), &fixture.host)
        .await
        .unwrap();
    fixture
        .trace
        .wait_for("application.change_role.begin")
        .await;
    let closed = fixture.owner.lifecycle_report().await.unwrap();
    converge(&fixture, &command, &closed, &mut status, &mut service);
    let PreviewServiceLocationStage::Published(projection) = &status.service_location_projection
    else {
        panic!("not cleared")
    };
    assert!(projection.location.is_none());
    fixture.owner.shutdown().await.unwrap();
}
