use kuberic_tests::controller as support;

use std::{
    collections::BTreeMap,
    sync::{atomic::Ordering, Arc},
};

use k8s_openapi::api::core::v1::{ConfigMap, ContainerStatus, Pod, Service, ServiceSpec};
use kube::{
    api::{DeleteParams, ObjectMeta, PostParams, Preconditions},
    Api, ResourceExt,
};
use kuberic_operator::{
    evaluate,
    resources::{self, POD_UID},
    ControllerPhase, FmApi, KubericSet, Plan, ProcessFm, Reconciler, StaleObservation,
};

use support::{api::Kind, Fixture, NAME, NAMESPACE};

async fn selector(fixture: &Fixture) -> Option<BTreeMap<String, String>> {
    Api::<Service>::namespaced(fixture.client.clone(), NAMESPACE)
        .get(&resources::write_service_name(NAME))
        .await
        .unwrap()
        .spec
        .unwrap()
        .selector
}

async fn assert_disabled(fixture: &Fixture) {
    assert_eq!(
        selector(fixture).await,
        Some(resources::disabled_selector(&fixture.set().await).unwrap())
    );
}

async fn request_switchover(fixture: &Fixture, target: Option<String>) {
    let mut set = fixture.set().await;
    set.status.as_mut().unwrap().target_primary_node_id = target;
    Api::<KubericSet>::namespaced(fixture.client.clone(), NAMESPACE)
        .replace_status(NAME, &PostParams::default(), &set)
        .await
        .unwrap();
}

#[tokio::test]
async fn status_target_withdraws_routing_before_handoff_and_retries_with_original_backend() {
    let fixture = Fixture::new().await;
    fixture.serving().await;
    let original = fixture.set().await;
    let cohort = original.status.as_ref().unwrap().cohort().unwrap();
    let target = cohort.allocation.nodes[2].node_id.clone();
    request_switchover(&fixture, Some(target.clone())).await;
    let observation = fixture.controller.observe(NAME).await.unwrap();
    let plan = evaluate(&observation, &support::settings());
    assert_eq!(
        plan,
        Plan::Route(resources::disabled_selector(&original).unwrap())
    );
    assert!(fixture
        .fm
        .observe()
        .await
        .unwrap()
        .unwrap()
        .status
        .switchover
        .is_none());
    fixture.controller.apply(&observation, plan).await.unwrap();
    assert_disabled(&fixture).await;
    fixture.api.lose_next_response(Kind::Set, true);
    assert!(fixture.controller.reconcile(NAME).await.is_err());
    fixture.drive().await;
    let switching = fixture.set().await.status.unwrap();
    assert_eq!(switching.phase, ControllerPhase::Switching);
    assert_eq!(
        switching.primary_node_id,
        original.status.as_ref().unwrap().primary_node_id
    );
    assert!(switching.service_endpoint.is_none());
    assert!(!switching.switchover.unwrap().completed);
    let command = fixture.fm.command().await;
    assert_eq!(command.node, 1);
    let recreated = fixture.recreate();
    assert!(!recreated.reconcile(NAME).await.unwrap().changed);
    assert_eq!(fixture.fm.retry().await, command);
    fixture.fm.complete(&command).await;
    fixture.drive().await;
    let complete = fixture.set().await.status.unwrap();
    assert_eq!(complete.phase, ControllerPhase::Serving);
    assert_eq!(complete.primary_node_id, Some(target.clone()));
    assert_eq!(complete.target_primary_node_id, Some(target));
    assert!(complete.switchover.as_ref().unwrap().completed);
    assert_eq!(complete.cohort(), Some(cohort.clone()));
    assert_eq!(
        selector(&fixture).await.unwrap().get(POD_UID),
        Some(&cohort.bindings[2].pod_uid)
    );
    assert!(!fixture.controller.reconcile(NAME).await.unwrap().changed);
    assert_eq!(fixture.fm.starts.load(Ordering::SeqCst), 1);
    fixture.api.check();
}

#[tokio::test]
async fn switchover_refuses_foreign_targets_cancellation_retargeting_and_stale_effects() {
    for fault in 0..4 {
        let fixture = Fixture::new().await;
        fixture.serving().await;
        let cohort = fixture.set().await.status.unwrap().cohort().unwrap();
        if fault == 0 {
            request_switchover(&fixture, Some("foreign".into())).await;
        } else {
            request_switchover(&fixture, Some(cohort.allocation.nodes[0].node_id.clone())).await;
            fixture.drive().await;
            if fault == 3 {
                let command = fixture.fm.command().await;
                fixture.fm.complete(&command).await;
            }
            request_switchover(
                &fixture,
                if fault == 1 {
                    None
                } else {
                    Some(cohort.allocation.nodes[2].node_id.clone())
                },
            )
            .await;
        }
        let outcome = fixture.controller.reconcile(NAME).await.unwrap();
        assert!(outcome.blocked.is_some(), "fault {fault}");
        assert_disabled(&fixture).await;
        fixture.api.check();
    }
    let fixture = Fixture::new().await;
    fixture.serving().await;
    let cohort = fixture.set().await.status.unwrap().cohort().unwrap();
    request_switchover(&fixture, Some(cohort.allocation.nodes[0].node_id.clone())).await;
    fixture.controller.reconcile(NAME).await.unwrap();
    fixture.controller.reconcile(NAME).await.unwrap();
    let observation = fixture.controller.observe(NAME).await.unwrap();
    let plan = evaluate(&observation, &support::settings());
    assert!(matches!(plan, Plan::Switchover { .. }));
    request_switchover(&fixture, Some(cohort.allocation.nodes[2].node_id.clone())).await;
    let error = fixture
        .controller
        .apply(&observation, plan)
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<StaleObservation>(),
        Some(&StaleObservation::Resources)
    );
    assert!(fixture
        .fm
        .observe()
        .await
        .unwrap()
        .unwrap()
        .status
        .switchover
        .is_none());
    fixture.api.check();
}

#[tokio::test]
async fn edited_handoff_receipts_never_grant_native_completion_or_another_transfer() {
    for fault in 0..3 {
        let fixture = Fixture::new().await;
        fixture.serving().await;
        let cohort = fixture.set().await.status.unwrap().cohort().unwrap();
        request_switchover(&fixture, Some(cohort.allocation.nodes[0].node_id.clone())).await;
        fixture.drive().await;
        let mut set = fixture.set().await;
        let receipt = set.status.as_mut().unwrap().switchover.as_mut().unwrap();
        match fault {
            0 => receipt.configuration_epoch += 1,
            1 => receipt.source_node_id = cohort.allocation.nodes[2].node_id.clone(),
            _ => receipt.completed = true,
        }
        Api::<KubericSet>::namespaced(fixture.client.clone(), NAMESPACE)
            .replace_status(NAME, &PostParams::default(), &set)
            .await
            .unwrap();
        let result = fixture.controller.reconcile(NAME).await.unwrap();
        assert!(result.blocked.is_some(), "fault {fault}");
        assert_disabled(&fixture).await;
        fixture.api.check();
    }
}

#[tokio::test]
async fn completed_handoffs_compose_with_each_app_loss_repair_and_new_status_requests() {
    for victim in 0..3 {
        let fixture = Fixture::new().await;
        fixture.serving().await;
        let cohort = fixture.set().await.status.unwrap().cohort().unwrap();
        request_switchover(&fixture, Some(cohort.allocation.nodes[0].node_id.clone())).await;
        fixture.drive().await;
        let first = fixture.fm.command().await;
        fixture.fm.complete(&first).await;
        fixture.drive().await;
        let receipt = fixture.set().await.status.unwrap().switchover.unwrap();
        fixture.fm.lose_application(victim).await;
        fixture.drive().await;
        let removal = fixture.fm.command().await;
        fixture.fm.complete(&removal).await;
        fixture.fm.restore().await;
        fixture.drive().await;
        let repaired = fixture.set().await.status.unwrap();
        assert_eq!(repaired.target_primary_node_id, repaired.primary_node_id);
        assert_eq!(repaired.switchover.as_ref(), Some(&receipt));
        assert!(repaired.configuration_epoch.unwrap() > receipt.configuration_epoch);
        for target in [victim, (victim + 1) % 3, victim] {
            request_switchover(
                &fixture,
                Some(cohort.allocation.nodes[target].node_id.clone()),
            )
            .await;
            fixture.drive().await;
            assert_disabled(&fixture).await;
            let switching = fixture.set().await.status.unwrap();
            assert_eq!(switching.phase, ControllerPhase::Switching);
            assert!(switching.service_endpoint.is_none());
            let accepted = switching.switchover.as_ref().unwrap();
            assert_eq!(
                Some(accepted.previous_configuration_epoch),
                switching.configuration_epoch
            );
            assert!(!accepted.completed);
            let command = fixture.fm.command().await;
            fixture.fm.complete(&command).await;
            fixture.drive().await;
            let complete = fixture.set().await.status.unwrap();
            assert_eq!(complete.phase, ControllerPhase::Serving);
            assert_eq!(complete.primary_node_id, complete.target_primary_node_id);
            assert_eq!(
                complete.primary_node_id.as_ref(),
                Some(&cohort.allocation.nodes[target].node_id)
            );
            assert_eq!(
                complete.configuration_epoch,
                Some(accepted.configuration_epoch)
            );
            assert!(complete.switchover.unwrap().completed);
            assert_eq!(
                selector(&fixture).await.unwrap().get(POD_UID),
                Some(&cohort.bindings[target].pod_uid)
            );
        }
        fixture.api.check();
    }
}

#[tokio::test]
async fn completed_repair_fences_a_previously_planned_handoff_effect_and_backend_command() {
    let fixture = Fixture::new().await;
    fixture.serving().await;
    let cohort = fixture.set().await.status.unwrap().cohort().unwrap();
    let target = &cohort.allocation.nodes[2].node_id;
    request_switchover(&fixture, Some(target.clone())).await;
    fixture.controller.reconcile(NAME).await.unwrap();
    fixture.controller.reconcile(NAME).await.unwrap();
    let observation = fixture.controller.observe(NAME).await.unwrap();
    let plan = evaluate(&observation, &support::settings());
    let Plan::Switchover {
        previous_configuration_epoch,
        ..
    } = &plan
    else {
        panic!("{plan:?}")
    };
    fixture.fm.lose_application(0).await;
    let removal = fixture.fm.command().await;
    fixture.fm.complete(&removal).await;
    fixture.fm.restore().await;
    assert!(fixture
        .fm
        .switchover(&cohort, target, *previous_configuration_epoch)
        .await
        .is_err());
    let error = fixture
        .controller
        .apply(&observation, plan)
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<StaleObservation>(),
        Some(&StaleObservation::Premise)
    );
    assert_disabled(&fixture).await;
    fixture.api.check();
}

#[tokio::test]
async fn primary_application_failover_withdraws_then_routes_only_native_completed_original_primary()
{
    let fixture = Fixture::new().await;
    fixture.serving().await;
    let original = fixture.set().await.status.unwrap().cohort().unwrap();
    fixture.fm.lose_primary().await;
    fixture.drive().await;
    assert_disabled(&fixture).await;
    let status = fixture.set().await.status.unwrap();
    assert_eq!(status.phase, ControllerPhase::Reconfiguring);
    assert!(status.primary_node_id.is_none());
    assert!(status.service_endpoint.is_none());
    let command = fixture.fm.command().await;
    fixture.drive().await;
    assert_disabled(&fixture).await;
    fixture.fm.complete(&command).await;
    fixture.drive().await;
    let status = fixture.set().await.status.unwrap();
    assert_eq!(status.phase, ControllerPhase::Restoring);
    assert_eq!(status.cohort().unwrap(), original);
    assert_eq!(
        status.primary_node_id.as_ref(),
        Some(&original.allocation.nodes[0].node_id)
    );
    assert_eq!(
        selector(&fixture).await.unwrap().get(POD_UID),
        Some(&original.bindings[0].pod_uid)
    );
    assert_eq!(fixture.fm.starts.load(Ordering::SeqCst), 1);
    fixture.fm.restore().await;
    fixture.drive().await;
    assert_eq!(
        fixture.set().await.status.unwrap().phase,
        ControllerPhase::Serving
    );
    fixture.api.check();
}

#[tokio::test]
async fn secondary_application_repair_keeps_exact_primary_routing_until_final_admission() {
    let fixture = Fixture::new().await;
    fixture.serving().await;
    let original = fixture.set().await.status.unwrap();
    let selected = selector(&fixture).await;
    fixture.fm.lose_application(0).await;
    for _ in 0..4 {
        fixture.drive().await;
        let status = fixture.set().await.status.unwrap();
        assert_eq!(status.phase, ControllerPhase::Restoring);
        assert_eq!(status.cohort(), original.cohort());
        assert_eq!(status.primary_node_id, original.primary_node_id);
        assert_eq!(status.service_endpoint, original.service_endpoint);
        assert_eq!(selector(&fixture).await, selected);
        let command = fixture.fm.command().await;
        fixture.fm.complete(&command).await;
    }
    fixture.drive().await;
    assert_eq!(
        fixture.set().await.status.unwrap().phase,
        ControllerPhase::Serving
    );
    assert_eq!(selector(&fixture).await, selected);
    fixture.api.check();
}

async fn delete<K>(api: &Api<K>, object: &K)
where
    K: kube::Resource<DynamicType = ()> + Clone + std::fmt::Debug + serde::de::DeserializeOwned,
{
    api.delete(
        &object.name_any(),
        &DeleteParams {
            preconditions: Some(Preconditions {
                uid: object.uid(),
                resource_version: object.resource_version(),
            }),
            ..DeleteParams::default()
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn bootstrap_observes_pods_but_native_replies_alone_authorize_routing() {
    let fixture = Fixture::new().await;
    fixture.drive().await;
    assert_eq!(fixture.pods().await.len(), 3);
    assert_eq!(fixture.fm.starts.load(Ordering::SeqCst), 0);
    let pods = fixture.pods().await;
    for (index, pod) in pods.iter().take(2).enumerate() {
        support::publish_ip(
            &fixture.client,
            pod,
            &format!("127.0.0.{}", index + 2),
            true,
        )
        .await;
    }
    fixture.drive().await;
    assert!(fixture.set().await.status.unwrap().bindings.is_none());
    assert_eq!(fixture.fm.starts.load(Ordering::SeqCst), 0);
    for pod in &pods {
        let map = Api::<ConfigMap>::namespaced(fixture.client.clone(), NAMESPACE)
            .get(&pod.name_any())
            .await
            .unwrap();
        assert!(map.data.unwrap().is_empty());
        assert_eq!(
            pod.spec.as_ref().unwrap().restart_policy.as_deref(),
            Some("Never")
        );
    }
    support::publish_ip(&fixture.client, &pods[2], "127.0.0.4", false).await;
    fixture.drive().await;
    let set = fixture.set().await;
    let cohort = set.status.as_ref().unwrap().cohort().unwrap();
    assert_eq!(fixture.fm.starts.load(Ordering::SeqCst), 1);
    assert_disabled(&fixture).await;
    assert_eq!(set.status.unwrap().phase, ControllerPhase::Starting);
    for (index, node) in cohort.allocation.nodes.iter().enumerate() {
        let map = Api::<ConfigMap>::namespaced(fixture.client.clone(), NAMESPACE)
            .get(&node.config_map_name)
            .await
            .unwrap();
        assert_eq!(map.immutable, Some(true));
        let config: fube_node_runner::Config =
            serde_json::from_str(&map.data.unwrap()["node.json"]).unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.node.address.ip().to_string(),
            cohort.bindings[index].ip
        );
        assert_eq!(config.peers.len(), 2);
        assert_eq!(config.node.identity.id, node.node_id);
        assert_eq!(
            config.run_directory.file_name().unwrap(),
            cohort.bindings[index].pod_uid.as_str()
        );
    }
    fixture.fm.bootstrap().await;
    for _ in 0..3 {
        let command = fixture.fm.command().await;
        fixture.fm.complete(&command).await;
        fixture.drive().await;
        assert_disabled(&fixture).await;
    }
    let final_configuration = fixture.fm.command().await;
    fixture.drive().await;
    assert_disabled(&fixture).await;
    fixture.fm.complete(&final_configuration).await;
    fixture.drive().await;
    let set = fixture.set().await;
    assert_eq!(set.status.as_ref().unwrap().phase, ControllerPhase::Serving);
    assert_eq!(
        set.status.unwrap().service_endpoint.as_deref(),
        Some("client://opaque-original-primary")
    );
    assert_eq!(
        selector(&fixture).await.unwrap().get(POD_UID),
        Some(&cohort.bindings[1].pod_uid)
    );
    let recreated = Reconciler::new(
        fixture.client.clone(),
        support::settings(),
        fixture.fm.clone(),
        fixture.processes.clone(),
    )
    .unwrap();
    assert!(!recreated.reconcile(NAME).await.unwrap().changed);
    assert_eq!(fixture.fm.starts.load(Ordering::SeqCst), 1);
    fixture.api.check();
}

#[tokio::test]
async fn cohort_uid_ip_loss_and_restart_withdraw_routing_without_replacement() {
    for fault in 0..7 {
        let fixture = Fixture::new().await;
        fixture.serving().await;
        let pod_api: Api<Pod> = Api::namespaced(fixture.client.clone(), NAMESPACE);
        let pod = fixture.pods().await.remove(0);
        match fault {
            0 | 2 => {
                delete(&pod_api, &pod).await;
                fixture
                    .api
                    .finalize(NAMESPACE, Kind::Pod, &pod.name_any(), &pod.uid().unwrap());
                if fault == 0 {
                    let mut replacement = pod.clone();
                    replacement.metadata.uid = None;
                    replacement.metadata.resource_version = None;
                    replacement.status = None;
                    let replacement = pod_api
                        .create(&PostParams::default(), &replacement)
                        .await
                        .unwrap();
                    support::publish_ip(
                        &fixture.client,
                        &replacement,
                        pod.status.as_ref().unwrap().pod_ip.as_ref().unwrap(),
                        true,
                    )
                    .await;
                }
            }
            1 => {
                support::publish_ip(&fixture.client, &pod, "127.0.0.9", true).await;
            }
            3 => {
                let maps: Api<ConfigMap> = Api::namespaced(fixture.client.clone(), NAMESPACE);
                let mut map = maps.get(&pod.name_any()).await.unwrap();
                delete(&maps, &map).await;
                map.metadata.uid = None;
                map.metadata.resource_version = None;
                maps.create(&PostParams::default(), &map).await.unwrap();
            }
            4 => {
                let mut stopped = pod.clone();
                stopped.status.as_mut().unwrap().phase = Some("Failed".into());
                pod_api
                    .replace_status(&stopped.name_any(), &PostParams::default(), &stopped)
                    .await
                    .unwrap();
            }
            5 => {
                let mut restarted = pod.clone();
                restarted.status.as_mut().unwrap().container_statuses =
                    Some(vec![ContainerStatus {
                        name: "node".into(),
                        restart_count: 1,
                        ..ContainerStatus::default()
                    }]);
                pod_api
                    .replace_status(&restarted.name_any(), &PostParams::default(), &restarted)
                    .await
                    .unwrap();
            }
            6 => {
                let sets: Api<KubericSet> = Api::namespaced(fixture.client.clone(), NAMESPACE);
                let mut set = fixture.set().await;
                set.spec.image = "sample:changed".into();
                sets.replace(NAME, &PostParams::default(), &set)
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let result = fixture.controller.reconcile(NAME).await.unwrap();
        assert!(result.blocked.is_some(), "fault {fault}");
        assert_disabled(&fixture).await;
        assert_eq!(
            fixture.set().await.status.unwrap().phase,
            ControllerPhase::Blocked
        );
        assert_eq!(
            fixture.fm.observe().await.unwrap().unwrap().status.phase,
            kuberic_fm::Phase::ControlUnknown
        );
        assert!(fixture
            .controller
            .reconcile(NAME)
            .await
            .unwrap()
            .blocked
            .is_some());
        assert_eq!(fixture.fm.starts.load(Ordering::SeqCst), 1);
        if fault == 2 {
            assert!(pod_api.get_opt(&pod.name_any()).await.unwrap().is_none());
        }
        fixture.api.check();
    }
}

#[tokio::test]
async fn status_loss_or_new_fm_owner_cannot_adopt_existing_resources() {
    let fixture = Fixture::new().await;
    fixture.serving().await;
    let fresh_fm = Arc::new(support::ScriptedFm::new());
    let restarted = Reconciler::new(
        fixture.client.clone(),
        support::settings(),
        fresh_fm.clone(),
        fixture.processes.clone(),
    )
    .unwrap();
    assert!(restarted.reconcile(NAME).await.unwrap().blocked.is_some());
    assert_disabled(&fixture).await;
    assert_eq!(fresh_fm.starts.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.fm.observe().await.unwrap().unwrap().status.phase,
        kuberic_fm::Phase::Serving,
        "withdrawing Kubernetes routing is not proof the old writer stopped"
    );

    let fixture = Fixture::new().await;
    fixture.serving().await;
    let sets: Api<KubericSet> = Api::namespaced(fixture.client.clone(), NAMESPACE);
    let mut set = fixture.set().await;
    set.status = None;
    sets.replace_status(NAME, &PostParams::default(), &set)
        .await
        .unwrap();
    assert!(fixture
        .controller
        .reconcile(NAME)
        .await
        .unwrap()
        .blocked
        .is_some());
    assert_disabled(&fixture).await;
    assert_eq!(fixture.fm.starts.load(Ordering::SeqCst), 1);
    fixture.api.check();
}

#[tokio::test]
async fn foreign_owned_objects_and_namespaces_are_never_mutated_or_adopted() {
    let fixture = Fixture::new().await;
    fixture.controller.reconcile(NAME).await.unwrap();
    let services: Api<Service> = Api::namespaced(fixture.client.clone(), NAMESPACE);
    let foreign = services
        .create(
            &PostParams::default(),
            &Service {
                metadata: ObjectMeta {
                    name: Some(resources::write_service_name(NAME)),
                    ..ObjectMeta::default()
                },
                spec: Some(ServiceSpec {
                    selector: Some(BTreeMap::from([("app".into(), "foreign".into())])),
                    ..ServiceSpec::default()
                }),
                ..Service::default()
            },
        )
        .await
        .unwrap();
    assert!(fixture
        .controller
        .reconcile(NAME)
        .await
        .unwrap()
        .blocked
        .is_some());
    let unchanged = services.get(&foreign.name_any()).await.unwrap();
    assert_eq!(unchanged, foreign);
    assert!(fixture.pods().await.is_empty());
    let other: Api<Pod> = Api::namespaced(fixture.client.clone(), "other-namespace");
    let unrelated = other
        .create(
            &PostParams::default(),
            &Pod {
                metadata: ObjectMeta {
                    name: Some("sample-node-1".into()),
                    ..ObjectMeta::default()
                },
                ..Pod::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(other.get("sample-node-1").await.unwrap(), unrelated);
    fixture.api.check();
}

#[tokio::test]
async fn committed_status_survives_lost_response_and_config_conflict_never_means_completion() {
    let fixture = Fixture::new().await;
    fixture.controller.reconcile(NAME).await.unwrap();
    fixture.api.lose_next_response(Kind::Set, true);
    assert!(fixture.controller.reconcile(NAME).await.is_err());
    let deployment = fixture
        .set()
        .await
        .status
        .unwrap()
        .allocation
        .unwrap()
        .deployment;
    fixture.drive().await;
    for (index, pod) in fixture.pods().await.iter().enumerate() {
        support::publish_ip(
            &fixture.client,
            pod,
            &format!("127.0.0.{}", index + 2),
            true,
        )
        .await;
    }
    for _ in 0..16 {
        let observation = fixture.controller.observe(NAME).await.unwrap();
        if matches!(
            evaluate(&observation, &support::settings()),
            Plan::ConfigureNode(_)
        ) {
            break;
        }
        fixture.controller.reconcile(NAME).await.unwrap();
    }
    fixture.api.conflict_next(Kind::ConfigMap, false);
    assert!(fixture.controller.reconcile(NAME).await.is_err());
    let map = Api::<ConfigMap>::namespaced(fixture.client.clone(), NAMESPACE)
        .get("sample-node-1")
        .await
        .unwrap();
    assert!(map.data.unwrap().is_empty());
    assert_disabled(&fixture).await;
    fixture.drive().await;
    assert_eq!(
        fixture
            .set()
            .await
            .status
            .unwrap()
            .allocation
            .unwrap()
            .deployment,
        deployment
    );
    assert_eq!(fixture.fm.starts.load(Ordering::SeqCst), 1);
    fixture.api.check();
}

#[tokio::test]
async fn stale_pod_snapshot_cannot_start_a_native_fm_session() {
    let fixture = Fixture::new().await;
    fixture.drive().await;
    for (index, pod) in fixture.pods().await.iter().enumerate() {
        support::publish_ip(
            &fixture.client,
            pod,
            &format!("127.0.0.{}", index + 2),
            false,
        )
        .await;
    }
    fixture.controller.reconcile(NAME).await.unwrap();
    let observation = fixture.controller.observe(NAME).await.unwrap();
    let plan = evaluate(&observation, &support::settings());
    assert!(matches!(plan, Plan::StartFm(_)));
    let pod = observation.pods[0].as_ref().unwrap();
    support::publish_ip(&fixture.client, pod, "127.0.0.9", true).await;
    let error = fixture
        .controller
        .apply(&observation, plan)
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<StaleObservation>(),
        Some(&StaleObservation::Resources)
    );
    assert_eq!(fixture.fm.starts.load(Ordering::SeqCst), 0);
    assert!(fixture
        .controller
        .reconcile(NAME)
        .await
        .unwrap()
        .blocked
        .is_some());
    fixture.api.check();
}

#[tokio::test]
async fn native_transition_rejects_stale_routing_as_retryable_without_publishing() {
    let fixture = Fixture::new().await;
    fixture.provision().await;
    fixture.fm.create().await;
    let observation = fixture.controller.observe(NAME).await.unwrap();
    let plan = evaluate(&observation, &support::settings());
    assert!(matches!(plan, Plan::Route(_)));
    let cohort = observation.set.status.as_ref().unwrap().cohort().unwrap();
    fixture.fm.shutdown(&cohort).await.unwrap();
    let error = fixture
        .controller
        .apply(&observation, plan)
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<StaleObservation>(),
        Some(&StaleObservation::Premise)
    );
    assert_disabled(&fixture).await;
    fixture.api.check();
}

#[tokio::test]
async fn guarded_process_fm_invalidates_and_joins_without_admitting_a_successor() {
    let fixture = Fixture::new().await;
    fixture.provision().await;
    let mut cohort = fixture.set().await.status.unwrap().cohort().unwrap();
    let fm = ProcessFm::new();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    cohort.allocation.fm.address = reserved.local_addr().unwrap().to_string();
    drop(reserved);
    fm.reserve(&cohort.allocation).await.unwrap();
    fm.start(&cohort).await.unwrap();
    fm.invalidate(&cohort, "original Pod UID changed")
        .await
        .unwrap();
    assert_eq!(
        fm.observe().await.unwrap().unwrap().status.phase,
        kuberic_fm::Phase::ControlUnknown
    );
    assert!(fm.start(&cohort).await.is_err());
    assert!(fm.join().await.is_err());
    let listener = std::net::TcpListener::bind(&cohort.allocation.fm.address).unwrap();
    drop(listener);
}
