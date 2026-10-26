use super::{ErrorKind, Result, TestError};
use kuberic_runtime::{
    StatefulServiceReplica, host::ApplicationStorageState, protocol::types::ReplicaIdentity,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

#[path = "process_backend.rs"]
mod process_backend;
pub(super) use process_backend::ProcessMember;

/// Constructs an inert application. Open and access are driven only by the host.
#[async_trait::async_trait]
pub trait ApplicationFactory: Send + Sync + 'static {
    type Service: StatefulServiceReplica + 'static;
    async fn create(&self, context: ReplicaContext) -> Result<CreatedApplication<Self::Service>>;
    /// Terminal and repeatable; join all workers retained after host Close/Abort.
    async fn shutdown_and_drain(&self, service: Arc<Self::Service>) -> Result<()>;

    /// Opt in to a self-contained test executable. Never builds a missing binary.
    /// The executable must call `ReplicaContext::member_main` and must not spawn
    /// external engines: those require independent descendant exclusion.
    fn member_executable(&self) -> Option<PathBuf> {
        None
    }

    /// Application data only; called on the actual child service, never a proxy.
    async fn member_data(
        &self,
        _service: Arc<Self::Service>,
        _request: serde_json::Value,
    ) -> Result<serde_json::Value> {
        Err(TestError::new(
            ErrorKind::UnsupportedCapability,
            "no member data adapter",
        ))
    }

    /// Test scheduling only, after the data reply was actually written. Useful
    /// when releasing an executable-exit gate must not race its own reply.
    fn member_data_reply_sent(&self, _service: &Self::Service, _request: &serde_json::Value) {}

    /// Read-only diagnostics. These cannot supply native/election authority.
    fn member_observation(&self, _service: &Self::Service) -> Result<serde_json::Value> {
        Err(TestError::new(
            ErrorKind::UnsupportedCapability,
            "no member observation adapter",
        ))
    }

    /// Called only after the exact owned OS child has exited and been reaped.
    /// Implementations must independently validate a durable, owner-bound runtime
    /// recovery cause. An exit code, failed Close, or task result alone is not one.
    /// Default factories never automatically replace a failed process.
    fn recoverable_member_exit(
        &self,
        _context: &ReplicaContext,
        _pid: u32,
        _status: std::process::ExitStatus,
    ) -> Result<bool> {
        Ok(false)
    }

    /// Test-only scheduling hook after the runtime's actual durable exit notice.
    /// Returning never certifies drainage or death; the executable and its exact
    /// supervisor Child still own those boundaries.
    async fn before_member_recovery_exit(
        &self,
        _service: Arc<Self::Service>,
        _notice: kuberic_runtime::protocol::public_operations::RetainedRecoveryExitNotice,
    ) -> Result<()> {
        Ok(())
    }
}

/// Immutable resolved application input; never includes agent credentials.
#[derive(Clone)]
pub struct ReplicaContext {
    pub(super) identity: ReplicaIdentity,
    pub(super) root: PathBuf,
    pub(super) environment: BTreeMap<String, String>,
    pub(super) member: Option<process_backend::MemberClient>,
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

    /// A proxy context never owns an application/provider in the parent.
    pub fn is_process_proxy(&self) -> bool {
        self.member.is_some()
    }

    /// Exact supervisor generation, not a native process-session certificate.
    pub fn member_generation(&self) -> Option<&str> {
        self.member
            .as_ref()
            .map(|client| client.generation.as_str())
    }

    pub async fn member_data(&self, request: serde_json::Value) -> Result<serde_json::Value> {
        let client = self.member.as_ref().ok_or_else(|| {
            TestError::new(ErrorKind::InvalidConfiguration, "not a process proxy")
        })?;
        client.data(request).await.map_err(Into::into)
    }

    /// Receive a generation-checked scheduling reply which itself releases an
    /// executable-exit gate. A later supervisor reap must not erase that already
    /// written reply. Ordinary application requests use `member_data` instead.
    pub async fn member_exit_gate_reply(
        &self,
        request: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let client = self.member.as_ref().ok_or_else(|| {
            TestError::new(ErrorKind::InvalidConfiguration, "not a process proxy")
        })?;
        client.exit_gate_reply(request).await.map_err(Into::into)
    }

    /// Synchronous, read-only, generation-qualified observation of a live child.
    /// A stale handle fails rather than reading a successor's retained files.
    pub fn member_observation(&self) -> Result<serde_json::Value> {
        let client = self.member.as_ref().ok_or_else(|| {
            TestError::new(ErrorKind::InvalidConfiguration, "not a process proxy")
        })?;
        client.observation().map_err(Into::into)
    }

    /// Validates a durable cause against this generation's actual observed
    /// runtime identity. The supervisor separately owns and reaps the OS child.
    pub fn validate_member_recovery_exit_cause(&self, reaped_pid: u32) -> Result<()> {
        self.member
            .as_ref()
            .ok_or_else(|| TestError::new(ErrorKind::InvalidConfiguration, "not a process proxy"))?
            .validate_recovery_cause(self, reaped_pid)
            .map_err(Into::into)
    }

    /// Entry point for an explicit test-only helper binary, before starting Tokio.
    /// Parent death is armed before any host/provider is created.
    pub fn member_main<F: ApplicationFactory>(factory: F) -> Result<()> {
        process_backend::member_main(factory).map_err(Into::into)
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
    /// Preserve the actual provider classification and bindings in a test adapter.
    pub fn map_service<T>(self, map: impl FnOnce(Arc<S>) -> Arc<T>) -> CreatedApplication<T> {
        CreatedApplication {
            service: map(self.service),
            storage: self.storage,
            paths: self.paths,
        }
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
