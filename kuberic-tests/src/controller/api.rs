use std::{
    collections::BTreeMap,
    convert::Infallible,
    sync::{Arc, Mutex},
};

use http::{Method, Request, Response};
use kube::{client::Body, Client};
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Set,
    Pod,
    ConfigMap,
    Service,
}

impl Kind {
    fn kind(self) -> &'static str {
        match self {
            Self::Set => "KubericSet",
            Self::Pod => "Pod",
            Self::ConfigMap => "ConfigMap",
            Self::Service => "Service",
        }
    }
    fn version(self) -> &'static str {
        if self == Self::Set {
            "operator.kuberic.io/v1alpha2"
        } else {
            "v1"
        }
    }
}

type Key = (String, Kind, String);

#[derive(Default)]
struct Store {
    objects: BTreeMap<Key, Value>,
    revision: u64,
    unsupported: Option<String>,
    conflict: Option<(Kind, bool)>,
    lose_response: Option<(Kind, bool)>,
    delete_conflict: Option<Kind>,
    lose_delete_response: Option<Kind>,
    deletes: Vec<(Kind, String, String, Option<i64>)>,
}

impl Store {
    fn revision(&mut self) -> Value {
        self.revision += 1;
        json!(self.revision.to_string())
    }
    fn unsupported(&mut self, reason: impl Into<String>) -> Failure {
        let reason = reason.into();
        self.unsupported.get_or_insert_with(|| reason.clone());
        Failure(501, "NotImplemented", reason)
    }
}

#[derive(Clone, Default)]
pub struct MockApi {
    store: Arc<Mutex<Store>>,
}

impl MockApi {
    pub fn client(&self, namespace: &str) -> Client {
        let server = self.clone();
        Client::new(
            tower::service_fn(move |request: Request<Body>| {
                let server = server.clone();
                async move { Ok::<_, Infallible>(server.handle(request).await) }
            }),
            namespace,
        )
    }

    pub fn conflict_next(&self, kind: Kind, status: bool) {
        self.store.lock().unwrap().conflict = Some((kind, status));
    }

    pub fn lose_next_response(&self, kind: Kind, status: bool) {
        self.store.lock().unwrap().lose_response = Some((kind, status));
    }

    pub fn conflict_next_delete(&self, kind: Kind) {
        self.store.lock().unwrap().delete_conflict = Some(kind);
    }

    pub fn lose_next_delete_response(&self, kind: Kind) {
        self.store.lock().unwrap().lose_delete_response = Some(kind);
    }

    pub fn deletes(&self) -> Vec<(Kind, String, String, Option<i64>)> {
        self.store.lock().unwrap().deletes.clone()
    }

    pub fn finalize(&self, namespace: &str, kind: Kind, name: &str, uid: &str) {
        let mut store = self.store.lock().unwrap();
        let key = (namespace.to_owned(), kind, name.to_owned());
        let object = store.objects.get(&key).expect("deleting object exists");
        assert_eq!(
            object["metadata"]["uid"], uid,
            "cannot finalize another incarnation"
        );
        assert!(
            !object["metadata"]["deletionTimestamp"].is_null(),
            "deletion was not requested"
        );
        assert!(
            object["metadata"]["finalizers"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "foreign finalizer remains"
        );
        store.objects.remove(&key);
        store.revision();
    }

    pub fn check(&self) {
        let error = self.store.lock().unwrap().unsupported.clone();
        assert!(error.is_none(), "unsupported mock API operation: {error:?}");
    }

    async fn handle(&self, request: Request<Body>) -> Response<Body> {
        let (parts, body) = request.into_parts();
        let body = body.collect_bytes().await.expect("test request body");
        let result = execute(&mut self.store.lock().unwrap(), &parts, &body);
        match result {
            Ok((code, body)) => response(code, body),
            Err(error) => response(
                error.0,
                json!({
                    "apiVersion": "v1", "kind": "Status", "status": "Failure",
                    "code": error.0, "reason": error.1, "message": error.2
                }),
            ),
        }
    }
}

struct Failure(u16, &'static str, String);
fn conflict(message: &str) -> Failure {
    Failure(409, "Conflict", message.into())
}
fn bad(message: &str) -> Failure {
    Failure(400, "BadRequest", message.into())
}
fn response(code: u16, value: Value) -> Response<Body> {
    Response::builder()
        .status(code)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&value).unwrap()))
        .unwrap()
}

struct Route {
    namespace: String,
    kind: Kind,
    name: Option<String>,
    status: bool,
}

fn route(store: &mut Store, path: &str) -> Result<Route, Failure> {
    let segments = path.split('/').collect::<Vec<_>>();
    let (namespace, resource, rest, custom) = match segments.as_slice() {
        ["", "api", "v1", "namespaces", namespace, resource, rest @ ..] => {
            (*namespace, *resource, rest, false)
        }
        ["", "apis", "operator.kuberic.io", "v1alpha2", "namespaces", namespace, resource, rest @ ..] => {
            (*namespace, *resource, rest, true)
        }
        _ => return Err(store.unsupported(format!("route {path}"))),
    };
    let kind = match (custom, resource) {
        (true, "kubericsets") => Kind::Set,
        (false, "pods") => Kind::Pod,
        (false, "configmaps") => Kind::ConfigMap,
        (false, "services") => Kind::Service,
        _ => return Err(store.unsupported(format!("resource {resource}"))),
    };
    let (name, status) = match rest {
        [] => (None, false),
        [name] => (Some((*name).to_owned()), false),
        [name, "status"] if matches!(kind, Kind::Set | Kind::Pod) => {
            (Some((*name).to_owned()), true)
        }
        _ => return Err(store.unsupported(format!("subresource {path}"))),
    };
    Ok(Route {
        namespace: namespace.into(),
        kind,
        name,
        status,
    })
}

fn validate_object(object: &Value, route: &Route, name: &str) -> Result<(), Failure> {
    if object["apiVersion"] != route.kind.version()
        || object["kind"] != route.kind.kind()
        || object["metadata"]["name"] != name
        || object["metadata"]
            .get("namespace")
            .is_some_and(|namespace| !namespace.is_null() && namespace != &route.namespace)
    {
        return Err(bad(
            "object identity does not match the namespaced API route",
        ));
    }
    Ok(())
}

fn execute(
    store: &mut Store,
    parts: &http::request::Parts,
    bytes: &[u8],
) -> Result<(u16, Value), Failure> {
    let route = route(store, parts.uri.path())?;
    let mut labels = Vec::new();
    for (name, value) in form_urlencoded::parse(parts.uri.query().unwrap_or("").as_bytes()) {
        if name != "labelSelector" || parts.method != Method::GET || route.name.is_some() {
            return Err(store.unsupported(format!("query {name}")));
        }
        for pair in value.split(',').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| store.unsupported("non-equality selector"))?;
            labels.push((key.to_owned(), value.to_owned()));
        }
    }
    if parts.method == Method::GET && route.name.is_none() {
        let items: Vec<_> = store
            .objects
            .iter()
            .filter(|((namespace, kind, _), object)| {
                namespace == &route.namespace
                    && kind == &route.kind
                    && labels.iter().all(|(key, value)| {
                        object["metadata"]["labels"][key].as_str() == Some(value)
                    })
            })
            .map(|(_, object)| object.clone())
            .collect();
        return Ok((
            200,
            json!({"apiVersion": route.kind.version(), "kind": format!("{}List", route.kind.kind()), "metadata": {"resourceVersion": store.revision.to_string()}, "items": items}),
        ));
    }
    if parts.method == Method::POST && route.name.is_none() {
        let mut object: Value = serde_json::from_slice(bytes).map_err(|_| bad("invalid JSON"))?;
        let name = object["metadata"]["name"]
            .as_str()
            .ok_or_else(|| bad("missing name"))?
            .to_owned();
        validate_object(&object, &route, &name)?;
        let key = (route.namespace.clone(), route.kind, name);
        if store.objects.contains_key(&key) {
            return Err(Failure(409, "AlreadyExists", "object exists".into()));
        }
        if !object["metadata"]["uid"].is_null() || !object["metadata"]["resourceVersion"].is_null()
        {
            return Err(bad("create supplied server-owned identity"));
        }
        object["apiVersion"] = json!(route.kind.version());
        object["kind"] = json!(route.kind.kind());
        object["metadata"]["namespace"] = json!(route.namespace);
        object["metadata"]["uid"] = json!(uuid::Uuid::new_v4().to_string());
        object["metadata"]["resourceVersion"] = store.revision();
        if route.kind == Kind::Set {
            object["metadata"]["generation"] = json!(1);
        }
        object
            .as_object_mut()
            .ok_or_else(|| bad("object is not a map"))?
            .remove("status");
        store.objects.insert(key, object.clone());
        return Ok((201, object));
    }
    let name = route
        .name
        .as_ref()
        .ok_or_else(|| store.unsupported(format!("method {}", parts.method)))?;
    let key = (route.namespace.clone(), route.kind, name.clone());
    let old = store
        .objects
        .get(&key)
        .cloned()
        .ok_or_else(|| Failure(404, "NotFound", "object absent".into()))?;
    if parts.method == Method::GET && !route.status {
        return Ok((200, old));
    }
    if parts.method == Method::DELETE && !route.status {
        let options: Value =
            serde_json::from_slice(bytes).map_err(|_| bad("invalid DeleteOptions"))?;
        if options["preconditions"]["uid"] != old["metadata"]["uid"] {
            return Err(conflict("delete requires the exact UID"));
        }
        if let Some(version) = options["preconditions"].get("resourceVersion") {
            if version != &old["metadata"]["resourceVersion"] {
                return Err(conflict("delete resourceVersion changed"));
            }
        }
        if store.delete_conflict == Some(route.kind) {
            store.delete_conflict = None;
            let mut bumped = old;
            bumped["metadata"]["resourceVersion"] = store.revision();
            store.objects.insert(key, bumped);
            return Err(conflict("injected concurrent deletion update"));
        }
        if !old["metadata"]["deletionTimestamp"].is_null() {
            return Ok((200, old));
        }
        let grace = options["gracePeriodSeconds"].as_i64();
        let finalizers = old["metadata"]["finalizers"]
            .as_array()
            .is_some_and(|finalizers| !finalizers.is_empty());
        let held = finalizers || (route.kind == Kind::Pod && grace != Some(0));
        let mut deleted = old.clone();
        deleted["metadata"]["deletionTimestamp"] =
            serde_json::to_value(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::now(),
            ))
            .unwrap();
        if route.kind == Kind::Pod {
            deleted["metadata"]["deletionGracePeriodSeconds"] = json!(grace.unwrap_or(30));
        }
        if route.kind == Kind::Set {
            deleted["metadata"]["generation"] =
                json!(old["metadata"]["generation"].as_i64().unwrap() + 1);
        }
        deleted["metadata"]["resourceVersion"] = store.revision();
        store.deletes.push((
            route.kind,
            name.clone(),
            old["metadata"]["uid"].as_str().unwrap().into(),
            grace,
        ));
        if held {
            store.objects.insert(key, deleted.clone());
        } else {
            store.objects.remove(&key);
        }
        if store.lose_delete_response == Some(route.kind) {
            store.lose_delete_response = None;
            return Err(Failure(
                500,
                "InternalError",
                "injected lost deletion response after commit".into(),
            ));
        }
        return Ok((200, deleted));
    }
    if parts.method != Method::PUT {
        return Err(store.unsupported(format!("method {} {}", parts.method, parts.uri)));
    }
    let incoming: Value = serde_json::from_slice(bytes).map_err(|_| bad("invalid JSON"))?;
    validate_object(&incoming, &route, name)?;
    if incoming["metadata"]["uid"] != old["metadata"]["uid"]
        || incoming["metadata"]["resourceVersion"] != old["metadata"]["resourceVersion"]
    {
        return Err(conflict("UID/resourceVersion changed"));
    }
    if !route.status && !old["metadata"]["deletionTimestamp"].is_null() {
        if incoming["metadata"]["deletionTimestamp"] != old["metadata"]["deletionTimestamp"] {
            return Err(bad("deletionTimestamp is immutable"));
        }
        if incoming["metadata"]["finalizers"]
            .as_array()
            .is_some_and(|finalizers| {
                finalizers.iter().any(|finalizer| {
                    !old["metadata"]["finalizers"]
                        .as_array()
                        .is_some_and(|old| old.contains(finalizer))
                })
            })
        {
            return Err(bad("new finalizers cannot be added during deletion"));
        }
    }
    if store.conflict == Some((route.kind, route.status)) {
        store.conflict = None;
        let mut bumped = old;
        bumped["metadata"]["resourceVersion"] = store.revision();
        store.objects.insert(key, bumped);
        return Err(conflict("injected concurrent update"));
    }
    if route.kind == Kind::ConfigMap
        && old["immutable"] == true
        && (incoming["data"] != old["data"]
            || incoming["binaryData"] != old["binaryData"]
            || incoming["immutable"] != true)
    {
        return Err(Failure(
            422,
            "Invalid",
            "immutable ConfigMap data changed".into(),
        ));
    }
    let mut next = if route.status {
        let mut next = old.clone();
        next["status"] = incoming["status"].clone();
        next
    } else {
        let mut next = incoming;
        if let Some(status) = old.get("status") {
            next["status"] = status.clone();
        }
        if route.kind == Kind::Set {
            next["metadata"]["generation"] = if next["spec"] != old["spec"] {
                json!(old["metadata"]["generation"].as_i64().unwrap() + 1)
            } else {
                old["metadata"]["generation"].clone()
            };
        }
        next
    };
    next["metadata"]["resourceVersion"] = store.revision();
    if !route.status
        && route.kind != Kind::Pod
        && !next["metadata"]["deletionTimestamp"].is_null()
        && next["metadata"]["finalizers"]
            .as_array()
            .is_none_or(Vec::is_empty)
    {
        store.objects.remove(&key);
    } else {
        store.objects.insert(key, next.clone());
    }
    if store.lose_response == Some((route.kind, route.status)) {
        store.lose_response = None;
        return Err(Failure(
            500,
            "InternalError",
            "injected lost response after commit".into(),
        ));
    }
    Ok((200, next))
}
