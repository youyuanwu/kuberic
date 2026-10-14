//! Public V1 control and peer services.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use tokio::net::TcpListener;
use tokio::sync::{Mutex, watch};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status};

use crate::RuntimeError;
use crate::application::OpenMode;
use crate::control::{normalize_execute_request, proto, public_v1_agent_status_to_proto};
use crate::host::Result;
use crate::host::hosting::PodRuntime;
use crate::host::provisioning::{InitializationAuthority, ObservedStorageIdentity};
use crate::host::session::ProcessSession;
use crate::host::sqlite_store::SqliteStore;
use crate::host::state::{
    AgentState, ApplicationStorageBinding, PublicOperationDisposition, PublicOperationStage,
};
use crate::host::store::AgentStore;
use crate::protocol::command::{
    ProtocolCommand, PublicV1HistoryCommand, PublicV1SessionRenewalCommand,
};
use crate::protocol::public_operations::{
    PublicOperationIntent, PublicOperationProgram, PublicV1LifecycleBinding,
};
use crate::protocol::types::{
    ProcessSessionId, ReplicaId, TransitionIntent, TransitionKind, derive_transition_id,
};

const AUTHORIZATION_HEADER: &str = "authorization";
pub(crate) const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub(crate) const PUBLIC_V1_CONTROL_SERVICE_REGISTRATIONS: &[&str] =
    &["AgentControlServer", "ReplicaPeerServer"];
pub(crate) const PUBLIC_V1_AGENT_REPORT_BRANCHES: &[&str] = &["public_v1"];
pub(crate) const PUBLIC_V1_SERVICE_STARTUP_OWNERS: &[&str] = &["PartitionOperationRuntime"];

pub(crate) struct InitializationService {
    observed: ObservedStorageIdentity,
    replica_id: ReplicaId,
    database_path: PathBuf,
    bearer_token: Arc<str>,
    session: Arc<ProcessSession>,
    initialized: watch::Sender<bool>,
    fresh_application_state: bool,
    application_storage_paths: Option<BTreeMap<String, PathBuf>>,
    binding: Option<PublicV1LifecycleBinding>,
}

impl InitializationService {
    pub(crate) fn new(
        observed: ObservedStorageIdentity,
        replica_id: ReplicaId,
        database_path: PathBuf,
        bearer_token: impl Into<Arc<str>>,
        initialized: watch::Sender<bool>,
        fresh_application_state: bool,
    ) -> Result<Self> {
        let bearer_token = bearer_token.into();
        if bearer_token.is_empty() {
            return Err(crate::host::HostError::CommandRejected(
                "agent bearer token must not be empty".into(),
            ));
        }
        Ok(Self {
            observed,
            replica_id,
            database_path,
            bearer_token,
            session: Arc::new(ProcessSession::new()),
            initialized,
            fresh_application_state,
            application_storage_paths: None,
            binding: None,
        })
    }

    pub(crate) fn with_application_storage_paths(
        mut self,
        paths: Option<BTreeMap<String, PathBuf>>,
    ) -> Self {
        self.application_storage_paths = paths;
        self
    }

    pub(crate) fn with_public_v1_binding(
        mut self,
        binding: Option<PublicV1LifecycleBinding>,
    ) -> Self {
        self.binding = binding;
        self
    }

    pub(crate) async fn serve(
        self,
        control_address: SocketAddr,
        ready: watch::Sender<bool>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let listener = TcpListener::bind(control_address).await?;
        debug_assert_eq!(PUBLIC_V1_CONTROL_SERVICE_REGISTRATIONS.len(), 2);
        let control = self.clone();
        let peer = self;
        ready.send_replace(true);
        let result = tonic::transport::Server::builder()
            .add_service(proto::agent_control_server::AgentControlServer::new(
                control,
            ))
            .add_service(proto::replica_peer_server::ReplicaPeerServer::new(peer))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                wait_for_shutdown(&mut shutdown).await;
            })
            .await
            .map_err(|error| crate::host::HostError::CommandRejected(error.to_string()));
        ready.send_replace(false);
        result
    }

    fn authorize<T>(&self, request: &Request<T>) -> std::result::Result<(), Status> {
        authorize_request(request, &self.bearer_token)
    }

    fn report(&self) -> proto::AgentStatusReport {
        proto::AgentStatusReport {
            protocol_version: crate::protocol::public_operations::PUBLIC_V1_PROTOCOL_VERSION,
            resource_uid: self.observed.resource_uid.to_string(),
            process_session_id: self.session.id().to_string(),
            storage_state: if self.fresh_application_state {
                proto::AgentStorageState::Uninitialized as i32
            } else {
                proto::AgentStorageState::Unsafe as i32
            },
            pod_uid: self.observed.pod_uid.to_string(),
            pvc_uid: self.observed.pvc_uid.to_string(),
            storage_error: if self.fresh_application_state {
                String::new()
            } else {
                "application state exists without matching Kuberic agent metadata".to_string()
            },
            healthy: self.fresh_application_state,
            replica_id: self.replica_id.value(),
            ..Default::default()
        }
    }

    fn validate_status_target(
        &self,
        request: &proto::GetAgentStatusRequest,
    ) -> std::result::Result<(), Status> {
        if request.protocol_version
            != crate::protocol::public_operations::PUBLIC_V1_PROTOCOL_VERSION
        {
            return Err(Status::failed_precondition("unsupported protocol version"));
        }
        if request.resource_uid != self.observed.resource_uid.as_str()
            || request.expected_instance_id != self.observed.instance_id.as_str()
            || request.replica_id != self.replica_id.value()
        {
            return Err(Status::failed_precondition(
                "status request targets another replica incarnation",
            ));
        }
        Ok(())
    }

    async fn initialize(
        &self,
        request: proto::ExecuteCommandRequest,
    ) -> std::result::Result<proto::ExecuteCommandResponse, Status> {
        if !self.fresh_application_state {
            return Err(Status::failed_precondition(
                "application state is not fresh and empty",
            ));
        }
        let envelope = normalize_execute_request(request)
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        if envelope.expected_process_session_id != *self.session.id() {
            return Err(Status::failed_precondition(
                "command targets a stale agent process session",
            ));
        }
        let ProtocolCommand::InitializeAgentStore(command) = envelope.command else {
            return Err(Status::failed_precondition(
                "fresh storage accepts only InitializeAgentStore",
            ));
        };
        let transition = TransitionIntent {
            secondary_scale_down: None,
            secondary_removal_evidence: None,
            scale_up: None,
            scale_up_failover: None,
            transition_id: derive_transition_id(
                &command.resource_uid,
                TransitionKind::Bootstrap,
                &command.bootstrap_configuration.configuration_id,
            ),
            kind: TransitionKind::Bootstrap,
            spec_generation: 0,
            effective_policy: command.effective_policy.clone(),
            previous_configuration_id: None,
            current_configuration: command.bootstrap_configuration.clone(),
            election_lsn: None,
            build_id: None,
            repair: None,
            switchover: None,
        };
        let authority = command.provisioning.as_ref().map_or(
            InitializationAuthority::Bootstrap(&transition),
            |provisioning| {
                if provisioning.scale_up().is_some() {
                    InitializationAuthority::ScaleUp(provisioning)
                } else {
                    InitializationAuthority::Replacement(provisioning)
                }
            },
        );
        let identity =
            crate::host::command::admit_initialization(&command, &self.observed, authority)
                .map_err(status_from_agent)?;
        let mut state = AgentState::new(identity.clone());
        state.application_storage =
            self.application_storage_paths
                .clone()
                .map(|paths| ApplicationStorageBinding {
                    paths,
                    initializing: true,
                });
        let binding = self.binding.as_ref().ok_or_else(|| {
            Status::failed_precondition("Public V1 lifecycle binding is required")
        })?;
        let store = match SqliteStore::create_replacement_authorized(
            &self.database_path,
            state,
            binding.clone(),
        ) {
            Ok(store) => store,
            Err(crate::host::HostError::Io(error))
                if error.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                SqliteStore::open_replacement_bound_existing(
                    &self.database_path,
                    Some(&identity),
                    binding,
                )
                .map_err(status_from_agent)?
            }
            Err(error) => return Err(status_from_agent(error)),
        };
        let generation =
            crate::protocol::types::PublicProtocolGeneration::new(binding.identity.generation);
        let history_initialization_id = crate::protocol::types::derive_history_initialization_id(
            &identity.resource_uid,
            generation,
        );
        let history = crate::protocol::types::derive_bootstrap_history(
            &identity.resource_uid,
            generation,
            &history_initialization_id,
        );
        let storage = crate::protocol::types::ReplicaStorageIncarnationContext {
            replica: identity.local_identity.clone(),
            storage: crate::protocol::types::ReplicaStorageIdentity::Persistent {
                pod_uid: identity.pod_uid.clone(),
                pvc_uid: identity.pvc_uid.clone(),
            },
            storage_generation: crate::protocol::types::StorageGeneration::new(format!(
                "storage-{}",
                identity.initialization_id
            )),
        };
        store
            .initialize_public_v1_history(&history_initialization_id, &history, &storage)
            .await
            .map_err(status_from_agent)?;
        drop(store);
        self.initialized.send_replace(true);
        Ok(proto::ExecuteCommandResponse {
            protocol_version: crate::protocol::public_operations::PUBLIC_V1_PROTOCOL_VERSION,
            observation: Some(self.report()),
        })
    }
}

impl Clone for InitializationService {
    fn clone(&self) -> Self {
        Self {
            observed: self.observed.clone(),
            replica_id: self.replica_id,
            database_path: self.database_path.clone(),
            bearer_token: self.bearer_token.clone(),
            session: self.session.clone(),
            initialized: self.initialized.clone(),
            fresh_application_state: self.fresh_application_state,
            application_storage_paths: self.application_storage_paths.clone(),
            binding: self.binding.clone(),
        }
    }
}

#[tonic::async_trait]
impl proto::agent_control_server::AgentControl for InitializationService {
    async fn get_status(
        &self,
        request: Request<proto::GetAgentStatusRequest>,
    ) -> std::result::Result<Response<proto::AgentStatusReport>, Status> {
        self.authorize(&request)?;
        self.validate_status_target(request.get_ref())?;
        Ok(Response::new(self.report()))
    }

    async fn execute(
        &self,
        request: Request<proto::ExecuteCommandRequest>,
    ) -> std::result::Result<Response<proto::ExecuteCommandResponse>, Status> {
        self.authorize(&request)?;
        Ok(Response::new(self.initialize(request.into_inner()).await?))
    }
}

#[tonic::async_trait]
impl proto::replica_peer_server::ReplicaPeer for InitializationService {
    async fn get_status(
        &self,
        request: Request<proto::GetAgentStatusRequest>,
    ) -> std::result::Result<Response<proto::AgentStatusReport>, Status> {
        self.authorize(&request)?;
        self.validate_status_target(request.get_ref())?;
        Ok(Response::new(self.report()))
    }

    async fn execute(
        &self,
        request: Request<proto::ExecuteCommandRequest>,
    ) -> std::result::Result<Response<proto::ExecuteCommandResponse>, Status> {
        self.authorize(&request)?;
        Ok(Response::new(self.initialize(request.into_inner()).await?))
    }
}

pub(crate) struct AgentService<S> {
    store: Arc<S>,
    runtime: Arc<PodRuntime>,
    session: Arc<ProcessSession>,
    bearer_token: Arc<str>,
    ready: Arc<AtomicBool>,
    report_sequence: Arc<AtomicU64>,
    registry: Arc<OnceLock<Arc<crate::host::operation::PartitionOperationRegistry>>>,
    operation_runtime:
        Arc<Mutex<Option<crate::host::operation_recovery::PartitionOperationRuntime>>>,
}

impl<S> Clone for AgentService<S> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            runtime: self.runtime.clone(),
            session: self.session.clone(),
            bearer_token: self.bearer_token.clone(),
            ready: self.ready.clone(),
            report_sequence: self.report_sequence.clone(),
            registry: self.registry.clone(),
            operation_runtime: self.operation_runtime.clone(),
        }
    }
}

impl<S> AgentService<S>
where
    S: AgentStore + 'static,
{
    pub(crate) fn new(
        store: Arc<S>,
        runtime: Arc<PodRuntime>,
        bearer_token: impl Into<Arc<str>>,
    ) -> Result<Self> {
        let bearer_token = bearer_token.into();
        if bearer_token.is_empty() {
            return Err(crate::host::HostError::CommandRejected(
                "agent bearer token must not be empty".into(),
            ));
        }
        Ok(Self {
            store,
            runtime,
            session: Arc::new(ProcessSession::new()),
            bearer_token,
            ready: Arc::new(AtomicBool::new(false)),
            report_sequence: Arc::new(AtomicU64::new(1)),
            registry: Arc::new(OnceLock::new()),
            operation_runtime: Arc::new(Mutex::new(None)),
        })
    }

    pub(crate) fn local_session(&self) -> &ProcessSessionId {
        self.session.id()
    }

    pub(crate) async fn public_operation_runtime(
        &self,
        preview: crate::protocol::public_operations::PublicV1Identity,
    ) -> Result<Arc<crate::host::operation::PartitionOperationRegistry>> {
        let registry = crate::host::operation::PartitionOperationRegistry::new(
            self.store.clone(),
            preview.clone(),
            self.session.id().clone(),
        )?;
        self.registry.set(registry.clone()).map_err(|_| {
            crate::host::HostError::CommandRejected(
                "Public V1 operation runtime is already active".into(),
            )
        })?;
        let runtime =
            crate::host::operation_recovery::PartitionOperationRuntime::start(registry.clone());
        self.runtime
            .bind_public_fault_runtime(
                self.store.clone(),
                registry.clone(),
                preview,
                self.session.id().clone(),
            )
            .await?;
        *self.operation_runtime.lock().await = Some(runtime);
        Ok(registry)
    }

    pub(crate) async fn serve_control_only(
        self,
        control_address: SocketAddr,
        ready: watch::Sender<bool>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let state = self.store.load_state().await?;
        self.runtime
            .bind_replica_session(state.identity.resource_uid, self.session.id().clone())?;
        let listener = TcpListener::bind(control_address).await?;
        let control = self.clone();
        let peer = self.clone();
        self.ready.store(true, Ordering::Release);
        ready.send_replace(true);
        let result = tonic::transport::Server::builder()
            .add_service(proto::agent_control_server::AgentControlServer::new(
                control,
            ))
            .add_service(proto::replica_peer_server::ReplicaPeerServer::new(peer))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                wait_for_shutdown(&mut shutdown).await;
            })
            .await
            .map_err(|error| crate::host::HostError::CommandRejected(error.to_string()));
        self.ready.store(false, Ordering::Release);
        ready.send_replace(false);
        if let Some(runtime) = self.operation_runtime.lock().await.take() {
            runtime.shutdown().await?;
        }
        let (load_metrics, reported_fault) = self.runtime.partition_report().await;
        self.store
            .record_partition_reports(load_metrics, reported_fault)
            .await?;
        self.runtime.abort();
        result
    }

    async fn public_report(&self) -> Result<proto::AgentStatusReport> {
        let state = self.store.load_state().await?;
        let preview = state.public_v1_operation_state.ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "Public V1 report requires replacement state".into(),
            )
        })?;
        self.runtime
            .refresh_replacement_projection(self.store.as_ref(), self.session.id(), false)
            .await?;
        let projection = self
            .runtime
            .replacement_report_projection(
                self.session.id().clone(),
                self.report_sequence.fetch_add(1, Ordering::Relaxed),
            )
            .await?;
        let report = crate::host::public_lifecycle::public_v1_report(
            self.store.as_ref(),
            &preview.identity,
            projection,
        )
        .await?;
        Ok(public_v1_agent_status_to_proto(&report))
    }

    async fn execute_public_intent(&self, intent: PublicOperationIntent) -> Result<()> {
        let terminal_program = matches!(
            intent.program.as_ref(),
            Some(PublicOperationProgram::Close | PublicOperationProgram::Abort)
        );
        let open_program = matches!(
            intent.program.as_ref(),
            Some(PublicOperationProgram::Open { .. })
        );
        if !matches!(
            intent.program,
            Some(PublicOperationProgram::Progress { .. } | PublicOperationProgram::Build(_))
        ) {
            self.runtime.fence_replacement_access().await;
        }
        let mut callbacks = self.runtime.public_lifecycle_callbacks()?;
        if open_program {
            let state = self.store.load_state().await?;
            let mode = if state
                .application_storage
                .as_ref()
                .is_some_and(|binding| binding.initializing)
            {
                OpenMode::New
            } else {
                OpenMode::Existing
            };
            callbacks.open_context = Some(self.runtime.public_open_context(mode).await);
        }
        let registry = self.registry.get().cloned().ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "Public V1 operation runtime is unavailable".into(),
            )
        })?;
        let operation =
            crate::host::public_lifecycle::launch(&registry, self.store.clone(), intent, callbacks)
                .await?;
        let terminal = operation.wait_for_terminal().await?;
        if terminal.stage != PublicOperationStage::Completed
            || terminal.disposition != Some(PublicOperationDisposition::Succeeded)
        {
            return Err(crate::host::HostError::CommandRejected(format!(
                "Public V1 operation did not complete successfully: {:?}",
                terminal.disposition
            )));
        }
        if open_program {
            self.runtime.complete_replacement_open().await;
            if self
                .store
                .load_state()
                .await?
                .application_storage
                .as_ref()
                .is_some_and(|binding| binding.initializing)
            {
                self.store.complete_application_initialization().await?;
            }
        }
        if !terminal_program {
            self.runtime
                .refresh_replacement_projection(self.store.as_ref(), self.session.id(), true)
                .await?;
        }
        Ok(())
    }

    async fn execute_history(&self, command: PublicV1HistoryCommand) -> Result<()> {
        command
            .validate()
            .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
        match command {
            PublicV1HistoryCommand::Initialize {
                accepted, storage, ..
            } => {
                self.store
                    .initialize_public_v1_history(
                        &accepted.history_initialization_id,
                        &accepted.history,
                        &storage,
                    )
                    .await
            }
            PublicV1HistoryCommand::Recover { intent, .. } => {
                self.execute_public_intent(*intent).await
            }
            PublicV1HistoryCommand::InstallInboundBuildAuthority { authority } => {
                let state = self.store.load_state().await?;
                if authority.attempt.target != state.identity.local_identity {
                    return Err(crate::host::HostError::IdentityMismatch(
                        "inbound build authority targets another replica".into(),
                    ));
                }
                self.runtime
                    .install_replacement_inbound_build_authority(&authority)
                    .await
                    .map_err(|error| crate::host::HostError::CommandRejected(error.to_string()))
            }
            PublicV1HistoryCommand::RevokeInboundBuildAuthority { authority } => {
                let state = self.store.load_state().await?;
                if authority.attempt.target != state.identity.local_identity {
                    return Err(crate::host::HostError::IdentityMismatch(
                        "inbound build authority targets another replica".into(),
                    ));
                }
                self.runtime
                    .revoke_replacement_inbound_build_authority(&authority)
                    .await
                    .map_err(|error| crate::host::HostError::CommandRejected(error.to_string()))
            }
            PublicV1HistoryCommand::SessionRenewal { command } => {
                self.execute_session_renewal(*command).await
            }
        }
    }

    async fn execute_session_renewal(&self, command: PublicV1SessionRenewalCommand) -> Result<()> {
        command
            .validate()
            .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
        let accepted = command.accepted();
        let current = self
            .store
            .load_state()
            .await?
            .public_v1_operation_state
            .and_then(|preview| preview.session_renewal)
            .ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "Public V1 session renewal is not durable".into(),
                )
            })?;
        if &current != accepted {
            return Err(crate::host::HostError::StaleEffectCompletion(
                "Public V1 session renewal command changed accepted state".into(),
            ));
        }
        let callbacks = self.runtime.public_lifecycle_callbacks()?;
        let next = match &command {
            PublicV1SessionRenewalCommand::CloseAccessAndIngress { .. } => {
                self.runtime.fence_replacement_access().await;
                if let Some(registry) = self.registry.get() {
                    registry.fence();
                }
                accepted.close_access()
            }
            PublicV1SessionRenewalCommand::UpdateEpochBarrier {
                survivor, epoch, ..
            } => {
                if survivor.identity == self.store.load_state().await?.identity.local_identity {
                    callbacks.replicator.update_epoch(*epoch).await?;
                }
                accepted.record_epoch_updated(survivor, *epoch)
            }
            PublicV1SessionRenewalCommand::ReplayReplacementRole {
                process_session_id,
                epoch,
                role,
                ..
            } => {
                callbacks.replicator.change_role(*epoch, *role).await?;
                accepted.replay_replacement_role(process_session_id, *epoch, *role)
            }
            PublicV1SessionRenewalCommand::InstallFreshConfiguration { configuration, .. } => {
                callbacks
                    .primary
                    .update_current_replica_set_configuration(configuration.clone().into())
                    .await?;
                accepted.install_fresh_configuration(configuration)
            }
        }
        .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
        self.store
            .persist_public_v1_session_renewal(Some(accepted), &next)
            .await
    }

    async fn execute_inner(
        &self,
        request: proto::ExecuteCommandRequest,
    ) -> std::result::Result<proto::ExecuteCommandResponse, Status> {
        let envelope = normalize_execute_request(request)
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        if envelope.expected_process_session_id != *self.session.id() {
            return Err(Status::failed_precondition(
                "command targets a stale agent process session",
            ));
        }
        let state = self.store.load_state().await.map_err(status_from_agent)?;
        if envelope.resource_uid != state.identity.resource_uid
            || envelope.target != state.identity.local_identity
        {
            return Err(Status::failed_precondition(
                "command targets another durable replica identity",
            ));
        }
        match envelope.command {
            ProtocolCommand::InitializeAgentStore(initialization) => {
                if initialization.initialization_id != state.identity.initialization_id
                    || initialization.resource_uid != state.identity.resource_uid
                    || initialization.local_replica_id != state.identity.local_identity.replica_id
                    || initialization.expected_instance_id
                        != state.identity.local_identity.instance_id
                    || initialization.expected_pod_uid != state.identity.pod_uid
                    || initialization.expected_pvc_uid != state.identity.pvc_uid
                    || initialization.assigned_agent_generation
                        != state.identity.local_identity.agent_generation
                    || initialization.effective_policy != state.identity.effective_policy
                {
                    return Err(Status::already_exists(
                        "agent store is initialized with different authority",
                    ));
                }
            }
            ProtocolCommand::PublicV1History(command) => {
                self.execute_history(*command)
                    .await
                    .map_err(status_from_agent)?;
            }
            ProtocolCommand::PublicV1Operation(command) => {
                command
                    .validate()
                    .map_err(|message| Status::invalid_argument(message.to_string()))?;
                if command.target != state.identity.local_identity {
                    return Err(Status::failed_precondition(
                        "Public V1 operation targets another replica",
                    ));
                }
                self.execute_public_intent(command.intent)
                    .await
                    .map_err(status_from_agent)?;
            }
        }
        Ok(proto::ExecuteCommandResponse {
            protocol_version: crate::protocol::public_operations::PUBLIC_V1_PROTOCOL_VERSION,
            observation: Some(self.public_report().await.map_err(status_from_agent)?),
        })
    }

    fn authorize<T>(&self, request: &Request<T>) -> std::result::Result<(), Status> {
        authorize_request(request, &self.bearer_token)
    }

    fn require_ready(&self) -> std::result::Result<(), Status> {
        if self.ready.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(Status::unavailable("agent runtime is not ready"))
        }
    }
}

#[tonic::async_trait]
impl<S> proto::agent_control_server::AgentControl for AgentService<S>
where
    S: AgentStore + 'static,
{
    async fn get_status(
        &self,
        request: Request<proto::GetAgentStatusRequest>,
    ) -> std::result::Result<Response<proto::AgentStatusReport>, Status> {
        self.authorize(&request)?;
        Ok(Response::new(
            self.public_report().await.map_err(status_from_agent)?,
        ))
    }

    async fn execute(
        &self,
        request: Request<proto::ExecuteCommandRequest>,
    ) -> std::result::Result<Response<proto::ExecuteCommandResponse>, Status> {
        self.authorize(&request)?;
        self.require_ready()?;
        Ok(Response::new(
            self.execute_inner(request.into_inner()).await?,
        ))
    }
}

#[tonic::async_trait]
impl<S> proto::replica_peer_server::ReplicaPeer for AgentService<S>
where
    S: AgentStore + 'static,
{
    async fn get_status(
        &self,
        request: Request<proto::GetAgentStatusRequest>,
    ) -> std::result::Result<Response<proto::AgentStatusReport>, Status> {
        self.authorize(&request)?;
        Ok(Response::new(
            self.public_report().await.map_err(status_from_agent)?,
        ))
    }

    async fn execute(
        &self,
        request: Request<proto::ExecuteCommandRequest>,
    ) -> std::result::Result<Response<proto::ExecuteCommandResponse>, Status> {
        self.authorize(&request)?;
        self.require_ready()?;
        Ok(Response::new(
            self.execute_inner(request.into_inner()).await?,
        ))
    }
}

fn authorize_request<T>(
    request: &Request<T>,
    bearer_token: &str,
) -> std::result::Result<(), Status> {
    let expected = format!("Bearer {bearer_token}");
    let observed = request
        .metadata()
        .get(AUTHORIZATION_HEADER)
        .and_then(|value| value.to_str().ok());
    if observed != Some(expected.as_str()) {
        return Err(Status::unauthenticated("invalid agent credentials"));
    }
    Ok(())
}

fn status_from_agent(error: crate::host::HostError) -> Status {
    match error {
        crate::host::HostError::DurableEffectConflict(_)
        | crate::host::HostError::StaleEffectCompletion(_)
        | crate::host::HostError::CommandRejected(_)
        | crate::host::HostError::SessionRejected(_) => {
            Status::failed_precondition(error.to_string())
        }
        crate::host::HostError::Backpressure(_) => Status::resource_exhausted(error.to_string()),
        crate::host::HostError::Runtime(error) => status_from_runtime(error),
        _ => Status::internal(error.to_string()),
    }
}

fn status_from_runtime(error: RuntimeError) -> Status {
    match error {
        RuntimeError::OperationCancelled => Status::cancelled(error.to_string()),
        RuntimeError::QueueFull => Status::resource_exhausted(error.to_string()),
        RuntimeError::ReplicaRemoved(_) | RuntimeError::ReconfigurationPending => {
            Status::unavailable(error.to_string())
        }
        RuntimeError::AuthorityMismatch(_)
        | RuntimeError::AuthorityNotAdmitted
        | RuntimeError::InvalidReplication(_)
        | RuntimeError::NotPrimary
        | RuntimeError::WriteClosed(_) => Status::failed_precondition(error.to_string()),
        RuntimeError::Closed | RuntimeError::NotOpen => Status::unavailable(error.to_string()),
        _ => Status::internal(error.to_string()),
    }
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::command::InitializeAgentStore;
    use crate::protocol::public_operations::{
        PublicV1Identity, PublicV1LifecycleBinding, StatePersistence,
    };
    use crate::protocol::types::{
        ConfigurationDescriptor, ConfigurationMember, EffectivePolicy, Epoch, InitializationId,
        PodUid, PvcUid, ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ResourceUid,
        derive_agent_generation, derive_initialization_id,
    };

    #[tokio::test]
    async fn initialization_service_persists_bootstrap_history_before_restart() {
        let root = tempfile::tempdir().unwrap();
        let resource_uid = ResourceUid::new("resource-init");
        let pod_uid = PodUid::new("pod-init");
        let pvc_uid = PvcUid::new("pvc-init");
        let replica_id = ReplicaId::new(1);
        let initialization_id =
            derive_initialization_id(&resource_uid, replica_id, &pod_uid, &pvc_uid);
        let local = ReplicaIdentity {
            replica_id,
            instance_id: ReplicaInstanceId::new(pod_uid.as_str()),
            agent_generation: derive_agent_generation(&initialization_id),
        };
        let policy = EffectivePolicy::fixed(1, 30).unwrap();
        let configuration = ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            replica_id,
            vec![ConfigurationMember {
                identity: local.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        let binding = PublicV1LifecycleBinding {
            identity: PublicV1Identity::new(1),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let database = root.path().join("agent.sqlite3");
        let (initialized, _) = watch::channel(false);
        let service = InitializationService::new(
            ObservedStorageIdentity {
                resource_uid: resource_uid.clone(),
                pod_uid: pod_uid.clone(),
                pvc_uid: pvc_uid.clone(),
                instance_id: local.instance_id.clone(),
            },
            replica_id,
            database.clone(),
            "test-token",
            initialized,
            true,
        )
        .unwrap()
        .with_public_v1_binding(Some(binding.clone()));
        let command = InitializeAgentStore {
            initialization_id: InitializationId::new(initialization_id.as_str()),
            resource_uid: resource_uid.clone(),
            local_replica_id: replica_id,
            expected_instance_id: local.instance_id.clone(),
            expected_pod_uid: pod_uid,
            expected_pvc_uid: pvc_uid,
            assigned_agent_generation: local.agent_generation.clone(),
            effective_policy: policy.clone(),
            bootstrap_configuration: configuration.clone(),
            provisioning: None,
        };
        let request = proto::ExecuteCommandRequest {
            protocol_version: crate::protocol::PROTOCOL_VERSION,
            resource_uid: resource_uid.to_string(),
            target: Some(local.clone().into()),
            expected_process_session_id: service.session.id().to_string(),
            command: Some(
                proto::execute_command_request::Command::InitializeAgentStore(
                    proto::InitializeAgentStoreCommand {
                        initialization_id: command.initialization_id.to_string(),
                        resource_uid: resource_uid.to_string(),
                        local_replica_id: replica_id.value(),
                        expected_instance_id: local.instance_id.to_string(),
                        expected_pod_uid: command.expected_pod_uid.to_string(),
                        expected_pvc_uid: command.expected_pvc_uid.to_string(),
                        assigned_agent_generation: local.agent_generation.to_string(),
                        effective_policy: Some(proto::EffectivePolicy {
                            replica_set_size: policy.replica_set_size,
                            write_quorum: policy.write_quorum,
                            read_quorum: policy.read_quorum,
                            failover_delay_seconds: policy.failover_delay_seconds,
                        }),
                        bootstrap_configuration: Some(configuration.into()),
                        provisioning: None,
                    },
                ),
            ),
        };
        service.initialize(request).await.unwrap();
        let store =
            SqliteStore::open_replacement_bound_existing(&database, None, &binding).unwrap();
        let state = store.load_state().await.unwrap();
        let public = state
            .public_v1_operation_state
            .expect("initialized Public V1 state");
        let history = public.history.expect("bootstrap history");
        assert_eq!(history.resource_uid, resource_uid);
        assert_eq!(history.protocol_generation.value(), 1);
    }
}
