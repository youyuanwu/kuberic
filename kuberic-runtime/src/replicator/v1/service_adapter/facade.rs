// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided facade lifetime composition from Replicator.cpp:178-253,296-562,
// ReplicatorState.cpp:39-126,177-248,
// Replicator.CatchupReplicaSetAsyncOperation.cpp:29-94 and
// Replicator.CloseAsyncOperation.cpp:31-154, microsoft/service-fabric
// @3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d, Reliability/Replication.

//! Private protected-trait bridge, not a factory or production host. One task
//! retains the actual caller-driven owner; abandoned callers do not drop its
//! selected work. Close and synchronous Abort are distinct from explicit join.
//! Exact peer bindings are injected, not derived from public string identities.

use std::collections::BTreeSet;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::error::RuntimeError;
use crate::protocol::types::{ReplicaId, ReplicaIdentity, ReplicaRole};
use crate::replicator::{
    PrimaryReplicator, ReplicaInformation, ReplicaSetConfiguration, ReplicaSetQuorumMode,
    Replicator,
};

use super::super::ingress::{MessageIngress, Target as IngressTarget};
use super::super::primary::{Primary, ReplicaDescriptor};
use super::*;

/// The host must supply the exact admitted process/storage/transport binding.
/// This private snapshot is not an authenticated or dynamic host registry.
pub(super) struct PeerBinding {
    pub(super) information: ReplicaInformation,
    pub(super) build: BuildConfig,
    pub(super) callbacks: BuildCallbacks,
}

pub(super) struct FacadeConfig {
    pub(super) local: ReplicaIdentity,
    pub(super) peers: Vec<PeerBinding>,
    pub(super) close_quorum_wait: Duration,
}

#[derive(Clone, Debug, thiserror::Error)]
pub(super) enum FacadeError {
    #[error(transparent)]
    Adapter(#[from] AdapterError),
    #[error("invalid private facade binding/configuration: {0}")]
    Input(String),
    #[error("private facade was aborted")]
    Aborted,
    #[error("private facade is closed")]
    Closed,
    #[error("private facade worker stopped before completion; join for its result")]
    Stopped,
    #[error("private facade worker join: {0}")]
    Join(String),
}

impl From<FacadeError> for RuntimeError {
    fn from(error: FacadeError) -> Self {
        match error {
            FacadeError::Aborted | FacadeError::Closed => Self::Closed,
            FacadeError::Adapter(AdapterError::Role(RoleError::Busy)) => {
                Self::ReconfigurationPending
            }
            error => Self::ControlPlane(error.to_string()),
        }
    }
}

type Reply = oneshot::Sender<Result<Value, FacadeError>>;

enum Value {
    Completed,
    Opened(String),
    Progress(Lsn),
    DataLoss(bool),
}

enum Command {
    Open,
    ChangeRole(Epoch, Role),
    UpdateEpoch(Epoch),
    Progress,
    Capability,
    DataLoss,
    Close,
    CatchupConfiguration(ReplicaSetConfiguration, ReplicaSetConfiguration),
    CurrentConfiguration(ReplicaSetConfiguration),
    Catchup(CatchupMode),
    Build(ReplicaInformation),
    Remove(i64),
}

struct Request {
    command: Command,
    reply: Reply,
}

struct Abort {
    service: ServiceHandle,
    ingress: MessageIngress,
    wake: Notify,
    closed: AtomicBool,
}

impl Abort {
    fn requested(&self) -> bool {
        self.service.aborted.load(Ordering::Acquire)
    }

    fn request(&self) {
        if self.service.aborted.swap(true, Ordering::AcqRel) {
            return;
        }
        self.ingress.set_target(IngressTarget::None);
        // Select under the projection guard, invoke actual child owners unlocked.
        let target = self.service.target.upgrade().map(|target| {
            std::mem::replace(
                &mut *target.lock().unwrap(),
                Target::Unavailable(RoleStatus::Aborting),
            )
        });
        let result = match target {
            Some(Target::Primary(primary)) => primary.abort().map_err(AdapterError::from),
            Some(Target::Secondary(secondary)) => secondary.abort().map_err(AdapterError::from),
            _ => Ok(()),
        };
        if let Err(error) = result {
            tracing::warn!(%error, "private V1 facade synchronous child abort failed");
        }
        self.wake.notify_one();
    }
}

struct Join {
    task: Option<JoinHandle<Result<(), FacadeError>>>,
    result: Option<Result<(), FacadeError>>,
}

pub(super) struct Facade {
    requests: mpsc::Sender<Request>,
    abort: Arc<Abort>,
    join: AsyncMutex<Join>,
}

impl Facade {
    pub(super) fn start(
        adapter: ServiceReplicator,
        config: FacadeConfig,
    ) -> Result<Arc<Self>, FacadeError> {
        config.validate(&adapter)?;
        let abort = Arc::new(Abort {
            service: adapter.service(),
            ingress: adapter.owner.ingress(),
            wake: Notify::new(),
            closed: AtomicBool::new(false),
        });
        let (requests, receiver) = mpsc::channel(64);
        let signal = abort.clone();
        let task = tokio::spawn(async move {
            let result = run(adapter, config, receiver, signal).await;
            if let Err(error) = &result {
                tracing::warn!(%error, "private V1 facade driver failed");
            }
            result
        });
        Ok(Arc::new(Self {
            requests,
            abort,
            join: AsyncMutex::new(Join {
                task: Some(task),
                result: None,
            }),
        }))
    }

    pub(super) fn service(&self) -> ServiceHandle {
        self.abort.service.clone()
    }

    /// Cancellation retains the JoinHandle; successful Close is not this join.
    pub(super) async fn join(&self) -> Result<(), FacadeError> {
        let mut join = self.join.lock().await;
        if join.result.is_none() {
            let result = join.task.as_mut().expect("unobserved driver").await;
            join.task = None;
            join.result = Some(
                result
                    .map_err(|error| FacadeError::Join(error.to_string()))
                    .and_then(|result| result),
            );
        }
        join.result.as_ref().unwrap().clone()
    }

    async fn call(&self, command: Command) -> crate::Result<Value> {
        if self.abort.closed.load(Ordering::Acquire) {
            return Err(FacadeError::Closed.into());
        }
        if self.abort.requested() {
            return Err(FacadeError::Aborted.into());
        }
        let (reply, receiver) = oneshot::channel();
        self.requests
            .send(Request { command, reply })
            .await
            .map_err(|_| RuntimeError::from(FacadeError::Stopped))?;
        receiver
            .await
            .map_err(|_| RuntimeError::from(FacadeError::Stopped))?
            .map_err(RuntimeError::from)
    }

    async fn completed(&self, command: Command) -> crate::Result<()> {
        match self.call(command).await? {
            Value::Completed => Ok(()),
            _ => unreachable!("unit command result"),
        }
    }
}

impl Drop for Facade {
    fn drop(&mut self) {
        // The retained task observes Abort and drains its real callbacks; dropping
        // this join owner is abnormal containment, never proof of a clean join.
        self.abort.request();
    }
}

impl FacadeConfig {
    fn validate(&self, adapter: &ServiceReplicator) -> Result<(), FacadeError> {
        let endpoint = adapter.owner.ingress().published_endpoint();
        if self.local.replica_id.value() != endpoint.endpoint.replica
            || self.local.instance_id.is_empty()
            || self.local.agent_generation.is_empty()
        {
            return Err(FacadeError::Input("local identity".into()));
        }
        let mut ids = BTreeSet::new();
        for peer in &self.peers {
            let info = &peer.information;
            peer.build
                .route
                .validate()
                .map_err(|error| FacadeError::Input(error.to_string()))?;
            if !ids.insert(info.identity.replica_id)
                || info.identity.replica_id == self.local.replica_id
                || info.identity.instance_id.is_empty()
                || info.identity.agent_generation.is_empty()
                || info.process_session_id.is_empty()
                || peer.build.route.source != endpoint.endpoint
                || peer.build.route.sender_address != endpoint.address
                || peer.build.route.destination.replica != info.identity.replica_id.value()
                || peer.build.route.remote_address != info.replication_address
            {
                return Err(FacadeError::Input("peer identity/route".into()));
            }
        }
        Ok(())
    }

    fn peer(&self, info: &ReplicaInformation) -> Result<&PeerBinding, FacadeError> {
        info.validate_public_progress()
            .map_err(|error| FacadeError::Input(error.to_string()))?;
        self.peers
            .iter()
            .find(|peer| {
                let bound = &peer.information;
                matches_binding(bound, info)
            })
            .ok_or_else(|| FacadeError::Input("no exact admitted peer binding".into()))
    }

    fn configuration(
        &self,
        input: &ReplicaSetConfiguration,
        final_configuration: bool,
    ) -> Result<ReplicaConfiguration, FacadeError> {
        let configuration = &input.configuration;
        if configuration.configuration_id != configuration.expected_id()
            || configuration.epoch.data_loss_number < 0
            || configuration.epoch.configuration_number < 0
            || configuration.write_quorum == 0
            || configuration.write_quorum as usize > configuration.members.len()
            || configuration
                .members
                .iter()
                .filter(|member| member.role == ReplicaRole::Primary)
                .count()
                != 1
            || !configuration.members.iter().any(|member| {
                member.role == ReplicaRole::Primary
                    && member.identity.replica_id == configuration.primary_id
            })
            || !configuration
                .members
                .iter()
                .any(|member| member.identity == self.local)
        {
            return Err(FacadeError::Input("voting configuration".into()));
        }
        let mut described = BTreeSet::new();
        for replica in &input.replicas {
            if !described.insert(replica.identity.replica_id) {
                return Err(FacadeError::Input("duplicate replica description".into()));
            }
            self.peer(replica)?;
        }
        let mut members = BTreeSet::new();
        let mut replicas = Vec::new();
        for member in &configuration.members {
            if member.identity.replica_id.value() <= 0
                || !members.insert(member.identity.replica_id)
                || !matches!(
                    member.role,
                    ReplicaRole::Primary | ReplicaRole::ActiveSecondary
                )
            {
                return Err(FacadeError::Input("invalid voting member".into()));
            }
            if member.identity == self.local {
                continue;
            }
            let info = input
                .replicas
                .iter()
                .find(|info| info.identity == member.identity)
                .ok_or_else(|| FacadeError::Input("missing exact replica description".into()))?;
            if info.role != member.role || (final_configuration && info.must_catchup) {
                return Err(FacadeError::Input("remote voting role/MustCatchup".into()));
            }
            let peer = self.peer(info)?;
            replicas.push(ReplicaDescriptor {
                route: peer.build.route.clone(),
                remote_incarnation: peer.build.remote_incarnation,
                progress: (info.current_progress != crate::application::INVALID_LSN)
                    .then_some(info.current_progress),
                must_catchup: info.must_catchup,
            });
        }
        Ok(ReplicaConfiguration {
            write_quorum: configuration.write_quorum as usize,
            replicas,
        })
    }
}

pub(super) fn matches_binding(bound: &ReplicaInformation, input: &ReplicaInformation) -> bool {
    bound.identity == input.identity
        && bound.process_session_id == input.process_session_id
        && bound.storage_incarnation == input.storage_incarnation
        && bound.replication_address == input.replication_address
}

fn reply(reply: Reply, result: Result<Value, FacadeError>) {
    // A missing waiter is expected cancellation, not cancellation of its work.
    let _ = reply.send(result);
}

fn handle(
    adapter: &mut ServiceReplicator,
    config: &FacadeConfig,
    request: Request,
    pending: &mut Option<Reply>,
    waits: &mut FuturesUnordered<BoxFuture<'static, ()>>,
) {
    let Request {
        command,
        reply: response,
    } = request;
    let mut response = Some(response);
    let result = (|| -> Result<Option<Value>, FacadeError> {
        if let Command::CatchupConfiguration(current, _) | Command::CurrentConfiguration(current) =
            &command
            && current.configuration.primary_id != config.local.replica_id
        {
            return Err(FacadeError::Input(
                "current configuration has a different primary".into(),
            ));
        }
        match command {
            Command::Open => {
                let endpoint = adapter
                    .owner
                    .open(&adapter.registry)
                    .map_err(AdapterError::from);
                adapter.refresh();
                return Ok(Some(Value::Opened(endpoint?.address)));
            }
            Command::ChangeRole(epoch, role) => adapter.select(Control::ChangeRole, |owner| {
                owner.begin_change_role(role, epoch)
            })?,
            Command::UpdateEpoch(epoch) => adapter.select(Control::UpdateEpoch, |owner| {
                owner.begin_update_epoch(epoch)
            })?,
            Command::Progress => match adapter.service.target()? {
                Target::Primary(primary) => {
                    return Ok(Some(Value::Progress(
                        primary.current_progress().map_err(AdapterError::from)?,
                    )));
                }
                Target::Secondary(secondary) => {
                    return Ok(Some(Value::Progress(
                        secondary.current_progress().map_err(AdapterError::from)?,
                    )));
                }
                Target::Unavailable(RoleStatus::Opened) => {
                    adapter.select(Control::Progress, RoleReplicator::begin_get_opened_progress)?;
                }
                Target::Unavailable(status) => {
                    return Err(AdapterError::InvalidState(status).into());
                }
            },
            Command::Capability => return Ok(Some(Value::Progress(adapter.catchup_capability()?))),
            Command::DataLoss => {
                adapter.select(Control::DataLoss, RoleReplicator::begin_data_loss)?
            }
            Command::Close => adapter.select(Control::Close, |owner| {
                owner.begin_close(config.close_quorum_wait)
            })?,
            Command::CatchupConfiguration(current, previous) => {
                let previous = config.configuration(&previous, false)?;
                let current = config.configuration(&current, false)?;
                adapter.update_catchup_configuration(Some(previous), current)?;
                return Ok(Some(Value::Completed));
            }
            Command::CurrentConfiguration(current) => {
                adapter.update_current_configuration(config.configuration(&current, true)?)?;
                return Ok(Some(Value::Completed));
            }
            Command::Catchup(mode) => {
                let mut completion = adapter.wait_for_catchup(mode)?;
                let response = response.take().unwrap();
                waits.push(
                    async move {
                        reply(
                            response,
                            completion
                                .wait()
                                .await
                                .map(|()| Value::Completed)
                                .map_err(FacadeError::from),
                        );
                    }
                    .boxed(),
                );
                return Ok(None);
            }
            Command::Build(info) => {
                if info.role != ReplicaRole::IdleSecondary {
                    return Err(FacadeError::Input("build requires an idle replica".into()));
                }
                let peer = config.peer(&info)?;
                let build = adapter.build_replica(peer.build.clone(), peer.callbacks.clone())?;
                let response = response.take().unwrap();
                waits.push(
                    async move {
                        reply(
                            response,
                            build
                                .wait()
                                .await
                                .map(|()| Value::Completed)
                                .map_err(FacadeError::from),
                        );
                    }
                    .boxed(),
                );
                return Ok(None);
            }
            Command::Remove(id) => {
                adapter.remove_replica(id)?;
                return Ok(Some(Value::Completed));
            }
        }
        Ok(None)
    })();
    match result {
        Ok(Some(value)) => reply(response.take().unwrap(), Ok(value)),
        Ok(None) => {
            if let Some(response) = response {
                *pending = Some(response);
            }
        }
        Err(error) => reply(response.take().unwrap(), Err(error)),
    }
}

async fn run(
    mut adapter: ServiceReplicator,
    config: FacadeConfig,
    mut requests: mpsc::Receiver<Request>,
    abort: Arc<Abort>,
) -> Result<(), FacadeError> {
    let mut pending = None;
    let mut waits = FuturesUnordered::new();
    let mut aborting = false;
    let mut failure = None;
    let mut retired = Vec::new();
    let mut contexts = FuturesUnordered::new();
    loop {
        retired.extend(adapter.owner.take_retired_primaries());
        retain_contexts(&mut adapter, &mut contexts);
        let wake = abort.wake.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        if abort.requested() && !aborting {
            if let Some(response) = pending.take() {
                reply(response, Err(FacadeError::Aborted));
            }
            adapter.abort()?;
            aborting = true;
        }
        let selected = adapter.control;
        let steady = matches!(
            adapter.status(),
            RoleStatus::Ready(Role::Primary | Role::IdleSecondary | Role::ActiveSecondary)
        );
        tokio::select! {
            biased;
            () = &mut wake, if !aborting => {}
            result = async {
                if selected.is_some() {
                    adapter.resume().await.map(Some)
                } else if steady {
                    adapter.drive().await.map(|()| None)
                } else {
                    std::future::pending().await
                }
            } => {
                // A provider/effect may synchronously reenter Abort in this poll.
                // Never publish that poll's late control success after revocation.
                if abort.requested() && !aborting {
                    continue;
                }
                if aborting {
                    match result {
                        Err(AdapterError::Role(RoleError::Aborted)) => break,
                        Err(error) => { failure.get_or_insert(FacadeError::from(error)); break; }
                        Ok(_) => unreachable!("Abort cannot complete successfully"),
                    }
                } else if let Some(response) = pending.take() {
                    let closed = matches!(selected, Some(Control::Close)) && result.is_ok();
                    if closed {
                        abort.closed.store(true, Ordering::Release);
                    }
                    reply(response, result.and_then(|result| match result.expect("selected control result") {
                        ControlResult::Completed => Ok(Value::Completed),
                        ControlResult::Progress(progress) => Ok(Value::Progress(progress)),
                        ControlResult::DataLoss(end) => {
                            // The protected Result<bool> cannot carry the changed
                            // flag on failed End; the actual owner retains it.
                            end.result.map(|()| Value::DataLoss(end.state_changed)).map_err(AdapterError::from)
                        }
                    }).map_err(FacadeError::from));
                    if closed { break; }
                } else if let Err(error) = result {
                    failure = Some(error.into());
                    abort.request();
                }
            }
            error = async {
                match drain_retired(&mut retired).await {
                    Err(error) => error,
                    Ok(()) => std::future::pending().await,
                }
            }, if failure.is_none() => {
                failure = Some(error.into());
                abort.request();
            }
            Some(()) = waits.next(), if !waits.is_empty() => {}
            Some(result) = contexts.next(), if !contexts.is_empty() => {
                if let Err(error) = result {
                    failure.get_or_insert(FacadeError::from(error));
                    abort.request();
                }
            }
            request = requests.recv(), if !aborting => match request {
                Some(request) => handle(&mut adapter, &config, request, &mut pending, &mut waits),
                None => abort.request(),
            }
        }
    }
    retired.extend(adapter.owner.take_retired_primaries());
    retain_contexts(&mut adapter, &mut contexts);
    requests.close();
    while let Ok(request) = requests.try_recv() {
        reply(
            request.reply,
            Err(if abort.closed.load(Ordering::Acquire) {
                FacadeError::Closed
            } else {
                FacadeError::Aborted
            }),
        );
    }
    drop(requests);
    let (active, retired_result, context_result) = tokio::join!(
        adapter.owner.drain_builds(),
        drain_retired(&mut retired),
        async {
            let mut result = Ok(());
            while let Some(context) = contexts.next().await {
                result = result.and(context);
            }
            result
        }
    );
    drop(adapter);
    drop(retired);
    while waits.next().await.is_some() {}
    if let Some(error) = failure {
        return Err(error);
    }
    active.map_err(AdapterError::from)?;
    retired_result?;
    context_result.map_err(FacadeError::from)
}

fn retain_contexts(
    adapter: &mut ServiceReplicator,
    contexts: &mut FuturesUnordered<BoxFuture<'static, Result<(), AdapterError>>>,
) {
    for mut secondary in adapter.owner.take_retired_secondaries() {
        contexts.push(
            async move { secondary.drain_context().await.map_err(AdapterError::from) }.boxed(),
        );
    }
}

async fn drain_retired(retired: &mut [Primary]) -> Result<(), AdapterError> {
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for primary in retired.iter_mut() {
            match primary.poll_drain_builds(cx) {
                Poll::Ready(result) => result?,
                Poll::Pending => pending = true,
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    })
    .await
}

#[async_trait]
impl Replicator for Facade {
    async fn open(&self) -> crate::Result<String> {
        match self.call(Command::Open).await? {
            Value::Opened(address) => Ok(address),
            _ => unreachable!("Open result"),
        }
    }

    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> crate::Result<()> {
        let role = match role {
            ReplicaRole::None => Role::None,
            ReplicaRole::Primary => Role::Primary,
            ReplicaRole::IdleSecondary => Role::IdleSecondary,
            ReplicaRole::ActiveSecondary => Role::ActiveSecondary,
        };
        self.completed(Command::ChangeRole(epoch, role)).await
    }

    async fn update_epoch(&self, epoch: Epoch) -> crate::Result<()> {
        self.completed(Command::UpdateEpoch(epoch)).await
    }

    async fn close(&self) -> crate::Result<()> {
        self.completed(Command::Close).await
    }

    fn abort(&self) {
        self.abort.request();
    }

    async fn current_progress(&self) -> crate::Result<Lsn> {
        match self.call(Command::Progress).await? {
            Value::Progress(progress) => Ok(progress),
            _ => unreachable!("progress result"),
        }
    }

    async fn catch_up_capability(&self) -> crate::Result<Lsn> {
        match self.call(Command::Capability).await? {
            Value::Progress(progress) => Ok(progress),
            _ => unreachable!("capability result"),
        }
    }
}

#[async_trait]
impl PrimaryReplicator for Facade {
    async fn on_data_loss(&self) -> crate::Result<bool> {
        match self.call(Command::DataLoss).await? {
            Value::DataLoss(changed) => Ok(changed),
            _ => unreachable!("data-loss result"),
        }
    }

    async fn update_catch_up_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
        previous: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        self.completed(Command::CatchupConfiguration(current, previous))
            .await
    }

    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> crate::Result<()> {
        let mode = match mode {
            ReplicaSetQuorumMode::WriteQuorum => CatchupMode::WriteQuorum,
            ReplicaSetQuorumMode::All => CatchupMode::All,
        };
        self.completed(Command::Catchup(mode)).await
    }

    async fn update_current_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        self.completed(Command::CurrentConfiguration(current)).await
    }

    async fn build_replica(&self, replica: ReplicaInformation) -> crate::Result<()> {
        self.completed(Command::Build(replica)).await
    }

    async fn remove_replica(&self, replica_id: ReplicaId) -> crate::Result<()> {
        self.completed(Command::Remove(replica_id.value())).await
    }
}
