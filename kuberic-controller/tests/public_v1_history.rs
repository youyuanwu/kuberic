use kuberic_controller::evaluator::test_bridge::{
    compare_public_v1_election_progress, ensure_public_v1_bootstrap_history,
    invalidate_public_v1_for_data_loss, is_public_v1_provisional_primary,
};
use kuberic_runtime::protocol::public_operations::{
    DataLossAttempt, DataLossAuthorization, DataLossReplay, ProcessAttemptContext,
    ProviderProgressReset, PublicOperationPreviewIdentity, PublicProgress, PublicProgressRole,
    PublicV1AcceptedOperationCompletion, PublicV1AcceptedReplayDisposition, PublicV1AcceptedResult,
    PublicV1AgentReport,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, AgentGeneration, Epoch, PodUid, ProcessSessionId, PvcUid, ReplicaId,
    ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity,
    ReplicaStorageIncarnationContext, ReplicatorCreationId, StorageGeneration,
};
use kuberic_runtime::protocol::types::{
    OperationId, PublicProtocolGeneration, ResourceUid, derive_bootstrap_history,
    derive_data_loss_history, derive_history_initialization_id,
};

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
    let identity = PublicOperationPreviewIdentity::public_v1(42);
    let accepted = ensure_public_v1_bootstrap_history(None, &resource_uid, &identity).unwrap();
    let retried =
        ensure_public_v1_bootstrap_history(Some(accepted.clone()), &resource_uid, &identity)
            .unwrap();
    assert_eq!(accepted, retried);

    let changed = PublicOperationPreviewIdentity::public_v1(43);
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
    let identity = PublicOperationPreviewIdentity::public_v1(42);
    let mut accepted = ensure_public_v1_bootstrap_history(None, &resource_uid, &identity).unwrap();
    accepted
        .completed_operations
        .push(PublicV1AcceptedOperationCompletion {
            operation_id: OperationId::new("old-completion"),
            revision: 1,
            input_digest_version: 1,
            input_digest_sha256: "old-input".into(),
            replay_disposition: PublicV1AcceptedReplayDisposition::Convergent,
            result: PublicV1AcceptedResult::Succeeded,
            result_digest_sha256: "old-result".into(),
        });
    let storage = storage();
    let report = PublicV1AgentReport {
        identity: identity.clone(),
        resource_uid: resource_uid.clone(),
        replica: storage.replica.clone(),
        process_session_id: ProcessSessionId::new("session-1"),
        report_sequence: 1,
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
        invalidate_public_v1_for_data_loss(&accepted, &report, &authorization, &attempt).unwrap();
    assert_eq!(invalidated.history, accepted.history);
    assert_eq!(
        invalidated.data_loss_recovery.as_ref().unwrap().attempt,
        attempt
    );
    assert!(invalidated.completed_operations.is_empty());

    let mut crossed = report;
    crossed.process_session_id = ProcessSessionId::new("other-session");
    assert!(
        invalidate_public_v1_for_data_loss(&accepted, &crossed, &authorization, &attempt).is_err()
    );
}

#[test]
fn public_v1_recovery_progress_only_selects_access_closed_provisional_primary() {
    let resource_uid = ResourceUid::new("resource-1");
    let identity = PublicOperationPreviewIdentity::public_v1(42);
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
        report_sequence: 2,
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
        data_loss_recovery: Some(recovery),
        completed_operations: Vec::new(),
        extensions: Default::default(),
    };
    assert!(is_public_v1_provisional_primary(&report));

    report.write_status = AccessStatus::Granted;
    assert!(!is_public_v1_provisional_primary(&report));
    report.write_status = AccessStatus::NotPrimary;
    report.role = ReplicaRole::ActiveSecondary;
    assert!(!is_public_v1_provisional_primary(&report));
}
