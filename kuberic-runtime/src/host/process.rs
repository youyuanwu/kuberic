//! Reusable replica-process hosting for stateful applications.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::StatefulServiceReplica;
use crate::protocol::public_operations::PublicV1LifecycleBinding;
use crate::protocol::public_operations::{PublicV1Identity, StatePersistence};
use crate::protocol::types::{PodUid, PvcUid, ReplicaId, ReplicaInstanceId, ResourceUid};
use serde::Serialize;
use tokio::sync::watch;

use crate::host::Result;
use crate::host::hosting::PodRuntime;
use crate::host::provisioning::ObservedStorageIdentity;
use crate::host::service::{AgentService, InitializationService, SHUTDOWN_TIMEOUT};
use crate::host::sqlite_store::SqliteStore;
use crate::host::store::AgentStore;
use crate::host::transport::ReplicaEndpointResolver;

#[cfg(kuberic_workspace_tests)]
pub(crate) const PUBLIC_V1_PROCESS_STARTUP_OWNERS: &[&str] = &["serve_control"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationStorageState {
    FreshEmpty,
    Established,
}

#[derive(Debug, Clone)]
pub struct ReplicaProcessConfig {
    pub resource_uid: ResourceUid,
    pub replica_id: ReplicaId,
    pub pod_uid: PodUid,
    pub pvc_uid: PvcUid,
    pub data_root: PathBuf,
    pub replication_address: SocketAddr,
    pub bearer_token: String,
    pub rpc_deadline: Duration,
    pub transport_window_capacity: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaDiagnostics {
    pub replica_id: i64,
    pub instance_id: String,
    pub agent_generation: String,
    pub process_session: String,
    pub role: String,
    pub epoch: String,
    pub previous_configuration: Option<String>,
    pub current_configuration: Option<String>,
    pub current_progress: crate::protocol::public_operations::ProgressQueryResult,
    pub verified_replication_lsn: Option<i64>,
    pub catch_up_capability: crate::protocol::public_operations::ProgressQueryResult,
    pub read_status: String,
    pub write_status: String,
    pub catch_up_boundary_lsn: Option<i64>,
    pub scale_up_operation: Option<String>,
    pub retired: bool,
    pub pending_operation: Option<String>,
    pub blocking: Option<String>,
    pub builds: Vec<ReplicaBuildDiagnostics>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaBuildDiagnostics {
    pub build_id: String,
    pub target_instance: String,
    pub replication_boundary_lsn: i64,
    pub durable_lsn: i64,
    pub completed: bool,
    pub catch_up_boundary_lsn: Option<i64>,
}

#[derive(Clone)]
pub struct ReplicaHandle {
    runtime: Arc<PodRuntime>,
    store: Arc<SqliteStore>,
    process_session: Arc<str>,
    replacement: Option<ReplacementProcessHandle>,
}

#[derive(Clone)]
struct ReplacementProcessHandle {
    binding: PublicV1LifecycleBinding,
    registry: Arc<crate::host::operation::PartitionOperationRegistry>,
    report_sequence: Arc<std::sync::atomic::AtomicU64>,
}

impl ReplicaHandle {
    /// Waits for a durable owner-scoped exit instruction. Returning this notice
    /// does not join cleanup, release storage, or assert OS process death.
    pub async fn wait_for_retained_recovery_exit(
        &self,
    ) -> Result<crate::protocol::public_operations::RetainedRecoveryExitNotice> {
        let replacement = self.replacement.as_ref().ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "retained recovery requires ordinary hosting".into(),
            )
        })?;
        let mut revision = replacement.registry.revision_receiver();
        let process = crate::protocol::types::ProcessSessionId::new(self.process_session.as_ref());
        loop {
            let state = self.store.load_state().await?;
            if let Some(notice) = crate::host::ordinary_goal::retained::status(&state, &process)
                && notice.process_id == std::process::id()
                && notice.process_lifetime
                    == crate::host::ordinary_goal::retained::process_lifetime_id()
                && notice.qualification.predecessor_creation_id
                    == self.runtime.replacement_replicator_creation_id()?
                && replacement.registry.is_fenced()
            {
                return Ok(notice);
            }
            revision.changed().await.map_err(|_| {
                crate::host::HostError::CommandRejected(
                    "recovery notice owner was lost; physical exclusion is unknown".into(),
                )
            })?;
        }
    }

    /// Read-only validation of the durable cause, NOT validation of death or
    /// reaping. The supervisor must independently reap its exact owned child.
    pub fn validate_retained_recovery_exit_cause(
        data_root: &Path,
        history: &crate::protocol::types::HistoryContext,
        storage: &crate::protocol::types::ReplicaStorageIncarnationContext,
        process: &crate::protocol::types::ProcessSessionId,
        creation: &crate::protocol::types::ReplicatorCreationId,
        reaped_process_id: u32,
    ) -> Result<crate::protocol::public_operations::RetainedRecoveryExitNotice> {
        let state = SqliteStore::read_retained_recovery_state(
            &SqliteStore::metadata_database_path(data_root),
        )?;
        let notice =
            crate::host::ordinary_goal::retained::status(&state, process).ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "no exact durable retained recovery cause".into(),
                )
            })?;
        if notice.history != *history
            || notice.storage != *storage
            || notice.process_id != reaped_process_id
            || notice.qualification.predecessor_creation_id != *creation
        {
            return Err(crate::host::HostError::IdentityMismatch(
                "reaped member differs from durable recovery cause".into(),
            ));
        }
        Ok(notice)
    }

    pub async fn diagnostics(&self) -> Result<ReplicaDiagnostics> {
        let state = self.store.load_state().await?;
        let blocking = state
            .public_v1_operation_state
            .as_ref()
            .and_then(|preview| {
                let operation_id = preview.current_operation.as_ref()?;
                let record = preview.operations.get(operation_id)?;
                Some(format!(
                    "{:?}/{:?}",
                    record.stage, record.lifecycle.in_flight
                ))
            });
        let (
            _open,
            _replication_address,
            role,
            read_status,
            write_status,
            current_progress,
            catch_up_capability,
        ) = self.runtime.diagnostic_projection().await;
        Ok(ReplicaDiagnostics {
            replica_id: state.identity.local_identity.replica_id.value(),
            instance_id: state.identity.local_identity.instance_id.to_string(),
            agent_generation: state.identity.local_identity.agent_generation.to_string(),
            process_session: self.process_session.to_string(),
            role: format!("{role:?}"),
            epoch: state
                .public_v1_operation_state
                .as_ref()
                .and_then(|preview| preview.history.as_ref())
                .map_or_else(
                    || "0.0".to_string(),
                    |history| {
                        format!(
                            "{}.{}",
                            history.data_loss_number,
                            history.protocol_generation.value()
                        )
                    },
                ),
            previous_configuration: None,
            current_configuration: None,
            current_progress,
            verified_replication_lsn: None,
            catch_up_capability,
            read_status: format!("{read_status:?}"),
            write_status: format!("{write_status:?}"),
            catch_up_boundary_lsn: None,
            scale_up_operation: None,
            retired: state
                .public_v1_operation_state
                .as_ref()
                .is_some_and(|preview| preview.terminal),
            pending_operation: state
                .public_v1_operation_state
                .as_ref()
                .and_then(|preview| preview.current_operation.as_ref().map(ToString::to_string)),
            blocking,
            builds: Vec::new(),
        })
    }

    #[cfg(feature = "testing")]
    pub async fn initialize_public_v1_history(
        &self,
        initialization_id: &crate::protocol::types::HistoryInitializationId,
        history: &crate::protocol::types::HistoryContext,
        storage: &crate::protocol::types::ReplicaStorageIncarnationContext,
    ) -> Result<()> {
        let preview = self.replacement.as_ref().ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "replica was not constructed with Public V1 replacement authority".into(),
            )
        })?;
        if !preview.binding.identity.is_public_v1() {
            return Err(crate::host::HostError::IdentityMismatch(
                "replacement runtime requires Public V1".into(),
            ));
        }
        self.store
            .initialize_public_v1_history(initialization_id, history, storage)
            .await
    }

    #[cfg(feature = "testing")]
    pub fn set_public_operation_cut(&self, cut: Option<Arc<crate::testing::PublicOperationCut>>) {
        self.runtime.set_public_operation_cut(cut);
    }

    #[cfg(feature = "testing")]
    pub async fn execute_public_v1(
        &self,
        intent: crate::protocol::public_operations::PublicOperationIntent,
    ) -> Result<()> {
        let preview = self.replacement.as_ref().ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "Public V1 operation runtime is unavailable".into(),
            )
        })?;
        let handle = self.clone();
        let (completed, result) = tokio::sync::oneshot::channel();
        preview.registry.try_own_control_task(async move {
            let outcome = handle.execute_public_v1_owned(intent).await;
            let _ = completed.send(outcome);
        })?;
        result.await.map_err(|error| {
            crate::host::HostError::CommandRejected(format!(
                "owned public testing execution stopped before replying: {error}"
            ))
        })?
    }

    #[cfg(feature = "testing")]
    async fn execute_public_v1_owned(
        &self,
        intent: crate::protocol::public_operations::PublicOperationIntent,
    ) -> Result<()> {
        use crate::host::state::{PublicOperationDisposition, PublicOperationStage};

        let preview = self.replacement.as_ref().ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "replica was not constructed with Public V1 replacement authority".into(),
            )
        })?;
        if intent.identity != preview.binding.identity
            || intent.process_session_id.as_str() != self.process_session.as_ref()
        {
            return Err(crate::host::HostError::IdentityMismatch(
                "public operation differs from the replacement process".into(),
            ));
        }
        let terminal_program = matches!(
            intent.program.as_ref(),
            Some(
                crate::protocol::public_operations::PublicOperationProgram::Close
                    | crate::protocol::public_operations::PublicOperationProgram::Abort
            )
        );
        let open_program = matches!(
            intent.program.as_ref(),
            Some(crate::protocol::public_operations::PublicOperationProgram::Open { .. })
        );
        let mut callbacks = self.runtime.public_lifecycle_callbacks()?;
        if matches!(
            intent.program,
            Some(crate::protocol::public_operations::PublicOperationProgram::Open { .. })
        ) {
            let state = self.store.load_state().await?;
            let mode = state.application_open_mode();
            callbacks.open_context = Some(self.runtime.public_open_context(mode).await);
        }
        let operation = crate::host::public_lifecycle::launch(
            &preview.registry,
            self.store.clone(),
            intent,
            callbacks,
        )
        .await?;
        let mut access_guard = self.runtime.public_access_completion_guard(&operation);
        let terminal = operation.wait_for_terminal().await?;
        if terminal.stage != PublicOperationStage::Completed
            || terminal.disposition != Some(PublicOperationDisposition::Succeeded)
        {
            return Err(crate::host::HostError::CommandRejected(format!(
                "public operation did not complete successfully: {:?}",
                terminal.disposition
            )));
        }
        self.runtime.complete_public_access(&operation);
        if open_program {
            self.runtime.complete_replacement_open().await;
            let state = self.store.load_state().await?;
            if state
                .application_storage
                .as_ref()
                .is_some_and(|binding| binding.initializing)
            {
                self.store.complete_application_initialization().await?;
            }
        }
        if !terminal_program {
            self.runtime
                .refresh_replacement_projection(
                    self.store.as_ref(),
                    &crate::protocol::types::ProcessSessionId::new(self.process_session.as_ref()),
                    true,
                )
                .await?;
        }
        access_guard.succeeded();
        Ok(())
    }

    #[cfg(feature = "testing")]
    pub async fn execute_public_v1_command(
        &self,
        command: crate::protocol::command::ProtocolCommand,
    ) -> Result<()> {
        let crate::protocol::command::ProtocolCommand::PublicV1Operation(command) = command else {
            return Err(crate::host::HostError::CommandRejected(
                "replacement operation dispatcher received another command".into(),
            ));
        };
        command
            .validate()
            .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
        let state = self.store.load_state().await?;
        if command.target != state.identity.local_identity {
            return Err(crate::host::HostError::IdentityMismatch(
                "public-v1 operation command targets another replica".into(),
            ));
        }
        self.execute_public_v1(command.intent).await
    }

    #[cfg(feature = "testing")]
    pub async fn fence_public_v1_access(&self) {
        self.runtime.fence_replacement_access().await;
    }

    #[cfg(feature = "testing")]
    pub async fn public_v1_process_endpoint(
        &self,
        role: crate::protocol::types::ReplicaRole,
    ) -> Result<crate::protocol::types::ProcessSessionEndpoint> {
        let state = self.store.load_state().await?;
        let preview = state.public_v1_operation_state.ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "replacement process endpoint requires current state".into(),
            )
        })?;
        Ok(crate::protocol::types::ProcessSessionEndpoint {
            identity: state.identity.local_identity,
            process_session_id: self.process_session_id(),
            replicator_creation_id: self.runtime.replacement_replicator_creation_id()?,
            role,
            storage_incarnation: preview.storage_incarnation.ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "replacement process endpoint lacks storage authority".into(),
                )
            })?,
        })
    }

    #[cfg(feature = "testing")]
    pub async fn public_v1_next_operation_revision(&self) -> Result<u64> {
        let preview = self
            .store
            .load_state()
            .await?
            .public_v1_operation_state
            .ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "replacement runtime requires operation state".into(),
                )
            })?;
        preview
            .operations
            .values()
            .map(|record| record.intent.revision)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "replacement operation revision exhausted".into(),
                )
            })
    }

    #[cfg(feature = "testing")]
    pub async fn public_v1_operation_intent(
        &self,
        operation_id: &crate::protocol::types::OperationId,
    ) -> Result<Option<crate::protocol::public_operations::PublicOperationIntent>> {
        let preview = self
            .store
            .load_state()
            .await?
            .public_v1_operation_state
            .ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "replacement runtime requires operation state".into(),
                )
            })?;
        Ok(preview
            .operations
            .get(operation_id)
            .map(|record| record.intent.clone()))
    }

    #[cfg(feature = "testing")]
    pub async fn public_v1_operation_is_contained(
        &self,
        operation_id: &crate::protocol::types::OperationId,
    ) -> Result<bool> {
        Ok(self
            .store
            .load_state()
            .await?
            .public_v1_operation_state
            .and_then(|state| state.operations.get(operation_id).cloned())
            .is_some_and(|record| {
                record.containment == super::state::PublicOperationContainment::Complete
            }))
    }

    #[cfg(feature = "testing")]
    pub async fn authorize_public_v1_session_renewal(
        &self,
        required: crate::protocol::public_operations::ProcessSessionRenewalRequired,
    ) -> Result<crate::protocol::public_operations::PublicV1SessionRenewalStatus> {
        use crate::protocol::public_operations::PublicV1SessionRenewalStatus;

        let mut current = PublicV1SessionRenewalStatus::required(required)
            .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
        self.store
            .persist_public_v1_session_renewal(None, &current)
            .await?;
        for next in [
            current.close_access(),
            current
                .close_access()
                .and_then(|status| status.freeze_surviving_set()),
            current
                .close_access()
                .and_then(|status| status.freeze_surviving_set())
                .and_then(|status| status.begin_epoch_barriers()),
        ] {
            let next =
                next.map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
            self.store
                .persist_public_v1_session_renewal(Some(&current), &next)
                .await?;
            current = next;
        }
        for survivor in current
            .barriers
            .iter()
            .filter(|barrier| {
                barrier.stage
                    == crate::protocol::public_operations::PublicSessionEpochBarrierStage::Pending
            })
            .map(|barrier| barrier.survivor.clone())
            .collect::<Vec<_>>()
        {
            let next = current
                .record_epoch_updated(&survivor, current.required.renewal_epoch)
                .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
            self.store
                .persist_public_v1_session_renewal(Some(&current), &next)
                .await?;
            current = next;
        }
        if current.stage
            == crate::protocol::public_operations::PublicSessionRenewalStage::EpochBarriersPending
        {
            let next = current
                .finish_epoch_barriers()
                .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
            self.store
                .persist_public_v1_session_renewal(Some(&current), &next)
                .await?;
            current = next;
        }
        if let Some(replacement) = current.required.replacement.clone() {
            let next = current
                .replay_replacement_role(
                    &replacement.process_session_id,
                    current.required.renewal_epoch,
                    current.required.predecessor.role,
                )
                .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
            self.store
                .persist_public_v1_session_renewal(Some(&current), &next)
                .await?;
            current = next;
        }
        let next = current
            .install_fresh_configuration(&current.required.fresh_configuration)
            .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
        self.store
            .persist_public_v1_session_renewal(Some(&current), &next)
            .await?;
        Ok(next)
    }

    #[cfg(feature = "testing")]
    pub async fn persist_public_v1_session_renewal(
        &self,
        expected: Option<&crate::protocol::public_operations::PublicV1SessionRenewalStatus>,
        next: &crate::protocol::public_operations::PublicV1SessionRenewalStatus,
    ) -> Result<()> {
        self.store
            .persist_public_v1_session_renewal(expected, next)
            .await
    }

    #[cfg(feature = "testing")]
    pub async fn public_v1_report(
        &self,
    ) -> Result<crate::protocol::public_operations::PublicV1AgentReport> {
        let preview = self.replacement.as_ref().ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "replica was not constructed with Public V1 replacement authority".into(),
            )
        })?;
        let session = crate::protocol::types::ProcessSessionId::new(self.process_session.as_ref());
        self.runtime
            .refresh_replacement_projection(self.store.as_ref(), &session, false)
            .await?;
        let projection = self
            .runtime
            .replacement_report_projection(
                session,
                preview
                    .report_sequence
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            )
            .await?;
        crate::host::public_lifecycle::public_v1_report(
            self.store.as_ref(),
            &preview.binding.identity,
            projection,
        )
        .await
    }

    #[cfg(feature = "testing")]
    pub fn process_session_id(&self) -> crate::protocol::types::ProcessSessionId {
        crate::protocol::types::ProcessSessionId::new(self.process_session.as_ref())
    }

    #[cfg(feature = "testing")]
    pub fn public_v1_provider_writer_count(&self) -> usize {
        self.runtime.provider_writer_lease_count()
    }
}

pub struct RunningReplica {
    handle: ReplicaHandle,
    shutdown: watch::Sender<bool>,
    completion: tokio::task::JoinHandle<Result<()>>,
}

impl RunningReplica {
    pub async fn recovery_exit_required(
        &self,
    ) -> Result<crate::protocol::public_operations::RetainedRecoveryExitNotice> {
        self.handle.wait_for_retained_recovery_exit().await
    }

    pub fn handle(&self) -> ReplicaHandle {
        self.handle.clone()
    }

    pub fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    pub fn is_finished(&self) -> bool {
        self.completion.is_finished()
    }

    pub async fn wait(&mut self) -> Result<()> {
        (&mut self.completion)
            .await
            .map_err(|error| crate::host::HostError::CommandRejected(error.to_string()))?
    }
}

impl Drop for RunningReplica {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
    }
}

pub struct ReplicaHost<A, R> {
    config: ReplicaProcessConfig,
    application: Arc<A>,
    application_storage: ApplicationStorageState,
    application_storage_paths: Option<BTreeMap<String, PathBuf>>,
    _resolver: Arc<R>,
    public_v1_binding: Option<PublicV1LifecycleBinding>,
}

impl<A, R> ReplicaHost<A, R>
where
    A: StatefulServiceReplica + 'static,
    R: ReplicaEndpointResolver + 'static,
{
    pub fn new(
        config: ReplicaProcessConfig,
        application: Arc<A>,
        application_storage: ApplicationStorageState,
        resolver: Arc<R>,
    ) -> Self {
        Self {
            public_v1_binding: Some(PublicV1LifecycleBinding {
                identity: PublicV1Identity::new(1),
                resource_uid: config.resource_uid.clone(),
                spec_generation: 1,
                state_persistence: StatePersistence::Persisted,
            }),
            config,
            application,
            application_storage,
            application_storage_paths: None,
            _resolver: resolver,
        }
    }

    pub fn with_public_v1_binding(mut self, binding: PublicV1LifecycleBinding) -> Self {
        self.public_v1_binding = Some(binding);
        self
    }

    /// Bind application paths at authorized initialization. Only an unfinished
    /// first open receives `OpenMode::New`; missing bindings on existing stores reject.
    pub fn with_application_storage_paths(mut self, paths: BTreeMap<String, PathBuf>) -> Self {
        self.application_storage_paths = Some(paths);
        self
    }

    /// Move a caller-bound control socket into the initialization and agent server task.
    pub async fn start(self, control_listener: tokio::net::TcpListener) -> Result<RunningReplica> {
        let (_shutdown, receiver) = watch::channel(false);
        self.start_with_shutdown(control_listener, receiver)
            .await?
            .ok_or(crate::host::HostError::Runtime(
                crate::RuntimeError::OperationCancelled,
            ))
    }

    /// Cooperatively cancel startup and await its acknowledgement and task cleanup.
    /// `None` means cancellation completed before readiness. A readiness race may
    /// return a replica instead; the caller must shut it down and await `wait`.
    /// Keep this future alive until completion, including after requesting shutdown.
    pub async fn start_with_shutdown(
        self,
        control_listener: tokio::net::TcpListener,
        mut startup_shutdown: watch::Receiver<bool>,
    ) -> Result<Option<RunningReplica>> {
        if *startup_shutdown.borrow() || startup_shutdown.has_changed().is_err() {
            return Ok(None);
        }
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (ready, mut ready_rx) = watch::channel(false);
        let (prepared, prepared_rx) = watch::channel(None);
        let mut control_task =
            tokio::spawn(self.serve_control(control_listener, prepared, ready, shutdown_rx));
        let mut control_finished = false;
        let mut startup_result = tokio::select! {
            biased;
            result = &mut control_task => {
                control_finished = true;
                match result {
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(crate::host::HostError::CommandRejected(error.to_string())),
                    Ok(Ok(())) => Err(crate::host::HostError::CommandRejected(
                        "control server stopped before becoming ready".into(),
                    )),
                }
            }
            result = async { ready_rx.wait_for(|ready| *ready).await.map(|_| ()) } => {
                match result {
                    Ok(_) => Ok(true),
                    Err(_) => {
                        control_finished = true;
                        match (&mut control_task).await {
                            Ok(Err(error)) => Err(error),
                            Err(error) => Err(crate::host::HostError::CommandRejected(error.to_string())),
                            Ok(Ok(())) => Err(crate::host::HostError::CommandRejected(
                                "control readiness channel closed".into(),
                            )),
                        }
                    }
                }
            }
            _ = async { let _ = startup_shutdown.wait_for(|stopped| *stopped).await; } => Ok(false),
        };
        if matches!(startup_result, Ok(true)) && prepared_rx.borrow().is_none() {
            startup_result = Err(crate::host::HostError::CommandRejected(
                "control server became ready without a replica handle".into(),
            ));
        }
        if !matches!(startup_result, Ok(true)) {
            shutdown.send_replace(true);
            let cleanup = if control_finished {
                Ok(())
            } else {
                finish_control(&mut control_task).await
            };
            let runtime = prepared_rx
                .borrow()
                .as_ref()
                .map(|handle| handle.runtime.clone());
            let drain = if let Some(runtime) = runtime {
                runtime.abort();
                drain_runtime(&runtime).await
            } else {
                Ok(())
            };
            let cleanup = with_shutdown_error(cleanup, drain, "replicator drain");
            let cleanup =
                match cleanup {
                    Err(crate::host::HostError::Runtime(
                        crate::RuntimeError::OperationCancelled,
                    )) if matches!(startup_result, Ok(false)) => Ok(()),
                    result => result,
                };
            let settled =
                with_shutdown_error(startup_result.map(|_| ()), cleanup, "control shutdown");
            if settled.is_ok()
                && let Some(handle) = prepared_rx.borrow().as_ref()
            {
                handle.store.release_process_storage();
            }
            settled?;
            return Ok(None);
        }
        let handle = prepared_rx
            .borrow()
            .clone()
            .expect("prepared before readiness");
        let supervisor_shutdown = shutdown.clone();
        let supervisor_runtime = handle.runtime.clone();
        let supervisor_store = handle.store.clone();
        let mut stopping = shutdown.subscribe();
        let completion = tokio::spawn(async move {
            let first = tokio::select! {
                    _ = async { stopping.wait_for(|stopped| *stopped).await.map(|_| ()) } => {
                        // Fence creation registration even if an in-flight Open
                        // has not returned its bundle yet. Its failed-registration
                        // path owns cleanup; it cannot publish a late creation.
                        supervisor_runtime.abort();
                        let drain = drain_runtime(&supervisor_runtime).await;
                        let control = finish_control(&mut control_task).await;
                        let settled = with_shutdown_error(control, drain, "replicator drain");
                        if settled.is_ok() {
                            supervisor_store.release_process_storage();
                        }
                        return settled;
                    }
                    result = &mut control_task => result,
            };
            supervisor_shutdown.send_replace(true);
            supervisor_runtime.abort();
            let first = first
                .map_err(|error| crate::host::HostError::CommandRejected(error.to_string()))
                .and_then(|result| result);
            let settled = with_shutdown_error(
                first,
                drain_runtime(&supervisor_runtime).await,
                "replicator drain",
            );
            if settled.is_ok() {
                supervisor_store.release_process_storage();
            }
            settled
        });
        Ok(Some(RunningReplica {
            handle,
            shutdown,
            completion,
        }))
    }

    async fn serve_control(
        self,
        control_listener: tokio::net::TcpListener,
        prepared: watch::Sender<Option<ReplicaHandle>>,
        ready: watch::Sender<bool>,
        shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        if *shutdown.borrow() || shutdown.has_changed().is_err() {
            return Ok(());
        }
        if self.config.replica_id.value() <= 0 {
            return Err(crate::host::HostError::CommandRejected(
                "replica ID must be positive".into(),
            ));
        }
        let binding = self.public_v1_binding.as_ref().ok_or_else(|| {
            crate::host::HostError::InitializationNotAuthorized(
                "Public V1 lifecycle binding is required".into(),
            )
        })?;
        binding.validate().map_err(|message| {
            crate::host::HostError::InitializationNotAuthorized(message.into())
        })?;
        if !binding.identity.is_public_v1() || binding.resource_uid != self.config.resource_uid {
            return Err(crate::host::HostError::InitializationNotAuthorized(
                "Public V1 binding differs from the selected process identity".into(),
            ));
        }
        let observed = observed_storage_identity(&self.config);
        let paths = self
            .application_storage_paths
            .map(|paths| {
                paths
                    .into_iter()
                    .map(|(name, path)| resolve_storage_path(&path).map(|path| (name, path)))
                    .collect::<std::io::Result<BTreeMap<_, _>>>()
            })
            .transpose()?;
        let database_path = SqliteStore::metadata_database_path(&self.config.data_root);
        let control_listener = if !database_path.is_file() {
            serve_initialization(
                &self.config,
                database_path.clone(),
                self.application_storage == ApplicationStorageState::FreshEmpty,
                paths.clone(),
                shutdown.clone(),
                self.public_v1_binding.clone(),
                control_listener,
            )
            .await?
        } else {
            control_listener
        };
        if *shutdown.borrow() || shutdown.has_changed().is_err() {
            return Ok(());
        }

        let store = Arc::new(SqliteStore::open_replacement_for_process(
            &database_path,
            &observed,
            self.config.replica_id,
            paths.as_ref(),
            binding,
        )?);
        let identity = store.identity().await?;
        let runtime = Arc::new(PodRuntime::new_replacement(
            identity.local_identity.clone(),
            self.application,
            store.clone(),
            binding.clone(),
            self.config.replication_address,
        ));
        runtime.configure_native_transport(Arc::from(self.config.bearer_token.as_str()))?;
        runtime.configure_discovery_resolver(self._resolver.clone())?;
        let agent = AgentService::new(
            store.clone(),
            runtime.clone(),
            self.config.bearer_token.clone(),
        )?;
        let local_session = agent.local_session().clone();
        store.bind_process_owner(&local_session)?;
        let process_session: Arc<str> = Arc::from(local_session.as_str());
        let replacement_registry = agent
            .public_operation_runtime(binding.identity.clone())
            .await?;
        prepared.send_replace(Some(ReplicaHandle {
            runtime,
            store,
            process_session,
            replacement: Some(ReplacementProcessHandle {
                binding: self.public_v1_binding.expect("validated Public V1 binding"),
                registry: replacement_registry,
                report_sequence: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            }),
        }));
        drop(prepared);
        agent
            .serve_control_only(control_listener, ready, shutdown)
            .await
    }
}

#[cfg(test)]
async fn abort_progress(
    task: &mut tokio::task::JoinHandle<Result<()>>,
    finished: bool,
) -> Result<()> {
    if finished {
        return Ok(());
    }
    task.abort();
    match task.await {
        Ok(result) => result,
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => Err(crate::host::HostError::CommandRejected(error.to_string())),
    }
}

fn with_shutdown_error(primary: Result<()>, cleanup: Result<()>, context: &str) -> Result<()> {
    match (primary, cleanup) {
        (Err(primary), Err(cleanup)) => Err(crate::host::HostError::CommandRejected(format!(
            "{primary}; {context}: {cleanup}"
        ))),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

const AGENT_SHUTDOWN_TIMEOUT: Duration = SHUTDOWN_TIMEOUT.saturating_add(Duration::from_secs(1));

async fn drain_runtime(runtime: &PodRuntime) -> Result<()> {
    tokio::time::timeout(AGENT_SHUTDOWN_TIMEOUT, runtime.shutdown_and_drain())
        .await
        .map_err(|_| crate::host::HostError::CommandRejected("replicator drain timed out".into()))?
        .map_err(crate::host::HostError::Runtime)
}

async fn finish_control(task: &mut tokio::task::JoinHandle<Result<()>>) -> Result<()> {
    finish_control_with_timeout(task, AGENT_SHUTDOWN_TIMEOUT).await
}

async fn finish_control_with_timeout(
    task: &mut tokio::task::JoinHandle<Result<()>>,
    timeout: Duration,
) -> Result<()> {
    match tokio::time::timeout(timeout, &mut *task).await {
        Ok(result) => {
            result.map_err(|error| crate::host::HostError::CommandRejected(error.to_string()))?
        }
        Err(_) => {
            task.abort();
            let _ = task.await;
            Err(crate::host::HostError::CommandRejected(
                "control server shutdown acknowledgement timed out".into(),
            ))
        }
    }
}

fn observed_storage_identity(config: &ReplicaProcessConfig) -> ObservedStorageIdentity {
    ObservedStorageIdentity {
        resource_uid: config.resource_uid.clone(),
        pod_uid: config.pod_uid.clone(),
        pvc_uid: config.pvc_uid.clone(),
        instance_id: ReplicaInstanceId::new(config.pod_uid.as_str()),
    }
}

async fn serve_initialization(
    config: &ReplicaProcessConfig,
    database_path: PathBuf,
    fresh_application_state: bool,
    application_storage_paths: Option<BTreeMap<String, PathBuf>>,
    mut startup_shutdown: watch::Receiver<bool>,
    public_v1_binding: Option<PublicV1LifecycleBinding>,
    listener: tokio::net::TcpListener,
) -> Result<tokio::net::TcpListener> {
    let (initialized, mut initialized_rx) = watch::channel(false);
    let service = InitializationService::new(
        observed_storage_identity(config),
        config.replica_id,
        database_path,
        config.bearer_token.clone(),
        initialized,
        fresh_application_state,
    )?
    .with_application_storage_paths(application_storage_paths);
    let service = service.with_public_v1_binding(public_v1_binding);
    let (shutdown, shutdown_rx) = watch::channel(false);
    let (ready, _) = watch::channel(false);
    let stop = tokio::spawn(async move {
        tokio::select! {
            _ = async { let _ = initialized_rx.wait_for(|initialized| *initialized).await; } => {}
            _ = async { let _ = startup_shutdown.wait_for(|stopped| *stopped).await; } => {}
        }
        shutdown.send_replace(true);
    });
    let result = service.serve(listener, ready, shutdown_rx).await;
    stop.abort();
    let _ = stop.await;
    result
}

// Resolve existing ancestors without creating even empty application directories.
fn resolve_storage_path(path: &Path) -> std::io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let absolute = std::path::absolute(path)?;
            let parent = absolute.parent().ok_or(error)?;
            let name = absolute.file_name().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid application path")
            })?;
            Ok(resolve_storage_path(parent)?.join(name))
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    #[tokio::test]
    async fn cancelling_progress_joins_and_retains_completed_failures() {
        let mut pending = tokio::spawn(std::future::pending::<Result<()>>());
        abort_progress(&mut pending, false).await.unwrap();
        assert!(pending.is_finished());
        let mut failed = tokio::spawn(async {
            Err(crate::host::HostError::CommandRejected(
                "outbound failed".into(),
            ))
        });
        while !failed.is_finished() {
            tokio::task::yield_now().await;
        }
        let cleanup = abort_progress(&mut failed, false).await;
        let result = with_shutdown_error(
            Err(crate::host::HostError::CommandRejected(
                "startup failed".into(),
            )),
            cleanup,
            "outbound shutdown",
        );
        let error = result.unwrap_err().to_string();
        assert!(error.find("startup failed").unwrap() < error.find("outbound failed").unwrap());
    }

    #[tokio::test]
    async fn agent_shutdown_waits_for_acknowledgement_and_propagates_persistence_failure() {
        let (release, wait) = tokio::sync::oneshot::channel();
        let mut agent = tokio::spawn(async move {
            wait.await.unwrap();
            Err(crate::host::HostError::CommandRejected(
                "injected persistence failure".into(),
            ))
        });
        let completion = finish_control(&mut agent);
        tokio::pin!(completion);
        tokio::select! {
            biased;
            result = &mut completion => panic!("returned before acknowledgement: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        release.send(()).unwrap();
        assert!(
            completion
                .await
                .unwrap_err()
                .to_string()
                .contains("injected persistence failure")
        );
    }

    #[tokio::test]
    async fn agent_shutdown_rejects_an_already_stopped_consumer_without_deadlock() {
        let mut agent = tokio::spawn(std::future::pending::<Result<()>>());
        agent.abort();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), finish_control(&mut agent))
                .await
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test]
    async fn agent_shutdown_timeout_is_exactly_eleven_seconds() {
        let mut agent = tokio::spawn(std::future::pending::<Result<()>>());
        assert!(
            finish_control_with_timeout(&mut agent, Duration::from_millis(10))
                .await
                .unwrap_err()
                .to_string()
                .contains("shutdown acknowledgement timed out")
        );
        assert_eq!(AGENT_SHUTDOWN_TIMEOUT, Duration::from_secs(11));
        assert!(agent.is_finished());
    }
}
