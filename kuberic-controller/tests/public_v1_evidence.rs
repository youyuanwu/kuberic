#![cfg(feature = "runtime-test-bridge")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kuberic_controller::cluster_api::PublicV1Scope;
use kuberic_controller::evaluator::testing::{
    PublicV1AcceptedTransitionIntent, PublicV1AcceptedTransitionStatus, PublicV1EvidenceConsumer,
    PublicV1EvidenceEffect, PublicV1EvidenceResource, PublicV1EvidenceState, PublicV1OperationApi,
    PublicV1ReconciliationOutcome, PublicV1ResourceAuthority, PublicV1ResourceDisposition,
    PublicV1ResourceProof, execute_public_v1_operation, reconcile_public_v1_consumer,
};
use kuberic_runtime::protocol::command::{ProtocolCommand, PublicV1OperationCommand};
use kuberic_runtime::protocol::public_operations::{
    CanonicalOperationInputDigest, CanonicalOperationInputDigestV1,
    CanonicalReplicaRebuildRequired, CapturedPublicOperationIdentity, ProviderReplicaIdentity,
    PublicBuildInput, PublicCatchUpMode, PublicConfiguration, PublicOperationClass,
    PublicOperationIntent, PublicOperationProgram, PublicOperationReplayDisposition,
    PublicProgress, PublicProgressRole, PublicV1AcceptedOperationCompletion,
    PublicV1AcceptedStatus, PublicV1AgentReport, PublicV1CallKind, PublicV1Identity,
    PublicV1OperationCompletion, PublicV1OperationResult, ReplicaRebuildReason,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, AgentGeneration, ConfigurationDescriptor, ConfigurationId, ConfigurationMember,
    Epoch, OperationId, PodUid, ProcessSessionId, PublicProtocolGeneration, PvcUid, ReplicaId,
    ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity,
    ReplicaStorageIncarnationContext, ResourceUid, StorageGeneration, derive_bootstrap_history,
    derive_history_initialization_id,
};

fn replica(id: i64) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
        agent_generation: AgentGeneration::new(format!("generation-{id}")),
    }
}

fn storage(id: i64) -> ReplicaStorageIncarnationContext {
    ReplicaStorageIncarnationContext {
        replica: replica(id),
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new(format!("pod-{id}")),
            pvc_uid: PvcUid::new(format!("pvc-{id}")),
        },
        storage_generation: StorageGeneration::new(format!("storage-{id}")),
    }
}

fn accepted() -> PublicV1AcceptedStatus {
    let resource_uid = ResourceUid::new("resource-1");
    let generation = PublicProtocolGeneration::new(42);
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    PublicV1AcceptedStatus {
        identity: PublicV1Identity::new(generation.value()),
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
        storage_incarnation: storage(1),
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
        replica: replica(1),
        process_session_id: ProcessSessionId::new("session-1"),
        report_sequence: 1,
        role: ReplicaRole::Primary,
        read_status: AccessStatus::NotPrimary,
        write_status: AccessStatus::NotPrimary,
        history: accepted.history.clone(),
        storage_incarnation: storage(1),
        progress: PublicProgress {
            role: PublicProgressRole::Primary,
            first: 1,
            last: 1,
        },
        service_location: None,
        data_loss_recovery: None,
        completed_operations: vec![completion],
        extensions: Default::default(),
    }
}

fn transition(
    accepted: PublicV1AcceptedStatus,
    consumer: PublicV1EvidenceConsumer,
    completion: &PublicV1OperationCompletion,
    target: Option<PublicV1ResourceAuthority>,
) -> PublicV1AcceptedTransitionStatus {
    PublicV1AcceptedTransitionStatus {
        resource_version: "status-rv-1".into(),
        intent: PublicV1AcceptedTransitionIntent {
            consumer,
            operation_id: completion.operation_id.clone(),
            revision: completion.revision,
            call_kind: completion.call_kind,
            history: completion.history.clone(),
            source: PublicV1ResourceAuthority {
                storage_incarnation: completion.storage_incarnation.clone(),
                resource_version: "source-rv-1".into(),
            },
            target,
            process_session_id: completion.process_session_id.clone(),
            input_digest: completion.input_digest.clone(),
            replay_disposition: completion.replay_disposition,
            previous_configuration_id: completion.previous_configuration_id.clone(),
            current_configuration_id: completion.current_configuration_id.clone(),
            catch_up_mode: completion.catch_up_mode,
            catch_up_specific_quorum: completion.catch_up_specific_quorum,
        },
        evidence: accepted,
    }
}

fn resource(accepted: &PublicV1AcceptedStatus, id: i64, version: &str) -> PublicV1EvidenceResource {
    let storage_incarnation = storage(id);
    PublicV1EvidenceResource {
        proof: Some(PublicV1ResourceProof {
            history: accepted.history.clone(),
            storage_incarnation: storage_incarnation.clone(),
        }),
        storage_incarnation,
        resource_version: version.into(),
        disposition: PublicV1ResourceDisposition::Active,
    }
}

fn state(
    accepted: &PublicV1AcceptedStatus,
    transition: PublicV1AcceptedTransitionStatus,
) -> PublicV1EvidenceState {
    PublicV1EvidenceState::new(
        transition,
        vec![
            resource(accepted, 1, "source-rv-1"),
            resource(accepted, 2, "target-rv-1"),
        ],
    )
    .unwrap()
}

async fn reconcile_to_effect(
    state: &PublicV1EvidenceState,
    report: &PublicV1AgentReport,
) -> Result<(), String> {
    assert_eq!(
        reconcile_public_v1_consumer(state, &test_scope(), report).await?,
        PublicV1ReconciliationOutcome::EvidencePersisted
    );
    assert_eq!(
        reconcile_public_v1_consumer(state, &test_scope(), report).await?,
        PublicV1ReconciliationOutcome::EffectApplied
    );
    Ok(())
}

fn call_for(consumer: PublicV1EvidenceConsumer) -> PublicV1CallKind {
    match consumer {
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
    }
}

struct DispatchProbe {
    observed: Mutex<Option<PublicV1AcceptedTransitionStatus>>,
    dispatches: AtomicUsize,
}

#[async_trait::async_trait]
impl PublicV1OperationApi for DispatchProbe {
    async fn observe_public_v1_operation(
        &self,
        _scope: &PublicV1Scope,
    ) -> Result<Option<PublicV1AcceptedTransitionStatus>, String> {
        Ok(self.observed.lock().unwrap().clone())
    }

    async fn dispatch_public_v1_operation(
        &self,
        _scope: &PublicV1Scope,
        _expected: &PublicV1AcceptedTransitionStatus,
        _command: &ProtocolCommand,
    ) -> Result<(), String> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn observe_public_v1_operation_report(
        &self,
        _scope: &PublicV1Scope,
        _expected: &PublicV1AcceptedTransitionStatus,
    ) -> Result<PublicV1AgentReport, String> {
        Err("dispatch probe does not publish runtime reports".into())
    }
}

fn test_scope() -> PublicV1Scope {
    PublicV1Scope {
        namespace: "kuberic-system".into(),
        name: "resource-1".into(),
        resource_uid: ResourceUid::new("resource-1"),
    }
}

fn operation_intent(
    accepted: &PublicV1AcceptedStatus,
    name: &str,
    program: PublicOperationProgram,
) -> PublicOperationIntent {
    let class = match &program {
        PublicOperationProgram::Swap { .. } => PublicOperationClass::PlannedSwap,
        PublicOperationProgram::Build(build) => PublicOperationClass::Build {
            target: build.replica.replica_id,
        },
        PublicOperationProgram::Remove(build) => PublicOperationClass::Remove {
            target: build.replica.replica_id,
        },
        _ => PublicOperationClass::Authority,
    };
    PublicOperationIntent {
        identity: accepted.identity.clone(),
        operation_id: OperationId::new(name),
        revision: 1,
        process_session_id: ProcessSessionId::new("source-session"),
        class,
        input_digest: program.digest(),
        replay_disposition: program.replay_disposition(),
        lifecycle: None,
        program: Some(program),
    }
}

#[tokio::test]
async fn wrong_operation_target_is_fenced_before_dispatch_for_every_mutating_program() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let accepted = accepted();
        let epoch = Epoch::new(0, 2);
        let source = replica(1);
        let target = replica(2);
        let configuration = |epoch| PublicConfiguration {
            current: ConfigurationDescriptor::new(
                epoch,
                source.replica_id,
                vec![ConfigurationMember {
                    identity: source.clone(),
                    role: ReplicaRole::Primary,
                }],
                1,
            ),
            previous: None,
            replicas: Vec::new(),
        };
        let build = |attempt: &str| PublicBuildInput {
            attempt: OperationId::new(attempt),
            epoch,
            current_configuration_id: Some(configuration(epoch).current.configuration_id),
            replica: target.clone(),
            target_storage: storage(2),
            process_session_id: ProcessSessionId::new("target-session"),
            replication_address: "http://127.0.0.1:2".into(),
            history: Some(accepted.history.clone()),
        };
        let cases = vec![
            (
                PublicV1CallKind::Role,
                operation_intent(
                    &accepted,
                    "wrong-target-role",
                    PublicOperationProgram::Role {
                        epoch,
                        role: ReplicaRole::Primary,
                    },
                ),
            ),
            (
                PublicV1CallKind::Epoch,
                operation_intent(
                    &accepted,
                    "wrong-target-epoch",
                    PublicOperationProgram::Epoch { epoch },
                ),
            ),
            (
                PublicV1CallKind::Swap,
                operation_intent(
                    &accepted,
                    "wrong-target-swap",
                    PublicOperationProgram::Swap {
                        starting: configuration(Epoch::new(0, 1)),
                        refreshed: configuration(epoch),
                        epoch,
                        handoff: ReplicaRole::None,
                        catch_up_specific_quorum: false,
                        mode: PublicCatchUpMode::All,
                        controller_composed: false,
                    },
                ),
            ),
            (
                PublicV1CallKind::Build,
                operation_intent(
                    &accepted,
                    "wrong-target-build",
                    PublicOperationProgram::Build(build("wrong-target-build")),
                ),
            ),
            (
                PublicV1CallKind::Remove,
                operation_intent(
                    &accepted,
                    "wrong-target-remove",
                    PublicOperationProgram::Remove(build("remove-attempt")),
                ),
            ),
        ];

        for (call_kind, intent) in cases {
            let (previous_configuration_id, current_configuration_id, catch_up_mode, specific) =
                match intent.program.as_ref().unwrap() {
                    PublicOperationProgram::Swap {
                        refreshed,
                        mode,
                        catch_up_specific_quorum,
                        ..
                    } => (
                        refreshed
                            .previous
                            .as_ref()
                            .map(|previous| previous.configuration_id.clone()),
                        Some(refreshed.current.configuration_id.clone()),
                        Some(*mode),
                        Some(*catch_up_specific_quorum),
                    ),
                    PublicOperationProgram::Build(build) => {
                        (None, build.current_configuration_id.clone(), None, None)
                    }
                    _ => (None, None, None, None),
                };
            let expected = PublicV1AcceptedTransitionStatus {
                resource_version: "target-fence-rv".into(),
                evidence: accepted.clone(),
                intent: PublicV1AcceptedTransitionIntent {
                    consumer: match call_kind {
                        PublicV1CallKind::Build => PublicV1EvidenceConsumer::ScaleUp,
                        PublicV1CallKind::Remove => PublicV1EvidenceConsumer::Removal,
                        PublicV1CallKind::Swap => PublicV1EvidenceConsumer::Switchover,
                        _ => PublicV1EvidenceConsumer::SessionRenewal,
                    },
                    operation_id: intent.operation_id.clone(),
                    revision: intent.revision,
                    call_kind,
                    history: accepted.history.clone(),
                    source: PublicV1ResourceAuthority {
                        storage_incarnation: storage(1),
                        resource_version: "source-rv-1".into(),
                    },
                    target: matches!(
                        call_kind,
                        PublicV1CallKind::Build | PublicV1CallKind::Remove
                    )
                    .then(|| PublicV1ResourceAuthority {
                        storage_incarnation: storage(2),
                        resource_version: "target-rv-1".into(),
                    }),
                    process_session_id: intent.process_session_id.clone(),
                    input_digest: intent.input_digest.clone(),
                    replay_disposition: intent.replay_disposition,
                    previous_configuration_id,
                    current_configuration_id,
                    catch_up_mode,
                    catch_up_specific_quorum: specific,
                },
            };
            let probe = DispatchProbe {
                observed: Mutex::new(Some(expected.clone())),
                dispatches: AtomicUsize::new(0),
            };
            let command = ProtocolCommand::PublicV1Operation(Box::new(PublicV1OperationCommand {
                target: target.clone(),
                intent,
            }));

            let error = execute_public_v1_operation(&probe, &test_scope(), &expected, command)
                .await
                .unwrap_err();
            assert!(error.contains("target differs from accepted source incarnation"));
            assert_eq!(probe.dispatches.load(Ordering::SeqCst), 0);
            assert_eq!(*probe.observed.lock().unwrap(), Some(expected));
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn consumer_reconciliation_derives_frozen_requirements_for_every_consumer() {
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
        let accepted = accepted();
        let completion = completion(
            &accepted,
            &format!("consumer-{index}"),
            call_for(consumer),
            PublicV1OperationResult::Succeeded,
        );
        let report = report(&accepted, completion.clone());
        let state = state(
            &accepted,
            transition(accepted.clone(), consumer, &completion, None),
        );

        reconcile_to_effect(&state, &report).await.unwrap();

        assert_eq!(
            state.effects(),
            vec![PublicV1EvidenceEffect::Accepted {
                consumer,
                operation_id: completion.operation_id,
                revision: completion.revision,
            }]
        );
    }
}

#[tokio::test]
async fn lost_accept_response_is_atomically_idempotent() {
    let accepted = accepted();
    let completion = completion(
        &accepted,
        "accept-once",
        PublicV1CallKind::Swap,
        PublicV1OperationResult::Succeeded,
    );
    let report = report(&accepted, completion.clone());
    let state = state(
        &accepted,
        transition(
            accepted.clone(),
            PublicV1EvidenceConsumer::Switchover,
            &completion,
            None,
        ),
    );

    reconcile_to_effect(&state, &report).await.unwrap();
    assert_eq!(
        reconcile_public_v1_consumer(&state, &test_scope(), &report)
            .await
            .unwrap(),
        PublicV1ReconciliationOutcome::EffectApplied
    );
    assert_eq!(state.effects().len(), 1);
}

#[tokio::test]
async fn concurrent_accept_reconciliation_commits_one_exact_effect() {
    let accepted = accepted();
    let completion = completion(
        &accepted,
        "concurrent-accept-once",
        PublicV1CallKind::Swap,
        PublicV1OperationResult::Succeeded,
    );
    let report = Arc::new(report(&accepted, completion.clone()));
    let state = Arc::new(state(
        &accepted,
        transition(
            accepted.clone(),
            PublicV1EvidenceConsumer::Switchover,
            &completion,
            None,
        ),
    ));
    assert_eq!(
        reconcile_public_v1_consumer(state.as_ref(), &test_scope(), report.as_ref())
            .await
            .unwrap(),
        PublicV1ReconciliationOutcome::EvidencePersisted
    );

    let scope = test_scope();
    let (first, second) = tokio::join!(
        reconcile_public_v1_consumer(state.as_ref(), &scope, report.as_ref()),
        reconcile_public_v1_consumer(state.as_ref(), &scope, report.as_ref()),
    );
    assert_eq!(first.unwrap(), PublicV1ReconciliationOutcome::EffectApplied);
    assert_eq!(
        second.unwrap(),
        PublicV1ReconciliationOutcome::EffectApplied
    );
    assert_eq!(state.effects().len(), 1);
}

#[tokio::test]
async fn hosted_public_v1_removal_drains_build_excludes_peer_and_rejects_stale_session() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let accepted = accepted();
        let completion = completion(
            &accepted,
            "retire-target-once",
            PublicV1CallKind::Remove,
            PublicV1OperationResult::Failed("exact removal requires retirement".into()),
        );
        let report = report(&accepted, completion.clone());
        let target = PublicV1ResourceAuthority {
            storage_incarnation: storage(2),
            resource_version: "target-rv-1".into(),
        };
        let state = state(
            &accepted,
            transition(
                accepted.clone(),
                PublicV1EvidenceConsumer::Removal,
                &completion,
                Some(target),
            ),
        );

        reconcile_to_effect(&state, &report).await.unwrap();
        reconcile_public_v1_consumer(&state, &test_scope(), &report)
            .await
            .unwrap();

        assert_eq!(state.effects().len(), 1);
        assert!(matches!(
            &state.effects()[0],
            PublicV1EvidenceEffect::Retired { storage: retired, .. }
                if **retired == storage(2)
        ));
        let resources = state.resources();
        assert_eq!(
            resources
                .iter()
                .find(|resource| resource.storage_incarnation == storage(1))
                .unwrap()
                .disposition,
            PublicV1ResourceDisposition::Active
        );
        assert_eq!(
            resources
                .iter()
                .find(|resource| resource.storage_incarnation == storage(2))
                .unwrap()
                .disposition,
            PublicV1ResourceDisposition::Retired
        );
    })
    .await
    .unwrap();
}

fn rebuild(
    accepted: &PublicV1AcceptedStatus,
    operation_id: &str,
) -> CanonicalReplicaRebuildRequired {
    CanonicalReplicaRebuildRequired::LocalProvider {
        reason: ReplicaRebuildReason::ProviderMismatch,
        provider: ProviderReplicaIdentity {
            replica: replica(1),
        },
        operation: CapturedPublicOperationIdentity {
            resource_uid: accepted.history.resource_uid.clone(),
            identity: accepted.identity.clone(),
            epoch: Epoch::new(0, 7),
            operation_id: OperationId::new(operation_id),
            revision: 3,
            detecting_session: ProcessSessionId::new("session-1"),
        },
        local_replica: replica(1),
        local_storage: PvcUid::new("pvc-1"),
    }
}

#[tokio::test]
async fn lost_local_provider_replacement_response_is_idempotent_and_replaces_source() {
    let accepted = accepted();
    let rebuild = rebuild(&accepted, "replace-target-once");
    let completion = completion(
        &accepted,
        "replace-target-once",
        PublicV1CallKind::Build,
        PublicV1OperationResult::RebuildRequired(Box::new(rebuild.clone())),
    );
    let report = report(&accepted, completion.clone());
    let target = PublicV1ResourceAuthority {
        storage_incarnation: storage(2),
        resource_version: "target-rv-1".into(),
    };
    let state = state(
        &accepted,
        transition(
            accepted.clone(),
            PublicV1EvidenceConsumer::RepairReplacement,
            &completion,
            Some(target),
        ),
    );

    reconcile_to_effect(&state, &report).await.unwrap();
    reconcile_public_v1_consumer(&state, &test_scope(), &report)
        .await
        .unwrap();

    assert_eq!(state.effects().len(), 1);
    assert!(matches!(
        &state.effects()[0],
        PublicV1EvidenceEffect::Replacement {
            storage: replaced,
            rebuild: observed,
            ..
        } if **replaced == storage(1) && **observed == rebuild
    ));
    let resources = state.resources();
    assert_eq!(
        resources
            .iter()
            .find(|resource| resource.storage_incarnation == storage(1))
            .unwrap()
            .disposition,
        PublicV1ResourceDisposition::ReplacementRequested
    );
    assert_eq!(
        resources
            .iter()
            .find(|resource| resource.storage_incarnation == storage(2))
            .unwrap()
            .disposition,
        PublicV1ResourceDisposition::Active
    );
}

#[tokio::test]
async fn consumer_reconciliation_rejects_stale_resource_versions_and_proof_free_resources() {
    let accepted = accepted();
    let completion = completion(
        &accepted,
        "conditional-target",
        PublicV1CallKind::Build,
        PublicV1OperationResult::Failed("target build failed".into()),
    );
    let report = report(&accepted, completion.clone());
    let stale_target = PublicV1ResourceAuthority {
        storage_incarnation: storage(2),
        resource_version: "stale-target-rv".into(),
    };
    let stale = state(
        &accepted,
        transition(
            accepted.clone(),
            PublicV1EvidenceConsumer::RepairReplacement,
            &completion,
            Some(stale_target),
        ),
    );
    assert_eq!(
        reconcile_public_v1_consumer(&stale, &test_scope(), &report)
            .await
            .unwrap(),
        PublicV1ReconciliationOutcome::EvidencePersisted
    );
    assert!(
        reconcile_public_v1_consumer(&stale, &test_scope(), &report)
            .await
            .unwrap_err()
            .contains("stale public-v1 evidence resource version")
    );
    assert!(stale.effects().is_empty());

    let target = PublicV1ResourceAuthority {
        storage_incarnation: storage(2),
        resource_version: "target-rv-1".into(),
    };
    let proof_free_transition = transition(
        accepted.clone(),
        PublicV1EvidenceConsumer::RepairReplacement,
        &completion,
        Some(target),
    );
    let proof_free = PublicV1EvidenceState::new(
        proof_free_transition,
        vec![
            resource(&accepted, 1, "source-rv-1"),
            PublicV1EvidenceResource {
                proof: None,
                ..resource(&accepted, 2, "target-rv-1")
            },
        ],
    )
    .unwrap();
    reconcile_public_v1_consumer(&proof_free, &test_scope(), &report)
        .await
        .unwrap();
    assert!(
        reconcile_public_v1_consumer(&proof_free, &test_scope(), &report)
            .await
            .unwrap_err()
            .contains("lacks accepted proof")
    );
    assert!(proof_free.effects().is_empty());
}

#[tokio::test]
async fn consumer_reconciliation_rejects_predecessor_reports_and_substituted_authority() {
    let accepted = accepted();
    let completion = completion(
        &accepted,
        "fenced-report",
        PublicV1CallKind::FailoverPromotion,
        PublicV1OperationResult::Succeeded,
    );
    let mut predecessor = report(&accepted, completion.clone());
    predecessor.process_session_id = ProcessSessionId::new("predecessor-session");
    predecessor.completed_operations[0].process_session_id =
        ProcessSessionId::new("predecessor-session");
    let state = state(
        &accepted,
        transition(
            accepted.clone(),
            PublicV1EvidenceConsumer::Failover,
            &completion,
            None,
        ),
    );
    assert!(
        reconcile_public_v1_consumer(&state, &test_scope(), &predecessor)
            .await
            .unwrap_err()
            .contains("accepted transition authority")
    );

    let mut substituted = report(&accepted, completion);
    substituted.completed_operations[0].current_configuration_id =
        Some(ConfigurationId::new("substituted"));
    assert!(
        reconcile_public_v1_consumer(&state, &test_scope(), &substituted)
            .await
            .unwrap_err()
            .contains("frozen accepted transition authority")
    );
}

#[tokio::test]
async fn consumer_reconciliation_waits_without_manufacturing_proof() {
    let accepted = accepted();
    let completion = completion(
        &accepted,
        "not-yet-reported",
        PublicV1CallKind::Epoch,
        PublicV1OperationResult::Succeeded,
    );
    let mut empty_report = report(&accepted, completion.clone());
    empty_report.completed_operations.clear();
    let state = state(
        &accepted,
        transition(
            accepted.clone(),
            PublicV1EvidenceConsumer::SessionRenewal,
            &completion,
            None,
        ),
    );

    assert_eq!(
        reconcile_public_v1_consumer(&state, &test_scope(), &empty_report)
            .await
            .unwrap(),
        PublicV1ReconciliationOutcome::Waiting
    );
    assert!(state.effects().is_empty());
    assert!(state.transition().evidence.completed_operations.is_empty());
}

#[tokio::test]
async fn persisted_completion_must_remain_exact_on_later_reconciliation() {
    let mut accepted = accepted();
    let completion = completion(
        &accepted,
        "persisted-exact",
        PublicV1CallKind::Remove,
        PublicV1OperationResult::Succeeded,
    );
    accepted
        .completed_operations
        .push(PublicV1AcceptedOperationCompletion::from_completion(
            &completion,
        ));
    let state = state(
        &accepted,
        transition(
            accepted.clone(),
            PublicV1EvidenceConsumer::Removal,
            &completion,
            None,
        ),
    );
    let mut changed = completion.clone();
    changed.revision += 1;

    assert!(
        reconcile_public_v1_consumer(&state, &test_scope(), &report(&accepted, changed))
            .await
            .is_err()
    );
    assert!(state.effects().is_empty());
}
