use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};

use async_trait::async_trait;
use futures::{Stream, StreamExt, stream};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock, watch};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status};

use crate::application::{Operation, OperationData, StateProvider};
use crate::control::proto;
use crate::engine::DurableState;
use crate::protocol::public_operations::PUBLIC_V1_PREVIEW_PROTOCOL_VERSION;
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
            ReplicatorCapabilities::conservative(),
        ))
    }
}

#[derive(Clone, PartialEq, Eq)]
struct StaticAuthority {
    epoch: Epoch,
    current: ConfigurationDescriptor,
    previous: Option<ConfigurationDescriptor>,
    peers: BTreeMap<ReplicaIdentity, ReplicaInformation>,
}

struct EngineState {
    opened: bool,
    role: ReplicaRole,
    role_epoch: Option<Epoch>,
    updated_epoch: Option<Epoch>,
    authority: Option<StaticAuthority>,
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

    async fn require_static_primary(&self) -> Result<StaticAuthority> {
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
            if state.ambiguous_suffix {
                return Err(Status::failed_precondition(
                    "replica has an ambiguous applied suffix",
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
        }
        if let Err(error) = self.publish_progress(false).await {
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
        }
        self.publish_progress(false).await.map_err(runtime_status)?;
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
        authority: &StaticAuthority,
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
            return Ok(());
        }
        if state.role != ReplicaRole::None
            && (state.role != role || state.role_epoch != Some(epoch))
        {
            state.authority = None;
            return Err(RuntimeError::ReconfigurationPending);
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
            if state.role_epoch != Some(epoch) {
                return Err(RuntimeError::AuthorityMismatch(
                    "epoch differs from the exact static role replay".into(),
                ));
            }
            if state.updated_epoch == Some(epoch) {
                return Ok(());
            }
            state.applied_lsn
        };
        self.provider
            .update_epoch(epoch, previous_epoch_last_lsn)
            .await?;
        let mut state = self.state.lock().await;
        if state.role_epoch != Some(epoch) {
            return Err(RuntimeError::OperationCancelled);
        }
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
        Err(RuntimeError::ReconfigurationPending)
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
        let state = self.state.lock().await;
        let authority = state
            .authority
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let ready = std::iter::once(&self.identity)
            .chain(
                authority
                    .peers
                    .values()
                    .filter(|peer| peer.current_progress >= state.committed_lsn)
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
            .filter(|peer| peer.current_progress >= state.committed_lsn)
            .count();
        let satisfied = match mode {
            ReplicaSetQuorumMode::WriteQuorum => {
                quorum_ready(&authority.current)
                    && authority.previous.as_ref().is_none_or(quorum_ready)
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

    async fn build_replica(&self, _replica: ReplicaInformation) -> Result<()> {
        self.fence_authority().await;
        Err(RuntimeError::ReconfigurationPending)
    }

    async fn remove_replica(&self, _replica_id: ReplicaId) -> Result<()> {
        self.fence_authority().await;
        Err(RuntimeError::ReconfigurationPending)
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
            || state.role_epoch != Some(current.configuration.epoch)
            || state.updated_epoch != Some(current.configuration.epoch)
        {
            return Err(RuntimeError::AuthorityMismatch(
                "configuration differs from static role and epoch replay".into(),
            ));
        }
        let authority = StaticAuthority {
            epoch: current.configuration.epoch,
            current: current.configuration,
            previous: previous.map(|previous| previous.configuration),
            peers,
        };
        if state.authority.as_ref().is_some_and(|existing| {
            existing.current != authority.current
                || existing.previous != authority.previous
                || existing.peers != authority.peers
        }) {
            state.authority = None;
            return Err(RuntimeError::ReconfigurationPending);
        }
        state.authority = Some(authority);
        Ok(())
    }
}

#[async_trait]
impl StateReplicator for StatelessReplicator {
    async fn replicate(&self, data: OperationData) -> Result<i64> {
        let _write = self.write_gate.lock().await;
        let authority = self.require_static_primary().await?;
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
        _request: Request<tonic::Streaming<proto::CopyItem>>,
    ) -> std::result::Result<Response<Self::BuildStream>, Status> {
        Err(Status::failed_precondition(
            "stateless preview build is a Phase 2 operation",
        ))
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
