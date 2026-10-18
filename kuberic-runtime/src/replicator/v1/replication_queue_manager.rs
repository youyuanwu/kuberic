// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided Rust translation of selected ReplicationQueueManager.h/.cpp:
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d
// src/prod/src/Reliability/Replication/. See sf-v1-port-provenance.md.

//! Synchronous, production-unselected primary retention and assignment owner.
//! Its caller serializes access and completes replicate requests separately.
//! Queue-only transfer normalization is not a functioning role transition.
//! Secondary demotion trimming and native V2 build selection are not provided.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::application::{Lsn, OperationData};
use crate::protocol::types::Epoch;

use super::buffers::{BufferError, BufferLimits, BufferPolicy};
use super::operation_queue::{EnqueueError, OperationQueue, QueueConfig, QueueOperation};
use super::sender::SenderOperation;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(super) enum QueueAdmissionError {
    #[error(transparent)]
    Buffer(#[from] BufferError),
    #[error("primary queue rejected admission: {0:?}")]
    Queue(EnqueueError),
    #[error("next sequence number cannot advance without overflow")]
    SequenceNumberOverflow,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub(super) enum QueueProgressError {
    #[error("progress {value} is outside the queue range ending at {latest}")]
    OutOfRange { value: Lsn, latest: Lsn },
    #[error("commit target crosses missing operation {lsn}")]
    MissingOperation { lsn: Lsn },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct QueueUpdate {
    pub(super) old_committed: Lsn,
    pub(super) new_committed: Lsn,
    pub(super) removed_items: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CatchupProgress {
    pub(super) committed: Lsn,
    pub(super) completed: Lsn,
    pub(super) latest: Lsn,
    pub(super) previous_last: Lsn,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected queue-only normalization errors")
)]
pub(super) enum QueueNormalizationError {
    #[error("initial progress must be nonnegative and leave room for a queue cursor")]
    InvalidInitialProgress,
    #[error("queue reset requires no pending operations")]
    PendingOperations,
    #[error("transferred secondary commit and completion cursors must agree")]
    InvalidSourceProgress,
    #[error("transferred queue exceeds the primary operation bound")]
    CapacityExceeded,
    #[error("transferred operation {lsn} has no original epoch")]
    MissingOriginalEpoch { lsn: Lsn },
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 primary queue owner")
)]
pub(super) struct ReplicationQueueManager {
    queue: OperationQueue,
    next_sequence_number: Lsn,
    original_epochs: BTreeMap<Lsn, Epoch>,
    previous_config_catchup_lsn: Lsn,
    previous_config_quorum_lsn: Lsn,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 primary queue API")
)]
impl ReplicationQueueManager {
    pub(super) fn new(config: QueueConfig, initial_progress: Lsn) -> Self {
        let start = Self::initial_cursor(initial_progress)
            .expect("primary initial progress must leave room for a queue cursor");
        Self {
            // No provider-commit requirement is invented by the primary owner.
            queue: OperationQueue::new(Self::primary_config(config), start, false, false),
            next_sequence_number: start,
            original_epochs: BTreeMap::new(),
            // Native Constants::InvalidLSN is zero, not the public API's -1.
            previous_config_catchup_lsn: 0,
            previous_config_quorum_lsn: 0,
        }
    }

    /// Native cpp 59–91, excluding role/provider/transport activation. The
    /// supplied epochs belong to retained operations, not the promotion epoch.
    /// ACK predicates are inherited from the source queue, as in native.
    pub(super) fn from_transferred_queue(
        config: QueueConfig,
        queue: OperationQueue,
        original_epochs: BTreeMap<Lsn, Epoch>,
    ) -> Result<Self, QueueNormalizationError> {
        let progress = queue.progress();
        if progress.commit_head != progress.complete_head {
            return Err(QueueNormalizationError::InvalidSourceProgress);
        }
        if config.max_capacity != 0 && queue.operation_count() > config.max_capacity {
            return Err(QueueNormalizationError::CapacityExceeded);
        }
        for lsn in progress.retained_head..progress.tail {
            if queue.get_operation(lsn).is_some() && !original_epochs.contains_key(&lsn) {
                return Err(QueueNormalizationError::MissingOriginalEpoch { lsn });
            }
        }

        // Native captures the issued tail BEFORE discarding unfinished work.
        let next_sequence_number = progress.tail;
        // Unlike native's ring, OperationQueue checks cleanup invariants at
        // discard. Defer only the cleanup flag until history is reclassified.
        let primary_config = Self::primary_config(config);
        let mut queue = queue.transfer(QueueConfig {
            clean_on_complete: false,
            ..primary_config
        });
        queue.set_ignore_commit(false);
        queue.discard_noncompleted();
        queue.reset_completed();
        queue.set_clean_on_complete(true);
        let previous_config_catchup_lsn = queue.progress().tail - 1;
        let previous_config_quorum_lsn = queue.progress().commit_head - 1;
        queue.set_commit_callback(None);
        let mut manager = Self {
            queue,
            next_sequence_number,
            original_epochs,
            previous_config_catchup_lsn,
            previous_config_quorum_lsn,
        };
        manager.prune_epochs();
        Ok(manager)
    }

    /// Native cpp 148–160 requires the caller to finish pending work first.
    pub(super) fn reset_queue(
        &mut self,
        initial_progress: Lsn,
    ) -> Result<(), QueueNormalizationError> {
        let start = Self::initial_cursor(initial_progress)?;
        if self.queue.progress().has_pending() {
            return Err(QueueNormalizationError::PendingOperations);
        }
        self.queue.reset(start);
        self.original_epochs.clear();
        self.previous_config_catchup_lsn = self.last_sequence_number();
        self.previous_config_quorum_lsn = self.current_progress();
        self.next_sequence_number = start;
        Ok(())
    }

    fn initial_cursor(initial_progress: Lsn) -> Result<Lsn, QueueNormalizationError> {
        if initial_progress < 0 {
            return Err(QueueNormalizationError::InvalidInitialProgress);
        }
        initial_progress
            .checked_add(1)
            .ok_or(QueueNormalizationError::InvalidInitialProgress)
    }

    fn primary_config(config: QueueConfig) -> QueueConfig {
        QueueConfig {
            clean_on_complete: true,
            max_completed_count: 0,
            max_completed_bytes: 0,
            ..config
        }
    }

    pub(super) fn enqueue(
        &mut self,
        data: OperationData,
        epoch: Epoch,
        policy: BufferPolicy,
        limits: BufferLimits,
    ) -> Result<SenderOperation, QueueAdmissionError> {
        // OperationQueue stores the exclusive tail; check before admission.
        let next = self
            .next_sequence_number
            .checked_add(1)
            .ok_or(QueueAdmissionError::SequenceNumberOverflow)?;
        let operation =
            QueueOperation::from_data(self.next_sequence_number, data, false, policy, limits)?;
        self.queue
            .try_enqueue(operation.clone())
            .map_err(QueueAdmissionError::Queue)?;
        self.original_epochs
            .insert(self.next_sequence_number, epoch);
        self.next_sequence_number = next;
        Ok(SenderOperation {
            operation,
            original_epoch: epoch,
        })
    }

    /// None is native NonInitializedLSN: no remote constraint, not no progress.
    pub(super) fn update_queue(
        &mut self,
        committed: Option<Lsn>,
        completed: Option<Lsn>,
    ) -> Result<QueueUpdate, QueueProgressError> {
        self.validate_progress(committed, completed)?;
        if let Some(last) = committed {
            for lsn in self.queue.progress().commit_head..=last {
                if self.queue.get_operation(lsn).is_none() {
                    return Err(QueueProgressError::MissingOperation { lsn });
                }
            }
        }
        let old_committed = self.current_progress();
        let initial_count = self.operation_count();
        if let Some(committed) = committed {
            self.queue.update_commit_head(committed);
        } else {
            self.queue.commit();
        }
        let completed = completed
            .unwrap_or(self.current_progress())
            .min(self.current_progress());
        self.queue.update_last_completed(completed);
        self.prune_epochs();
        Ok(QueueUpdate {
            old_committed,
            new_committed: self.current_progress(),
            removed_items: self.operation_count() < initial_count,
        })
    }

    /// Native cpp 323–357 is a projection, even when commitment would rewind.
    pub(super) fn catchup_progress(
        &self,
        committed: Option<Lsn>,
        completed: Option<Lsn>,
    ) -> Result<CatchupProgress, QueueProgressError> {
        self.validate_progress(committed, completed)?;
        let committed = committed.unwrap_or(self.last_sequence_number());
        Ok(CatchupProgress {
            committed,
            completed: completed.unwrap_or(committed).min(committed),
            latest: self.last_sequence_number(),
            previous_last: self.previous_config_catchup_lsn,
        })
    }

    fn validate_progress(
        &self,
        committed: Option<Lsn>,
        completed: Option<Lsn>,
    ) -> Result<(), QueueProgressError> {
        let latest = self.last_sequence_number();
        if let Some(value) = committed
            && (value < 0 || value > latest)
        {
            return Err(QueueProgressError::OutOfRange { value, latest });
        }
        // Native caps all-replica completion, including values past tail.
        if let Some(value) = completed
            && value < 0
        {
            return Err(QueueProgressError::OutOfRange { value, latest });
        }
        Ok(())
    }

    pub(super) fn current_progress(&self) -> Lsn {
        self.queue.progress().commit_head - 1
    }

    pub(super) fn last_sequence_number(&self) -> Lsn {
        self.queue.progress().tail - 1
    }

    pub(super) fn catchup_capability(&self) -> Lsn {
        self.queue.progress().first_committed()
    }

    pub(super) fn first_lsn_in_replication_queue(&self) -> Lsn {
        self.queue.progress().complete_head
    }

    pub(super) fn previous_config_catchup_lsn(&self) -> Lsn {
        self.previous_config_catchup_lsn
    }

    pub(super) fn previous_config_quorum_lsn(&self) -> Lsn {
        self.previous_config_quorum_lsn
    }

    pub(super) fn operation_count(&self) -> usize {
        self.queue.operation_count()
    }

    pub(super) fn total_bytes(&self) -> usize {
        self.queue.total_bytes()
    }

    pub(super) fn get_operations(&self, first: Lsn, out: &mut Vec<SenderOperation>) -> bool {
        if first < self.queue.progress().retained_head {
            return false;
        }
        let mut operations = Vec::new();
        let found = self.queue.get_operations(first, &mut operations);
        out.extend(operations.into_iter().map(|operation| SenderOperation {
            original_epoch: self.original_epochs[&operation.lsn()],
            operation,
        }));
        found
    }

    /// Call only when capturing a configuration boundary, not on each write.
    pub(super) fn update_catchup_completion_lsn(&mut self) {
        self.previous_config_catchup_lsn = self
            .previous_config_catchup_lsn
            .max(self.last_sequence_number());
        self.previous_config_quorum_lsn =
            self.previous_config_quorum_lsn.max(self.current_progress());
    }

    pub(super) fn check_with_catchup_lsn(&self, value: Lsn) -> Lsn {
        value.min(self.previous_config_catchup_lsn)
    }

    fn prune_epochs(&mut self) {
        let progress = self.queue.progress();
        self.original_epochs
            .retain(|lsn, _| *lsn >= progress.retained_head && *lsn < progress.tail);
    }
}

#[cfg(test)]
mod tests;
