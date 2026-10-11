use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use kuberic_runtime::protocol::command::{PublicV1HistoryCommand, PublicV1SessionRenewalCommand};
use kuberic_runtime::protocol::public_operations::{
    DataLossAttempt, DataLossAuthorization, DataLossRenewalDecision, DataLossReplay,
    PossibleDataLossIntent, ProcessAttemptContext, ProcessSessionRenewalRequired,
    ProviderProgressReset, PublicDataLossRecoveryInput, PublicDataLossRecoveryStage,
    PublicDataLossRecoveryStatus, PublicDataLossSessionRenewal, PublicLifecycleInput,
    PublicLifecycleRecipe, PublicOperationClass, PublicOperationIntent,
    PublicOperationPreviewIdentity, PublicOperationReplayDisposition, PublicProgress,
    PublicProgressRole, PublicSessionRenewalStage, PublicV1AcceptedStatus, PublicV1AgentReport,
    PublicV1SessionRenewalStatus,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, AgentGeneration, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy,
    Epoch, InitializationId, OperationId, PodUid, ProcessSessionEndpoint, ProcessSessionId,
    ProcessSessionRenewalReason, PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity, ReplicaStorageIncarnationContext,
    ReplicatorCreationId, ResourceUid, StorageGeneration, derive_bootstrap_history,
    derive_data_loss_history, derive_history_initialization_id,
};
use kuberic_runtime::testing::sqlite_store::{PUBLIC_V1_PREVIEW_SCHEMA_VERSION, SqliteStore};
use kuberic_runtime::testing::state::{SCHEMA_VERSION, StorageIdentity};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

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
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        extensions: Default::default(),
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
        authorization: None,
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
    store
        .persist_public_v1_session_renewal(None, &accepted)
        .await
        .unwrap();
    let closed = store
        .apply_public_v1_session_renewal_command(
            &PublicV1SessionRenewalCommand::CloseAccessAndIngress {
                accepted: accepted.clone(),
            },
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
        )
        .await
        .unwrap();
    let authorized = store
        .apply_public_v1_session_renewal_command(
            &PublicV1SessionRenewalCommand::InstallFreshConfiguration {
                accepted: configuration_pending,
                configuration: required.fresh_configuration.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(authorized.stage, PublicSessionRenewalStage::Authorized);
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
async fn public_v1_schema10_requires_a_fresh_exact_store() {
    assert_eq!(PUBLIC_V1_PREVIEW_SCHEMA_VERSION, 10);
    let root = test_root("schema10-fresh");
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
    store
        .initialize_public_v1_history(&initialization, &history, &storage(local))
        .await
        .unwrap();
    drop(store);
    assert!(SqliteStore::open_public_v1(&path, &preview).is_ok());
    assert!(
        SqliteStore::create_public_v1(
            &path,
            preview,
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
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn public_v1_schema10_rejects_prior_preview_stores() {
    let preview = PublicOperationPreviewIdentity::public_v1(42);
    for version in [6, 7, 8, 9] {
        let root = test_root(&format!("schema10-reject-{version}"));
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
