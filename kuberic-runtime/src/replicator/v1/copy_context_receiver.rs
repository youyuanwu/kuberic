// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided Rust translation of CopyContextReceiver.h:12-85,
// CopyContextReceiver.cpp:27-63,80-97,105-145,147-234,
// ComOperationDataAsyncEnumerator.cpp:59-75,90-157,188-196,
// AckSender.cpp:98-208, ReplicationSession.cpp:466-509 and
// ReplicationTransport.cpp:776-802:
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d,
// src/prod/src/Reliability/Replication/. See sf-v1-port-provenance.md.

//! Primary-side receipt of persisted target context, not context generation.
//!
//! The caller serializes short transitions and dispatches every returned effect
//! outside its guard. Queue callbacks only stage entries. ACK callbacks sample
//! current progress at issuance, release the snapshot lock, then call transport.
//! No provider is invoked, and context consumers have no service ACK obligation.
//!
//! Shared Rust streams replace COM enumerators and their Begin/End operations.
//! A pending poll represents an in-flight native dequeue: successful explicit
//! close permits that poll to finish, but rejects subsequent polls. Cancelling a
//! `next()` future does not undo that registration. Error close aborts pending
//! polls. Protocol terminal EOF, ordinary empty data and explicit close differ.
//! Native transport validation is represented by explicit local input errors;
//! RemoteSession ownership, copy sending/build and cross-receiver drain are not
//! implemented here. Payload bytes share backing storage rather than retaining
//! a COM operation wrapper. Queue/snapshot Arcs replace ComponentRoot/enumerator
//! pointer cycles; dispatch effects replace native Threadpool::Post. Callers
//! must explicitly close before retiring the owner; Drop fabricates no EOF.
//! On error close, shared queue abort retires inaccessible data immediately
//! rather than retaining COM wrappers until failed dequeue/owner destruction.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use thiserror::Error;

use crate::application::{Lsn, OperationData};

use super::ack::{AckDispatch, AckSender};
use super::operation_queue::{
    CallbackControl, EnqueueError, OperationQueue, QueueConfig, QueueOperation,
};
use super::operation_stream::{
    DispatchEntry, DispatchWake, EpochBarrier, OperationStream, SharedDispatchQueue, StreamError,
    StreamEvent,
};
use super::received_operation::{AckState, ReceivedOperation};

#[derive(Clone, Copy, Debug)]
pub(super) struct CopyContextConfig {
    pub(super) initial_queue_size: usize,
    pub(super) max_queue_size: usize,
    pub(super) batch_ack_interval: Duration,
    pub(super) retry_interval: Duration,
    pub(super) max_pending_acknowledgements: usize,
    pub(super) trace_interval: Duration,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(super) enum CopyContextError {
    #[error("copy-context count limits must be ordered powers of two greater than one")]
    InvalidQueueConfig,
    #[error("copy-context ACK batching must be shorter than retry and the threshold representable")]
    InvalidAckConfig,
    #[error("copy-context ACK sender is already open")]
    AlreadyOpen,
    #[error("copy-context receiver is closed")]
    Closed,
    #[error("copy-context operation {0} is not fresh, positive, data-only context")]
    InvalidOperation(Lsn),
    #[error("copy-context terminal {0} must be empty and follow all admitted data")]
    InvalidTerminal(Lsn),
    #[error("copy-context data {0} does not precede the terminal marker")]
    DataAfterTerminal(Lsn),
    #[error("copy-context enumerator was explicitly closed (owner error {0})")]
    EnumeratorClosed(i32),
    #[error("unexpected epoch handoff in data-only context: {0:?}")]
    UnexpectedEpoch(EpochBarrier),
    #[error("unexpected service EOS operation in data-only context")]
    UnexpectedEndOfStream,
    #[error(transparent)]
    Stream(#[from] StreamError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CopyContextAck {
    pub(super) completed: Lsn,
    pub(super) error_code: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ContextAdmission {
    NoInput,
    Accepted,
    Terminal,
    Duplicate,
    Full,
    IgnoredCompleted,
    IgnoredClosed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ContextObservation {
    pub(super) admission: ContextAdmission,
    /// Native ProcessCopyContext's true result, not transport acceptance.
    pub(super) ack_requested: bool,
    pub(super) force_ack: bool,
}

/// Includes rejected/retired input roots, so even their destructors run outside
/// the caller's guard. Errors are delivered there rather than dropping input in
/// an early `Result::Err` return while the owner remains serialized.
#[must_use = "dispatch context wakeups, transport callbacks and retirement outside owner guards"]
pub(super) struct CopyContextEffects {
    wakes: Vec<DispatchWake>,
    acknowledgements: Vec<AckDispatch>,
    retired: Vec<ReceivedOperation>,
    outcome: Result<ContextObservation, CopyContextError>,
}

impl CopyContextEffects {
    fn new(admission: ContextAdmission) -> Self {
        Self {
            wakes: Vec::new(),
            acknowledgements: Vec::new(),
            retired: Vec::new(),
            outcome: Ok(ContextObservation {
                admission,
                ack_requested: false,
                force_ack: false,
            }),
        }
    }

    pub(super) fn dispatch(self) -> Result<ContextObservation, CopyContextError> {
        for wake in self.wakes {
            wake.dispatch();
        }
        drop(self.retired);
        for acknowledgement in self.acknowledgements {
            acknowledgement.dispatch();
        }
        self.outcome
    }
}

struct AckSnapshot {
    completed: Lsn,
    terminal: Option<Lsn>,
    last_ack_sent: Lsn,
    error_code: i32,
}

impl AckSnapshot {
    fn current(&self) -> CopyContextAck {
        CopyContextAck {
            completed: match self.terminal {
                Some(terminal) if self.completed == terminal - 1 => terminal,
                _ => self.completed,
            },
            error_code: self.error_code,
        }
    }
}

type SendContextAck = Arc<dyn Fn(CopyContextAck, bool) -> bool + Send + Sync>;

pub(super) struct CopyContextStream {
    inner: OperationStream,
    explicitly_closed: Arc<Mutex<Option<i32>>>,
    pending: bool,
    terminated: bool,
}

impl Stream for CopyContextStream {
    type Item = Result<OperationData, CopyContextError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.terminated {
            return Poll::Ready(None);
        }
        let closed = *this.explicitly_closed.lock().unwrap();
        if let Some(error_code) = closed
            && (error_code != 0 || !this.pending)
        {
            this.terminated = true;
            return Poll::Ready(Some(Err(CopyContextError::EnumeratorClosed(error_code))));
        }
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Pending => {
                this.pending = true;
                Poll::Pending
            }
            Poll::Ready(None) => {
                this.pending = false;
                this.terminated = true;
                Poll::Ready(None)
            }
            Poll::Ready(Some(result)) => {
                this.pending = false;
                let data = match result {
                    Ok(StreamEvent::Operation(operation)) => {
                        // Native FinishDequeue checks error close again after
                        // dequeuing; it must not leak data across that race.
                        let closed = *this.explicitly_closed.lock().unwrap();
                        match closed {
                            Some(error_code) if error_code != 0 => {
                                Err(CopyContextError::EnumeratorClosed(error_code))
                            }
                            _ if operation.is_end_of_stream() => {
                                Err(CopyContextError::UnexpectedEndOfStream)
                            }
                            _ => Ok(operation.queue_operation().data().clone()),
                        }
                    }
                    Ok(StreamEvent::UpdateEpoch(barrier)) => {
                        Err(CopyContextError::UnexpectedEpoch(barrier))
                    }
                    Err(error) => Err(error.into()),
                };
                if data.is_err() {
                    this.terminated = true;
                }
                Poll::Ready(Some(data))
            }
        }
    }
}

pub(super) struct CopyContextReceiver {
    queue: OperationQueue,
    operations: BTreeMap<Lsn, ReceivedOperation>,
    staged: Arc<Mutex<Vec<Arc<QueueOperation>>>>,
    dispatch: SharedDispatchQueue,
    snapshot: Arc<Mutex<AckSnapshot>>,
    explicitly_closed: Arc<Mutex<Option<i32>>>,
    ack_sender: AckSender,
    max_pending: Lsn,
    opened: bool,
    active: bool,
    all_received: bool,
}

impl CopyContextReceiver {
    pub(super) fn new(config: CopyContextConfig) -> Result<Self, CopyContextError> {
        if config.initial_queue_size <= 1
            || !config.initial_queue_size.is_power_of_two()
            || !config.max_queue_size.is_power_of_two()
            || config.initial_queue_size > config.max_queue_size
        {
            return Err(CopyContextError::InvalidQueueConfig);
        }
        let max_pending = Lsn::try_from(config.max_pending_acknowledgements)
            .map_err(|_| CopyContextError::InvalidAckConfig)?;
        if config.batch_ack_interval >= config.retry_interval {
            return Err(CopyContextError::InvalidAckConfig);
        }
        let mut queue = OperationQueue::new(
            QueueConfig {
                initial_capacity: config.initial_queue_size,
                max_capacity: config.max_queue_size,
                max_bytes: 0,
                max_completed_count: 0,
                max_completed_bytes: 0,
                clean_on_complete: true,
            },
            1,
            false,
            true,
        );
        let staged = Arc::new(Mutex::new(Vec::new()));
        let callback_staging = staged.clone();
        queue.set_commit_callback(Some(Box::new(move |operation, _| {
            callback_staging.lock().unwrap().push(operation);
            CallbackControl::Keep
        })));
        Ok(Self {
            queue,
            operations: BTreeMap::new(),
            staged,
            dispatch: SharedDispatchQueue::new(),
            snapshot: Arc::new(Mutex::new(AckSnapshot {
                completed: 0,
                terminal: None,
                last_ack_sent: 0,
                error_code: 0,
            })),
            explicitly_closed: Arc::new(Mutex::new(None)),
            ack_sender: AckSender::new(
                config.batch_ack_interval,
                config.retry_interval,
                config.max_pending_acknowledgements,
                config.trace_interval,
            ),
            max_pending,
            opened: false,
            active: true,
            all_received: false,
        })
    }

    pub(super) fn open(
        &mut self,
        send: &SendContextAck,
        now: Duration,
    ) -> Result<(), CopyContextError> {
        if !self.active {
            return Err(CopyContextError::Closed);
        }
        if self.opened {
            return Err(CopyContextError::AlreadyOpen);
        }
        let snapshot = self.snapshot.clone();
        // Borrow the caller's transport root so rejection cannot destroy it
        // while the receiver is serialized.
        let send = send.clone();
        self.ack_sender.open(
            Arc::new(move |should_trace| {
                let current = {
                    let mut state = snapshot.lock().unwrap();
                    let current = state.current();
                    state.last_ack_sent = current.completed;
                    current
                };
                // Native AckSender ignores false; no retry or requeue here.
                send(current, should_trace)
            }),
            now,
        );
        self.opened = true;
        Ok(())
    }

    pub(super) fn stream(&self) -> CopyContextStream {
        CopyContextStream {
            inner: self.dispatch.stream(),
            explicitly_closed: self.explicitly_closed.clone(),
            pending: false,
            terminated: false,
        }
    }

    pub(super) fn all_operations_received(&self) -> bool {
        self.all_received
    }

    pub(super) fn completed(&self) -> Lsn {
        self.queue.progress().complete_head - 1
    }

    #[cfg(test)]
    pub(super) fn current_ack(&self) -> CopyContextAck {
        self.snapshot.lock().unwrap().current()
    }

    #[cfg(test)]
    pub(super) fn last_ack_sent(&self) -> Lsn {
        self.snapshot.lock().unwrap().last_ack_sent
    }

    pub(super) fn ack_deadline(&self) -> Option<Duration> {
        self.ack_sender.deadline()
    }

    #[cfg(test)]
    pub(super) fn last_ack_issued_at(&self) -> Option<Duration> {
        self.ack_sender.last_issued()
    }

    fn request_ack(&mut self, force: bool, now: Duration, effects: &mut CopyContextEffects) {
        if let Ok(observation) = &mut effects.outcome {
            observation.ack_requested = true;
            observation.force_ack = force;
        }
        if let Some(acknowledgement) = self.ack_sender.schedule(force, now) {
            effects.acknowledgements.push(acknowledgement);
        }
    }

    fn validate(
        &self,
        operation: &ReceivedOperation,
        is_last: bool,
    ) -> Result<(), CopyContextError> {
        let entry = operation.queue_operation();
        let lsn = entry.lsn();
        if lsn <= 0
            || lsn == Lsn::MAX
            || (operation.is_end_of_stream() && !is_last)
            || operation.callback_required()
            || operation.ack_state() != AckState::NotCalled
            || !entry.needs_ack()
            || operation.epoch().data_loss_number < 0
            || operation.epoch().configuration_number < 0
        {
            return Err(CopyContextError::InvalidOperation(lsn));
        }
        if is_last {
            // Native assumes a well-formed terminal operation from transport.
            // Validate before replacing terminal metadata or completing data.
            if entry.logical_bytes() != 0 || lsn < self.queue.progress().tail {
                return Err(CopyContextError::InvalidTerminal(lsn));
            }
        } else if self
            .snapshot
            .lock()
            .unwrap()
            .terminal
            .is_some_and(|terminal| lsn >= terminal)
        {
            return Err(CopyContextError::DataAfterTerminal(lsn));
        }
        Ok(())
    }

    pub(super) fn process(
        &mut self,
        operation: ReceivedOperation,
        is_last: bool,
        now: Duration,
    ) -> CopyContextEffects {
        let mut effects = CopyContextEffects::new(ContextAdmission::NoInput);
        if !self.active || self.all_received {
            effects.outcome.as_mut().unwrap().admission = if self.active {
                ContextAdmission::IgnoredCompleted
            } else {
                ContextAdmission::IgnoredClosed
            };
            effects.retired.push(operation);
            self.request_ack(false, now, &mut effects);
            return effects;
        }
        if let Err(error) = self.validate(&operation, is_last) {
            effects.retired.push(operation);
            effects.outcome = Err(error);
            return effects;
        }
        if is_last {
            self.snapshot.lock().unwrap().terminal = Some(operation.queue_operation().lsn());
            effects.retired.push(operation);
            effects.outcome.as_mut().unwrap().admission = ContextAdmission::Terminal;
        } else {
            let admission = self.queue.try_enqueue(operation.queue_operation().clone());
            match admission {
                Ok(()) => {
                    self.operations
                        .insert(operation.queue_operation().lsn(), operation);
                    effects.outcome.as_mut().unwrap().admission = ContextAdmission::Accepted;
                    if !self.queue.complete() {
                        // Native returns false: admitting only out-of-order
                        // context does not itself schedule an ACK.
                        return effects;
                    }
                    self.snapshot.lock().unwrap().completed = self.completed();
                    if let Err(error) = self.stage_dispatch(&mut effects) {
                        effects.outcome = Err(error);
                        return effects;
                    }
                }
                Err(error) => {
                    effects.retired.push(operation);
                    effects.outcome.as_mut().unwrap().admission = match error {
                        EnqueueError::Duplicate => ContextAdmission::Duplicate,
                        EnqueueError::Full => ContextAdmission::Full,
                    };
                }
            }
        }
        let force = {
            let state = self.snapshot.lock().unwrap();
            if state
                .terminal
                .is_some_and(|terminal| state.completed == terminal - 1)
            {
                self.all_received = true;
                effects.wakes.push(self.dispatch.close());
                true
            } else {
                // Context's native threshold is strict, even when configured
                // to zero; it does not use AckSender's replication predicate.
                state.completed - state.last_ack_sent > self.max_pending
            }
        };
        self.request_ack(force, now, &mut effects);
        effects
    }

    fn stage_dispatch(&mut self, effects: &mut CopyContextEffects) -> Result<(), CopyContextError> {
        let staged = std::mem::take(&mut *self.staged.lock().unwrap());
        let mut entries = Vec::with_capacity(staged.len());
        for entry in staged {
            let operation = self
                .operations
                .remove(&entry.lsn())
                .expect("queue completion belongs to admitted context");
            entries.push(DispatchEntry::Operation(operation.clone()));
            // Also root append failures, which otherwise release their owned
            // input while the caller's receiver guard is still held.
            effects.retired.push(operation);
        }
        if !entries.is_empty() {
            effects.wakes.push(self.dispatch.append(entries)?);
        }
        Ok(())
    }

    /// Entry of an actual selected timer callback, not a deadline eligibility
    /// check. The external timer owner supplies deterministic time.
    pub(super) fn on_ack_timer(&mut self, now: Duration) -> CopyContextEffects {
        let mut effects = CopyContextEffects::new(ContextAdmission::NoInput);
        if let Some(acknowledgement) = self.ack_sender.on_timer(now) {
            effects.acknowledgements.push(acknowledgement);
        }
        effects
    }

    /// Enumerator close, queue closure, optional forced error ACK, then ACK
    /// sender close/flush. This is receiver-local, not copy/build completion.
    /// Late input follows native inactive ACK scheduling. A repeated close can
    /// update the native error state, but cannot reactivate the ACK sender.
    pub(super) fn close(&mut self, error_code: i32, now: Duration) -> CopyContextEffects {
        let mut effects = CopyContextEffects::new(ContextAdmission::NoInput);
        self.active = false;
        self.snapshot.lock().unwrap().error_code = error_code;
        *self.explicitly_closed.lock().unwrap() = Some(error_code);
        if error_code == 0 {
            effects.wakes.push(self.dispatch.close());
        } else {
            effects.wakes.push(self.dispatch.abort(StreamError::Closed));
        }
        // Native queue members retire with their receiver. Rust releases the
        // unreachable gap entries now, preserving the same incomplete cursor.
        effects
            .retired
            .extend(std::mem::take(&mut self.operations).into_values());
        self.queue.discard_noncompleted();
        if error_code != 0 {
            self.request_ack(true, now, &mut effects);
        }
        if let Some(acknowledgement) = self.ack_sender.close(now) {
            effects.acknowledgements.push(acknowledgement);
        }
        effects
    }
}

#[cfg(test)]
mod tests;
