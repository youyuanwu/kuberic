use std::sync::{Arc, Mutex};

use kuberic_controller::evaluator::test_bridge::{
    PublicV1CompletionRequirement, PublicV1EvidenceApi, PublicV1EvidenceConsumer,
    PublicV1EvidenceEffect, PublicV1EvidencePlan, PublicV1EvidenceState,
    execute_public_v1_evidence, plan_public_v1_evidence,
};
use kuberic_runtime::protocol::public_operations::{
    CanonicalOperationInputDigest, CanonicalOperationInputDigestV1,
    CanonicalReplicaRebuildRequired, CapturedPublicOperationIdentity, ProviderReplicaIdentity,
    PublicOperationPreviewIdentity, PublicOperationReplayDisposition, PublicProgress,
    PublicProgressRole, PublicV1AcceptedStatus, PublicV1AgentReport, PublicV1CallKind,
    PublicV1OperationCompletion, PublicV1OperationResult, ReplicaRebuildReason,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, AgentGeneration, ConfigurationId, Epoch, OperationId, PodUid, ProcessSessionId,
    PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole,
    ReplicaStorageIdentity, ReplicaStorageIncarnationContext, ResourceUid, StorageGeneration,
    derive_bootstrap_history, derive_history_initialization_id,
};

#[derive(Default)]
struct FakeApi {
    status: Mutex<Option<PublicV1AcceptedStatus>>,
    accepted: Mutex<Vec<(PublicV1EvidenceConsumer, OperationId)>>,
    retired: Mutex<Vec<(PublicV1EvidenceConsumer, OperationId)>>,
    replacements: Mutex<Vec<(PublicV1EvidenceConsumer, OperationId)>>,
}

#[async_trait::async_trait]
impl PublicV1EvidenceApi for FakeApi {
    async fn observe_public_v1_evidence_status(
        &self,
    ) -> Result<Option<PublicV1AcceptedStatus>, String> {
        Ok(self.status.lock().unwrap().clone())
    }

    async fn persist_public_v1_evidence_status(
        &self,
        expected: &PublicV1AcceptedStatus,
        next: &PublicV1AcceptedStatus,
    ) -> Result<(), String> {
        let mut status = self.status.lock().unwrap();
        if status.as_ref() != Some(expected) {
            return Err("stale public-v1 evidence persistence".into());
        }
        *status = Some(next.clone());
        Ok(())
    }

    async fn accept_public_v1_completion(
        &self,
        _expected: &PublicV1AcceptedStatus,
        consumer: PublicV1EvidenceConsumer,
        completion: &PublicV1OperationCompletion,
    ) -> Result<(), String> {
        self.accepted
            .lock()
            .unwrap()
            .push((consumer, completion.operation_id.clone()));
        Ok(())
    }

    async fn retire_public_v1_incarnation(
        &self,
        _expected: &PublicV1AcceptedStatus,
        consumer: PublicV1EvidenceConsumer,
        completion: &PublicV1OperationCompletion,
        _storage: &ReplicaStorageIncarnationContext,
    ) -> Result<(), String> {
        self.retired
            .lock()
            .unwrap()
            .push((consumer, completion.operation_id.clone()));
        Ok(())
    }

    async fn provision_public_v1_replacement(
        &self,
        _expected: &PublicV1AcceptedStatus,
        consumer: PublicV1EvidenceConsumer,
        completion: &PublicV1OperationCompletion,
        _rebuild: &CanonicalReplicaRebuildRequired,
    ) -> Result<(), String> {
        self.replacements
            .lock()
            .unwrap()
            .push((consumer, completion.operation_id.clone()));
        Ok(())
    }
}

fn replica() -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(1),
        instance_id: ReplicaInstanceId::new("pod-1"),
        agent_generation: AgentGeneration::new("generation-1"),
    }
}

fn storage() -> ReplicaStorageIncarnationContext {
    ReplicaStorageIncarnationContext {
        replica: replica(),
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new("pod-1"),
            pvc_uid: PvcUid::new("pvc-1"),
        },
        storage_generation: StorageGeneration::new("storage-1"),
    }
}

fn accepted() -> PublicV1AcceptedStatus {
    let resource_uid = ResourceUid::new("resource-1");
    let generation = PublicProtocolGeneration::new(42);
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    PublicV1AcceptedStatus {
        identity: PublicOperationPreviewIdentity::public_v1(generation.value()),
        history_initialization_id: initialization.clone(),
        history: derive_bootstrap_history(&resource_uid, generation, &initialization),
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        extensions: Default::default(),
    }
}

fn completion(
    accepted: &PublicV1AcceptedStatus,
    operation: &str,
    call_kind: PublicV1CallKind,
    result: PublicV1OperationResult,
) -> PublicV1OperationCompletion {
    PublicV1OperationCompletion {
        operation_id: OperationId::new(operation),
        revision: 3,
        call_kind,
        process_session_id: ProcessSessionId::new("session-1"),
        input_digest: CanonicalOperationInputDigest::V1(CanonicalOperationInputDigestV1 {
            sha256: "05".repeat(32),
        }),
        replay_disposition: match call_kind {
            PublicV1CallKind::Build => {
                PublicOperationReplayDisposition::RetainBuildSuccessRetireAmbiguity
            }
            PublicV1CallKind::Remove => PublicOperationReplayDisposition::ExactRemoval,
            _ => PublicOperationReplayDisposition::Convergent,
        },
        history: accepted.history.clone(),
        storage_incarnation: storage(),
        previous_configuration_id: Some(ConfigurationId::new("previous")),
        current_configuration_id: Some(ConfigurationId::new("current")),
        catch_up_mode: None,
        catch_up_specific_quorum: None,
        result,
    }
}

fn report(
    accepted: &PublicV1AcceptedStatus,
    completion: PublicV1OperationCompletion,
) -> PublicV1AgentReport {
    PublicV1AgentReport {
        identity: accepted.identity.clone(),
        resource_uid: accepted.history.resource_uid.clone(),
        replica: replica(),
        process_session_id: ProcessSessionId::new("session-1"),
        report_sequence: 1,
        role: ReplicaRole::Primary,
        read_status: AccessStatus::NotPrimary,
        write_status: AccessStatus::NotPrimary,
        history: accepted.history.clone(),
        storage_incarnation: storage(),
        progress: PublicProgress {
            role: PublicProgressRole::Primary,
            first: 1,
            last: 1,
        },
        data_loss_recovery: None,
        completed_operations: vec![completion],
        extensions: Default::default(),
    }
}

fn requirement(
    consumer: PublicV1EvidenceConsumer,
    completion: &PublicV1OperationCompletion,
) -> PublicV1CompletionRequirement {
    PublicV1CompletionRequirement {
        consumer,
        operation_id: completion.operation_id.clone(),
        revision: completion.revision,
        call_kind: completion.call_kind,
        history: completion.history.clone(),
        storage_incarnation: completion.storage_incarnation.clone(),
        process_session_id: completion.process_session_id.clone(),
        input_digest: completion.input_digest.clone(),
        replay_disposition: completion.replay_disposition,
        previous_configuration_id: completion.previous_configuration_id.clone(),
        current_configuration_id: completion.current_configuration_id.clone(),
        catch_up_mode: completion.catch_up_mode,
        catch_up_specific_quorum: completion.catch_up_specific_quorum,
    }
}

#[tokio::test]
async fn public_v1_evidence_consumers_persist_and_use_exact_public_completion() {
    let cases = [
        (
            PublicV1EvidenceConsumer::Failover,
            PublicV1CallKind::FailoverPromotion,
        ),
        (PublicV1EvidenceConsumer::Switchover, PublicV1CallKind::Swap),
        (
            PublicV1EvidenceConsumer::RepairReplacement,
            PublicV1CallKind::Build,
        ),
        (PublicV1EvidenceConsumer::ScaleUp, PublicV1CallKind::Build),
        (
            PublicV1EvidenceConsumer::ScaleDown,
            PublicV1CallKind::Remove,
        ),
        (PublicV1EvidenceConsumer::Removal, PublicV1CallKind::Remove),
        (
            PublicV1EvidenceConsumer::DataLoss,
            PublicV1CallKind::InitialPrimary,
        ),
        (
            PublicV1EvidenceConsumer::SessionRenewal,
            PublicV1CallKind::Epoch,
        ),
    ];
    for (index, (consumer, call_kind)) in cases.into_iter().enumerate() {
        let accepted = accepted();
        let completion = completion(
            &accepted,
            &format!("operation-{index}"),
            call_kind,
            PublicV1OperationResult::Succeeded,
        );
        let report = report(&accepted, completion.clone());
        let requirement = requirement(consumer, &completion);
        let api = FakeApi {
            status: Mutex::new(Some(accepted.clone())),
            ..Default::default()
        };
        let persist = plan_public_v1_evidence(&accepted, &report, &requirement).unwrap();
        assert!(matches!(
            persist,
            PublicV1EvidencePlan::PersistStatus { .. }
        ));
        execute_public_v1_evidence(&api, persist).await.unwrap();
        let persisted = api.status.lock().unwrap().clone().unwrap();
        assert_eq!(
            persisted.completed_operations,
            vec![
                kuberic_runtime::protocol::public_operations::
                    PublicV1AcceptedOperationCompletion::from_completion(&completion)
            ]
        );
        let accept = plan_public_v1_evidence(&persisted, &report, &requirement).unwrap();
        assert!(matches!(accept, PublicV1EvidencePlan::Accept { .. }));
        execute_public_v1_evidence(&api, accept).await.unwrap();
        assert_eq!(
            api.accepted.lock().unwrap().as_slice(),
            &[(consumer, completion.operation_id.clone())]
        );

        let mut changed = requirement.clone();
        changed.process_session_id = ProcessSessionId::new("predecessor-session");
        assert!(plan_public_v1_evidence(&persisted, &report, &changed).is_err());
    }
}

#[tokio::test]
async fn public_v1_concrete_evidence_state_executes_consumer_specific_effects() {
    for (index, consumer) in [
        PublicV1EvidenceConsumer::Failover,
        PublicV1EvidenceConsumer::Switchover,
        PublicV1EvidenceConsumer::RepairReplacement,
        PublicV1EvidenceConsumer::ScaleUp,
        PublicV1EvidenceConsumer::ScaleDown,
        PublicV1EvidenceConsumer::Removal,
        PublicV1EvidenceConsumer::DataLoss,
        PublicV1EvidenceConsumer::SessionRenewal,
    ]
    .into_iter()
    .enumerate()
    {
        let call_kind = match consumer {
            PublicV1EvidenceConsumer::Failover | PublicV1EvidenceConsumer::DataLoss => {
                PublicV1CallKind::FailoverPromotion
            }
            PublicV1EvidenceConsumer::Switchover => PublicV1CallKind::Swap,
            PublicV1EvidenceConsumer::RepairReplacement | PublicV1EvidenceConsumer::ScaleUp => {
                PublicV1CallKind::Build
            }
            PublicV1EvidenceConsumer::ScaleDown | PublicV1EvidenceConsumer::Removal => {
                PublicV1CallKind::Remove
            }
            PublicV1EvidenceConsumer::SessionRenewal => PublicV1CallKind::Epoch,
        };
        let base = accepted();
        let completion = completion(
            &base,
            &format!("concrete-{index}"),
            call_kind,
            PublicV1OperationResult::Succeeded,
        );
        let mut persisted = base.clone();
        persisted.completed_operations.push(
            kuberic_runtime::protocol::public_operations::PublicV1AcceptedOperationCompletion::
                from_completion(&completion),
        );
        let api = PublicV1EvidenceState::new(Some(persisted.clone()));
        let plan = plan_public_v1_evidence(
            &persisted,
            &report(&base, completion.clone()),
            &requirement(consumer, &completion),
        )
        .unwrap();
        execute_public_v1_evidence(&api, plan).await.unwrap();
        assert_eq!(
            api.effects(),
            vec![PublicV1EvidenceEffect::Accepted {
                consumer,
                operation_id: completion.operation_id,
            }]
        );
    }
}

#[tokio::test]
async fn public_v1_concrete_evidence_state_atomically_fences_stale_writers_and_effects() {
    let base = accepted();
    let first = completion(
        &base,
        "concurrent-first",
        PublicV1CallKind::Build,
        PublicV1OperationResult::Succeeded,
    );
    let second = completion(
        &base,
        "concurrent-second",
        PublicV1CallKind::Build,
        PublicV1OperationResult::Succeeded,
    );
    let mut first_status = base.clone();
    first_status.completed_operations.push(
        kuberic_runtime::protocol::public_operations::PublicV1AcceptedOperationCompletion::
            from_completion(&first),
    );
    let mut second_status = base.clone();
    second_status.completed_operations.push(
        kuberic_runtime::protocol::public_operations::PublicV1AcceptedOperationCompletion::
            from_completion(&second),
    );
    let api = Arc::new(PublicV1EvidenceState::new(Some(base.clone())));
    let (first_write, second_write) = tokio::join!(
        api.persist_public_v1_evidence_status(&base, &first_status),
        api.persist_public_v1_evidence_status(&base, &second_status),
    );
    assert_ne!(first_write.is_ok(), second_write.is_ok());
    assert!(api.status() == Some(first_status) || api.status() == Some(second_status));

    assert!(
        api.accept_public_v1_completion(&base, PublicV1EvidenceConsumer::ScaleUp, &first)
            .await
            .is_err()
    );
    assert!(api.effects().is_empty());
}

#[tokio::test]
async fn public_v1_evidence_failure_and_rebuild_are_exact_incarnation_actions() {
    let accepted = accepted();
    let failed = completion(
        &accepted,
        "failed-build",
        PublicV1CallKind::Build,
        PublicV1OperationResult::Failed("provider rejected build".into()),
    );
    let mut accepted_failed = accepted.clone();
    accepted_failed.completed_operations.push(
        kuberic_runtime::protocol::public_operations::PublicV1AcceptedOperationCompletion::
            from_completion(&failed),
    );
    let failed_plan = plan_public_v1_evidence(
        &accepted_failed,
        &report(&accepted, failed.clone()),
        &requirement(PublicV1EvidenceConsumer::RepairReplacement, &failed),
    )
    .unwrap();
    assert!(matches!(
        &failed_plan,
        PublicV1EvidencePlan::RetireExactIncarnation {
            storage: retired,
            ..
        } if **retired == storage()
    ));
    let failed_api = PublicV1EvidenceState::new(Some(accepted_failed));
    execute_public_v1_evidence(&failed_api, failed_plan)
        .await
        .unwrap();
    assert_eq!(failed_api.retired_incarnations(), vec![storage()]);

    let rebuild = CanonicalReplicaRebuildRequired::LocalProvider {
        reason: ReplicaRebuildReason::ProviderMismatch,
        provider: ProviderReplicaIdentity { replica: replica() },
        operation: CapturedPublicOperationIdentity {
            resource_uid: accepted.history.resource_uid.clone(),
            preview: accepted.identity.clone(),
            epoch: Epoch::new(0, 7),
            operation_id: OperationId::new("rebuild-build"),
            revision: 3,
            detecting_session: ProcessSessionId::new("session-1"),
        },
        local_replica: replica(),
        local_storage: PvcUid::new("pvc-1"),
    };
    let rebuilt = completion(
        &accepted,
        "rebuild-build",
        PublicV1CallKind::Build,
        PublicV1OperationResult::RebuildRequired(Box::new(rebuild.clone())),
    );
    let mut accepted_rebuild = accepted.clone();
    accepted_rebuild.completed_operations.push(
        kuberic_runtime::protocol::public_operations::PublicV1AcceptedOperationCompletion::
            from_completion(&rebuilt),
    );
    let rebuild_plan = plan_public_v1_evidence(
        &accepted_rebuild,
        &report(&accepted, rebuilt.clone()),
        &requirement(PublicV1EvidenceConsumer::RepairReplacement, &rebuilt),
    )
    .unwrap();
    assert!(matches!(
        &rebuild_plan,
        PublicV1EvidencePlan::ProvisionReplacement {
            rebuild: observed,
            ..
        } if **observed == rebuild
    ));
    let rebuild_api = PublicV1EvidenceState::new(Some(accepted_rebuild));
    execute_public_v1_evidence(&rebuild_api, rebuild_plan)
        .await
        .unwrap();
    assert_eq!(rebuild_api.replacements(), vec![rebuild]);
}

#[test]
fn public_v1_evidence_rejects_incompatible_history_and_proof_free_scaffolding() {
    let accepted = accepted();
    let observed_completion = completion(
        &accepted,
        "history-fence",
        PublicV1CallKind::FailoverPromotion,
        PublicV1OperationResult::Succeeded,
    );
    let requirement = requirement(PublicV1EvidenceConsumer::Failover, &observed_completion);
    let mut incompatible = report(&accepted, observed_completion);
    incompatible.history.history_id =
        kuberic_runtime::protocol::types::HistoryId::new("other-history");
    assert!(plan_public_v1_evidence(&accepted, &incompatible, &requirement).is_err());

    assert!(matches!(
        plan_public_v1_evidence(
            &accepted,
            &PublicV1AgentReport {
                completed_operations: Vec::new(),
                ..report(
                    &accepted,
                    completion(
                        &accepted,
                        "unobserved",
                        PublicV1CallKind::FailoverPromotion,
                        PublicV1OperationResult::Succeeded,
                    ),
                )
            },
            &requirement,
        )
        .unwrap(),
        PublicV1EvidencePlan::Wait
    ));
}

#[test]
fn public_v1_controller_conformance_rejects_stale_or_substituted_completion() {
    let accepted = accepted();
    let observed = completion(
        &accepted,
        "exact-build",
        PublicV1CallKind::Build,
        PublicV1OperationResult::Succeeded,
    );
    let report = report(&accepted, observed.clone());
    let requirement = requirement(PublicV1EvidenceConsumer::ScaleUp, &observed);

    let mut stale_revision = requirement.clone();
    stale_revision.revision += 1;
    assert!(plan_public_v1_evidence(&accepted, &report, &stale_revision).is_err());

    let mut predecessor = requirement.clone();
    predecessor.process_session_id = ProcessSessionId::new("predecessor");
    assert!(plan_public_v1_evidence(&accepted, &report, &predecessor).is_err());

    let mut substituted_storage = requirement.clone();
    substituted_storage.storage_incarnation.storage_generation =
        StorageGeneration::new("replacement-storage");
    assert!(plan_public_v1_evidence(&accepted, &report, &substituted_storage).is_err());

    let mut substituted_configuration = requirement;
    substituted_configuration.current_configuration_id = Some(ConfigurationId::new("other"));
    assert!(plan_public_v1_evidence(&accepted, &report, &substituted_configuration).is_err());
}

#[test]
fn public_v1_first_completion_rejects_substituted_input_and_policy() {
    let accepted = accepted();
    let observed = completion(
        &accepted,
        "frozen-swap",
        PublicV1CallKind::Swap,
        PublicV1OperationResult::Succeeded,
    );
    let requirement = requirement(PublicV1EvidenceConsumer::Switchover, &observed);

    let mut changed_digest = observed.clone();
    changed_digest.input_digest =
        CanonicalOperationInputDigest::V1(CanonicalOperationInputDigestV1 {
            sha256: "06".repeat(32),
        });
    assert!(
        plan_public_v1_evidence(&accepted, &report(&accepted, changed_digest), &requirement)
            .is_err()
    );

    let mut changed_replay = observed.clone();
    changed_replay.replay_disposition = PublicOperationReplayDisposition::ReevaluateCatchUp;
    assert!(
        plan_public_v1_evidence(&accepted, &report(&accepted, changed_replay), &requirement)
            .is_err()
    );

    let mut changed_mode = observed.clone();
    changed_mode.catch_up_mode =
        Some(kuberic_runtime::protocol::public_operations::PublicCatchUpMode::All);
    assert!(
        plan_public_v1_evidence(&accepted, &report(&accepted, changed_mode), &requirement).is_err()
    );

    let mut changed_capability = observed;
    changed_capability.catch_up_specific_quorum = Some(true);
    assert!(
        plan_public_v1_evidence(
            &accepted,
            &report(&accepted, changed_capability),
            &requirement
        )
        .is_err()
    );
}
