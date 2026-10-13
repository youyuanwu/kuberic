use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};

use async_trait::async_trait;
use futures::{Stream, StreamExt, stream};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock, watch};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status};

use crate::application::{CopyChunk, Operation, OperationData, StateProvider};
use crate::control::proto;
use crate::engine::DurableState;
use crate::protocol::public_operations::{
    PUBLIC_V1_PREVIEW_PROTOCOL_VERSION, RemoteBuildTargetRebuildRequired, ReplicaRebuildReason,
    ReplicaRebuildRequired,
};
use crate::protocol::types::{
    ConfigurationDescriptor, Epoch, ProcessSessionId, ReplicaId, ReplicaIdentity, ReplicaRole,
};
use crate::replicator::stream::{OperationStream, ServiceStreams};
use crate::replicator::{
    PrimaryReplicator, ReplicaInformation, ReplicaSetConfiguration, ReplicaSetQuorumMode,
    Replicator, ReplicatorCapabilities, ReplicatorFactory, ReplicatorFactoryContext,
    ReplicatorInterfaces, ReplicatorSettings, StateReplicator,
};
use crate::{Result, RuntimeError};

pub struct StatelessReplicatorFactory {
    storage: Arc<dyn DurableState>,
}

impl StatelessReplicatorFactory {
    pub fn new(storage: Arc<dyn DurableState>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl ReplicatorFactory for StatelessReplicatorFactory {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        state_provider: Option<Arc<dyn StateProvider>>,
        settings: ReplicatorSettings,
    ) -> Result<ReplicatorInterfaces> {
        let binding = context.preview_binding()?.clone();
        if !binding.preview.is_public_v1() || !settings.public_v1 {
            return Err(RuntimeError::Application(
                "stateless preview requires exact Public V1 process and bundle selection".into(),
            ));
        }
        if settings.replication_address.is_empty() {
            return Err(RuntimeError::Application(
                "stateless preview requires a replication address".into(),
            ));
        }
        if parse_bind_address(&settings.replication_address)?
            != context.preview_replication_address()?
        {
            return Err(RuntimeError::Application(
                "stateless preview bundle listener differs from process selection".into(),
            ));
        }
        let provider = state_provider.ok_or_else(|| {
            RuntimeError::Application("stateless preview requires a state provider".into())
        })?;
        let engine = Arc::new(StatelessReplicator::new(
            context.identity()?,
            context.process_session_id()?,
            context,
            provider,
            self.storage.clone(),
            settings,
        ));
        engine
            .self_ref
            .set(Arc::downgrade(&engine))
            .expect("stateless preview self reference is unset");
        Ok(ReplicatorInterfaces::primary(
            engine.clone(),
            Some(engine),
            ReplicatorCapabilities {
                catch_up_specific_quorum: true,
                data_loss_replay:
                    crate::protocol::public_operations::DataLossReplay::ReplaceOnAmbiguity,
            },
        ))
    }
}

#[derive(Clone, PartialEq, Eq)]
struct ConfigurationAuthority {
    epoch: Epoch,
    current: ConfigurationDescriptor,
    previous: Option<ConfigurationDescriptor>,
    peers: BTreeMap<ReplicaIdentity, ReplicaInformation>,
}

#[derive(Clone, PartialEq, Eq)]
struct InboundBuild {
    build_id: crate::protocol::types::OperationId,
    source: ReplicaIdentity,
    source_session: ProcessSessionId,
    epoch: Epoch,
    current_configuration_id: crate::protocol::types::ConfigurationId,
    replication_boundary_lsn: i64,
    catch_up_boundary_lsn: Option<i64>,
    next_sequence: u64,
    copy_finished: bool,
}

struct EngineState {
    opened: bool,
    role: ReplicaRole,
    role_epoch: Option<Epoch>,
    updated_epoch: Option<Epoch>,
    authority: Option<ConfigurationAuthority>,
    retired: BTreeSet<ReplicaIdentity>,
    inbound_build: Option<InboundBuild>,
    applied_lsn: i64,
    committed_lsn: i64,
    ambiguous_suffix: bool,
    closed: bool,
}

struct ListenerOwner {
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<()>>,
}

pub struct StatelessReplicator {
    identity: ReplicaIdentity,
    process_session: ProcessSessionId,
    context: ReplicatorFactoryContext,
    provider: Arc<dyn StateProvider>,
    storage: Arc<dyn DurableState>,
    settings: RwLock<ReplicatorSettings>,
    streams: Arc<ServiceStreams>,
    state: Mutex<EngineState>,
    open_gate: Mutex<()>,
    write_gate: Mutex<()>,
    listener: StdMutex<Option<ListenerOwner>>,
    descendants: StdMutex<Vec<tokio::task::AbortHandle>>,
    self_ref: OnceLock<Weak<StatelessReplicator>>,
}

impl StatelessReplicator {
    fn new(
        identity: ReplicaIdentity,
        process_session: ProcessSessionId,
        context: ReplicatorFactoryContext,
        provider: Arc<dyn StateProvider>,
        storage: Arc<dyn DurableState>,
        settings: ReplicatorSettings,
    ) -> Self {
        Self {
            identity,
            process_session,
            context,
            provider,
            storage,
            settings: RwLock::new(settings),
            streams: Arc::new(ServiceStreams::new()),
            state: Mutex::new(EngineState {
                opened: false,
                role: ReplicaRole::None,
                role_epoch: None,
                updated_epoch: None,
                authority: None,
                retired: BTreeSet::new(),
                inbound_build: None,
                applied_lsn: 0,
                committed_lsn: 0,
                ambiguous_suffix: false,
                closed: false,
            }),
            open_gate: Mutex::new(()),
            write_gate: Mutex::new(()),
            listener: StdMutex::new(None),
            descendants: StdMutex::new(Vec::new()),
            self_ref: OnceLock::new(),
        }
    }

    async fn require_primary(&self) -> Result<ConfigurationAuthority> {
        let state = self.state.lock().await;
        if state.closed {
            return Err(RuntimeError::Closed);
        }
        if !state.opened {
            return Err(RuntimeError::NotOpen);
        }
        if state.role != ReplicaRole::Primary {
            return Err(RuntimeError::NotPrimary);
        }
        if state.ambiguous_suffix {
            return Err(RuntimeError::ReconfigurationPending);
        }
        let authority = state
            .authority
            .clone()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if state.role_epoch != Some(authority.epoch)
            || state.updated_epoch != Some(authority.epoch)
            || authority.current.primary_id != self.identity.replica_id
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(authority)
    }

    async fn publish_progress(&self, ambiguous: bool) -> Result<()> {
        let (applied_lsn, committed_lsn) = {
            let state = self.state.lock().await;
            (state.applied_lsn, state.committed_lsn)
        };
        self.context
            .publish_preview_progress(applied_lsn, committed_lsn, ambiguous)
            .await
    }

    async fn contain_ambiguous_suffix(&self, applied_floor: i64) {
        {
            let mut state = self.state.lock().await;
            state.applied_lsn = state.applied_lsn.max(applied_floor);
            state.ambiguous_suffix = true;
        }
        let _ = self.publish_progress(true).await;
    }

    async fn verify_retained_payload(&self, lsn: i64, data: &[u8]) -> Result<()> {
        let mut retained = self.storage.get_replication_operations(lsn, lsn).await?;
        let operation = retained.next().await.ok_or_else(|| {
            RuntimeError::InvalidReplication("committed watermark lacks retained operation".into())
        })??;
        if operation.lsn != lsn
            || operation.data.as_ref() != data
            || retained.next().await.is_some()
        {
            return Err(RuntimeError::InvalidReplication(
                "committed watermark changed retained operation".into(),
            ));
        }
        Ok(())
    }

    async fn refresh_provider_progress(&self) -> Result<()> {
        let provider_committed = self.provider.last_committed_lsn().await?;
        let progress = self.storage.durable_progress().await?;
        if provider_committed != progress.committed_lsn
            || progress.applied_lsn < progress.committed_lsn
            || progress.committed_lsn < 0
        {
            return Err(RuntimeError::Application(
                "provider and DurableState progress disagree".into(),
            ));
        }
        let mut state = self.state.lock().await;
        state.applied_lsn = progress.applied_lsn;
        state.committed_lsn = progress.committed_lsn;
        state.ambiguous_suffix = progress.applied_lsn > progress.committed_lsn;
        drop(state);
        self.publish_progress(progress.applied_lsn > progress.committed_lsn)
            .await
    }

    async fn accept_replication(
        &self,
        item: proto::ReplicationItem,
    ) -> std::result::Result<proto::ReplicationAck, Status> {
        if item.protocol_version != PUBLIC_V1_PREVIEW_PROTOCOL_VERSION {
            return Err(Status::failed_precondition(
                "stateless preview requires protocol 11",
            ));
        }
        let sender: ReplicaIdentity = item
            .sender
            .clone()
            .ok_or_else(|| Status::invalid_argument("missing sender"))?
            .try_into()
            .map_err(|error: crate::control::WireError| {
                Status::invalid_argument(error.to_string())
            })?;
        let receiver: ReplicaIdentity = item
            .receiver
            .clone()
            .ok_or_else(|| Status::invalid_argument("missing receiver"))?
            .try_into()
            .map_err(|error: crate::control::WireError| {
                Status::invalid_argument(error.to_string())
            })?;
        let epoch: Epoch = item
            .epoch
            .clone()
            .ok_or_else(|| Status::invalid_argument("missing epoch"))?
            .into();
        if receiver != self.identity
            || item.receiver_session_id != self.process_session.as_str()
            || item.lsn <= 0
            || item.committed_lsn < 0
            || item.committed_lsn > item.lsn
        {
            return Err(Status::failed_precondition(
                "replication receiver, session, or progress is stale",
            ));
        }
        let (authority, prior_applied, prior_committed) = {
            let state = self.state.lock().await;
            if state.closed || !state.opened || state.role != ReplicaRole::ActiveSecondary {
                return Err(Status::failed_precondition(
                    "replica is not an admitted active secondary",
                ));
            }
            let authority = state.authority.clone().ok_or_else(|| {
                Status::failed_precondition("static configuration is not admitted")
            })?;
            let peer = authority.peers.get(&sender).ok_or_else(|| {
                Status::failed_precondition("sender is not in the exact configuration")
            })?;
            let expected_previous = authority
                .previous
                .as_ref()
                .map_or_else(String::new, |previous| {
                    previous.configuration_id.to_string()
                });
            if epoch != authority.epoch
                || item.current_configuration_id != authority.current.configuration_id.as_str()
                || item.previous_configuration_id != expected_previous
                || item.sender_session_id != peer.process_session_id.as_str()
            {
                return Err(Status::failed_precondition(
                    "replication authority or predecessor session is stale",
                ));
            }
            (authority, state.applied_lsn, state.committed_lsn)
        };
        if item.committed_lsn < prior_committed || item.lsn > prior_applied.saturating_add(1) {
            return Err(Status::failed_precondition(
                "replication progress is not contiguous",
            ));
        }
        let operation = Operation {
            lsn: item.lsn,
            committed_lsn: prior_committed,
            data: item.data.clone().into(),
        };
        let applied = if item.lsn > prior_applied {
            match self.storage.apply(operation).await {
                Ok(applied) => applied,
                Err(error) => {
                    self.contain_ambiguous_suffix(item.lsn).await;
                    return Err(runtime_status(error));
                }
            }
        } else {
            if let Err(error) = self
                .verify_retained_payload(item.lsn, item.data.as_slice())
                .await
            {
                self.contain_ambiguous_suffix(prior_applied).await;
                return Err(runtime_status(error));
            }
            match self.storage.durable_progress().await {
                Ok(progress) => progress,
                Err(error) => {
                    self.contain_ambiguous_suffix(prior_applied).await;
                    return Err(runtime_status(error));
                }
            }
        };
        if applied.applied_lsn < item.lsn
            || applied.committed_lsn > applied.applied_lsn
            || applied.committed_lsn < prior_committed
        {
            self.contain_ambiguous_suffix(item.lsn).await;
            return Err(Status::failed_precondition(
                "provider returned inconsistent durable apply progress",
            ));
        }
        {
            let mut state = self.state.lock().await;
            if state.authority.as_ref() != Some(&authority)
                || state.role != ReplicaRole::ActiveSecondary
            {
                drop(state);
                self.contain_ambiguous_suffix(item.lsn).await;
                return Err(Status::failed_precondition(
                    "replication authority changed during provider durability",
                ));
            }
            state.applied_lsn = state.applied_lsn.max(applied.applied_lsn);
            state.ambiguous_suffix = state.applied_lsn > state.committed_lsn;
        }
        let ambiguous = {
            let state = self.state.lock().await;
            state.ambiguous_suffix
        };
        if let Err(error) = self.publish_progress(ambiguous).await {
            self.contain_ambiguous_suffix(item.lsn).await;
            return Err(runtime_status(error));
        }
        let committed = if item.committed_lsn > prior_committed {
            match self.storage.commit(item.committed_lsn).await {
                Ok(committed) => committed,
                Err(error) => {
                    self.contain_ambiguous_suffix(item.lsn).await;
                    return Err(runtime_status(error));
                }
            }
        } else {
            applied
        };
        if committed.committed_lsn < item.committed_lsn
            || committed.committed_lsn > committed.applied_lsn
            || committed.applied_lsn < item.lsn
        {
            self.contain_ambiguous_suffix(item.lsn).await;
            return Err(Status::failed_precondition(
                "provider returned inconsistent durable commit progress",
            ));
        }
        {
            let mut state = self.state.lock().await;
            if state.authority.as_ref() != Some(&authority)
                || state.role != ReplicaRole::ActiveSecondary
            {
                drop(state);
                self.contain_ambiguous_suffix(item.lsn).await;
                return Err(Status::failed_precondition(
                    "replication authority changed during provider commit",
                ));
            }
            state.applied_lsn = state.applied_lsn.max(committed.applied_lsn);
            state.committed_lsn = state.committed_lsn.max(committed.committed_lsn);
            state.ambiguous_suffix = state.applied_lsn > state.committed_lsn;
        }
        let ambiguous = {
            let state = self.state.lock().await;
            state.ambiguous_suffix
        };
        self.publish_progress(ambiguous)
            .await
            .map_err(runtime_status)?;
        Ok(proto::ReplicationAck {
            protocol_version: PUBLIC_V1_PREVIEW_PROTOCOL_VERSION,
            sender: Some(sender.into()),
            receiver: Some(receiver.into()),
            epoch: Some(epoch.into()),
            previous_configuration_id: authority
                .previous
                .as_ref()
                .map_or_else(String::new, |previous| {
                    previous.configuration_id.to_string()
                }),
            current_configuration_id: authority.current.configuration_id.to_string(),
            received_lsn: item.lsn,
            applied_lsn: applied.applied_lsn,
            committed_lsn: committed.committed_lsn,
            sender_session_id: item.sender_session_id,
            receiver_session_id: item.receiver_session_id,
        })
    }

    async fn send_replication(
        &self,
        authority: &ConfigurationAuthority,
        peer: &ReplicaInformation,
        operation: &Operation,
        committed_lsn: i64,
    ) -> Result<()> {
        let endpoint = if peer.replication_address.starts_with("http://")
            || peer.replication_address.starts_with("https://")
        {
            peer.replication_address.clone()
        } else {
            format!("http://{}", peer.replication_address)
        };
        let mut client = proto::replication_data_client::ReplicationDataClient::connect(endpoint)
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let item = proto::ReplicationItem {
            protocol_version: PUBLIC_V1_PREVIEW_PROTOCOL_VERSION,
            sender: Some(self.identity.clone().into()),
            receiver: Some(peer.identity.clone().into()),
            epoch: Some(authority.epoch.into()),
            previous_configuration_id: authority
                .previous
                .as_ref()
                .map_or_else(String::new, |previous| {
                    previous.configuration_id.to_string()
                }),
            current_configuration_id: authority.current.configuration_id.to_string(),
            lsn: operation.lsn,
            committed_lsn,
            data: operation.data.to_vec(),
            sender_session_id: self.process_session.to_string(),
            receiver_session_id: peer.process_session_id.to_string(),
        };
        let response = client
            .replicate(Request::new(stream::iter([item])))
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let acknowledgement = response
            .into_inner()
            .next()
            .await
            .ok_or(RuntimeError::WriteCompletionClosed)?
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let acknowledgement_sender: ReplicaIdentity = acknowledgement
            .sender
            .clone()
            .ok_or_else(|| RuntimeError::InvalidReplication("ACK sender is absent".into()))?
            .try_into()
            .map_err(|error: crate::control::WireError| {
                RuntimeError::InvalidReplication(error.to_string())
            })?;
        let acknowledgement_receiver: ReplicaIdentity = acknowledgement
            .receiver
            .clone()
            .ok_or_else(|| RuntimeError::InvalidReplication("ACK receiver is absent".into()))?
            .try_into()
            .map_err(|error: crate::control::WireError| {
                RuntimeError::InvalidReplication(error.to_string())
            })?;
        let acknowledgement_epoch: Epoch = acknowledgement
            .epoch
            .clone()
            .ok_or_else(|| RuntimeError::InvalidReplication("ACK epoch is absent".into()))?
            .into();
        if acknowledgement.protocol_version != PUBLIC_V1_PREVIEW_PROTOCOL_VERSION
            || acknowledgement_sender != self.identity
            || acknowledgement_receiver != peer.identity
            || acknowledgement_epoch != authority.epoch
            || acknowledgement.sender_session_id != self.process_session.as_str()
            || acknowledgement.receiver_session_id != peer.process_session_id.as_str()
            || acknowledgement.current_configuration_id
                != authority.current.configuration_id.as_str()
            || acknowledgement.previous_configuration_id
                != authority
                    .previous
                    .as_ref()
                    .map_or_else(String::new, |previous| {
                        previous.configuration_id.to_string()
                    })
            || acknowledgement.received_lsn != operation.lsn
            || acknowledgement.applied_lsn < operation.lsn
            || acknowledgement.committed_lsn < committed_lsn
        {
            return Err(RuntimeError::InvalidReplication(
                "stateless preview ACK changed exact authority".into(),
            ));
        }
        Ok(())
    }

    async fn catch_up_peer(
        &self,
        authority: &ConfigurationAuthority,
        peer: &ReplicaInformation,
        through_lsn: i64,
        committed_lsn: i64,
    ) -> Result<i64> {
        if peer.current_progress >= through_lsn {
            return Ok(peer.current_progress);
        }
        let first = peer.current_progress.saturating_add(1).max(1);
        let mut retained = self
            .storage
            .get_replication_operations(first, through_lsn)
            .await?;
        let mut expected = first;
        while let Some(operation) = retained.next().await {
            let operation = operation?;
            if operation.lsn != expected || !self.storage.verify_applied(&operation).await? {
                return Err(RuntimeError::ReplicaRebuildRequired(Box::new(
                    ReplicaRebuildRequired::RemoteBuildTarget(Box::new(
                        RemoteBuildTargetRebuildRequired {
                            reason: ReplicaRebuildReason::IncompatibleRetainedHistory,
                            epoch: authority.epoch,
                            target: peer.identity.clone(),
                            target_session: peer.process_session_id.clone(),
                        },
                    )),
                )));
            }
            self.send_replication(
                authority,
                peer,
                &operation,
                committed_lsn.min(operation.lsn),
            )
            .await?;
            expected = expected.saturating_add(1);
        }
        if expected <= through_lsn {
            return Err(RuntimeError::ReplicaRebuildRequired(Box::new(
                ReplicaRebuildRequired::RemoteBuildTarget(Box::new(
                    RemoteBuildTargetRebuildRequired {
                        reason: ReplicaRebuildReason::IncompatibleRetainedHistory,
                        epoch: authority.epoch,
                        target: peer.identity.clone(),
                        target_session: peer.process_session_id.clone(),
                    },
                )),
            )));
        }
        if through_lsn > 0 && committed_lsn >= through_lsn {
            let mut retained = self
                .storage
                .get_replication_operations(through_lsn, through_lsn)
                .await?;
            let operation = retained.next().await.ok_or_else(|| {
                RuntimeError::InvalidReplication(
                    "catch-up watermark lacks its retained operation".into(),
                )
            })??;
            self.send_replication(authority, peer, &operation, committed_lsn)
                .await?;
        }
        Ok(through_lsn)
    }

    async fn settle_applied_suffix(
        &self,
        authority: &ConfigurationAuthority,
    ) -> Result<BTreeMap<ReplicaIdentity, i64>> {
        let (applied_lsn, committed_lsn) = {
            let state = self.state.lock().await;
            (state.applied_lsn, state.committed_lsn)
        };
        let mut peer_progress = BTreeMap::new();
        for peer in authority.peers.values() {
            match self
                .catch_up_peer(authority, peer, applied_lsn, committed_lsn)
                .await
            {
                Ok(progress) => {
                    peer_progress.insert(peer.identity.clone(), progress);
                }
                Err(error @ RuntimeError::ReplicaRebuildRequired(_)) => return Err(error),
                Err(_) => {}
            }
        }
        if applied_lsn > committed_lsn {
            let ready = std::iter::once(self.identity.clone())
                .chain(peer_progress.keys().cloned())
                .collect::<BTreeSet<_>>();
            let quorum = |configuration: &ConfigurationDescriptor| {
                configuration
                    .members
                    .iter()
                    .filter(|member| ready.contains(&member.identity))
                    .count()
                    >= configuration.write_quorum as usize
            };
            if !quorum(&authority.current)
                || authority
                    .previous
                    .as_ref()
                    .is_some_and(|previous| !quorum(previous))
            {
                return Err(RuntimeError::ReconfigurationPending);
            }
            let committed = self.storage.commit(applied_lsn).await?;
            if committed.committed_lsn < applied_lsn
                || committed.committed_lsn > committed.applied_lsn
            {
                self.contain_ambiguous_suffix(applied_lsn).await;
                return Err(RuntimeError::Application(
                    "provider could not settle its applied suffix".into(),
                ));
            }
            {
                let mut state = self.state.lock().await;
                if state.authority.as_ref() != Some(authority) {
                    return Err(RuntimeError::OperationCancelled);
                }
                state.applied_lsn = committed.applied_lsn;
                state.committed_lsn = committed.committed_lsn;
                state.ambiguous_suffix = false;
            }
            self.publish_progress(false).await?;
            for peer in authority.peers.values() {
                if peer_progress.contains_key(&peer.identity) {
                    let retained = self
                        .storage
                        .get_replication_operations(applied_lsn, applied_lsn)
                        .await?;
                    futures::pin_mut!(retained);
                    if let Some(Ok(operation)) = retained.next().await {
                        let _ = self
                            .send_replication(authority, peer, &operation, applied_lsn)
                            .await;
                    }
                }
            }
        }
        Ok(peer_progress)
    }

    async fn send_build(&self, replica: &ReplicaInformation) -> Result<()> {
        let authority = self.require_primary().await?;
        if replica.role != ReplicaRole::IdleSecondary
            || replica.build_id.is_empty()
            || replica.process_session_id.is_empty()
            || replica.replication_address.is_empty()
            || replica.current_progress != crate::application::INVALID_LSN
            || replica.catch_up_capability != crate::application::INVALID_LSN
            || replica
                .storage_incarnation
                .as_ref()
                .is_none_or(|storage| storage.replica != replica.identity)
        {
            return Err(RuntimeError::AuthorityMismatch(
                "build target lacks exact idle build authority".into(),
            ));
        }
        {
            let state = self.state.lock().await;
            if state.retired.contains(&replica.identity)
                || authority
                    .current
                    .members
                    .iter()
                    .any(|member| member.identity == replica.identity)
                || authority.previous.as_ref().is_some_and(|configuration| {
                    configuration
                        .members
                        .iter()
                        .any(|member| member.identity == replica.identity)
                })
            {
                return Err(RuntimeError::AuthorityMismatch(
                    "build target is retired or already in PC/CC".into(),
                ));
            }
        }
        let replication_boundary_lsn = {
            let state = self.state.lock().await;
            state.committed_lsn
        };
        let copy_context = self.provider.get_copy_context().await?;
        let mut copy = self
            .provider
            .get_copy_state(replication_boundary_lsn, copy_context)
            .await?;
        let mut items = Vec::new();
        let mut sequence = 0_u64;
        while let Some(chunk) = copy.next().await {
            items.push(proto::CopyItem {
                protocol_version: PUBLIC_V1_PREVIEW_PROTOCOL_VERSION,
                build_id: replica.build_id.to_string(),
                sender: Some(self.identity.clone().into()),
                receiver: Some(replica.identity.clone().into()),
                epoch: Some(authority.epoch.into()),
                current_configuration_id: authority.current.configuration_id.to_string(),
                sequence,
                lsn: replication_boundary_lsn,
                committed_lsn: replication_boundary_lsn,
                replication_boundary_lsn,
                final_item: false,
                data: chunk?.to_vec(),
                snapshot_chunk: true,
                sender_session_id: self.process_session.to_string(),
                receiver_session_id: replica.process_session_id.to_string(),
                catch_up_boundary_lsn: None,
            });
            sequence = sequence.saturating_add(1);
        }
        items.push(proto::CopyItem {
            protocol_version: PUBLIC_V1_PREVIEW_PROTOCOL_VERSION,
            build_id: replica.build_id.to_string(),
            sender: Some(self.identity.clone().into()),
            receiver: Some(replica.identity.clone().into()),
            epoch: Some(authority.epoch.into()),
            current_configuration_id: authority.current.configuration_id.to_string(),
            sequence,
            lsn: replication_boundary_lsn,
            committed_lsn: replication_boundary_lsn,
            replication_boundary_lsn,
            final_item: false,
            data: Vec::new(),
            snapshot_chunk: false,
            sender_session_id: self.process_session.to_string(),
            receiver_session_id: replica.process_session_id.to_string(),
            catch_up_boundary_lsn: None,
        });
        sequence = sequence.saturating_add(1);
        let catch_up_boundary_lsn = self.storage.durable_progress().await?.committed_lsn;
        if catch_up_boundary_lsn < replication_boundary_lsn {
            return Err(RuntimeError::Application(
                "provider committed progress regressed during build".into(),
            ));
        }
        if catch_up_boundary_lsn > replication_boundary_lsn {
            let mut retained = self
                .storage
                .get_replication_operations(
                    replication_boundary_lsn.saturating_add(1),
                    catch_up_boundary_lsn,
                )
                .await?;
            let mut expected = replication_boundary_lsn.saturating_add(1);
            while let Some(operation) = retained.next().await {
                let operation = operation?;
                if operation.lsn != expected || !self.storage.verify_applied(&operation).await? {
                    return Err(RuntimeError::ReplicaRebuildRequired(Box::new(
                        ReplicaRebuildRequired::RemoteBuildTarget(Box::new(
                            RemoteBuildTargetRebuildRequired {
                                reason: ReplicaRebuildReason::IncompatibleRetainedHistory,
                                epoch: authority.epoch,
                                target: replica.identity.clone(),
                                target_session: replica.process_session_id.clone(),
                            },
                        )),
                    )));
                }
                items.push(proto::CopyItem {
                    protocol_version: PUBLIC_V1_PREVIEW_PROTOCOL_VERSION,
                    build_id: replica.build_id.to_string(),
                    sender: Some(self.identity.clone().into()),
                    receiver: Some(replica.identity.clone().into()),
                    epoch: Some(authority.epoch.into()),
                    current_configuration_id: authority.current.configuration_id.to_string(),
                    sequence,
                    lsn: operation.lsn,
                    committed_lsn: operation.lsn,
                    replication_boundary_lsn,
                    final_item: false,
                    data: operation.data.to_vec(),
                    snapshot_chunk: false,
                    sender_session_id: self.process_session.to_string(),
                    receiver_session_id: replica.process_session_id.to_string(),
                    catch_up_boundary_lsn: Some(catch_up_boundary_lsn),
                });
                sequence = sequence.saturating_add(1);
                expected = expected.saturating_add(1);
            }
            if expected <= catch_up_boundary_lsn {
                return Err(RuntimeError::ReplicaRebuildRequired(Box::new(
                    ReplicaRebuildRequired::RemoteBuildTarget(Box::new(
                        RemoteBuildTargetRebuildRequired {
                            reason: ReplicaRebuildReason::IncompatibleRetainedHistory,
                            epoch: authority.epoch,
                            target: replica.identity.clone(),
                            target_session: replica.process_session_id.clone(),
                        },
                    )),
                )));
            }
        }
        items.push(proto::CopyItem {
            protocol_version: PUBLIC_V1_PREVIEW_PROTOCOL_VERSION,
            build_id: replica.build_id.to_string(),
            sender: Some(self.identity.clone().into()),
            receiver: Some(replica.identity.clone().into()),
            epoch: Some(authority.epoch.into()),
            current_configuration_id: authority.current.configuration_id.to_string(),
            sequence,
            lsn: catch_up_boundary_lsn,
            committed_lsn: catch_up_boundary_lsn,
            replication_boundary_lsn,
            final_item: true,
            data: Vec::new(),
            snapshot_chunk: false,
            sender_session_id: self.process_session.to_string(),
            receiver_session_id: replica.process_session_id.to_string(),
            catch_up_boundary_lsn: Some(catch_up_boundary_lsn),
        });

        let endpoint = if replica.replication_address.starts_with("http://")
            || replica.replication_address.starts_with("https://")
        {
            replica.replication_address.clone()
        } else {
            format!("http://{}", replica.replication_address)
        };
        let mut client = proto::replication_data_client::ReplicationDataClient::connect(endpoint)
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let response = client
            .build(Request::new(stream::iter(items)))
            .await
            .map_err(|_| {
                RuntimeError::ReplicaRebuildRequired(Box::new(
                    ReplicaRebuildRequired::RemoteBuildTarget(Box::new(
                        RemoteBuildTargetRebuildRequired {
                            reason: ReplicaRebuildReason::RemoteCopyConflict,
                            epoch: authority.epoch,
                            target: replica.identity.clone(),
                            target_session: replica.process_session_id.clone(),
                        },
                    )),
                ))
            })?;
        let mut acknowledgements = response.into_inner();
        let mut final_ack = None;
        while let Some(acknowledgement) = acknowledgements.next().await {
            let acknowledgement = acknowledgement.map_err(|_| {
                RuntimeError::ReplicaRebuildRequired(Box::new(
                    ReplicaRebuildRequired::RemoteBuildTarget(Box::new(
                        RemoteBuildTargetRebuildRequired {
                            reason: ReplicaRebuildReason::RemoteCopyConflict,
                            epoch: authority.epoch,
                            target: replica.identity.clone(),
                            target_session: replica.process_session_id.clone(),
                        },
                    )),
                ))
            })?;
            if acknowledgement.protocol_version != PUBLIC_V1_PREVIEW_PROTOCOL_VERSION
                || acknowledgement.build_id != replica.build_id.as_str()
                || acknowledgement.receiver_session_id != replica.process_session_id.as_str()
                || acknowledgement.sender_session_id != self.process_session.as_str()
                || acknowledgement.current_configuration_id
                    != authority.current.configuration_id.as_str()
                || acknowledgement.replication_boundary_lsn != replication_boundary_lsn
            {
                return Err(RuntimeError::InvalidReplication(
                    "build ACK changed exact authority".into(),
                ));
            }
            if acknowledgement.final_item {
                final_ack = Some(acknowledgement);
            }
        }
        let final_ack = final_ack.ok_or(RuntimeError::WriteCompletionClosed)?;
        if final_ack.durable_lsn < catch_up_boundary_lsn
            || final_ack.catch_up_boundary_lsn != Some(catch_up_boundary_lsn)
        {
            return Err(RuntimeError::ReplicaRebuildRequired(Box::new(
                ReplicaRebuildRequired::RemoteBuildTarget(Box::new(
                    RemoteBuildTargetRebuildRequired {
                        reason: ReplicaRebuildReason::IncompatibleRetainedHistory,
                        epoch: authority.epoch,
                        target: replica.identity.clone(),
                        target_session: replica.process_session_id.clone(),
                    },
                )),
            )));
        }
        Ok(())
    }

    async fn accept_copy(
        &self,
        item: proto::CopyItem,
    ) -> std::result::Result<proto::CopyAck, Status> {
        if item.protocol_version != PUBLIC_V1_PREVIEW_PROTOCOL_VERSION {
            return Err(Status::failed_precondition(
                "stateless preview requires protocol 11",
            ));
        }
        let build_id = crate::protocol::types::OperationId::new(item.build_id.clone());
        let sender: ReplicaIdentity = item
            .sender
            .clone()
            .ok_or_else(|| Status::invalid_argument("missing build sender"))?
            .try_into()
            .map_err(|error: crate::control::WireError| {
                Status::invalid_argument(error.to_string())
            })?;
        let receiver: ReplicaIdentity = item
            .receiver
            .clone()
            .ok_or_else(|| Status::invalid_argument("missing build receiver"))?
            .try_into()
            .map_err(|error: crate::control::WireError| {
                Status::invalid_argument(error.to_string())
            })?;
        let epoch: Epoch = item
            .epoch
            .clone()
            .ok_or_else(|| Status::invalid_argument("missing build epoch"))?
            .into();
        if build_id.is_empty()
            || receiver != self.identity
            || item.receiver_session_id != self.process_session.as_str()
            || item.sender_session_id.is_empty()
            || item.current_configuration_id.is_empty()
            || item.replication_boundary_lsn < 0
            || item.committed_lsn < 0
        {
            return Err(Status::failed_precondition(
                "build identity, session, or progress is stale",
            ));
        }
        let current_configuration_id =
            crate::protocol::types::ConfigurationId::new(item.current_configuration_id.clone());
        let mut build = {
            let mut state = self.state.lock().await;
            if state.closed
                || !state.opened
                || state.role != ReplicaRole::IdleSecondary
                || state.role_epoch != Some(epoch)
                || state.updated_epoch != Some(epoch)
                || state.retired.contains(&receiver)
            {
                return Err(Status::failed_precondition(
                    "build target is not an admitted idle secondary",
                ));
            }
            let candidate = InboundBuild {
                build_id: build_id.clone(),
                source: sender.clone(),
                source_session: ProcessSessionId::new(item.sender_session_id.clone()),
                epoch,
                current_configuration_id: current_configuration_id.clone(),
                replication_boundary_lsn: item.replication_boundary_lsn,
                catch_up_boundary_lsn: item.catch_up_boundary_lsn,
                next_sequence: 0,
                copy_finished: false,
            };
            match &state.inbound_build {
                Some(existing)
                    if existing.build_id != candidate.build_id
                        || existing.source != candidate.source
                        || existing.source_session != candidate.source_session
                        || existing.epoch != candidate.epoch
                        || existing.current_configuration_id
                            != candidate.current_configuration_id
                        || existing.replication_boundary_lsn
                            != candidate.replication_boundary_lsn =>
                {
                    return Err(Status::failed_precondition(
                        "another exact build attempt owns the target",
                    ));
                }
                Some(existing) => existing.clone(),
                None => {
                    state.inbound_build = Some(candidate.clone());
                    candidate
                }
            }
        };
        if item.sequence > build.next_sequence {
            return Err(Status::failed_precondition("build sequence has a gap"));
        }
        if item.sequence < build.next_sequence {
            if item.snapshot_chunk {
                let chunk = CopyChunk {
                    data: item.data.clone().into(),
                };
                if !self
                    .storage
                    .verify_copy_chunk(&build_id, item.sequence, &chunk)
                    .await
                    .map_err(runtime_status)?
                {
                    return Err(Status::failed_precondition(
                        "replayed copy chunk changed bytes",
                    ));
                }
            } else if !item.final_item && item.lsn > build.replication_boundary_lsn {
                let operation = Operation {
                    lsn: item.lsn,
                    committed_lsn: item.committed_lsn.saturating_sub(1),
                    data: item.data.clone().into(),
                };
                if !self
                    .storage
                    .verify_applied(&operation)
                    .await
                    .map_err(runtime_status)?
                {
                    return Err(Status::failed_precondition(
                        "replayed retained build operation changed bytes",
                    ));
                }
            }
        } else if item.snapshot_chunk {
            if build.copy_finished {
                return Err(Status::failed_precondition(
                    "snapshot chunk arrived after copy completion",
                ));
            }
            self.storage
                .apply_copy_chunk(
                    &build_id,
                    item.sequence,
                    CopyChunk {
                        data: item.data.clone().into(),
                    },
                )
                .await
                .map_err(runtime_status)?;
            build.next_sequence = build.next_sequence.saturating_add(1);
        } else {
            if !build.copy_finished {
                let progress = self
                    .storage
                    .finish_copy(
                        &build_id,
                        build.replication_boundary_lsn,
                        build.replication_boundary_lsn,
                    )
                    .await
                    .map_err(runtime_status)?;
                if progress.applied_lsn < build.replication_boundary_lsn
                    || progress.committed_lsn < build.replication_boundary_lsn
                {
                    return Err(Status::failed_precondition(
                        "copy completion did not reach the frozen boundary",
                    ));
                }
                build.copy_finished = true;
            }
            if !item.final_item && item.lsn > build.replication_boundary_lsn {
                let operation = Operation {
                    lsn: item.lsn,
                    committed_lsn: item.committed_lsn.saturating_sub(1),
                    data: item.data.clone().into(),
                };
                let progress = self
                    .storage
                    .apply(operation)
                    .await
                    .map_err(runtime_status)?;
                if progress.applied_lsn < item.lsn {
                    return Err(Status::failed_precondition(
                        "retained build operation was not durable",
                    ));
                }
                let progress = self
                    .storage
                    .commit(item.committed_lsn)
                    .await
                    .map_err(runtime_status)?;
                if progress.committed_lsn < item.committed_lsn {
                    return Err(Status::failed_precondition(
                        "retained build operation was not committed",
                    ));
                }
            }
            build.catch_up_boundary_lsn = item.catch_up_boundary_lsn;
            build.next_sequence = build.next_sequence.saturating_add(1);
        }
        let progress = self
            .storage
            .durable_progress()
            .await
            .map_err(runtime_status)?;
        {
            let mut state = self.state.lock().await;
            if item.final_item {
                if item.catch_up_boundary_lsn != Some(item.lsn) || progress.committed_lsn < item.lsn
                {
                    return Err(Status::failed_precondition(
                        "build final admission precedes retained gap closure",
                    ));
                }
                state.applied_lsn = progress.applied_lsn;
                state.committed_lsn = progress.committed_lsn;
                state.ambiguous_suffix = progress.applied_lsn > progress.committed_lsn;
                state.inbound_build = None;
            } else {
                state.inbound_build = Some(build.clone());
            }
        }
        self.publish_progress(progress.applied_lsn > progress.committed_lsn)
            .await
            .map_err(runtime_status)?;
        Ok(proto::CopyAck {
            protocol_version: PUBLIC_V1_PREVIEW_PROTOCOL_VERSION,
            build_id: build_id.to_string(),
            sender: Some(sender.into()),
            receiver: Some(receiver.into()),
            epoch: Some(epoch.into()),
            current_configuration_id: current_configuration_id.to_string(),
            sequence: item.sequence,
            durable_lsn: progress.committed_lsn,
            replication_boundary_lsn: item.replication_boundary_lsn,
            final_item: item.final_item,
            snapshot_chunk: item.snapshot_chunk,
            sender_session_id: item.sender_session_id,
            receiver_session_id: item.receiver_session_id,
            catch_up_boundary_lsn: item.catch_up_boundary_lsn,
        })
    }

    fn abort_listener(&self) {
        if let Some(owner) = self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            owner.shutdown.send_replace(true);
            owner.task.abort();
        }
        for descendant in self
            .descendants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            descendant.abort();
        }
    }

    async fn fence_authority(&self) {
        let mut state = self.state.lock().await;
        state.authority = None;
    }
}

#[async_trait]
impl Replicator for StatelessReplicator {
    async fn open(&self) -> Result<String> {
        let _open = self.open_gate.lock().await;
        {
            let state = self.state.lock().await;
            if state.closed {
                return Err(RuntimeError::Closed);
            }
            if state.opened {
                return Ok(self.settings.read().await.replication_address.clone());
            }
        }
        let provider_committed = self.provider.last_committed_lsn().await?;
        let progress = self.storage.durable_progress().await?;
        if provider_committed != progress.committed_lsn
            || progress.applied_lsn < progress.committed_lsn
            || progress.committed_lsn < 0
        {
            return Err(RuntimeError::Application(
                "provider and DurableState progress disagree".into(),
            ));
        }
        if progress.applied_lsn > progress.committed_lsn {
            let mut retained = self
                .storage
                .get_replication_operations(
                    progress.committed_lsn.saturating_add(1),
                    progress.applied_lsn,
                )
                .await?;
            let mut expected = progress.committed_lsn.saturating_add(1);
            while let Some(operation) = retained.next().await {
                let operation = operation?;
                if operation.lsn != expected || !self.storage.verify_applied(&operation).await? {
                    return Err(RuntimeError::Application(
                        "provider retained suffix is incomplete or changed".into(),
                    ));
                }
                expected = expected.saturating_add(1);
            }
            if expected <= progress.applied_lsn {
                return Err(RuntimeError::Application(
                    "provider cannot reconstruct its applied suffix".into(),
                ));
            }
        }
        let address = self.settings.read().await.replication_address.clone();
        let bind = parse_bind_address(&address)?;
        let listener = TcpListener::bind(bind)
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let service = PreviewReplicationService {
            engine: self.self_ref.get().cloned().ok_or_else(|| {
                RuntimeError::Application("preview creation is unattached".into())
            })?,
        };
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(proto::replication_data_server::ReplicationDataServer::new(
                    service,
                ))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                    while !*shutdown_rx.borrow_and_update() {
                        if shutdown_rx.changed().await.is_err() {
                            break;
                        }
                    }
                })
                .await
                .map_err(|error| RuntimeError::Application(error.to_string()))
        });
        *self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(ListenerOwner { shutdown, task });
        let mut state = self.state.lock().await;
        if state.closed {
            drop(state);
            self.abort_listener();
            return Err(RuntimeError::Closed);
        }
        state.opened = true;
        state.applied_lsn = progress.applied_lsn;
        state.committed_lsn = progress.committed_lsn;
        state.ambiguous_suffix = progress.applied_lsn > progress.committed_lsn;
        let ambiguous = state.ambiguous_suffix;
        drop(state);
        if let Err(error) = self.publish_progress(ambiguous).await {
            self.abort_listener();
            return Err(error);
        }
        Ok(address)
    }

    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.closed {
            return Err(RuntimeError::Closed);
        }
        if !state.opened {
            return Err(RuntimeError::NotOpen);
        }
        if role == ReplicaRole::None {
            state.role = role;
            state.role_epoch = Some(epoch);
            state.updated_epoch = None;
            state.authority = None;
            state.inbound_build = None;
            return Ok(());
        }
        if let Some(current) = state.role_epoch
            && (epoch.data_loss_number < current.data_loss_number
                || (epoch.data_loss_number == current.data_loss_number
                    && epoch.configuration_number < current.configuration_number))
        {
            state.authority = None;
            return Err(RuntimeError::AuthorityMismatch(
                "role replay regressed the exact epoch".into(),
            ));
        }
        if state.role != ReplicaRole::None
            && (state.role != role || state.role_epoch != Some(epoch))
            && state.updated_epoch != Some(epoch)
            && state.role_epoch != Some(epoch)
        {
            state.authority = None;
            return Err(RuntimeError::ReconfigurationPending);
        }
        if state.role != role || state.role_epoch != Some(epoch) {
            state.authority = None;
            state.inbound_build = None;
        }
        state.role = role;
        state.role_epoch = Some(epoch);
        Ok(())
    }

    async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        let previous_epoch_last_lsn = {
            let state = self.state.lock().await;
            if state.closed {
                return Err(RuntimeError::Closed);
            }
            if let Some(current) = state.role_epoch
                && (epoch.data_loss_number < current.data_loss_number
                    || (epoch.data_loss_number == current.data_loss_number
                        && epoch.configuration_number < current.configuration_number))
            {
                return Err(RuntimeError::AuthorityMismatch(
                    "epoch regressed from the exact role replay".into(),
                ));
            }
            if state.updated_epoch == Some(epoch) {
                return Ok(());
            }
            state.committed_lsn
        };
        self.provider
            .update_epoch(epoch, previous_epoch_last_lsn)
            .await?;
        let mut state = self.state.lock().await;
        state.authority = None;
        state.updated_epoch = Some(epoch);
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        let owner = self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.abort_listener();
        self.streams.shutdown();
        {
            let mut state = self.state.lock().await;
            state.closed = true;
            state.role = ReplicaRole::None;
            state.authority = None;
        }
        if let Some(mut owner) = owner {
            owner.shutdown.send_replace(true);
            match tokio::time::timeout(std::time::Duration::from_secs(5), &mut owner.task).await {
                Ok(result) => {
                    result.map_err(|error| RuntimeError::Application(error.to_string()))??
                }
                Err(_) => {
                    owner.task.abort();
                    let _ = owner.task.await;
                    return Err(RuntimeError::OperationCancelled);
                }
            }
        }
        Ok(())
    }

    fn abort(&self) {
        self.streams.shutdown();
        self.abort_listener();
        if let Ok(mut state) = self.state.try_lock() {
            state.closed = true;
            state.role = ReplicaRole::None;
            state.authority = None;
        }
    }

    async fn current_progress(&self) -> Result<i64> {
        let state = self.state.lock().await;
        if state.closed {
            return Err(RuntimeError::Closed);
        }
        Ok(state.applied_lsn)
    }

    async fn catch_up_capability(&self) -> Result<i64> {
        let state = self.state.lock().await;
        if state.closed {
            return Err(RuntimeError::Closed);
        }
        Ok(if state.opened {
            state.committed_lsn
        } else {
            -1
        })
    }
}

#[async_trait]
impl PrimaryReplicator for StatelessReplicator {
    async fn on_data_loss(&self) -> Result<bool> {
        self.fence_authority().await;
        let changed = self.provider.on_data_loss().await?;
        self.refresh_provider_progress().await?;
        Ok(changed)
    }

    async fn update_catch_up_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
        previous: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.install_static_configuration(current, Some(previous))
            .await
    }

    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()> {
        let authority = {
            let state = self.state.lock().await;
            state
                .authority
                .clone()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?
        };
        let peer_progress = self.settle_applied_suffix(&authority).await?;
        let state = self.state.lock().await;
        let ready = std::iter::once(&self.identity)
            .chain(
                authority
                    .peers
                    .values()
                    .filter(|peer| {
                        peer_progress
                            .get(&peer.identity)
                            .copied()
                            .unwrap_or(peer.current_progress)
                            >= state.committed_lsn
                    })
                    .map(|peer| &peer.identity),
            )
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let quorum_ready = |configuration: &ConfigurationDescriptor| {
            configuration
                .members
                .iter()
                .filter(|member| ready.contains(&member.identity))
                .count()
                >= configuration.write_quorum as usize
        };
        let all_ready = 1 + authority
            .peers
            .values()
            .filter(|peer| {
                peer_progress
                    .get(&peer.identity)
                    .copied()
                    .unwrap_or(peer.current_progress)
                    >= state.committed_lsn
            })
            .count();
        let successor_ready = authority
            .peers
            .values()
            .find(|peer| peer.must_catchup)
            .is_none_or(|peer| {
                peer_progress
                    .get(&peer.identity)
                    .copied()
                    .unwrap_or(peer.current_progress)
                    >= state.committed_lsn
            });
        let satisfied = match mode {
            ReplicaSetQuorumMode::WriteQuorum => {
                quorum_ready(&authority.current)
                    && authority.previous.as_ref().is_none_or(quorum_ready)
                    && successor_ready
            }
            ReplicaSetQuorumMode::All => {
                all_ready
                    == authority
                        .current
                        .members
                        .iter()
                        .chain(
                            authority
                                .previous
                                .iter()
                                .flat_map(|previous| previous.members.iter()),
                        )
                        .map(|member| &member.identity)
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
            }
        };
        if !satisfied {
            return Err(RuntimeError::ReconfigurationPending);
        }
        Ok(())
    }

    async fn update_current_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.install_static_configuration(current, None).await
    }

    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()> {
        self.send_build(&replica).await
    }

    async fn remove_replica(&self, replica_id: ReplicaId) -> Result<()> {
        let mut state = self.state.lock().await;
        let removed = state
            .authority
            .as_mut()
            .and_then(|authority| {
                let identity = authority
                    .peers
                    .keys()
                    .find(|identity| identity.replica_id == replica_id)
                    .cloned()?;
                authority.peers.remove(&identity);
                Some(identity)
            })
            .ok_or_else(|| {
                RuntimeError::AuthorityMismatch("removed replica lacks exact authority".into())
            })?;
        state.retired.insert(removed);
        Ok(())
    }
}

impl StatelessReplicator {
    async fn install_static_configuration(
        &self,
        current: ReplicaSetConfiguration,
        previous: Option<ReplicaSetConfiguration>,
    ) -> Result<()> {
        if current.configuration.configuration_id != current.configuration.expected_id()
            || current.configuration.write_quorum == 0
            || current.configuration.write_quorum as usize > current.configuration.members.len()
            || previous.as_ref().is_some_and(|previous| {
                previous.configuration.configuration_id != previous.configuration.expected_id()
                    || previous.configuration.write_quorum == 0
                    || previous.configuration.write_quorum as usize
                        > previous.configuration.members.len()
                    || previous.configuration.epoch >= current.configuration.epoch
            })
        {
            return Err(RuntimeError::AuthorityMismatch(
                "invalid exact static configuration".into(),
            ));
        }
        let local_role = current
            .configuration
            .members
            .iter()
            .find(|member| member.identity == self.identity)
            .map(|member| member.role)
            .ok_or_else(|| {
                RuntimeError::AuthorityMismatch(
                    "local replica is absent from static configuration".into(),
                )
            })?;
        let mut peers = BTreeMap::new();
        for peer in current.replicas.iter().chain(
            previous
                .iter()
                .flat_map(|previous| previous.replicas.iter()),
        ) {
            peer.validate_public_progress()?;
            if peer.replication_address.is_empty()
                || peer.process_session_id.is_empty()
                || !matches!(
                    peer.role,
                    ReplicaRole::Primary | ReplicaRole::ActiveSecondary
                )
                || !std::iter::once(&current.configuration)
                    .chain(previous.iter().map(|previous| &previous.configuration))
                    .any(|configuration| {
                        configuration.members.iter().any(|member| {
                            member.identity == peer.identity && member.role == peer.role
                        })
                    })
            {
                return Err(RuntimeError::AuthorityMismatch(
                    "static peer lacks exact active session authority".into(),
                ));
            }
            if let Some(existing) = peers.insert(peer.identity.clone(), peer.clone())
                && existing != *peer
            {
                return Err(RuntimeError::AuthorityMismatch(
                    "static configuration changes an exact peer descriptor".into(),
                ));
            }
        }
        let expected_peers = std::iter::once(&current.configuration)
            .chain(previous.iter().map(|previous| &previous.configuration))
            .flat_map(|configuration| configuration.members.iter())
            .filter(|member| member.identity != self.identity)
            .map(|member| member.identity.clone())
            .collect::<std::collections::BTreeSet<_>>();
        if peers
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            != expected_peers
        {
            return Err(RuntimeError::AuthorityMismatch(
                "static configuration lacks exact remote descriptors".into(),
            ));
        }
        let mut state = self.state.lock().await;
        if state.role != local_role
            || (state.role_epoch != Some(current.configuration.epoch)
                && state.updated_epoch != Some(current.configuration.epoch))
            || state.updated_epoch != Some(current.configuration.epoch)
        {
            return Err(RuntimeError::AuthorityMismatch(
                "configuration differs from static role and epoch replay".into(),
            ));
        }
        let authority = ConfigurationAuthority {
            epoch: current.configuration.epoch,
            current: current.configuration,
            previous: previous.map(|previous| previous.configuration),
            peers,
        };
        state.authority = Some(authority);
        Ok(())
    }
}

#[async_trait]
impl StateReplicator for StatelessReplicator {
    async fn replicate(&self, data: OperationData) -> Result<i64> {
        let _write = self.write_gate.lock().await;
        let authority = self.require_primary().await?;
        let (lsn, committed_lsn) = {
            let state = self.state.lock().await;
            (
                state
                    .applied_lsn
                    .checked_add(1)
                    .ok_or_else(|| RuntimeError::Application("LSN exhausted".into()))?,
                state.committed_lsn,
            )
        };
        let operation = Operation {
            lsn,
            committed_lsn,
            data,
        };
        let applied = match self.storage.apply(operation.clone()).await {
            Ok(applied) => applied,
            Err(error) => {
                self.contain_ambiguous_suffix(lsn).await;
                return Err(error);
            }
        };
        if applied.applied_lsn < lsn || applied.committed_lsn > applied.applied_lsn {
            self.contain_ambiguous_suffix(lsn).await;
            return Err(RuntimeError::Application(
                "provider returned inconsistent durable apply progress".into(),
            ));
        }
        {
            let mut state = self.state.lock().await;
            state.applied_lsn = state.applied_lsn.max(applied.applied_lsn);
        }
        if let Err(error) = self.publish_progress(false).await {
            self.contain_ambiguous_suffix(lsn).await;
            return Err(error);
        }
        let mut acknowledgements = std::collections::BTreeSet::from([self.identity.clone()]);
        for peer in authority.peers.values() {
            if self
                .send_replication(&authority, peer, &operation, committed_lsn)
                .await
                .is_ok()
            {
                acknowledgements.insert(peer.identity.clone());
            }
        }
        let quorum = |configuration: &ConfigurationDescriptor| {
            configuration
                .members
                .iter()
                .filter(|member| acknowledgements.contains(&member.identity))
                .count()
                >= configuration.write_quorum as usize
        };
        if !quorum(&authority.current) || authority.previous.as_ref().is_some_and(|p| !quorum(p)) {
            self.contain_ambiguous_suffix(lsn).await;
            return Err(RuntimeError::OperationCancelled);
        }
        let committed = match self.storage.commit(lsn).await {
            Ok(committed) => committed,
            Err(error) => {
                self.contain_ambiguous_suffix(lsn).await;
                return Err(error);
            }
        };
        if committed.committed_lsn < lsn || committed.committed_lsn > committed.applied_lsn {
            self.contain_ambiguous_suffix(lsn).await;
            return Err(RuntimeError::Application(
                "provider returned inconsistent durable commit progress".into(),
            ));
        }
        let mut state = self.state.lock().await;
        state.applied_lsn = state.applied_lsn.max(committed.applied_lsn);
        state.committed_lsn = state.committed_lsn.max(committed.committed_lsn);
        drop(state);
        self.publish_progress(false).await?;
        for peer in authority.peers.values() {
            let _ = self
                .send_replication(&authority, peer, &operation, lsn)
                .await;
        }
        Ok(lsn)
    }

    async fn get_replication_stream(&self) -> Result<OperationStream> {
        self.streams.take_replication().await
    }

    async fn get_copy_stream(&self) -> Result<OperationStream> {
        self.streams.take_copy().await
    }

    async fn update_replicator_settings(&self, settings: ReplicatorSettings) -> Result<()> {
        if !settings.public_v1 {
            return Err(RuntimeError::Application(
                "stateless preview cannot disable Public V1".into(),
            ));
        }
        let mut current = self.settings.write().await;
        if self.state.lock().await.opened
            && current.replication_address != settings.replication_address
        {
            return Err(RuntimeError::Application(
                "stateless preview listener address is immutable after Open".into(),
            ));
        }
        *current = settings;
        Ok(())
    }
}

type ReplicationResponseStream =
    Pin<Box<dyn Stream<Item = std::result::Result<proto::ReplicationAck, Status>> + Send>>;
type CopyResponseStream =
    Pin<Box<dyn Stream<Item = std::result::Result<proto::CopyAck, Status>> + Send>>;

struct PreviewReplicationService {
    engine: Weak<StatelessReplicator>,
}

#[tonic::async_trait]
impl proto::replication_data_server::ReplicationData for PreviewReplicationService {
    type ReplicateStream = ReplicationResponseStream;
    type BuildStream = CopyResponseStream;

    async fn replicate(
        &self,
        request: Request<tonic::Streaming<proto::ReplicationItem>>,
    ) -> std::result::Result<Response<Self::ReplicateStream>, Status> {
        let engine = self
            .engine
            .upgrade()
            .ok_or_else(|| Status::unavailable("stateless preview is closed"))?;
        let mut incoming = request.into_inner();
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        let task_engine = engine.clone();
        let task = tokio::spawn(async move {
            while let Some(item) = incoming.next().await {
                let result = match item {
                    Ok(item) => task_engine.accept_replication(item).await,
                    Err(error) => Err(error),
                };
                if sender.send(result).await.is_err() {
                    break;
                }
            }
        });
        engine
            .descendants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task.abort_handle());
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn build(
        &self,
        request: Request<tonic::Streaming<proto::CopyItem>>,
    ) -> std::result::Result<Response<Self::BuildStream>, Status> {
        let engine = self
            .engine
            .upgrade()
            .ok_or_else(|| Status::unavailable("stateless preview is closed"))?;
        let mut incoming = request.into_inner();
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        let task_engine = engine.clone();
        let task = tokio::spawn(async move {
            while let Some(item) = incoming.next().await {
                let result = match item {
                    Ok(item) => task_engine.accept_copy(item).await,
                    Err(error) => Err(error),
                };
                if sender.send(result).await.is_err() {
                    break;
                }
            }
        });
        engine
            .descendants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task.abort_handle());
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

fn parse_bind_address(address: &str) -> Result<SocketAddr> {
    address
        .strip_prefix("http://")
        .unwrap_or(address)
        .parse()
        .map_err(|error| RuntimeError::Application(format!("invalid replication address: {error}")))
}

fn runtime_status(error: RuntimeError) -> Status {
    Status::failed_precondition(error.to_string())
}
