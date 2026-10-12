use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kuberic_runtime::protocol::command::{PublicV1HistoryCommand, PublicV1SessionRenewalCommand};
use kuberic_runtime::protocol::public_operations::{
    DataLossAttempt, DataLossAuthorization, DataLossRenewalDecision, DataLossReplay,
    PossibleDataLossIntent, ProcessAttemptContext, ProcessSessionRenewalRequired,
    ProviderProgressReset, PublicBuildInput, PublicDataLossRecoveryInput,
    PublicDataLossRecoveryStage, PublicDataLossRecoveryStatus, PublicDataLossSessionRenewal,
    PublicLifecycleInput, PublicLifecycleRecipe, PublicOperationClass, PublicOperationIntent,
    PublicOperationPreviewIdentity, PublicOperationProgram, PublicOperationReplayDisposition,
    PublicProgress, PublicProgressRole, PublicSessionRenewalStage, PublicV1AcceptedStatus,
    PublicV1AgentReport, PublicV1OperationResult, PublicV1SessionRenewalStatus,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, AgentGeneration, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy,
    Epoch, InitializationId, OperationId, PodUid, ProcessSessionEndpoint, ProcessSessionId,
    ProcessSessionRenewalReason, PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity, ReplicaStorageIncarnationContext,
    ReplicatorCreationId, ResourceUid, StorageGeneration, derive_bootstrap_history,
    derive_data_loss_history, derive_history_initialization_id,
};
use kuberic_runtime::testing::sqlite_store::{
    PUBLIC_V1_PREVIEW_SCHEMA_VERSION, PublicV1BuildRetirementSnapshot,
    PublicV1OperationSnapshotDisposition, PublicV1OperationSnapshotStage, PublicV1ReportProjection,
    PublicV1SessionRenewalExecutor, SqliteStore,
};
use kuberic_runtime::testing::state::{SCHEMA_VERSION, StorageIdentity};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct RenewalExecutor {
    events: Mutex<Vec<&'static str>>,
    fail_next: AtomicBool,
}

impl RenewalExecutor {
    fn fail_next(&self) {
        self.fail_next.store(true, Ordering::Release);
    }

    fn record(&self, event: &'static str) -> kuberic_runtime::Result<()> {
        if self.fail_next.swap(false, Ordering::AcqRel) {
            return Err(kuberic_runtime::RuntimeError::Application(format!(
                "{event} failed"
            )));
        }
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

#[async_trait::async_trait]
impl PublicV1SessionRenewalExecutor for RenewalExecutor {
    async fn close_access_and_ingress(&self) -> kuberic_runtime::Result<()> {
        self.record("close")
    }

    async fn update_epoch_barrier(
        &self,
        _survivor: &ProcessSessionEndpoint,
        _epoch: Epoch,
    ) -> kuberic_runtime::Result<()> {
        self.record("epoch")
    }

    async fn replay_replacement_role(
        &self,
        _process_session_id: &ProcessSessionId,
        _epoch: Epoch,
        _role: ReplicaRole,
    ) -> kuberic_runtime::Result<()> {
        self.record("role")
    }

    async fn install_fresh_configuration(
        &self,
        _configuration: &ConfigurationDescriptor,
    ) -> kuberic_runtime::Result<()> {
        self.record("configuration")
    }
}

fn test_root(name: &str) -> PathBuf {
    let root = std::env::current_dir()
        .unwrap()
        .join("target")
        .join("public-v1-session-renewal")
        .join(format!(
            "{name}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn replica(id: i64) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
        agent_generation: AgentGeneration::new(format!("generation-{id}")),
    }
}

fn storage(identity: ReplicaIdentity) -> ReplicaStorageIncarnationContext {
    let id = identity.replica_id.value();
    ReplicaStorageIncarnationContext {
        replica: identity,
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new(format!("pod-{id}")),
            pvc_uid: PvcUid::new(format!("pvc-{id}")),
        },
        storage_generation: StorageGeneration::new(format!("storage-{id}")),
    }
}

fn build_intent(
    preview: &PublicOperationPreviewIdentity,
    history: &kuberic_runtime::protocol::types::HistoryContext,
    target: ReplicaStorageIncarnationContext,
) -> PublicOperationIntent {
    let operation_id = OperationId::new("schema11-build");
    let source_session = ProcessSessionId::new("schema11-source-session");
    let target_session = ProcessSessionId::new("schema11-target-session");
    let program = PublicOperationProgram::Build(PublicBuildInput {
        attempt: operation_id.clone(),
        epoch: Epoch::new(history.data_loss_number, 7),
        replica: target.replica.clone(),
        target_storage: target.clone(),
        process_session_id: target_session,
        replication_address: "https://10.0.0.2:7000".into(),
        history: Some(history.clone()),
    });
    PublicOperationIntent {
        preview: preview.clone(),
        operation_id,
        revision: 1,
        process_session_id: source_session,
        class: PublicOperationClass::Build {
            target: target.replica.replica_id,
        },
        input_digest: program.digest(),
        replay_disposition: PublicOperationReplayDisposition::RetainBuildSuccessRetireAmbiguity,
        lifecycle: None,
        program: Some(program),
    }
}

fn endpoint(id: i64, session: &str, creation: &str, role: ReplicaRole) -> ProcessSessionEndpoint {
    let identity = replica(id);
    ProcessSessionEndpoint {
        storage_incarnation: storage(identity.clone()),
        identity,
        process_session_id: ProcessSessionId::new(session),
        replicator_creation_id: ReplicatorCreationId::new(creation),
        role,
    }
}

fn configuration(
    epoch: Epoch,
    primary: &ProcessSessionEndpoint,
    others: &[ProcessSessionEndpoint],
) -> ConfigurationDescriptor {
    let mut members = vec![ConfigurationMember {
        identity: primary.identity.clone(),
        role: ReplicaRole::Primary,
    }];
    members.extend(others.iter().map(|endpoint| ConfigurationMember {
        identity: endpoint.identity.clone(),
        role: endpoint.role,
    }));
    ConfigurationDescriptor::new(epoch, primary.identity.replica_id, members, 1)
}

fn ordinary_required(previous_epoch: Epoch, renewal_epoch: Epoch) -> ProcessSessionRenewalRequired {
    let resource_uid = ResourceUid::new("resource-1");
    let generation = PublicProtocolGeneration::new(42);
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
    let predecessor = endpoint(1, "session-old", "creation-old", ReplicaRole::Primary);
    let replacement = endpoint(1, "session-new", "creation-new", ReplicaRole::None);
    let survivor = endpoint(
        2,
        "session-secondary",
        "creation-secondary",
        ReplicaRole::ActiveSecondary,
    );
    ProcessSessionRenewalRequired {
        operation_id: OperationId::new("renew-primary"),
        reason: ProcessSessionRenewalReason::PrimaryReplacement,
        resource_uid,
        history,
        previous_epoch,
        renewal_epoch,
        predecessor: predecessor.clone(),
        replacement: Some(replacement),
        surviving_sessions: vec![survivor.clone()],
        fresh_configuration: configuration(renewal_epoch, &predecessor, &[survivor]),
        predecessor_contained: true,
        data_loss: None,
    }
}

fn replacement_report(required: &ProcessSessionRenewalRequired) -> PublicV1AgentReport {
    let replacement = required.replacement.as_ref().unwrap();
    PublicV1AgentReport {
        identity: PublicOperationPreviewIdentity::public_v1(42),
        resource_uid: required.resource_uid.clone(),
        replica: replacement.identity.clone(),
        process_session_id: replacement.process_session_id.clone(),
        report_sequence: 1,
        role: ReplicaRole::None,
        read_status: AccessStatus::NotPrimary,
        write_status: AccessStatus::NotPrimary,
        history: required.history.clone(),
        storage_incarnation: replacement.storage_incarnation.clone(),
        progress: PublicProgress {
            role: PublicProgressRole::Recovery,
            first: 7,
            last: 7,
        },
        service_location: None,
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        extensions: Default::default(),
    }
}

fn replacement_data_loss_status(
    required: &ProcessSessionRenewalRequired,
) -> PublicDataLossRecoveryStatus {
    let renewal = required.data_loss.as_ref().unwrap();
    PublicDataLossRecoveryStatus {
        authorization_id: renewal.authorization.authorization_id.clone(),
        attempt: renewal.replacement_attempt.clone(),
        predecessor_history: renewal.authorization.predecessor.clone(),
        proposed_history: renewal.authorization.proposed.clone(),
        provisional_primary: renewal.authorization.provisional_primary.clone(),
        reset_scope: renewal.authorization.reset_scope.clone(),
        replay_disposition: renewal.authorization.replay_disposition,
        stage: PublicDataLossRecoveryStage::Invalidated,
        observed_progress: None,
    }
}

fn attempt(
    authorization_id: &OperationId,
    attempt_id: &str,
    session: &str,
    creation: &str,
    epoch: Epoch,
    replay: DataLossReplay,
) -> DataLossAttempt {
    DataLossAttempt {
        authorization_id: authorization_id.clone(),
        attempt_id: OperationId::new(attempt_id),
        process: ProcessAttemptContext {
            process_session_id: ProcessSessionId::new(session),
            replicator_creation_id: ReplicatorCreationId::new(creation),
            catch_up_specific_quorum: true,
            data_loss_replay: replay,
            epoch,
            attempt_revision: 1,
        },
    }
}

fn data_loss_required(
    captured: DataLossReplay,
    replacement_capability: DataLossReplay,
) -> ProcessSessionRenewalRequired {
    let resource_uid = ResourceUid::new("resource-1");
    let generation = PublicProtocolGeneration::new(42);
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
    let predecessor = endpoint(1, "session-old", "creation-old", ReplicaRole::Primary);
    let replacement = endpoint(1, "session-new", "creation-new", ReplicaRole::None);
    let authorization_id = OperationId::new("data-loss");
    let authorization = DataLossAuthorization {
        authorization_id: authorization_id.clone(),
        predecessor: history.clone(),
        proposed: derive_data_loss_history(
            &authorization_id,
            &history,
            1,
            &predecessor.storage_incarnation,
        )
        .unwrap(),
        provisional_primary: predecessor.storage_incarnation.clone(),
        reset_scope: ProviderProgressReset::ResetToEmpty,
        replay_disposition: captured,
    };
    let previous_epoch = Epoch::new(1, 10);
    let renewal_epoch = Epoch::new(1, 11);
    ProcessSessionRenewalRequired {
        operation_id: OperationId::new("renew-data-loss"),
        reason: ProcessSessionRenewalReason::DataLossAttempt,
        resource_uid,
        history,
        previous_epoch,
        renewal_epoch,
        predecessor: predecessor.clone(),
        replacement: Some(replacement),
        surviving_sessions: Vec::new(),
        fresh_configuration: configuration(renewal_epoch, &predecessor, &[]),
        predecessor_contained: true,
        data_loss: Some(PublicDataLossSessionRenewal {
            predecessor_attempt: attempt(
                &authorization_id,
                "attempt-old",
                "session-old",
                "creation-old",
                previous_epoch,
                captured,
            ),
            replacement_attempt: attempt(
                &authorization_id,
                "attempt-new",
                "session-new",
                "creation-new",
                renewal_epoch,
                replacement_capability,
            ),
            authorization,
        }),
    }
}

fn recovery_intent(
    preview: &PublicOperationPreviewIdentity,
    authorization: &DataLossAuthorization,
    attempt: &DataLossAttempt,
) -> PublicOperationIntent {
    let local = authorization.provisional_primary.replica.clone();
    let members = vec![ConfigurationMember {
        identity: local.clone(),
        role: ReplicaRole::Primary,
    }];
    let lifecycle = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::InitialPrimary,
        replica: local.clone(),
        epoch: attempt.process.epoch,
        possible_data_loss: PossibleDataLossIntent::Possible,
        current: ConfigurationDescriptor::new(
            attempt.process.epoch,
            local.replica_id,
            members.clone(),
            1,
        ),
        previous: Some(ConfigurationDescriptor::new(
            Epoch::new(
                attempt.process.epoch.data_loss_number,
                attempt.process.epoch.configuration_number - 1,
            ),
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
    PublicOperationIntent {
        preview: preview.clone(),
        operation_id: attempt.attempt_id.clone(),
        revision: attempt.process.attempt_revision,
        process_session_id: attempt.process.process_session_id.clone(),
        class: PublicOperationClass::Authority,
        input_digest: lifecycle.digest(),
        replay_disposition: PublicOperationReplayDisposition::DataLoss(
            authorization.replay_disposition,
        ),
        lifecycle: Some(lifecycle),
        program: None,
    }
}

fn accepted_recovery(
    preview: &PublicOperationPreviewIdentity,
    initialization_id: kuberic_runtime::protocol::types::HistoryInitializationId,
    authorization: &DataLossAuthorization,
    attempt: &DataLossAttempt,
) -> PublicV1AcceptedStatus {
    PublicV1AcceptedStatus {
        identity: preview.clone(),
        history_initialization_id: initialization_id,
        history: authorization.predecessor.clone(),
        data_loss_recovery: Some(PublicDataLossRecoveryStatus {
            authorization_id: authorization.authorization_id.clone(),
            attempt: attempt.clone(),
            predecessor_history: authorization.predecessor.clone(),
            proposed_history: authorization.proposed.clone(),
            provisional_primary: authorization.provisional_primary.clone(),
            reset_scope: authorization.reset_scope.clone(),
            replay_disposition: authorization.replay_disposition,
            stage: PublicDataLossRecoveryStage::Invalidated,
            observed_progress: None,
        }),
        completed_operations: Vec::new(),
        extensions: Default::default(),
    }
}

#[tokio::test]
async fn public_v1_session_renewal_requires_higher_epoch() {
    let required = ordinary_required(Epoch::new(0, 10), Epoch::new(0, 11));
    required.validate().unwrap();
    required
        .validate_replacement_report(&replacement_report(&required))
        .unwrap();

    let mut same_epoch = required.clone();
    same_epoch.renewal_epoch = same_epoch.previous_epoch;
    same_epoch.fresh_configuration = configuration(
        same_epoch.renewal_epoch,
        &same_epoch.predecessor,
        &same_epoch.surviving_sessions,
    );
    assert!(same_epoch.validate().is_err());

    let root = test_root("higher-epoch");
    let path = SqliteStore::metadata_database_path(&root);
    let preview = PublicOperationPreviewIdentity::public_v1(42);
    let store = SqliteStore::create_public_v1(
        &path,
        preview.clone(),
        StorageIdentity {
            schema_version: SCHEMA_VERSION,
            resource_uid: required.resource_uid.clone(),
            pod_uid: PodUid::new("pod-1"),
            pvc_uid: PvcUid::new("pvc-1"),
            initialization_id: InitializationId::new("local-initialization"),
            local_identity: required.predecessor.identity.clone(),
            effective_policy: EffectivePolicy::fixed(2, 30).unwrap(),
        },
    )
    .unwrap();
    let initialization = derive_history_initialization_id(
        &required.resource_uid,
        required.history.protocol_generation,
    );
    store
        .initialize_public_v1_history(
            &initialization,
            &required.history,
            &required.predecessor.storage_incarnation,
        )
        .await
        .unwrap();

    let accepted = PublicV1SessionRenewalStatus::required(required.clone()).unwrap();
    let executor = RenewalExecutor::default();
    store
        .persist_public_v1_session_renewal(None, &accepted)
        .await
        .unwrap();
    executor.fail_next();
    assert!(
        store
            .apply_public_v1_session_renewal_command(
                &PublicV1SessionRenewalCommand::CloseAccessAndIngress {
                    accepted: accepted.clone(),
                },
                &executor,
            )
            .await
            .is_err()
    );
    assert_eq!(
        store
            .public_v1_history_snapshot()
            .await
            .unwrap()
            .session_renewal,
        Some(accepted.clone())
    );
    let closed = store
        .apply_public_v1_session_renewal_command(
            &PublicV1SessionRenewalCommand::CloseAccessAndIngress {
                accepted: accepted.clone(),
            },
            &executor,
        )
        .await
        .unwrap();
    let frozen = closed.freeze_surviving_set().unwrap();
    store
        .persist_public_v1_session_renewal(Some(&closed), &frozen)
        .await
        .unwrap();
    let barriers = frozen.begin_epoch_barriers().unwrap();
    store
        .persist_public_v1_session_renewal(Some(&frozen), &barriers)
        .await
        .unwrap();
    assert!(
        barriers
            .replay_replacement_role(
                &required.replacement.as_ref().unwrap().process_session_id,
                required.renewal_epoch,
                ReplicaRole::Primary,
            )
            .is_err()
    );
    let updated = store
        .apply_public_v1_session_renewal_command(
            &PublicV1SessionRenewalCommand::UpdateEpochBarrier {
                accepted: barriers.clone(),
                survivor: required.surviving_sessions[0].clone(),
                epoch: required.renewal_epoch,
            },
            &executor,
        )
        .await
        .unwrap();
    let role_pending = updated.finish_epoch_barriers().unwrap();
    store
        .persist_public_v1_session_renewal(Some(&updated), &role_pending)
        .await
        .unwrap();
    let configuration_pending = store
        .apply_public_v1_session_renewal_command(
            &PublicV1SessionRenewalCommand::ReplayReplacementRole {
                accepted: role_pending,
                process_session_id: required
                    .replacement
                    .as_ref()
                    .unwrap()
                    .process_session_id
                    .clone(),
                epoch: required.renewal_epoch,
                role: ReplicaRole::Primary,
            },
            &executor,
        )
        .await
        .unwrap();
    let authorized = store
        .apply_public_v1_session_renewal_command(
            &PublicV1SessionRenewalCommand::InstallFreshConfiguration {
                accepted: configuration_pending,
                configuration: required.fresh_configuration.clone(),
            },
            &executor,
        )
        .await
        .unwrap();
    assert_eq!(authorized.stage, PublicSessionRenewalStage::Authorized);
    assert_eq!(
        executor.events.lock().unwrap().as_slice(),
        ["close", "epoch", "role", "configuration"]
    );
    drop(store);

    let reopened = SqliteStore::open_public_v1(&path, &preview).unwrap();
    assert_eq!(
        reopened
            .public_v1_history_snapshot()
            .await
            .unwrap()
            .session_renewal,
        Some(authorized)
    );
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn ordinary_replacement_report_validates_and_round_trips_without_data_loss_authority() {
    let required = ordinary_required(Epoch::new(0, 10), Epoch::new(0, 11));
    let report = replacement_report(&required);

    report.validate().unwrap();
    required.validate_replacement_report(&report).unwrap();
    assert_eq!(
        kuberic_runtime::control::normalize_public_v1_agent_status_report(
            kuberic_runtime::control::public_v1_agent_status_to_proto(&report),
        )
        .unwrap(),
        report
    );

    let mut assigned_recovery = report.clone();
    assigned_recovery.role = ReplicaRole::ActiveSecondary;
    assert!(assigned_recovery.validate().is_err());
    assert!(
        kuberic_runtime::control::normalize_public_v1_agent_status_report(
            kuberic_runtime::control::public_v1_agent_status_to_proto(&assigned_recovery),
        )
        .is_err()
    );

    let mut serving_recovery = report.clone();
    serving_recovery.read_status = AccessStatus::Granted;
    assert!(serving_recovery.validate().is_err());

    let data_loss_required =
        data_loss_required(DataLossReplay::Convergent, DataLossReplay::Convergent);
    let mut data_loss_observation = report.clone();
    data_loss_observation.data_loss_recovery =
        Some(replacement_data_loss_status(&data_loss_required));
    data_loss_observation.validate().unwrap();
    assert!(
        required
            .validate_replacement_report(&data_loss_observation)
            .is_err()
    );

    data_loss_observation
        .data_loss_recovery
        .as_mut()
        .unwrap()
        .authorization_id = OperationId::new("wrong-authorization");
    assert!(data_loss_observation.validate().is_err());
    assert!(
        kuberic_runtime::control::normalize_public_v1_agent_status_report(
            kuberic_runtime::control::public_v1_agent_status_to_proto(&data_loss_observation),
        )
        .is_err()
    );
}

#[tokio::test]
async fn public_v1_data_loss_attempt_renews_same_authorization() {
    let required = data_loss_required(DataLossReplay::Convergent, DataLossReplay::Convergent);
    required.validate().unwrap();
    let renewal = required.data_loss.as_ref().unwrap();
    assert_eq!(
        renewal.authorization.authorization_id,
        renewal.predecessor_attempt.authorization_id
    );
    assert_eq!(
        renewal.authorization.authorization_id,
        renewal.replacement_attempt.authorization_id
    );
    assert_eq!(renewal.authorization.predecessor, required.history);
    assert_eq!(
        renewal.authorization.provisional_primary,
        required.predecessor.storage_incarnation
    );
    assert_eq!(
        required.data_loss_decision(),
        Some(DataLossRenewalDecision::InvokeCallback)
    );

    let mut changed_storage = required.clone();
    changed_storage
        .predecessor
        .storage_incarnation
        .storage_generation = StorageGeneration::new("other-storage");
    assert!(changed_storage.validate().is_err());

    let root = test_root("data-loss-renewal");
    let path = SqliteStore::metadata_database_path(&root);
    let preview = PublicOperationPreviewIdentity::public_v1(42);
    let store = SqliteStore::create_public_v1(
        &path,
        preview.clone(),
        StorageIdentity {
            schema_version: SCHEMA_VERSION,
            resource_uid: required.resource_uid.clone(),
            pod_uid: PodUid::new("pod-1"),
            pvc_uid: PvcUid::new("pvc-1"),
            initialization_id: InitializationId::new("local-initialization"),
            local_identity: required.predecessor.identity.clone(),
            effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
        },
    )
    .unwrap();
    let initialization = derive_history_initialization_id(
        &required.resource_uid,
        required.history.protocol_generation,
    );
    store
        .initialize_public_v1_history(
            &initialization,
            &required.history,
            &required.predecessor.storage_incarnation,
        )
        .await
        .unwrap();

    let predecessor_intent = recovery_intent(
        &preview,
        &renewal.authorization,
        &renewal.predecessor_attempt,
    );
    store
        .apply_public_v1_history_command(&PublicV1HistoryCommand::Recover {
            accepted: accepted_recovery(
                &preview,
                initialization.clone(),
                &renewal.authorization,
                &renewal.predecessor_attempt,
            ),
            intent: Box::new(predecessor_intent.clone()),
        })
        .await
        .unwrap();
    store
        .contain_public_v1_data_loss_attempt(&predecessor_intent)
        .await
        .unwrap();
    drop(store);
    let store = SqliteStore::open_public_v1(&path, &preview).unwrap();
    assert_eq!(
        store.public_v1_operation_intents().await.unwrap(),
        vec![predecessor_intent.clone()],
        "an ambiguous contained callback attempt must survive exact schema-10 restart"
    );

    let accepted = PublicV1SessionRenewalStatus::required(required.clone()).unwrap();
    store
        .persist_public_v1_session_renewal(None, &accepted)
        .await
        .unwrap();
    let closed = accepted.close_access().unwrap();
    store
        .persist_public_v1_session_renewal(Some(&accepted), &closed)
        .await
        .unwrap();
    let frozen = closed.freeze_surviving_set().unwrap();
    store
        .persist_public_v1_session_renewal(Some(&closed), &frozen)
        .await
        .unwrap();
    let barriers = frozen.begin_epoch_barriers().unwrap();
    store
        .persist_public_v1_session_renewal(Some(&frozen), &barriers)
        .await
        .unwrap();
    let role_pending = barriers.finish_epoch_barriers().unwrap();
    store
        .persist_public_v1_session_renewal(Some(&barriers), &role_pending)
        .await
        .unwrap();
    let configuration_pending = role_pending
        .replay_replacement_role(
            &required.replacement.as_ref().unwrap().process_session_id,
            required.renewal_epoch,
            ReplicaRole::Primary,
        )
        .unwrap();
    store
        .persist_public_v1_session_renewal(Some(&role_pending), &configuration_pending)
        .await
        .unwrap();
    let authorized = configuration_pending
        .install_fresh_configuration(&required.fresh_configuration)
        .unwrap();
    store
        .persist_public_v1_session_renewal(Some(&configuration_pending), &authorized)
        .await
        .unwrap();

    let successor_intent = recovery_intent(
        &preview,
        &renewal.authorization,
        &renewal.replacement_attempt,
    );
    let mut changed_authorization = renewal.authorization.clone();
    changed_authorization.reset_scope = ProviderProgressReset::Preserve;
    let changed_intent = recovery_intent(
        &preview,
        &changed_authorization,
        &renewal.replacement_attempt,
    );
    assert!(
        store
            .apply_public_v1_history_command(&PublicV1HistoryCommand::Recover {
                accepted: accepted_recovery(
                    &preview,
                    initialization.clone(),
                    &changed_authorization,
                    &renewal.replacement_attempt,
                ),
                intent: Box::new(changed_intent),
            })
            .await
            .is_err()
    );
    store
        .apply_public_v1_history_command(&PublicV1HistoryCommand::Recover {
            accepted: accepted_recovery(
                &preview,
                initialization,
                &renewal.authorization,
                &renewal.replacement_attempt,
            ),
            intent: Box::new(successor_intent),
        })
        .await
        .unwrap();
    let snapshot = store.public_v1_history_snapshot().await.unwrap();
    assert_eq!(snapshot.authorization, Some(renewal.authorization.clone()));
    assert_eq!(
        snapshot.recovery.unwrap().attempt,
        renewal.replacement_attempt
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn public_v1_data_loss_renewal_requires_exact_replay_capability() {
    let exact = data_loss_required(DataLossReplay::Convergent, DataLossReplay::Convergent);
    exact.validate().unwrap();
    assert_eq!(
        exact.data_loss_decision(),
        Some(DataLossRenewalDecision::InvokeCallback)
    );

    let conservative = data_loss_required(
        DataLossReplay::Convergent,
        DataLossReplay::ReplaceOnAmbiguity,
    );
    conservative.validate().unwrap();
    assert_eq!(
        conservative.data_loss_decision(),
        Some(DataLossRenewalDecision::RetireWithoutCallback)
    );
    let retired = PublicV1SessionRenewalStatus::required(conservative)
        .unwrap()
        .close_access()
        .unwrap()
        .freeze_surviving_set()
        .unwrap()
        .begin_epoch_barriers()
        .unwrap()
        .finish_epoch_barriers()
        .unwrap();
    assert_eq!(retired.stage, PublicSessionRenewalStage::Retired);

    let upgrade = data_loss_required(
        DataLossReplay::ReplaceOnAmbiguity,
        DataLossReplay::Convergent,
    );
    assert!(upgrade.validate().is_err());
    assert_eq!(
        upgrade.data_loss_decision(),
        Some(DataLossRenewalDecision::Terminal)
    );
}

#[tokio::test]
async fn public_v1_schema11_requires_a_fresh_exact_store() {
    assert_eq!(PUBLIC_V1_PREVIEW_SCHEMA_VERSION, 11);
    let root = test_root("schema11-fresh");
    let path = SqliteStore::metadata_database_path(&root);
    let preview = PublicOperationPreviewIdentity::public_v1(42);
    let local = replica(1);
    let store = SqliteStore::create_public_v1(
        &path,
        preview.clone(),
        StorageIdentity {
            schema_version: SCHEMA_VERSION,
            resource_uid: ResourceUid::new("resource-1"),
            pod_uid: PodUid::new("pod-1"),
            pvc_uid: PvcUid::new("pvc-1"),
            initialization_id: InitializationId::new("local-initialization"),
            local_identity: local.clone(),
            effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
        },
    )
    .unwrap();
    let generation = PublicProtocolGeneration::new(42);
    let initialization =
        derive_history_initialization_id(&ResourceUid::new("resource-1"), generation);
    let history =
        derive_bootstrap_history(&ResourceUid::new("resource-1"), generation, &initialization);
    let local_storage = storage(local);
    store
        .initialize_public_v1_history(&initialization, &history, &local_storage)
        .await
        .unwrap();
    let build_intent = build_intent(&preview, &history, storage(replica(2)));
    build_intent.validate().unwrap();
    store
        .begin_public_v1_operation(&build_intent)
        .await
        .unwrap();
    let required = ordinary_required(Epoch::new(0, 10), Epoch::new(0, 11));
    let active = PublicV1SessionRenewalStatus::required(required.clone()).unwrap();
    store
        .persist_public_v1_session_renewal(None, &active)
        .await
        .unwrap();
    drop(store);
    let reopened = SqliteStore::open_public_v1(&path, &preview).unwrap();
    assert_eq!(
        reopened.public_v1_operation_intents().await.unwrap(),
        vec![build_intent.clone()]
    );
    let active_builds = reopened.public_v1_build_records().await.unwrap();
    assert_eq!(active_builds.len(), 1);
    assert_eq!(
        active_builds[0].retirement,
        PublicV1BuildRetirementSnapshot::Retain
    );
    assert!(active_builds[0].result.is_none());
    let PublicOperationProgram::Build(build) = build_intent.program.as_ref().unwrap() else {
        panic!("expected public-v1 build program");
    };
    assert_eq!(active_builds[0].target, build.replica);
    assert_eq!(active_builds[0].source_storage, storage(replica(1)));
    assert_eq!(active_builds[0].target_storage, build.target_storage);
    assert_eq!(active_builds[0].target_session, build.process_session_id);
    assert_eq!(active_builds[0].target_address, build.replication_address);
    assert_eq!(
        reopened
            .public_v1_history_snapshot()
            .await
            .unwrap()
            .session_renewal,
        Some(active.clone())
    );
    let mut stale_required = required.clone();
    stale_required.operation_id = OperationId::new("stale-renewal");
    assert!(
        reopened
            .persist_public_v1_session_renewal(
                None,
                &PublicV1SessionRenewalStatus::required(stale_required).unwrap(),
            )
            .await
            .is_err()
    );
    let mut superseded_intent = build_intent.clone();
    let PublicOperationProgram::Build(build) = superseded_intent.program.as_mut().unwrap() else {
        panic!("expected public-v1 build program");
    };
    build.replication_address = "https://10.0.0.2:7001".into();
    superseded_intent.input_digest = superseded_intent.program.as_ref().unwrap().digest();
    assert!(
        reopened
            .begin_public_v1_operation(&superseded_intent)
            .await
            .is_err()
    );
    let mut incomplete_intent = build_intent.clone();
    let PublicOperationProgram::Build(build) = incomplete_intent.program.as_mut().unwrap() else {
        panic!("expected public-v1 build program");
    };
    build.process_session_id = ProcessSessionId::default();
    incomplete_intent.input_digest = incomplete_intent.program.as_ref().unwrap().digest();
    assert!(
        reopened
            .begin_public_v1_operation(&incomplete_intent)
            .await
            .is_err()
    );
    reopened
        .complete_public_v1_build(&build_intent)
        .await
        .unwrap();
    drop(reopened);
    let reopened = SqliteStore::open_public_v1(&path, &preview).unwrap();
    let terminal_build = reopened
        .public_v1_operation_snapshot(&build_intent.operation_id)
        .await
        .unwrap();
    assert_eq!(&terminal_build.intent, &build_intent);
    assert_eq!(
        terminal_build.stage,
        PublicV1OperationSnapshotStage::Completed
    );
    assert_eq!(
        terminal_build.disposition,
        Some(PublicV1OperationSnapshotDisposition::Succeeded)
    );
    assert_eq!(
        terminal_build.intent.replay_disposition,
        PublicOperationReplayDisposition::RetainBuildSuccessRetireAmbiguity
    );
    let terminal_builds = reopened.public_v1_build_records().await.unwrap();
    assert_eq!(terminal_builds.len(), 1);
    assert_eq!(
        terminal_builds[0].result,
        Some(kuberic_runtime::protocol::public_operations::PublicV1OperationResult::Succeeded)
    );
    let report = reopened
        .public_v1_agent_report(
            &preview,
            PublicV1ReportProjection {
                process_session_id: ProcessSessionId::new("schema11-source-session"),
                report_sequence: 1,
                role: ReplicaRole::Primary,
                read_status: AccessStatus::NotPrimary,
                write_status: AccessStatus::NotPrimary,
                progress: PublicProgress {
                    role: PublicProgressRole::Primary,
                    first: 0,
                    last: 0,
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(report.completed_operations.len(), 1);
    let completion = &report.completed_operations[0];
    assert_eq!(
        completion.call_kind,
        kuberic_runtime::protocol::public_operations::PublicV1CallKind::Build
    );
    assert_eq!(
        completion.process_session_id,
        ProcessSessionId::new("schema11-source-session")
    );
    assert_eq!(
        kuberic_runtime::control::normalize_public_v1_agent_status_report(
            kuberic_runtime::control::public_v1_agent_status_to_proto(&report),
        )
        .unwrap(),
        report
    );
    let PublicOperationProgram::Build(build) = build_intent.program.as_ref().unwrap() else {
        panic!("expected public-v1 build program");
    };
    let removal_build = build.clone();
    let removal_program = PublicOperationProgram::Remove(removal_build.clone());
    let removal_intent = PublicOperationIntent {
        preview: preview.clone(),
        operation_id: OperationId::new("schema11-remove-build"),
        revision: 2,
        process_session_id: ProcessSessionId::new("schema11-source-session"),
        class: PublicOperationClass::Remove {
            target: removal_build.replica.replica_id,
        },
        input_digest: removal_program.digest(),
        replay_disposition: removal_program.replay_disposition(),
        lifecycle: None,
        program: Some(removal_program),
    };
    reopened
        .begin_public_v1_operation(&removal_intent)
        .await
        .unwrap();
    reopened
        .complete_public_v1_remove(&removal_intent)
        .await
        .unwrap();
    drop(reopened);
    let reopened = SqliteStore::open_public_v1(&path, &preview).unwrap();
    let retired_builds = reopened.public_v1_build_records().await.unwrap();
    assert_eq!(retired_builds.len(), 1);
    assert_eq!(
        retired_builds[0].retirement,
        PublicV1BuildRetirementSnapshot::Retired
    );
    let closed = active.close_access().unwrap();
    reopened
        .persist_public_v1_session_renewal(Some(&active), &closed)
        .await
        .unwrap();
    drop(reopened);
    let reopened = SqliteStore::open_public_v1(&path, &preview).unwrap();
    assert_eq!(
        reopened
            .public_v1_history_snapshot()
            .await
            .unwrap()
            .session_renewal,
        Some(closed.clone())
    );
    let frozen = closed.freeze_surviving_set().unwrap();
    reopened
        .persist_public_v1_session_renewal(Some(&closed), &frozen)
        .await
        .unwrap();
    let barriers = frozen.begin_epoch_barriers().unwrap();
    reopened
        .persist_public_v1_session_renewal(Some(&frozen), &barriers)
        .await
        .unwrap();
    let survivor = required.surviving_sessions[0].clone();
    let barriers_complete = barriers
        .record_epoch_updated(&survivor, required.renewal_epoch)
        .unwrap();
    reopened
        .persist_public_v1_session_renewal(Some(&barriers), &barriers_complete)
        .await
        .unwrap();
    let role_pending = barriers_complete.finish_epoch_barriers().unwrap();
    reopened
        .persist_public_v1_session_renewal(Some(&barriers_complete), &role_pending)
        .await
        .unwrap();
    let configuration_pending = role_pending
        .replay_replacement_role(
            &required.replacement.as_ref().unwrap().process_session_id,
            required.renewal_epoch,
            ReplicaRole::Primary,
        )
        .unwrap();
    reopened
        .persist_public_v1_session_renewal(Some(&role_pending), &configuration_pending)
        .await
        .unwrap();
    let terminal = configuration_pending
        .install_fresh_configuration(&required.fresh_configuration)
        .unwrap();
    reopened
        .persist_public_v1_session_renewal(Some(&configuration_pending), &terminal)
        .await
        .unwrap();
    drop(reopened);
    let reopened = SqliteStore::open_public_v1(&path, &preview).unwrap();
    assert_eq!(
        reopened
            .public_v1_history_snapshot()
            .await
            .unwrap()
            .session_renewal,
        Some(terminal)
    );
    drop(reopened);
    assert!(
        SqliteStore::create_public_v1(
            &path,
            preview.clone(),
            StorageIdentity {
                schema_version: SCHEMA_VERSION,
                resource_uid: ResourceUid::new("resource-1"),
                pod_uid: PodUid::new("pod-1"),
                pvc_uid: PvcUid::new("pvc-1"),
                initialization_id: InitializationId::new("local-initialization"),
                local_identity: replica(1),
                effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
            },
        )
        .is_err()
    );
    let connection = rusqlite::Connection::open(&path).unwrap();
    let state_json: String = connection
        .query_row(
            "SELECT state_json FROM agent_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let original_state: serde_json::Value = serde_json::from_str(&state_json).unwrap();
    let mut mismatched_record = original_state.clone();
    *mismatched_record
        .get_mut("publicOperationPreview")
        .and_then(|preview| preview.get_mut("buildRecords"))
        .and_then(|records| records.get_mut("schema11-build"))
        .and_then(|record| record.get_mut("admissionRevision"))
        .unwrap() = serde_json::json!(2);
    connection
        .execute(
            "UPDATE agent_state SET state_json = ?1 WHERE singleton = 1",
            [serde_json::to_string(&mismatched_record).unwrap()],
        )
        .unwrap();
    assert!(SqliteStore::open_public_v1(&path, &preview).is_err());
    connection
        .execute(
            "UPDATE agent_state SET state_json = ?1 WHERE singleton = 1",
            [state_json],
        )
        .unwrap();

    for (field, value) in [
        (
            "epoch",
            serde_json::json!({"dataLossNumber": 0, "configurationNumber": 99}),
        ),
        ("result", serde_json::Value::Null),
    ] {
        let mut changed = original_state.clone();
        *changed
            .get_mut("publicOperationPreview")
            .and_then(|preview| preview.get_mut("buildRecords"))
            .and_then(|records| records.get_mut("schema11-build"))
            .and_then(|record| record.get_mut(field))
            .unwrap() = value;
        connection
            .execute(
                "UPDATE agent_state SET state_json = ?1 WHERE singleton = 1",
                [serde_json::to_string(&changed).unwrap()],
            )
            .unwrap();
        assert!(SqliteStore::open_public_v1(&path, &preview).is_err());
    }
    connection
        .execute(
            "UPDATE agent_state SET state_json = ?1 WHERE singleton = 1",
            [serde_json::to_string(&original_state).unwrap()],
        )
        .unwrap();

    let mut changed_storage = original_state.clone();
    *changed_storage
        .get_mut("publicOperationPreview")
        .and_then(|preview| preview.get_mut("buildRecords"))
        .and_then(|records| records.get_mut("schema11-build"))
        .and_then(|record| record.get_mut("targetStorage"))
        .and_then(|storage| storage.get_mut("storageGeneration"))
        .unwrap() = serde_json::json!("substituted-storage-generation");
    connection
        .execute(
            "UPDATE agent_state SET state_json = ?1 WHERE singleton = 1",
            [serde_json::to_string(&changed_storage).unwrap()],
        )
        .unwrap();
    assert!(SqliteStore::open_public_v1(&path, &preview).is_err());
    connection
        .execute(
            "UPDATE agent_state SET state_json = ?1 WHERE singleton = 1",
            [serde_json::to_string(&original_state).unwrap()],
        )
        .unwrap();

    let mut state = original_state;
    let operations = state
        .get_mut("publicOperationPreview")
        .and_then(|preview| preview.get_mut("operations"))
        .and_then(serde_json::Value::as_object_mut)
        .unwrap();
    let record = operations.get_mut("schema11-build").unwrap();
    let process_session = record
        .get_mut("intent")
        .and_then(|intent| intent.get_mut("program"))
        .and_then(|program| program.get_mut("Build"))
        .and_then(serde_json::Value::as_object_mut)
        .unwrap()
        .remove("process_session_id");
    assert!(process_session.is_some());
    connection
        .execute(
            "UPDATE agent_state SET state_json = ?1 WHERE singleton = 1",
            [serde_json::to_string(&state).unwrap()],
        )
        .unwrap();
    drop(connection);
    assert!(SqliteStore::open_public_v1(&path, &preview).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn public_v1_schema11_build_ambiguity_requires_retirement() {
    let root = test_root("schema11-build-ambiguity");
    let path = SqliteStore::metadata_database_path(&root);
    let preview = PublicOperationPreviewIdentity::public_v1(42);
    let local = replica(1);
    let local_storage = storage(local.clone());
    let store = SqliteStore::create_public_v1(
        &path,
        preview.clone(),
        StorageIdentity {
            schema_version: SCHEMA_VERSION,
            resource_uid: ResourceUid::new("resource-1"),
            pod_uid: PodUid::new("pod-1"),
            pvc_uid: PvcUid::new("pvc-1"),
            initialization_id: InitializationId::new("local-initialization"),
            local_identity: local,
            effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
        },
    )
    .unwrap();
    let generation = PublicProtocolGeneration::new(42);
    let initialization =
        derive_history_initialization_id(&ResourceUid::new("resource-1"), generation);
    let history =
        derive_bootstrap_history(&ResourceUid::new("resource-1"), generation, &initialization);
    store
        .initialize_public_v1_history(&initialization, &history, &local_storage)
        .await
        .unwrap();
    let intent = build_intent(&preview, &history, storage(replica(2)));
    store.begin_public_v1_operation(&intent).await.unwrap();
    store.mark_public_v1_build_ambiguous(&intent).await.unwrap();
    drop(store);

    let reopened = SqliteStore::open_public_v1(&path, &preview).unwrap();
    let records = reopened.public_v1_build_records().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].retirement,
        PublicV1BuildRetirementSnapshot::Required
    );
    assert!(matches!(
        records[0].result,
        Some(kuberic_runtime::protocol::public_operations::PublicV1OperationResult::Ambiguous(_))
    ));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn public_v1_schema11_failed_build_containment_survives_reopen() {
    let root = test_root("schema11-build-failed-pending");
    let path = SqliteStore::metadata_database_path(&root);
    let preview = PublicOperationPreviewIdentity::public_v1(42);
    let local = replica(1);
    let local_storage = storage(local.clone());
    let store = SqliteStore::create_public_v1(
        &path,
        preview.clone(),
        StorageIdentity {
            schema_version: SCHEMA_VERSION,
            resource_uid: ResourceUid::new("resource-1"),
            pod_uid: PodUid::new("pod-1"),
            pvc_uid: PvcUid::new("pvc-1"),
            initialization_id: InitializationId::new("local-initialization"),
            local_identity: local,
            effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
        },
    )
    .unwrap();
    let generation = PublicProtocolGeneration::new(42);
    let initialization =
        derive_history_initialization_id(&ResourceUid::new("resource-1"), generation);
    let history =
        derive_bootstrap_history(&ResourceUid::new("resource-1"), generation, &initialization);
    store
        .initialize_public_v1_history(&initialization, &history, &local_storage)
        .await
        .unwrap();
    let intent = build_intent(&preview, &history, storage(replica(2)));
    store.begin_public_v1_operation(&intent).await.unwrap();
    store
        .mark_public_v1_build_failed_pending(&intent, "build callback failed")
        .await
        .unwrap();
    drop(store);

    let reopened = SqliteStore::open_public_v1(&path, &preview).unwrap();
    let record = reopened
        .public_v1_operation_snapshot(&intent.operation_id)
        .await
        .unwrap();
    assert_eq!(
        record.stage,
        PublicV1OperationSnapshotStage::ContainmentPending
    );
    assert_eq!(
        record.disposition,
        Some(PublicV1OperationSnapshotDisposition::Failed(
            "build callback failed".into()
        ))
    );
    assert!(matches!(
        reopened.public_v1_build_records().await.unwrap()[0].result,
        Some(PublicV1OperationResult::Failed(ref detail))
            if detail == "build callback failed"
    ));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn public_v1_schema11_rebuild_then_cancellation_survives_reopen() {
    use kuberic_runtime::protocol::public_operations::{
        CanonicalReplicaRebuildRequired, CapturedPublicOperationIdentity,
        PublicBuildAttemptReference, ReplicaRebuildReason,
    };

    let root = test_root("schema11-build-rebuild-cancelled");
    let path = SqliteStore::metadata_database_path(&root);
    let preview = PublicOperationPreviewIdentity::public_v1(42);
    let local = replica(1);
    let local_storage = storage(local.clone());
    let target_storage = storage(replica(2));
    let store = SqliteStore::create_public_v1(
        &path,
        preview.clone(),
        StorageIdentity {
            schema_version: SCHEMA_VERSION,
            resource_uid: ResourceUid::new("resource-1"),
            pod_uid: PodUid::new("pod-1"),
            pvc_uid: PvcUid::new("pvc-1"),
            initialization_id: InitializationId::new("local-initialization"),
            local_identity: local.clone(),
            effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
        },
    )
    .unwrap();
    let generation = PublicProtocolGeneration::new(42);
    let initialization =
        derive_history_initialization_id(&ResourceUid::new("resource-1"), generation);
    let history =
        derive_bootstrap_history(&ResourceUid::new("resource-1"), generation, &initialization);
    store
        .initialize_public_v1_history(&initialization, &history, &local_storage)
        .await
        .unwrap();
    let intent = build_intent(&preview, &history, target_storage.clone());
    let PublicOperationProgram::Build(build) = intent.program.as_ref().unwrap() else {
        panic!("expected build program");
    };
    let rebuild = CanonicalReplicaRebuildRequired::RemoteBuildTarget {
        reason: ReplicaRebuildReason::RemoteCopyConflict,
        operation: CapturedPublicOperationIdentity {
            resource_uid: ResourceUid::new("resource-1"),
            preview: preview.clone(),
            epoch: build.epoch,
            operation_id: intent.operation_id.clone(),
            revision: intent.revision,
            detecting_session: intent.process_session_id.clone(),
        },
        build: Box::new(PublicBuildAttemptReference {
            resource_uid: ResourceUid::new("resource-1"),
            epoch: build.epoch,
            build_id: intent.operation_id.clone(),
            source: local,
            source_storage: local_storage,
            source_session: intent.process_session_id.clone(),
            target: build.replica.clone(),
            target_storage,
            target_session: build.process_session_id.clone(),
        }),
    };
    store.begin_public_v1_operation(&intent).await.unwrap();
    store
        .mark_public_v1_build_rebuild_pending(&intent, rebuild.clone())
        .await
        .unwrap();
    drop(store);

    let reopened = SqliteStore::open_public_v1(&path, &preview).unwrap();
    let pending = reopened
        .public_v1_operation_snapshot(&intent.operation_id)
        .await
        .unwrap();
    assert_eq!(pending.stage, PublicV1OperationSnapshotStage::Running);
    assert_eq!(pending.disposition, None);
    assert_eq!(
        reopened.public_v1_build_records().await.unwrap()[0].result,
        Some(PublicV1OperationResult::RebuildRequired(Box::new(
            rebuild.clone()
        )))
    );
    reopened
        .cancel_public_v1_build_rebuild_pending(&intent)
        .await
        .unwrap();
    drop(reopened);

    let reopened = SqliteStore::open_public_v1(&path, &preview).unwrap();
    let record = reopened
        .public_v1_operation_snapshot(&intent.operation_id)
        .await
        .unwrap();
    assert_eq!(
        record.stage,
        PublicV1OperationSnapshotStage::ContainmentPending
    );
    assert_eq!(
        record.disposition,
        Some(PublicV1OperationSnapshotDisposition::Cancelled)
    );
    assert_eq!(
        reopened.public_v1_build_records().await.unwrap()[0].result,
        Some(PublicV1OperationResult::RebuildRequired(Box::new(rebuild)))
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn public_v1_schema11_rejects_prior_preview_stores() {
    let preview = PublicOperationPreviewIdentity::public_v1(42);
    for version in [6, 7, 8, 9, 10] {
        let root = test_root(&format!("schema11-reject-{version}"));
        let path = SqliteStore::metadata_database_path(&root);
        let store = SqliteStore::create_public_v1(
            &path,
            preview.clone(),
            StorageIdentity {
                schema_version: SCHEMA_VERSION,
                resource_uid: ResourceUid::new("resource-1"),
                pod_uid: PodUid::new("pod-1"),
                pvc_uid: PvcUid::new("pvc-1"),
                initialization_id: InitializationId::new("local-initialization"),
                local_identity: replica(1),
                effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
            },
        )
        .unwrap();
        drop(store);
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(connection);
        assert!(SqliteStore::open_public_v1(&path, &preview).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
