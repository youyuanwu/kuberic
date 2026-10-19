// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation of selected ReplicaManager.h/.cpp and
// PrimaryReplicator.CatchupAsyncOperation.cpp responsibilities:
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d
// src/prod/src/Reliability/Replication/. See sf-v1-port-provenance.md.

//! Unselected, short-serialized primary ownership core, not UpdateConfiguration
//! or a full PrimaryReplicator. The caller establishes and opens real sessions,
//! supplies their logical replica IDs and serializes these `&mut self` calls.
//! Session identity is checked, but this is not transport authentication.
//!
//! BuildIdle owns fresh idle-session creation and registers its actual copy target
//! and cancellation here. Primary resolves descriptor-created/reused sessions and
//! idle-to-active session promotion. Private queue policy excludes faulted
//! senders from reclamation, not quorum/catch-up. Host fault handling is separate.
//! The private role owner supplies queue transfer, epoch and data-loss orchestration.
//! Deliver returned effects after releasing the owner's guard. ACK processors
//! must retain their ordered copy handoffs; call `update_progress` when their
//! primary-progress handoff is delivered, not on raw ACK ingress.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use tokio::sync::oneshot;

use crate::application::{Lsn, OperationData};
use crate::protocol::types::Epoch;

use super::buffers::{BufferLimits, BufferPolicy};
use super::build_idle::BuildIdleHandle;
use super::copy_async_operation::CopyFault;
use super::copy_establishment::PrimaryEstablishment;
use super::operation_queue::QueueConfig;
use super::replication_queue_manager::{
    QueueAdmissionError, QueueNormalizationError, QueueTransfer, QueueUpdate,
    ReplicationQueueManager,
};
use super::sender::{SenderOperation, SenderProgress};
use super::session::{ReplicationSession, SessionIdentity, SessionSendBatch};
use super::transport::Route;

mod policy;
pub(super) use policy::QueuePolicyConfig;

#[derive(Clone)]
#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 membership seam"))]
pub(super) struct SessionMember {
    pub(super) replica_id: i64,
    pub(super) session: ReplicationSession,
    pub(super) must_catchup: bool,
}

#[derive(Clone)]
#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 replica set"))]
pub(super) struct ReplicaSet {
    /// Includes the primary's immediate vote, unlike `members`.
    pub(super) write_quorum: usize,
    pub(super) members: Vec<SessionMember>,
}

#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 configuration"))]
pub(super) struct EstablishedConfiguration {
    pub(super) current: ReplicaSet,
    /// None deactivates PC. Some(W1, []) is an active primary-only PC.
    pub(super) previous: Option<ReplicaSet>,
    pub(super) catchup_configuration: bool,
}

#[derive(Clone)]
#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 idle membership"))]
pub(super) struct EstablishedIdle {
    pub(super) replica_id: i64,
    pub(super) session: ReplicationSession,
    /// Supplied by the actual copy owner, not a build-success assertion.
    pub(super) finish_copy_target: Option<Lsn>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 terminal outcome")
)]
pub(super) enum CompletionError {
    /// Native ReplicaManager::CloseInnerObjects cancels remaining replicate ops.
    OperationCanceled,
    /// PrimaryReplicator::UpdateCatchupOperation observes the closed manager.
    ObjectClosed,
}

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 validation errors")
)]
pub(super) enum ConfigurationError {
    InvalidPrimary,
    InvalidEpoch,
    InvalidInitialProgress,
    InvalidQueueConfig,
    ZeroWriteQuorum,
    LocalReplica(i64),
    DuplicateReplica(i64),
    InvalidSessionIdentity(i64),
    SessionRootChanged(i64),
    SessionIdentityAliased(i64),
    PreviousConfigurationExpanded(i64),
    UnexpectedMustCatchup(i64),
    MissingCurrentReplay(i64),
    IdleAlreadyExists(i64),
    IdleNotFound(i64),
    InvalidCopyTarget(i64),
    FaultedIdlePromotion(i64),
}

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 manager errors"))]
pub(super) enum ManagerError {
    ObjectClosed,
    ReconfigurationPending,
    QueueNormalization(QueueNormalizationError),
    Configuration(ConfigurationError),
    Admission(QueueAdmissionError),
    InvalidSessionProgress {
        replica_id: i64,
        received: Lsn,
        applied: Lsn,
        latest: Lsn,
    },
    CatchupAlreadyPending,
    BuildLifecycle(String),
    BuildProgress(String),
    ReportedFault {
        description: &'static str,
        error: String,
    },
}

impl From<ConfigurationError> for ManagerError {
    fn from(value: ConfigurationError) -> Self {
        Self::Configuration(value)
    }
}

#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 completion seam"))]
pub(super) type CompletionReceiver = oneshot::Receiver<Result<(), CompletionError>>;
type CompletionSender = oneshot::Sender<Result<(), CompletionError>>;

#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 admission"))]
pub(super) struct Admission {
    pub(super) operation: SenderOperation,
    pub(super) completion: CompletionReceiver,
    pub(super) effects: ManagerEffects,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 catchup mode"))]
pub(super) enum CatchupMode {
    All,
    WriteQuorum,
}

struct CatchupWait {
    mode: CatchupMode,
    completion: CompletionSender,
}

enum SessionEffect {
    Send {
        session: ReplicationSession,
        operation: SenderOperation,
        completed_hint: Lsn,
    },
    Replay {
        session: ReplicationSession,
        operations: Vec<SenderOperation>,
        completed_hint: Lsn,
    },
    Close(ReplicationSession),
}

/// Dropping an undispatched batch is an observable channel cancellation, never
/// success. The integration owner must deliver it, including during shutdown.
#[must_use = "dispatch sends and terminal completions outside the owner guard"]
#[derive(Default)]
#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 manager effects"))]
pub(super) struct ManagerEffects {
    sessions: Vec<SessionEffect>,
    completions: Vec<(CompletionSender, Result<(), CompletionError>)>,
    pub(super) queue_update: Option<QueueUpdate>,
    /// Native newly established PC-only sessions may lack retained replay.
    /// Their sender is paused by a staged close; no successful build is claimed.
    pub(super) paused_previous_replay: Vec<i64>,
    /// A malformed session cursor can prevent close's final cap-release update.
    /// Shutdown still drains every waiter with an explicit terminal result.
    pub(super) close_progress_error: Option<ManagerError>,
    build_cancellations: Vec<BuildIdleHandle>,
    build_wakes: Vec<BuildIdleHandle>,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 effects dispatch")
)]
impl ManagerEffects {
    /// Always delivers terminal cleanup before reporting a failed final update.
    pub(super) fn dispatch(self, now: Duration) -> Result<(), ManagerError> {
        for effect in self.sessions {
            match effect {
                SessionEffect::Send {
                    session,
                    operation,
                    completed_hint,
                } => session.add(operation, completed_hint, now).dispatch(),
                SessionEffect::Replay {
                    session,
                    operations,
                    completed_hint,
                } => session
                    .add_replay(operations, completed_hint, now)
                    .dispatch(),
                SessionEffect::Close(session) => session.close(),
            }
        }
        for build in self.build_wakes {
            build.wake_timers();
        }
        for (completion, result) in self.completions {
            // A dropped waiter does not roll back its already admitted operation.
            let _ = completion.send(result);
        }
        let mut error = self.close_progress_error;
        for build in self.build_cancellations {
            if let Err(failure) = build.cancel_for_removal()
                && error.is_none()
            {
                error = Some(ManagerError::BuildLifecycle(failure.to_string()));
            }
        }
        error.map_or(Ok(()), Err)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 catchup view"))]
pub(super) struct CatchupObservation {
    pub(super) quorum: Lsn,
    pub(super) all: Lsn,
    pub(super) latest: Lsn,
    pub(super) configuration_boundary: Lsn,
    pub(super) minimum_must_catchup: Option<Lsn>,
}

#[derive(Clone, Copy)]
struct ConfigProgress {
    quorum: Option<Lsn>,
    all_applied: Option<Lsn>,
    completed: Option<Lsn>,
    minimum_applied: Option<Lsn>,
}

struct Observation {
    current: Option<ConfigProgress>,
    normal: Option<(Option<Lsn>, Option<Lsn>)>,
}

#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 primary owner"))]
pub(super) struct ReplicaManager {
    queue: ReplicationQueueManager,
    local_replica_id: i64,
    primary_endpoint: String,
    epoch: Epoch,
    allow_multiple_quorum_set: bool,
    active: bool,
    current: Option<ReplicaSet>,
    previous: Option<ReplicaSet>,
    idle: Vec<EstablishedIdle>,
    builds: BTreeMap<i64, BuildIdleHandle>,
    build_fault: Option<CopyFault>,
    catchup_configuration: bool,
    // Native majorityReplicasApplyAckedLSNinCC_/allReplicasApplyAckedLSNinCC_.
    // Sampling sender state alone must not bypass the ordered primary handoff.
    cached_current: Option<ConfigProgress>,
    pending: BTreeMap<Lsn, CompletionSender>,
    catchup_wait: Option<CatchupWait>,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 primary owner API")
)]
impl ReplicaManager {
    pub(super) fn from_transferred(
        queue_config: QueueConfig,
        transfer: QueueTransfer,
        local_replica_id: i64,
        primary_endpoint: String,
        epoch: Epoch,
        allow_multiple_quorum_set: bool,
    ) -> Result<Self, ManagerError> {
        let mut manager = Self::new(
            queue_config,
            0,
            local_replica_id,
            primary_endpoint,
            epoch,
            allow_multiple_quorum_set,
        )?;
        manager.queue = ReplicationQueueManager::from_transferred_queue(
            queue_config,
            transfer.queue,
            transfer.original_epochs,
        )
        .map_err(ManagerError::QueueNormalization)?;
        Ok(manager)
    }

    pub(super) fn update_epoch(&mut self, epoch: Epoch) -> Result<Lsn, ManagerError> {
        self.ensure_active()?;
        if epoch < self.epoch || epoch.data_loss_number < 0 || epoch.configuration_number < 0 {
            return Err(ConfigurationError::InvalidEpoch.into());
        }
        self.check_reported_fault()?;
        self.epoch = epoch;
        for session in self.established_sessions().into_values() {
            session.update_sending_epoch(epoch);
        }
        Ok(self.queue.last_sequence_number())
    }

    pub(super) fn take_for_secondary(
        &mut self,
        config: QueueConfig,
    ) -> Result<QueueTransfer, ManagerError> {
        if self.active {
            return Err(ManagerError::ReconfigurationPending);
        }
        self.queue
            .take_for_secondary(config)
            .map_err(ManagerError::QueueNormalization)
    }

    pub(super) fn reset_replication_queue(&mut self, progress: Lsn) -> Result<(), ManagerError> {
        self.ensure_active()?;
        self.check_reported_fault()?;
        if !self.pending.is_empty() {
            return Err(ManagerError::QueueNormalization(
                QueueNormalizationError::PendingOperations,
            ));
        }
        self.queue
            .reset_queue(progress)
            .map_err(ManagerError::QueueNormalization)
    }

    pub(super) fn new(
        queue_config: QueueConfig,
        initial_progress: Lsn,
        local_replica_id: i64,
        primary_endpoint: String,
        epoch: Epoch,
        allow_multiple_quorum_set: bool,
    ) -> Result<Self, ManagerError> {
        if primary_endpoint.is_empty() {
            return Err(ConfigurationError::InvalidPrimary.into());
        }
        if epoch.data_loss_number < 0 || epoch.configuration_number < 0 {
            return Err(ConfigurationError::InvalidEpoch.into());
        }
        if initial_progress < 0 || initial_progress == Lsn::MAX {
            return Err(ConfigurationError::InvalidInitialProgress.into());
        }
        if queue_config.initial_capacity <= 1
            || !queue_config.initial_capacity.is_power_of_two()
            || (queue_config.max_capacity != 0
                && (!queue_config.max_capacity.is_power_of_two()
                    || queue_config.initial_capacity > queue_config.max_capacity))
            || (queue_config.max_capacity == 0 && queue_config.max_bytes == 0)
        {
            return Err(ConfigurationError::InvalidQueueConfig.into());
        }
        Ok(Self {
            // Only the consuming transfer constructor normalizes issued-LSN gaps.
            queue: ReplicationQueueManager::new(queue_config, initial_progress),
            local_replica_id,
            primary_endpoint,
            epoch,
            allow_multiple_quorum_set,
            active: true,
            current: None,
            previous: None,
            idle: Vec::new(),
            builds: BTreeMap::new(),
            build_fault: None,
            catchup_configuration: false,
            cached_current: None,
            pending: BTreeMap::new(),
            catchup_wait: None,
        })
    }

    fn ensure_active(&self) -> Result<(), ManagerError> {
        if self.active {
            Ok(())
        } else {
            Err(ManagerError::ObjectClosed)
        }
    }

    pub(super) fn queue(&self) -> &ReplicationQueueManager {
        &self.queue
    }

    pub(super) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub(super) fn last_sequence_number(&self) -> Result<Lsn, ManagerError> {
        self.ensure_active()?;
        Ok(self.queue.last_sequence_number())
    }

    pub(super) fn build_fault(&self) -> Option<&CopyFault> {
        self.build_fault.as_ref()
    }

    pub(super) fn check_reported_fault(&self) -> Result<(), ManagerError> {
        if let Some(fault) = &self.build_fault {
            return Err(ManagerError::ReportedFault {
                description: fault.description,
                error: fault.error.to_string(),
            });
        }
        Ok(())
    }

    pub(super) fn report_build_fault(&mut self, fault: CopyFault) {
        if self.build_fault.is_none() {
            self.build_fault = Some(fault);
        }
    }

    pub(super) fn build_handle(&self, replica_id: i64) -> Option<BuildIdleHandle> {
        self.builds.get(&replica_id).cloned()
    }

    pub(super) fn contains_session(&self, replica_id: i64) -> bool {
        self.is_idle(replica_id)
            || self.current.iter().chain(self.previous.iter()).any(|set| {
                set.members
                    .iter()
                    .any(|member| member.replica_id == replica_id)
            })
    }

    pub(super) fn has_idle_membership(&self, replica_id: i64) -> bool {
        self.idle.iter().any(|idle| idle.replica_id == replica_id)
    }

    pub(super) fn accepts_unknown_current(&self, replica_id: i64) -> bool {
        self.has_idle_membership(replica_id)
            || self.current.as_ref().is_some_and(|set| {
                set.members
                    .iter()
                    .any(|member| member.replica_id == replica_id)
            })
    }

    pub(super) fn is_idle(&self, replica_id: i64) -> bool {
        self.idle.iter().any(|idle| idle.replica_id == replica_id)
            && !self.current.iter().chain(self.previous.iter()).any(|set| {
                set.members
                    .iter()
                    .any(|member| member.replica_id == replica_id)
            })
    }

    pub(super) fn build_identity(
        &self,
        route: &Route,
        remote_incarnation: i64,
    ) -> Result<(SessionIdentity, Epoch), ManagerError> {
        self.ensure_active()?;
        self.check_reported_fault()?;
        if route.source.replica != self.local_replica_id
            || route.sender_address != self.primary_endpoint
        {
            return Err(
                ConfigurationError::InvalidSessionIdentity(route.destination.replica).into(),
            );
        }
        Ok((
            SessionIdentity {
                primary_endpoint: route.sender_address.clone(),
                remote_endpoint: route.remote_address.clone(),
                remote_incarnation,
            },
            self.epoch,
        ))
    }

    fn established_sessions(&self) -> BTreeMap<i64, ReplicationSession> {
        let mut sessions = BTreeMap::new();
        for set in self.current.iter().chain(self.previous.iter()) {
            for member in &set.members {
                sessions.insert(member.replica_id, member.session.clone());
            }
        }
        for idle in &self.idle {
            sessions.insert(idle.replica_id, idle.session.clone());
        }
        sessions
    }

    fn validate_session(
        &self,
        replica_id: i64,
        session: &ReplicationSession,
    ) -> Result<(), ManagerError> {
        if replica_id == self.local_replica_id {
            return Err(ConfigurationError::LocalReplica(replica_id).into());
        }
        let identity = session.identity();
        if identity.primary_endpoint != self.primary_endpoint
            || identity.remote_endpoint.is_empty()
            || identity.remote_endpoint == self.primary_endpoint
            || identity.remote_incarnation < 0
        {
            return Err(ConfigurationError::InvalidSessionIdentity(replica_id).into());
        }
        self.sample_progress(replica_id, session)?;
        Ok(())
    }

    fn sample_progress(
        &self,
        replica_id: i64,
        session: &ReplicationSession,
    ) -> Result<SenderProgress, ManagerError> {
        let progress = session.progress(self.queue.first_lsn_in_replication_queue() - 1);
        let latest = self.queue.last_sequence_number();
        if progress.received > latest || progress.applied > latest {
            return Err(ManagerError::InvalidSessionProgress {
                replica_id,
                received: progress.received,
                applied: progress.applied,
                latest,
            });
        }
        Ok(progress)
    }

    fn validate_set(
        &self,
        set: &ReplicaSet,
        allow_must_catchup: bool,
        sessions: &mut BTreeMap<i64, ReplicationSession>,
    ) -> Result<(), ManagerError> {
        if set.write_quorum == 0 {
            return Err(ConfigurationError::ZeroWriteQuorum.into());
        }
        let mut ids = BTreeSet::new();
        for member in &set.members {
            if !ids.insert(member.replica_id) {
                return Err(ConfigurationError::DuplicateReplica(member.replica_id).into());
            }
            if member.must_catchup && !allow_must_catchup {
                return Err(ConfigurationError::UnexpectedMustCatchup(member.replica_id).into());
            }
            self.validate_session(member.replica_id, &member.session)?;
            Self::validate_root(sessions, member.replica_id, &member.session)?;
            sessions.insert(member.replica_id, member.session.clone());
        }
        Ok(())
    }

    fn validate_root(
        sessions: &BTreeMap<i64, ReplicationSession>,
        replica_id: i64,
        session: &ReplicationSession,
    ) -> Result<(), ManagerError> {
        if let Some(existing) = sessions.get(&replica_id)
            && !existing.same_instance(session)
        {
            return Err(ConfigurationError::SessionRootChanged(replica_id).into());
        }
        for (other_id, other) in sessions {
            if *other_id != replica_id
                && (other.same_instance(session) || other.identity() == session.identity())
            {
                return Err(ConfigurationError::SessionIdentityAliased(replica_id).into());
            }
        }
        Ok(())
    }

    /// Caller-established membership seam, NOT native session creation/build
    /// lookup. Reused replica IDs must retain the exact sender/ACK root.
    /// A new CC member must have retained replay; a new PC-only member may be
    /// explicitly paused when replay is missing (native cpp:1580-1603).
    pub(super) fn configure_established(
        &mut self,
        configuration: EstablishedConfiguration,
    ) -> Result<ManagerEffects, ManagerError> {
        self.ensure_active()?;
        for member in &configuration.current.members {
            if self.has_idle_membership(member.replica_id) && member.session.idle_faulted() {
                return Err(ConfigurationError::FaultedIdlePromotion(member.replica_id).into());
            }
        }
        let old_sessions = self.established_sessions();
        let mut checked = old_sessions.clone();
        self.validate_set(
            &configuration.current,
            configuration.catchup_configuration,
            &mut checked,
        )?;
        if let Some(previous) = &configuration.previous {
            self.validate_set(previous, false, &mut checked)?;
            if let Some(old_previous) = &self.previous
                && !old_previous.members.is_empty()
            {
                for member in &previous.members {
                    if !old_previous
                        .members
                        .iter()
                        .any(|old| old.replica_id == member.replica_id)
                    {
                        return Err(ConfigurationError::PreviousConfigurationExpanded(
                            member.replica_id,
                        )
                        .into());
                    }
                }
            }
        }
        let mut effects = ManagerEffects::default();
        let hint = self.queue.first_lsn_in_replication_queue() - 1;
        let current_ids: BTreeSet<_> = configuration
            .current
            .members
            .iter()
            .map(|member| member.replica_id)
            .collect();
        let retained_idle: Vec<_> = self
            .idle
            .iter()
            .filter(|idle| !current_ids.contains(&idle.replica_id))
            .cloned()
            .collect();
        // Snapshot all fallible progress observations before changing membership,
        // capturing boundaries or transferring any staged effects to the caller.
        let observation = self.observe_sets(
            Some(&configuration.current),
            configuration.previous.as_ref(),
            &retained_idle,
        )?;
        let mut new_sessions = BTreeMap::new();
        for set in std::iter::once(&configuration.current).chain(configuration.previous.iter()) {
            for member in &set.members {
                if new_sessions
                    .insert(member.replica_id, member.session.clone())
                    .is_some()
                    || old_sessions.contains_key(&member.replica_id)
                {
                    continue;
                }
                let first = member.session.apply_lsn(hint).checked_add(1);
                let mut operations = Vec::new();
                let available =
                    first.is_some_and(|first| self.queue.get_operations(first, &mut operations));
                if !available {
                    if current_ids.contains(&member.replica_id) {
                        return Err(
                            ConfigurationError::MissingCurrentReplay(member.replica_id).into()
                        );
                    }
                    effects.paused_previous_replay.push(member.replica_id);
                    effects
                        .sessions
                        .push(SessionEffect::Close(member.session.clone()));
                } else if !operations.is_empty() {
                    effects.sessions.push(SessionEffect::Replay {
                        session: member.session.clone(),
                        operations,
                        completed_hint: hint,
                    });
                }
            }
        }
        // An idle session can also remain in PC, but leaves idle on CC admission.
        self.idle = retained_idle;
        for idle in &self.idle {
            new_sessions.insert(idle.replica_id, idle.session.clone());
        }
        for (id, session) in old_sessions {
            if !new_sessions.contains_key(&id) {
                effects.sessions.push(SessionEffect::Close(session));
            }
        }
        let leaving_idle: Vec<_> = self
            .builds
            .keys()
            .filter(|id| !self.idle.iter().any(|idle| idle.replica_id == **id))
            .copied()
            .collect();
        for id in leaving_idle {
            let build = self.builds.remove(&id).unwrap();
            if !new_sessions.contains_key(&id) {
                effects.build_cancellations.push(build);
            }
        }
        self.current = Some(configuration.current);
        self.previous = configuration.previous;
        self.catchup_configuration = configuration.catchup_configuration;
        // Capture configuration boundary, not per-wait or per-enqueue boundary.
        self.queue.update_catchup_completion_lsn();
        self.advance(observation, &mut effects);
        Ok(effects)
    }

    fn prepare_idle(&self, idle: &EstablishedIdle) -> Result<(Lsn, ManagerEffects), ManagerError> {
        self.ensure_active()?;
        self.validate_session(idle.replica_id, &idle.session)?;
        let sessions = self.established_sessions();
        if sessions.contains_key(&idle.replica_id) {
            return Err(ConfigurationError::IdleAlreadyExists(idle.replica_id).into());
        }
        Self::validate_root(&sessions, idle.replica_id, &idle.session)?;
        if idle
            .finish_copy_target
            .is_some_and(|target| target < 0 || target > self.queue.last_sequence_number())
        {
            return Err(ConfigurationError::InvalidCopyTarget(idle.replica_id).into());
        }
        let Some((first, operations)) = self.queue.pending_for_build() else {
            return Err(ConfigurationError::MissingCurrentReplay(idle.replica_id).into());
        };
        let mut effects = ManagerEffects::default();
        if !operations.is_empty() {
            effects.sessions.push(SessionEffect::Replay {
                session: idle.session.clone(),
                operations,
                completed_hint: self.queue.first_lsn_in_replication_queue() - 1,
            });
        }
        Ok((first, effects))
    }

    /// Caller-established seam retained for isolated quorum scenarios.
    pub(super) fn add_established_idle(
        &mut self,
        idle: EstablishedIdle,
    ) -> Result<ManagerEffects, ManagerError> {
        let (_, effects) = self.prepare_idle(&idle)?;
        self.idle.push(idle);
        Ok(effects)
    }

    pub(super) fn admit_build(
        &mut self,
        idle: EstablishedIdle,
        build: BuildIdleHandle,
        establishment: &mut PrimaryEstablishment,
    ) -> Result<(ManagerEffects, SessionSendBatch), ManagerError> {
        if let Some(fault) = &self.build_fault {
            return Err(ManagerError::ReportedFault {
                description: fault.description,
                error: fault.error.to_string(),
            });
        }
        let (first, effects) = self.prepare_idle(&idle)?;
        let start = establishment
            .prepare_begin(first)
            .map_err(|error| ManagerError::BuildLifecycle(error.to_string()))?;
        self.builds.insert(idle.replica_id, build);
        self.idle.push(idle);
        Ok((effects, start))
    }

    pub(super) fn remove_idle(&mut self, replica_id: i64) -> Result<ManagerEffects, ManagerError> {
        self.ensure_active()?;
        let Some(index) = self
            .idle
            .iter()
            .position(|idle| idle.replica_id == replica_id)
        else {
            return Err(ConfigurationError::IdleNotFound(replica_id).into());
        };
        let mut retained_idle = self.idle.clone();
        let idle = retained_idle.remove(index);
        let observation = self.observe_sets(
            self.current.as_ref(),
            self.previous.as_ref(),
            &retained_idle,
        )?;
        self.idle = retained_idle;
        let mut effects = ManagerEffects::default();
        if let Some(build) = self.builds.remove(&replica_id) {
            effects.build_cancellations.push(build);
        }
        if !self.established_sessions().contains_key(&replica_id) {
            effects.sessions.push(SessionEffect::Close(idle.session));
        }
        self.advance(observation, &mut effects);
        Ok(effects)
    }

    pub(super) fn enqueue(
        &mut self,
        data: OperationData,
        policy: BufferPolicy,
        limits: BufferLimits,
    ) -> Result<Admission, ManagerError> {
        self.ensure_active()?;
        let Some(current) = &self.current else {
            return Err(ManagerError::ReconfigurationPending);
        };
        // Validate observations before admission; an invalid ACK cannot consume
        // an LSN and then hide the admitted operation behind an error.
        let sessions = self.established_sessions();
        for (replica_id, session) in &sessions {
            self.sample_progress(*replica_id, session)?;
        }
        let observation = self.observe()?;
        let only_primary = current.members.is_empty()
            && self
                .previous
                .as_ref()
                .is_none_or(|previous| previous.members.is_empty());
        let operation = self
            .queue
            .enqueue(data, self.epoch, policy, limits)
            .map_err(ManagerError::Admission)?;
        let (completion, receiver) = oneshot::channel();
        let previous = self.pending.insert(operation.operation.lsn(), completion);
        assert!(previous.is_none(), "admission must assign a fresh LSN");
        let mut effects = ManagerEffects::default();
        if only_primary {
            // Native cpp:2161: W1 with remote sessions is NOT this fast path.
            self.advance(observation, &mut effects);
        } else {
            // All observes the live tail; WriteQuorum retains the config boundary.
            self.evaluate_wait(&mut effects);
        }
        let hint = self.queue.first_lsn_in_replication_queue() - 1;
        for session in sessions.into_values() {
            effects.sessions.push(SessionEffect::Send {
                session,
                operation: operation.clone(),
                completed_hint: hint,
            });
        }
        effects.build_wakes.extend(self.builds.values().cloned());
        Ok(Admission {
            operation,
            completion: receiver,
            effects,
        })
    }

    fn has_enough_replicas(set: &ReplicaSet) -> bool {
        set.write_quorum <= 1 || set.members.len() >= set.write_quorum - 1
    }

    fn set_progress(&self, set: &ReplicaSet) -> Result<ConfigProgress, ManagerError> {
        let mut applied = Vec::with_capacity(set.members.len());
        let mut minimum_received = None;
        for member in &set.members {
            let progress = self.sample_progress(member.replica_id, &member.session)?;
            let apply = progress.applied.max(0);
            applied.push(apply);
            if !member.session.active_faulted() {
                minimum_received = min_progress(minimum_received, Some(progress.received.max(0)));
            }
        }
        if set.write_quorum <= 1 {
            // Native Helpers::GetReplicasProgress cpp:358-363 deliberately
            // ignores present CC remotes for W1, including all-applied progress.
            return Ok(ConfigProgress {
                quorum: None,
                all_applied: None,
                completed: None,
                minimum_applied: applied.iter().copied().min(),
            });
        }
        applied.sort_unstable_by(|a, b| b.cmp(a));
        let quorum = Some(applied[set.write_quorum - 2]);
        Ok(ConfigProgress {
            quorum,
            all_applied: applied.last().copied(),
            completed: min_progress(minimum_received, quorum),
            minimum_applied: applied.last().copied(),
        })
    }

    fn observe(&self) -> Result<Observation, ManagerError> {
        self.observe_sets(self.current.as_ref(), self.previous.as_ref(), &self.idle)
    }

    fn observe_sets(
        &self,
        current: Option<&ReplicaSet>,
        previous: Option<&ReplicaSet>,
        idle: &[EstablishedIdle],
    ) -> Result<Observation, ManagerError> {
        let Some(current) = current.filter(|current| Self::has_enough_replicas(current)) else {
            return Ok(Observation {
                current: None,
                normal: None,
            });
        };
        let current_progress = self.set_progress(current)?;
        let mut committed = current_progress.quorum;
        let mut completed = current_progress.completed;
        for idle in idle {
            if idle.session.idle_faulted() {
                continue;
            }
            // ReplicationSession.cpp:285-326, from one checked sender snapshot.
            let progress = self.sample_progress(idle.replica_id, &idle.session)?;
            let target = match self.builds.get(&idle.replica_id) {
                Some(build) => build
                    .finish_copy_target()
                    .map_err(|error| ManagerError::BuildProgress(error.to_string()))?,
                None => idle.finish_copy_target,
            };
            completed = min_progress(completed, Some(progress.idle_progress(target)));
        }
        if let Some(previous) = previous {
            if !Self::has_enough_replicas(previous) {
                // CC observation is still valid for catchup (cpp:2338-2350).
                return Ok(Observation {
                    current: Some(current_progress),
                    normal: None,
                });
            }
            let previous_progress = self.set_progress(previous)?;
            let (previous_committed, previous_completed) =
                if previous.write_quorum == 1 && !previous.members.is_empty() {
                    // Native PC-only W1 special case, cpp:2476-2499. It uses
                    // apply, NOT receive, even though normal cleanup uses receive.
                    let minimum = previous_progress.minimum_applied;
                    (minimum, minimum)
                } else {
                    (previous_progress.quorum, previous_progress.completed)
                };
            committed = min_progress(committed, previous_committed);
            completed = min_progress(completed, previous_completed);
        }
        Ok(Observation {
            current: Some(current_progress),
            normal: Some((committed, completed)),
        })
    }

    fn advance(&mut self, observation: Observation, effects: &mut ManagerEffects) {
        self.cached_current = observation.current;
        self.advance_queue(observation.normal, effects);
        self.evaluate_wait(effects);
    }

    fn advance_queue(
        &mut self,
        normal: Option<(Option<Lsn>, Option<Lsn>)>,
        effects: &mut ManagerEffects,
    ) {
        if let Some((mut committed, mut completed)) = normal {
            if !self.allow_multiple_quorum_set && self.catchup_configuration {
                committed = committed.map(|lsn| self.queue.check_with_catchup_lsn(lsn));
                completed = completed.map(|lsn| self.queue.check_with_catchup_lsn(lsn));
            }
            let update = self
                .queue
                .update_queue(committed, completed)
                .expect("session observations were checked against the admitted queue tail");
            let completed_lsns: Vec<_> = self
                .pending
                .range(..=update.new_committed)
                .map(|(lsn, _)| *lsn)
                .collect();
            for lsn in completed_lsns {
                effects
                    .completions
                    .push((self.pending.remove(&lsn).unwrap(), Ok(())));
            }
            effects.queue_update = Some(update);
        }
    }

    fn catchup_from(&self, progress: ConfigProgress) -> CatchupObservation {
        let view = self
            .queue
            .catchup_progress(progress.quorum, progress.all_applied)
            .expect("current session observations were checked against the queue tail");
        CatchupObservation {
            quorum: view.committed,
            all: view.completed,
            latest: view.latest,
            configuration_boundary: view.previous_last,
            // Unlike cached CC quorum/all, native cpp:1092-1100 samples the
            // marked-session minimum live when the wait is observed.
            minimum_must_catchup: self.current.as_ref().and_then(|current| {
                let hint = self.queue.first_lsn_in_replication_queue() - 1;
                current
                    .members
                    .iter()
                    .filter(|member| member.must_catchup)
                    .map(|member| member.session.apply_lsn(hint))
                    .min()
            }),
        }
    }

    pub(super) fn catchup_observation(&self) -> Result<Option<CatchupObservation>, ManagerError> {
        self.ensure_active()?;
        Ok(self
            .cached_current
            .map(|progress| self.catchup_from(progress)))
    }

    fn evaluate_wait(&mut self, effects: &mut ManagerEffects) {
        let (Some(wait), Some(current)) = (&self.catchup_wait, self.cached_current) else {
            return;
        };
        let view = self.catchup_from(current);
        let achieved = match wait.mode {
            CatchupMode::All => view.all >= view.latest,
            CatchupMode::WriteQuorum => {
                view.quorum >= view.configuration_boundary
                    && view
                        .minimum_must_catchup
                        .is_none_or(|minimum| minimum >= view.quorum)
            }
        };
        if achieved {
            let wait = self.catchup_wait.take().unwrap();
            effects.completions.push((wait.completion, Ok(())));
        }
    }

    pub(super) fn begin_catchup(
        &mut self,
        mode: CatchupMode,
    ) -> Result<(CompletionReceiver, ManagerEffects), ManagerError> {
        self.ensure_active()?;
        if self.catchup_wait.is_some() {
            return Err(ManagerError::CatchupAlreadyPending);
        }
        let (completion, receiver) = oneshot::channel();
        self.catchup_wait = Some(CatchupWait { mode, completion });
        let mut effects = ManagerEffects::default();
        self.evaluate_wait(&mut effects);
        Ok((receiver, effects))
    }

    pub(super) fn cancel_catchup(&mut self) -> ManagerEffects {
        let mut effects = ManagerEffects::default();
        if let Some(wait) = self.catchup_wait.take() {
            effects
                .completions
                .push((wait.completion, Err(CompletionError::OperationCanceled)));
        }
        effects
    }

    pub(super) fn update_progress(&mut self) -> Result<ManagerEffects, ManagerError> {
        self.ensure_active()?;
        let observation = self.observe()?;
        let mut effects = ManagerEffects::default();
        self.advance(observation, &mut effects);
        Ok(effects)
    }

    pub(super) fn close(&mut self) -> ManagerEffects {
        self.close_inner(false)
    }

    pub(super) fn close_for_secondary(&mut self) -> ManagerEffects {
        self.close_inner(true)
    }

    fn close_inner(&mut self, creating_secondary: bool) -> ManagerEffects {
        if !self.active {
            return ManagerEffects::default();
        }
        let mut effects = ManagerEffects::default();
        self.active = false;
        if !self.allow_multiple_quorum_set && self.catchup_configuration {
            // Native cpp:809-818 first removes the cap and completes newly
            // committed writes, THEN cancels the rest, before removing sessions.
            self.catchup_configuration = false;
            match self.observe() {
                Ok(observation) => {
                    self.cached_current = observation.current;
                    self.advance_queue(observation.normal, &mut effects);
                }
                Err(error) => effects.close_progress_error = Some(error),
            }
        }
        for (_, completion) in std::mem::take(&mut self.pending) {
            effects
                .completions
                .push((completion, Err(CompletionError::OperationCanceled)));
        }
        if let Some(wait) = self.catchup_wait.take() {
            effects
                .completions
                .push((wait.completion, Err(CompletionError::ObjectClosed)));
        }
        effects.sessions.extend(
            self.established_sessions()
                .into_values()
                .map(SessionEffect::Close),
        );
        if creating_secondary {
            self.idle.clear();
            match self.observe() {
                Ok(observation) => {
                    self.cached_current = observation.current;
                    self.advance_queue(observation.normal, &mut effects);
                }
                Err(error) => effects.close_progress_error = Some(error),
            }
        }
        effects
            .build_cancellations
            .extend(std::mem::take(&mut self.builds).into_values());
        effects
    }
}

/// None is native NonInitializedLSN, an unconstrained primary-only vote.
fn min_progress(left: Option<Lsn>, right: Option<Lsn>) -> Option<Lsn> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests;
