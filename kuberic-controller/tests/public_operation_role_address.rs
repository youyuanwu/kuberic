#![cfg(feature = "runtime-test-bridge")]

use std::sync::Mutex;

use async_trait::async_trait;
use kuberic_controller::evaluator::test_bridge::*;
use kuberic_runtime::protocol::public_operations::*;
use kuberic_runtime::protocol::types::*;

struct Api {
    state: Mutex<(PreviewAcceptedStatus, PreviewWriteService)>,
    calls: Mutex<usize>,
    lose_response: Option<usize>,
    unavailable: bool,
}

impl Api {
    fn new() -> Self {
        Self {
            state: Mutex::new((PreviewAcceptedStatus::default(), serde_json::from_value(serde_json::json!({
                "metadata":{"uid":"service-1","name":"write","resourceVersion":"1","annotations":{"unrelated":"retained"}},
                "spec":{"selector":{"operator.kuberic.io/instance":"disabled"}}
            })).unwrap())),
            calls: Mutex::new(0), lose_response: None, unavailable: false,
        }
    }

    fn after_write(&self) -> Result<(), String> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if self.lose_response == Some(*calls) {
            Err("response lost after durable write".into())
        } else {
            Ok(())
        }
    }

    fn restart(&self) -> Self {
        let bytes = serde_json::to_vec(&*self.state.lock().unwrap()).unwrap();
        Self {
            state: Mutex::new(serde_json::from_slice(&bytes).unwrap()),
            calls: Mutex::new(0),
            lose_response: None,
            unavailable: false,
        }
    }
}

#[async_trait]
impl PreviewServiceApi for Api {
    async fn observe_preview(
        &self,
    ) -> Result<(PreviewAcceptedStatus, PreviewWriteService), String> {
        if self.unavailable {
            return Err("Kubernetes unavailable".into());
        }
        Ok(self.state.lock().unwrap().clone())
    }
    async fn persist_preview_status(
        &self,
        expected: &PreviewAcceptedStatus,
        next: &PreviewAcceptedStatus,
    ) -> Result<(), String> {
        if self.unavailable {
            return Err("Kubernetes unavailable".into());
        }
        {
            let mut state = self.state.lock().unwrap();
            if &state.0 != expected {
                return Err("status precondition failed".into());
            }
            state.0 = next.clone();
        }
        self.after_write()
    }
    async fn replace_preview_service(
        &self,
        mut service: PreviewWriteService,
    ) -> Result<(), String> {
        if self.unavailable {
            return Err("Kubernetes unavailable".into());
        }
        {
            let mut state = self.state.lock().unwrap();
            if service.metadata.uid != state.1.metadata.uid
                || service.metadata.resource_version != state.1.metadata.resource_version
            {
                return Err("Service precondition failed".into());
            }
            service.metadata.resource_version = Some(format!(
                "{}+",
                service.metadata.resource_version.as_ref().unwrap()
            ));
            // Selector and annotation become observable together in this single write.
            state.1 = service;
        }
        self.after_write()
    }
}

fn fixture(
    revision: u64,
    address: Option<&str>,
) -> (
    PublicOperationPreviewEvaluationConfig,
    PublicOperationIntent,
    PublicLifecycleReport,
) {
    let preview = PublicOperationPreviewIdentity::new(42);
    let config = PublicOperationPreviewEvaluationConfig::new(preview.clone(), Default::default());
    let replica = ReplicaIdentity {
        replica_id: ReplicaId::new(1),
        instance_id: ReplicaInstanceId::new("pod-1"),
        agent_generation: AgentGeneration::new("generation-1"),
    };
    let epoch = Epoch::new(57, revision as i64);
    let intent = plan_public_lifecycle(
        &config,
        OperationId::new(format!("role-{revision}")),
        revision,
        ProcessSessionId::new("session-1"),
        PublicLifecycleInput {
            recipe: PublicLifecycleRecipe::InitialPrimary,
            replica: replica.clone(),
            epoch,
            possible_data_loss: PossibleDataLossIntent::Possible,
            current: ConfigurationDescriptor::new(
                epoch,
                replica.replica_id,
                vec![ConfigurationMember {
                    identity: replica.clone(),
                    role: ReplicaRole::Primary,
                }],
                1,
            ),
            previous: None,
        },
        PreviewTransition::Ordinary,
    )
    .unwrap();
    let report = PublicLifecycleReport {
        preview: preview.clone(),
        resource_uid: ResourceUid::new("resource-1"),
        replica: replica.clone(),
        process_session_id: intent.process_session_id.clone(),
        revision,
        operation_id: Some(intent.operation_id.clone()),
        role: ReplicaRole::Primary,
        write_access: true,
        service_location: address.map(|address| ServiceLocation {
            preview,
            resource_uid: ResourceUid::new("resource-1"),
            replica,
            process_session_id: intent.process_session_id.clone(),
            revision,
            operation_id: intent.operation_id.clone(),
            epoch,
            address: address.into(),
        }),
    };
    (config, intent, report)
}

async fn plan(
    api: &Api,
    config: &PublicOperationPreviewEvaluationConfig,
    authority: &PublicOperationIntent,
    report: &PublicLifecycleReport,
) -> PreviewServiceLocationPlan {
    let (status, service) = api.observe_preview().await.unwrap();
    evaluate_service_location(
        config,
        &ResourceUid::new("resource-1"),
        authority,
        Some(report),
        &status,
        &service,
    )
    .unwrap()
}

async fn converge(
    api: &Api,
    config: &PublicOperationPreviewEvaluationConfig,
    authority: &PublicOperationIntent,
    report: &PublicLifecycleReport,
) {
    for _ in 0..8 {
        let next = plan(api, config, authority, report).await;
        if next == PreviewServiceLocationPlan::Stable {
            return;
        }
        execute_preview_service_location(api, next).await.unwrap();
    }
    panic!("did not converge");
}

#[tokio::test]
async fn public_operation_role_address_status_service_status_crash_and_response_loss_matrix() {
    for address in [Some("opaque://one"), Some(""), None] {
        for cut in 0..=3 {
            for response_loss in [false, true] {
                let (config, authority, report) = fixture(1, address);
                let mut api = Api::new();
                api.lose_response = response_loss.then_some(cut);
                for _ in 0..cut {
                    let action = plan(&api, &config, &authority, &report).await;
                    let _ = execute_preview_service_location(&api, action).await;
                }
                let restarted = api.restart();
                let config = PublicOperationPreviewEvaluationConfig::new(
                    config.identity.clone(),
                    Default::default(),
                );
                converge(&restarted, &config, &authority, &report).await;
                let (status, service) = restarted.observe_preview().await.unwrap();
                let PreviewServiceLocationStage::Published(projection) =
                    status.service_location_projection
                else {
                    panic!("not published")
                };
                assert_eq!(projection.location, report.service_location);
                assert!(preview_service_matches(&service, &projection));
                assert_eq!(
                    service.metadata.annotations.unwrap()["unrelated"],
                    "retained"
                );
            }
        }
    }
}

#[tokio::test]
async fn public_operation_role_address_pending_precedes_service_then_exact_reobservation() {
    let (config, authority, report) = fixture(1, Some("opaque://one"));
    let api = Api::new();
    let pending = plan(&api, &config, &authority, &report).await;
    assert!(matches!(
        pending,
        PreviewServiceLocationPlan::PersistStatus { .. }
    ));
    execute_preview_service_location(&api, pending)
        .await
        .unwrap();
    let (status, service) = api.observe_preview().await.unwrap();
    assert!(matches!(
        status.service_location_projection,
        PreviewServiceLocationStage::Pending(_)
    ));
    assert_eq!(
        service.spec.unwrap().selector.unwrap()["operator.kuberic.io/instance"],
        "disabled"
    );
    let write = plan(&api, &config, &authority, &report).await;
    assert!(matches!(
        write,
        PreviewServiceLocationPlan::WriteService { .. }
    ));
    execute_preview_service_location(&api, write).await.unwrap();
    assert!(matches!(
        api.observe_preview()
            .await
            .unwrap()
            .0
            .service_location_projection,
        PreviewServiceLocationStage::Pending(_)
    ));
    let published = plan(&api, &config, &authority, &report).await;
    assert!(matches!(
        published,
        PreviewServiceLocationPlan::PersistStatus { .. }
    ));
    execute_preview_service_location(&api, published)
        .await
        .unwrap();
    assert_eq!(
        plan(&api, &config, &authority, &report).await,
        PreviewServiceLocationPlan::Stable
    );
}

#[tokio::test]
async fn public_operation_role_address_pending_role_can_publish_exact_completion() {
    let (config, authority, pending) = fixture(1, None);
    let (_, same_authority, completed) = fixture(1, Some("ready"));
    assert_eq!(authority, same_authority);
    let api = Api::new();
    converge(&api, &config, &authority, &pending).await;
    let (status, _) = api.observe_preview().await.unwrap();
    let PreviewServiceLocationStage::Published(projection) = status.service_location_projection
    else {
        panic!("pending role absence was not published")
    };
    assert!(!projection.revoked && projection.location.is_none());

    converge(&api, &config, &authority, &completed).await;
    let (status, service) = api.observe_preview().await.unwrap();
    let PreviewServiceLocationStage::Published(projection) = status.service_location_projection
    else {
        panic!("exact role completion was not published")
    };
    assert_eq!(projection.location, completed.service_location);
    assert!(!projection.revoked);
    assert!(preview_service_matches(&service, &projection));
}

#[tokio::test]
async fn public_operation_role_address_delayed_old_revision_and_resource_replacement_are_fenced() {
    for delayed_status in [false, true] {
        let (config, old, old_report) = fixture(1, Some("old"));
        let (_, new, new_report) = fixture(2, Some("new"));
        let api = Api::new();
        let action = plan(&api, &config, &old, &old_report).await;
        execute_preview_service_location(&api, action)
            .await
            .unwrap();
        if delayed_status {
            let action = plan(&api, &config, &old, &old_report).await;
            execute_preview_service_location(&api, action)
                .await
                .unwrap();
        }
        let delayed = plan(&api, &config, &old, &old_report).await;
        converge(&api, &config, &new, &new_report).await;
        assert!(
            execute_preview_service_location(&api, delayed)
                .await
                .is_err()
        );
        let (status, service) = api.observe_preview().await.unwrap();
        assert!(
            evaluate_service_location(
                &config,
                &ResourceUid::new("resource-1"),
                &old,
                Some(&old_report),
                &status,
                &service,
            )
            .is_err()
        );
        assert_eq!(
            plan(&api, &config, &new, &new_report).await,
            PreviewServiceLocationPlan::Stable
        );
    }
    let (config, authority, report) = fixture(1, Some("old"));
    let api = Api::new();
    let pending = plan(&api, &config, &authority, &report).await;
    execute_preview_service_location(&api, pending)
        .await
        .unwrap();
    let old_write = plan(&api, &config, &authority, &report).await;
    api.state.lock().unwrap().1.metadata.uid = Some("replacement-service".into());
    assert!(
        execute_preview_service_location(&api, old_write)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn public_operation_role_address_clears_old_location_before_new_publication() {
    let (config, old, old_report) = fixture(1, Some("old"));
    let (_, new, new_report) = fixture(2, Some("new"));
    let api = Api::new();
    converge(&api, &config, &old, &old_report).await;

    let pending_clear = plan(&api, &config, &new, &new_report).await;
    let PreviewServiceLocationPlan::PersistStatus { next, .. } = &pending_clear else {
        panic!("new authority did not begin with a persisted clear")
    };
    let PreviewServiceLocationStage::Pending(projection) = &next.service_location_projection else {
        panic!("clear was not pending")
    };
    assert!(projection.location.is_none());
    assert_eq!(projection.deferred_location, new_report.service_location);
    execute_preview_service_location(&api, pending_clear)
        .await
        .unwrap();

    let service_clear = plan(&api, &config, &new, &new_report).await;
    let PreviewServiceLocationPlan::WriteService { projection, .. } = &service_clear else {
        panic!("pending clear did not mutate the Service")
    };
    assert!(projection.location.is_none());
    execute_preview_service_location(&api, service_clear)
        .await
        .unwrap();

    let publish_clear = plan(&api, &config, &new, &new_report).await;
    let PreviewServiceLocationPlan::PersistStatus { next, .. } = &publish_clear else {
        panic!("cleared Service was not published absent")
    };
    let PreviewServiceLocationStage::Published(projection) = &next.service_location_projection
    else {
        panic!("clear was not published")
    };
    assert!(projection.location.is_none());
    assert_eq!(projection.deferred_location, new_report.service_location);
    execute_preview_service_location(&api, publish_clear)
        .await
        .unwrap();

    let pending_publish = plan(&api, &config, &new, &new_report).await;
    let PreviewServiceLocationPlan::PersistStatus { next, .. } = pending_publish else {
        panic!("new location did not begin after published absence")
    };
    let PreviewServiceLocationStage::Pending(projection) = next.service_location_projection else {
        panic!("new location was not pending")
    };
    assert_eq!(projection.location, new_report.service_location);
    assert!(projection.deferred_location.is_none());
}

#[tokio::test]
async fn public_operation_role_address_newer_authority_preserves_inflight_clear() {
    let (config, first, first_report) = fixture(1, Some("first"));
    let (_, second, second_report) = fixture(2, Some("second"));
    let (_, third, third_report) = fixture(3, Some("third"));
    let api = Api::new();
    converge(&api, &config, &first, &first_report).await;

    let second_clear = plan(&api, &config, &second, &second_report).await;
    execute_preview_service_location(&api, second_clear)
        .await
        .unwrap();
    let (status, service) = api.observe_preview().await.unwrap();
    let PreviewServiceLocationStage::Pending(second_projection) =
        status.service_location_projection
    else {
        panic!("second authority did not retain a pending clear")
    };

    assert!(second_projection.location.is_none());
    assert_eq!(
        second_projection.deferred_location,
        second_report.service_location
    );
    assert_eq!(
        service.metadata.annotations.unwrap()["operator.kuberic.io/preview-service-location"],
        "first"
    );

    let third_clear = plan(&api, &config, &third, &third_report).await;
    let PreviewServiceLocationPlan::PersistStatus { next, .. } = third_clear else {
        panic!("third authority bypassed the persisted clear")
    };
    let PreviewServiceLocationStage::Pending(third_projection) = next.service_location_projection
    else {
        panic!("third authority clear was not pending")
    };
    assert!(third_projection.location.is_none());
    assert_eq!(
        third_projection.deferred_location,
        third_report.service_location
    );
}

#[tokio::test]
async fn public_operation_role_address_completion_cannot_bypass_required_clear() {
    let (config, first, first_report) = fixture(1, Some("first"));
    let (_, second, second_pending) = fixture(2, None);
    let (_, same_second, second_completed) = fixture(2, Some("second"));
    assert_eq!(second, same_second);
    let api = Api::new();
    converge(&api, &config, &first, &first_report).await;

    let pending_clear = plan(&api, &config, &second, &second_pending).await;
    let PreviewServiceLocationPlan::PersistStatus { next, .. } = &pending_clear else {
        panic!("pending authority did not persist a clear")
    };
    let PreviewServiceLocationStage::Pending(projection) = &next.service_location_projection else {
        panic!("pending authority clear was not pending")
    };
    assert!(projection.location.is_none());
    assert!(projection.deferred_location.is_none());
    assert!(projection.clear_required);
    execute_preview_service_location(&api, pending_clear)
        .await
        .unwrap();

    let completed = plan(&api, &config, &second, &second_completed).await;
    let PreviewServiceLocationPlan::PersistStatus { next, .. } = completed else {
        panic!("role completion bypassed the durable clear stage")
    };
    let PreviewServiceLocationStage::Pending(projection) = next.service_location_projection else {
        panic!("completed authority clear was not pending")
    };
    assert!(projection.location.is_none());
    assert_eq!(
        projection.deferred_location,
        second_completed.service_location
    );
    assert!(projection.clear_required);
}

#[tokio::test]
async fn public_operation_role_address_closed_mixed_stale_fresh_session_reports_clear() {
    for case in 0..8 {
        let (config, authority, mut report) = fixture(1, Some("old"));
        let api = Api::new();
        converge(&api, &config, &authority, &report).await;
        match case {
            0 => report.write_access = false,
            1 => report.role = ReplicaRole::ActiveSecondary,
            2 => report.service_location = None,
            3 => report.process_session_id = ProcessSessionId::new("fresh"),
            4 => report.revision = 0,
            5 => report.service_location.as_mut().unwrap().revision = 0,
            6 => report.service_location.as_mut().unwrap().operation_id = OperationId::new("stale"),
            7 => report.service_location.as_mut().unwrap().epoch = Epoch::new(0, 0),
            _ => unreachable!(),
        }
        converge(&api, &config, &authority, &report).await;
        let (status, service) = api.observe_preview().await.unwrap();
        let PreviewServiceLocationStage::Published(projection) = status.service_location_projection
        else {
            panic!("not cleared")
        };
        assert!(projection.location.is_none());
        assert!(preview_service_matches(&service, &projection));
    }
    let (config, authority, mut report) = fixture(1, Some("old"));
    report.preview = PublicOperationPreviewIdentity::new(999);
    let api = Api::new();
    let (status, service) = api.observe_preview().await.unwrap();
    assert!(
        evaluate_service_location(
            &config,
            &ResourceUid::new("resource-1"),
            &authority,
            Some(&report),
            &status,
            &service,
        )
        .is_err()
    );
}

#[tokio::test]
async fn public_operation_role_address_fresh_session_clear_fences_predecessor_replay() {
    let (config, authority, predecessor) = fixture(1, Some("old"));
    let api = Api::new();
    converge(&api, &config, &authority, &predecessor).await;

    let mut fresh = predecessor.clone();
    fresh.process_session_id = ProcessSessionId::new("fresh-session");
    fresh.service_location = None;
    converge(&api, &config, &authority, &fresh).await;

    let (status, service) = api.observe_preview().await.unwrap();
    let PreviewServiceLocationStage::Published(projection) = &status.service_location_projection
    else {
        panic!("fresh-session clear was not published")
    };
    assert!(projection.location.is_none());
    assert_eq!(projection.resource_uid, ResourceUid::new("resource-1"));
    assert_eq!(projection.primary, predecessor.replica);
    assert_eq!(projection.process_session_id, authority.process_session_id);
    assert_eq!(projection.revision, authority.revision);
    assert_eq!(
        projection.address_digest,
        service_location_address_digest(None)
    );
    assert_eq!(projection.service_uid, "service-1");
    assert_eq!(
        projection.service_resource_version,
        service.metadata.resource_version.clone().unwrap()
    );
    assert!(preview_service_matches(&service, projection));
    assert!(
        evaluate_service_location(
            &config,
            &ResourceUid::new("resource-1"),
            &authority,
            Some(&predecessor),
            &status,
            &service,
        )
        .is_err()
    );
}

#[tokio::test]
async fn public_operation_role_address_rejects_inconsistent_persisted_location_fences() {
    let (config, authority, report) = fixture(1, Some("old"));
    let api = Api::new();
    converge(&api, &config, &authority, &report).await;
    let (status, service) = api.observe_preview().await.unwrap();

    for case in 0..4 {
        let mut changed = status.clone();
        let PreviewServiceLocationStage::Published(projection) =
            &mut changed.service_location_projection
        else {
            panic!("fixture was not published")
        };
        match case {
            0 => projection.location.as_mut().unwrap().preview.generation += 1,
            1 => projection.location.as_mut().unwrap().operation_id = OperationId::new("different"),
            2 => projection.location.as_mut().unwrap().epoch = Epoch::new(999, 1),
            3 => {
                projection.authority = PublicOperationIntent {
                    preview: authority.preview.clone(),
                    operation_id: OperationId::new("terminal"),
                    revision: 2,
                    process_session_id: authority.process_session_id.clone(),
                    class: PublicOperationClass::Abort,
                    input_digest: "terminal".into(),
                    lifecycle: None,
                    program: None,
                };
                projection.process_session_id = projection.authority.process_session_id.clone();
                projection.revision = projection.authority.revision;
            }
            _ => unreachable!(),
        }
        assert!(
            evaluate_service_location(
                &config,
                &ResourceUid::new("resource-1"),
                &authority,
                Some(&report),
                &changed,
                &service,
            )
            .is_err(),
            "persisted fence case {case} was accepted"
        );
    }
}

#[tokio::test]
async fn public_operation_role_address_unavailable_kubernetes_retains_pending_and_converges_clear()
{
    let (config, authority, mut report) = fixture(1, Some("old"));
    let mut api = Api::new();
    converge(&api, &config, &authority, &report).await;
    report.write_access = false;
    let pending_clear = plan(&api, &config, &authority, &report).await;
    api.unavailable = true;
    assert!(
        execute_preview_service_location(&api, pending_clear)
            .await
            .is_err()
    );
    api.unavailable = false;
    let pending_clear = plan(&api, &config, &authority, &report).await;
    execute_preview_service_location(&api, pending_clear)
        .await
        .unwrap();
    let service_clear = plan(&api, &config, &authority, &report).await;
    api.unavailable = true;
    assert!(
        execute_preview_service_location(&api, service_clear)
            .await
            .is_err()
    );
    let restarted = api.restart();
    converge(&restarted, &config, &authority, &report).await;
    let (status, service) = restarted.observe_preview().await.unwrap();
    let PreviewServiceLocationStage::Published(projection) = status.service_location_projection
    else {
        panic!("not cleared")
    };
    assert!(projection.location.is_none() && preview_service_matches(&service, &projection));
}

#[tokio::test]
async fn public_operation_role_address_publication_reobserves_service_and_drift_reenters_pending() {
    let (config, authority, report) = fixture(1, Some("address"));
    let api = Api::new();
    for _ in 0..2 {
        let next = plan(&api, &config, &authority, &report).await;
        execute_preview_service_location(&api, next).await.unwrap();
    }
    let publication = plan(&api, &config, &authority, &report).await;
    api.state.lock().unwrap().1.metadata.resource_version = Some("external-write".into());
    assert!(
        execute_preview_service_location(&api, publication)
            .await
            .is_err()
    );
    converge(&api, &config, &authority, &report).await;
    api.state.lock().unwrap().1.spec.as_mut().unwrap().selector = None;
    let repair = plan(&api, &config, &authority, &report).await;
    assert!(matches!(
        repair,
        PreviewServiceLocationPlan::PersistStatus { .. }
    ));
    execute_preview_service_location(&api, repair)
        .await
        .unwrap();
    converge(&api, &config, &authority, &report).await;
}
