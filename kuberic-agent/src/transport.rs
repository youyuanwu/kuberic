use bytes::Bytes;
use kuberic_protocol::types::{ProcessSessionId, ReplicaId, ReplicaIdentity, ReplicaRole};
use kuberic_runtime::{Result as RuntimeResult, RuntimeError};
use kuberic_runtime_internal::transport::{
    CopyAck, CopyItem, OutboundOperation, ReplicaEndpoint, ReplicationAck, ReplicationItem,
};
use kuberic_wire::{
    normalize_copy_ack, normalize_copy_item, normalize_replication_ack, normalize_replication_item,
    proto,
};
use std::collections::BTreeMap;

use crate::hosting::PodRuntime;
use crate::service::SessionRegistry;
use crate::store::AgentStore;
use crate::{AgentError, Result};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{Mutex, watch};
use tokio_stream::iter;
use tonic::Request;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedMessage<T> {
    pub sequence: u64,
    pub payload: T,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeWindow<T> {
    Retained(Vec<RetainedMessage<T>>),
    FullCopyRequired,
}

#[derive(Debug)]
pub struct ReliableWindow<T> {
    capacity: usize,
    next_sequence: u64,
    acknowledged_sequence: u64,
    retained: BTreeMap<u64, T>,
    cancelled: bool,
    ever_enqueued: bool,
}

impl<T: Clone> ReliableWindow<T> {
    pub fn new(capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(AgentError::Backpressure(
                "reliable send window capacity must be positive".into(),
            ));
        }

        Ok(Self {
            capacity,
            next_sequence: 1,
            acknowledged_sequence: 0,
            retained: BTreeMap::new(),
            cancelled: false,
            ever_enqueued: false,
        })
    }

    pub fn enqueue(&mut self, payload: T) -> Result<RetainedMessage<T>> {
        if self.cancelled {
            return Err(AgentError::SessionRejected(
                "reliable send window is cancelled".into(),
            ));
        }
        if self.retained.len() >= self.capacity {
            return Err(AgentError::Backpressure(
                "reliable send window is full".into(),
            ));
        }
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.ever_enqueued = true;
        self.retained.insert(sequence, payload.clone());
        Ok(RetainedMessage { sequence, payload })
    }

    pub fn acknowledge_through(&mut self, sequence: u64) -> Result<()> {
        if sequence < self.acknowledged_sequence || sequence >= self.next_sequence {
            return Err(AgentError::SessionRejected(
                "acknowledgement is outside the retained send window".into(),
            ));
        }
        self.acknowledged_sequence = sequence;
        self.retained.retain(|retained, _| *retained > sequence);
        Ok(())
    }

    pub fn reconnect_from(&self, sequence: u64) -> ResumeWindow<T> {
        if self.cancelled {
            return ResumeWindow::FullCopyRequired;
        }
        if sequence <= self.acknowledged_sequence {
            return ResumeWindow::Retained(
                self.retained
                    .iter()
                    .map(|(sequence, payload)| RetainedMessage {
                        sequence: *sequence,
                        payload: payload.clone(),
                    })
                    .collect(),
            );
        }
        let first = self
            .retained
            .first_key_value()
            .map(|(sequence, _)| *sequence);
        if first.is_some_and(|first| sequence < first) || sequence >= self.next_sequence {
            return ResumeWindow::FullCopyRequired;
        }
        ResumeWindow::Retained(
            self.retained
                .range(sequence..)
                .map(|(sequence, payload)| RetainedMessage {
                    sequence: *sequence,
                    payload: payload.clone(),
                })
                .collect(),
        )
    }

    pub fn retained(&self) -> Vec<RetainedMessage<T>> {
        self.retained
            .iter()
            .map(|(sequence, payload)| RetainedMessage {
                sequence: *sequence,
                payload: payload.clone(),
            })
            .collect()
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.retained.clear();
    }
}

impl ReliableWindow<ReplicationItem> {
    pub fn catch_up_capability(&self) -> Option<i64> {
        self.retained.values().map(|item| item.lsn).min()
    }

    pub fn reconnect_from_lsn(&self, lsn: i64) -> ResumeWindow<ReplicationItem> {
        if self.cancelled {
            return ResumeWindow::FullCopyRequired;
        }
        let Some(first_lsn) = self.catch_up_capability() else {
            return if self.ever_enqueued || lsn > 0 {
                ResumeWindow::FullCopyRequired
            } else {
                ResumeWindow::Retained(Vec::new())
            };
        };
        if lsn < first_lsn {
            return ResumeWindow::FullCopyRequired;
        }
        ResumeWindow::Retained(
            self.retained
                .iter()
                .filter(|(_, item)| item.lsn >= lsn)
                .map(|(sequence, payload)| RetainedMessage {
                    sequence: *sequence,
                    payload: payload.clone(),
                })
                .collect(),
        )
    }
}

#[derive(Debug)]
pub enum RoleTransportState {
    None,
    Primary {
        sessions: BTreeMap<ReplicaIdentity, ReliableWindow<ReplicationItem>>,
    },
    Secondary {
        source: ReplicaIdentity,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueuedOutbound {
    Replication {
        receiver: ReplicaIdentity,
        sequence: u64,
        item: proto::ReplicationItem,
    },
    Copy {
        receiver: ReplicaIdentity,
        sequence: u64,
        item: proto::CopyItem,
    },
    Build(ReplicaEndpoint),
    Remove(ReplicaId),
}

struct PeerWindows {
    session: ProcessSessionId,
    replication: ReliableWindow<ReplicationItem>,
    copy: ReliableWindow<CopyItem>,
}

pub struct ReliableTransport {
    local_session: ProcessSessionId,
    capacity: usize,
    peers: BTreeMap<ReplicaIdentity, PeerWindows>,
}

#[async_trait]
pub trait OutboundDispatcher: Send + Sync {
    async fn dispatch(&self, outbound: QueuedOutbound) -> Result<()>;
}

pub trait ReplicaEndpointResolver: Send + Sync {
    fn control_endpoint(&self, identity: &ReplicaIdentity) -> String;

    fn replication_endpoint(&self, identity: &ReplicaIdentity) -> String;
}

#[derive(Debug, Clone)]
pub struct KubernetesDnsResolver {
    set_name: Arc<str>,
    namespace: Arc<str>,
}

impl KubernetesDnsResolver {
    pub fn new(set_name: impl Into<Arc<str>>, namespace: impl Into<Arc<str>>) -> Self {
        Self {
            set_name: set_name.into(),
            namespace: namespace.into(),
        }
    }

    fn host(&self, identity: &ReplicaIdentity) -> String {
        format!(
            "{}-{}.{}-peer.{}.svc",
            self.set_name,
            identity.replica_id.value(),
            self.set_name,
            self.namespace
        )
    }
}

impl ReplicaEndpointResolver for KubernetesDnsResolver {
    fn control_endpoint(&self, identity: &ReplicaIdentity) -> String {
        format!("http://{}:50051", self.host(identity))
    }

    fn replication_endpoint(&self, identity: &ReplicaIdentity) -> String {
        format!("http://{}:50052", self.host(identity))
    }
}

pub struct GrpcOutboundDispatcher<R> {
    runtime: Arc<PodRuntime>,
    transport: Arc<Mutex<ReliableTransport>>,
    resolver: Arc<R>,
    resource_uid: Arc<str>,
    bearer_token: Arc<str>,
    deadline: std::time::Duration,
}

impl<R> GrpcOutboundDispatcher<R>
where
    R: ReplicaEndpointResolver,
{
    pub fn new(
        runtime: Arc<PodRuntime>,
        transport: Arc<Mutex<ReliableTransport>>,
        resolver: Arc<R>,
        resource_uid: impl Into<Arc<str>>,
        bearer_token: impl Into<Arc<str>>,
        deadline: std::time::Duration,
    ) -> Result<Self> {
        let bearer_token = bearer_token.into();
        if bearer_token.is_empty() {
            return Err(AgentError::CommandRejected(
                "agent bearer token must not be empty".into(),
            ));
        }
        Ok(Self {
            runtime,
            transport,
            resolver,
            resource_uid: resource_uid.into(),
            bearer_token,
            deadline,
        })
    }

    pub async fn peer_session(&self, receiver: &ReplicaIdentity) -> Result<ProcessSessionId> {
        let endpoint = self.resolver.control_endpoint(receiver);
        let mut client = tokio::time::timeout(
            self.deadline,
            proto::agent_control_client::AgentControlClient::connect(endpoint),
        )
        .await
        .map_err(|_| AgentError::SessionRejected("peer status connection timed out".into()))?
        .map_err(|error| AgentError::SessionRejected(error.to_string()))?;
        let mut request = Request::new(proto::GetAgentStatusRequest {
            protocol_version: kuberic_protocol::PROTOCOL_VERSION,
            resource_uid: self.resource_uid.to_string(),
            replica_id: receiver.replica_id.value(),
            expected_instance_id: receiver.instance_id.to_string(),
        });
        add_bearer_token(&mut request, &self.bearer_token)?;
        let report = tokio::time::timeout(self.deadline, client.get_status(request))
            .await
            .map_err(|_| AgentError::SessionRejected("peer status request timed out".into()))?
            .map_err(|error| AgentError::SessionRejected(error.to_string()))?
            .into_inner();
        kuberic_wire::validate_agent_status_report(&report)
            .map_err(|error| AgentError::SessionRejected(error.to_string()))?;
        let observed = report
            .identity
            .clone()
            .ok_or_else(|| AgentError::SessionRejected("peer is not initialized".into()))?;
        let observed: ReplicaIdentity =
            observed
                .try_into()
                .map_err(|error: kuberic_wire::WireError| {
                    AgentError::SessionRejected(error.to_string())
                })?;
        if observed != *receiver {
            return Err(AgentError::SessionRejected(
                "peer status returned another exact identity".into(),
            ));
        }
        Ok(ProcessSessionId::new(report.process_session_id))
    }
}

#[async_trait]
impl<R> OutboundDispatcher for GrpcOutboundDispatcher<R>
where
    R: ReplicaEndpointResolver + 'static,
{
    async fn dispatch(&self, outbound: QueuedOutbound) -> Result<()> {
        match outbound {
            QueuedOutbound::Replication {
                receiver,
                sequence: _,
                mut item,
            } => {
                item.receiver_session_id = self.peer_session(&receiver).await?.to_string();
                let endpoint = self.resolver.replication_endpoint(&receiver);
                let mut client = tokio::time::timeout(
                    self.deadline,
                    proto::replication_data_client::ReplicationDataClient::connect(endpoint),
                )
                .await
                .map_err(|_| {
                    AgentError::SessionRejected("replication connection timed out".into())
                })?
                .map_err(|error| AgentError::SessionRejected(error.to_string()))?;
                let mut request = Request::new(iter([item]));
                add_bearer_token(&mut request, &self.bearer_token)?;
                let mut acknowledgements =
                    tokio::time::timeout(self.deadline, client.replicate(request))
                        .await
                        .map_err(|_| {
                            AgentError::SessionRejected("replication request timed out".into())
                        })?
                        .map_err(|error| AgentError::SessionRejected(error.to_string()))?
                        .into_inner();
                let acknowledgement =
                    tokio::time::timeout(self.deadline, acknowledgements.message())
                        .await
                        .map_err(|_| {
                            AgentError::SessionRejected(
                                "replication acknowledgement timed out".into(),
                            )
                        })?
                        .map_err(|error| AgentError::SessionRejected(error.to_string()))?
                        .ok_or_else(|| {
                            AgentError::SessionRejected(
                                "replication peer returned no acknowledgement".into(),
                            )
                        })?;
                let applied_lsn = acknowledgement.applied_lsn;
                self.runtime
                    .data_plane()
                    .accept_acknowledgement(acknowledgement)
                    .await?;
                self.transport
                    .lock()
                    .await
                    .acknowledge_replication(&receiver, applied_lsn)
            }
            QueuedOutbound::Copy {
                receiver,
                sequence: _,
                mut item,
            } => {
                item.receiver_session_id = self.peer_session(&receiver).await?.to_string();
                let endpoint = self.resolver.replication_endpoint(&receiver);
                let mut client = tokio::time::timeout(
                    self.deadline,
                    proto::replication_data_client::ReplicationDataClient::connect(endpoint),
                )
                .await
                .map_err(|_| AgentError::SessionRejected("copy connection timed out".into()))?
                .map_err(|error| AgentError::SessionRejected(error.to_string()))?;
                let mut request = Request::new(iter([item]));
                add_bearer_token(&mut request, &self.bearer_token)?;
                let mut acknowledgements =
                    tokio::time::timeout(self.deadline, client.build(request))
                        .await
                        .map_err(|_| AgentError::SessionRejected("copy request timed out".into()))?
                        .map_err(|error| AgentError::SessionRejected(error.to_string()))?
                        .into_inner();
                let acknowledgement =
                    tokio::time::timeout(self.deadline, acknowledgements.message())
                        .await
                        .map_err(|_| {
                            AgentError::SessionRejected("copy acknowledgement timed out".into())
                        })?
                        .map_err(|error| AgentError::SessionRejected(error.to_string()))?
                        .ok_or_else(|| {
                            AgentError::SessionRejected(
                                "copy peer returned no acknowledgement".into(),
                            )
                        })?;
                let sequence = acknowledgement.sequence;
                self.runtime
                    .data_plane()
                    .accept_copy_acknowledgement(acknowledgement)
                    .await?;
                self.transport
                    .lock()
                    .await
                    .acknowledge_copy(&receiver, sequence)
            }
            QueuedOutbound::Build(_) => Err(AgentError::CommandRejected(
                "replica build orchestration requires an admitted build authority".into(),
            )),
            QueuedOutbound::Remove(replica_id) => {
                let receiver = self
                    .transport
                    .lock()
                    .await
                    .peers
                    .keys()
                    .find(|identity| identity.replica_id == replica_id)
                    .cloned();
                if let Some(receiver) = receiver {
                    self.transport.lock().await.retire_peer(&receiver);
                }
                Ok(())
            }
        }
    }
}

fn add_bearer_token<T>(request: &mut Request<T>, token: &str) -> Result<()> {
    let value = format!("Bearer {token}")
        .parse()
        .map_err(|error| AgentError::CommandRejected(format!("invalid bearer token: {error}")))?;
    request.metadata_mut().insert("authorization", value);
    Ok(())
}

pub async fn run_outbound<D: OutboundDispatcher + 'static>(
    runtime: Arc<PodRuntime>,
    transport: Arc<Mutex<ReliableTransport>>,
    dispatcher: Arc<D>,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let data_plane = runtime.data_plane();
    let mut deliveries = tokio::task::JoinSet::new();
    loop {
        let mut receive_shutdown = shutdown.clone();
        tokio::select! {
            _ = wait_for_shutdown_signal(&mut receive_shutdown) => {
                deliveries.abort_all();
                return Ok(());
            },
            completed = deliveries.join_next(), if !deliveries.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::warn!(%error, "outbound delivery task failed");
                }
            }
            outbound = data_plane.next_domain_outbound() => {
                let Some(outbound) = outbound else {
                    deliveries.abort_all();
                    return Ok(());
                };
                deliveries.spawn(deliver_outbound(
                    transport.clone(),
                    dispatcher.clone(),
                    outbound,
                    shutdown.clone(),
                ));
            }
        }
    }
}

async fn deliver_outbound<D: OutboundDispatcher>(
    transport: Arc<Mutex<ReliableTransport>>,
    dispatcher: Arc<D>,
    outbound: OutboundOperation,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let queued = loop {
        match transport.lock().await.queue(outbound.clone()) {
            Ok(queued) => break queued,
            Err(AgentError::SessionRejected(_) | AgentError::Backpressure(_)) => {
                let mut retry_shutdown = shutdown.clone();
                tokio::select! {
                    _ = wait_for_shutdown_signal(&mut retry_shutdown) => return Ok(()),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                }
            }
            Err(error) => return Err(error),
        }
    };
    loop {
        let mut dispatch_shutdown = shutdown.clone();
        let result = tokio::select! {
            biased;
            _ = wait_for_shutdown_signal(&mut dispatch_shutdown) => return Ok(()),
            result = dispatcher.dispatch(queued.clone()) => result,
        };
        match result {
            Ok(()) => return Ok(()),
            Err(AgentError::SessionRejected(_) | AgentError::Backpressure(_)) => {
                tracing::warn!("outbound peer unavailable; retaining message for retry");
                let mut retry_shutdown = shutdown.clone();
                tokio::select! {
                    _ = wait_for_shutdown_signal(&mut retry_shutdown) => return Ok(()),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                }
            }
            Err(error) => return Err(error),
        }
    }
}

pub async fn run_peer_discovery<S, R>(
    local: ReplicaIdentity,
    store: Arc<S>,
    transport: Arc<Mutex<ReliableTransport>>,
    dispatcher: Arc<GrpcOutboundDispatcher<R>>,
    sessions: Arc<SessionRegistry>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()>
where
    S: AgentStore + 'static,
    R: ReplicaEndpointResolver + 'static,
{
    loop {
        if *shutdown.borrow_and_update() {
            return Ok(());
        }
        if let Some(configuration) = store.load_state().await?.current_configuration {
            for member in configuration
                .members
                .into_iter()
                .filter(|member| member.identity != local)
            {
                if let Ok(session) = dispatcher.peer_session(&member.identity).await {
                    sessions
                        .register_peer(member.identity.clone(), session.clone())
                        .await;
                    transport
                        .lock()
                        .await
                        .admit_peer(member.identity, session)?;
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow_and_update() {
                    return Ok(());
                }
            }
        }
    }
}

async fn wait_for_shutdown_signal(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

impl ReliableTransport {
    pub fn new(local_session: ProcessSessionId, capacity: usize) -> Result<Self> {
        ReliableWindow::<ReplicationItem>::new(capacity)?;
        Ok(Self {
            local_session,
            capacity,
            peers: BTreeMap::new(),
        })
    }

    pub fn admit_peer(
        &mut self,
        identity: ReplicaIdentity,
        session: ProcessSessionId,
    ) -> Result<()> {
        if self
            .peers
            .get(&identity)
            .is_some_and(|peer| peer.session == session)
        {
            return Ok(());
        }
        self.peers.insert(
            identity,
            PeerWindows {
                session,
                replication: ReliableWindow::new(self.capacity)?,
                copy: ReliableWindow::new(self.capacity)?,
            },
        );
        Ok(())
    }

    pub fn queue(&mut self, outbound: OutboundOperation) -> Result<QueuedOutbound> {
        match outbound {
            OutboundOperation::Replication(item) => {
                let receiver = item.receiver.clone();
                let peer = self.peers.get_mut(&receiver).ok_or_else(|| {
                    AgentError::SessionRejected("replication peer is not admitted".into())
                })?;
                let retained = peer.replication.enqueue(item)?;
                Ok(QueuedOutbound::Replication {
                    receiver,
                    sequence: retained.sequence,
                    item: replication_to_session_proto(
                        retained.payload,
                        self.local_session.as_str(),
                        peer.session.as_str(),
                    ),
                })
            }
            OutboundOperation::Copy(item) => {
                let receiver = item.receiver.clone();
                let peer = self.peers.get_mut(&receiver).ok_or_else(|| {
                    AgentError::SessionRejected("copy peer is not admitted".into())
                })?;
                let retained = peer.copy.enqueue(item)?;
                Ok(QueuedOutbound::Copy {
                    receiver,
                    sequence: retained.sequence,
                    item: copy_to_session_proto(
                        retained.payload,
                        self.local_session.as_str(),
                        peer.session.as_str(),
                    ),
                })
            }
            OutboundOperation::Build(replica) => Ok(QueuedOutbound::Build(replica)),
            OutboundOperation::Remove(replica_id) => Ok(QueuedOutbound::Remove(replica_id)),
        }
    }

    pub fn acknowledge_replication(
        &mut self,
        receiver: &ReplicaIdentity,
        applied_lsn: i64,
    ) -> Result<()> {
        let peer = self.peers.get_mut(receiver).ok_or_else(|| {
            AgentError::SessionRejected("replication peer is not admitted".into())
        })?;
        if let Some(sequence) = peer
            .replication
            .retained
            .iter()
            .filter(|(_, item)| item.lsn <= applied_lsn)
            .map(|(sequence, _)| *sequence)
            .max()
        {
            peer.replication.acknowledge_through(sequence)?;
        }
        Ok(())
    }

    pub fn acknowledge_copy(
        &mut self,
        receiver: &ReplicaIdentity,
        item_sequence: u64,
    ) -> Result<()> {
        let peer = self
            .peers
            .get_mut(receiver)
            .ok_or_else(|| AgentError::SessionRejected("copy peer is not admitted".into()))?;
        if let Some(sequence) = peer
            .copy
            .retained
            .iter()
            .filter(|(_, item)| item.sequence <= item_sequence)
            .map(|(sequence, _)| *sequence)
            .max()
        {
            peer.copy.acknowledge_through(sequence)?;
        }
        Ok(())
    }

    pub fn reconnect_replication(
        &self,
        receiver: &ReplicaIdentity,
        from_lsn: i64,
    ) -> Result<ResumeWindow<ReplicationItem>> {
        self.peers
            .get(receiver)
            .ok_or_else(|| AgentError::SessionRejected("replication peer is not admitted".into()))
            .map(|peer| peer.replication.reconnect_from_lsn(from_lsn))
    }

    pub fn retire_peer(&mut self, receiver: &ReplicaIdentity) {
        if let Some(mut peer) = self.peers.remove(receiver) {
            peer.replication.cancel();
            peer.copy.cancel();
        }
    }
}

impl RoleTransportState {
    pub fn transition(&mut self, role: ReplicaRole, source: Option<ReplicaIdentity>) -> Result<()> {
        for window in match self {
            Self::Primary { sessions } => Some(sessions.values_mut()),
            _ => None,
        }
        .into_iter()
        .flatten()
        {
            window.cancel();
        }
        *self = match role {
            ReplicaRole::Primary => Self::Primary {
                sessions: BTreeMap::new(),
            },
            ReplicaRole::ActiveSecondary | ReplicaRole::IdleSecondary => Self::Secondary {
                source: source.ok_or_else(|| {
                    AgentError::SessionRejected(
                        "secondary transport requires an exact primary source".into(),
                    )
                })?,
            },
            ReplicaRole::None => Self::None,
        };
        Ok(())
    }
}

pub fn replication_from_proto(item: proto::ReplicationItem) -> RuntimeResult<ReplicationItem> {
    let item = normalize_replication_item(item)
        .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))?;
    Ok(ReplicationItem {
        sender: item.sender,
        receiver: item.receiver,
        epoch: item.epoch,
        previous_configuration_id: item.previous_configuration_id,
        current_configuration_id: item.current_configuration_id,
        lsn: item.lsn,
        committed_lsn: item.committed_lsn,
        data: Bytes::from(item.data),
    })
}

pub fn replication_to_proto(item: ReplicationItem) -> proto::ReplicationItem {
    proto::ReplicationItem {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        sender: Some(item.sender.into()),
        epoch: Some(item.epoch.into()),
        previous_configuration_id: item
            .previous_configuration_id
            .map_or_else(String::new, |configuration| configuration.to_string()),
        current_configuration_id: item.current_configuration_id.to_string(),
        lsn: item.lsn,
        committed_lsn: item.committed_lsn,
        data: item.data.to_vec(),
        receiver: Some(item.receiver.into()),
        sender_session_id: String::new(),
        receiver_session_id: String::new(),
    }
}

pub fn replication_to_session_proto(
    item: ReplicationItem,
    sender_session: &str,
    receiver_session: &str,
) -> proto::ReplicationItem {
    let mut item = replication_to_proto(item);
    item.sender_session_id = sender_session.to_string();
    item.receiver_session_id = receiver_session.to_string();
    item
}

pub fn replication_ack_from_proto(
    acknowledgement: proto::ReplicationAck,
) -> RuntimeResult<ReplicationAck> {
    let acknowledgement = normalize_replication_ack(acknowledgement)
        .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))?;
    Ok(ReplicationAck {
        sender: acknowledgement.sender,
        receiver: acknowledgement.receiver,
        epoch: acknowledgement.epoch,
        previous_configuration_id: acknowledgement.previous_configuration_id,
        current_configuration_id: acknowledgement.current_configuration_id,
        received_lsn: acknowledgement.received_lsn,
        applied_lsn: acknowledgement.applied_lsn,
        committed_lsn: acknowledgement.committed_lsn,
    })
}

pub fn replication_ack_to_proto(acknowledgement: ReplicationAck) -> proto::ReplicationAck {
    proto::ReplicationAck {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        sender: Some(acknowledgement.sender.into()),
        receiver: Some(acknowledgement.receiver.into()),
        epoch: Some(acknowledgement.epoch.into()),
        previous_configuration_id: acknowledgement
            .previous_configuration_id
            .map_or_else(String::new, |configuration| configuration.to_string()),
        current_configuration_id: acknowledgement.current_configuration_id.to_string(),
        received_lsn: acknowledgement.received_lsn,
        applied_lsn: acknowledgement.applied_lsn,
        committed_lsn: acknowledgement.committed_lsn,
        sender_session_id: String::new(),
        receiver_session_id: String::new(),
    }
}

pub fn copy_from_proto(item: proto::CopyItem) -> RuntimeResult<CopyItem> {
    let item = normalize_copy_item(item)
        .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))?;
    Ok(CopyItem {
        build_id: item.build_id,
        sender: item.sender,
        receiver: item.receiver,
        epoch: item.epoch,
        current_configuration_id: item.current_configuration_id,
        sequence: item.sequence,
        lsn: item.lsn,
        committed_lsn: item.committed_lsn,
        replication_boundary_lsn: item.replication_boundary_lsn,
        final_item: item.final_item,
        snapshot_chunk: item.snapshot_chunk,
        data: Bytes::from(item.data),
    })
}

pub fn copy_to_proto(item: CopyItem) -> proto::CopyItem {
    proto::CopyItem {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        build_id: item.build_id.to_string(),
        sender: Some(item.sender.into()),
        receiver: Some(item.receiver.into()),
        epoch: Some(item.epoch.into()),
        current_configuration_id: item.current_configuration_id.to_string(),
        sequence: item.sequence,
        lsn: item.lsn,
        committed_lsn: item.committed_lsn,
        replication_boundary_lsn: item.replication_boundary_lsn,
        final_item: item.final_item,
        data: item.data.to_vec(),
        snapshot_chunk: item.snapshot_chunk,
        sender_session_id: String::new(),
        receiver_session_id: String::new(),
    }
}

pub fn copy_to_session_proto(
    item: CopyItem,
    sender_session: &str,
    receiver_session: &str,
) -> proto::CopyItem {
    let mut item = copy_to_proto(item);
    item.sender_session_id = sender_session.to_string();
    item.receiver_session_id = receiver_session.to_string();
    item
}

pub fn copy_ack_from_proto(acknowledgement: proto::CopyAck) -> RuntimeResult<CopyAck> {
    let acknowledgement = normalize_copy_ack(acknowledgement)
        .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))?;
    Ok(CopyAck {
        build_id: acknowledgement.build_id,
        sender: acknowledgement.sender,
        receiver: acknowledgement.receiver,
        epoch: acknowledgement.epoch,
        current_configuration_id: acknowledgement.current_configuration_id,
        sequence: acknowledgement.sequence,
        durable_lsn: acknowledgement.durable_lsn,
        replication_boundary_lsn: acknowledgement.replication_boundary_lsn,
        final_item: acknowledgement.final_item,
        snapshot_chunk: acknowledgement.snapshot_chunk,
    })
}

pub fn copy_ack_to_proto(acknowledgement: CopyAck) -> proto::CopyAck {
    proto::CopyAck {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        build_id: acknowledgement.build_id.to_string(),
        sender: Some(acknowledgement.sender.into()),
        receiver: Some(acknowledgement.receiver.into()),
        epoch: Some(acknowledgement.epoch.into()),
        current_configuration_id: acknowledgement.current_configuration_id.to_string(),
        sequence: acknowledgement.sequence,
        durable_lsn: acknowledgement.durable_lsn,
        replication_boundary_lsn: acknowledgement.replication_boundary_lsn,
        final_item: acknowledgement.final_item,
        snapshot_chunk: acknowledgement.snapshot_chunk,
        sender_session_id: String::new(),
        receiver_session_id: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuberic_protocol::types::{AgentGeneration, ConfigurationId, Epoch, ReplicaInstanceId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FlakyDispatcher {
        failed_peer_attempts: AtomicUsize,
        delivered: tokio::sync::mpsc::UnboundedSender<ReplicaId>,
    }

    #[async_trait]
    impl OutboundDispatcher for FlakyDispatcher {
        async fn dispatch(&self, outbound: QueuedOutbound) -> Result<()> {
            let receiver = match outbound {
                QueuedOutbound::Replication { receiver, .. }
                | QueuedOutbound::Copy { receiver, .. } => receiver,
                QueuedOutbound::Build(endpoint) => endpoint.identity,
                QueuedOutbound::Remove(replica_id) => {
                    self.delivered.send(replica_id).unwrap();
                    return Ok(());
                }
            };
            if receiver.replica_id == ReplicaId::new(2)
                && self.failed_peer_attempts.fetch_add(1, Ordering::SeqCst) < 2
            {
                return Err(AgentError::SessionRejected(
                    "injected unavailable peer".into(),
                ));
            }
            self.delivered.send(receiver.replica_id).unwrap();
            Ok(())
        }
    }

    fn identity(replica_id: i64) -> ReplicaIdentity {
        ReplicaIdentity {
            replica_id: ReplicaId::new(replica_id),
            instance_id: ReplicaInstanceId::new(format!("pod-{replica_id}")),
            agent_generation: AgentGeneration::new(format!("generation-{replica_id}")),
        }
    }

    fn outbound(receiver: ReplicaIdentity) -> OutboundOperation {
        OutboundOperation::Replication(ReplicationItem {
            sender: identity(1),
            receiver,
            epoch: Epoch::new(0, 1),
            previous_configuration_id: None,
            current_configuration_id: ConfigurationId::new("configuration"),
            lsn: 1,
            committed_lsn: 0,
            data: Bytes::from_static(b"value"),
        })
    }

    #[tokio::test]
    async fn unavailable_peer_does_not_block_another_peer_delivery() {
        let transport = Arc::new(Mutex::new(
            ReliableTransport::new(ProcessSessionId::new("primary-session"), 8).unwrap(),
        ));
        for replica_id in [2, 3] {
            transport
                .lock()
                .await
                .admit_peer(
                    identity(replica_id),
                    ProcessSessionId::new(format!("session-{replica_id}")),
                )
                .unwrap();
        }
        let (delivered, mut delivered_rx) = tokio::sync::mpsc::unbounded_channel();
        let dispatcher = Arc::new(FlakyDispatcher {
            failed_peer_attempts: AtomicUsize::new(0),
            delivered,
        });
        let (_shutdown, shutdown) = watch::channel(false);
        let unavailable = tokio::spawn(deliver_outbound(
            transport.clone(),
            dispatcher.clone(),
            outbound(identity(2)),
            shutdown.clone(),
        ));
        let available = tokio::spawn(deliver_outbound(
            transport,
            dispatcher,
            outbound(identity(3)),
            shutdown,
        ));

        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(50), delivered_rx.recv())
                .await
                .unwrap(),
            Some(ReplicaId::new(3))
        );
        available.await.unwrap().unwrap();
        unavailable.await.unwrap().unwrap();
        assert_eq!(delivered_rx.recv().await, Some(ReplicaId::new(2)));
    }
}
