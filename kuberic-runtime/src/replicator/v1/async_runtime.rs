// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Timer/effect ownership guided by ReliableOperationSender.cpp:113-137,548-755,
// AckSender.cpp:64-208 and RemoteSession.cpp:448-655 at service-fabric
// 3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d.

//! Private, unselected Tokio owners for the actual V1 scheduling cores.
//! A transition selects rooted work; delivery happens later, outside all guards.
//! Closing disables new timer selection, not already-selected effects. Shutdown
//! additionally drains the worker and must be joined. Dropping an unjoined owner
//! is exceptional abort containment, NOT native close or successful shutdown.
//! Handles are weak so callbacks can retain/reenter them without root cycles.
//!
//! Each staging queue holds at most 64 effects. Session admission conservatively
//! reserves one slot even for a possibly empty batch; ACK updates already owned
//! by a processor coalesce without another slot. ACK scheduling reserves one of
//! its 64 slots for the active sender's pending-only final close effect. Timers
//! select into the worker's single local effect, never an overflowing queue.
//! StagingFull is retryable runtime backpressure checked BEFORE core mutation,
//! not carrier send-queue-full. This count bound does not bound replay lengths,
//! native sender pending operations or payload bytes; their owners/carrier have
//! separate limits. Shutdown rejects new ingress, unlike ordinary native close.

#![cfg_attr(not(test), expect(dead_code, reason = "unselected V1 async owners"))]

use std::collections::VecDeque;
use std::error::Error;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;

use crate::application::Lsn;
use crate::protocol::types::Epoch;

use super::ack::{AckDispatch, AckSender};
use super::sender::{SenderConfig, SenderOperation, SenderProgress};
use super::session::{
    AckAction, AckProcessor, AckUpdate, ReplicationSession, SendIntent, SessionIdentity,
    SessionSendBatch,
};

/// One origin is shared by every observation and absolute timer deadline.
#[derive(Clone)]
pub(super) struct Clock {
    origin: Instant,
}

impl Clock {
    pub(super) fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    pub(super) fn now(&self) -> Duration {
        Instant::now().duration_since(self.origin)
    }

    fn instant(&self, deadline: Duration) -> Instant {
        self.origin + deadline
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum AccessError {
    #[error("the timer/effect task has stopped")]
    Stopped,
    #[error("the timer/effect task failed; join its owner for the cause")]
    Failed,
    #[error("the unjoined task owner was dropped (abort containment, not graceful close)")]
    OwnerDropped,
    #[error("the timer/effect owner is shutting down")]
    ShuttingDown,
    #[error("the bounded runtime effect staging queue is full; retry admission")]
    StagingFull,
    #[error("the timer/effect transition mutex is poisoned")]
    Poisoned,
}

pub(super) type OwnerError = Box<dyn Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub(super) enum TaskError {
    #[error(transparent)]
    Access(#[from] AccessError),
    #[error("the required ACK owner rejected a handoff: {0}")]
    Owner(#[source] OwnerError),
    #[error("the timer/effect task panicked or was cancelled: {0}")]
    Join(#[from] JoinError),
}

/// These are delivery obligations, not fabricated copy/build/quorum results.
/// Implementations must consume both native handoffs synchronously, or transfer
/// them to their actual owner before returning. Errors stop the task explicitly.
pub(super) trait CopyAckOwner: Send + Sync {
    fn replication_gate(
        &self,
        copy_applied: Lsn,
        replication_applied: Lsn,
    ) -> Result<(), OwnerError>;

    fn copy_ack(&self, received: Lsn, applied: Lsn) -> Result<(), OwnerError>;
}

const RUNNING: u8 = 0;
const STOPPED: u8 = 1;
const FAILED: u8 = 2;
const ABANDONED: u8 = 3;
const MAX_STAGED_EFFECTS: usize = 64;

struct TaskState {
    status: AtomicU8,
    wake: Notify,
}

impl TaskState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            status: AtomicU8::new(RUNNING),
            wake: Notify::new(),
        })
    }

    fn check(&self) -> Result<(), AccessError> {
        match self.status.load(Ordering::Acquire) {
            RUNNING => Ok(()),
            STOPPED => Err(AccessError::Stopped),
            FAILED => Err(AccessError::Failed),
            ABANDONED => Err(AccessError::OwnerDropped),
            _ => unreachable!(),
        }
    }

    fn finish(&self) {
        self.status.store(STOPPED, Ordering::Release);
    }
}

/// Failure signaling does not lock potentially poisoned core state or catch a
/// callback panic. The actual panic remains observable through the JoinHandle.
struct FailureGuard(Arc<TaskState>);

impl Drop for FailureGuard {
    fn drop(&mut self) {
        let _ =
            self.0
                .status
                .compare_exchange(RUNNING, FAILED, Ordering::AcqRel, Ordering::Acquire);
        self.0.wake.notify_one();
    }
}

struct WakeOnDrop<'a>(&'a Notify);

impl Drop for WakeOnDrop<'_> {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

/// Retain the handle across a cancelled join future. Only an observed join
/// removes it. Drop is deliberately an abnormal, observable containment path.
struct OwnedTask {
    state: Arc<TaskState>,
    join: Option<JoinHandle<Result<(), TaskError>>>,
}

impl OwnedTask {
    async fn join(&mut self) -> Result<(), TaskError> {
        let Some(join) = self.join.as_mut() else {
            return match self.state.check() {
                Err(AccessError::Stopped) => Ok(()),
                Err(error) => Err(error.into()),
                Ok(()) => unreachable!("a running owner retains its join handle"),
            };
        };
        let result = join.await;
        self.join.take();
        result?
    }
}

impl Drop for OwnedTask {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            // Never claim this is native cancellation or drain. No detached
            // service/reaper is spawned, and handles report abandonment.
            self.state.status.store(ABANDONED, Ordering::Release);
            join.abort();
        }
    }
}

enum SessionEffect {
    Sends(SessionSendBatch),
    Ack(AckProcessor),
}

struct SessionState {
    core: ReplicationSession,
    effects: VecDeque<SessionEffect>,
    shutdown: bool,
}

impl SessionState {
    fn admit(&self, needs_slot: bool) -> Result<(), AccessError> {
        if self.shutdown {
            return Err(AccessError::ShuttingDown);
        }
        if needs_slot && self.effects.len() >= MAX_STAGED_EFFECTS {
            return Err(AccessError::StagingFull);
        }
        Ok(())
    }
}

struct SessionRoot {
    clock: Clock,
    task: Arc<TaskState>,
    state: Mutex<SessionState>,
    owner: Arc<dyn CopyAckOwner>,
}

enum SessionWork {
    Effect(SessionEffect),
    Wait(Option<Duration>),
    Done,
}

impl SessionRoot {
    fn transition<T>(
        &self,
        change: impl FnOnce(&mut SessionState, Duration) -> T,
    ) -> Result<T, AccessError> {
        let _wake = WakeOnDrop(&self.task.wake);
        let mut state = self.state.lock().map_err(|_| AccessError::Poisoned)?;
        self.task.check()?;
        Ok(change(&mut state, self.clock.now()))
    }

    fn next(&self) -> Result<SessionWork, AccessError> {
        let mut state = self.state.lock().map_err(|_| AccessError::Poisoned)?;
        self.task.check()?;
        if let Some(effect) = state.effects.pop_front() {
            return Ok(SessionWork::Effect(effect));
        }
        if state.shutdown {
            self.task.finish();
            return Ok(SessionWork::Done);
        }
        Ok(SessionWork::Wait(state.core.retry_deadline()))
    }

    fn enter_timer(&self) -> Result<SessionEffect, AccessError> {
        self.transition(|state, now| {
            // The sleep branch already entered the callback. Do not recheck a
            // deadline concurrently disabled by ACK progress. The core itself
            // checks active state; selected batches survive later close.
            SessionEffect::Sends(state.core.on_timer(now))
        })
    }

    fn dispatch(&self, effect: SessionEffect) -> Result<(), TaskError> {
        match effect {
            SessionEffect::Sends(batch) => batch.dispatch(),
            SessionEffect::Ack(mut processor) => loop {
                // next() may change deadlines even when it returns None. All
                // processor transitions participate in the same wake protocol.
                let handoff = self.transition(|_, now| processor.next(now))?;
                let Some(handoff) = handoff else {
                    break;
                };
                let mut delivered = Ok(());
                processor = handoff.deliver(|action| {
                    delivered = match action {
                        AckAction::ReplicationSends(batch) => {
                            batch.dispatch();
                            Ok(())
                        }
                        AckAction::CopyReplicationGate {
                            copy_applied,
                            replication_applied,
                        } => self
                            .owner
                            .replication_gate(copy_applied, replication_applied),
                        AckAction::PrimaryProgress(notification) => {
                            notification.dispatch();
                            Ok(())
                        }
                        AckAction::CopyAck { received, applied } => {
                            self.owner.copy_ack(received, applied)
                        }
                    };
                });
                delivered.map_err(TaskError::Owner)?;
            },
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct SessionHandle {
    root: Weak<SessionRoot>,
    task: Arc<TaskState>,
}

impl SessionHandle {
    fn root(&self) -> Result<Arc<SessionRoot>, AccessError> {
        self.task.check()?;
        self.root.upgrade().ok_or(AccessError::Stopped)
    }

    pub(super) fn add(&self, operation: SenderOperation, hint: Lsn) -> Result<(), AccessError> {
        self.root()?.transition(|state, now| {
            state.admit(true)?;
            let batch = state.core.add(operation, hint, now);
            state.effects.push_back(SessionEffect::Sends(batch));
            Ok(())
        })?
    }

    pub(super) fn add_replay(
        &self,
        operations: Vec<SenderOperation>,
        hint: Lsn,
    ) -> Result<(), AccessError> {
        self.root()?.transition(|state, now| {
            state.admit(true)?;
            let batch = state.core.add_replay(operations, hint, now);
            state.effects.push_back(SessionEffect::Sends(batch));
            Ok(())
        })?
    }

    pub(super) fn update_sending_epoch(&self, epoch: Epoch) -> Result<(), AccessError> {
        self.root()?
            .transition(|state, _| state.core.update_sending_epoch(epoch))
    }

    pub(super) fn update_ack(
        &self,
        update: AckUpdate,
        progress: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<(), AccessError> {
        self.root()?.transition(|state, _| {
            state.admit(!state.core.is_processing())?;
            if let Some(processor) = state.core.update_ack(update, progress) {
                state.effects.push_back(SessionEffect::Ack(processor));
            }
            Ok(())
        })?
    }

    pub(super) fn progress(&self, completed_hint: Lsn) -> Result<SenderProgress, AccessError> {
        self.root()?
            .transition(|state, _| state.core.progress(completed_hint))
    }

    pub(super) fn retry_deadline(&self) -> Result<Option<Duration>, AccessError> {
        self.root()?
            .transition(|state, _| state.core.retry_deadline())
    }

    pub(super) fn is_processing(&self) -> Result<bool, AccessError> {
        self.root()?
            .transition(|state, _| state.core.is_processing())
    }

    /// Native session close, not task shutdown. ACK ingress and selected effects
    /// can still outlive it; the runtime owner must subsequently shut down.
    pub(super) fn close(&self) -> Result<(), AccessError> {
        self.root()?.transition(|state, _| state.core.close())
    }
}

#[must_use = "shutdown and join this task owner; Drop is abort containment only"]
pub(super) struct SessionRuntime {
    root: Option<Arc<SessionRoot>>,
    task: OwnedTask,
}

impl SessionRuntime {
    pub(super) fn start(
        identity: SessionIdentity,
        epoch: Epoch,
        last_ack: Lsn,
        config: SenderConfig,
        send: Arc<dyn Fn(SendIntent) -> bool + Send + Sync>,
        owner: Arc<dyn CopyAckOwner>,
        clock: Clock,
    ) -> Self {
        let core = ReplicationSession::new(identity, epoch, last_ack, config, send, clock.now());
        core.open();
        let task = TaskState::new();
        let root = Arc::new(SessionRoot {
            clock,
            task: task.clone(),
            state: Mutex::new(SessionState {
                core,
                effects: VecDeque::new(),
                shutdown: false,
            }),
            owner,
        });
        let worker = root.clone();
        let join = tokio::spawn(async move {
            let _failure = FailureGuard(worker.task.clone());
            loop {
                let notified = worker.task.wake.notified();
                tokio::pin!(notified);
                // Register BEFORE inspecting state: idle-to-scheduled changes
                // cannot fall between observation and waiter registration.
                notified.as_mut().enable();
                match worker.next()? {
                    SessionWork::Effect(effect) => worker.dispatch(effect)?,
                    SessionWork::Wait(Some(deadline)) => tokio::select! {
                        _ = tokio::time::sleep_until(worker.clock.instant(deadline)) => {
                            let selected = worker.enter_timer()?;
                            worker.dispatch(selected)?;
                        },
                        _ = &mut notified => {},
                    },
                    SessionWork::Wait(None) => notified.await,
                    SessionWork::Done => return Ok(()),
                }
            }
        });
        Self {
            root: Some(root),
            task: OwnedTask {
                state: task,
                join: Some(join),
            },
        }
    }

    pub(super) fn handle(&self) -> SessionHandle {
        SessionHandle {
            root: self.root.as_ref().map(Arc::downgrade).unwrap_or_default(),
            task: self.task.state.clone(),
        }
    }

    /// Native close alone intentionally permits ACK processors to outlive it.
    /// Shutdown closes, drains admitted work and terminates the runtime owner.
    pub(super) async fn shutdown(&mut self) -> Result<(), TaskError> {
        if let Some(root) = self.root.as_ref()
            && self.task.state.check().is_ok()
        {
            // Wake even on poison; the worker will report that failure at join.
            let _ = root.transition(|state, _| {
                state.core.close();
                state.shutdown = true;
            });
        }
        self.join().await
    }

    /// Observe a shutdown/failure. Native handle.close() alone deliberately
    /// leaves the worker alive; use shutdown() for graceful termination.
    pub(super) async fn join(&mut self) -> Result<(), TaskError> {
        let result = self.task.join().await;
        self.root.take();
        result
    }
}

struct AckState {
    core: AckSender,
    effects: VecDeque<AckDispatch>,
    active: bool,
    shutdown: bool,
}

struct AckRoot {
    clock: Clock,
    task: Arc<TaskState>,
    state: Mutex<AckState>,
    immediate: bool,
}

enum AckWork {
    Effect(AckDispatch),
    Wait(Option<Duration>),
    Done,
}

impl AckRoot {
    fn transition<T>(
        &self,
        change: impl FnOnce(&mut AckState, Duration) -> T,
    ) -> Result<T, AccessError> {
        let _wake = WakeOnDrop(&self.task.wake);
        let mut state = self.state.lock().map_err(|_| AccessError::Poisoned)?;
        self.task.check()?;
        Ok(change(&mut state, self.clock.now()))
    }

    fn next(&self) -> Result<AckWork, AccessError> {
        let mut state = self.state.lock().map_err(|_| AccessError::Poisoned)?;
        self.task.check()?;
        if let Some(effect) = state.effects.pop_front() {
            return Ok(AckWork::Effect(effect));
        }
        if state.shutdown {
            self.task.finish();
            return Ok(AckWork::Done);
        }
        Ok(AckWork::Wait(state.core.deadline()))
    }

    fn enter_timer(&self) -> Result<Option<AckDispatch>, AccessError> {
        self.transition(|state, now| {
            // Force may disable the deadline AFTER this callback entered.
            // Native cancellation is advisory, so only the core's active test
            // may suppress it, not a new deadline check.
            state.core.on_timer(now)
        })
    }
}

#[derive(Clone)]
pub(super) struct AckHandle {
    root: Weak<AckRoot>,
    task: Arc<TaskState>,
}

impl AckHandle {
    fn root(&self) -> Result<Arc<AckRoot>, AccessError> {
        self.task.check()?;
        self.root.upgrade().ok_or(AccessError::Stopped)
    }

    pub(super) fn open(
        &self,
        callback: Arc<dyn Fn(bool) -> bool + Send + Sync>,
    ) -> Result<(), AccessError> {
        self.root()?.transition(|state, now| {
            if state.shutdown {
                return Err(AccessError::ShuttingDown);
            }
            // Every activation needs a reserved final-close slot. Reopening
            // cannot bypass the bound by repeatedly staging final ACKs.
            if state.effects.len() >= MAX_STAGED_EFFECTS {
                return Err(AccessError::StagingFull);
            }
            state.core.open(callback, now);
            state.active = true;
            Ok(())
        })?
    }

    pub(super) fn schedule(&self, force: bool) -> Result<(), AccessError> {
        let root = self.root()?;
        root.transition(|state, now| {
            if state.shutdown {
                return Err(AccessError::ShuttingDown);
            }
            if state.active
                && (force || root.immediate)
                && state.effects.len() >= MAX_STAGED_EFFECTS - 1
            {
                return Err(AccessError::StagingFull);
            }
            if let Some(effect) = state.core.schedule(force, now) {
                state.effects.push_back(effect);
            }
            Ok(())
        })?
    }

    /// Native close stages a pending-only final ACK but permits later reopen.
    /// It does not discharge the runtime owner's shutdown/join obligation.
    pub(super) fn close(&self) -> Result<(), AccessError> {
        self.root()?.transition(|state, now| {
            if let Some(effect) = state.core.close(now) {
                state.effects.push_back(effect);
            }
            state.active = false;
        })
    }

    pub(super) fn deadline(&self) -> Result<Option<Duration>, AccessError> {
        self.root()?.transition(|state, _| state.core.deadline())
    }

    pub(super) fn last_issued(&self) -> Result<Option<Duration>, AccessError> {
        self.root()?.transition(|state, _| state.core.last_issued())
    }

    pub(super) fn force_on_pending_limit(&self) -> Result<bool, AccessError> {
        self.root()?
            .transition(|state, _| state.core.force_on_pending_limit())
    }

    pub(super) fn set_trace_interval(&self, interval: Duration) -> Result<(), AccessError> {
        self.root()?
            .transition(|state, _| state.core.set_trace_interval(interval))
    }
}

#[must_use = "shutdown and join this task owner; Drop is abort containment only"]
pub(super) struct AckRuntime {
    root: Option<Arc<AckRoot>>,
    task: OwnedTask,
}

impl AckRuntime {
    pub(super) fn start(
        batch: Duration,
        retry: Duration,
        max_pending: usize,
        trace_interval: Duration,
        clock: Clock,
    ) -> Self {
        let task = TaskState::new();
        let root = Arc::new(AckRoot {
            clock,
            task: task.clone(),
            state: Mutex::new(AckState {
                core: AckSender::new(batch, retry, max_pending, trace_interval),
                effects: VecDeque::new(),
                active: false,
                shutdown: false,
            }),
            immediate: batch.is_zero(),
        });
        let worker = root.clone();
        let join = tokio::spawn(async move {
            let _failure = FailureGuard(worker.task.clone());
            loop {
                let notified = worker.task.wake.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                match worker.next()? {
                    AckWork::Effect(effect) => effect.dispatch(),
                    AckWork::Wait(Some(deadline)) => tokio::select! {
                        _ = tokio::time::sleep_until(worker.clock.instant(deadline)) => {
                            if let Some(selected) = worker.enter_timer()? {
                                selected.dispatch();
                            }
                        },
                        _ = &mut notified => {},
                    },
                    AckWork::Wait(None) => notified.await,
                    AckWork::Done => return Ok(()),
                }
            }
        });
        Self {
            root: Some(root),
            task: OwnedTask {
                state: task,
                join: Some(join),
            },
        }
    }

    pub(super) fn handle(&self) -> AckHandle {
        AckHandle {
            root: self.root.as_ref().map(Arc::downgrade).unwrap_or_default(),
            task: self.task.state.clone(),
        }
    }

    pub(super) async fn shutdown(&mut self) -> Result<(), TaskError> {
        if let Some(root) = self.root.as_ref()
            && self.task.state.check().is_ok()
        {
            let _ = root.transition(|state, now| {
                if let Some(effect) = state.core.close(now) {
                    state.effects.push_back(effect);
                }
                state.active = false;
                state.shutdown = true;
            });
        }
        self.join().await
    }

    /// Wait for shutdown/failure, retaining ownership if this wait is cancelled.
    /// close() alone keeps the worker alive so the native core can reopen.
    pub(super) async fn join(&mut self) -> Result<(), TaskError> {
        let result = self.task.join().await;
        self.root.take();
        result
    }
}

#[cfg(test)]
mod tests;
