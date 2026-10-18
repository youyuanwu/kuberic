// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation of CopyAsyncOperation.h/.cpp and enumeration
// semantics from ComProxyAsyncEnumOperation.cpp, microsoft/service-fabric
// @3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d,
// src/prod/src/Reliability/Replication/. See sf-v1-port-provenance.md.

//! Private copy/context enumeration and reliable sending, not CopySender's
//! replication build gate, provider acquisition, establishment or role owner.
//! Drive on an executor outside owner guards. The stream stays owned across
//! cancelled drive futures; abandoning that future is not native cancellation.
//! Handles are weak. All external polls, callbacks, wakes and final payload
//! releases occur outside the short transition mutex.

#![cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 copy enumeration")
)]

use std::future::{pending, poll_fn};
use std::sync::{Arc, Mutex, Weak};
use std::task::Poll;
use std::time::Duration;

use tokio::sync::Notify;

use crate::application::{Lsn, OperationData, OperationDataStream};
use crate::error::RuntimeError;
use crate::protocol::types::Epoch;

use super::async_runtime::Clock;
use super::buffers::{BufferError, BufferLimits, BufferPolicy};
use super::operation_queue::{EnqueueError, OperationQueue, QueueConfig, QueueOperation};
use super::sender::{
    ReliableOperationSender, SendAction, SendBatch, SenderConfig, SenderOperation, SenderProgress,
};

#[derive(Clone, Copy)]
pub(super) struct CopyConfig {
    /// Native copy queue and initial/max sender windows use the same settings.
    pub(super) sender: SenderConfig,
    pub(super) buffer_policy: BufferPolicy,
    pub(super) buffer_limits: BufferLimits,
}

impl CopyConfig {
    pub(super) fn validate(self) -> Result<(), CopyError> {
        let settings = self.sender;
        if settings.initial_window <= 1
            || !settings.initial_window.is_power_of_two()
            || !settings.max_window.is_power_of_two()
            || settings.initial_window > settings.max_window
            || settings.retry.is_zero()
            || !(0.0..=1.0).contains(&settings.decay_factor)
            || settings.decay_interval.is_zero()
        {
            return Err(CopyError::InvalidConfig);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, thiserror::Error)]
pub(super) enum CopyError {
    #[error(
        "copy capacities must be powers of two with 1 < initial <= maximum; retry/decay settings must be valid"
    )]
    InvalidConfig,
    #[error("copy operation was cancelled")]
    Cancelled,
    #[error("provider EndGetNext failed: {0}")]
    Provider(#[source] Arc<RuntimeError>),
    #[error(transparent)]
    Buffer(#[from] BufferError),
    #[error("copy sequence number exhausted")]
    SequenceExhausted,
    #[error("copy ACK is negative, beyond admitted data, inconsistent, or not the known terminal")]
    InvalidAck,
    #[error("copy enumeration queue rejected its next unique operation")]
    QueueInvariant,
    #[error("copy operation has not completed; End cannot close its sender")]
    NotComplete,
    #[error("copy operation has ended")]
    Ended,
    #[error("copy operation owner was dropped")]
    OwnerDropped,
    #[error("copy operation transition mutex is poisoned")]
    Poisoned,
    #[error("copy has not begun")]
    NotStarted,
    #[error("copy may only begin once")]
    AlreadyStarted,
    #[error("replication progress must be nonnegative")]
    InvalidReplicationProgress,
    #[error("copy context does not wait for replication ACKs")]
    ReplicationNotRequired,
    #[error("build completion is already disabled")]
    AlreadyDisabled,
    #[error("build completed successfully after completion was disabled")]
    BuildCompletionDisabled,
    #[error("capturing the replication target failed: {0}")]
    ReplicationTarget(#[source] Arc<RuntimeError>),
    #[error("remote copy/context failed with error {0}")]
    Remote(i32),
}

#[derive(Clone, Debug)]
pub(super) struct CopyFault {
    pub(super) error: Arc<RuntimeError>,
    pub(super) description: &'static str,
}

#[derive(Clone, Debug)]
pub(super) struct CopyEnd {
    pub(super) result: Result<(), CopyError>,
    /// Provider or outer CopySender fault; no host report is performed here.
    pub(super) fault: Option<CopyFault>,
}

#[derive(Clone, Debug)]
pub(super) struct CopySnapshot {
    pub(super) queued: usize,
    pub(super) last_admitted: Lsn,
    pub(super) parked: Option<Lsn>,
    pub(super) terminal: Option<Lsn>,
    pub(super) provider_in_flight: bool,
    pub(super) enumeration_done: bool,
    pub(super) cancel_requested: bool,
    pub(super) ready_to_complete: bool,
    pub(super) result: Option<Result<(), CopyError>>,
}

pub(super) enum CopySendAction {
    Operation {
        operation: Arc<QueueOperation>,
        is_last: bool,
    },
    RequestAck,
}

type SendCallback = Arc<dyn Fn(CopySendAction) -> bool + Send + Sync>;
type LastCallback = Arc<dyn Fn(Lsn) + Send + Sync>;

struct State {
    queue: OperationQueue,
    sender: ReliableOperationSender,
    parked: Option<Arc<QueueOperation>>,
    terminal: Option<Lsn>,
    provider_in_flight: bool,
    cancel_requested: bool,
    ready_to_complete: bool,
    result: Option<Result<(), CopyError>>,
    fault: Option<Arc<RuntimeError>>,
    ended: bool,
}

impl State {
    fn complete(&mut self, result: Result<(), CopyError>) -> bool {
        if self.result.is_some() {
            return false;
        }
        self.result = Some(result);
        true
    }
}

struct Root {
    config: CopyConfig,
    clock: Clock,
    state: Mutex<State>,
    provider: Mutex<Option<OperationDataStream>>,
    wake: Notify,
    send: SendCallback,
    last: LastCallback,
}

/// Already-selected work survives End or owner release and may reenter handles.
#[must_use = "dispatch selected copy sends outside all owner guards"]
pub(super) struct CopySendBatch {
    root: Arc<Root>,
    batch: SendBatch,
    terminal: Option<Lsn>,
}

impl CopySendBatch {
    pub(super) fn dispatch(self) {
        self.batch.dispatch(|action, hint| {
            debug_assert_eq!(hint, 0);
            let action = match action {
                SendAction::Operation(entry) => CopySendAction::Operation {
                    is_last: self.terminal == Some(entry.operation.lsn()),
                    operation: entry.operation,
                },
                SendAction::RequestAck => CopySendAction::RequestAck,
            };
            (self.root.send)(action)
        });
    }
}

impl Root {
    fn batch(self: &Arc<Self>, batch: SendBatch, terminal: Option<Lsn>) -> CopySendBatch {
        CopySendBatch {
            root: self.clone(),
            batch,
            terminal,
        }
    }

    fn add(self: &Arc<Self>, operation: Arc<QueueOperation>) -> Result<CopySendBatch, CopyError> {
        let mut state = self.state.lock().map_err(|_| CopyError::Poisoned)?;
        let batch = state.sender.add(
            SenderOperation {
                operation: operation.clone(),
                original_epoch: Epoch::new(0, 0),
            },
            0,
            self.clock.now(),
        );
        Ok(self.batch(batch, state.terminal))
    }

    fn begin_next(&self) -> Result<bool, CopyError> {
        let mut state = self.state.lock().map_err(|_| CopyError::Poisoned)?;
        if state.result.is_some() || (state.cancel_requested && !state.provider_in_flight) {
            state.complete(Err(CopyError::Cancelled));
            return Ok(false);
        }
        state.provider_in_flight = true;
        Ok(true)
    }

    fn finish_next(
        self: &Arc<Self>,
        item: Option<crate::Result<OperationData>>,
    ) -> Result<(), CopyError> {
        let (lsn, data, is_last) = {
            let mut state = self.state.lock().map_err(|_| CopyError::Poisoned)?;
            state.provider_in_flight = false;
            if let Some(Err(error)) = item {
                if matches!(error, RuntimeError::OperationCancelled) {
                    state.complete(Err(CopyError::Cancelled));
                } else {
                    let error = Arc::new(error);
                    state.fault = Some(error.clone());
                    state.complete(Err(CopyError::Provider(error)));
                }
                return Ok(());
            }
            if state.result.is_some() || state.cancel_requested {
                state.complete(Err(CopyError::Cancelled));
                return Ok(());
            }
            let lsn = state.queue.progress().tail;
            if lsn == Lsn::MAX {
                state.complete(Err(CopyError::SequenceExhausted));
                return Ok(());
            }
            match item {
                Some(Ok(data)) => (lsn, data, false),
                None => (lsn, OperationData::new(), true),
                Some(Err(_)) => unreachable!(),
            }
        };
        // Decoding and rejected payload destruction may execute foreign owners.
        let operation = if is_last {
            QueueOperation::new(lsn, data, false)
        } else {
            match QueueOperation::from_data(
                lsn,
                data,
                false,
                self.config.buffer_policy,
                self.config.buffer_limits,
            ) {
                Ok(operation) => operation,
                Err(error) => {
                    self.state
                        .lock()
                        .map_err(|_| CopyError::Poisoned)?
                        .complete(Err(error.into()));
                    return Ok(());
                }
            }
        };
        let admitted = {
            let mut state = self.state.lock().map_err(|_| CopyError::Poisoned)?;
            if state.result.is_some() || state.cancel_requested {
                state.complete(Err(CopyError::Cancelled));
                return Ok(());
            }
            let admitted = match state.queue.try_enqueue(operation.clone()) {
                Ok(()) => true,
                Err(EnqueueError::Full) => {
                    assert!(state.parked.is_none());
                    state.parked = Some(operation.clone());
                    false
                }
                Err(EnqueueError::Duplicate) => {
                    state.complete(Err(CopyError::QueueInvariant));
                    return Ok(());
                }
            };
            if is_last {
                state.terminal = Some(lsn);
            }
            admitted
        };
        if is_last {
            (self.last)(lsn);
        }
        if admitted {
            self.add(operation)?.dispatch();
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct CopyHandle {
    root: Weak<Root>,
}

impl CopyHandle {
    fn root(&self) -> Result<Arc<Root>, CopyError> {
        self.root.upgrade().ok_or(CopyError::OwnerDropped)
    }

    pub(super) fn snapshot(&self) -> Result<CopySnapshot, CopyError> {
        let root = self.root()?;
        let state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
        Ok(CopySnapshot {
            queued: state.queue.operation_count(),
            last_admitted: state.queue.progress().tail - 1,
            parked: state.parked.as_ref().map(|operation| operation.lsn()),
            terminal: state.terminal,
            provider_in_flight: state.provider_in_flight,
            enumeration_done: state.terminal.is_some(),
            cancel_requested: state.cancel_requested,
            ready_to_complete: state.ready_to_complete,
            result: state.result.clone(),
        })
    }

    pub(super) fn progress(&self) -> Result<SenderProgress, CopyError> {
        let root = self.root()?;
        let result = root
            .state
            .lock()
            .map_err(|_| CopyError::Poisoned)?
            .sender
            .progress(0);
        Ok(result)
    }

    pub(super) fn averages(&self) -> Result<(Duration, Duration), CopyError> {
        let root = self.root()?;
        let result = root
            .state
            .lock()
            .map_err(|_| CopyError::Poisoned)?
            .sender
            .averages();
        Ok(result)
    }

    pub(super) fn cancel(&self) -> Result<(), CopyError> {
        let root = self.root()?;
        {
            let mut state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
            state.cancel_requested = true;
            if state.parked.is_some() || state.terminal.is_some() {
                state.complete(Err(CopyError::Cancelled));
            }
        }
        root.wake.notify_one();
        Ok(())
    }

    /// Native CopySender::FinishOperation, not enumeration cancellation timing.
    pub(super) fn complete_error(&self, error: CopyError) -> Result<bool, CopyError> {
        let root = self.root()?;
        let completed = root
            .state
            .lock()
            .map_err(|_| CopyError::Poisoned)?
            .complete(Err(error));
        root.wake.notify_one();
        Ok(completed)
    }

    /// Inputs are already normalized by CopySender, not wire -1
    /// sentinels. Sender dispatch precedes queue completion and parked replay.
    pub(super) fn update_progress(
        &self,
        received: Lsn,
        applied: Lsn,
        is_last: bool,
    ) -> Result<(), CopyError> {
        let root = self.root()?;
        let (batch, retired) = {
            let mut state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
            if state.ended {
                return Err(CopyError::Ended);
            }
            let last = state.queue.progress().tail - 1;
            if received < 0
                || applied < 0
                || received > last
                || applied > last
                || (received != 0 && received < applied)
                || (is_last && state.terminal != Some(applied))
            {
                return Err(CopyError::InvalidAck);
            }
            let retired = state
                .sender
                .pending_operations()
                .filter(|entry| is_last || entry.operation.lsn() <= received)
                .map(|entry| entry.operation.clone())
                .collect::<Vec<_>>();
            let batch = if received == 0 {
                None
            } else if is_last {
                state.sender.close();
                None
            } else {
                let result = state.sender.on_ack(received, applied, root.clock.now());
                Some(root.batch(result.sends, state.terminal))
            };
            (batch, retired)
        };
        drop(retired);
        if let Some(batch) = batch {
            batch.dispatch();
        }
        let mut retired = Vec::new();
        let admitted = {
            let mut state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
            let first = state.queue.progress().complete_head;
            retired.extend(
                (first..=applied).filter_map(|lsn| state.queue.get_operation(lsn).cloned()),
            );
            let advanced = state.queue.complete_through(applied);
            if advanced {
                if let Some(operation) = state.parked.as_ref().cloned() {
                    match state.queue.try_enqueue(operation.clone()) {
                        Ok(()) => {
                            state.parked = None;
                            Some(operation)
                        }
                        Err(EnqueueError::Full) => None,
                        Err(EnqueueError::Duplicate) => {
                            state.complete(Err(CopyError::QueueInvariant));
                            return Err(CopyError::QueueInvariant);
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            }
        };
        drop(retired);
        if let Some(operation) = admitted {
            root.add(operation)?.dispatch();
        }
        if is_last {
            root.state
                .lock()
                .map_err(|_| CopyError::Poisoned)?
                .ready_to_complete = true;
        }
        root.wake.notify_one();
        Ok(())
    }

    /// Explicit outer-owner request, NOT proof that the replication build gate
    /// exists or has passed. A final copy ACK alone never invokes this method.
    pub(super) fn try_complete_with_success_if_copy_done(&self) -> Result<bool, CopyError> {
        let root = self.root()?;
        let completed = {
            let mut state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
            if state.ready_to_complete {
                state.complete(Ok(()))
            } else {
                false
            }
        };
        root.wake.notify_one();
        Ok(completed)
    }

    pub(super) fn on_timer(&self) -> Result<CopySendBatch, CopyError> {
        let root = self.root()?;
        let mut state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
        let batch = state.sender.on_timer(root.clock.now());
        Ok(root.batch(batch, state.terminal))
    }

    /// Native End is separate from completion/cancellation and closes retries.
    pub(super) fn end(&self) -> Result<CopyEnd, CopyError> {
        let root = self.root()?;
        let (end, retired) = {
            let mut state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
            let result = state.result.clone().ok_or(CopyError::NotComplete)?;
            let retired = state
                .sender
                .pending_operations()
                .map(|entry| entry.operation.clone())
                .collect::<Vec<_>>();
            state.sender.close();
            state.ended = true;
            (
                CopyEnd {
                    result,
                    fault: state.fault.as_ref().map(|error| CopyFault {
                        error: error.clone(),
                        description: "EndGetNext",
                    }),
                },
                retired,
            )
        };
        drop(retired);
        root.wake.notify_one();
        Ok(end)
    }
}

pub(super) struct CopyAsyncOperation {
    root: Arc<Root>,
}

// Only the uniquely borrowed driver leases the stream. Cancellation returns it
// to the callback root without holding a guard across any foreign poll or drop.
struct ProviderLease<'a> {
    root: &'a Root,
    provider: Option<OperationDataStream>,
}

impl Drop for ProviderLease<'_> {
    fn drop(&mut self) {
        let mut slot = self.root.provider.lock().unwrap();
        assert!(slot.is_none());
        *slot = self.provider.take();
    }
}

enum ProviderPoll {
    Item(Option<crate::Result<OperationData>>),
    Skipped,
}

impl CopyAsyncOperation {
    pub(super) fn new(
        config: CopyConfig,
        provider: OperationDataStream,
        send: SendCallback,
        last: LastCallback,
        clock: Clock,
    ) -> Result<Self, CopyError> {
        let settings = config.sender;
        config.validate()?;
        let queue = OperationQueue::new(
            QueueConfig {
                initial_capacity: settings.initial_window,
                max_capacity: settings.max_window,
                max_bytes: 0,
                max_completed_count: 0,
                max_completed_bytes: 0,
                clean_on_complete: true,
            },
            1,
            false,
            true,
        );
        let mut sender = ReliableOperationSender::new(settings, 0, clock.now());
        sender.open();
        Ok(Self {
            root: Arc::new(Root {
                config,
                clock,
                state: Mutex::new(State {
                    queue,
                    sender,
                    parked: None,
                    terminal: None,
                    provider_in_flight: false,
                    cancel_requested: false,
                    ready_to_complete: false,
                    result: None,
                    fault: None,
                    ended: false,
                }),
                provider: Mutex::new(Some(provider)),
                wake: Notify::new(),
                send,
                last,
            }),
        })
    }

    pub(super) fn handle(&self) -> CopyHandle {
        CopyHandle {
            root: Arc::downgrade(&self.root),
        }
    }

    /// Drives serial provider polls and the actual sender's one-shot deadlines.
    /// In-flight provider work is retained across cancel/timer/wake branches.
    /// ACK ingress only wakes this future; it never polls the provider inline.
    pub(super) async fn drive(&mut self) -> Result<(), CopyError> {
        let root = self.root.clone();
        let provider = root
            .provider
            .lock()
            .map_err(|_| CopyError::Poisoned)?
            .take()
            .expect("the unique driver owns the only provider lease");
        let mut provider = ProviderLease {
            root: &root,
            provider: Some(provider),
        };
        loop {
            let notified = root.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (can_poll, deadline, result) = {
                let mut state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
                if state.cancel_requested && !state.provider_in_flight {
                    state.complete(Err(CopyError::Cancelled));
                }
                (
                    state.provider_in_flight
                        || (state.parked.is_none() && state.terminal.is_none()),
                    state.sender.deadline(),
                    state.result.clone(),
                )
            };
            if let Some(result) = result {
                return result;
            }
            let timer = async {
                match deadline {
                    Some(deadline) => {
                        tokio::time::sleep(deadline.saturating_sub(root.clock.now())).await;
                    }
                    None => pending().await,
                }
            };
            tokio::select! {
                item = poll_fn(|cx| {
                    match root.begin_next() {
                        Ok(true) => provider.provider.as_mut().unwrap().as_mut().poll_next(cx)
                            .map(|item| Ok(ProviderPoll::Item(item))),
                        Ok(false) => Poll::Ready(Ok(ProviderPoll::Skipped)),
                        Err(error) => Poll::Ready(Err(error)),
                    }
                }), if can_poll => {
                    if let ProviderPoll::Item(item) = item? {
                        root.finish_next(item)?;
                    }
                }
                _ = notified.as_mut() => {}
                _ = timer => self.handle().on_timer()?.dispatch(),
            }
        }
    }
}

#[cfg(test)]
mod tests;
