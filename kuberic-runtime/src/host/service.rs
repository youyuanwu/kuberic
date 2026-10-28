//! Public V1 control and peer services.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use tokio::net::TcpListener;
use tokio::sync::{Mutex, watch};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status};

use crate::RuntimeError;
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
#[path = "ordinary_goal_service.rs"]
mod ordinary_goals;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicV1ControlServiceRegistration {
    AgentControl,
    ReplicaPeer,
}

impl PublicV1ControlServiceRegistration {
    #[cfg(kuberic_workspace_tests)]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::AgentControl => "AgentControlServer",
            Self::ReplicaPeer => "ReplicaPeerServer",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct AgentControlEndpoint;

impl AgentControlEndpoint {
    fn bind<S>(self, service: S) -> proto::agent_control_server::AgentControlServer<S>
    where
        S: proto::agent_control_server::AgentControl,
    {
        proto::agent_control_server::AgentControlServer::new(service)
    }
}

#[derive(Debug, Clone, Copy)]
struct ReplicaPeerEndpoint;

impl ReplicaPeerEndpoint {
    fn bind<S>(self, service: S) -> proto::replica_peer_server::ReplicaPeerServer<S>
    where
        S: proto::replica_peer_server::ReplicaPeer,
    {
        proto::replica_peer_server::ReplicaPeerServer::new(service)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PublicV1ControlServiceManifest {
    agent_control: AgentControlEndpoint,
    replica_peer: ReplicaPeerEndpoint,
}

impl PublicV1ControlServiceManifest {
    pub(crate) const fn registrations(self) -> [PublicV1ControlServiceRegistration; 2] {
        [
            PublicV1ControlServiceRegistration::AgentControl,
            PublicV1ControlServiceRegistration::ReplicaPeer,
        ]
    }

    fn agent_control<S>(self, service: S) -> proto::agent_control_server::AgentControlServer<S>
    where
        S: proto::agent_control_server::AgentControl,
    {
        self.agent_control.bind(service)
    }

    fn replica_peer<S>(self, service: S) -> proto::replica_peer_server::ReplicaPeerServer<S>
    where
        S: proto::replica_peer_server::ReplicaPeer,
    {
        self.replica_peer.bind(service)
    }
}

pub(crate) const PUBLIC_V1_CONTROL_SERVICES: PublicV1ControlServiceManifest =
    PublicV1ControlServiceManifest {
        agent_control: AgentControlEndpoint,
        replica_peer: ReplicaPeerEndpoint,
    };

pub(crate) fn validate_public_v1_control_services(
    manifest: PublicV1ControlServiceManifest,
) -> Result<()> {
    if manifest.registrations()
        != [
            PublicV1ControlServiceRegistration::AgentControl,
            PublicV1ControlServiceRegistration::ReplicaPeer,
        ]
    {
        return Err(crate::host::HostError::CommandRejected(
            "Public V1 control service registrations changed".into(),
        ));
    }
    Ok(())
}
#[cfg(kuberic_workspace_tests)]
pub(crate) const PUBLIC_V1_AGENT_REPORT_BRANCHES: &[&str] = &["public_v1"];
#[cfg(kuberic_workspace_tests)]
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
        listener: TcpListener,
        ready: watch::Sender<bool>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<TcpListener> {
        let services = PUBLIC_V1_CONTROL_SERVICES;
        validate_public_v1_control_services(services)?;
        let control = self.clone();
        let peer = self;
        let mut incoming = TcpListenerStream::new(listener);
        ready.send_replace(true);
        let result = tonic::transport::Server::builder()
            .add_service(services.agent_control(control))
            .add_service(services.replica_peer(peer))
            .serve_with_incoming_shutdown(&mut incoming, async move {
                wait_for_shutdown(&mut shutdown).await;
            })
            .await
            .map_err(|error| crate::host::HostError::CommandRejected(error.to_string()));
        ready.send_replace(false);
        result?;
        Ok(incoming.into_inner())
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
    async fn submit_goal(
        &self,
        request: Request<proto::ReplicaGoalRequest>,
    ) -> std::result::Result<Response<proto::ReplicaSnapshotResponse>, Status> {
        self.authorize(&request)?;
        Err(Status::failed_precondition(
            "agent store is not initialized",
        ))
    }
    async fn get_snapshot(
        &self,
        request: Request<proto::ReplicaSnapshotRequest>,
    ) -> std::result::Result<Response<proto::ReplicaSnapshotResponse>, Status> {
        self.authorize(&request)?;
        let query = request.into_inner();
        let target = query
            .target
            .ok_or_else(|| Status::invalid_argument("missing snapshot target"))?;
        self.validate_status_target(&proto::GetAgentStatusRequest {
            protocol_version: query.protocol_version,
            resource_uid: query.resource_uid,
            replica_id: target.replica_id,
            expected_instance_id: target.instance_id,
        })?;
        if !query.expected_process_session_id.is_empty()
            && query.expected_process_session_id != self.session.id().as_str()
        {
            return Err(Status::failed_precondition("snapshot process changed"));
        }
        let fresh = crate::control::normalize_uninitialized_agent_status_report(self.report())
            .map_err(|error| Status::failed_precondition(error.to_string()))?;
        Ok(Response::new(proto::ReplicaSnapshotResponse {
            protocol_version: crate::protocol::public_operations::ORDINARY_ACTION_FORMAT,
            canonical_json: serde_json::to_vec(
                &crate::protocol::public_operations::ReplicaAgentQueryResult::Uninitialized(fresh),
            )
            .map_err(|error| Status::internal(error.to_string()))?,
        }))
    }
    async fn acknowledge_goal(
        &self,
        request: Request<proto::ReplicaGoalRequest>,
    ) -> std::result::Result<Response<proto::ReplicaSnapshotResponse>, Status> {
        self.authorize(&request)?;
        Err(Status::failed_precondition(
            "agent store is not initialized",
        ))
    }
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
    async fn discover(
        &self,
        request: Request<proto::ReplicaDiscoveryRequest>,
    ) -> std::result::Result<Response<proto::ReplicaSnapshotResponse>, Status> {
        self.authorize(&request)?;
        Err(Status::failed_precondition(
            "agent store is not initialized",
        ))
    }
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

pub(crate) async fn align_session_renewal(
    store: &dyn crate::host::store::AgentStore,
    mut current: crate::protocol::public_operations::PublicV1SessionRenewalStatus,
    accepted: &crate::protocol::public_operations::PublicV1SessionRenewalStatus,
) -> Result<crate::protocol::public_operations::PublicV1SessionRenewalStatus> {
    use crate::protocol::public_operations::{
        PublicSessionEpochBarrierStage, PublicSessionRenewalStage,
    };

    if current.required != accepted.required {
        return Err(crate::host::HostError::StaleEffectCompletion(
            "Public V1 session renewal changed required authority".into(),
        ));
    }
    for _ in 0..32 {
        if &current == accepted {
            return Ok(current);
        }
        let next = match current.stage {
            PublicSessionRenewalStage::Required => current.close_access(),
            PublicSessionRenewalStage::AccessClosed => current.freeze_surviving_set(),
            PublicSessionRenewalStage::SurvivingSetFrozen => current.begin_epoch_barriers(),
            PublicSessionRenewalStage::EpochBarriersPending => {
                if let Some(barrier) = current.barriers.iter().find(|barrier| {
                    barrier.stage == PublicSessionEpochBarrierStage::Pending
                        && accepted
                            .barriers
                            .iter()
                            .find(|accepted| accepted.survivor == barrier.survivor)
                            .is_some_and(|accepted| {
                                accepted.stage != PublicSessionEpochBarrierStage::Pending
                            })
                }) {
                    let accepted_barrier = accepted
                        .barriers
                        .iter()
                        .find(|accepted| accepted.survivor == barrier.survivor)
                        .expect("selected accepted barrier");
                    match accepted_barrier.stage {
                        PublicSessionEpochBarrierStage::Updated => current.record_epoch_updated(
                            &barrier.survivor,
                            current.required.renewal_epoch,
                        ),
                        PublicSessionEpochBarrierStage::Excluded => current.exclude_survivor(
                            &barrier.survivor,
                            accepted_barrier
                                .retirement_operation_id
                                .clone()
                                .expect("validated excluded barrier"),
                        ),
                        PublicSessionEpochBarrierStage::Pending => unreachable!(),
                    }
                } else {
                    current.finish_epoch_barriers()
                }
            }
            PublicSessionRenewalStage::ReplacementRolePending => {
                let replacement = current
                    .required
                    .replacement
                    .as_ref()
                    .expect("validated replacement stage");
                current.replay_replacement_role(
                    &replacement.process_session_id,
                    current.required.renewal_epoch,
                    current.required.predecessor.role,
                )
            }
            PublicSessionRenewalStage::ConfigurationPending => {
                current.install_fresh_configuration(&current.required.fresh_configuration)
            }
            PublicSessionRenewalStage::Authorized | PublicSessionRenewalStage::Retired => {
                return Err(crate::host::HostError::StaleEffectCompletion(
                    "Public V1 session renewal regressed accepted state".into(),
                ));
            }
        }
        .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
        store
            .persist_public_v1_session_renewal(Some(&current), &next)
            .await?;
        current = next;
    }
    Err(crate::host::HostError::CommandRejected(
        "Public V1 session renewal exceeded bounded local replay".into(),
    ))
}

pub(crate) async fn replay_role_at_epoch(
    replicator: &dyn crate::replicator::Replicator,
    native: Option<(
        crate::replicator::v1::LifecycleObservation,
        Option<crate::protocol::types::Epoch>,
    )>,
    epoch: crate::protocol::types::Epoch,
    role: crate::protocol::types::ReplicaRole,
) -> crate::Result<()> {
    if let Some((observation, selected)) = native {
        use crate::replicator::v1::LifecycleObservation;
        match observation {
            LifecycleObservation::Opened => return replicator.change_role(epoch, role).await,
            LifecycleObservation::Ready(current) => {
                if selected != Some(epoch) {
                    replicator.update_epoch(epoch).await?;
                }
                if current != role {
                    replicator.change_role(epoch, role).await?;
                }
                return Ok(());
            }
            LifecycleObservation::Closed | LifecycleObservation::Faulted => {
                return Err(RuntimeError::Closed);
            }
            LifecycleObservation::TransportQuorum { .. }
            | LifecycleObservation::EpochApplied(_) => {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
        }
    }
    replicator.update_epoch(epoch).await?;
    replicator.change_role(epoch, role).await
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
        if self
            .store
            .public_operation_records()
            .await?
            .iter()
            .any(|record| record.intent.goal().is_some())
        {
            self.runtime.enable_ordinary_contract()?;
        }
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
        let runtime = Arc::downgrade(&self.runtime);
        registry.bind_terminal_recovery(move || {
            runtime
                .upgrade()
                .ok_or(crate::host::HostError::CommandRejected(
                    "terminal recovery runtime lost".into(),
                ))?
                .public_lifecycle_callbacks()
                .map_err(Into::into)
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
        listener: TcpListener,
        ready: watch::Sender<bool>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let state = self.store.load_state().await?;
        self.runtime
            .bind_replica_session(state.identity.resource_uid, self.session.id().clone())?;
        let services = PUBLIC_V1_CONTROL_SERVICES;
        validate_public_v1_control_services(services)?;
        let control = self.clone();
        let peer = self.clone();
        self.ready.store(true, Ordering::Release);
        ready.send_replace(true);
        let result = tonic::transport::Server::builder()
            .add_service(services.agent_control(control))
            .add_service(services.replica_peer(peer))
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
        if !self.runtime.is_ordinary_contract() {
            self.runtime
                .refresh_replacement_projection(self.store.as_ref(), self.session.id(), false)
                .await?;
        }
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
        let registry = self.registry.get().cloned().ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "Public V1 operation runtime is unavailable".into(),
            )
        })?;
        let service = self.clone();
        let (completed, result) = tokio::sync::oneshot::channel();
        // Access-fence release and projection publication belong to the accepted
        // execution, not to the lifetime of the RPC waiting for its reply.
        registry.try_own_control_task(async move {
            let outcome = service.execute_public_intent_owned(intent).await;
            let _ = completed.send(outcome);
        })?;
        result.await.map_err(|error| {
            crate::host::HostError::CommandRejected(format!(
                "owned public execution stopped before replying: {error}"
            ))
        })?
    }

    async fn execute_public_intent_owned(&self, intent: PublicOperationIntent) -> Result<()> {
        let terminal_program = matches!(
            intent.program.as_ref(),
            Some(PublicOperationProgram::Close | PublicOperationProgram::Abort)
        );
        let open_program = matches!(
            intent.program.as_ref(),
            Some(PublicOperationProgram::Open { .. })
        );
        let mut callbacks = self.runtime.public_lifecycle_callbacks()?;
        if open_program {
            let state = self.store.load_state().await?;
            let mode = state.application_open_mode();
            callbacks.open_context = Some(self.runtime.public_open_context(mode).await);
        }
        let registry = self.registry.get().cloned().ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "Public V1 operation runtime is unavailable".into(),
            )
        })?;
        let (operation, attempt, mut access_guard) =
            crate::host::public_lifecycle::launch_bound_attempt(
                &registry,
                self.store.clone(),
                intent,
                callbacks,
                |operation| self.runtime.public_access_completion_guard(operation),
            )
            .await?;
        let terminal = operation.wait_for_attempt(attempt).await?;
        if terminal.stage != PublicOperationStage::Completed
            || terminal.disposition != Some(PublicOperationDisposition::Succeeded)
        {
            return Err(crate::host::HostError::CommandRejected(format!(
                "Public V1 operation did not complete successfully: {:?}",
                terminal.disposition
            )));
        }
        registry
            .release_session_renewal_fence(&operation.intent())
            .await?;
        self.runtime.complete_public_access(&operation);
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
        access_guard.succeeded();
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
            PublicV1HistoryCommand::SessionRenewal { command } => {
                self.execute_session_renewal(*command).await
            }
        }
    }

    async fn execute_session_renewal(&self, command: PublicV1SessionRenewalCommand) -> Result<()> {
        let registry = self.registry.get().cloned().ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "Public V1 operation runtime is unavailable".into(),
            )
        })?;
        let service = self.clone();
        let (completed, result) = tokio::sync::oneshot::channel();
        registry.try_own_control_task(async move {
            let outcome = service.execute_session_renewal_owned(command).await;
            let _ = completed.send(outcome);
        })?;
        result.await.map_err(|error| {
            crate::host::HostError::CommandRejected(format!(
                "owned session renewal stopped before replying: {error}"
            ))
        })?
    }

    async fn execute_session_renewal_owned(
        &self,
        command: PublicV1SessionRenewalCommand,
    ) -> Result<()> {
        command
            .validate()
            .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
        let endpoint = command.expected_endpoint();
        if endpoint.process_session_id != *self.session.id()
            || endpoint.replicator_creation_id
                != self.runtime.replacement_replicator_creation_id()?
        {
            return Err(crate::host::HostError::IdentityMismatch(
                "session-renewal command targets another process endpoint".into(),
            ));
        }
        let accepted = command.accepted();
        let current = self
            .store
            .load_state()
            .await?
            .public_v1_operation_state
            .and_then(|preview| preview.session_renewal);
        if let PublicV1SessionRenewalCommand::ReplayReplacementRole {
            process_session_id,
            epoch,
            role,
            ..
        } = &command
            && current.as_ref()
                == Some(
                    &accepted
                        .replay_replacement_role(process_session_id, *epoch, *role)
                        .map_err(|message| {
                            crate::host::HostError::CommandRejected(message.into())
                        })?,
                )
        {
            let operation_id =
                crate::host::public_lifecycle::replacement_role_operation_id(accepted);
            if self
                .store
                .public_operation_records()
                .await?
                .iter()
                .any(|record| {
                    record.intent.operation_id == operation_id
                        && crate::host::public_lifecycle::is_replacement_role_replay(
                            accepted,
                            &endpoint.identity,
                            &record.intent,
                        )
                        && record.stage == PublicOperationStage::Completed
                        && record.disposition == Some(PublicOperationDisposition::Succeeded)
                })
            {
                return Ok(());
            }
            return Err(crate::host::HostError::CommandRejected(
                "replacement role replay lacks its successful owned completion".into(),
            ));
        }
        let current = match current {
            Some(current)
                if crate::host::session_renewal_supersedes_local_journal(accepted, &current) =>
            {
                self.store
                    .persist_public_v1_session_renewal(Some(&current), accepted)
                    .await?;
                accepted.clone()
            }
            Some(current) => align_session_renewal(self.store.as_ref(), current, accepted).await?,
            None => {
                self.store
                    .persist_public_v1_session_renewal(None, accepted)
                    .await?;
                accepted.clone()
            }
        };
        debug_assert_eq!(&current, accepted);
        let callbacks = self.runtime.public_lifecycle_callbacks()?;
        let next = match &command {
            PublicV1SessionRenewalCommand::CloseAccessAndIngress { .. } => {
                self.runtime.fence_replacement_access().await;
                if let Some(registry) = self.registry.get() {
                    registry.fence_session_renewal();
                }
                accepted.close_access()
            }
            PublicV1SessionRenewalCommand::UpdateEpochBarrier {
                survivor, epoch, ..
            } => {
                if survivor.identity == self.store.load_state().await?.identity.local_identity {
                    replay_role_at_epoch(
                        callbacks.replicator.as_ref(),
                        None,
                        *epoch,
                        survivor.role,
                    )
                    .await?;
                }
                accepted.record_epoch_updated(survivor, *epoch)
            }
            PublicV1SessionRenewalCommand::ReplayReplacementRole {
                process_session_id,
                epoch,
                role,
                ..
            } => {
                let records = self.store.public_operation_records().await?;
                let opened = records.iter().any(|record| {
                    record.intent.process_session_id == *process_session_id
                        && record.stage == PublicOperationStage::Completed
                        && record.disposition == Some(PublicOperationDisposition::Succeeded)
                        && record.accepted_storage.as_ref() == Some(&endpoint.storage_incarnation)
                        && matches!(record.intent.program,
                            Some(PublicOperationProgram::Open { .. }))
                });
                if !opened {
                    let operation_id = crate::protocol::types::OperationId::new(format!(
                        "{}:replacement-open", accepted.required.operation_id,
                    ));
                    let program = PublicOperationProgram::Open {
                        replica: endpoint.identity.clone(),
                        existing: true,
                    };
                    let intent = match records.iter()
                        .find(|record| record.intent.operation_id == operation_id)
                    {
                        Some(record) if record.intent.program.as_ref() == Some(&program)
                            && record.intent.process_session_id == *process_session_id
                            && record.intent.class
                                == crate::protocol::public_operations::PublicOperationClass::Authority =>
                            record.intent.clone(),
                        Some(_) => return Err(crate::host::HostError::DurableEffectConflict(
                            "replacement Open operation changed captured authority".into(),
                        )),
                        None => PublicOperationIntent {
                            identity: crate::protocol::public_operations::PublicV1Identity::new(
                                accepted.required.history.protocol_generation.value(),
                            ),
                            operation_id,
                            revision: records.iter().map(|record| record.intent.revision)
                                .max().unwrap_or(0).checked_add(1).ok_or_else(|| {
                                    crate::host::HostError::CommandRejected(
                                        "replacement Open revision exhausted".into(),
                                    )
                                })?,
                            process_session_id: process_session_id.clone(),
                            class: crate::protocol::public_operations::PublicOperationClass::Authority,
                            input_digest: program.digest(),
                            replay_disposition: program.replay_disposition(),
                            lifecycle: None,
                            program: Some(program),
                        },
                    };
                    self.execute_public_intent_owned(intent).await?;
                }
                let operation_id =
                    crate::host::public_lifecycle::replacement_role_operation_id(accepted);
                let records = self.store.public_operation_records().await?;
                let program = PublicOperationProgram::Role {
                    epoch: *epoch,
                    role: *role,
                };
                let intent = match records
                    .iter()
                    .find(|record| record.intent.operation_id == operation_id)
                {
                    Some(record) => record.intent.clone(),
                    None => PublicOperationIntent {
                        identity: self
                            .store
                            .load_state()
                            .await?
                            .public_v1_operation_state
                            .ok_or_else(|| {
                                crate::host::HostError::CommandRejected(
                                    "role replay requires Public V1 state".into(),
                                )
                            })?
                            .identity,
                        operation_id,
                        revision: records
                            .iter()
                            .map(|record| record.intent.revision)
                            .max()
                            .unwrap_or(0)
                            .checked_add(1)
                            .ok_or_else(|| {
                                crate::host::HostError::CommandRejected(
                                    "role replay revision exhausted".into(),
                                )
                            })?,
                        process_session_id: process_session_id.clone(),
                        class: crate::protocol::public_operations::PublicOperationClass::Authority,
                        input_digest: program.digest(),
                        replay_disposition: program.replay_disposition(),
                        lifecycle: None,
                        program: Some(program),
                    },
                };
                let state = self.store.load_state().await?;
                if !crate::host::public_lifecycle::is_replacement_role_replay(
                    accepted,
                    &state.identity.local_identity,
                    &intent,
                ) {
                    return Err(crate::host::HostError::IdentityMismatch(
                        "replacement role operation changed captured authority".into(),
                    ));
                }
                self.execute_public_intent_owned(intent).await?;
                accepted.replay_replacement_role(process_session_id, *epoch, *role)
            }
            PublicV1SessionRenewalCommand::InstallFreshConfiguration { configuration, .. } => {
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
                if !matches!(command.as_ref(), PublicV1HistoryCommand::Initialize { .. }) {
                    return Err(Status::failed_precondition(
                        "legacy renewal dispatch is unavailable in ordinary hosting format 18",
                    ));
                }
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
                return Err(Status::failed_precondition(
                    "controller callback programs are unavailable; submit a semantic replica goal",
                ));
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
    async fn submit_goal(
        &self,
        request: Request<proto::ReplicaGoalRequest>,
    ) -> std::result::Result<Response<proto::ReplicaSnapshotResponse>, Status> {
        self.authorize(&request)?;
        self.require_ready()?;
        Ok(Response::new(
            self.submit_ordinary_goal(request.into_inner()).await?,
        ))
    }
    async fn get_snapshot(
        &self,
        request: Request<proto::ReplicaSnapshotRequest>,
    ) -> std::result::Result<Response<proto::ReplicaSnapshotResponse>, Status> {
        self.authorize(&request)?;
        self.validate_snapshot_request(request.get_ref()).await?;
        let snapshot = if request.get_ref().sample_native_progress {
            self.sampled_ordinary_snapshot().await?
        } else {
            self.ordinary_snapshot().await?
        };
        Ok(Response::new(proto::ReplicaSnapshotResponse {
            protocol_version: crate::protocol::public_operations::ORDINARY_ACTION_FORMAT,
            canonical_json: serde_json::to_vec(
                &crate::protocol::public_operations::ReplicaAgentQueryResult::Initialized(
                    Box::new(snapshot),
                ),
            )
            .map_err(|error| Status::internal(error.to_string()))?,
        }))
    }
    async fn acknowledge_goal(
        &self,
        request: Request<proto::ReplicaGoalRequest>,
    ) -> std::result::Result<Response<proto::ReplicaSnapshotResponse>, Status> {
        self.authorize(&request)?;
        Ok(Response::new(
            self.acknowledge_ordinary_goal(request.into_inner()).await?,
        ))
    }
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
    async fn discover(
        &self,
        request: Request<proto::ReplicaDiscoveryRequest>,
    ) -> std::result::Result<Response<proto::ReplicaSnapshotResponse>, Status> {
        self.authorize(&request)?;
        Ok(Response::new(
            self.discover_ordinary_peer(request.into_inner()).await?,
        ))
    }
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
#[path = "service_cancellation_tests.rs"]
mod cancellation_tests;

#[cfg(test)]
#[path = "ordinary_goal_tests.rs"]
mod ordinary_goal_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use crate::application::{OpenContext, RoleChange, StatefulServiceReplica};
    use crate::protocol::command::{
        InitializeAgentStore, PublicV1HistoryCommand, PublicV1SessionRenewalCommand,
    };
    use crate::protocol::public_operations::{
        ProcessSessionRenewalRequired, PublicV1Identity, PublicV1LifecycleBinding,
        PublicV1SessionRenewalStatus, StatePersistence,
    };
    use crate::protocol::types::{
        ConfigurationDescriptor, ConfigurationMember, EffectivePolicy, Epoch, InitializationId,
        OperationId, PodUid, ProcessSessionEndpoint, ProcessSessionId, ProcessSessionRenewalReason,
        PvcUid, ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity,
        ReplicaStorageIncarnationContext, ReplicatorCreationId, ResourceUid, StorageGeneration,
        derive_agent_generation, derive_initialization_id,
    };
    use crate::replicator::Replicator;

    #[tokio::test]
    async fn dynamic_control_listener_reserves_port_until_dropped() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        assert_ne!(address.port(), 0);
        assert!(TcpListener::bind(address).await.is_err());
        drop(listener);
        TcpListener::bind(address).await.unwrap();
    }

    #[tokio::test]
    async fn initialization_returns_the_same_bound_listener_for_reuse() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let root = tempfile::tempdir().unwrap();
            let mut listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            for _ in 0..2 {
                let (initialized, _) = watch::channel(false);
                let service = InitializationService::new(
                    ObservedStorageIdentity {
                        resource_uid: ResourceUid::new("listener-resource"),
                        pod_uid: PodUid::new("listener-pod"),
                        pvc_uid: PvcUid::new("listener-pvc"),
                        instance_id: ReplicaInstanceId::new("listener-pod"),
                    },
                    ReplicaId::new(1),
                    root.path().join("agent.sqlite3"),
                    "test-token",
                    initialized,
                    true,
                )
                .unwrap();
                let (ready, mut ready_rx) = watch::channel(false);
                let (shutdown, shutdown_rx) = watch::channel(false);
                let server = tokio::spawn(service.serve(listener, ready, shutdown_rx));
                ready_rx.wait_for(|ready| *ready).await.unwrap();
                assert!(TcpListener::bind(address).await.is_err());
                let mut client = proto::agent_control_client::AgentControlClient::connect(format!(
                    "http://{address}"
                ))
                .await
                .unwrap();
                let mut request = Request::new(proto::GetAgentStatusRequest {
                    protocol_version: crate::protocol::PROTOCOL_VERSION,
                    resource_uid: "listener-resource".into(),
                    replica_id: 1,
                    expected_instance_id: "listener-pod".into(),
                });
                request
                    .metadata_mut()
                    .insert(AUTHORIZATION_HEADER, "Bearer test-token".parse().unwrap());
                let report = client.get_status(request).await.unwrap().into_inner();
                assert_eq!(
                    report.storage_state,
                    proto::AgentStorageState::Uninitialized as i32
                );
                drop(client);
                shutdown.send_replace(true);
                listener = server.await.unwrap().unwrap();
                assert_eq!(listener.local_addr().unwrap(), address);
                assert!(TcpListener::bind(address).await.is_err());
            }
            drop(listener);
            TcpListener::bind(address).await.unwrap();
        })
        .await
        .unwrap();
    }

    #[derive(Default)]
    struct RenewalReplicator {
        calls: StdMutex<Vec<(&'static str, Epoch, Option<ReplicaRole>)>>,
        updated_epoch: StdMutex<Option<Epoch>>,
    }

    #[async_trait::async_trait]
    impl Replicator for RenewalReplicator {
        async fn open(&self) -> crate::Result<String> {
            Ok(String::new())
        }

        async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> crate::Result<()> {
            if *self.updated_epoch.lock().unwrap() != Some(epoch) {
                return Err(crate::RuntimeError::ReconfigurationPending);
            }
            self.calls.lock().unwrap().push(("role", epoch, Some(role)));
            Ok(())
        }

        async fn update_epoch(&self, epoch: Epoch) -> crate::Result<()> {
            *self.updated_epoch.lock().unwrap() = Some(epoch);
            self.calls.lock().unwrap().push(("epoch", epoch, None));
            Ok(())
        }

        async fn close(&self) -> crate::Result<()> {
            Ok(())
        }

        fn abort(&self) {}

        async fn current_progress(&self) -> crate::Result<i64> {
            Ok(0)
        }

        async fn catch_up_capability(&self) -> crate::Result<i64> {
            Ok(0)
        }
    }

    struct RenewalApplication;

    #[async_trait::async_trait]
    impl StatefulServiceReplica for RenewalApplication {
        async fn open(
            self: Arc<Self>,
            _context: OpenContext,
        ) -> crate::Result<Arc<dyn Replicator>> {
            Err(crate::RuntimeError::NotOpen)
        }

        async fn change_role(&self, _role: ReplicaRole) -> crate::Result<RoleChange> {
            Err(crate::RuntimeError::NotOpen)
        }

        async fn close(&self) -> crate::Result<()> {
            Ok(())
        }

        fn abort(&self) {}
    }

    #[tokio::test]
    async fn role_replay_updates_epoch_before_role() {
        let replicator = RenewalReplicator::default();
        let epoch = Epoch::new(0, 2);

        replay_role_at_epoch(&replicator, None, epoch, ReplicaRole::ActiveSecondary)
            .await
            .unwrap();

        assert_eq!(
            *replicator.calls.lock().unwrap(),
            vec![
                ("epoch", epoch, None),
                ("role", epoch, Some(ReplicaRole::ActiveSecondary)),
            ]
        );
    }

    #[tokio::test]
    async fn native_role_replay_does_not_repeat_same_role_change() {
        use crate::replicator::v1::LifecycleObservation;

        let replicator = RenewalReplicator::default();
        let epoch = Epoch::new(0, 2);
        for selected in [Some(Epoch::new(0, 1)), Some(epoch)] {
            replay_role_at_epoch(
                &replicator,
                Some((LifecycleObservation::Ready(ReplicaRole::Primary), selected)),
                epoch,
                ReplicaRole::Primary,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            *replicator.calls.lock().unwrap(),
            vec![("epoch", epoch, None)]
        );
    }

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

    #[tokio::test]
    async fn agent_service_rejects_legacy_session_renewal_before_mutation() {
        let root = tempfile::tempdir().unwrap();
        let resource_uid = ResourceUid::new("resource-renewal");
        let local = ReplicaIdentity {
            replica_id: ReplicaId::new(1),
            instance_id: ReplicaInstanceId::new("pod-renewal"),
            agent_generation: crate::protocol::types::AgentGeneration::new("generation-renewal"),
        };
        let storage = ReplicaStorageIncarnationContext {
            replica: local.clone(),
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new("pod-renewal"),
                pvc_uid: PvcUid::new("pvc-renewal"),
            },
            storage_generation: StorageGeneration::new("storage-renewal"),
        };
        let identity = PublicV1Identity::new(1);
        let binding = PublicV1LifecycleBinding {
            identity: identity.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let database = root.path().join("agent.sqlite3");
        let store = Arc::new(
            SqliteStore::create_replacement_authorized(
                &database,
                crate::host::state::AgentState::new(crate::host::state::StorageIdentity {
                    schema_version: crate::host::state::SCHEMA_VERSION,
                    resource_uid: resource_uid.clone(),
                    pod_uid: PodUid::new("pod-renewal"),
                    pvc_uid: PvcUid::new("pvc-renewal"),
                    initialization_id: InitializationId::new("init-renewal"),
                    local_identity: local.clone(),
                    effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
                }),
                binding.clone(),
            )
            .unwrap(),
        );
        let initialization = crate::protocol::types::derive_history_initialization_id(
            &resource_uid,
            crate::protocol::types::PublicProtocolGeneration::new(1),
        );
        let history = crate::protocol::types::derive_bootstrap_history(
            &resource_uid,
            crate::protocol::types::PublicProtocolGeneration::new(1),
            &initialization,
        );
        store
            .initialize_public_v1_history(&initialization, &history, &storage)
            .await
            .unwrap();
        let runtime = Arc::new(PodRuntime::new_replacement(
            local.clone(),
            Arc::new(RenewalApplication),
            store.clone(),
            binding,
            "127.0.0.1:0".parse().unwrap(),
        ));
        let service = AgentService::new(store.clone(), runtime.clone(), "test-token").unwrap();
        runtime
            .bind_replica_session(resource_uid.clone(), service.local_session().clone())
            .unwrap();
        service.public_operation_runtime(identity).await.unwrap();
        let replacement = ProcessSessionEndpoint {
            identity: local.clone(),
            storage_incarnation: storage.clone(),
            process_session_id: service.local_session().clone(),
            replicator_creation_id: runtime.replacement_replicator_creation_id().unwrap(),
            role: ReplicaRole::None,
        };
        let renewal_epoch = Epoch::new(0, 2);
        let accepted = PublicV1SessionRenewalStatus::required(ProcessSessionRenewalRequired {
            operation_id: OperationId::new("renewal-wire"),
            reason: ProcessSessionRenewalReason::PrimaryReplacement,
            resource_uid: resource_uid.clone(),
            history,
            previous_epoch: Epoch::new(0, 1),
            renewal_epoch,
            predecessor: ProcessSessionEndpoint {
                process_session_id: ProcessSessionId::new("predecessor-session"),
                replicator_creation_id: ReplicatorCreationId::new("predecessor-creation"),
                role: ReplicaRole::Primary,
                ..replacement.clone()
            },
            replacement: Some(replacement),
            surviving_sessions: Vec::new(),
            fresh_configuration: ConfigurationDescriptor::new(
                renewal_epoch,
                local.replica_id,
                vec![ConfigurationMember {
                    identity: local.clone(),
                    role: ReplicaRole::Primary,
                }],
                1,
            ),
            predecessor_contained: true,
            data_loss: None,
        })
        .unwrap();
        let command = PublicV1HistoryCommand::SessionRenewal {
            command: Box::new(PublicV1SessionRenewalCommand::CloseAccessAndIngress { accepted }),
        };
        let request = proto::ExecuteCommandRequest {
            protocol_version: crate::protocol::PROTOCOL_VERSION,
            resource_uid: resource_uid.to_string(),
            target: Some(local.into()),
            expected_process_session_id: service.local_session().to_string(),
            command: Some(proto::execute_command_request::Command::PublicV1History(
                crate::control::public_v1_history_command_to_proto(&command).unwrap(),
            )),
        };
        let before = store.load_state().await.unwrap();
        let error = service.execute_inner(request).await.unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(
            error
                .message()
                .contains("unavailable in ordinary hosting format 18")
        );
        assert_eq!(store.load_state().await.unwrap(), before);
        service
            .operation_runtime
            .lock()
            .await
            .take()
            .unwrap()
            .shutdown()
            .await
            .unwrap();
    }
}
