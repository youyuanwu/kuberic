// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation of PrimaryReplicator.BuildIdleAsyncOperation.cpp,
// ReplicaManager.cpp:792-870,1142-1220,1783-1832 and classic build selection in
// ReplicationQueueManager.cpp:165-194, microsoft/service-fabric
// @3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d, Reliability/Replication/.

//! Private caller-driven BuildIdle lifecycle, not a primary/role coordinator.
//! The actual ReplicaManager owns membership and retention; the operation owns
//! provider acquisition and CopySender drivers. Registry handles retain metadata
//! but only weak references to drivers/manager, never a strong ownership cycle.
//! Dropping an operation is abandonment, not cancellation or provider drain.
//! Health/API monitoring and slow-peer/promotion policy are not implemented.

#![cfg_attr(not(test), expect(dead_code, reason = "unselected V1 BuildIdle"))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::Notify;

use crate::application::Lsn;
use crate::error::RuntimeError;
use crate::traits::StateProvider;

use super::async_runtime::Clock;
use super::copy_async_operation::{CopyConfig, CopyError, CopyFault, CopySendAction};
use super::copy_context_receiver::{ContextObservation, CopyContextAck, CopyContextConfig};
use super::copy_establishment::{EstablishError, PrimaryEstablishment, PrimaryEstablishmentHandle};
use super::copy_sender::CopySenderHandle;
use super::replica_manager::{EstablishedIdle, ManagerEffects, ManagerError, ReplicaManager};
use super::session::{AckAction, AckProcessor, ReplicationSession, SendIntent, SessionSendBatch};
use super::transport::{ProtocolError, Route, wire};

#[derive(Clone)]
pub(super) struct BuildConfig {
    pub(super) route: Route,
    /// Logical sender identity, not a conversion of the route's GUID.
    pub(super) remote_incarnation: i64,
    pub(super) copy: CopyConfig,
    pub(super) context: Option<CopyContextConfig>,
}

#[derive(Clone)]
pub(super) struct BuildCallbacks {
    pub(super) fence: Option<super::service_adapter::engine_session::Fence>,
    pub(super) replication: Arc<dyn Fn(SendIntent) -> bool + Send + Sync>,
    pub(super) copy: Arc<dyn Fn(CopySendAction) -> bool + Send + Sync>,
    pub(super) context_ack: Arc<dyn Fn(CopyContextAck, bool) -> bool + Send + Sync>,
}

#[derive(Clone, Debug, thiserror::Error)]
pub(super) enum BuildError {
    #[error("replica manager rejected BuildIdle: {0:?}")]
    Manager(Arc<ManagerError>),
    #[error("BuildIdle manager owner was dropped")]
    ManagerDropped,
    #[error("BuildIdle driver was abandoned before End")]
    OwnerDropped,
    #[error(transparent)]
    Establishment(#[from] EstablishError),
    #[error(transparent)]
    Copy(#[from] CopyError),
    #[error("BuildIdle failed ({operation}); cleanup also failed: {cleanup}")]
    Cleanup {
        operation: Arc<BuildError>,
        cleanup: EstablishError,
    },
}

impl From<ManagerError> for BuildError {
    fn from(error: ManagerError) -> Self {
        Self::Manager(Arc::new(error))
    }
}

#[derive(Clone, Debug)]
pub(super) struct BuildEnd {
    pub(super) result: Result<(), BuildError>,
    pub(super) fault: Option<CopyFault>,
}

struct Terminal {
    end: BuildEnd,
    finish_copy_target: Option<Lsn>,
}

struct Root {
    manager: Weak<Mutex<ReplicaManager>>,
    establishment: PrimaryEstablishmentHandle,
    copy: CopySenderHandle,
    session: ReplicationSession,
    clock: Clock,
    wake: Notify,
    completed: Notify,
    alive: AtomicBool,
    terminal: Mutex<Option<Terminal>>,
}

impl Root {
    fn update_progress(&self) -> Result<(), BuildError> {
        let manager = self.manager.upgrade().ok_or(BuildError::ManagerDropped)?;
        let effects = manager.lock().unwrap().update_progress()?;
        effects.dispatch(self.clock.now())?;
        Ok(())
    }

    async fn timer(&self) -> Result<(), BuildError> {
        let wake = self.wake.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let context = self.establishment.context_ack_deadline()?;
        // Acquisition drives StartCopy while waiting for establishment. After
        // that boundary, this owner continues retries until ACK or final cleanup.
        let start = self
            .establishment
            .is_established()?
            .then(|| self.session.start_copy_deadline())
            .flatten();
        let deadline = [
            self.session.retry_deadline(),
            self.session.fault_deadline(),
            context,
            start,
        ]
        .into_iter()
        .flatten()
        .min();
        tokio::select! {
            () = &mut wake => return Ok(()),
            () = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep(deadline.saturating_sub(self.clock.now())).await,
                    None => std::future::pending().await,
                }
            } => {}
        }
        if start.is_some_and(|deadline| deadline <= self.clock.now())
            && let Some(batch) = self.session.on_start_copy_timer(self.clock.now())
        {
            batch.dispatch();
        }
        if self
            .session
            .retry_deadline()
            .is_some_and(|deadline| deadline <= self.clock.now())
        {
            self.session.on_timer(self.clock.now()).dispatch();
        }
        if context.is_some_and(|deadline| deadline <= self.clock.now()) {
            self.establishment.context_ack_timer()?;
        }
        if let Some(send) = self.session.on_fault_timer(self.clock.now()) {
            send.dispatch();
        }
        Ok(())
    }
}

/// Rooted lifecycle metadata; provider and enumeration handles remain weak.
#[derive(Clone)]
pub(super) struct BuildIdleHandle(Arc<Root>);

impl BuildIdleHandle {
    pub(super) fn session(&self) -> ReplicationSession {
        self.0.session.clone()
    }

    pub(super) fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub(super) fn wake_timers(&self) {
        self.0.wake.notify_one();
    }

    pub(super) fn result(&self) -> Option<BuildEnd> {
        self.0
            .terminal
            .lock()
            .unwrap()
            .as_ref()
            .map(|terminal| terminal.end.clone())
    }

    pub(super) async fn wait(&self) -> Result<BuildEnd, BuildError> {
        loop {
            let completed = self.0.completed.notified();
            tokio::pin!(completed);
            completed.as_mut().enable();
            if let Some(end) = self.result() {
                return Ok(end);
            }
            if !self.0.alive.load(Ordering::Acquire) {
                return Err(BuildError::OwnerDropped);
            }
            completed.await;
        }
    }

    pub(super) fn finish_copy_target(&self) -> Result<Option<Lsn>, CopyError> {
        if let Some(terminal) = self.0.terminal.lock().unwrap().as_ref() {
            return Ok(terminal.finish_copy_target);
        }
        Ok(self.0.copy.snapshot()?.replication_target)
    }

    pub(super) fn retarget_replication(&self, lsn: Lsn) -> Result<bool, CopyError> {
        self.0
            .copy
            .update_last_replication_operation_during_copy(lsn)
    }

    pub(super) fn try_fault_slow(&self, reason: String, now: Duration) -> Result<bool, CopyError> {
        let faulted = self.0.session.try_fault_idle(&self.0.copy, reason, now)?;
        self.wake_timers();
        Ok(faulted)
    }

    /// Native BuildIdle OnCancel, not RemoveReplica/RemoteSession::CancelCopy.
    pub(super) fn cancel(&self) -> Result<(), EstablishError> {
        if self.result().is_some() {
            return Ok(());
        }
        let result = self.0.establishment.request_build_cancel();
        self.wake_timers();
        result
    }

    pub(super) fn cancel_for_removal(&self) -> Result<(), EstablishError> {
        if self.result().is_some() {
            return Ok(());
        }
        let result = self.0.establishment.cancel();
        self.wake_timers();
        result
    }

    pub(super) fn receive_context(
        &self,
        envelope: &wire::Envelope,
    ) -> Result<ContextObservation, BuildError> {
        let result = self.0.establishment.receive_context(envelope);
        self.wake_timers();
        Ok(result?)
    }

    /// Continue EVERY handoff even if an owner rejects one. Otherwise the
    /// session remains processing and silently coalesces all future ACKs.
    pub(super) fn receive_ack(&self, envelope: &wire::Envelope) -> Result<(), BuildError> {
        self.receive_ack_for_role(envelope, true, || self.0.update_progress())
    }

    pub(super) fn receive_ack_for_role(
        &self,
        envelope: &wire::Envelope,
        process_copy: bool,
        progress: impl FnMut() -> Result<(), BuildError>,
    ) -> Result<(), BuildError> {
        if let Some(wire::envelope::Body::ReplicationAck(body)) = &envelope.body {
            let manager = self.0.manager.upgrade().ok_or(BuildError::ManagerDropped)?;
            let latest = manager.lock().unwrap().last_sequence_number()?;
            if body.replication_received > latest {
                return Err(EstablishError::from(ProtocolError::Shape(
                    "ACK exceeds issued replication",
                ))
                .into());
            }
            if process_copy && body.error == 0 {
                self.0
                    .copy
                    .snapshot()?
                    .validate_ack(body.copy_received, body.copy_applied)?;
            }
        }
        // The notification marks ordering; the fallible manager transition is
        // performed at PrimaryProgress below, not hidden in a unit callback.
        let processor = self.0.establishment.receive_ack_for_role(
            envelope,
            process_copy,
            Some(Arc::new(|| {})),
        )?;
        let result = deliver_ack(processor, &self.0.copy, &self.0.clock, progress);
        self.wake_timers();
        result
    }
}

pub(super) fn deliver_ack(
    processor: Option<AckProcessor>,
    copy: &CopySenderHandle,
    clock: &Clock,
    mut progress: impl FnMut() -> Result<(), BuildError>,
) -> Result<(), BuildError> {
    let mut failure = None;
    if let Some(mut processor) = processor {
        while let Some(handoff) = processor.next(clock.now()) {
            processor = handoff.deliver(|action| {
                let result = match action {
                    AckAction::ReplicationSends(batch) => {
                        batch.dispatch();
                        Ok(())
                    }
                    AckAction::CopyReplicationGate {
                        copy_applied,
                        replication_applied,
                    } => copy
                        .process_replication_ack(copy_applied, replication_applied)
                        .map(|_| ())
                        .map_err(BuildError::from),
                    AckAction::PrimaryProgress(notification) => {
                        notification.dispatch();
                        progress()
                    }
                    AckAction::CopyAck { received, applied } => copy
                        .process_copy_ack(received, applied)
                        .map(|_| ())
                        .map_err(BuildError::from),
                };
                if let Err(error) = result
                    && failure.is_none()
                {
                    failure = Some(error);
                }
            });
        }
    }
    failure.map_or(Ok(()), Err)
}

pub(super) struct BuildIdle {
    handle: BuildIdleHandle,
    establishment: PrimaryEstablishment,
    send_copy: Arc<dyn Fn(CopySendAction) -> bool + Send + Sync>,
}

impl BuildIdle {
    pub(super) fn begin(
        manager: &Arc<Mutex<ReplicaManager>>,
        config: BuildConfig,
        provider: Arc<dyn StateProvider>,
        callbacks: BuildCallbacks,
        clock: Clock,
    ) -> Result<Self, BuildError> {
        let (owner, pending, start) =
            Self::prepare(manager, config, provider, callbacks, clock.clone())?;
        pending.dispatch(clock.now())?;
        start.dispatch();
        Ok(owner)
    }

    /// Publish the returned owner before delivering either reentrant send batch.
    pub(super) fn prepare(
        manager: &Arc<Mutex<ReplicaManager>>,
        config: BuildConfig,
        provider: Arc<dyn StateProvider>,
        callbacks: BuildCallbacks,
        clock: Clock,
    ) -> Result<(Self, ManagerEffects, SessionSendBatch), BuildError> {
        Self::prepare_transfer(manager, config, provider, callbacks, clock, None, false)
    }

    pub(super) fn prepare_recovery(
        manager: &Arc<Mutex<ReplicaManager>>,
        config: BuildConfig,
        provider: Arc<dyn StateProvider>,
        callbacks: BuildCallbacks,
        clock: Clock,
    ) -> Result<(Self, ManagerEffects, SessionSendBatch), BuildError> {
        Self::prepare_transfer(manager, config, provider, callbacks, clock, None, true)
    }

    pub(super) fn prepare_replacement(
        manager: &Arc<Mutex<ReplicaManager>>,
        predecessor: ReplicationSession,
        config: BuildConfig,
        provider: Arc<dyn StateProvider>,
        callbacks: BuildCallbacks,
        clock: Clock,
    ) -> Result<(Self, ManagerEffects, SessionSendBatch), BuildError> {
        Self::prepare_transfer(
            manager,
            config,
            provider,
            callbacks,
            clock,
            Some(predecessor),
            false,
        )
    }

    fn prepare_transfer(
        manager: &Arc<Mutex<ReplicaManager>>,
        config: BuildConfig,
        provider: Arc<dyn StateProvider>,
        callbacks: BuildCallbacks,
        clock: Clock,
        predecessor: Option<ReplicationSession>,
        recovery: bool,
    ) -> Result<(Self, ManagerEffects, SessionSendBatch), BuildError> {
        if callbacks
            .fence
            .as_ref()
            .is_some_and(|fence| fence.check().is_err())
        {
            return Err(EstablishError::Cancelled.into());
        }
        config.route.validate().map_err(EstablishError::from)?;
        if config.route.source.partition != config.route.destination.partition {
            return Err(EstablishError::from(ProtocolError::Shape("cross-partition build")).into());
        }
        let (identity, epoch) = manager
            .lock()
            .unwrap()
            .build_identity(&config.route, config.remote_incarnation)?;
        config.copy.validate()?;
        let session = ReplicationSession::new(
            identity,
            epoch,
            0,
            config.copy.sender,
            callbacks.replication,
            clock.now(),
        );
        session.open();
        let replica_id = config.route.destination.replica;
        let mut establishment = PrimaryEstablishment::new(
            config.route,
            session.clone(),
            config.copy,
            config.context,
            provider,
            callbacks.context_ack,
            clock.clone(),
        )?;
        establishment.fence = callbacks.fence;
        let handle = BuildIdleHandle(Arc::new(Root {
            manager: Arc::downgrade(manager),
            establishment: establishment.handle(),
            copy: establishment.copy().handle(),
            session: session.clone(),
            clock: clock.clone(),
            wake: Notify::new(),
            completed: Notify::new(),
            alive: AtomicBool::new(true),
            terminal: Mutex::new(None),
        }));
        let member = EstablishedIdle {
            replica_id,
            session,
            finish_copy_target: None,
        };
        let (pending, start) = if let Some(predecessor) = predecessor {
            manager.lock().unwrap().admit_replacement(
                &predecessor,
                member,
                handle.clone(),
                &mut establishment,
            )?
        } else if recovery {
            manager
                .lock()
                .unwrap()
                .admit_recovery(member, handle.clone(), &mut establishment)?
        } else {
            manager
                .lock()
                .unwrap()
                .admit_build(member, handle.clone(), &mut establishment)?
        };
        let owner = Self {
            handle,
            establishment,
            send_copy: callbacks.copy,
        };
        Ok((owner, pending, start))
    }

    pub(super) fn handle(&self) -> BuildIdleHandle {
        self.handle.clone()
    }

    async fn drive_to_end(&mut self) -> Result<BuildEnd, BuildError> {
        let manager = self.handle.0.manager.clone();
        let latest = Arc::new(move || {
            let manager = manager.upgrade().ok_or(RuntimeError::Closed)?;
            manager
                .lock()
                .unwrap()
                .last_sequence_number()
                .map_err(|_| RuntimeError::Closed)
        });
        let acquired = self
            .establishment
            .acquire_copy(self.send_copy.clone(), latest)
            .await;
        let mut end = match acquired {
            Err(error) => {
                let fault = match &error {
                    EstablishError::Provider { description, error } => Some(CopyFault {
                        error: error.clone(),
                        description,
                    }),
                    _ => None,
                };
                let mut error = BuildError::from(error);
                if let Err(cleanup) = self.establishment.cleanup(-1) {
                    error = BuildError::Cleanup {
                        operation: Arc::new(error),
                        cleanup,
                    };
                }
                BuildEnd {
                    result: Err(error),
                    fault,
                }
            }
            Ok(()) => {
                // End owns the terminal result and fault, not the driver's
                // success/error alone. Infrastructure failures leave End pending.
                let driven = self.establishment.copy().drive().await;
                let copy_end = match self.establishment.end_copy() {
                    Ok(end) => end,
                    Err(cleanup) => {
                        return Err(match driven {
                            Err(error) => BuildError::Cleanup {
                                operation: Arc::new(error.into()),
                                cleanup,
                            },
                            Ok(()) => cleanup.into(),
                        });
                    }
                };
                BuildEnd {
                    result: copy_end.result.map_err(BuildError::from),
                    fault: copy_end.fault,
                }
            }
        };
        let target = self.handle.0.copy.snapshot()?.replication_target;
        if let Some(fault) = &end.fault {
            match self.handle.0.manager.upgrade() {
                Some(manager) => manager.lock().unwrap().report_build_fault(fault.clone()),
                None => end.result = Err(BuildError::ManagerDropped),
            }
        }
        let mut terminal = self.handle.0.terminal.lock().unwrap();
        let end = terminal
            .get_or_insert(Terminal {
                end,
                finish_copy_target: target,
            })
            .end
            .clone();
        drop(terminal);
        self.handle.0.completed.notify_waiters();
        Ok(end)
    }

    /// Interruptible driver; acquisition/enumeration state lives in the owner.
    /// An outer error is NOT BuildEnd: retry or explicitly contain the owner.
    pub(super) async fn run(&mut self) -> Result<BuildEnd, BuildError> {
        if let Some(end) = self.handle.result() {
            return Ok(end);
        }
        let root = self.handle.0.clone();
        let drive = self.drive_to_end();
        tokio::pin!(drive);
        loop {
            tokio::select! {
                end = &mut drive => return end,
                timer = root.timer() => timer?,
            }
        }
    }
}

impl Drop for BuildIdle {
    fn drop(&mut self) {
        self.handle.0.alive.store(false, Ordering::Release);
        self.handle.0.completed.notify_waiters();
    }
}

#[cfg(test)]
mod tests;
