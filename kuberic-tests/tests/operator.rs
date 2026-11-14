use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use k8s_openapi::api::core::v1::{
    ContainerState, ContainerStateTerminated, ContainerStatus, Pod, Service,
};
use kube::{
    api::{DeleteParams, PostParams, Preconditions},
    Api, ResourceExt,
};
use kuberic_operator::{
    processes::{PodProcesses, FINALIZER},
    resources, CleanupState, Config, ControllerPhase, FmApi, KubericSet, Operator, ProcessApi,
    Reconciler, StaleObservation,
};
use kuberic_tests::controller::{self as support, api::Kind, Fixture, NAME, NAMESPACE};

fn config() -> Config {
    Config {
        namespace: NAMESPACE.into(),
        advertise_ip: Ipv4Addr::LOCALHOST,
        node_port: 19001,
        teardown_timeout_ms: 60_000,
        requeue: Duration::from_millis(50),
    }
}

#[tokio::test]
async fn runtime_retains_independent_uid_owners_and_reserved_native_listeners() {
    let fixture = Fixture::new().await;
    let sets: Api<KubericSet> = Api::namespaced(fixture.client.clone(), NAMESPACE);
    sets.create(
        &PostParams::default(),
        &KubericSet::new("another", support::spec("/opt/fube/package".into(), 8080)),
    )
    .await
    .unwrap();
    let operator = Operator::new(fixture.client.clone(), config()).unwrap();
    for _ in 0..16 {
        for name in [NAME, "another"] {
            operator
                .reconcile(&sets.get(name).await.unwrap())
                .await
                .unwrap();
        }
    }
    let first = sets
        .get(NAME)
        .await
        .unwrap()
        .status
        .unwrap()
        .allocation
        .unwrap();
    let second = sets
        .get("another")
        .await
        .unwrap()
        .status
        .unwrap()
        .allocation
        .unwrap();
    assert_ne!(first.fm, second.fm);
    assert_ne!(first.deployment, second.deployment);
    for allocation in [&first, &second] {
        assert!(tokio::net::TcpListener::bind(&allocation.fm.address)
            .await
            .is_err());
        operator
            .reconcile(&sets.get(&allocation.resource_name).await.unwrap())
            .await
            .unwrap();
        assert_eq!(
            sets.get(&allocation.resource_name)
                .await
                .unwrap()
                .status
                .unwrap()
                .allocation
                .as_ref(),
            Some(allocation)
        );
    }
    let mut stale = sets.get(NAME).await.unwrap();
    stale.metadata.uid = Some("old-incarnation".into());
    operator.reconcile(&stale).await.unwrap();
    assert_eq!(
        sets.get(NAME).await.unwrap().status.unwrap().allocation,
        Some(first)
    );
    fixture.api.check();
}

#[tokio::test]
async fn a_fresh_runtime_withdraws_routing_without_adopting_or_claiming_cleanup() {
    let fixture = Fixture::new().await;
    fixture.serving().await;
    let original = fixture.set().await;
    let original_status = original.status.clone().unwrap();
    let operator = Operator::new(fixture.client.clone(), config()).unwrap();
    operator.reconcile(&original).await.unwrap();
    let blocked = fixture.set().await;
    let status = blocked.status.as_ref().unwrap();
    assert_eq!(status.phase, ControllerPhase::Blocked);
    assert!(status
        .reason
        .as_ref()
        .unwrap()
        .contains("restart recovery and adoption are unsupported"));
    assert_eq!(status.allocation, original_status.allocation);
    assert_eq!(status.bindings, original_status.bindings);
    assert!(status.service_endpoint.is_none());
    assert!(blocked
        .finalizers()
        .iter()
        .any(|value| value == resources::FINALIZER));
    assert_eq!(
        fixture.fm.observe().await.unwrap().unwrap().status.phase,
        kuberic_fm::Phase::Serving
    );
    assert_eq!(
        Api::<Service>::namespaced(fixture.client.clone(), NAMESPACE)
            .get(&resources::write_service_name(NAME))
            .await
            .unwrap()
            .spec
            .unwrap()
            .selector,
        Some(resources::disabled_selector(&blocked).unwrap())
    );
    assert_eq!(
        fixture.pods().await.len(),
        original_status.allocation.unwrap().nodes.len()
    );
    operator.reconcile(&blocked).await.unwrap();
    fixture.api.check();
}

#[tokio::test]
async fn uid_fenced_reconciliation_refuses_a_recreated_parent() {
    let fixture = Fixture::new().await;
    let error = fixture
        .controller
        .reconcile_uid(NAME, "old-incarnation")
        .await
        .unwrap_err();
    assert!(error.is::<StaleObservation>());
    assert!(fixture.set().await.finalizers().is_empty());
    fixture.api.check();
}

#[tokio::test]
async fn stopping_the_original_runtime_blocks_without_deleting_its_nodes() {
    let fixture = Fixture::new().await;
    let operator = Operator::new(fixture.client.clone(), config()).unwrap();
    for _ in 0..16 {
        operator.reconcile(&fixture.set().await).await.unwrap();
    }
    let original = fixture.set().await;
    operator.stop().await.unwrap();
    let blocked = fixture.set().await;
    let status = blocked.status.as_ref().unwrap();
    assert_eq!(status.phase, ControllerPhase::Blocked);
    assert!(status.reason.as_ref().unwrap().contains("operator stopped"));
    assert_eq!(status.allocation, original.status.unwrap().allocation);
    assert!(blocked
        .finalizers()
        .iter()
        .any(|value| value == resources::FINALIZER));
    assert!(fixture
        .pods()
        .await
        .iter()
        .all(|pod| pod.metadata.deletion_timestamp.is_none()));
    assert_eq!(
        Api::<Service>::namespaced(fixture.client.clone(), NAMESPACE)
            .get(&resources::write_service_name(NAME))
            .await
            .unwrap()
            .spec
            .unwrap()
            .selector,
        Some(resources::disabled_selector(&blocked).unwrap())
    );
    fixture.api.check();
}

#[tokio::test]
async fn deployed_receipts_are_original_cached_evidence_and_failures_withdraw_routing() {
    let fixture = Fixture::new().await;
    fixture.serving().await;
    let cohort = fixture.set().await.status.unwrap().cohort().unwrap();
    let processes = Arc::new(PodProcesses::new(fixture.client.clone(), NAMESPACE));
    assert!(processes.observe(&cohort).await.unwrap().is_none());
    processes.reserve(&cohort.allocation).await.unwrap();
    let observed = processes.observe(&cohort).await.unwrap().unwrap();
    assert!(observed
        .nodes
        .iter()
        .all(|node| node.outcome == CleanupState::Pending));
    let pods: Api<Pod> = Api::namespaced(fixture.client.clone(), NAMESPACE);
    let first = &cohort.allocation.nodes[0];
    let mut pod = pods.get(&first.pod_name).await.unwrap();
    assert!(pod.finalizers().iter().any(|value| value == FINALIZER));
    pod.metadata
        .finalizers
        .as_mut()
        .unwrap()
        .push("testing.kuberic.io/foreign".into());
    let mut pod = pods
        .replace(&first.pod_name, &PostParams::default(), &pod)
        .await
        .unwrap();
    let report = serde_json::json!({
        "resource_uid": cohort.allocation.resource_uid, "deployment": cohort.allocation.deployment,
        "pod_uid": cohort.bindings[0].pod_uid, "node_id": first.node_id,
        "node_reaped": true, "node_exit_success": true, "application_reaped": true,
        "resources_joined": true, "forced": false
    });
    pod.status.as_mut().unwrap().container_statuses = Some(vec![ContainerStatus {
        name: "node".into(),
        state: Some(ContainerState {
            terminated: Some(ContainerStateTerminated {
                exit_code: 0,
                message: Some(report.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }]);
    pods.replace_status(&first.pod_name, &PostParams::default(), &pod)
        .await
        .unwrap();
    let observed = processes.observe(&cohort).await.unwrap().unwrap();
    assert!(matches!(
        observed.nodes[0].outcome,
        CleanupState::Complete(_)
    ));
    let mut pod = pods.get(&first.pod_name).await.unwrap();
    assert_eq!(pod.finalizers(), ["testing.kuberic.io/foreign"]);
    pod.metadata.finalizers = None;
    let pod = pods
        .replace(&first.pod_name, &PostParams::default(), &pod)
        .await
        .unwrap();
    pods.delete(
        &first.pod_name,
        &DeleteParams {
            preconditions: Some(Preconditions {
                uid: pod.uid(),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    fixture.api.finalize(
        NAMESPACE,
        Kind::Pod,
        &first.pod_name,
        &cohort.bindings[0].pod_uid,
    );
    assert!(matches!(
        processes.observe(&cohort).await.unwrap().unwrap().nodes[0].outcome,
        CleanupState::Complete(_)
    ));
    let second = &cohort.allocation.nodes[1];
    let mut failed = pods.get(&second.pod_name).await.unwrap();
    failed.status.as_mut().unwrap().container_statuses = Some(vec![ContainerStatus {
        name: "node".into(),
        state: Some(ContainerState {
            terminated: Some(ContainerStateTerminated {
                exit_code: 137,
                signal: Some(9),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }]);
    pods.replace_status(&second.pod_name, &PostParams::default(), &failed)
        .await
        .unwrap();
    assert!(matches!(
        processes.observe(&cohort).await.unwrap().unwrap().nodes[1].outcome,
        CleanupState::Failed(_)
    ));
    let controller = Reconciler::new(
        fixture.client.clone(),
        support::settings(),
        fixture.fm.clone(),
        processes,
    )
    .unwrap();
    assert!(controller.reconcile(NAME).await.unwrap().blocked.is_some());
    let set = fixture.set().await;
    assert_eq!(set.status.as_ref().unwrap().phase, ControllerPhase::Blocked);
    assert!(pods
        .get(&second.pod_name)
        .await
        .unwrap()
        .finalizers()
        .iter()
        .any(|value| value == FINALIZER));
    assert_eq!(
        Api::<Service>::namespaced(fixture.client.clone(), NAMESPACE)
            .get(&resources::write_service_name(NAME))
            .await
            .unwrap()
            .spec
            .unwrap()
            .selector,
        Some(resources::disabled_selector(&set).unwrap())
    );
    fixture.api.check();
}
