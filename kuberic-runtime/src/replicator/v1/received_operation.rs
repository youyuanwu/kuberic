// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided Rust translation of ComFromBytesOperation.cpp:127-257 and
// ComFromBytesOperation.h/ComOperation.h, microsoft/service-fabric
// @3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d, Reliability/Replication.
// Numbered network EOS: ReplicationTransport.cpp:722-752,776-802 and
// ComFromBytesOperation.cpp:11-24; local zero-LSN EOS remains distinct.
// See sf-v1-port-provenance.md for the private V1 port boundary.

//! Received-operation identity, ACK callback state and retirement ownership.
//! Selecting work is not starting its callback: ReadyToRun still blocks queue
//! completion. The receiver calls `start` under its serialization, performs the
//! real callback work, then calls `finish`. Dropping work does none of these.
//! COM plumbing and provider/receiver callback execution are not translated.

use std::fmt;
use std::sync::{Arc, Mutex};

use crate::application::{Lsn, OperationData};
use crate::protocol::types::Epoch;

use super::operation_queue::QueueOperation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AckState {
    NotCalled,
    ReadyToRun,
    Running,
    Completed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum AckError {
    #[error("the operation ACK callback is not in a valid state for this transition")]
    InvalidOperation,
    #[error("copy EOS sequence number must be positive and leave room for a queue cursor")]
    InvalidCopySequenceNumber,
}

struct CallbackState {
    ack: AckState,
    owner: Option<Arc<dyn Send + Sync>>,
}

struct Inner {
    queue_operation: Arc<QueueOperation>,
    epoch: Epoch,
    end_of_stream: bool,
    callback_required: bool,
    callback: Mutex<CallbackState>,
}

/// Clones refer to one callback instance, not merely the same sequence number.
#[derive(Clone)]
pub(super) struct ReceivedOperation {
    inner: Arc<Inner>,
}

impl fmt::Debug for ReceivedOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceivedOperation")
            .field("lsn", &self.queue_operation().lsn())
            .field("epoch", &self.epoch())
            .field("end_of_stream", &self.is_end_of_stream())
            .field("ack_state", &self.ack_state())
            .finish_non_exhaustive()
    }
}

impl ReceivedOperation {
    /// The caller supplies a fresh queue entry with its ACK predicate set.
    pub(super) fn new(
        queue_operation: Arc<QueueOperation>,
        epoch: Epoch,
        callback_required: bool,
    ) -> Self {
        assert!(
            queue_operation.needs_ack(),
            "a received operation requires a fresh ACK-blocking queue entry"
        );
        Self {
            inner: Arc::new(Inner {
                queue_operation,
                epoch,
                end_of_stream: false,
                callback_required,
                callback: Mutex::new(CallbackState {
                    ack: AckState::NotCalled,
                    owner: None,
                }),
            }),
        }
    }

    pub(super) fn end_of_stream(callback_required: bool) -> Self {
        // Native Constants::InvalidLSN and InvalidEpoch are zero, not the
        // public API's noninitialized -1. Locally generated EOS is not queued.
        Self::end_of_stream_at(0, callback_required)
    }

    pub(super) fn copy_end_of_stream(lsn: Lsn, callback_required: bool) -> Result<Self, AckError> {
        if lsn <= 0 || lsn == Lsn::MAX {
            return Err(AckError::InvalidCopySequenceNumber);
        }
        Ok(Self::end_of_stream_at(lsn, callback_required))
    }

    fn end_of_stream_at(lsn: Lsn, callback_required: bool) -> Self {
        let mut operation = Self::new(
            QueueOperation::new(lsn, OperationData::new(), true),
            Epoch::new(0, 0),
            callback_required,
        );
        Arc::get_mut(&mut operation.inner).unwrap().end_of_stream = true;
        operation
    }

    pub(super) fn queue_operation(&self) -> &Arc<QueueOperation> {
        &self.inner.queue_operation
    }

    pub(super) fn epoch(&self) -> Epoch {
        self.inner.epoch
    }

    pub(super) fn callback_required(&self) -> bool {
        self.inner.callback_required
    }

    pub(super) fn is_end_of_stream(&self) -> bool {
        self.inner.end_of_stream
    }

    pub(super) fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(super) fn ack_state(&self) -> AckState {
        self.inner.callback.lock().unwrap().ack
    }

    pub(super) fn acknowledge(&self) -> Result<Option<AckWork>, AckError> {
        let released_owner = {
            let mut callback = self.inner.callback.lock().unwrap();
            match callback.ack {
                AckState::NotCalled if self.inner.callback_required => {
                    callback.ack = AckState::ReadyToRun;
                    return Ok(Some(AckWork {
                        operation: self.clone(),
                    }));
                }
                AckState::NotCalled => {
                    callback.ack = AckState::Completed;
                    self.queue_operation().release_ack_requirement();
                    callback.owner.take()
                }
                AckState::Cancelled => return Ok(None),
                AckState::ReadyToRun | AckState::Running | AckState::Completed => {
                    return Err(AckError::InvalidOperation);
                }
            }
        };
        // Owner destruction may reenter this operation; never run it locked.
        drop(released_owner);
        Ok(None)
    }

    pub(super) fn ignore_ack_and_keep_owner(&self, owner: Arc<dyn Send + Sync>) {
        let released_owner = {
            let mut callback = self.inner.callback.lock().unwrap();
            match callback.ack {
                AckState::NotCalled if !self.is_end_of_stream() => {
                    callback.ack = AckState::Cancelled;
                    self.queue_operation().release_ack_requirement();
                    None
                }
                AckState::NotCalled | AckState::ReadyToRun | AckState::Running => {
                    callback.owner.replace(owner)
                }
                AckState::Completed | AckState::Cancelled => None,
            }
        };
        drop(released_owner);
    }
}

/// The operation itself roots selected work. Retirement may additionally retain
/// its receiver owner in the operation until the real callback finishes.
/// Abandonment leaves the callback pending and any retained owner untouched.
#[derive(Debug)]
#[must_use = "selected ACK work must be explicitly started and finished by its receiver"]
pub(super) struct AckWork {
    operation: ReceivedOperation,
}

impl AckWork {
    pub(super) fn operation(&self) -> &ReceivedOperation {
        &self.operation
    }

    /// Call only after entering the receiver's callback serialization.
    pub(super) fn start(&mut self) -> Result<(), AckError> {
        let mut callback = self.operation.inner.callback.lock().unwrap();
        if callback.ack != AckState::ReadyToRun {
            return Err(AckError::InvalidOperation);
        }
        callback.ack = AckState::Running;
        self.operation.queue_operation().release_ack_requirement();
        Ok(())
    }

    pub(super) fn finish(self) -> Result<(), AckError> {
        let released_owner = {
            let mut callback = self.operation.inner.callback.lock().unwrap();
            if callback.ack != AckState::Running {
                return Err(AckError::InvalidOperation);
            }
            callback.ack = AckState::Completed;
            callback.owner.take()
        };
        drop(released_owner);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
