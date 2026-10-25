//! The deliberately narrow Kubernetes storage boundary of the in-process harness.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use http::{Method, Request, Response, StatusCode};
use k8s_openapi::api::core::v1::Secret;
use kube::{Client, client::Body};
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum Kind {
    KubericSet,
    Pod,
    Pvc,
    Service,
    Secret,
}

impl Kind {
    fn resource(self) -> &'static str {
        match self {
            Self::KubericSet => "kubericsets",
            Self::Pod => "pods",
            Self::Pvc => "persistentvolumeclaims",
            Self::Service => "services",
            Self::Secret => "secrets",
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Self::KubericSet => "KubericSet",
            Self::Pod => "Pod",
            Self::Pvc => "PersistentVolumeClaim",
            Self::Service => "Service",
            Self::Secret => "Secret",
        }
    }

    fn api_version(self) -> &'static str {
        match self {
            Self::KubericSet => "operator.kuberic.io/v1alpha1",
            _ => "v1",
        }
    }
}

type Key = (String, Kind, String);

fn prune_structural(value: &mut Value, schema: &Value) {
    if schema["x-kubernetes-preserve-unknown-fields"] == true {
        return;
    }
    match value {
        Value::Object(fields) if schema["type"] == "object" => {
            let properties = schema.get("properties").and_then(Value::as_object);
            fields.retain(|name, child| {
                if let Some(property) = properties.and_then(|properties| properties.get(name)) {
                    prune_structural(child, property);
                    true
                } else {
                    match schema.get("additionalProperties") {
                        Some(Value::Bool(true)) => true,
                        Some(property @ Value::Object(_)) => {
                            prune_structural(child, property);
                            true
                        }
                        _ => false,
                    }
                }
            });
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for item in items {
                    prune_structural(item, item_schema);
                }
            }
        }
        _ => {}
    }
}

fn prune_kuberic_status(value: &mut Value) {
    static CRD: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../../deploy/crd.json"))
            .expect("checked-in structural CRD")
    });
    if let Some(status) = value.get_mut("status") {
        prune_structural(
            status,
            &CRD["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["status"],
        );
    }
}

#[derive(Default)]
struct Store {
    objects: BTreeMap<Key, Value>,
    revision: u64,
    fatal: Option<String>,
    operation_barrier: Option<ArmedBarrier>,
    completion_conflict: Option<ArmedBarrier>,
}

impl Store {
    fn rv(&mut self) -> String {
        self.revision += 1;
        self.revision.to_string()
    }

    fn unsupported(&mut self, message: impl Into<String>) -> ApiError {
        let message = message.into();
        self.fatal.get_or_insert_with(|| message.clone());
        ApiError(501, "NotImplemented", message)
    }
}

struct ArmedBarrier {
    key: Key,
    uid: String,
    entered: oneshot::Sender<Value>,
    release: oneshot::Receiver<()>,
    call_kind: Option<&'static str>,
    operation_fragment: Option<String>,
    matcher: Option<super::faults::Barrier>,
}

/// Both barriers publish the persisted object before waiting, without a store lock.
/// Dropping the handle releases a waiting request. Cancelling the caller never rolls
/// back the committed object (or the conflict's metadata-only resourceVersion bump).
pub(super) struct StatusBarrier {
    entered: oneshot::Receiver<Value>,
    release: Option<oneshot::Sender<()>>,
}

impl StatusBarrier {
    pub(super) async fn entered(&mut self) -> anyhow::Result<Value> {
        Ok((&mut self.entered).await?)
    }

    pub(super) fn release(mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

#[derive(Clone)]
pub(super) struct ApiServer {
    namespace: String,
    store: Arc<Mutex<Store>>,
    changed: Arc<Notify>,
}

impl ApiServer {
    pub(super) fn new(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            store: Arc::new(Mutex::new(Store::default())),
            changed: Arc::new(Notify::new()),
        }
    }

    pub(super) fn client(&self) -> Client {
        let server = self.clone();
        Client::new(
            tower::service_fn(move |request: Request<Body>| {
                let server = server.clone();
                async move { Ok::<_, Infallible>(server.request(request).await) }
            }),
            self.namespace.clone(),
        )
    }

    /// Notifications are hints: register `notified()` before inspecting the store.
    pub(super) fn changed(&self) -> Arc<Notify> {
        self.changed.clone()
    }

    pub(super) fn sticky_error(&self) -> Option<String> {
        self.store.lock().unwrap().fatal.clone()
    }

    pub(super) fn check(&self) -> anyhow::Result<()> {
        match self.sticky_error() {
            Some(message) => anyhow::bail!("unsupported Kubernetes API request: {message}"),
            None => Ok(()),
        }
    }

    /// Supervisor access is scoped to this server's default namespace. API requests
    /// may use other namespaces, but cannot see each other's objects.
    pub(super) fn snapshot(&self, kind: Kind) -> Vec<Value> {
        self.store
            .lock()
            .unwrap()
            .objects
            .iter()
            .filter(|((namespace, candidate, _), _)| {
                namespace == &self.namespace && *candidate == kind
            })
            .map(|(_, object)| object.clone())
            .collect()
    }

    pub(super) fn get(&self, kind: Kind, name: &str) -> Option<Value> {
        self.store
            .lock()
            .unwrap()
            .objects
            .get(&self.key(kind, name))
            .cloned()
    }

    #[cfg(test)]
    pub(super) fn pending_deletions(&self, kind: Kind) -> Vec<Value> {
        self.snapshot(kind)
            .into_iter()
            .filter(is_deleting)
            .collect()
    }

    /// Only the exact, already deleting incarnation can be removed. No cascading
    /// deletion: in particular, removing a Pod does not remove its PVC.
    pub(super) fn finalize_deletion(
        &self,
        kind: Kind,
        name: &str,
        uid: &str,
    ) -> anyhow::Result<bool> {
        let mut store = self.store.lock().unwrap();
        let key = self.key(kind, name);
        let Some(object) = store.objects.get(&key) else {
            return Ok(false);
        };
        anyhow::ensure!(object["metadata"]["uid"] == uid, "deletion UID changed");
        anyhow::ensure!(is_deleting(object), "deletion was not requested");
        store.objects.remove(&key);
        store.rv();
        drop(store);
        self.changed.notify_waiters();
        Ok(true)
    }

    /// Publish only infrastructure facts, with exact UID and RV fencing. The
    /// callback cannot alter identities, spec, labels, controller status or reports.
    /// Pod IP/phase and the control-address annotation, or PVC phase, are supported.
    pub(super) fn update_infrastructure(
        &self,
        kind: Kind,
        name: &str,
        uid: &str,
        resource_version: &str,
        update: impl FnOnce(&mut Value),
    ) -> anyhow::Result<Value> {
        let mut store = self.store.lock().unwrap();
        let key = self.key(kind, name);
        let old = store
            .objects
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("object absent"))?;
        anyhow::ensure!(old["metadata"]["uid"] == uid, "infrastructure UID changed");
        anyhow::ensure!(
            old["metadata"]["resourceVersion"] == resource_version,
            "infrastructure resourceVersion changed"
        );
        anyhow::ensure!(!is_deleting(old), "infrastructure object is deleting");
        let mut next = old.clone();
        update(&mut next);
        let mut allowed = old.clone();
        match kind {
            Kind::Pod => {
                for field in ["podIP", "phase"] {
                    copy_optional_nested(&next, &mut allowed, "status", field)?;
                }
                const ADDRESS: &str = "operator.kuberic.io/control-address";
                let address = next
                    .pointer("/metadata/annotations")
                    .and_then(|v| v.get(ADDRESS));
                if let Some(address) = address {
                    anyhow::ensure!(address.is_string(), "control address must be a string");
                    allowed["metadata"]
                        .as_object_mut()
                        .unwrap()
                        .entry("annotations")
                        .or_insert_with(|| json!({}))[ADDRESS] = address.clone();
                }
            }
            Kind::Pvc => copy_optional_nested(&next, &mut allowed, "status", "phase")?,
            _ => anyhow::bail!("only Pod/PVC infrastructure updates are supported"),
        }
        anyhow::ensure!(allowed == next, "update changed non-infrastructure fields");
        next["metadata"]["resourceVersion"] = json!(store.rv());
        store.objects.insert(key, next.clone());
        drop(store);
        self.changed.notify_waiters();
        Ok(next)
    }

    #[cfg(test)]
    pub(super) fn arm_operation_barrier(&self, name: &str) -> anyhow::Result<StatusBarrier> {
        self.arm_matching(name, false, None, None)
    }

    /// Targets the next status write that increases accepted completion count.
    /// Instead of accepting its status, bump only the stored RV and return 409
    /// after release. The same submitted RV will genuinely remain stale on retry.
    pub(super) fn arm_completion_conflict(&self, name: &str) -> anyhow::Result<StatusBarrier> {
        self.arm_matching(name, true, None, None)
    }

    pub(super) fn arm_matching(
        &self,
        name: &str,
        conflict: bool,
        call_kind: Option<&'static str>,
        operation_fragment: Option<&str>,
    ) -> anyhow::Result<StatusBarrier> {
        let mut store = self.store.lock().unwrap();
        let key = self.key(Kind::KubericSet, name);
        let object = store
            .objects
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("set absent"))?;
        let uid = object["metadata"]["uid"].as_str().unwrap().to_owned();
        let slot = if conflict {
            &mut store.completion_conflict
        } else {
            &mut store.operation_barrier
        };
        anyhow::ensure!(slot.is_none(), "status barrier is already armed");
        let (entered_tx, entered) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        *slot = Some(ArmedBarrier {
            key,
            uid,
            entered: entered_tx,
            release: release_rx,
            call_kind,
            operation_fragment: operation_fragment.map(str::to_owned),
            matcher: None,
        });
        Ok(StatusBarrier {
            entered,
            release: Some(release),
        })
    }

    pub(super) fn arm_typed(
        &self,
        name: &str,
        matcher: super::faults::Barrier,
    ) -> anyhow::Result<StatusBarrier> {
        let completion = matcher.stage() == super::faults::BarrierStage::Completion;
        let barrier = self.arm_matching(name, completion, None, None)?;
        let mut store = self.store.lock().unwrap();
        let slot = if completion {
            &mut store.completion_conflict
        } else {
            &mut store.operation_barrier
        };
        slot.as_mut().unwrap().matcher = Some(matcher);
        Ok(barrier)
    }

    pub(super) fn release_barriers(&self) {
        let mut store = self.store.lock().unwrap();
        store.operation_barrier.take();
        store.completion_conflict.take();
    }

    fn key(&self, kind: Kind, name: &str) -> Key {
        (self.namespace.clone(), kind, name.to_owned())
    }

    async fn request(&self, request: Request<Body>) -> Response<Body> {
        let (parts, body) = request.into_parts();
        let body = match body.collect_bytes().await {
            Ok(bytes) => bytes,
            Err(error) => return ApiError::bad_request(error.to_string()).response(),
        };
        let (result, mutated) = {
            let mut store = self.store.lock().unwrap();
            let revision = store.revision;
            let result = execute(&mut store, &parts, &body);
            (result, revision != store.revision || store.fatal.is_some())
        };
        if mutated {
            self.changed.notify_waiters();
        }
        match result {
            Ok(mut outcome) => {
                if let Some(barrier) = outcome.barrier.take() {
                    let _ = barrier.entered.send(outcome.persisted.take().unwrap());
                    let _ = barrier.release.await;
                }
                response(outcome.code, outcome.body)
            }
            Err(error) => error.response(),
        }
    }
}

fn copy_optional_nested(
    from: &Value,
    to: &mut Value,
    parent: &str,
    field: &str,
) -> anyhow::Result<()> {
    if let Some(value) = from.get(parent).and_then(|value| value.get(field)) {
        anyhow::ensure!(value.is_string(), "infrastructure field must be a string");
        to.as_object_mut()
            .unwrap()
            .entry(parent)
            .or_insert_with(|| json!({}))[field] = value.clone();
    }
    Ok(())
}

#[derive(Debug)]
struct ApiError(u16, &'static str, String);

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self(400, "BadRequest", message.into())
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self(409, "Conflict", message.into())
    }

    fn body(&self) -> Value {
        json!({
            "apiVersion": "v1", "kind": "Status", "status": "Failure",
            "message": self.2, "reason": self.1, "code": self.0
        })
    }

    fn response(self) -> Response<Body> {
        response(self.0, self.body())
    }
}

struct Outcome {
    code: u16,
    body: Value,
    barrier: Option<ArmedBarrier>,
    persisted: Option<Value>,
}

impl Outcome {
    fn new(code: u16, body: Value) -> Self {
        Self {
            code,
            body,
            barrier: None,
            persisted: None,
        }
    }

    fn gated(mut self, barrier: Option<ArmedBarrier>, persisted: Value) -> Self {
        self.barrier = barrier;
        self.persisted = Some(persisted);
        self
    }
}

fn response(code: u16, body: Value) -> Response<Body> {
    Response::builder()
        .status(StatusCode::from_u16(code).unwrap())
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

struct Route {
    namespace: String,
    kind: Kind,
    name: Option<String>,
    status: bool,
}

fn route(store: &mut Store, path: &str) -> Result<Route, ApiError> {
    let segments = path.split('/').collect::<Vec<_>>();
    let (namespace, resource, rest, custom) = match segments.as_slice() {
        ["", "api", "v1", "namespaces", ns, resource, rest @ ..] => (*ns, *resource, rest, false),
        [
            "",
            "apis",
            "operator.kuberic.io",
            "v1alpha1",
            "namespaces",
            ns,
            resource,
            rest @ ..,
        ] => (*ns, *resource, rest, true),
        _ => return Err(store.unsupported(format!("route {path}"))),
    };
    let kind = match (custom, resource) {
        (true, "kubericsets") => Kind::KubericSet,
        (false, "pods") => Kind::Pod,
        (false, "persistentvolumeclaims") => Kind::Pvc,
        (false, "services") => Kind::Service,
        (false, "secrets") => Kind::Secret,
        _ => return Err(store.unsupported(format!("resource {path}"))),
    };
    let (name, status) = match rest {
        [] => (None, false),
        [name] if valid_path_segment(name) => (Some((*name).to_owned()), false),
        [name, "status"] if valid_path_segment(name) && kind != Kind::Secret => {
            (Some((*name).to_owned()), true)
        }
        _ => return Err(store.unsupported(format!("subresource {path}"))),
    };
    if !valid_path_segment(namespace) {
        return Err(store.unsupported(format!("namespace {namespace}")));
    }
    Ok(Route {
        namespace: namespace.to_owned(),
        kind,
        name,
        status,
    })
}

fn valid_path_segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
}

fn selectors(
    store: &mut Store,
    parts: &http::request::Parts,
    route: &Route,
) -> Result<Vec<(String, String)>, ApiError> {
    let mut labels = Vec::new();
    let mut seen = false;
    for (key, value) in form_urlencoded::parse(parts.uri.query().unwrap_or("").as_bytes()) {
        if key != "labelSelector" || parts.method != Method::GET || route.name.is_some() || seen {
            return Err(store.unsupported(format!("query parameter {key}={value}")));
        }
        seen = true;
        if value.is_empty() {
            continue;
        }
        for selector in value.split(',') {
            let Some((key, value)) = selector.split_once('=') else {
                return Err(store.unsupported(format!("label selector {selector}")));
            };
            let value = value.strip_prefix('=').unwrap_or(value);
            let valid = |s: &str| {
                s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            };
            let valid_key = !key.is_empty()
                && key.split('/').count() <= 2
                && key.split('/').all(|part| !part.is_empty() && valid(part));
            if !valid_key || !valid(value) {
                return Err(store.unsupported(format!("label selector {selector}")));
            }
            labels.push((key.to_owned(), value.to_owned()));
        }
    }
    Ok(labels)
}

fn execute(
    store: &mut Store,
    parts: &http::request::Parts,
    bytes: &[u8],
) -> Result<Outcome, ApiError> {
    let route = route(store, parts.uri.path())?;
    let labels = selectors(store, parts, &route)?;
    let supported = matches!(
        (parts.method.as_str(), route.name.is_some(), route.status),
        ("GET", _, false)
            | ("POST", false, false)
            | ("PUT", true, _)
            | ("PATCH", true, false)
            | ("DELETE", true, false)
    );
    if !supported {
        return Err(store.unsupported(format!("{} {}", parts.method, parts.uri)));
    }
    if parts.method == Method::GET && !bytes.is_empty() {
        return Err(store.unsupported("GET request body"));
    }
    if parts.method == Method::GET && route.name.is_none() {
        let items: Vec<_> = store
            .objects
            .iter()
            .filter(|((ns, kind, _), object)| {
                ns == &route.namespace
                    && kind == &route.kind
                    && labels.iter().all(|(key, value)| {
                        object["metadata"]["labels"]
                            .get(key)
                            .and_then(Value::as_str)
                            == Some(value)
                    })
            })
            .map(|(_, value)| value.clone())
            .collect();
        return Ok(Outcome::new(
            200,
            json!({
                "apiVersion": route.kind.api_version(), "kind": format!("{}List", route.kind.kind()),
                "metadata": {"resourceVersion": store.revision.to_string()}, "items": items
            }),
        ));
    }
    if parts.method == Method::POST {
        let mut object = parse_json(bytes)?;
        let name = object["metadata"]["name"]
            .as_str()
            .filter(|name| valid_path_segment(name))
            .ok_or_else(|| ApiError::bad_request("metadata.name is required"))?
            .to_owned();
        validate_object(&object, &route, &name)?;
        reject_finalizers(store, &object)?;
        let key = (route.namespace.clone(), route.kind, name);
        if store.objects.contains_key(&key) {
            return Err(ApiError(
                409,
                "AlreadyExists",
                "object already exists".into(),
            ));
        }
        for field in ["uid", "resourceVersion", "deletionTimestamp"] {
            if object["metadata"]
                .get(field)
                .is_some_and(|value| !value.is_null())
            {
                return Err(ApiError::bad_request(format!(
                    "create supplied server-owned {field}"
                )));
            }
        }
        object["apiVersion"] = json!(route.kind.api_version());
        object["kind"] = json!(route.kind.kind());
        object["metadata"]["namespace"] = json!(route.namespace);
        object["metadata"]["uid"] = json!(uuid::Uuid::new_v4().to_string());
        object["metadata"]["creationTimestamp"] =
            json!(k8s_openapi::jiff::Timestamp::now().to_string());
        if route.kind == Kind::KubericSet {
            object["metadata"]["generation"] = json!(1);
        }
        object.as_object_mut().unwrap().remove("status");
        normalize_secret(route.kind, &mut object)?;
        object["metadata"]["resourceVersion"] = json!(store.rv());
        store.objects.insert(key, object.clone());
        return Ok(Outcome::new(201, object));
    }
    let name = route.name.as_ref().unwrap();
    let key = (route.namespace.clone(), route.kind, name.clone());
    let old = store.objects.get(&key).cloned().ok_or_else(|| {
        ApiError(
            404,
            "NotFound",
            format!("{} {name} not found", route.kind.resource()),
        )
    })?;
    match parts.method.as_str() {
        "GET" => Ok(Outcome::new(200, old)),
        "PUT" => {
            let incoming = parse_json(bytes)?;
            validate_object(&incoming, &route, name)?;
            if incoming["metadata"]["resourceVersion"] != old["metadata"]["resourceVersion"] {
                return Err(ApiError::conflict("resourceVersion changed"));
            }
            if incoming["metadata"]
                .get("uid")
                .is_some_and(|uid| !uid.is_null() && *uid != old["metadata"]["uid"])
            {
                return Err(ApiError::conflict("UID changed"));
            }
            if route.status {
                let completion = completion_count(&incoming) > completion_count(&old);
                if route.kind == Kind::KubericSet
                    && completion
                    && barrier_matches(&store.completion_conflict, &key, &old)
                {
                    let mut bumped = old;
                    bumped["metadata"]["resourceVersion"] = json!(store.rv());
                    store.objects.insert(key, bumped.clone());
                    let conflict = ApiError::conflict("injected concurrent metadata update");
                    return Ok(Outcome::new(409, conflict.body())
                        .gated(store.completion_conflict.take(), bumped));
                }
                let mut next = old.clone();
                copy_field(&incoming, &mut next, "status");
                if route.kind == Kind::KubericSet {
                    prune_kuberic_status(&mut next);
                }
                next["metadata"]["resourceVersion"] = json!(store.rv());
                let newly_pending = !next["status"]["publicV1Operation"].is_null()
                    && next["status"]["publicV1Operation"] != old["status"]["publicV1Operation"];
                let barrier = if route.kind == Kind::KubericSet
                    && newly_pending
                    && barrier_matches(&store.operation_barrier, &key, &next)
                {
                    store.operation_barrier.take()
                } else {
                    None
                };
                store.objects.insert(key, next.clone());
                return Ok(Outcome::new(200, next.clone()).gated(barrier, next));
            }
            reject_finalizers(store, &incoming)?;
            let mut next = incoming;
            copy_field(&old, &mut next, "status");
            preserve_metadata(&old, &mut next);
            normalize_secret(route.kind, &mut next)?;
            generation(route.kind, &old, &mut next);
            next["metadata"]["resourceVersion"] = json!(store.rv());
            store.objects.insert(key, next.clone());
            Ok(Outcome::new(200, next))
        }
        "PATCH" => {
            if parts
                .headers
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                != Some("application/json-patch+json")
            {
                return Err(store.unsupported("only JSON patch is supported"));
            }
            let patch = parse_json(bytes)?;
            let mut next = apply_patch(store, route.kind, &old, &patch)?;
            generation(route.kind, &old, &mut next);
            next["metadata"]["resourceVersion"] = json!(store.rv());
            store.objects.insert(key, next.clone());
            Ok(Outcome::new(200, next))
        }
        "DELETE" => {
            let options = if bytes.is_empty() {
                json!({})
            } else {
                parse_json(bytes)?
            };
            let fields = options
                .as_object()
                .ok_or_else(|| ApiError::bad_request("invalid DeleteOptions"))?;
            for (field, value) in fields {
                match field.as_str() {
                    "apiVersion" if value == "v1" => {}
                    "kind" if value == "DeleteOptions" => {}
                    "preconditions" => {}
                    "gracePeriodSeconds" if value == 0 => {}
                    _ => return Err(store.unsupported(format!("delete option {field}"))),
                }
            }
            if let Some(preconditions) = fields.get("preconditions") {
                let preconditions = preconditions
                    .as_object()
                    .ok_or_else(|| ApiError::bad_request("invalid preconditions"))?;
                for (field, value) in preconditions {
                    if !matches!(field.as_str(), "uid" | "resourceVersion") {
                        return Err(store.unsupported(format!("delete precondition {field}")));
                    }
                    if !value.is_string() {
                        return Err(ApiError::bad_request("precondition must be a string"));
                    }
                    if *value != old["metadata"][field] {
                        return Err(ApiError::conflict(format!("delete {field} changed")));
                    }
                }
            }
            if is_deleting(&old) {
                return Ok(Outcome::new(200, old));
            }
            let mut next = old;
            next["metadata"]["deletionTimestamp"] =
                json!(k8s_openapi::jiff::Timestamp::now().to_string());
            next["metadata"]["deletionGracePeriodSeconds"] = json!(0);
            next["metadata"]["resourceVersion"] = json!(store.rv());
            store.objects.insert(key, next.clone());
            Ok(Outcome::new(200, next))
        }
        _ => unreachable!(),
    }
}

fn parse_json(bytes: &[u8]) -> Result<Value, ApiError> {
    serde_json::from_slice(bytes).map_err(|error| ApiError::bad_request(error.to_string()))
}

fn validate_object(object: &Value, route: &Route, name: &str) -> Result<(), ApiError> {
    if !object["metadata"].is_object() || object["metadata"]["name"] != name {
        return Err(ApiError::bad_request("metadata.name differs from request"));
    }
    for (actual, expected) in [
        (&object["metadata"]["namespace"], route.namespace.as_str()),
        (&object["apiVersion"], route.kind.api_version()),
        (&object["kind"], route.kind.kind()),
    ] {
        if !actual.is_null() && actual.as_str() != Some(expected) {
            return Err(ApiError::bad_request(
                "object namespace or type differs from request",
            ));
        }
    }
    Ok(())
}

fn reject_finalizers(store: &mut Store, object: &Value) -> Result<(), ApiError> {
    if object["metadata"]
        .get("finalizers")
        .is_some_and(|v| !v.is_null() && v != &json!([]))
    {
        return Err(store.unsupported("finalizers"));
    }
    Ok(())
}

fn normalize_secret(kind: Kind, object: &mut Value) -> Result<(), ApiError> {
    if kind != Kind::Secret {
        return Ok(());
    }
    let mut secret: Secret = serde_json::from_value(object.clone())
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    if let Some(strings) = secret.string_data.take() {
        let data = secret.data.get_or_insert_default();
        for (key, value) in strings {
            data.insert(key, k8s_openapi::ByteString(value.into_bytes()));
        }
    }
    *object = serde_json::to_value(secret).unwrap();
    Ok(())
}

fn copy_field(from: &Value, to: &mut Value, field: &str) {
    match from.get(field) {
        Some(value) => {
            to[field] = value.clone();
        }
        None => {
            to.as_object_mut().unwrap().remove(field);
        }
    }
}

fn preserve_metadata(old: &Value, next: &mut Value) {
    for field in [
        "name",
        "namespace",
        "uid",
        "creationTimestamp",
        "generation",
        "deletionTimestamp",
        "deletionGracePeriodSeconds",
    ] {
        copy_field(&old["metadata"], &mut next["metadata"], field);
    }
    for field in ["apiVersion", "kind"] {
        copy_field(old, next, field);
    }
}

fn generation(kind: Kind, old: &Value, next: &mut Value) {
    if kind == Kind::KubericSet && next["spec"] != old["spec"] {
        next["metadata"]["generation"] = json!(old["metadata"]["generation"].as_u64().unwrap() + 1);
    }
}

fn is_deleting(object: &Value) -> bool {
    !object["metadata"]["deletionTimestamp"].is_null()
}

fn barrier_matches(barrier: &Option<ArmedBarrier>, key: &Key, object: &Value) -> bool {
    barrier.as_ref().is_some_and(|barrier| {
        &barrier.key == key
            && object["metadata"]["uid"] == barrier.uid
            && barrier.call_kind.is_none_or(|kind| {
                object["status"]["publicV1Transition"]["callKind"].as_str() == Some(kind)
            })
            && barrier
                .operation_fragment
                .as_deref()
                .is_none_or(|fragment| {
                    object["status"]["publicV1Operation"]["operationId"]
                        .as_str()
                        .is_some_and(|id| id.contains(fragment))
                })
            && barrier
                .matcher
                .as_ref()
                .is_none_or(|matcher| matcher.matches(object))
    })
}

fn completion_count(object: &Value) -> u64 {
    object["status"]["publicV1Authority"]["completedOperationCount"]
        .as_u64()
        .unwrap_or(0)
}

fn apply_patch(
    store: &mut Store,
    kind: Kind,
    old: &Value,
    patch: &Value,
) -> Result<Value, ApiError> {
    let operations = patch
        .as_array()
        .ok_or_else(|| ApiError::bad_request("JSON patch must be an array"))?;
    // Validate the complete program before applying any operation. Unknown features
    // must latch even if an earlier precondition would otherwise short-circuit.
    for operation in operations {
        let fields = operation
            .as_object()
            .ok_or_else(|| ApiError::bad_request("invalid JSON patch operation"))?;
        if fields.len() != 3 || !fields.contains_key("value") {
            return Err(store.unsupported("JSON patch requires exactly op, path, value"));
        }
        match (operation["op"].as_str(), operation["path"].as_str()) {
            (Some("test"), Some("/metadata/uid" | "/metadata/resourceVersion")) => {}
            (Some("add"), Some("/metadata/labels"))
                if kind == Kind::Pod && operation["value"].is_object() => {}
            (Some("add"), Some("/spec/selector"))
                if kind == Kind::Service && operation["value"].is_object() => {}
            _ => return Err(store.unsupported(format!("JSON patch operation {operation}"))),
        }
    }
    let mut next = old.clone();
    for operation in operations {
        let path = operation["path"].as_str().unwrap();
        if operation["op"] == "test" {
            if next.pointer(path) != Some(&operation["value"]) {
                return Err(ApiError::conflict(format!(
                    "JSON patch test failed: {path}"
                )));
            }
        } else {
            let (parent, field) = path.rsplit_once('/').unwrap();
            let parent = next
                .pointer_mut(parent)
                .and_then(Value::as_object_mut)
                .ok_or_else(|| ApiError(422, "Invalid", "JSON patch parent absent".into()))?;
            parent.insert(field.to_owned(), operation["value"].clone());
        }
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::KubericSet;
    use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Service};
    use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams, Preconditions};
    use kube::{Api, ResourceExt};
    use tokio::time::{Duration, timeout};

    fn pod(name: &str) -> Pod {
        serde_json::from_value(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name}, "spec": {"containers": []}
        }))
        .unwrap()
    }

    async fn set(server: &ApiServer) -> Value {
        let sets: Api<KubericSet> = Api::default_namespaced(server.client());
        let object = serde_json::from_value(json!({
            "apiVersion": "operator.kuberic.io/v1alpha1", "kind": "KubericSet",
            "metadata": {"name": "demo"}, "spec": {"hostingFormat": kuberic_runtime::protocol::public_operations::ORDINARY_ACTION_FORMAT, "replicas": 3, "image": "kvstore2"}
        }))
        .unwrap();
        serde_json::to_value(sets.create(&PostParams::default(), &object).await.unwrap()).unwrap()
    }

    async fn raw(
        client: &Client,
        method: &str,
        path: &str,
        value: Value,
    ) -> Result<Value, kube::Error> {
        let bytes = if value.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&value).unwrap()
        };
        client
            .request(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(
                        "content-type",
                        if method == "PATCH" {
                            "application/json-patch+json"
                        } else {
                            "application/json"
                        },
                    )
                    .body(bytes)
                    .unwrap(),
            )
            .await
    }

    const SET: &str = "/apis/operator.kuberic.io/v1alpha1/namespaces/test/kubericsets/demo";
    const STATUS: &str =
        "/apis/operator.kuberic.io/v1alpha1/namespaces/test/kubericsets/demo/status";
    const POD: &str = "/api/v1/namespaces/test/pods/pod";

    fn error_code<T: std::fmt::Debug>(result: Result<T, kube::Error>, code: u16) {
        match result.unwrap_err() {
            kube::Error::Api(status) => {
                assert_eq!(status.code, code);
                assert_eq!(
                    serde_json::to_value(status.status).unwrap(),
                    json!("Failure")
                );
            }
            error => panic!("expected Kubernetes API error {code}, got {error}"),
        }
    }

    #[tokio::test]
    async fn name_reuse_has_fresh_uid_and_stale_writers_conflict() {
        let server = ApiServer::new("test");
        let pods: Api<Pod> = Api::default_namespaced(server.client());
        let first = pods
            .create(&PostParams::default(), &pod("pod"))
            .await
            .unwrap();
        let other_pods: Api<Pod> = Api::default_namespaced(ApiServer::new("test").client());
        let isolated = other_pods
            .create(&PostParams::default(), &pod("pod"))
            .await
            .unwrap();
        assert_ne!(first.uid(), isolated.uid());
        error_code(pods.create(&PostParams::default(), &pod("pod")).await, 409);
        assert!(
            server
                .finalize_deletion(Kind::Pod, "pod", first.uid().as_deref().unwrap())
                .is_err()
        );
        pods.delete("pod", &DeleteParams::default()).await.unwrap();
        server
            .finalize_deletion(Kind::Pod, "pod", first.uid().as_deref().unwrap())
            .unwrap();
        let second = pods
            .create(&PostParams::default(), &pod("pod"))
            .await
            .unwrap();
        assert_ne!(first.uid(), second.uid());
        assert_ne!(first.resource_version(), second.resource_version());
        error_code(
            pods.replace("pod", &PostParams::default(), &first).await,
            409,
        );
        pods.delete("pod", &DeleteParams::default()).await.unwrap();
        assert!(
            server
                .finalize_deletion(Kind::Pod, "pod", first.uid().as_deref().unwrap())
                .is_err()
        );
        assert_eq!(pods.get("pod").await.unwrap().uid(), second.uid());
        server.check().unwrap();
    }

    #[tokio::test]
    async fn status_is_isolated_and_only_spec_advances_generation() {
        let server = ApiServer::new("test");
        let original = set(&server).await;
        let client = server.client();
        assert_eq!(original["metadata"]["generation"], 1);
        let mut proposed = original.clone();
        proposed["spec"]["replicas"] = json!(99);
        proposed["metadata"]["generation"] = json!(90);
        proposed["metadata"]["labels"] = json!({"ignored": "true"});
        proposed["status"] = json!({});
        let accepted = raw(&client, "PUT", STATUS, proposed).await.unwrap();
        assert_eq!(accepted["spec"], original["spec"]);
        assert!(accepted["metadata"]["labels"].is_null());
        assert_eq!(accepted["metadata"]["generation"], 1);
        assert_ne!(
            accepted["metadata"]["resourceVersion"],
            original["metadata"]["resourceVersion"]
        );
        error_code(raw(&client, "PUT", STATUS, original).await, 409);
        let mut metadata_update = accepted.clone();
        metadata_update["metadata"]["labels"] = json!({"desired": "true"});
        metadata_update["status"] = json!({"publicV1Operation": "must not be accepted"});
        let updated = raw(&client, "PUT", SET, metadata_update).await.unwrap();
        assert_eq!(updated["status"], accepted["status"]);
        assert_eq!(updated["metadata"]["generation"], 1);
        let mut spec_update = updated.clone();
        spec_update["spec"]["replicas"] = json!(5);
        spec_update["metadata"]
            .as_object_mut()
            .unwrap()
            .remove("uid");
        spec_update["metadata"]["creationTimestamp"] = json!("2099-01-01T00:00:00Z");
        let updated = raw(&client, "PUT", SET, spec_update).await.unwrap();
        assert_eq!(updated["spec"]["replicas"], 5);
        assert_eq!(updated["metadata"]["generation"], 2);
        assert_eq!(updated["metadata"]["uid"], accepted["metadata"]["uid"]);
        assert_eq!(
            updated["metadata"]["creationTimestamp"],
            accepted["metadata"]["creationTimestamp"]
        );
        let mut uid_change = updated.clone();
        uid_change["metadata"]["uid"] = json!("another-uid");
        error_code(raw(&client, "PUT", STATUS, uid_change).await, 409);
        server.check().unwrap();
    }

    #[tokio::test]
    async fn namespaces_and_equality_label_selectors_are_respected() {
        let server = ApiServer::new("test");
        let pods: Api<Pod> = Api::default_namespaced(server.client());
        let other: Api<Pod> = Api::namespaced(server.client(), "other");
        for (api, name, labels) in [
            (
                &pods,
                "matching",
                json!({"set": "x", "replica": "1", "empty": ""}),
            ),
            (&pods, "different", json!({"set": "x", "replica": "2"})),
            (&other, "matching", json!({"set": "x", "replica": "1"})),
        ] {
            let mut object = pod(name);
            object.metadata.labels = serde_json::from_value(labels).unwrap();
            api.create(&PostParams::default(), &object).await.unwrap();
        }
        let listed = pods
            .list(&ListParams::default().labels("set=x,replica==1"))
            .await
            .unwrap();
        assert_eq!(listed.items.len(), 1);
        assert_eq!(listed.items[0].name_any(), "matching");
        assert!(listed.metadata.resource_version.is_some());
        assert_eq!(
            pods.list(&ListParams::default().labels("empty="))
                .await
                .unwrap()
                .items
                .len(),
            1
        );
        assert_eq!(
            pods.list(&ListParams::default()).await.unwrap().items.len(),
            2
        );
        assert_eq!(
            other
                .list(&ListParams::default())
                .await
                .unwrap()
                .items
                .len(),
            1
        );
        assert_eq!(server.snapshot(Kind::Pod).len(), 2);
        error_code(other.get("different").await, 404);
        let mut wrong_namespace = pod("wrong");
        wrong_namespace.metadata.namespace = Some("other".into());
        error_code(
            pods.create(&PostParams::default(), &wrong_namespace).await,
            400,
        );
        server.check().unwrap();
        error_code(
            pods.list(&ListParams::default().labels("set!=x")).await,
            501,
        );
        assert!(server.check().is_err());
    }

    #[tokio::test]
    async fn json_patch_tests_are_atomic_and_service_selectors_work() {
        let server = ApiServer::new("test");
        let client = server.client();
        let pods: Api<Pod> = Api::default_namespaced(client.clone());
        let created = pods
            .create(&PostParams::default(), &pod("pod"))
            .await
            .unwrap();
        let before = server.get(Kind::Pod, "pod").unwrap();
        error_code(
            raw(
                &client,
                "PATCH",
                POD,
                json!([
                    {"op": "add", "path": "/metadata/labels", "value": {"instance": "wrong"}},
                    {"op": "test", "path": "/metadata/resourceVersion", "value": "stale"}
                ]),
            )
            .await,
            409,
        );
        assert_eq!(server.get(Kind::Pod, "pod").unwrap(), before);
        let patch = json!([
            {"op": "test", "path": "/metadata/uid", "value": created.uid()},
            {"op": "test", "path": "/metadata/resourceVersion", "value": created.resource_version()},
            {"op": "add", "path": "/metadata/labels", "value": {"instance": "exact"}}
        ]);
        let result = pods
            .patch(
                "pod",
                &PatchParams::default(),
                &Patch::<Value>::Json(serde_json::from_value(patch).unwrap()),
            )
            .await
            .unwrap();
        assert_eq!(result.labels()["instance"], "exact");
        assert_ne!(result.resource_version(), created.resource_version());
        let services: Api<Service> = Api::default_namespaced(client.clone());
        let service: Service = serde_json::from_value(json!({
            "metadata": {"name": "write"}, "spec": {"selector": {"instance": "disabled"}}
        }))
        .unwrap();
        let service = services
            .create(&PostParams::default(), &service)
            .await
            .unwrap();
        let patched = raw(&client, "PATCH", "/api/v1/namespaces/test/services/write", json!([
            {"op": "test", "path": "/metadata/uid", "value": service.uid()},
            {"op": "test", "path": "/metadata/resourceVersion", "value": service.resource_version()},
            {"op": "add", "path": "/spec/selector", "value": {"instance": "exact"}}
        ])).await.unwrap();
        assert_eq!(patched["spec"]["selector"]["instance"], "exact");
        server.check().unwrap();
        let before = server.get(Kind::Pod, "pod").unwrap();
        error_code(
            raw(
                &client,
                "PATCH",
                POD,
                json!([
                    {"op": "add", "path": "/metadata/labels", "value": {"instance": "bad"}},
                    {"op": "remove", "path": "/metadata/uid"}
                ]),
            )
            .await,
            501,
        );
        assert_eq!(server.get(Kind::Pod, "pod").unwrap(), before);
        assert!(server.check().is_err());
    }

    #[tokio::test]
    async fn deletion_is_delayed_uid_and_rv_guarded_and_does_not_cascade() {
        let server = ApiServer::new("test");
        let pods: Api<Pod> = Api::default_namespaced(server.client());
        let pvcs: Api<PersistentVolumeClaim> = Api::default_namespaced(server.client());
        let created = pods
            .create(&PostParams::default(), &pod("pod"))
            .await
            .unwrap();
        let pvc: PersistentVolumeClaim = serde_json::from_value(json!({
            "metadata": {"name": "data"}, "spec": {"accessModes": ["ReadWriteOnce"]}
        }))
        .unwrap();
        pvcs.create(&PostParams::default(), &pvc).await.unwrap();
        for (uid, rv) in [
            (Some("wrong".into()), created.resource_version()),
            (created.uid(), Some("wrong".into())),
        ] {
            error_code(
                pods.delete(
                    "pod",
                    &DeleteParams {
                        preconditions: Some(Preconditions {
                            uid,
                            resource_version: rv,
                        }),
                        ..Default::default()
                    },
                )
                .await,
                409,
            );
        }
        assert!(server.pending_deletions(Kind::Pod).is_empty());
        pods.delete(
            "pod",
            &DeleteParams {
                preconditions: Some(Preconditions {
                    uid: created.uid(),
                    resource_version: created.resource_version(),
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let deleting = pods.get("pod").await.unwrap();
        assert!(deleting.metadata.deletion_timestamp.is_some());
        assert_ne!(deleting.resource_version(), created.resource_version());
        assert_eq!(server.pending_deletions(Kind::Pod).len(), 1);
        assert_eq!(server.pending_deletions(Kind::Pvc).len(), 0);
        pods.delete("pod", &DeleteParams::default()).await.unwrap();
        assert_eq!(
            pods.get("pod").await.unwrap().resource_version(),
            deleting.resource_version()
        );
        error_code(pods.create(&PostParams::default(), &pod("pod")).await, 409);
        server
            .finalize_deletion(Kind::Pod, "pod", created.uid().as_deref().unwrap())
            .unwrap();
        assert!(
            !server
                .finalize_deletion(Kind::Pod, "pod", created.uid().as_deref().unwrap())
                .unwrap()
        );
        error_code(pods.get("pod").await, 404);
        assert!(pvcs.get("data").await.is_ok());
        server.check().unwrap();
    }

    #[tokio::test]
    async fn unsupported_operations_and_failed_lists_latch_fatal_errors() {
        let server = ApiServer::new("test");
        let pods: Api<Pod> = Api::default_namespaced(server.client());
        error_code(
            pods.list(&ListParams::default().fields("metadata.name=pod"))
                .await,
            501,
        );
        let first_error = server.sticky_error().unwrap();
        let client = server.client();
        error_code(
            raw(
                &client,
                "GET",
                "/api/v1/namespaces/test/configmaps",
                Value::Null,
            )
            .await,
            501,
        );
        error_code(
            raw(
                &client,
                "GET",
                "/api/v1/namespaces/test/pods?watch=true",
                Value::Null,
            )
            .await,
            501,
        );
        error_code(
            raw(&client, "DELETE", "/api/v1/namespaces/test/pods", json!({})).await,
            501,
        );
        assert_eq!(server.sticky_error().unwrap(), first_error);
        // Successful later requests must never erase the failure.
        assert!(
            pods.list(&ListParams::default())
                .await
                .unwrap()
                .items
                .is_empty()
        );
        assert!(server.check().is_err());
    }

    #[tokio::test]
    async fn infrastructure_updates_are_exact_and_cannot_publish_authority() {
        let server = ApiServer::new("test");
        let pods: Api<Pod> = Api::default_namespaced(server.client());
        let created = pods
            .create(&PostParams::default(), &pod("pod"))
            .await
            .unwrap();
        let uid = created.uid().unwrap();
        let rv = created.resource_version().unwrap();
        let changed = server.changed();
        let notification = changed.notified();
        tokio::pin!(notification);
        notification.as_mut().enable();
        let updated = server
            .update_infrastructure(Kind::Pod, "pod", &uid, &rv, |value| {
                value["status"] = json!({"podIP": "127.0.0.1", "phase": "Running"});
                value["metadata"]["annotations"] =
                    json!({"operator.kuberic.io/control-address": "http://127.0.0.1:12345"});
            })
            .unwrap();
        timeout(Duration::from_secs(2), notification).await.unwrap();
        assert_ne!(updated["metadata"]["resourceVersion"], rv);
        assert!(
            server
                .update_infrastructure(Kind::Pod, "pod", &uid, &rv, |_| {})
                .is_err()
        );
        let rv = updated["metadata"]["resourceVersion"].as_str().unwrap();
        assert!(
            server
                .update_infrastructure(Kind::Pod, "pod", "stale-uid", rv, |_| {})
                .is_err()
        );
        for path in ["/spec", "/metadata/uid", "/status/role"] {
            assert!(
                server
                    .update_infrastructure(Kind::Pod, "pod", &uid, rv, |value| {
                        if path == "/status/role" {
                            value["status"]["role"] = json!("Primary");
                        } else {
                            *value.pointer_mut(path).unwrap() = json!("forbidden");
                        }
                    })
                    .is_err()
            );
        }
        assert_eq!(server.get(Kind::Pod, "pod").unwrap(), updated);
        server.check().unwrap();
    }

    #[tokio::test]
    async fn secrets_round_trip_string_data_as_real_kubernetes_bytes() {
        let server = ApiServer::new("test");
        let secrets: Api<Secret> = Api::default_namespaced(server.client());
        let desired: Secret = serde_json::from_value(json!({
            "metadata": {"name": "credentials"}, "stringData": {"bearer-token": "test-token"}, "type": "Opaque"
        })).unwrap();
        let created = secrets
            .create(&PostParams::default(), &desired)
            .await
            .unwrap();
        assert_eq!(
            created.data.as_ref().unwrap()["bearer-token"].0,
            b"test-token"
        );
        assert!(created.string_data.is_none());
        let mut replacement = desired;
        replacement.metadata.resource_version = created.resource_version();
        let updated = secrets
            .replace("credentials", &PostParams::default(), &replacement)
            .await
            .unwrap();
        assert_eq!(created.uid(), updated.uid());
        assert_ne!(created.resource_version(), updated.resource_version());
        assert_eq!(created.data, updated.data);
        assert_eq!(
            secrets
                .list(&ListParams::default())
                .await
                .unwrap()
                .items
                .len(),
            1
        );
        server.check().unwrap();
    }

    #[tokio::test]
    async fn operation_barrier_commits_before_reply_and_survives_cancellation() {
        let server = ApiServer::new("test");
        let mut initial = set(&server).await;
        let mut barrier = server.arm_operation_barrier("demo").unwrap();
        assert!(server.arm_operation_barrier("demo").is_err());
        initial["status"] = json!({"publicV1Operation": {
            "target": {"replicaId": 1, "instanceId": "pod-uid", "agentGeneration": "generation"},
            "operationId": "operation", "revision": 1, "canonicalCommandJson": "{}"
        }});
        let client = server.client();
        let write = tokio::spawn(async move { raw(&client, "PUT", STATUS, initial).await });
        let persisted = timeout(Duration::from_secs(2), barrier.entered())
            .await
            .unwrap()
            .unwrap();
        assert!(!write.is_finished());
        // Reading through the client proves the store lock is not held at the gate.
        let observed = timeout(
            Duration::from_secs(2),
            raw(&server.client(), "GET", SET, Value::Null),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(observed, persisted);
        write.abort();
        assert!(write.await.unwrap_err().is_cancelled());
        barrier.release();
        assert_eq!(server.get(Kind::KubericSet, "demo").unwrap(), persisted);
        let mut next = persisted.clone();
        next["status"]["publicV1Operation"]["revision"] = json!(2);
        // One-shot: a subsequent operation does not block.
        timeout(
            Duration::from_secs(2),
            raw(&server.client(), "PUT", STATUS, next),
        )
        .await
        .unwrap()
        .unwrap();
        server.check().unwrap();
    }

    #[tokio::test]
    async fn completion_conflict_bumps_only_metadata_and_is_one_shot() {
        let server = ApiServer::new("test");
        let mut before = set(&server).await;
        before["status"] = json!({"publicV1Authority": {"completedOperationCount": 0}});
        let before = raw(&server.client(), "PUT", STATUS, before).await.unwrap();
        let mut barrier = server.arm_completion_conflict("demo").unwrap();
        let mut proposed = before.clone();
        proposed["status"]["publicV1Authority"]["completedOperationCount"] = json!(1);
        let client = server.client();
        let submitted = proposed.clone();
        let write = tokio::spawn(async move { raw(&client, "PUT", STATUS, submitted).await });
        let bumped = timeout(Duration::from_secs(2), barrier.entered())
            .await
            .unwrap()
            .unwrap();
        assert!(!write.is_finished());
        let mut expected = before.clone();
        expected["metadata"]["resourceVersion"] = bumped["metadata"]["resourceVersion"].clone();
        assert_eq!(bumped, expected);
        assert_ne!(
            bumped["metadata"]["resourceVersion"],
            before["metadata"]["resourceVersion"]
        );
        assert_eq!(server.get(Kind::KubericSet, "demo").unwrap(), bumped);
        barrier.release();
        error_code(write.await.unwrap(), 409);
        // The conflict is real, not merely a synthetic error response.
        error_code(
            raw(&server.client(), "PUT", STATUS, proposed.clone()).await,
            409,
        );
        proposed["metadata"]["resourceVersion"] = bumped["metadata"]["resourceVersion"].clone();
        let completed = timeout(
            Duration::from_secs(2),
            raw(&server.client(), "PUT", STATUS, proposed),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(completion_count(&completed), 1);
        assert_eq!(
            completed["metadata"]["generation"],
            before["metadata"]["generation"]
        );
        server.check().unwrap();
    }
}
