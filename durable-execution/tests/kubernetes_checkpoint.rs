#![cfg(feature = "kubernetes")]

use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use http::{Method, Request, Response, StatusCode};
use kube::{Client, client::Body};
use serde_json::{Value, json};
use tower::service_fn;

use kuberic_durable_execution::{
    CasOutcome, CheckpointEnvelope, CheckpointLimits, CheckpointStore, DurableHost, ExactBytes,
    ExecutionId, ExecutionSpec, HostEpoch, HostOutcome, KubernetesCheckpointStore, ReloadReason,
    StorageRevision, StoreErrorKind, StoreOperation, TerminalOutcome, Workflow, WorkflowContext,
};

#[derive(Clone, Debug)]
enum Step {
    Response(StatusCode, Value),
    TransportFailure,
}

#[derive(Clone, Debug)]
struct RecordedRequest {
    method: Method,
    uri: String,
    body: Value,
}

#[derive(Clone, Debug)]
struct Harness {
    steps: Arc<Mutex<VecDeque<Step>>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl Harness {
    fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn client(&self) -> Client {
        let steps = Arc::clone(&self.steps);
        let requests = Arc::clone(&self.requests);
        let service = service_fn(move |request: Request<Body>| {
            let steps = Arc::clone(&steps);
            let requests = Arc::clone(&requests);
            async move {
                let (parts, body) = request.into_parts();
                let body = body
                    .collect_bytes()
                    .await
                    .map_err(|error| io::Error::other(error.to_string()))?;
                let body = if body.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&body)
                        .map_err(|error| io::Error::other(error.to_string()))?
                };
                requests
                    .lock()
                    .expect("requests lock")
                    .push(RecordedRequest {
                        method: parts.method,
                        uri: parts.uri.to_string(),
                        body,
                    });
                match steps.lock().expect("steps lock").pop_front() {
                    Some(Step::Response(status, body)) => Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::to_vec(&body).expect("response JSON"),
                        ))
                        .map_err(io::Error::other),
                    Some(Step::TransportFailure) => {
                        Err(io::Error::other("injected transport failure"))
                    }
                    None => Err(io::Error::other("unexpected Kubernetes request")),
                }
            }
        });
        Client::new(service, "default")
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("requests lock").clone()
    }

    fn assert_consumed(&self) {
        assert!(
            self.steps.lock().expect("steps lock").is_empty(),
            "unconsumed scripted responses"
        );
    }
}

fn execution(byte: u8) -> ExecutionId {
    ExecutionId::from_bytes([byte; 16])
}

fn checkpoint(label: &[u8]) -> CheckpointEnvelope {
    CheckpointEnvelope::new(3, ExactBytes::new(label))
}

fn config_map_response(
    execution_id: ExecutionId,
    revision: Option<&str>,
    checkpoint: Option<&CheckpointEnvelope>,
) -> Value {
    let mut metadata = json!({
        "name": KubernetesCheckpointStore::object_name(execution_id),
        "namespace": "checkpoint-tests"
    });
    if let Some(revision) = revision {
        metadata["resourceVersion"] = json!(revision);
    }
    let mut object = json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": metadata
    });
    if let Some(checkpoint) = checkpoint {
        object["data"] = json!({
            "checkpoint.json": serde_json::to_string(checkpoint).expect("checkpoint JSON")
        });
    }
    object
}

fn api_error(code: u16, reason: &str) -> Step {
    Step::Response(
        StatusCode::from_u16(code).expect("valid status"),
        json!({
            "apiVersion": "v1",
            "kind": "Status",
            "metadata": {},
            "status": "Failure",
            "message": format!("{reason} without credential or checkpoint content"),
            "reason": reason,
            "code": code
        }),
    )
}

#[tokio::test]
async fn deterministic_names_and_namespace_validation() {
    let expected = format!("kuberic-checkpoint-{}", "ab".repeat(16));
    assert_eq!(
        KubernetesCheckpointStore::object_name(execution(0xab)),
        expected
    );
    assert!(expected.len() < 253);
    assert!(
        expected
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    );

    let harness = Harness::new([]);
    let error = KubernetesCheckpointStore::new(harness.client(), "Invalid_Namespace")
        .expect_err("invalid namespace must fail before requests");
    assert_eq!(error.kind(), StoreErrorKind::Other);
    assert!(harness.requests().is_empty());
}

#[tokio::test]
async fn create_and_replace_round_trip_exact_opaque_revisions_and_metrics() {
    let execution_id = execution(0x11);
    let first = checkpoint(b"first");
    let second = checkpoint(b"second");
    let harness = Harness::new([
        Step::Response(
            StatusCode::CREATED,
            config_map_response(execution_id, Some("opaque-alpha"), Some(&first)),
        ),
        Step::Response(
            StatusCode::OK,
            config_map_response(execution_id, Some("rv/not-a-number"), Some(&second)),
        ),
    ]);
    let store =
        KubernetesCheckpointStore::new(harness.client(), "checkpoint-tests").expect("store");

    let first_outcome = store
        .compare_and_swap(execution_id, None, first.clone())
        .await
        .expect("create result");
    assert_eq!(
        first_outcome,
        CasOutcome::Accepted(StorageRevision::new("opaque-alpha").expect("revision"))
    );
    let second_outcome = store
        .compare_and_swap(
            execution_id,
            Some(StorageRevision::new("opaque-alpha").expect("revision")),
            second.clone(),
        )
        .await
        .expect("replace result");
    assert_eq!(
        second_outcome,
        CasOutcome::Accepted(StorageRevision::new("rv/not-a-number").expect("revision"))
    );

    let requests = harness.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, Method::POST);
    assert_eq!(requests[1].method, Method::PUT);
    assert!(
        requests[0]
            .uri
            .starts_with("/api/v1/namespaces/checkpoint-tests/configmaps?")
    );
    assert!(requests[0].body["metadata"]["resourceVersion"].is_null());
    assert_eq!(
        requests[1].body["metadata"]["resourceVersion"],
        "opaque-alpha"
    );
    assert_eq!(
        requests[0].body["data"]["checkpoint.json"],
        serde_json::to_string(&first).expect("checkpoint JSON")
    );
    assert_eq!(
        requests[1].body["data"]["checkpoint.json"],
        serde_json::to_string(&second).expect("checkpoint JSON")
    );

    let metrics = store.metrics().snapshot();
    assert_eq!(metrics.accepted_writes(), 2);
    assert_eq!(
        metrics.checkpoint_bytes(),
        (serde_json::to_vec(&first).expect("JSON").len()
            + serde_json::to_vec(&second).expect("JSON").len()) as u64
    );
    assert!(metrics.object_bytes() > metrics.checkpoint_bytes());
    assert_eq!(metrics.measurement_failures(), 0);
    harness.assert_consumed();
}

#[tokio::test]
async fn load_distinguishes_absence_success_and_malformed_objects() {
    let execution_id = execution(0x22);
    let stored = checkpoint(b"stored");
    let mut invalid = config_map_response(execution_id, Some("rv-invalid"), None);
    invalid["data"] = json!({"checkpoint.json": "not-json"});
    let harness = Harness::new([
        api_error(404, "NotFound"),
        Step::Response(
            StatusCode::OK,
            config_map_response(execution_id, Some("opaque-load"), Some(&stored)),
        ),
        Step::Response(
            StatusCode::OK,
            config_map_response(execution_id, None, Some(&stored)),
        ),
        Step::Response(
            StatusCode::OK,
            config_map_response(execution_id, Some("rv-missing-data"), None),
        ),
        Step::Response(
            StatusCode::OK,
            config_map_response(execution_id, Some(""), Some(&stored)),
        ),
        Step::Response(StatusCode::OK, invalid),
    ]);
    let store =
        KubernetesCheckpointStore::new(harness.client(), "checkpoint-tests").expect("store");

    assert_eq!(store.load(execution_id).await.expect("absence"), None);
    let loaded = store
        .load(execution_id)
        .await
        .expect("load")
        .expect("stored object");
    assert_eq!(loaded.revision().as_str(), "opaque-load");
    assert_eq!(loaded.checkpoint(), &stored);

    let missing_revision = store
        .load(execution_id)
        .await
        .expect_err("missing revision");
    assert_eq!(missing_revision.kind(), StoreErrorKind::MalformedResponse);
    assert!(missing_revision.description().contains("load"));
    let missing_data = store.load(execution_id).await.expect_err("missing data");
    assert_eq!(missing_data.kind(), StoreErrorKind::MalformedResponse);
    assert!(missing_data.description().contains("checkpoint.json"));
    let empty_revision = store.load(execution_id).await.expect_err("empty revision");
    assert_eq!(empty_revision.kind(), StoreErrorKind::MalformedResponse);
    assert!(empty_revision.description().contains("resourceVersion"));
    let malformed_checkpoint = store.load(execution_id).await.expect_err("invalid JSON");
    assert_eq!(
        malformed_checkpoint.kind(),
        StoreErrorKind::MalformedResponse
    );
    assert!(!malformed_checkpoint.description().contains("not-json"));
    harness.assert_consumed();
}

#[tokio::test]
async fn conflicts_cover_create_stale_replace_and_deleted_object_without_recreation() {
    let execution_id = execution(0x33);
    let harness = Harness::new([
        api_error(409, "AlreadyExists"),
        api_error(409, "Conflict"),
        api_error(404, "NotFound"),
    ]);
    let store =
        KubernetesCheckpointStore::new(harness.client(), "checkpoint-tests").expect("store");

    assert_eq!(
        store
            .compare_and_swap(execution_id, None, checkpoint(b"create-race"))
            .await
            .expect("create conflict"),
        CasOutcome::Conflict
    );
    for revision in ["stale", "deleted"] {
        assert_eq!(
            store
                .compare_and_swap(
                    execution_id,
                    Some(StorageRevision::new(revision).expect("revision")),
                    checkpoint(revision.as_bytes()),
                )
                .await
                .expect("replace conflict"),
            CasOutcome::Conflict
        );
    }

    let requests = harness.requests();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.method.clone())
            .collect::<Vec<_>>(),
        [Method::POST, Method::PUT, Method::PUT]
    );
    assert_eq!(store.metrics().snapshot(), Default::default());
    harness.assert_consumed();
}

#[tokio::test]
async fn mutation_classification_is_conservative_and_diagnostics_are_portable() {
    let execution_id = execution(0x44);
    let secret_checkpoint = checkpoint(b"secret-checkpoint-content");
    let harness = Harness::new([
        Step::Response(
            StatusCode::FORBIDDEN,
            json!({
                "apiVersion": "v1",
                "kind": "Status",
                "metadata": {},
                "status": "Failure",
                "message": "Bearer credential-secret is forbidden",
                "reason": "Forbidden",
                "code": 403
            }),
        ),
        api_error(400, "BadRequest"),
        api_error(401, "Unauthorized"),
        api_error(413, "RequestEntityTooLarge"),
        api_error(415, "UnsupportedMediaType"),
        api_error(422, "Invalid"),
        api_error(429, "TooManyRequests"),
        api_error(500, "InternalError"),
        api_error(504, "Timeout"),
        Step::TransportFailure,
        Step::Response(
            StatusCode::CREATED,
            config_map_response(execution_id, None, Some(&secret_checkpoint)),
        ),
        Step::Response(
            StatusCode::CREATED,
            config_map_response(execution_id, Some(""), Some(&secret_checkpoint)),
        ),
    ]);
    let store =
        KubernetesCheckpointStore::new(harness.client(), "checkpoint-tests").expect("store");

    let forbidden = store
        .compare_and_swap(execution_id, None, secret_checkpoint.clone())
        .await
        .expect_err("403 is a definite rejection");
    assert_eq!(forbidden.kind(), StoreErrorKind::Authorization);
    assert!(forbidden.description().contains("compare-and-swap"));
    assert!(forbidden.description().contains("code=403"));
    assert!(forbidden.description().contains("reason=Forbidden"));
    assert!(
        !forbidden
            .description()
            .contains("secret-checkpoint-content")
    );
    assert!(!forbidden.description().contains("credential-secret"));

    for (expected_kind, reason) in [
        (StoreErrorKind::Other, "BadRequest"),
        (StoreErrorKind::Authorization, "Unauthorized"),
        (StoreErrorKind::Other, "RequestEntityTooLarge"),
        (StoreErrorKind::Other, "UnsupportedMediaType"),
        (StoreErrorKind::Other, "Invalid"),
        (StoreErrorKind::Unavailable, "TooManyRequests"),
    ] {
        let rejected = store
            .compare_and_swap(execution_id, None, secret_checkpoint.clone())
            .await
            .expect_err("API response proves rejection");
        assert_eq!(rejected.kind(), expected_kind);
        assert!(rejected.description().contains(reason));
        assert!(!rejected.description().contains("secret-checkpoint-content"));
    }

    for expected in [
        None,
        Some(StorageRevision::new("timeout").expect("revision")),
        None,
        None,
        None,
    ] {
        assert_eq!(
            store
                .compare_and_swap(execution_id, expected, secret_checkpoint.clone())
                .await
                .expect("ambiguous mutation"),
            CasOutcome::OutcomeUnknown
        );
    }
    assert_eq!(store.metrics().snapshot(), Default::default());
    harness.assert_consumed();
}

#[tokio::test]
async fn load_errors_map_to_every_relevant_portable_category() {
    let execution_id = execution(0x55);
    let harness = Harness::new([
        api_error(403, "Forbidden"),
        api_error(504, "Timeout"),
        api_error(500, "InternalError"),
        api_error(422, "Invalid"),
        Step::TransportFailure,
    ]);
    let store =
        KubernetesCheckpointStore::new(harness.client(), "checkpoint-tests").expect("store");

    for expected in [
        StoreErrorKind::Authorization,
        StoreErrorKind::Timeout,
        StoreErrorKind::Unavailable,
        StoreErrorKind::Other,
        StoreErrorKind::Unavailable,
    ] {
        let error = store.load(execution_id).await.expect_err("load failure");
        assert_eq!(error.kind(), expected);
        assert!(error.description().contains("load"));
        assert!(!error.description().contains("checkpoint-content"));
    }
    harness.assert_consumed();
}

#[tokio::test]
async fn oversized_checkpoint_is_rejected_before_dispatch() {
    let execution_id = execution(0x66);
    let harness = Harness::new([]);
    let store =
        KubernetesCheckpointStore::new(harness.client(), "checkpoint-tests").expect("store");
    let oversized = checkpoint(&vec![0_u8; 1024 * 1024]);
    let error = store
        .compare_and_swap(execution_id, None, oversized)
        .await
        .expect_err("oversized checkpoint");
    assert_eq!(error.kind(), StoreErrorKind::Other);
    assert!(error.description().contains("ConfigMap data limit"));
    assert!(harness.requests().is_empty());
}

struct ImmediateWorkflow;

#[async_trait(?Send)]
impl Workflow for ImmediateWorkflow {
    async fn run(&self, _context: &mut WorkflowContext<'_>, _input: ExactBytes) -> TerminalOutcome {
        TerminalOutcome::succeeded(ExactBytes::new(b"done".to_vec()))
    }
}

#[tokio::test]
async fn host_rejects_unsupported_format_and_reloads_after_unit_conflict() {
    let execution_id = execution(0x77);
    let unsupported = CheckpointEnvelope::new(99, ExactBytes::default());
    let unsupported_harness = Harness::new([Step::Response(
        StatusCode::OK,
        config_map_response(execution_id, Some("rv-unsupported"), Some(&unsupported)),
    )]);
    let unsupported_store =
        KubernetesCheckpointStore::new(unsupported_harness.client(), "checkpoint-tests")
            .expect("store");
    let mut unsupported_host = DurableHost::new(
        unsupported_store,
        HostEpoch::from_bytes([1; 16]),
        CheckpointLimits::new(8, 4096).expect("limits"),
    );
    let execution_spec = ExecutionSpec::new(execution_id, ExactBytes::default(), 64);
    assert!(matches!(
        unsupported_host
            .turn(&ImmediateWorkflow, execution_spec.clone())
            .await,
        HostOutcome::CheckpointRejected(_)
    ));

    let reload_harness = Harness::new([
        api_error(404, "NotFound"),
        api_error(409, "AlreadyExists"),
        api_error(403, "Forbidden"),
    ]);
    let reload_store =
        KubernetesCheckpointStore::new(reload_harness.client(), "checkpoint-tests").expect("store");
    let mut reload_host = DurableHost::new(
        reload_store,
        HostEpoch::from_bytes([2; 16]),
        CheckpointLimits::new(8, 4096).expect("limits"),
    );
    assert!(matches!(
        reload_host
            .turn(&ImmediateWorkflow, execution_spec.clone())
            .await,
        HostOutcome::ReloadRequired {
            reason: ReloadReason::Conflict,
            ..
        }
    ));
    assert!(matches!(
        reload_host.turn(&ImmediateWorkflow, execution_spec).await,
        HostOutcome::StoreFailed {
            operation: StoreOperation::Load,
            ..
        }
    ));
    reload_harness.assert_consumed();
}
