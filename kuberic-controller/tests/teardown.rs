mod support;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use k8s_openapi::api::core::v1::{ConfigMap, Pod, Service};
use kube::{
    api::{DeleteParams, PostParams, Preconditions},
    Api, ResourceExt,
};
use kuberic_controller::{
    evaluate,
    resources::{self, FINALIZER},
    CleanupReceipt, CleanupState, ControllerPhase, FmApi, FrozenCohort, KubericSet, Plan,
    TeardownStage,
};

use support::{api::Kind, Fixture, NAME, NAMESPACE};

fn receipt() -> CleanupReceipt {
    CleanupReceipt {
        node_reaped: true,
        node_exit_success: true,
        application_reaped: true,
        resources_joined: true,
        forced: false,
    }
}

async fn request_delete(fixture: &Fixture) {
    let set = fixture.set().await;
    Api::<KubericSet>::namespaced(fixture.client.clone(), NAMESPACE)
        .delete(
            NAME,
            &DeleteParams {
                preconditions: Some(Preconditions {
                    uid: set.uid(),
                    resource_version: set.resource_version(),
                }),
                ..DeleteParams::default()
            },
        )
        .await
        .unwrap();
}

async fn drive_unrouted(fixture: &Fixture) {
    for _ in 0..64 {
        let outcome = fixture.controller.reconcile(NAME).await.unwrap();
        assert!(outcome.blocked.is_none(), "{outcome:?}");
        let set = fixture.set().await;
        let status = set.status.as_ref().unwrap();
        assert_ne!(status.phase, ControllerPhase::Serving);
        assert!(status.service_endpoint.is_none());
        if let Some(service) = Api::<Service>::namespaced(fixture.client.clone(), NAMESPACE)
            .get_opt(&resources::write_service_name(NAME))
            .await
            .unwrap()
        {
            assert_eq!(
                service.spec.unwrap().selector,
                Some(resources::disabled_selector(&set).unwrap())
            );
        }
        if !outcome.changed {
            fixture.api.check();
            return;
        }
    }
    panic!("deleting bootstrap did not reach a waiting state");
}

async fn begin(fixture: &Fixture) -> FrozenCohort {
    fixture.serving().await;
    let cohort = fixture.set().await.status.unwrap().cohort().unwrap();
    request_delete(fixture).await;
    fixture.drive().await;
    assert_eq!(
        fixture.set().await.status.unwrap().teardown.unwrap().stage,
        TeardownStage::NativeDeletion
    );
    assert_eq!(fixture.fm.shutdowns.load(Ordering::SeqCst), 1);
    for pod in fixture.pods().await {
        assert!(pod.metadata.deletion_timestamp.is_none());
    }
    let service = Api::<Service>::namespaced(fixture.client.clone(), NAMESPACE)
        .get(&resources::write_service_name(NAME))
        .await
        .unwrap();
    assert_eq!(
        service.spec.unwrap().selector,
        Some(resources::disabled_selector(&fixture.set().await).unwrap())
    );
    cohort
}

async fn native_deletes(fixture: &Fixture) {
    for _ in 0..3 {
        let command = fixture.fm.command().await;
        assert_eq!(
            command.message.properties().unwrap().action,
            "DeleteReplica"
        );
        fixture.fm.complete(&command).await;
    }
}

async fn pod_deletions(fixture: &Fixture) {
    native_deletes(fixture).await;
    fixture.drive().await;
    assert_eq!(
        fixture.set().await.status.unwrap().teardown.unwrap().stage,
        TeardownStage::Pods
    );
    for pod in fixture.pods().await {
        assert!(pod.metadata.deletion_timestamp.is_some());
        assert_eq!(pod.metadata.deletion_grace_period_seconds, Some(30));
    }
}

async fn joined_processes(fixture: &Fixture, cohort: &FrozenCohort) {
    for (index, node) in cohort.allocation.nodes.iter().enumerate() {
        fixture.fm.disconnect(index).await;
        fixture
            .processes
            .publish(cohort, index, CleanupState::Complete(receipt()))
            .await;
        fixture.api.finalize(
            NAMESPACE,
            Kind::Pod,
            &node.pod_name,
            &cohort.bindings[index].pod_uid,
        );
    }
}

async fn drive_to(controller: &kuberic_controller::Reconciler, predicate: impl Fn(&Plan) -> bool) {
    for _ in 0..64 {
        let observation = controller.observe(NAME).await.unwrap();
        let plan = evaluate(&observation, &support::settings());
        if predicate(&plan) {
            return;
        }
        let outcome = controller.reconcile(NAME).await.unwrap();
        assert!(outcome.blocked.is_none(), "{outcome:?}");
    }
    panic!("expected teardown stage was not reached");
}

async fn assert_gone(fixture: &Fixture, cohort: &FrozenCohort) {
    let sets: Api<KubericSet> = Api::namespaced(fixture.client.clone(), NAMESPACE);
    assert!(sets.get_opt(NAME).await.unwrap().is_none());
    for node in &cohort.allocation.nodes {
        assert!(Api::<Pod>::namespaced(fixture.client.clone(), NAMESPACE)
            .get_opt(&node.pod_name)
            .await
            .unwrap()
            .is_none());
        assert!(
            Api::<ConfigMap>::namespaced(fixture.client.clone(), NAMESPACE)
                .get_opt(&node.config_map_name)
                .await
                .unwrap()
                .is_none()
        );
    }
    assert!(
        Api::<Service>::namespaced(fixture.client.clone(), NAMESPACE)
            .get_opt(&resources::write_service_name(NAME))
            .await
            .unwrap()
            .is_none()
    );
    let native = fixture.fm.observe().await.unwrap().unwrap();
    assert_eq!(native.status.phase, kuberic_fm::Phase::Stopped);
    assert!(native.joined);
    assert_eq!(fixture.fm.starts.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.fm.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.fm.joins.load(Ordering::SeqCst), 1);
    fixture.api.check();
}

#[tokio::test]
async fn finalizer_precedes_provisioning_and_empty_deletion_needs_no_runtime() {
    let fixture = Fixture::new().await;
    fixture.controller.reconcile(NAME).await.unwrap();
    let set = fixture.set().await;
    assert!(set.finalizers().iter().any(|value| value == FINALIZER));
    assert!(set.status.is_none());
    assert!(fixture.pods().await.is_empty());
    request_delete(&fixture).await;
    fixture.drive().await;
    assert!(
        fixture
            .controller
            .reconcile(NAME)
            .await
            .unwrap()
            .resource_absent
    );
    assert_eq!(fixture.fm.starts.load(Ordering::SeqCst), 0);
    fixture.api.check();
}

#[tokio::test]
async fn teardown_requires_native_deletion_then_owned_joins_before_resource_release() {
    let fixture = Fixture::new().await;
    let cohort = begin(&fixture).await;
    let original = fixture.set().await.status.unwrap().teardown.unwrap();
    let lost = fixture.fm.command().await;
    fixture.drive().await;
    assert!(fixture
        .api
        .deletes()
        .iter()
        .all(|(kind, ..)| *kind == Kind::Set));
    let retry = fixture.fm.retry().await;
    assert_eq!(retry, lost);
    fixture.fm.complete(&retry).await;
    fixture.fm.complete(&lost).await;
    for _ in 0..2 {
        let command = fixture.fm.command().await;
        fixture.fm.complete(&command).await;
    }
    fixture.drive().await;
    for pod in fixture.pods().await {
        assert!(pod.metadata.deletion_timestamp.is_some());
    }
    let retained = fixture.set().await.status.unwrap().teardown.unwrap();
    assert_eq!(retained.cohort, original.cohort);
    assert_eq!(retained.deadline_unix_ms, original.deadline_unix_ms);
    for index in 0..3 {
        fixture.fm.disconnect(index).await;
        fixture.api.finalize(
            NAMESPACE,
            Kind::Pod,
            &cohort.allocation.nodes[index].pod_name,
            &cohort.bindings[index].pod_uid,
        );
    }
    fixture.drive().await;
    assert_eq!(
        fixture.fm.observe().await.unwrap().unwrap().status.phase,
        kuberic_fm::Phase::Stopped
    );
    assert!(fixture
        .set()
        .await
        .finalizers()
        .iter()
        .any(|value| value == FINALIZER));
    assert!(fixture
        .api
        .deletes()
        .iter()
        .all(|(kind, ..)| !matches!(kind, Kind::ConfigMap | Kind::Service)));
    for index in 0..3 {
        fixture
            .processes
            .publish(&cohort, index, CleanupState::Complete(receipt()))
            .await;
    }
    fixture.drive().await;
    assert_gone(&fixture, &cohort).await;
    assert!(fixture
        .api
        .deletes()
        .iter()
        .filter(|(kind, ..)| *kind == Kind::Pod)
        .all(|(_, _, _, grace)| *grace == Some(30)));
}

#[tokio::test]
async fn conflicts_and_lost_replies_resume_the_same_teardown_after_facade_recreation() {
    let fixture = Fixture::new().await;
    fixture.serving().await;
    let cohort = fixture.set().await.status.unwrap().cohort().unwrap();
    request_delete(&fixture).await;
    fixture.api.conflict_next(Kind::Set, true);
    assert!(fixture.controller.reconcile(NAME).await.is_err());
    assert_eq!(fixture.fm.shutdowns.load(Ordering::SeqCst), 0);
    fixture.drive().await;
    native_deletes(&fixture).await;
    fixture.controller.reconcile(NAME).await.unwrap();
    fixture.api.lose_next_delete_response(Kind::Pod);
    assert!(fixture.controller.reconcile(NAME).await.is_err());
    let retained = fixture.set().await.status.unwrap().teardown.unwrap();
    let recreated = fixture.recreate();
    for _ in 0..16 {
        let outcome = recreated.reconcile(NAME).await.unwrap();
        assert!(outcome.blocked.is_none());
        if !outcome.changed {
            break;
        }
    }
    assert_eq!(
        fixture.set().await.status.unwrap().teardown.unwrap().cohort,
        retained.cohort
    );
    joined_processes(&fixture, &cohort).await;
    drive_to(&recreated, |plan| matches!(plan, Plan::DeleteConfigMap(_))).await;
    fixture.api.conflict_next_delete(Kind::ConfigMap);
    assert!(recreated.reconcile(NAME).await.is_err());
    fixture.api.lose_next_delete_response(Kind::ConfigMap);
    assert!(recreated.reconcile(NAME).await.is_err());
    let recreated = fixture.recreate();
    drive_to(&recreated, |plan| matches!(plan, Plan::RemoveFinalizer)).await;
    fixture.api.lose_next_response(Kind::Set, false);
    assert!(recreated.reconcile(NAME).await.is_err());
    assert!(recreated.reconcile(NAME).await.unwrap().resource_absent);
    assert_gone(&fixture, &cohort).await;
}

#[tokio::test]
async fn missing_physical_cleanup_times_out_without_certifying_channel_or_api_disappearance() {
    let fixture = Fixture::new().await;
    let cohort = begin(&fixture).await;
    pod_deletions(&fixture).await;
    for index in 0..3 {
        fixture.fm.disconnect(index).await;
        fixture.api.finalize(
            NAMESPACE,
            Kind::Pod,
            &cohort.allocation.nodes[index].pod_name,
            &cohort.bindings[index].pod_uid,
        );
    }
    fixture.drive().await;
    let deadline = fixture
        .set()
        .await
        .status
        .unwrap()
        .teardown
        .unwrap()
        .deadline_unix_ms;
    fixture.clock.store(deadline, Ordering::SeqCst);
    let outcome = fixture.controller.reconcile(NAME).await.unwrap();
    assert!(outcome.blocked.as_deref().unwrap().contains("deadline"));
    let set = fixture.set().await;
    assert_eq!(set.status.as_ref().unwrap().phase, ControllerPhase::Blocked);
    assert!(set.finalizers().iter().any(|value| value == FINALIZER));
    assert!(fixture
        .api
        .deletes()
        .iter()
        .all(|(kind, ..)| !matches!(kind, Kind::ConfigMap | Kind::Service)));
    fixture.api.check();
}

#[tokio::test]
async fn failed_forced_or_partial_cleanup_never_releases_the_finalizer() {
    for failure in 0..3 {
        let fixture = Fixture::new().await;
        let cohort = begin(&fixture).await;
        pod_deletions(&fixture).await;
        let mut incomplete = receipt();
        let outcome = match failure {
            0 => CleanupState::Failed("owned task join lost".into()),
            1 => {
                incomplete.forced = true;
                CleanupState::Complete(incomplete)
            }
            2 => {
                incomplete.application_reaped = false;
                CleanupState::Complete(incomplete)
            }
            _ => unreachable!(),
        };
        fixture.processes.publish(&cohort, 0, outcome).await;
        assert!(fixture
            .controller
            .reconcile(NAME)
            .await
            .unwrap()
            .blocked
            .is_some());
        assert!(fixture
            .set()
            .await
            .finalizers()
            .iter()
            .any(|value| value == FINALIZER));
        assert!(fixture
            .api
            .deletes()
            .iter()
            .all(|(kind, ..)| !matches!(kind, Kind::ConfigMap | Kind::Service)));
        fixture.api.check();
    }
}

#[tokio::test]
async fn a_replacement_pod_during_teardown_is_never_deleted_as_the_original() {
    let fixture = Fixture::new().await;
    let cohort = begin(&fixture).await;
    pod_deletions(&fixture).await;
    let pods: Api<Pod> = Api::namespaced(fixture.client.clone(), NAMESPACE);
    let mut pod = pods
        .get(&cohort.allocation.nodes[0].pod_name)
        .await
        .unwrap();
    fixture
        .api
        .finalize(NAMESPACE, Kind::Pod, &pod.name_any(), &pod.uid().unwrap());
    pod.metadata.uid = None;
    pod.metadata.resource_version = None;
    pod.metadata.deletion_timestamp = None;
    pod.metadata.deletion_grace_period_seconds = None;
    pod.status = None;
    let replacement = pods.create(&PostParams::default(), &pod).await.unwrap();
    let before = fixture.api.deletes();
    assert!(fixture
        .controller
        .reconcile(NAME)
        .await
        .unwrap()
        .blocked
        .is_some());
    assert_eq!(fixture.api.deletes(), before);
    assert_eq!(
        pods.get(&pod.name_any()).await.unwrap().uid(),
        replacement.uid()
    );
    assert!(fixture
        .set()
        .await
        .finalizers()
        .iter()
        .any(|value| value == FINALIZER));
    fixture.api.check();
}

#[tokio::test]
async fn replacement_support_resources_are_not_deleted_after_positive_process_cleanup() {
    for kind in [Kind::ConfigMap, Kind::Service] {
        let fixture = Fixture::new().await;
        let cohort = begin(&fixture).await;
        pod_deletions(&fixture).await;
        joined_processes(&fixture, &cohort).await;
        drive_to(&fixture.controller, |plan| {
            matches!(plan, Plan::DeleteConfigMap(_))
        })
        .await;
        let mut replacement_service = None;
        if kind == Kind::ConfigMap {
            let maps: Api<ConfigMap> = Api::namespaced(fixture.client.clone(), NAMESPACE);
            let mut original = maps
                .get(&cohort.allocation.nodes[0].config_map_name)
                .await
                .unwrap();
            maps.delete(
                &original.name_any(),
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: original.uid(),
                        resource_version: original.resource_version(),
                    }),
                    ..DeleteParams::default()
                },
            )
            .await
            .unwrap();
            original.metadata.uid = None;
            original.metadata.resource_version = None;
            maps.create(&PostParams::default(), &original)
                .await
                .unwrap();
        } else {
            let services: Api<Service> = Api::namespaced(fixture.client.clone(), NAMESPACE);
            let mut original = services
                .get(&resources::write_service_name(NAME))
                .await
                .unwrap();
            services
                .delete(
                    &original.name_any(),
                    &DeleteParams {
                        preconditions: Some(Preconditions {
                            uid: original.uid(),
                            resource_version: original.resource_version(),
                        }),
                        ..DeleteParams::default()
                    },
                )
                .await
                .unwrap();
            original.metadata.uid = None;
            original.metadata.resource_version = None;
            original.spec.as_mut().unwrap().selector = Some(std::collections::BTreeMap::from([(
                "app".into(),
                "replacement".into(),
            )]));
            replacement_service = Some(
                services
                    .create(&PostParams::default(), &original)
                    .await
                    .unwrap(),
            );
        }
        let requests = fixture.api.deletes();
        assert!(fixture
            .controller
            .reconcile(NAME)
            .await
            .unwrap()
            .blocked
            .is_some());
        assert_eq!(fixture.api.deletes(), requests);
        if let Some(replacement) = replacement_service {
            let actual = Api::<Service>::namespaced(fixture.client.clone(), NAMESPACE)
                .get(&replacement.name_any())
                .await
                .unwrap();
            assert_eq!(
                actual, replacement,
                "a replacement Service is not the original routing target"
            );
        }
        assert!(fixture
            .set()
            .await
            .finalizers()
            .iter()
            .any(|value| value == FINALIZER));
        let report = fixture.fm.observe().await.unwrap().unwrap();
        assert_eq!(report.status.phase, kuberic_fm::Phase::Stopped);
        assert!(report.joined);
        assert!(report.status.error.is_none());
        fixture.api.check();
    }
}

#[tokio::test]
async fn a_new_process_owner_cannot_reconstruct_cleanup_from_status() {
    let fixture = Fixture::new().await;
    let cohort = begin(&fixture).await;
    pod_deletions(&fixture).await;
    joined_processes(&fixture, &cohort).await;
    drive_to(&fixture.controller, |plan| {
        matches!(plan, Plan::DeleteConfigMap(_))
    })
    .await;
    let replacement_owner = Arc::new(support::ProcessEvidence::new());
    let controller = kuberic_controller::Reconciler::new(
        fixture.client.clone(),
        support::settings(),
        fixture.fm.clone(),
        replacement_owner,
    )
    .unwrap();
    assert!(controller.reconcile(NAME).await.unwrap().blocked.is_some());
    assert!(fixture
        .set()
        .await
        .finalizers()
        .iter()
        .any(|value| value == FINALIZER));
    assert!(fixture
        .api
        .deletes()
        .iter()
        .all(|(kind, ..)| !matches!(kind, Kind::ConfigMap | Kind::Service)));
    fixture.api.check();
}

#[tokio::test]
async fn accepted_teardown_ignores_later_spec_edits_and_preserves_foreign_finalizers() {
    let fixture = Fixture::new().await;
    let sets: Api<KubericSet> = Api::namespaced(fixture.client.clone(), NAMESPACE);
    let mut set = fixture.set().await;
    set.metadata.finalizers = Some(vec!["example.com/keep".into()]);
    sets.replace(NAME, &PostParams::default(), &set)
        .await
        .unwrap();
    let cohort = begin(&fixture).await;
    let mut set = fixture.set().await;
    set.spec.image = "sample:later-edit".into();
    sets.replace(NAME, &PostParams::default(), &set)
        .await
        .unwrap();
    let mut policy = support::settings();
    policy.teardown_timeout_ms = 100;
    let clock = fixture.clock.clone();
    let controller = kuberic_controller::Reconciler::new(
        fixture.client.clone(),
        policy,
        fixture.fm.clone(),
        fixture.processes.clone(),
    )
    .unwrap()
    .with_clock(move || Ok(clock.load(Ordering::SeqCst)));
    assert!(controller.reconcile(NAME).await.unwrap().blocked.is_none());
    pod_deletions(&fixture).await;
    joined_processes(&fixture, &cohort).await;
    fixture.drive().await;
    let set = fixture.set().await;
    assert_eq!(set.finalizers(), ["example.com/keep"]);
    assert_eq!(
        set.status
            .as_ref()
            .unwrap()
            .teardown
            .as_ref()
            .unwrap()
            .stage,
        TeardownStage::Complete
    );
    assert_eq!(set.status.unwrap().phase, ControllerPhase::Stopped);
    fixture.api.check();
}

#[tokio::test]
async fn deletion_after_allocation_finishes_provisioning_before_normal_teardown() {
    let fixture = Fixture::new().await;
    fixture.controller.reconcile(NAME).await.unwrap();
    fixture.controller.reconcile(NAME).await.unwrap();
    let allocation = fixture.set().await.status.unwrap().allocation.unwrap();
    assert!(fixture.pods().await.is_empty());
    request_delete(&fixture).await;
    drive_unrouted(&fixture).await;
    assert_eq!(fixture.pods().await.len(), 3);
    assert_eq!(fixture.fm.starts.load(Ordering::SeqCst), 0);
    for (index, pod) in fixture.pods().await.iter().enumerate() {
        support::publish_ip(
            &fixture.client,
            pod,
            &format!("127.0.0.{}", index + 2),
            true,
        )
        .await;
    }
    drive_unrouted(&fixture).await;
    let status = fixture.set().await.status.unwrap();
    assert_eq!(status.allocation.as_ref(), Some(&allocation));
    let cohort = status.cohort().unwrap();
    assert_eq!(status.phase, ControllerPhase::Stopping);
    assert!(status.bootstrap_wait.is_some());
    assert!(status.teardown.is_none());
    fixture.fm.create().await;
    drive_unrouted(&fixture).await;
    assert_eq!(fixture.fm.shutdowns.load(Ordering::SeqCst), 1);
    pod_deletions(&fixture).await;
    joined_processes(&fixture, &cohort).await;
    fixture.drive().await;
    assert_gone(&fixture, &cohort).await;
}

#[tokio::test]
async fn deletion_during_each_native_create_step_finishes_the_original_request_without_routing() {
    for completed in 0..4 {
        let fixture = Fixture::new().await;
        fixture.provision().await;
        let cohort = fixture.set().await.status.unwrap().cohort().unwrap();
        fixture.fm.bootstrap().await;
        for _ in 0..completed {
            let command = fixture.fm.command().await;
            fixture.fm.complete(&command).await;
        }
        let pending = fixture.fm.command().await;
        request_delete(&fixture).await;
        drive_unrouted(&fixture).await;
        assert_eq!(fixture.fm.shutdowns.load(Ordering::SeqCst), 0);
        assert!(fixture
            .api
            .deletes()
            .iter()
            .all(|(kind, ..)| *kind == Kind::Set));
        let retry = fixture.fm.retry().await;
        assert_eq!(retry, pending);
        fixture.fm.complete(&retry).await;
        for _ in completed + 1..4 {
            drive_unrouted(&fixture).await;
            assert_eq!(fixture.fm.shutdowns.load(Ordering::SeqCst), 0);
            let command = fixture.fm.command().await;
            fixture.fm.complete(&command).await;
        }
        drive_unrouted(&fixture).await;
        let retained = fixture.set().await.status.unwrap();
        assert_eq!(retained.cohort().as_ref(), Some(&cohort));
        assert_eq!(retained.phase, ControllerPhase::Stopping);
        assert_eq!(fixture.fm.shutdowns.load(Ordering::SeqCst), 1);
        pod_deletions(&fixture).await;
        joined_processes(&fixture, &cohort).await;
        fixture.drive().await;
        assert_gone(&fixture, &cohort).await;
    }
}

#[tokio::test]
async fn bootstrap_wait_survives_conflicts_lost_replies_and_facade_or_policy_changes() {
    let fixture = Fixture::new().await;
    fixture.provision().await;
    let cohort = fixture.set().await.status.unwrap().cohort().unwrap();
    request_delete(&fixture).await;
    fixture.api.conflict_next(Kind::Set, true);
    assert!(fixture.controller.reconcile(NAME).await.is_err());
    assert!(fixture.set().await.status.unwrap().bootstrap_wait.is_none());
    fixture.api.lose_next_response(Kind::Set, true);
    assert!(fixture.controller.reconcile(NAME).await.is_err());
    let wait = fixture.set().await.status.unwrap().bootstrap_wait.unwrap();
    fixture
        .clock
        .store(wait.started_unix_ms + 100, Ordering::SeqCst);
    let mut policy = support::settings();
    policy.teardown_timeout_ms = 100;
    let clock = fixture.clock.clone();
    let recreated = kuberic_controller::Reconciler::new(
        fixture.client.clone(),
        policy,
        fixture.fm.clone(),
        fixture.processes.clone(),
    )
    .unwrap()
    .with_clock(move || Ok(clock.load(Ordering::SeqCst)));
    let sets: Api<KubericSet> = Api::namespaced(fixture.client.clone(), NAMESPACE);
    let mut set = fixture.set().await;
    set.spec.image = "sample:later-edit".into();
    sets.replace(NAME, &PostParams::default(), &set)
        .await
        .unwrap();
    let outcome = recreated.reconcile(NAME).await.unwrap();
    assert!(outcome.blocked.is_none(), "{outcome:?}");
    assert_eq!(
        fixture.set().await.status.unwrap().bootstrap_wait.as_ref(),
        Some(&wait)
    );
    fixture.fm.create().await;
    drive_unrouted(&fixture).await;
    assert_eq!(
        fixture.set().await.status.unwrap().bootstrap_wait.as_ref(),
        Some(&wait)
    );
    pod_deletions(&fixture).await;
    joined_processes(&fixture, &cohort).await;
    fixture.drive().await;
    assert_gone(&fixture, &cohort).await;
}

#[tokio::test]
async fn bootstrap_completion_deadline_blocks_before_or_after_native_start() {
    for native_started in [false, true] {
        let fixture = Fixture::new().await;
        if native_started {
            fixture.provision().await;
        } else {
            fixture.drive().await;
        }
        request_delete(&fixture).await;
        drive_unrouted(&fixture).await;
        let wait = fixture.set().await.status.unwrap().bootstrap_wait.unwrap();
        fixture.clock.store(wait.deadline_unix_ms, Ordering::SeqCst);
        if native_started {
            fixture.fm.create().await;
        }
        let recreated = fixture.recreate();
        let outcome = recreated.reconcile(NAME).await.unwrap();
        assert!(outcome
            .blocked
            .as_deref()
            .unwrap()
            .contains("bootstrap completion deadline"));
        let set = fixture.set().await;
        let status = set.status.as_ref().unwrap();
        assert_eq!(status.phase, ControllerPhase::Blocked);
        assert!(status.service_endpoint.is_none());
        assert!(status.teardown.is_none());
        assert!(set.finalizers().iter().any(|value| value == FINALIZER));
        assert_eq!(fixture.fm.shutdowns.load(Ordering::SeqCst), 0);
        assert!(fixture
            .api
            .deletes()
            .iter()
            .all(|(kind, ..)| *kind == Kind::Set));
        assert!(recreated.reconcile(NAME).await.unwrap().blocked.is_some());
        fixture.api.check();
    }
}

#[tokio::test]
async fn failed_bootstrap_keeps_the_finalizer_instead_of_attempting_normal_shutdown() {
    let fixture = Fixture::new().await;
    fixture.provision().await;
    let cohort = fixture.set().await.status.unwrap().cohort().unwrap();
    request_delete(&fixture).await;
    drive_unrouted(&fixture).await;
    fixture
        .fm
        .invalidate(&cohort, "bootstrap build failed")
        .await
        .unwrap();
    let outcome = fixture.controller.reconcile(NAME).await.unwrap();
    assert!(outcome
        .blocked
        .as_deref()
        .unwrap()
        .contains("bootstrap build failed"));
    let set = fixture.set().await;
    assert_eq!(set.status.as_ref().unwrap().phase, ControllerPhase::Blocked);
    assert!(set.finalizers().iter().any(|value| value == FINALIZER));
    assert_eq!(fixture.fm.shutdowns.load(Ordering::SeqCst), 0);
    assert!(fixture
        .api
        .deletes()
        .iter()
        .all(|(kind, ..)| *kind == Kind::Set));
    fixture.api.check();
}
