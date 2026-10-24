// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided primary/session lifetime composition from PrimaryReplicator.cpp:
// 261-315,340-462,550-728, PrimaryReplicator.CloseAsyncOperation.cpp:36-132,
// ReplicaManager.cpp:306-335,792-870,926-1007,1205-1779,2187-2222,
// ReplicaManager.cpp:871-920 and PrimaryReplicator.UpdateEpochAsyncOperation.cpp,
// PrimaryReplicator.cpp:501-505, ReplicaManager.cpp:1015-1023 and
// PrimaryReplicator.cpp:319-336 (queue progress/capability queries),
// Replicator.cpp:659-708,799-838 (CheckingDataLoss admission),
// ReplicationSession.cpp:279-283 and RemoteSession.cpp:448-656,
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d,
// src/prod/src/Reliability/Replication/. See sf-v1-port-provenance.md.

//! Private caller-driven primary lifetime and descriptor-based configuration.
//! The private role owner supplies promotion queues and provider epoch calls;
//! this owner does not activate a public engine. One shared clock
//! covers owned sessions; the established-session seam remains for isolated use.
//! No task is spawned; dropping the owner is abandonment, not Close or drain.
//! Terminal Close/Abort drains selected synchronous effect/oneshot deliveries,
//! not client waiter bodies or arbitrary provider build calls. Retained builds
//! can be driven separately after terminal primary completion.

#![cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 primary lifetime")
)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use tokio::sync::Notify;

use crate::application::{Lsn, OperationData};
use crate::protocol::types::Epoch;
use crate::traits::StateProvider;

use super::async_runtime::Clock;
use super::buffers::{BufferLimits, BufferPolicy};
use super::build_idle::{
    BuildCallbacks, BuildConfig, BuildEnd, BuildError, BuildIdle, BuildIdleHandle, deliver_ack,
};
use super::copy_context_receiver::ContextObservation;
use super::copy_sender::CopySender;
use super::operation_queue::QueueConfig;
use super::replica_manager::{
    CatchupMode, CompletionReceiver, EstablishedConfiguration, ManagerEffects, ManagerError,
    QueuePolicyConfig, ReplicaManager, ReplicaSet, SessionMember,
};
use super::replication_queue_manager::QueueTransfer;
use super::sender::SenderConfig;
use super::session::{AckUpdate, ReplicationSession, SendIntent};
use super::transport::{ProtocolError, Route, wire};

#[derive(Clone, Debug, thiserror::Error)]
pub(super) enum PrimaryError {
    #[error("primary is closing or closed")]
    Closed,
    #[error("primary owner was abandoned")]
    OwnerDropped,
    #[error("primary was aborted, not ordinarily closed")]
    Aborted,
    #[error("primary is checking data loss")]
    CheckingDataLoss,
    #[error("primary has no admitted data-loss operation")]
    NoDataLoss,
    #[error("queue reset requires selected primary activities to finish")]
    PendingActivity,
    #[error("no admitted primary session for replica {0}")]
    UnknownSession(i64),
    #[error("replica {0} is not an idle build")]
    NotBuilding(i64),
    #[error("replica {0} has no unique matching route/session binding")]
    InvalidBinding(i64),
    #[error("invalid primary session sender settings")]
    InvalidSenderConfig,
    #[error("invalid primary queue-policy timer settings")]
    InvalidQueuePolicy,
    #[error("current replica {0} has unknown progress and no current or idle session")]
    UnknownCurrentProgress(i64),
    #[error("replica {0} has invalid initial progress")]
    InvalidProgress(i64),
    #[error("current write quorum one requires an empty remote replica set")]
    InvalidCurrentQuorum,
    #[error("StateProvider EndUpdateEpoch: {0}")]
    Provider(Arc<crate::error::RuntimeError>),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error("primary manager: {0:?}")]
    Manager(Arc<ManagerError>),
}

impl From<ManagerError> for PrimaryError {
    fn from(error: ManagerError) -> Self {
        Self::Manager(Arc::new(error))
    }
}

#[derive(Clone)]
pub(super) struct PrimaryConfig {
    pub(super) queue: QueueConfig,
    pub(super) initial_progress: Lsn,
    pub(super) endpoint: wire::Endpoint,
    pub(super) address: String,
    pub(super) epoch: Epoch,
    pub(super) allow_multiple_quorum_set: bool,
    pub(super) sender: SenderConfig,
    pub(super) queue_policy: QueuePolicyConfig,
    pub(super) send: Arc<dyn Fn(SendIntent) -> bool + Send + Sync>,
}

#[derive(Clone)]
pub(super) struct ReplicaDescriptor {
    pub(super) route: Route,
    pub(super) remote_incarnation: i64,
    /// None is native FABRIC_INVALID_SEQUENCE_NUMBER, not progress zero.
    pub(super) progress: Option<Lsn>,
    pub(super) must_catchup: bool,
}

pub(super) struct ReplicaConfiguration {
    /// Includes the primary's immediate vote; descriptors exclude the primary.
    pub(super) write_quorum: usize,
    pub(super) replicas: Vec<ReplicaDescriptor>,
}

pub(super) struct PrimaryAdmission {
    pub(super) lsn: Lsn,
    pub(super) completion: CompletionReceiver,
}

#[derive(Clone, Debug)]
pub(super) struct PrimaryEnd {
    pub(super) result: Result<(), PrimaryError>,
    pub(super) aborted: bool,
    pub(super) quorum_wait_expired: bool,
}

#[derive(Clone)]
enum SessionOwner {
    Build(BuildIdleHandle),
    // Native active sessions still receive the replication-gate handoff.
    Established(Arc<CopySender>),
}

#[derive(Clone)]
struct Binding {
    route: Route,
    session: ReplicationSession,
    owner: SessionOwner,
    idle: bool,
}

impl Binding {
    fn drives_replication_timer(&self) -> bool {
        match &self.owner {
            SessionOwner::Build(build) => build.result().is_some(),
            SessionOwner::Established(_) => true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Open,
    PreClose(Duration),
    Closing,
    Closed,
}

struct State {
    phase: Phase,
    bindings: BTreeMap<i64, Binding>,
    queued: Vec<BuildIdle>,
    activities: usize,
    aborted: bool,
    quorum_wait_expired: bool,
    failure: Option<PrimaryError>,
    creating_secondary: bool,
    transferred: bool,
    checking_data_loss: bool,
    queue_policy: QueuePolicyConfig,
    policy_deadline: Duration,
    idle_refreshed_at: Duration,
}

struct Root {
    state: Mutex<State>,
    manager: Arc<Mutex<ReplicaManager>>,
    endpoint: wire::Endpoint,
    address: String,
    clock: Clock,
    sender: SenderConfig,
    send: Arc<dyn Fn(SendIntent) -> bool + Send + Sync>,
    wake: Notify,
    alive: AtomicBool,
}

struct Activity(Arc<Root>);

impl Drop for Activity {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().activities -= 1;
        self.0.wake.notify_one();
    }
}

impl Root {
    fn activity(self: &Arc<Self>, state: &mut State) -> Activity {
        state.activities += 1;
        Activity(self.clone())
    }

    fn check_open(state: &State) -> Result<(), PrimaryError> {
        Self::check_active(state)?;
        if state.checking_data_loss {
            return Err(PrimaryError::CheckingDataLoss);
        }
        Ok(())
    }

    fn check_active(state: &State) -> Result<(), PrimaryError> {
        if state.phase != Phase::Open {
            return Err(PrimaryError::Closed);
        }
        if let Some(error) = &state.failure {
            return Err(error.clone());
        }
        Ok(())
    }

    fn check_route(&self, route: &Route) -> Result<(), PrimaryError> {
        route.validate()?;
        if route.source != self.endpoint
            || route.sender_address != self.address
            || route.destination.partition != self.endpoint.partition
        {
            return Err(PrimaryError::InvalidBinding(route.destination.replica));
        }
        Ok(())
    }

    fn progress(self: &Arc<Self>) -> Result<(), BuildError> {
        let (effects, _activity) = {
            let mut state = self.state.lock().unwrap();
            if matches!(state.phase, Phase::Closing | Phase::Closed) {
                // Native weak-primary progress callbacks do not reopen a closed manager.
                return Ok(());
            }
            let effects = self.manager.lock().unwrap().update_progress()?;
            (effects, self.activity(&mut state))
        };
        effects.dispatch(self.clock.now())?;
        Ok(())
    }

    fn close_effects(self: &Arc<Self>, state: &mut State) -> (ManagerEffects, Activity) {
        state.phase = Phase::Closing;
        let effects = if state.creating_secondary {
            self.manager.lock().unwrap().close_for_secondary()
        } else {
            self.manager.lock().unwrap().close()
        };
        (effects, self.activity(state))
    }

    fn deliver_close(&self, effects: ManagerEffects) -> Result<(), PrimaryError> {
        let result = effects
            .dispatch(self.clock.now())
            .map_err(PrimaryError::from);
        if let Err(error) = &result {
            let mut state = self.state.lock().unwrap();
            if state.failure.is_none() {
                state.failure = Some(error.clone());
            }
        }
        result
    }

    fn queue_pressure(self: &Arc<Self>) -> Result<(), PrimaryError> {
        let (effects, result, _activity) = {
            let mut state = self.state.lock().unwrap();
            if !matches!(state.phase, Phase::Open | Phase::PreClose(_)) {
                return Ok(());
            }
            let (effects, result) = self
                .manager
                .lock()
                .unwrap()
                .apply_queue_pressure(state.queue_policy, self.clock.now());
            let result = result.map_err(PrimaryError::from);
            if let Err(error) = &result {
                state.failure.get_or_insert(error.clone());
            }
            (effects, result, self.activity(&mut state))
        };
        self.wake.notify_one();
        let delivery = effects.dispatch(self.clock.now());
        result?;
        delivery?;
        Ok(())
    }

    fn policy_tick(&self) -> Result<(), PrimaryError> {
        let mut state = self.state.lock().unwrap();
        let now = self.clock.now();
        if !matches!(state.phase, Phase::Open | Phase::PreClose(_)) || state.policy_deadline > now {
            return Ok(());
        }
        let config = state.queue_policy;
        if now.saturating_sub(state.idle_refreshed_at) > config.idle_max_lag {
            self.manager.lock().unwrap().refresh_idle_targets()?;
            state.idle_refreshed_at = now;
        }
        state.policy_deadline = now.saturating_add(config.interval());
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct PrimaryHandle(Weak<Root>);

impl PrimaryHandle {
    pub(super) fn idle_replica_ids(&self) -> Result<Vec<i64>, PrimaryError> {
        let root = self.root()?;
        let state = root.state.lock().unwrap();
        Root::check_open(&state)?;
        Ok(root.manager.lock().unwrap().idle_replica_ids())
    }

    fn root(&self) -> Result<Arc<Root>, PrimaryError> {
        let root = self.0.upgrade().ok_or(PrimaryError::OwnerDropped)?;
        if !root.alive.load(Ordering::Acquire) {
            return Err(PrimaryError::OwnerDropped);
        }
        Ok(root)
    }

    pub(super) fn current_progress(&self) -> Result<Lsn, PrimaryError> {
        let root = self.root()?;
        let state = root.state.lock().unwrap();
        Root::check_open(&state)?;
        let manager = root.manager.lock().unwrap();
        manager.check_reported_fault()?;
        Ok(manager.queue().current_progress())
    }

    pub(super) fn catchup_capability(&self) -> Result<Lsn, PrimaryError> {
        let root = self.root()?;
        let state = root.state.lock().unwrap();
        Root::check_open(&state)?;
        let manager = root.manager.lock().unwrap();
        manager.check_reported_fault()?;
        Ok(manager.queue().catchup_capability())
    }

    pub(super) fn update_queue_policy(
        &self,
        config: QueuePolicyConfig,
    ) -> Result<(), PrimaryError> {
        let root = self.root()?;
        let mut state = root.state.lock().unwrap();
        Root::check_open(&state)?;
        if !config.is_valid() {
            return Err(PrimaryError::InvalidQueuePolicy);
        }
        state.queue_policy = config;
        root.wake.notify_one();
        Ok(())
    }

    /// Explicit established-membership seam, not native promotion/role creation.
    /// Routes cover exactly CC/PC members; previously admitted idles are retained.
    /// Returns the actual manager's explicitly paused PC-only replay sessions.
    pub(super) fn configure_established(
        &self,
        configuration: EstablishedConfiguration,
        routes: Vec<Route>,
    ) -> Result<Vec<i64>, PrimaryError> {
        let root = self.root()?;
        let (effects, _activity) = {
            let mut state = root.state.lock().unwrap();
            Root::check_open(&state)?;
            let effects = Self::install_configuration(&root, &mut state, configuration, routes)?;
            (effects, root.activity(&mut state))
        };
        let paused = effects.paused_previous_replay.clone();
        effects.dispatch(root.clock.now())?;
        Ok(paused)
    }

    fn install_configuration(
        root: &Root,
        state: &mut State,
        configuration: EstablishedConfiguration,
        routes: Vec<Route>,
    ) -> Result<ManagerEffects, PrimaryError> {
        let mut supplied = BTreeMap::new();
        for route in routes {
            root.check_route(&route)?;
            let id = route.destination.replica;
            if supplied.insert(id, route).is_some() {
                return Err(PrimaryError::InvalidBinding(id));
            }
        }
        let mut bindings = state.bindings.clone();
        for set in std::iter::once(&configuration.current).chain(configuration.previous.iter()) {
            for member in &set.members {
                let id = member.replica_id;
                let route = supplied.get(&id).ok_or(PrimaryError::InvalidBinding(id))?;
                let identity = member.session.identity();
                if identity.primary_endpoint != route.sender_address
                    || identity.remote_endpoint != route.remote_address
                    || bindings.get(&id).is_some_and(|binding| {
                        !binding.session.same_instance(&member.session)
                            || binding.route.source != route.source
                            || binding.route.destination != route.destination
                            || binding.route.remote_address != route.remote_address
                    })
                {
                    return Err(PrimaryError::InvalidBinding(id));
                }
                bindings.entry(id).or_insert_with(|| Binding {
                    route: route.clone(),
                    session: member.session.clone(),
                    owner: SessionOwner::Established(Arc::new(CopySender::new(false))),
                    idle: false,
                });
            }
        }
        if let Some(id) = supplied.keys().find(|id| {
            !std::iter::once(&configuration.current)
                .chain(configuration.previous.iter())
                .any(|set| set.members.iter().any(|member| member.replica_id == **id))
        }) {
            return Err(PrimaryError::InvalidBinding(*id));
        }
        let effects = {
            let mut manager = root.manager.lock().unwrap();
            let effects = manager.configure_established(configuration)?;
            bindings.retain(|id, binding| {
                binding.idle = manager.is_idle(*id);
                manager.contains_session(*id)
            });
            effects
        };
        state.bindings = bindings;
        Ok(effects)
    }

    pub(super) fn update_catchup_configuration(
        &self,
        previous: Option<ReplicaConfiguration>,
        current: ReplicaConfiguration,
    ) -> Result<Vec<i64>, PrimaryError> {
        self.update_configuration(previous, current, true)
    }

    pub(super) fn update_current_configuration(
        &self,
        current: ReplicaConfiguration,
    ) -> Result<Vec<i64>, PrimaryError> {
        self.update_configuration(None, current, false)
    }

    fn update_configuration(
        &self,
        previous: Option<ReplicaConfiguration>,
        current: ReplicaConfiguration,
        catchup_configuration: bool,
    ) -> Result<Vec<i64>, PrimaryError> {
        let root = self.root()?;
        let (effects, _activity) = {
            let mut state = root.state.lock().unwrap();
            Root::check_open(&state)?;
            if current.write_quorum == 1 && !current.replicas.is_empty() {
                return Err(PrimaryError::InvalidCurrentQuorum);
            }
            let mut created = BTreeMap::<i64, (Route, ReplicationSession)>::new();
            let mut routes = BTreeMap::new();
            let mut promotions = Vec::new();
            let manager = root.manager.lock().unwrap();
            if let Some(fault) = manager.build_fault() {
                return Err(ManagerError::ReportedFault {
                    description: fault.description,
                    error: fault.error.to_string(),
                }
                .into());
            }
            let mut resolve =
                |set: ReplicaConfiguration, is_current: bool| -> Result<ReplicaSet, PrimaryError> {
                    let mut members = Vec::new();
                    for replica in set.replicas {
                        root.check_route(&replica.route)?;
                        let id = replica.route.destination.replica;
                        if replica
                            .progress
                            .is_some_and(|progress| progress < 0 || progress == Lsn::MAX)
                        {
                            return Err(PrimaryError::InvalidProgress(id));
                        }
                        if is_current
                            && replica.progress.is_none()
                            && !manager.accepts_unknown_current(id)
                        {
                            return Err(PrimaryError::UnknownCurrentProgress(id));
                        }
                        // Native configuration lookup is ID-based. An existing root
                        // retains its bound endpoint, incarnation, callback and ACKs.
                        let (route, session) = if let Some(binding) = state.bindings.get(&id) {
                            (binding.route.clone(), binding.session.clone())
                        } else if let Some(binding) = created.get(&id) {
                            binding.clone()
                        } else {
                            let (identity, epoch) = manager
                                .build_identity(&replica.route, replica.remote_incarnation)?;
                            let session = ReplicationSession::new(
                                identity,
                                epoch,
                                replica.progress.unwrap_or(-1),
                                root.sender,
                                root.send.clone(),
                                root.clock.now(),
                            );
                            session.open();
                            let binding = (replica.route, session);
                            created.insert(id, binding.clone());
                            binding
                        };
                        if is_current && manager.has_idle_membership(id) {
                            promotions.push(session.clone());
                        }
                        routes.insert(id, route);
                        members.push(SessionMember {
                            replica_id: id,
                            session,
                            must_catchup: replica.must_catchup,
                        });
                    }
                    Ok(ReplicaSet {
                        write_quorum: set.write_quorum,
                        members,
                    })
                };
            let current = resolve(current, true)?;
            let previous = previous.map(|set| resolve(set, false)).transpose()?;
            drop(manager);
            let effects = Self::install_configuration(
                &root,
                &mut state,
                EstablishedConfiguration {
                    current,
                    previous,
                    catchup_configuration,
                },
                routes.into_values().collect(),
            )?;
            for session in promotions {
                session.promote_to_active_secondary(root.clock.now());
            }
            (effects, root.activity(&mut state))
        };
        let paused = effects.paused_previous_replay.clone();
        effects.dispatch(root.clock.now())?;
        Ok(paused)
    }

    pub(super) fn begin_build(
        &self,
        config: BuildConfig,
        provider: Arc<dyn StateProvider>,
        callbacks: BuildCallbacks,
    ) -> Result<BuildIdleHandle, PrimaryError> {
        self.begin_build_with_cut(config, provider, callbacks, || {})
    }

    pub(super) fn begin_build_with_cut(
        &self,
        config: BuildConfig,
        provider: Arc<dyn StateProvider>,
        callbacks: BuildCallbacks,
        acquired: impl FnOnce(),
    ) -> Result<BuildIdleHandle, PrimaryError> {
        let root = self.root()?;
        let (handle, pending, start, _activity) = {
            let mut state = root.state.lock().unwrap();
            acquired();
            Root::check_open(&state)?;
            root.check_route(&config.route)?;
            let route = config.route.clone();
            let (build, pending, start) = BuildIdle::prepare(
                &root.manager,
                config,
                provider,
                callbacks,
                root.clock.clone(),
            )?;
            let handle = build.handle();
            state.bindings.insert(
                route.destination.replica,
                Binding {
                    route,
                    session: handle.session(),
                    owner: SessionOwner::Build(handle.clone()),
                    idle: true,
                },
            );
            state.queued.push(build);
            (handle, pending, start, root.activity(&mut state))
        };
        let result = pending.dispatch(root.clock.now());
        start.dispatch();
        result?;
        Ok(handle)
    }

    pub(super) fn enqueue(
        &self,
        data: OperationData,
        policy: BufferPolicy,
        limits: BufferLimits,
    ) -> Result<PrimaryAdmission, PrimaryError> {
        self.enqueue_with_lease(data, policy, limits, ())
    }

    pub(super) fn enqueue_with_lease(
        &self,
        data: OperationData,
        policy: BufferPolicy,
        limits: BufferLimits,
        lease: impl Sized,
    ) -> Result<PrimaryAdmission, PrimaryError> {
        let root = self.root()?;
        let (admission, _activity) = {
            let mut state = root.state.lock().unwrap();
            Root::check_open(&state)?;
            let admission = root.manager.lock().unwrap().enqueue(data, policy, limits);
            (admission, root.activity(&mut state))
        };
        // The host admission cut covers only assignment, never foreign effects
        // or policy callbacks. Early rejection also drops the lease.
        drop(lease);
        let admission = match admission {
            Ok(admission) => {
                admission.effects.dispatch(root.clock.now())?;
                Ok(PrimaryAdmission {
                    lsn: admission.operation.operation.lsn(),
                    completion: admission.completion,
                })
            }
            Err(error) => Err(PrimaryError::from(error)),
        };
        if let Err(error) = root.queue_pressure() {
            // Never hide an already admitted LSN behind a policy error.
            root.state.lock().unwrap().failure.get_or_insert(error);
            root.wake.notify_one();
        }
        admission
    }

    pub(super) fn begin_catchup(
        &self,
        mode: CatchupMode,
    ) -> Result<CompletionReceiver, PrimaryError> {
        let root = self.root()?;
        let (receiver, effects, _activity) = {
            let mut state = root.state.lock().unwrap();
            Root::check_open(&state)?;
            let (receiver, effects) = root.manager.lock().unwrap().begin_catchup(mode)?;
            (receiver, effects, root.activity(&mut state))
        };
        effects.dispatch(root.clock.now())?;
        Ok(receiver)
    }

    pub(super) fn remove_idle(&self, id: i64) -> Result<(), PrimaryError> {
        let root = self.root()?;
        let (effects, _activity) = {
            let mut state = root.state.lock().unwrap();
            Root::check_open(&state)?;
            let effects = {
                let mut manager = root.manager.lock().unwrap();
                let effects = manager.remove_idle(id)?;
                if !manager.contains_session(id) {
                    state.bindings.remove(&id);
                } else if let Some(binding) = state.bindings.get_mut(&id) {
                    binding.idle = false;
                }
                effects
            };
            (effects, root.activity(&mut state))
        };
        effects.dispatch(root.clock.now())?;
        Ok(())
    }

    fn select(
        &self,
        envelope: &wire::Envelope,
    ) -> Result<(Arc<Root>, Binding, Activity), PrimaryError> {
        let root = self.root()?;
        let (binding, activity) = {
            let mut state = root.state.lock().unwrap();
            if matches!(state.phase, Phase::Closing | Phase::Closed) {
                return Err(PrimaryError::Closed);
            }
            if state.checking_data_loss {
                return Err(PrimaryError::CheckingDataLoss);
            }
            let id = envelope
                .source
                .as_ref()
                .ok_or(ProtocolError::Missing("source"))?
                .replica;
            let binding = state
                .bindings
                .get(&id)
                .ok_or(PrimaryError::UnknownSession(id))?
                .clone();
            binding.route.check(envelope, true)?;
            if envelope.version != 1 {
                return Err(ProtocolError::UnsupportedVersion(envelope.version).into());
            }
            if envelope.message_id.is_empty() {
                return Err(ProtocolError::Missing("message_id").into());
            }
            (binding, root.activity(&mut state))
        };
        Ok((root, binding, activity))
    }

    pub(super) fn receive_context(
        &self,
        envelope: &wire::Envelope,
    ) -> Result<ContextObservation, PrimaryError> {
        let (_root, binding, _activity) = self.select(envelope)?;
        if !binding.idle {
            return Err(PrimaryError::NotBuilding(binding.route.destination.replica));
        }

        match binding.owner {
            SessionOwner::Build(build) => Ok(build.receive_context(envelope)?),
            SessionOwner::Established(_) => {
                Err(PrimaryError::NotBuilding(binding.route.destination.replica))
            }
        }
    }

    pub(super) fn ingress_faulted(&self) -> Result<bool, PrimaryError> {
        let root = self.root()?;
        let state = root.state.lock().unwrap();
        Ok(matches!(state.failure, Some(PrimaryError::Provider(_)))
            || root.manager.lock().unwrap().check_reported_fault().is_err())
    }

    pub(super) fn receive_ack(&self, envelope: &wire::Envelope) -> Result<(), PrimaryError> {
        let (root, binding, _activity) = self.select(envelope)?;
        match binding.owner {
            SessionOwner::Build(build) => {
                build.receive_ack_for_role(envelope, binding.idle, || root.progress())?;
            }
            SessionOwner::Established(copy) => {
                let Some(wire::envelope::Body::ReplicationAck(body)) = &envelope.body else {
                    return Err(ProtocolError::Shape("expected replication ACK").into());
                };
                let latest = root.manager.lock().unwrap().last_sequence_number()?;
                if body.replication_received < 0
                    || body.replication_applied < 0
                    || body.replication_applied > body.replication_received
                    || body.replication_received > latest
                {
                    return Err(ProtocolError::Shape("invalid replication ACK progress").into());
                }
                let processor = binding.session.update_ack_at(
                    AckUpdate {
                        replication_received: body.replication_received,
                        replication_applied: body.replication_applied,
                        copy_received: -1,
                        copy_applied: -1,
                        process_copy: false,
                        message_id: envelope.message_id.clone(),
                    },
                    Some(Arc::new(|| {})),
                    root.clock.now(),
                );
                deliver_ack(processor, &copy.handle(), &root.clock, || root.progress())?;
            }
        }
        root.wake.notify_one();
        Ok(())
    }

    pub(super) fn close(&self, quorum_wait: Duration) -> Result<(), PrimaryError> {
        let root = self.root()?;
        let selected = {
            let mut state = root.state.lock().unwrap();
            if state.phase != Phase::Open {
                return Ok(());
            }
            if quorum_wait.is_zero() {
                Some(root.close_effects(&mut state))
            } else {
                state.phase = Phase::PreClose(root.clock.now().saturating_add(quorum_wait));
                root.wake.notify_one();
                None
            }
        };
        if let Some((effects, _activity)) = selected {
            root.deliver_close(effects)?;
        }
        Ok(())
    }

    pub(super) fn close_for_secondary(&self) -> Result<(), PrimaryError> {
        let root = self.root()?;
        let selected = {
            let mut state = root.state.lock().unwrap();
            Root::check_open(&state)?;
            root.manager.lock().unwrap().check_reported_fault()?;
            state.creating_secondary = true;
            root.close_effects(&mut state)
        };
        let (effects, _activity) = selected;
        root.deliver_close(effects)
    }

    pub(super) fn update_epoch(&self, epoch: Epoch) -> Result<Lsn, PrimaryError> {
        let root = self.root()?;
        let state = root.state.lock().unwrap();
        Root::check_open(&state)?;
        if let Some(error) = &state.failure {
            return Err(error.clone());
        }
        Ok(root.manager.lock().unwrap().update_epoch(epoch)?)
    }

    pub(super) fn report_provider_fault(&self, error: Arc<crate::error::RuntimeError>) {
        if let Ok(root) = self.root() {
            root.state
                .lock()
                .unwrap()
                .failure
                .get_or_insert(PrimaryError::Provider(error));
            root.wake.notify_one();
        }
    }

    pub(super) fn abort(&self) -> Result<(), PrimaryError> {
        let root = self.root()?;
        let selected = {
            let mut state = root.state.lock().unwrap();
            state.aborted = true;
            if matches!(state.phase, Phase::Open | Phase::PreClose(_)) {
                Some(root.close_effects(&mut state))
            } else {
                None
            }
        };
        if let Some((effects, _activity)) = selected {
            root.deliver_close(effects)?;
        }
        root.wake.notify_one();
        Ok(())
    }
}

type BuildDriver = BoxFuture<'static, (BuildIdle, Result<BuildEnd, BuildError>)>;

pub(super) struct Primary {
    root: Arc<Root>,
    drivers: FuturesUnordered<BuildDriver>,
    retained: Vec<BuildIdle>,
}

impl Primary {
    pub(super) fn new(config: PrimaryConfig, clock: Clock) -> Result<Self, PrimaryError> {
        Self::create(config, clock, None)
    }

    pub(super) fn from_secondary(
        config: PrimaryConfig,
        clock: Clock,
        transfer: QueueTransfer,
    ) -> Result<Self, PrimaryError> {
        Self::create(config, clock, Some(transfer))
    }

    fn create(
        config: PrimaryConfig,
        clock: Clock,
        transfer: Option<QueueTransfer>,
    ) -> Result<Self, PrimaryError> {
        if config.endpoint.partition.len() != 16 || config.endpoint.incarnation.len() != 16 {
            return Err(ProtocolError::Shape("endpoint GUID length").into());
        }
        if !config.sender.is_valid() {
            return Err(PrimaryError::InvalidSenderConfig);
        }
        if !config.queue_policy.is_valid() {
            return Err(PrimaryError::InvalidQueuePolicy);
        }
        let manager = match transfer {
            Some(transfer) => ReplicaManager::from_transferred(
                config.queue,
                transfer,
                config.endpoint.replica,
                config.address.clone(),
                config.epoch,
                config.allow_multiple_quorum_set,
            )?,
            None => ReplicaManager::new(
                config.queue,
                config.initial_progress,
                config.endpoint.replica,
                config.address.clone(),
                config.epoch,
                config.allow_multiple_quorum_set,
            )?,
        };
        Ok(Self {
            root: Arc::new(Root {
                state: Mutex::new(State {
                    phase: Phase::Open,
                    bindings: BTreeMap::new(),
                    queued: Vec::new(),
                    activities: 0,
                    aborted: false,
                    quorum_wait_expired: false,
                    failure: None,
                    creating_secondary: false,
                    transferred: false,
                    checking_data_loss: false,
                    queue_policy: config.queue_policy,
                    policy_deadline: clock.now().saturating_add(config.queue_policy.interval()),
                    idle_refreshed_at: clock.now(),
                }),
                manager: Arc::new(Mutex::new(manager)),
                endpoint: config.endpoint,
                address: config.address,
                clock,
                sender: config.sender,
                send: config.send,
                wake: Notify::new(),
                alive: AtomicBool::new(true),
            }),
            drivers: FuturesUnordered::new(),
            retained: Vec::new(),
        })
    }

    pub(super) fn handle(&self) -> PrimaryHandle {
        PrimaryHandle(Arc::downgrade(&self.root))
    }

    pub(super) fn begin_data_loss(&self) -> Result<(), PrimaryError> {
        let mut state = self.root.state.lock().unwrap();
        Root::check_open(&state)?;
        self.root.manager.lock().unwrap().check_reported_fault()?;
        state.checking_data_loss = true;
        Ok(())
    }

    pub(super) fn reset_replication_queue(&self, progress: Lsn) -> Result<(), PrimaryError> {
        let state = self.root.state.lock().unwrap();
        Root::check_active(&state)?;
        if !state.checking_data_loss {
            return Err(PrimaryError::NoDataLoss);
        }
        if state.activities != 0 {
            return Err(PrimaryError::PendingActivity);
        }
        Ok(self
            .root
            .manager
            .lock()
            .unwrap()
            .reset_replication_queue(progress)?)
    }

    pub(super) fn end_data_loss(&self) -> Result<(), PrimaryError> {
        let mut state = self.root.state.lock().unwrap();
        if !state.checking_data_loss {
            return Err(PrimaryError::NoDataLoss);
        }
        state.checking_data_loss = false;
        Root::check_active(&state)
    }

    pub(super) fn take_for_secondary(
        &mut self,
        config: QueueConfig,
    ) -> Result<QueueTransfer, PrimaryError> {
        let mut state = self.root.state.lock().unwrap();
        if state.phase != Phase::Closed
            || !state.creating_secondary
            || state.aborted
            || state.transferred
        {
            return Err(PrimaryError::Closed);
        }
        if let Some(error) = &state.failure {
            return Err(error.clone());
        }
        let transfer = self
            .root
            .manager
            .lock()
            .unwrap()
            .take_for_secondary(config)?;
        state.transferred = true;
        Ok(transfer)
    }

    fn collect_builds(&mut self) {
        let queued = std::mem::take(&mut self.root.state.lock().unwrap().queued);
        for mut build in queued {
            self.drivers.push(
                async move {
                    let result = build.run().await;
                    (build, result)
                }
                .boxed(),
            );
        }
        let state = self.root.state.lock().unwrap();
        // Release removed roots outside serialization; their providers may own destructors.
        let retained = std::mem::take(&mut self.retained);
        let (keep, removed): (Vec<_>, Vec<_>) = retained.into_iter().partition(|build| {
            state.bindings.values().any(|binding| match &binding.owner {
                SessionOwner::Build(handle) => handle.same_instance(&build.handle()),
                SessionOwner::Established(_) => false,
            })
        });
        drop(state);
        self.retained = keep;
        drop(removed);
    }

    fn finish_build(
        &mut self,
        build: BuildIdle,
        result: Result<BuildEnd, BuildError>,
    ) -> Result<(), PrimaryError> {
        if let Err(error) = result {
            self.root.state.lock().unwrap().queued.push(build);
            return Err(error.into());
        }
        self.retained.push(build);
        Ok(())
    }

    /// Explicit provider/build drainage, separate from native primary Close.
    pub(super) async fn drain_builds(&mut self) -> Result<(), PrimaryError> {
        std::future::poll_fn(|cx| self.poll_drain_builds(cx)).await
    }

    pub(super) fn poll_drain_builds(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), PrimaryError>> {
        use futures::Stream;
        use std::task::Poll;

        loop {
            self.collect_builds();
            match std::pin::Pin::new(&mut self.drivers).poll_next(cx) {
                Poll::Ready(Some((build, result))) => self.finish_build(build, result)?,
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    pub(super) async fn run(&mut self) -> Result<PrimaryEnd, PrimaryError> {
        loop {
            let root = self.root.clone();
            let wake = root.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            self.collect_builds();
            let (deadline, selected_close, end) = {
                let mut state = root.state.lock().unwrap();
                if matches!(state.phase, Phase::Open | Phase::PreClose(_))
                    && let Some(error) = &state.failure
                {
                    return Err(error.clone());
                }
                let mut deadline = None;
                let mut selected = None;
                if let Phase::PreClose(until) = state.phase {
                    let pending = root.manager.lock().unwrap().pending_count();
                    if pending == 0 || until <= root.clock.now() {
                        state.quorum_wait_expired = pending != 0;
                        selected = Some(root.close_effects(&mut state));
                    } else {
                        deadline = Some(until);
                    }
                }
                let end = if matches!(state.phase, Phase::Closing | Phase::Closed)
                    && state.activities == 0
                {
                    state.phase = Phase::Closed;
                    Some(PrimaryEnd {
                        result: state
                            .failure
                            .clone()
                            .or_else(|| state.aborted.then_some(PrimaryError::Aborted))
                            .map_or(Ok(()), Err),
                        aborted: state.aborted,
                        quorum_wait_expired: state.quorum_wait_expired,
                    })
                } else {
                    if matches!(state.phase, Phase::Open | Phase::PreClose(_)) {
                        deadline =
                            Some(deadline.map_or(state.policy_deadline, |old| {
                                old.min(state.policy_deadline)
                            }));
                        for binding in state
                            .bindings
                            .values()
                            .filter(|binding| binding.drives_replication_timer())
                        {
                            if let Some(next) = binding.session.retry_deadline() {
                                deadline =
                                    Some(deadline.map_or(next, |old: Duration| old.min(next)));
                            }
                            if let Some(next) = binding.session.fault_deadline() {
                                deadline = Some(deadline.map_or(next, |old| old.min(next)));
                            }
                        }
                    }
                    None
                };
                (deadline, selected, end)
            };
            if let Some((effects, _activity)) = selected_close {
                root.deliver_close(effects)?;
                continue;
            }
            if let Some(end) = end {
                return Ok(end);
            }
            tokio::select! {
                () = &mut wake => {}
                Some((build, result)) = self.drivers.next(), if !self.drivers.is_empty() => {
                    self.finish_build(build, result)?;
                }
                () = async {
                    match deadline {
                        Some(deadline) => match tokio::time::Instant::now()
                            .checked_add(deadline.saturating_sub(root.clock.now())) {
                                Some(instant) => tokio::time::sleep_until(instant).await,
                                None => std::future::pending().await,
                            },
                        None => std::future::pending().await,
                    }
                } => {
                    let selected = {
                        let mut state = root.state.lock().unwrap();
                        if matches!(state.phase, Phase::Open | Phase::PreClose(_)) {
                            let batches: Vec<_> = state.bindings.values()
                                .filter(|binding| binding.drives_replication_timer())
                                .filter(|binding| binding.session.retry_deadline().is_some_and(|due| due <= root.clock.now()))
                                .map(|binding| binding.session.on_timer(root.clock.now())).collect();
                            let faults: Vec<_> = state.bindings.values()
                                .filter(|binding| binding.drives_replication_timer())
                                .filter_map(|binding| binding.session.on_fault_timer(root.clock.now()))
                                .collect();
                            Some((batches, faults, root.activity(&mut state)))
                        } else {
                            None
                        }
                    };
                    if let Some((batches, faults, _activity)) = selected {
                        for batch in batches { batch.dispatch(); }
                        for fault in faults { fault.dispatch(); }
                    }
                    if let Err(error) = root.policy_tick() {
                        root.state.lock().unwrap().failure.get_or_insert(error.clone());
                        return Err(error);
                    }
                }
            }
        }
    }
}

impl Drop for Primary {
    fn drop(&mut self) {
        self.root.alive.store(false, Ordering::Release);
        self.root.wake.notify_waiters();
    }
}

#[cfg(test)]
mod tests;
