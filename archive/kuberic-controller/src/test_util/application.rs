use super::{ErrorKind, Result, TestError};
use kuberic_runtime::{
    StatefulServiceReplica, host::ApplicationStorageState, protocol::types::ReplicaIdentity,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Constructs an inert application. Open and access are driven only by the host.
#[async_trait::async_trait]
pub trait ApplicationFactory: Send + Sync + 'static {
    type Service: StatefulServiceReplica + 'static;
    async fn create(&self, context: ReplicaContext) -> Result<CreatedApplication<Self::Service>>;
    /// Terminal and repeatable; join all workers retained after host Close/Abort.
    async fn shutdown_and_drain(&self, service: Arc<Self::Service>) -> Result<()>;
}

/// Immutable resolved application input; never includes agent credentials.
#[derive(Clone)]
pub struct ReplicaContext {
    pub(super) identity: ReplicaIdentity,
    pub(super) root: PathBuf,
    pub(super) environment: BTreeMap<String, String>,
}
impl ReplicaContext {
    pub fn identity(&self) -> &ReplicaIdentity {
        &self.identity
    }
    pub fn application_root(&self) -> &Path {
        &self.root
    }
    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }
    pub fn replication_address(&self) -> &str {
        "http://127.0.0.1:0"
    }
}

/// A service and its actual provider state, with optional owned storage bindings.
pub struct CreatedApplication<S> {
    pub(super) service: Arc<S>,
    pub(super) storage: ApplicationStorageState,
    pub(super) paths: Vec<PathBuf>,
}
impl<S> CreatedApplication<S> {
    pub fn new(service: Arc<S>, storage: ApplicationStorageState) -> Self {
        Self {
            service,
            storage,
            paths: Vec::new(),
        }
    }
    pub fn with_storage_paths(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.paths = paths.into_iter().collect();
        self
    }
    pub(super) fn validate(&self, root: &Path) -> Result<()> {
        let canonical_root = root.canonicalize()?;
        for path in &self.paths {
            let mut ancestor = path.as_path();
            while !ancestor.exists() {
                ancestor = ancestor.parent().ok_or_else(|| {
                    TestError::new(
                        ErrorKind::InvalidConfiguration,
                        "invalid application storage binding",
                    )
                })?;
            }
            if path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
                || !(path.starts_with(root) || path.starts_with(&canonical_root))
                || !ancestor.canonicalize()?.starts_with(&canonical_root)
            {
                return Err(TestError::new(
                    ErrorKind::InvalidConfiguration,
                    "application storage binding escapes owned directory",
                ));
            }
        }
        Ok(())
    }
}
