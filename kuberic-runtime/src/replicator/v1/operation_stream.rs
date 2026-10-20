// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided Rust translation of OperationStream.h/.cpp,
// OperationStream.GetOperationAsyncOperation.h/.cpp and the DispatchQueueSPtr
// seam in ReplicationPointers.h, microsoft/service-fabric
// @3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d, Reliability/Replication.
// See sf-v1-port-provenance.md for selected source/test ancestry.

//! Shared, consuming dispatch stream, not a provider or role integration.
//!
//! Rust ownership and `Stream` replace native COM/dequeue async operations.
//! Epoch markers expose an explicit provider handoff: only matching completion
//! unblocks every reader. Dropping a reader or marker does not complete it.
//! The queue has no additional bound; retention/transport admission is separate.

use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures::Stream;
use tokio::sync::Notify;

use crate::application::Lsn;
use crate::protocol::types::Epoch;

use super::received_operation::ReceivedOperation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EpochBarrier {
    pub(super) epoch: Epoch,
    pub(super) previous_epoch_last_lsn: Lsn,
}

pub(super) enum DispatchEntry {
    Operation(ReceivedOperation),
    UpdateEpoch(EpochBarrier),
}

pub(super) enum StreamEvent {
    Operation(ReceivedOperation),
    UpdateEpoch(EpochBarrier),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum StreamError {
    #[error("epoch completion does not match the blocked stream marker")]
    InvalidEpochCompletion,
    #[error("operation dispatch queue is closed")]
    Closed,
    #[error("operation stream fault: {0}")]
    Fault(String),
}

/// Must be dispatched after releasing the caller's serialization guard.
/// Aborted entries are also released here, outside queue/owner guards.
#[must_use = "dispatch staged stream wakeups outside the caller's guard"]
pub(super) struct DispatchWake {
    wakers: Vec<Waker>,
    retired: VecDeque<DispatchEntry>,
    changed: Option<Arc<Notify>>,
}

impl DispatchWake {
    pub(super) fn dispatch(self) {
        drop(self.retired);
        for waker in self.wakers {
            waker.wake();
        }
        if let Some(changed) = self.changed {
            changed.notify_one();
        }
    }
}

enum QueueStatus {
    Open,
    Closing,
    Closed,
    Aborted(StreamError),
}

struct DispatchState {
    entries: VecDeque<DispatchEntry>,
    status: QueueStatus,
    blocked_epoch: Option<EpochBarrier>,
    next_reader: u64,
    waiters: BTreeMap<u64, Waker>,
    initialized: bool,
}

impl DispatchState {
    fn take_wake(&mut self) -> DispatchWake {
        DispatchWake {
            wakers: std::mem::take(&mut self.waiters).into_values().collect(),
            retired: VecDeque::new(),
            changed: None,
        }
    }

    fn finish_close(&mut self) {
        if matches!(self.status, QueueStatus::Closing)
            && self.entries.is_empty()
            && self.blocked_epoch.is_none()
        {
            self.status = QueueStatus::Closed;
        }
    }
}

/// Every handle consumes the same queue; reacquiring a stream does not replay.
/// An abort is sticky: each nonterminated/new reader sees its error once, then
/// EOF. Graceful EOF is permanent, including for subsequently acquired readers;
/// abort can override a draining close, but cannot reopen a drained queue.
#[derive(Clone)]
pub(super) struct SharedDispatchQueue {
    state: Arc<Mutex<DispatchState>>,
    changed: Arc<Notify>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DispatchSnapshot {
    pub(super) initialized: bool,
    pub(super) drained: bool,
    pub(super) aborted: Option<StreamError>,
}

impl SharedDispatchQueue {
    pub(super) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(DispatchState {
                entries: VecDeque::new(),
                status: QueueStatus::Open,
                blocked_epoch: None,
                next_reader: 0,
                waiters: BTreeMap::new(),
                initialized: false,
            })),
            changed: Arc::new(Notify::new()),
        }
    }

    pub(super) fn stream(&self) -> OperationStream {
        let mut state = self.state.lock().unwrap();
        let reader = state.next_reader;
        state.next_reader = reader.checked_add(1).expect("stream reader IDs exhausted");
        state.initialized = true;
        OperationStream {
            state: self.state.clone(),
            reader,
            terminated: false,
            changed: self.changed.clone(),
        }
    }

    pub(super) fn notifier(&self) -> Arc<Notify> {
        self.changed.clone()
    }

    pub(super) fn snapshot(&self) -> DispatchSnapshot {
        let state = self.state.lock().unwrap();
        DispatchSnapshot {
            initialized: state.initialized,
            drained: state.entries.is_empty() && state.blocked_epoch.is_none(),
            aborted: match &state.status {
                QueueStatus::Aborted(error) => Some(error.clone()),
                _ => None,
            },
        }
    }

    /// Keep an already-selected epoch handoff; retire only queued entries.
    pub(super) fn clear_pending(&self) -> DispatchWake {
        let mut state = self.state.lock().unwrap();
        let retired = std::mem::take(&mut state.entries);
        state.finish_close();
        let mut wake = state.take_wake();
        wake.retired = retired;
        wake.changed = Some(self.changed.clone());
        wake
    }

    pub(super) fn append(&self, entries: Vec<DispatchEntry>) -> Result<DispatchWake, StreamError> {
        let mut state = self.state.lock().unwrap();
        match &state.status {
            QueueStatus::Open => {}
            QueueStatus::Closing | QueueStatus::Closed => return Err(StreamError::Closed),
            QueueStatus::Aborted(error) => return Err(error.clone()),
        }
        let has_entries = !entries.is_empty();
        state.entries.extend(entries);
        let mut wake = if has_entries {
            state.take_wake()
        } else {
            DispatchWake {
                wakers: Vec::new(),
                retired: VecDeque::new(),
                changed: None,
            }
        };
        wake.changed = Some(self.changed.clone());
        Ok(wake)
    }

    /// Close admission, drain data and explicit epoch handoffs, then return EOF.
    pub(super) fn close(&self) -> DispatchWake {
        let mut state = self.state.lock().unwrap();
        if matches!(state.status, QueueStatus::Open) {
            state.status = QueueStatus::Closing;
            state.finish_close();
        }
        let mut wake = state.take_wake();
        wake.changed = Some(self.changed.clone());
        wake
    }

    /// First abort wins. Discard queued data and any outstanding epoch handoff.
    pub(super) fn abort(&self, error: StreamError) -> DispatchWake {
        let mut state = self.state.lock().unwrap();
        let mut retired = VecDeque::new();
        if matches!(state.status, QueueStatus::Open | QueueStatus::Closing) {
            retired = std::mem::take(&mut state.entries);
            state.blocked_epoch = None;
            state.status = QueueStatus::Aborted(error);
        }
        let mut wake = state.take_wake();
        wake.retired = retired;
        wake.changed = Some(self.changed.clone());
        wake
    }

    pub(super) fn blocked_epoch(&self) -> Option<EpochBarrier> {
        self.state.lock().unwrap().blocked_epoch
    }

    /// A mismatched or absent handoff leaves data, readers and the barrier intact.
    pub(super) fn finish_epoch(&self, barrier: EpochBarrier) -> Result<DispatchWake, StreamError> {
        let mut state = self.state.lock().unwrap();
        if state.blocked_epoch != Some(barrier) {
            return Err(StreamError::InvalidEpochCompletion);
        }
        state.blocked_epoch = None;
        state.finish_close();
        let mut wake = state.take_wake();
        wake.changed = Some(self.changed.clone());
        Ok(wake)
    }
}

pub(super) struct OperationStream {
    state: Arc<Mutex<DispatchState>>,
    reader: u64,
    terminated: bool,
    changed: Arc<Notify>,
}

impl Stream for OperationStream {
    type Item = Result<StreamEvent, StreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.terminated {
            return Poll::Ready(None);
        }

        let mut state = this.state.lock().unwrap();
        // Replace stale registrations, including a cancelled next() future.
        state.waiters.remove(&this.reader);
        if let QueueStatus::Aborted(error) = &state.status {
            this.terminated = true;
            return Poll::Ready(Some(Err(error.clone())));
        }
        if state.blocked_epoch.is_none()
            && let Some(entry) = state.entries.pop_front()
        {
            let event = match entry {
                DispatchEntry::Operation(operation) => StreamEvent::Operation(operation),
                DispatchEntry::UpdateEpoch(barrier) => {
                    state.blocked_epoch = Some(barrier);
                    StreamEvent::UpdateEpoch(barrier)
                }
            };
            state.finish_close();
            drop(state);
            this.changed.notify_one();
            return Poll::Ready(Some(Ok(event)));
        }
        if matches!(state.status, QueueStatus::Closed) {
            this.terminated = true;
            return Poll::Ready(None);
        }

        state.waiters.insert(this.reader, cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for OperationStream {
    fn drop(&mut self) {
        // Every transition broadcasts to all waiters, so cancellation never
        // needs to wake/transfer a single reserved-reader notification.
        self.state.lock().unwrap().waiters.remove(&self.reader);
    }
}

#[cfg(test)]
mod tests;
