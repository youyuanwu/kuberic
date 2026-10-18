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

use std::sync::{Arc, Mutex, Weak};

use tokio::sync::Notify;

use crate::application::Lsn;
use crate::error::RuntimeError;
use crate::traits::StateProvider;

use super::async_runtime::Clock;
use super::copy_async_operation::{CopyConfig, CopyError, CopyFault, CopySendAction};
use super::copy_context_receiver::{ContextObservation, CopyContextAck, CopyContextConfig};
use super::copy_establishment::{EstablishError, PrimaryEstablishment, PrimaryEstablishmentHandle};
use super::copy_sender::CopySenderHandle;
use super::replica_manager::{EstablishedIdle, ManagerError, ReplicaManager};
use super::session::{AckAction, ReplicationSession, SendIntent};
use super::transport::{ProtocolError, Route, wire};

pub(super) struct BuildConfig {
    pub(super) route: Route,
    /// Logical sender identity, not a conversion of the route's GUID.
    pub(super) remote_incarnation: i64,
    pub(super) copy: CopyConfig,
    pub(super) context: Option<CopyContextConfig>,
}

pub(super) struct BuildCallbacks {
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
        let deadline = [self.session.retry_deadline(), context, start]
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
        Ok(())
    }
}

/// Rooted lifecycle metadata; provider and enumeration handles remain weak.
#[derive(Clone)]
pub(super) struct BuildIdleHandle(Arc<Root>);

impl BuildIdleHandle {
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

    pub(super) fn finish_copy_target(&self) -> Result<Option<Lsn>, CopyError> {
        if let Some(terminal) = self.0.terminal.lock().unwrap().as_ref() {
            return Ok(terminal.finish_copy_target);
        }
        Ok(self.0.copy.snapshot()?.replication_target)
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
        if let Some(wire::envelope::Body::ReplicationAck(body)) = &envelope.body {
            let manager = self.0.manager.upgrade().ok_or(BuildError::ManagerDropped)?;
            let latest = manager.lock().unwrap().last_sequence_number()?;
            if body.replication_received > latest {
                return Err(EstablishError::from(ProtocolError::Shape(
                    "ACK exceeds issued replication",
                ))
                .into());
            }
            if body.error == 0 {
                self.0
                    .copy
                    .snapshot()?
                    .validate_ack(body.copy_received, body.copy_applied)?;
            }
        }
        // The notification marks ordering; the fallible manager transition is
        // performed at PrimaryProgress below, not hidden in a unit callback.
        let processor = self
            .0
            .establishment
            .receive_ack_with_progress(envelope, Some(Arc::new(|| {})))?;
        let mut failure = None;
        if let Some(mut processor) = processor {
            while let Some(handoff) = processor.next(self.0.clock.now()) {
                processor = handoff.deliver(|action| {
                    let result = match action {
                        AckAction::ReplicationSends(batch) => {
                            batch.dispatch();
                            Ok(())
                        }
                        AckAction::CopyReplicationGate {
                            copy_applied,
                            replication_applied,
                        } => self
                            .0
                            .copy
                            .process_replication_ack(copy_applied, replication_applied)
                            .map(|_| ())
                            .map_err(BuildError::from),
                        AckAction::PrimaryProgress(notification) => {
                            notification.dispatch();
                            self.0.update_progress()
                        }
                        AckAction::CopyAck { received, applied } => self
                            .0
                            .copy
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
        self.wake_timers();
        failure.map_or(Ok(()), Err)
    }
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
        let handle = BuildIdleHandle(Arc::new(Root {
            manager: Arc::downgrade(manager),
            establishment: establishment.handle(),
            copy: establishment.copy().handle(),
            session: session.clone(),
            clock: clock.clone(),
            wake: Notify::new(),
            terminal: Mutex::new(None),
        }));
        let (pending, start) = manager.lock().unwrap().admit_build(
            EstablishedIdle {
                replica_id,
                session,
                finish_copy_target: None,
            },
            handle.clone(),
            &mut establishment,
        )?;
        let owner = Self {
            handle,
            establishment,
            send_copy: callbacks.copy,
        };
        pending.dispatch(clock.now())?;
        start.dispatch();
        Ok(owner)
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
        let terminal = terminal.get_or_insert(Terminal {
            end,
            finish_copy_target: target,
        });
        Ok(terminal.end.clone())
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

#[cfg(test)]
mod tests;
