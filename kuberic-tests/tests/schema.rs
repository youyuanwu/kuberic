use kuberic_tests::controller as support;

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use kube::CustomResourceExt;
use kuberic_operator::{FmApi, KubericSet, ProcessApi, Reconciler};
use serde_json::Value;

fn properties(value: &Value, fields: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if let Some(Value::Object(properties)) = object.get("properties") {
                fields.extend(properties.keys().cloned());
            }
            for value in object.values() {
                properties(value, fields);
            }
        }
        Value::Array(values) => {
            for value in values {
                properties(value, fields);
            }
        }
        _ => {}
    }
}

#[test]
fn crd_schema_does_not_expose_backend_local_ownership_settings_or_receipts() {
    let schema = serde_json::to_value(KubericSet::crd()).unwrap();
    let mut fields = BTreeSet::new();
    properties(&schema, &mut fields);
    for field in [
        "fmOwner",
        "processOwner",
        "settings",
        "runRoot",
        "cleanupReceipts",
        "nodeReaped",
        "nodeExitSuccess",
        "applicationReaped",
        "resourcesJoined",
        "forced",
    ] {
        assert!(
            !fields.contains(field),
            "backend-local {field} leaked into the CRD schema"
        );
    }
}

#[tokio::test]
async fn allocation_conflict_reuses_backend_draft_without_a_second_admission() {
    let fixture = support::Fixture::new().await;
    fixture.controller.reconcile(support::NAME).await.unwrap();
    fixture.api.conflict_next(support::api::Kind::Set, true);
    assert!(fixture.controller.reconcile(support::NAME).await.is_err());
    assert!(fixture.set().await.status.is_none());
    let draft = fixture.fm.allocation().await.unwrap().unwrap();
    assert_eq!(
        fixture.processes.allocation().await.unwrap().as_ref(),
        Some(&draft)
    );
    let recreated = fixture.recreate();
    recreated.reconcile(support::NAME).await.unwrap();
    assert_eq!(
        fixture.set().await.status.unwrap().allocation.as_ref(),
        Some(&draft)
    );
    assert_eq!(fixture.fm.starts.load(Ordering::SeqCst), 0);
    fixture.api.check();
}

#[tokio::test]
async fn persisted_allocation_cannot_admit_a_fresh_backend_before_native_start() {
    let fixture = support::Fixture::new().await;
    fixture.controller.reconcile(support::NAME).await.unwrap();
    fixture.controller.reconcile(support::NAME).await.unwrap();
    let fresh_fm = Arc::new(support::ScriptedFm::new());
    let controller = Reconciler::new(
        fixture.client.clone(),
        support::settings(),
        fresh_fm.clone(),
        fixture.processes.clone(),
    )
    .unwrap();
    assert!(controller
        .reconcile(support::NAME)
        .await
        .unwrap()
        .blocked
        .is_some());
    assert!(fresh_fm.allocation().await.unwrap().is_none());
    assert_eq!(fresh_fm.starts.load(Ordering::SeqCst), 0);
    assert!(fixture.pods().await.is_empty());
    fixture.api.check();
}

#[tokio::test]
async fn accepted_listener_ports_are_not_rederived_from_new_bootstrap_defaults() {
    let fixture = support::Fixture::new().await;
    fixture.serving().await;
    let original = fixture.set().await.status.unwrap().cohort().unwrap();
    let mut settings = support::settings();
    settings.node_port += 1;
    let controller = Reconciler::new(
        fixture.client.clone(),
        settings,
        fixture.fm.clone(),
        fixture.processes.clone(),
    )
    .unwrap();
    let outcome = controller.reconcile(support::NAME).await.unwrap();
    assert!(outcome.blocked.is_none());
    assert!(!outcome.changed);
    let current = fixture.set().await.status.unwrap().cohort().unwrap();
    assert_eq!(current, original);
    for (node, binding) in current
        .fm_config()
        .unwrap()
        .nodes
        .iter()
        .zip(&original.bindings)
    {
        assert_eq!(node.address.port(), binding.control_port);
    }
}
