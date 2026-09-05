use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use k8s_openapi::{api::core::v1::ConfigMap, apimachinery::pkg::apis::meta::v1::ObjectMeta};
use kube::{
    Api, Client, Error,
    api::{PostParams, ResourceExt},
};

use crate::{
    CasOutcome, CheckpointEnvelope, CheckpointStore, ExecutionId, StorageRevision, StoreError,
    StoreErrorKind, StoredCheckpoint,
};

const CHECKPOINT_DATA_KEY: &str = "checkpoint.json";
const CHECKPOINT_NAME_PREFIX: &str = "kuberic-checkpoint-";
const CONFIG_MAP_DATA_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Default)]
struct Metrics {
    accepted_writes: AtomicU64,
    checkpoint_bytes: AtomicU64,
    object_bytes: AtomicU64,
    measurement_failures: AtomicU64,
}

/// Clone-shared measurements recorded by a Kubernetes checkpoint store.
#[derive(Clone, Debug, Default)]
pub struct KubernetesCheckpointMetrics {
    inner: Arc<Metrics>,
}

impl KubernetesCheckpointMetrics {
    /// Return a point-in-time snapshot of cumulative provider measurements.
    pub fn snapshot(&self) -> KubernetesCheckpointMetricsSnapshot {
        KubernetesCheckpointMetricsSnapshot {
            accepted_writes: self.inner.accepted_writes.load(Ordering::Relaxed),
            checkpoint_bytes: self.inner.checkpoint_bytes.load(Ordering::Relaxed),
            object_bytes: self.inner.object_bytes.load(Ordering::Relaxed),
            measurement_failures: self.inner.measurement_failures.load(Ordering::Relaxed),
        }
    }

    fn record_accepted(&self, checkpoint_bytes: usize, object: &ConfigMap) {
        self.inner.accepted_writes.fetch_add(1, Ordering::Relaxed);
        self.inner
            .checkpoint_bytes
            .fetch_add(checkpoint_bytes as u64, Ordering::Relaxed);
        match serde_json::to_vec(object) {
            Ok(encoded) => {
                self.inner
                    .object_bytes
                    .fetch_add(encoded.len() as u64, Ordering::Relaxed);
            }
            Err(_) => {
                self.inner
                    .measurement_failures
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Cumulative measurements for writes whose acceptance the provider proved.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KubernetesCheckpointMetricsSnapshot {
    accepted_writes: u64,
    checkpoint_bytes: u64,
    object_bytes: u64,
    measurement_failures: u64,
}

impl KubernetesCheckpointMetricsSnapshot {
    pub const fn accepted_writes(self) -> u64 {
        self.accepted_writes
    }

    pub const fn checkpoint_bytes(self) -> u64 {
        self.checkpoint_bytes
    }

    pub const fn object_bytes(self) -> u64 {
        self.object_bytes
    }

    pub const fn measurement_failures(self) -> u64 {
        self.measurement_failures
    }
}

/// A namespaced ConfigMap-backed implementation of the portable checkpoint
/// store contract.
#[derive(Clone, Debug)]
pub struct KubernetesCheckpointStore {
    api: Api<ConfigMap>,
    namespace: String,
    metrics: KubernetesCheckpointMetrics,
}

impl KubernetesCheckpointStore {
    /// Construct a store from a caller-owned Kubernetes client and namespace.
    pub fn new(client: Client, namespace: impl Into<String>) -> Result<Self, StoreError> {
        let namespace = namespace.into();
        validate_namespace(&namespace)?;
        Ok(Self {
            api: Api::namespaced(client, &namespace),
            namespace,
            metrics: KubernetesCheckpointMetrics::default(),
        })
    }

    /// Return the clone-shared provider measurements.
    pub fn metrics(&self) -> KubernetesCheckpointMetrics {
        self.metrics.clone()
    }

    /// Return the deterministic ConfigMap name for an execution.
    pub fn object_name(execution_id: ExecutionId) -> String {
        let mut name = String::with_capacity(CHECKPOINT_NAME_PREFIX.len() + 32);
        name.push_str(CHECKPOINT_NAME_PREFIX);
        for byte in execution_id.as_bytes() {
            use std::fmt::Write as _;
            write!(&mut name, "{byte:02x}").expect("writing to a String cannot fail");
        }
        name
    }

    fn object(
        &self,
        execution_id: ExecutionId,
        expected: Option<&StorageRevision>,
        checkpoint_json: String,
    ) -> ConfigMap {
        let mut labels = BTreeMap::new();
        labels.insert(
            "kuberic.io/component".to_string(),
            "durable-checkpoint".to_string(),
        );
        let mut data = BTreeMap::new();
        data.insert(CHECKPOINT_DATA_KEY.to_string(), checkpoint_json);
        ConfigMap {
            metadata: ObjectMeta {
                name: Some(Self::object_name(execution_id)),
                namespace: Some(self.namespace.clone()),
                resource_version: expected.map(|revision| revision.as_str().to_string()),
                labels: Some(labels),
                ..ObjectMeta::default()
            },
            data: Some(data),
            ..ConfigMap::default()
        }
    }
}

#[async_trait(?Send)]
impl CheckpointStore for KubernetesCheckpointStore {
    async fn load(
        &self,
        execution_id: ExecutionId,
    ) -> Result<Option<StoredCheckpoint>, StoreError> {
        let name = Self::object_name(execution_id);
        let object = self
            .api
            .get_opt(&name)
            .await
            .map_err(|error| load_error("load", error))?;
        object.map(decode_object).transpose()
    }

    async fn compare_and_swap(
        &self,
        execution_id: ExecutionId,
        expected: Option<StorageRevision>,
        checkpoint: CheckpointEnvelope,
    ) -> Result<CasOutcome, StoreError> {
        let checkpoint_json = serde_json::to_string(&checkpoint).map_err(|error| {
            StoreError::new(
                StoreErrorKind::MalformedResponse,
                format!("compare-and-swap request serialization failed: {error}"),
            )
        })?;
        if checkpoint_json.len() > CONFIG_MAP_DATA_LIMIT {
            return Err(StoreError::new(
                StoreErrorKind::Other,
                format!(
                    "compare-and-swap checkpoint data is {} bytes, exceeding the ConfigMap data limit of {CONFIG_MAP_DATA_LIMIT} bytes",
                    checkpoint_json.len()
                ),
            ));
        }

        let checkpoint_bytes = checkpoint_json.len();
        let object = self.object(execution_id, expected.as_ref(), checkpoint_json);
        let name = object.name_any();
        let result = if expected.is_some() {
            self.api
                .replace(&name, &PostParams::default(), &object)
                .await
        } else {
            self.api.create(&PostParams::default(), &object).await
        };

        match result {
            Ok(object) => {
                let Some(revision) = response_revision(&object) else {
                    return Ok(CasOutcome::OutcomeUnknown);
                };
                self.metrics.record_accepted(checkpoint_bytes, &object);
                Ok(CasOutcome::Accepted(revision))
            }
            Err(error) => classify_mutation_error(expected.is_some(), error),
        }
    }
}

fn validate_namespace(namespace: &str) -> Result<(), StoreError> {
    let valid_length = !namespace.is_empty() && namespace.len() <= 63;
    let valid_bytes = namespace.bytes().enumerate().all(|(index, byte)| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || (byte == b'-' && index > 0 && index + 1 < namespace.len())
    });
    if valid_length && valid_bytes {
        Ok(())
    } else {
        Err(StoreError::new(
            StoreErrorKind::Other,
            "Kubernetes checkpoint namespace must be a nonempty DNS label of at most 63 characters",
        ))
    }
}

fn decode_object(object: ConfigMap) -> Result<StoredCheckpoint, StoreError> {
    let revision = response_revision(&object).ok_or_else(|| {
        StoreError::new(
            StoreErrorKind::MalformedResponse,
            "load response omitted a nonempty metadata.resourceVersion",
        )
    })?;
    let checkpoint_json = object
        .data
        .as_ref()
        .and_then(|data| data.get(CHECKPOINT_DATA_KEY))
        .ok_or_else(|| {
            StoreError::new(
                StoreErrorKind::MalformedResponse,
                "load response omitted data[checkpoint.json]",
            )
        })?;
    let checkpoint = serde_json::from_str(checkpoint_json).map_err(|error| {
        StoreError::new(
            StoreErrorKind::MalformedResponse,
            format!("load response contained invalid checkpoint JSON: {error}"),
        )
    })?;
    Ok(StoredCheckpoint::new(revision, checkpoint))
}

fn response_revision(object: &ConfigMap) -> Option<StorageRevision> {
    object
        .metadata
        .resource_version
        .as_ref()
        .and_then(|revision| StorageRevision::new(revision.clone()).ok())
}

fn classify_mutation_error(replacing: bool, error: Error) -> Result<CasOutcome, StoreError> {
    match error {
        Error::Api(status)
            if status.is_conflict() || (!replacing && status.is_already_exists()) =>
        {
            Ok(CasOutcome::Conflict)
        }
        Error::Api(status) if replacing && status.is_not_found() => Ok(CasOutcome::Conflict),
        Error::Api(status) if status.code >= 500 || status.code == 408 => {
            Ok(CasOutcome::OutcomeUnknown)
        }
        Error::Api(status) => Err(api_status_error("compare-and-swap", &status)),
        Error::Auth(error) => Err(StoreError::new(
            StoreErrorKind::Authorization,
            format!("compare-and-swap authentication failed before dispatch: {error}"),
        )),
        Error::BuildRequest(error) => Err(StoreError::new(
            StoreErrorKind::Other,
            format!("compare-and-swap request construction failed before dispatch: {error}"),
        )),
        Error::HttpError(error) => Err(StoreError::new(
            StoreErrorKind::Other,
            format!("compare-and-swap HTTP request construction failed before dispatch: {error}"),
        )),
        _ => Ok(CasOutcome::OutcomeUnknown),
    }
}

fn load_error(operation: &str, error: Error) -> StoreError {
    match error {
        Error::Api(status) => api_status_error(operation, &status),
        Error::Auth(error) => StoreError::new(
            StoreErrorKind::Authorization,
            format!("{operation} authentication failed: {error}"),
        ),
        Error::SerdeError(error) => StoreError::new(
            StoreErrorKind::MalformedResponse,
            format!("{operation} response decoding failed: {error}"),
        ),
        Error::FromUtf8(error) => StoreError::new(
            StoreErrorKind::MalformedResponse,
            format!("{operation} response was not UTF-8: {error}"),
        ),
        Error::BuildRequest(error) => StoreError::new(
            StoreErrorKind::Other,
            format!("{operation} request construction failed: {error}"),
        ),
        other => StoreError::new(
            StoreErrorKind::Unavailable,
            format!("{operation} Kubernetes request failed: {other}"),
        ),
    }
}

fn api_status_error(operation: &str, status: &kube::error::Status) -> StoreError {
    let kind = match status.code {
        401 | 403 => StoreErrorKind::Authorization,
        408 | 504 => StoreErrorKind::Timeout,
        500..=599 | 429 => StoreErrorKind::Unavailable,
        _ => StoreErrorKind::Other,
    };
    StoreError::new(
        kind,
        format!(
            "{operation} Kubernetes API rejected the request: code={}, reason={}",
            status.code, status.reason
        ),
    )
}
