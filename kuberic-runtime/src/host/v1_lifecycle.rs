// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation from Reliability/Failover/ra:
// FailoverUnitProxy.ReadWriteStatusCalculator.cpp:34-381,
// FailoverUnitProxy.cpp:1399-1455,
// FailoverUnitProxy.ChangeReplicatorRoleAsyncOperation.cpp:99-140,
// FailoverUnitProxy.ChangeReplicaRoleAsyncOperation.cpp:93-150,
// FailoverUnitProxy.CloseAsyncOperation.cpp:30-110,164-276,
// microsoft/service-fabric @3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d.

//! Private caller-driven host preparation, not an RA/FM or production host.
//! The application is already opened. Host membership/intent are explicit;
//! actual child roles and catch-up come from retained callbacks. Cancelled host
//! calls require `resume`; the facade independently retains engine execution.

#![cfg_attr(not(test), expect(dead_code, reason = "unselected V1 host adapter"))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use crate::error::RuntimeError;
use crate::protocol::types::{AccessStatus, Epoch, ReplicaRole};
use crate::replicator::v1::{
    Facade, FacadeSetup, FacadeState, LifecycleObservation, matches_binding,
};
use crate::replicator::{PrimaryReplicator, ReplicaInformation, ReplicaSetQuorumMode, Replicator};
use crate::traits::StatefulServiceReplica;

#[derive(Clone)]
pub(crate) struct RemoteReplica {
    pub(crate) information: ReplicaInformation,
    pub(crate) previous: ReplicaRole,
    pub(crate) current: ReplicaRole,
    pub(crate) is_up: bool,
    pub(crate) ready: bool,
}

#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum HostError {
    #[error("invalid private host input/state: {0}")]
    Input(&'static str),
    #[error("a host callback is outstanding; resume it")]
    Busy,
    #[error("no host callback is outstanding")]
    NoOperation,
    #[error("private host was aborted")]
    Aborted,
    #[error("host child callback: {0}")]
    Callback(Arc<RuntimeError>),
    #[error(transparent)]
    Facade(Arc<RuntimeError>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lifecycle {
    Created,
    Opening,
    Opened,
    Closing,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigurationStage {
    PreWriteStatusRevoke,
    Catchup,
    Activated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fence {
    Authority,
    Fault,
    Retired,
}

struct Projection {
    lifecycle: Lifecycle,
    target: ReplicaRole,
    service_role: ReplicaRole,
    replicator_role: ReplicaRole,
    replicator_open: bool,
    reconfiguring: bool,
    primary_change: bool,
    stage: ConfigurationStage,
    caught_up: bool,
    minimum: usize,
    remotes: Vec<RemoteReplica>,
    authority_fenced: bool,
    faulted: bool,
    retired: bool,
    close_outcome: Option<CloseOutcome>,
}

#[derive(Clone)]
pub(crate) struct PartitionAccess {
    state: Arc<Mutex<Projection>>,
    replicator: FacadeState,
}

impl Projection {
    fn roles_current(&self) -> bool {
        self.service_role == self.target && self.replicator_role == self.target
    }

    /// Production steady-state projection. Membership/readiness is host input,
    /// separate from native queue progress and service-local commitment.
    pub(crate) fn activated_access(
        role: ReplicaRole,
        minimum: usize,
        remotes: Vec<RemoteReplica>,
    ) -> Result<(AccessStatus, AccessStatus), HostError> {
        Projection {
            lifecycle: Lifecycle::Opened,
            target: role,
            service_role: role,
            replicator_role: role,
            replicator_open: true,
            reconfiguring: false,
            primary_change: false,
            stage: ConfigurationStage::Activated,
            caught_up: true,
            minimum,
            remotes,
            authority_fenced: false,
            faulted: false,
            retired: false,
            close_outcome: None,
        }
        .status()
    }

    fn quorum(&self, previous: bool) -> bool {
        if !self.replicator_open {
            return false;
        }
        let current = |replica: &&RemoteReplica| {
            matches!(
                replica.current,
                ReplicaRole::Primary | ReplicaRole::ActiveSecondary
            )
        };
        let prior = |replica: &&RemoteReplica| replica.previous == ReplicaRole::ActiveSecondary;
        let available = |replica: &&RemoteReplica| replica.is_up && replica.ready;
        let cc_size = 1 + self.remotes.iter().filter(current).count();
        let cc_ready = 1 + self
            .remotes
            .iter()
            .filter(current)
            .filter(available)
            .count();
        let pc_size = 1 + self.remotes.iter().filter(prior).count();
        let pc_ready = 1 + self.remotes.iter().filter(prior).filter(available).count();
        cc_size >= self.minimum && cc_ready > cc_size / 2 && (!previous || pc_ready > pc_size / 2)
    }

    fn write_quorum(&self, previous: bool) -> AccessStatus {
        if self.quorum(previous) {
            AccessStatus::Granted
        } else {
            AccessStatus::NoWriteQuorum
        }
    }

    fn status(&self) -> Result<(AccessStatus, AccessStatus), HostError> {
        use AccessStatus::{Granted, NotPrimary, ReconfigurationPending as TryAgain};
        let denied = (NotPrimary, NotPrimary);
        if self.authority_fenced || self.faulted || self.retired {
            return Ok(denied);
        }
        match self.lifecycle {
            Lifecycle::Created | Lifecycle::Closing | Lifecycle::Closed => return Ok(denied),
            Lifecycle::Opening => {
                return Ok(if self.target != ReplicaRole::Primary {
                    denied
                } else if self.roles_current() {
                    (Granted, self.write_quorum(false))
                } else {
                    (TryAgain, TryAgain)
                });
            }
            Lifecycle::Opened => {}
        }
        if self.target == ReplicaRole::IdleSecondary || !self.reconfiguring {
            return Ok(
                if self.target != ReplicaRole::IdleSecondary
                    && self.service_role == ReplicaRole::Primary
                {
                    (Granted, self.write_quorum(false))
                } else {
                    denied
                },
            );
        }
        if self.remotes.is_empty() && self.target == ReplicaRole::ActiveSecondary {
            return Ok(denied);
        }
        match (self.primary_change, self.target) {
            (true, ReplicaRole::Primary) => Ok(if !self.roles_current() || !self.caught_up {
                (TryAgain, TryAgain)
            } else if self.stage == ConfigurationStage::Catchup {
                (Granted, TryAgain)
            } else {
                (Granted, self.write_quorum(false))
            }),
            (true, ReplicaRole::ActiveSecondary) => Ok(if !self.caught_up {
                if self.stage == ConfigurationStage::PreWriteStatusRevoke {
                    (Granted, self.write_quorum(true))
                } else {
                    (Granted, TryAgain)
                }
            } else {
                // The production selector chooses TransitioningRole/Completed,
                // not the table's otherwise unreachable CatchupCompleted row.
                denied
            }),
            (false, ReplicaRole::Primary) => Ok((
                Granted,
                self.write_quorum(!self.caught_up || self.stage == ConfigurationStage::Catchup),
            )),
            _ => Err(HostError::Input("unknown native access state")),
        }
    }
}

pub(crate) fn activated_access(
    role: ReplicaRole,
    minimum: usize,
    remotes: Vec<RemoteReplica>,
) -> Result<(AccessStatus, AccessStatus), HostError> {
    Projection::activated_access(role, minimum, remotes)
}

impl PartitionAccess {
    pub(crate) fn status(&self) -> Result<(AccessStatus, AccessStatus), HostError> {
        if self.replicator.aborted() {
            return Ok((AccessStatus::NotPrimary, AccessStatus::NotPrimary));
        }
        let state = self.state.lock().unwrap();
        if !matches!(
            state.lifecycle,
            Lifecycle::Created | Lifecycle::Closing | Lifecycle::Closed
        ) && !state.authority_fenced
            && !state.faulted
            && !state.retired
            && !self.replicator.is_alive()
        {
            return Err(HostError::Input("replicator owner stopped"));
        }
        state.status()
    }

    fn write_access(&self) -> AccessStatus {
        match self.status() {
            Ok((_, access)) => access,
            Err(error) => {
                tracing::warn!(%error, "private host access projection failed");
                AccessStatus::NotPrimary
            }
        }
    }

    pub(crate) fn fence(&self, fence: Fence) {
        let mut state = self.state.lock().unwrap();
        match fence {
            Fence::Authority => state.authority_fenced = true,
            Fence::Fault => state.faulted = true,
            Fence::Retired => state.retired = true,
        }
    }

    fn observe(&self, observation: LifecycleObservation) {
        let mut state = self.state.lock().unwrap();
        match observation {
            LifecycleObservation::TransportQuorum { .. } => {}
            LifecycleObservation::Ready(role) => {
                state.replicator_open = true;
                if state.replicator_role != role && role != ReplicaRole::Primary {
                    state.remotes.clear();
                }
                state.replicator_role = role;
            }
            LifecycleObservation::Opened => state.replicator_open = true,
            LifecycleObservation::Closed => state.replicator_open = false,
            LifecycleObservation::Faulted => {
                state.faulted = true;
            }
        }
    }

    fn check_live(&self) -> Result<(), HostError> {
        if self.replicator.aborted() || self.state.lock().unwrap().retired {
            Err(HostError::Aborted)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum ChildClose {
    Pending,
    Closed,
    Failed(Arc<RuntimeError>),
    Aborted,
}

#[derive(Clone, Debug)]
pub(crate) struct CloseOutcome {
    pub(crate) role_mismatch: bool,
    pub(crate) replicator: ChildClose,
    pub(crate) application: ChildClose,
}

#[derive(Clone, Debug)]
pub(crate) enum HostOutcome {
    Opened {
        replication_address: String,
        service: crate::application::RoleChange,
    },
    RoleChanged(crate::application::RoleChange),
    Completed,
    Closed(CloseOutcome),
}

struct Application {
    replica: Arc<dyn StatefulServiceReplica>,
    aborted: AtomicBool,
}

impl Application {
    fn abort(&self) {
        if !self.aborted.swap(true, Ordering::AcqRel) {
            self.replica.abort();
        }
    }
}

pub(crate) struct HostLifecycle {
    facade: Arc<Facade>,
    application: Arc<Application>,
    access: PartitionAccess,
    bindings: Vec<ReplicaInformation>,
    pending: Option<BoxFuture<'static, Result<HostOutcome, HostError>>>,
    closed: Option<CloseOutcome>,
}

impl HostLifecycle {
    /// The caller has opened the application; no factory/partition is installed.
    pub(crate) fn start(
        setup: FacadeSetup,
        application: Arc<dyn StatefulServiceReplica>,
        minimum: usize,
    ) -> Result<Self, HostError> {
        if minimum == 0 {
            return Err(HostError::Input("minimum replica set size"));
        }
        let access = PartitionAccess {
            state: Arc::new(Mutex::new(Projection {
                lifecycle: Lifecycle::Created,
                target: ReplicaRole::None,
                service_role: ReplicaRole::None,
                replicator_role: ReplicaRole::None,
                replicator_open: false,
                reconfiguring: false,
                primary_change: false,
                stage: ConfigurationStage::Activated,
                caught_up: false,
                minimum,
                remotes: Vec::new(),
                authority_fenced: false,
                faulted: false,
                retired: false,
                close_outcome: None,
            })),
            replicator: setup.state(),
        };
        let policy = access.clone();
        let observation = access.clone();
        let bindings = setup.bindings();
        let facade = setup
            .start(
                Arc::new(move || policy.write_access()),
                Arc::new(move |event| observation.observe(event)),
            )
            .map_err(|error| HostError::Facade(Arc::new(error)))?;
        Ok(Self {
            facade,
            application: Arc::new(Application {
                replica: application,
                aborted: AtomicBool::new(false),
            }),
            access,
            bindings,
            pending: None,
            closed: None,
        })
    }

    pub(crate) fn access(&self) -> PartitionAccess {
        self.access.clone()
    }

    pub(crate) fn facade(&self) -> Arc<Facade> {
        self.facade.clone()
    }

    fn idle(&self) -> Result<(), HostError> {
        self.access.check_live()?;
        if self.pending.is_some() {
            Err(HostError::Busy)
        } else if self.closed.is_some() {
            Err(HostError::Input("host is closed"))
        } else {
            Ok(())
        }
    }

    pub(crate) fn begin_open(&mut self, epoch: Epoch, role: ReplicaRole) -> Result<(), HostError> {
        self.idle()?;
        {
            let mut state = self.access.state.lock().unwrap();
            if state.lifecycle != Lifecycle::Created
                || role == ReplicaRole::None
                || epoch.data_loss_number < 0
                || epoch.configuration_number < 0
            {
                return Err(HostError::Input("Open role/state"));
            }
            state.lifecycle = Lifecycle::Opening;
            state.target = role;
        }
        let facade = self.facade.clone();
        let application = self.application.clone();
        let access = self.access.clone();
        self.pending = Some(Box::pin(async move {
            let address = facade.open().await.map_err(callback)?;
            facade.change_role(epoch, role).await.map_err(callback)?;
            access.check_live()?;
            let service = application
                .replica
                .change_role(role)
                .await
                .map_err(callback)?;
            access.check_live()?;
            let mut state = access.state.lock().unwrap();
            state.service_role = role;
            state.lifecycle = Lifecycle::Opened;
            Ok(HostOutcome::Opened {
                replication_address: address,
                service,
            })
        }));
        Ok(())
    }

    pub(crate) fn prepare_reconfiguration(
        &mut self,
        target: ReplicaRole,
        primary_change: bool,
        stage: ConfigurationStage,
        remotes: Vec<RemoteReplica>,
    ) -> Result<(), HostError> {
        self.idle()?;
        self.validate_remotes(&remotes)?;
        let mut state = self.access.state.lock().unwrap();
        if state.lifecycle != Lifecycle::Opened || state.reconfiguring || !state.roles_current() {
            return Err(HostError::Input("reconfiguration state"));
        }
        let valid = match (state.service_role, target, primary_change) {
            (ReplicaRole::Primary, ReplicaRole::Primary, false) => {
                stage == ConfigurationStage::Catchup
            }
            (ReplicaRole::Primary, ReplicaRole::ActiveSecondary, true) => {
                !remotes.is_empty() && stage != ConfigurationStage::Activated
            }
            (
                ReplicaRole::IdleSecondary | ReplicaRole::ActiveSecondary,
                ReplicaRole::Primary,
                true,
            ) => stage == ConfigurationStage::Catchup,
            (ReplicaRole::IdleSecondary, ReplicaRole::ActiveSecondary, false) => {
                remotes.is_empty() && stage == ConfigurationStage::Activated
            }
            _ => false,
        };
        if !valid {
            return Err(HostError::Input("reconfiguration edge/stage"));
        }
        state.target = target;
        state.primary_change = primary_change;
        state.reconfiguring = true;
        state.stage = stage;
        state.caught_up = false;
        state.remotes = remotes;
        Ok(())
    }

    fn validate_remotes(&self, remotes: &[RemoteReplica]) -> Result<(), HostError> {
        let mut ids = std::collections::BTreeSet::new();
        for remote in remotes {
            if !ids.insert(remote.information.identity.replica_id)
                || !self
                    .bindings
                    .iter()
                    .any(|bound| matches_binding(bound, &remote.information))
            {
                return Err(HostError::Input("remote membership binding"));
            }
        }
        Ok(())
    }

    pub(crate) fn update_availability(
        &self,
        information: &ReplicaInformation,
        is_up: bool,
        ready: bool,
    ) -> Result<(), HostError> {
        self.access.check_live()?;
        let mut state = self.access.state.lock().unwrap();
        let remote = state
            .remotes
            .iter_mut()
            .find(|remote| matches_binding(&remote.information, information))
            .ok_or(HostError::Input("unknown remote availability binding"))?;
        remote.is_up = is_up;
        remote.ready = ready;
        Ok(())
    }

    pub(crate) fn begin_change_role(&mut self, epoch: Epoch) -> Result<(), HostError> {
        self.idle()?;
        if epoch.data_loss_number < 0 || epoch.configuration_number < 0 {
            return Err(HostError::Input("role-change epoch"));
        }
        let role = {
            let state = self.access.state.lock().unwrap();
            if !state.reconfiguring
                || state.roles_current()
                || (state.target == ReplicaRole::ActiveSecondary
                    && !state.remotes.is_empty()
                    && !state.caught_up)
            {
                return Err(HostError::Input("role change before/after its host phase"));
            }
            state.target
        };
        let facade = self.facade.clone();
        let application = self.application.clone();
        let access = self.access.clone();
        self.pending = Some(Box::pin(async move {
            facade.change_role(epoch, role).await.map_err(callback)?;
            access.check_live()?;
            let service = application
                .replica
                .change_role(role)
                .await
                .map_err(callback)?;
            access.check_live()?;
            access.state.lock().unwrap().service_role = role;
            Ok(HostOutcome::RoleChanged(service))
        }));
        Ok(())
    }

    pub(crate) fn begin_catchup(&mut self, mode: ReplicaSetQuorumMode) -> Result<(), HostError> {
        self.idle()?;
        {
            let mut state = self.access.state.lock().unwrap();
            if !state.reconfiguring
                || state.caught_up
                || state.replicator_role != ReplicaRole::Primary
                || state.service_role != ReplicaRole::Primary
            {
                return Err(HostError::Input("catch-up phase/role"));
            }
            state.stage = ConfigurationStage::Catchup;
        }
        let facade = self.facade.clone();
        let access = self.access.clone();
        self.pending = Some(Box::pin(async move {
            facade
                .wait_for_catch_up_quorum(mode)
                .await
                .map_err(callback)?;
            access.check_live()?;
            access.state.lock().unwrap().caught_up = true;
            Ok(HostOutcome::Completed)
        }));
        Ok(())
    }

    /// Host acknowledgement of activation, not an engine ACK/configuration call.
    pub(crate) fn complete_reconfiguration(&mut self) -> Result<(), HostError> {
        self.idle()?;
        let mut state = self.access.state.lock().unwrap();
        let idle_activation =
            state.target == ReplicaRole::ActiveSecondary && state.remotes.is_empty();
        if !state.reconfiguring || !state.roles_current() || (!idle_activation && !state.caught_up)
        {
            return Err(HostError::Input(
                "activation before role/catch-up completion",
            ));
        }
        state.stage = ConfigurationStage::Activated;
        state.reconfiguring = false;
        Ok(())
    }

    pub(crate) fn begin_close(&mut self) -> Result<(), HostError> {
        if self.pending.is_some() {
            return Err(HostError::Busy);
        }
        if let Some(outcome) = self.closed.clone() {
            self.pending = Some(Box::pin(async move { Ok(HostOutcome::Closed(outcome)) }));
            return Ok(());
        }
        if self.access.state.lock().unwrap().retired {
            return Err(HostError::Aborted);
        }
        let mismatch = {
            let mut state = self.access.state.lock().unwrap();
            if !matches!(state.lifecycle, Lifecycle::Opening | Lifecycle::Opened) {
                return Err(HostError::Input("Close lifecycle"));
            }
            state.lifecycle = Lifecycle::Closing;
            let role_mismatch = state.service_role != state.replicator_role;
            state.close_outcome = Some(CloseOutcome {
                role_mismatch,
                replicator: ChildClose::Pending,
                application: ChildClose::Pending,
            });
            role_mismatch
        };
        let facade = self.facade.clone();
        let application = self.application.clone();
        let access = self.access.clone();
        self.pending = Some(Box::pin(async move {
            let (replicator, app) = if mismatch {
                tracing::warn!("private outer Close aborts mismatched service/replicator roles");
                facade.abort();
                application.abort();
                (ChildClose::Aborted, ChildClose::Aborted)
            } else {
                let replicator = if access.replicator.aborted()
                    || !access.state.lock().unwrap().replicator_open
                {
                    facade.abort();
                    ChildClose::Aborted
                } else {
                    match facade.close().await {
                        Ok(()) => ChildClose::Closed,
                        Err(error) => {
                            tracing::warn!(%error, "private outer Close: replicator Close failed");
                            facade.abort();
                            ChildClose::Failed(Arc::new(error))
                        }
                    }
                };
                access.state.lock().unwrap().close_outcome = Some(CloseOutcome {
                    role_mismatch: false,
                    replicator: replicator.clone(),
                    application: ChildClose::Pending,
                });
                access.check_live_after_child_abort(&replicator)?;
                let app = match application.replica.close().await {
                    Ok(()) => ChildClose::Closed,
                    Err(error) => {
                        tracing::warn!(%error, "private outer Close: application Close failed");
                        application.abort();
                        ChildClose::Failed(Arc::new(error))
                    }
                };
                (replicator, app)
            };
            let outcome = CloseOutcome {
                role_mismatch: mismatch,
                replicator,
                application: app,
            };
            let mut state = access.state.lock().unwrap();
            state.close_outcome = Some(outcome.clone());
            if state.retired {
                return Err(HostError::Aborted);
            }
            state.lifecycle = Lifecycle::Closed;
            Ok(HostOutcome::Closed(outcome))
        }));
        Ok(())
    }

    pub(crate) async fn resume(&mut self) -> Result<HostOutcome, HostError> {
        let result = self.pending.as_mut().ok_or(HostError::NoOperation)?.await;
        self.pending = None;
        let result = result.and_then(|outcome| {
            if !matches!(outcome, HostOutcome::Closed(_)) {
                self.access.check_live()?;
            }
            Ok(outcome)
        });
        if let Ok(HostOutcome::Closed(outcome)) = &result {
            self.closed = Some(outcome.clone());
        } else if result.is_err() {
            self.access.fence(Fence::Fault);
        }
        result
    }

    pub(crate) fn close_outcome(&self) -> Option<CloseOutcome> {
        self.access.state.lock().unwrap().close_outcome.clone()
    }

    pub(crate) fn abort(&self) {
        self.access.fence(Fence::Retired);
        self.facade.abort();
        self.application.abort();
    }

    /// Independent of ordinary outer Close; reports actual retained task drain.
    pub(crate) async fn join(&self) -> Result<(), HostError> {
        if self.pending.is_some() {
            return Err(HostError::Busy);
        }
        if self.closed.is_none() && !self.access.replicator.aborted() {
            return Err(HostError::Input("join before host Close/Abort"));
        }
        let result = self
            .facade
            .drain()
            .await
            .map_err(|error| HostError::Facade(Arc::new(error)));
        if result.is_err() {
            self.access.fence(Fence::Fault);
        }
        result
    }
}

impl PartitionAccess {
    fn check_live_after_child_abort(&self, child: &ChildClose) -> Result<(), HostError> {
        // A failed child Close requires native Abort, which seals the shared
        // facade gate. That is not an external host Abort.
        if self.state.lock().unwrap().retired {
            Err(HostError::Aborted)
        } else if matches!(child, ChildClose::Failed(_) | ChildClose::Aborted) {
            Ok(())
        } else {
            self.check_live()
        }
    }
}

impl Drop for HostLifecycle {
    fn drop(&mut self) {
        if self.closed.is_none() {
            self.abort();
        }
    }
}

fn callback(error: RuntimeError) -> HostError {
    HostError::Callback(Arc::new(error))
}
