//! Tonic control, peer, and replication services.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use futures::{Stream, StreamExt};
use kuberic_protocol::command::ProtocolCommand;
use kuberic_protocol::types::{ProcessSessionId, ReplicaIdentity};
use kuberic_runtime::application::OpenMode;
use kuberic_wire::{normalize_execute_request, proto};
use tokio::net::TcpListener;
use tokio::sync::{RwLock, watch};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status};

use crate::coordinator::Coordinator;
use crate::hosting::{PodRuntime, RuntimeDataPlane};
use crate::report::AgentReporter;
use crate::runtime_adapter::RuntimeEffectExecutor;
use crate::store::AgentStore;
use crate::{AgentError, Result};

const AUTHORIZATION_HEADER: &str = "authorization";

pub struct SessionRegistry {
    local_session: ProcessSessionId,
    peers: RwLock<BTreeMap<ReplicaIdentity, ProcessSessionId>>,
}

impl SessionRegistry {
    pub fn new(local_session: ProcessSessionId) -> Self {
        Self {
            local_session,
            peers: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn local_session(&self) -> &ProcessSessionId {
        &self.local_session
    }

    pub async fn register_peer(&self, identity: ReplicaIdentity, session: ProcessSessionId) {
        self.peers.write().await.insert(identity, session);
    }

    pub async fn validate_peer(
        &self,
        sender: &ReplicaIdentity,
        sender_session: &str,
        receiver_session: &str,
    ) -> std::result::Result<(), Status> {
        if receiver_session != self.local_session.as_str() {
            return Err(Status::failed_precondition(
                "replication targets a retired receiver session",
            ));
        }
        let peers = self.peers.read().await;
        let expected = peers
            .get(sender)
            .ok_or_else(|| Status::failed_precondition("sender session has not been admitted"))?;
        if expected.as_str() != sender_session {
            return Err(Status::failed_precondition(
                "replication originates from a retired sender session",
            ));
        }
        Ok(())
    }
}

pub struct AgentService<S, E> {
    store: Arc<S>,
    runtime: Arc<PodRuntime>,
    data_plane: RuntimeDataPlane,
    coordinator: Arc<Coordinator<S, E>>,
    reporter: Arc<AgentReporter<S>>,
    sessions: Arc<SessionRegistry>,
    bearer_token: Arc<str>,
}

impl<S, E> Clone for AgentService<S, E> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            runtime: self.runtime.clone(),
            data_plane: self.data_plane.clone(),
            coordinator: self.coordinator.clone(),
            reporter: self.reporter.clone(),
            sessions: self.sessions.clone(),
            bearer_token: self.bearer_token.clone(),
        }
    }
}

impl<S, E> AgentService<S, E>
where
    S: AgentStore + 'static,
    E: RuntimeEffectExecutor + 'static,
{
    pub fn new(
        store: Arc<S>,
        runtime: Arc<PodRuntime>,
        executor: Arc<E>,
        bearer_token: impl Into<Arc<str>>,
    ) -> Result<Self> {
        let bearer_token = bearer_token.into();
        if bearer_token.is_empty() {
            return Err(AgentError::CommandRejected(
                "agent bearer token must not be empty".into(),
            ));
        }
        let reporter = Arc::new(AgentReporter::new(store.clone()));
        let sessions = Arc::new(SessionRegistry::new(reporter.session().id().clone()));
        Ok(Self {
            coordinator: Arc::new(Coordinator::new(store.clone(), executor)),
            store,
            data_plane: runtime.data_plane(),
            runtime,
            reporter,
            sessions,
            bearer_token,
        })
    }

    pub fn sessions(&self) -> &Arc<SessionRegistry> {
        &self.sessions
    }

    pub async fn serve(
        self,
        control_address: SocketAddr,
        replication_address: SocketAddr,
        ready: watch::Sender<bool>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let control_listener = TcpListener::bind(control_address).await?;
        let replication_listener = TcpListener::bind(replication_address).await?;
        self.coordinator
            .open_runtime(OpenMode::Existing, self.sessions.local_session())
            .await?;
        ready.send_replace(true);

        let control_service = self.clone();
        let peer_service = self.clone();
        let replication_service = self;
        let mut replication_shutdown = shutdown.clone();
        let control = tonic::transport::Server::builder()
            .add_service(proto::agent_control_server::AgentControlServer::new(
                control_service,
            ))
            .add_service(proto::replica_peer_server::ReplicaPeerServer::new(
                peer_service,
            ))
            .serve_with_incoming_shutdown(TcpListenerStream::new(control_listener), async move {
                let _ = shutdown.wait_for(|stopped| *stopped).await;
            });
        let replication = tonic::transport::Server::builder()
            .add_service(proto::replication_data_server::ReplicationDataServer::new(
                replication_service,
            ))
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(replication_listener),
                async move {
                    let _ = replication_shutdown.wait_for(|stopped| *stopped).await;
                },
            );
        tokio::try_join!(control, replication)
            .map(|_| ())
            .map_err(|error| AgentError::CommandRejected(error.to_string()))
    }

    fn authorize<T>(&self, request: &Request<T>) -> std::result::Result<(), Status> {
        let expected = format!("Bearer {}", self.bearer_token);
        let observed = request
            .metadata()
            .get(AUTHORIZATION_HEADER)
            .and_then(|value| value.to_str().ok());
        if observed != Some(expected.as_str()) {
            return Err(Status::unauthenticated("invalid agent credentials"));
        }
        Ok(())
    }

    async fn get_status_inner(
        &self,
        request: proto::GetAgentStatusRequest,
    ) -> std::result::Result<proto::AgentStatusReport, Status> {
        if request.protocol_version != kuberic_protocol::PROTOCOL_VERSION {
            return Err(Status::failed_precondition("unsupported protocol version"));
        }
        let state = self.store.load_state().await.map_err(status_from_agent)?;
        if request.resource_uid != state.identity.resource_uid.as_str()
            || request.replica_id != state.identity.local_identity.replica_id.value()
            || request.expected_instance_id != state.identity.local_identity.instance_id.as_str()
        {
            return Err(Status::failed_precondition(
                "status request targets another replica incarnation",
            ));
        }
        self.reporter
            .report(&self.runtime)
            .await
            .map_err(status_from_agent)
    }

    async fn execute_inner(
        &self,
        request: proto::ExecuteCommandRequest,
    ) -> std::result::Result<proto::ExecuteCommandResponse, Status> {
        let command = normalize_execute_request(request)
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        let state = self.store.load_state().await.map_err(status_from_agent)?;
        if command.resource_uid != state.identity.resource_uid
            || command.target != state.identity.local_identity
        {
            return Err(Status::failed_precondition(
                "command targets another durable replica identity",
            ));
        }
        match command.command {
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
            ProtocolCommand::EnsureConfiguration(command) => {
                self.coordinator
                    .ensure_configuration(*command)
                    .await
                    .map_err(status_from_agent)?;
            }
        }
        Ok(proto::ExecuteCommandResponse {
            protocol_version: kuberic_protocol::PROTOCOL_VERSION,
            observation: Some(
                self.reporter
                    .report(&self.runtime)
                    .await
                    .map_err(status_from_agent)?,
            ),
        })
    }
}

#[tonic::async_trait]
impl<S, E> proto::agent_control_server::AgentControl for AgentService<S, E>
where
    S: AgentStore + 'static,
    E: RuntimeEffectExecutor + 'static,
{
    async fn get_status(
        &self,
        request: Request<proto::GetAgentStatusRequest>,
    ) -> std::result::Result<Response<proto::AgentStatusReport>, Status> {
        self.authorize(&request)?;
        Ok(Response::new(
            self.get_status_inner(request.into_inner()).await?,
        ))
    }

    async fn execute(
        &self,
        request: Request<proto::ExecuteCommandRequest>,
    ) -> std::result::Result<Response<proto::ExecuteCommandResponse>, Status> {
        self.authorize(&request)?;
        Ok(Response::new(
            self.execute_inner(request.into_inner()).await?,
        ))
    }
}

#[tonic::async_trait]
impl<S, E> proto::replica_peer_server::ReplicaPeer for AgentService<S, E>
where
    S: AgentStore + 'static,
    E: RuntimeEffectExecutor + 'static,
{
    async fn get_status(
        &self,
        request: Request<proto::GetAgentStatusRequest>,
    ) -> std::result::Result<Response<proto::AgentStatusReport>, Status> {
        self.authorize(&request)?;
        Ok(Response::new(
            self.get_status_inner(request.into_inner()).await?,
        ))
    }

    async fn execute(
        &self,
        request: Request<proto::ExecuteCommandRequest>,
    ) -> std::result::Result<Response<proto::ExecuteCommandResponse>, Status> {
        self.authorize(&request)?;
        Ok(Response::new(
            self.execute_inner(request.into_inner()).await?,
        ))
    }
}

type ReplicationResponseStream =
    Pin<Box<dyn Stream<Item = std::result::Result<proto::ReplicationAck, Status>> + Send>>;
type CopyResponseStream =
    Pin<Box<dyn Stream<Item = std::result::Result<proto::CopyAck, Status>> + Send>>;

#[tonic::async_trait]
impl<S, E> proto::replication_data_server::ReplicationData for AgentService<S, E>
where
    S: AgentStore + 'static,
    E: RuntimeEffectExecutor + 'static,
{
    type ReplicateStream = ReplicationResponseStream;
    type BuildStream = CopyResponseStream;

    async fn replicate(
        &self,
        request: Request<tonic::Streaming<proto::ReplicationItem>>,
    ) -> std::result::Result<Response<Self::ReplicateStream>, Status> {
        self.authorize(&request)?;
        let mut incoming = request.into_inner();
        let data_plane = self.data_plane.clone();
        let sessions = self.sessions.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            while let Some(item) = incoming.next().await {
                let result = async {
                    let item = item?;
                    let sender_identity: ReplicaIdentity = item
                        .sender
                        .clone()
                        .ok_or_else(|| Status::invalid_argument("missing sender"))?
                        .try_into()
                        .map_err(|error: kuberic_wire::WireError| {
                            Status::invalid_argument(error.to_string())
                        })?;
                    sessions
                        .validate_peer(
                            &sender_identity,
                            &item.sender_session_id,
                            &item.receiver_session_id,
                        )
                        .await?;
                    let sender_session_id = item.sender_session_id.clone();
                    let receiver_session_id = item.receiver_session_id.clone();
                    let mut acknowledgement = data_plane
                        .receive_replication(item)
                        .await
                        .map_err(status_from_runtime)?
                        .applied()
                        .await
                        .map_err(status_from_runtime)?;
                    acknowledgement.sender_session_id = sender_session_id;
                    acknowledgement.receiver_session_id = receiver_session_id;
                    Ok(acknowledgement)
                }
                .await;
                if sender.send(result).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn build(
        &self,
        request: Request<tonic::Streaming<proto::CopyItem>>,
    ) -> std::result::Result<Response<Self::BuildStream>, Status> {
        self.authorize(&request)?;
        let mut incoming = request.into_inner();
        let data_plane = self.data_plane.clone();
        let sessions = self.sessions.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            while let Some(item) = incoming.next().await {
                let result = async {
                    let item = item?;
                    let sender_identity: ReplicaIdentity = item
                        .sender
                        .clone()
                        .ok_or_else(|| Status::invalid_argument("missing sender"))?
                        .try_into()
                        .map_err(|error: kuberic_wire::WireError| {
                            Status::invalid_argument(error.to_string())
                        })?;
                    sessions
                        .validate_peer(
                            &sender_identity,
                            &item.sender_session_id,
                            &item.receiver_session_id,
                        )
                        .await?;
                    let sender_session_id = item.sender_session_id.clone();
                    let receiver_session_id = item.receiver_session_id.clone();
                    let mut acknowledgement = data_plane
                        .receive_copy_item(item)
                        .await
                        .map_err(status_from_runtime)?;
                    acknowledgement.sender_session_id = sender_session_id;
                    acknowledgement.receiver_session_id = receiver_session_id;
                    Ok(acknowledgement)
                }
                .await;
                if sender.send(result).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

fn status_from_agent(error: AgentError) -> Status {
    match error {
        AgentError::EffectConflict(_) | AgentError::CommandRejected(_) => {
            Status::failed_precondition(error.to_string())
        }
        AgentError::SessionRejected(_) => Status::failed_precondition(error.to_string()),
        AgentError::Backpressure(_) => Status::resource_exhausted(error.to_string()),
        AgentError::Runtime(error) => status_from_runtime(error),
        _ => Status::internal(error.to_string()),
    }
}

fn status_from_runtime(error: kuberic_runtime::RuntimeError) -> Status {
    use kuberic_runtime::RuntimeError;
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
