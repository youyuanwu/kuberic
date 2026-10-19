// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Selected SecondaryReplicationReceiver.h/.cpp and SecondaryReplicator.cpp
// algorithms, microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d.
// src/prod/src/Reliability/Replication/. See sf-v1-port-provenance.md.

//! Private receiver core, not a SecondaryReplicator or a role/provider adapter.
//! The caller serializes short transitions and dispatches returned work outside
//! its guard. StartCopy initialization is supplied, not an executed copy/build.
//! Provider epoch requests remain explicit obligations; dropping one cannot
//! unblock dispatch. Retirement is not native graceful Close or promotion.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thiserror::Error;

use crate::application::Lsn;
use crate::protocol::types::Epoch;

use super::ack::{AckDispatch, AckSender};
use super::operation_queue::{CallbackControl, EnqueueError, OperationQueue, QueueConfig};
use super::operation_stream::{
    DispatchEntry, DispatchWake, EpochBarrier, OperationStream, SharedDispatchQueue, StreamError,
};
use super::received_operation::{AckError, AckState, AckWork, ReceivedOperation};
use super::replication_queue_manager::QueueTransfer;

#[derive(Clone, Copy, Debug)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 receiver settings")
)]
pub(super) struct ReceiverConfig {
    pub(super) queue: QueueConfig,
    pub(super) has_persisted_state: bool,
    pub(super) require_service_ack: bool,
    pub(super) enable_eos_ack: bool,
}

#[derive(Debug, Error, PartialEq, Eq)]
#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 receiver errors"))]
pub(super) enum ReceiverError {
    #[error("receiver has not received StartCopy initialization")]
    NotInitialized,
    #[error("receiver already has a replication queue")]
    AlreadyInitialized,
    #[error("receiver is retired")]
    Closed,
    #[error("replication start must be positive and leave room for a queue cursor")]
    InvalidStart,
    #[error("epoch components must be nonnegative")]
    InvalidEpoch,
    #[error("sending epoch {sending:?} precedes the minimum {minimum:?}")]
    StaleSendingEpoch { sending: Epoch, minimum: Epoch },
    #[error("replication batch is empty")]
    EmptyBatch,
    #[error("operation {0} is not fresh replication data with the receiver's ACK mode")]
    InvalidOperation(Lsn),
    #[error("completed hint must be nonnegative and leave room for a queue cursor")]
    InvalidCompletedHint,
    #[error("ACK does not belong to an operation admitted by this receiver")]
    ForeignAck,
    #[error("EOS ACK mode is disabled")]
    EosDisabled,
    #[error("provider epoch completion does not match an outstanding handoff")]
    InvalidEpochCompletion,
    #[error("state-provider epoch update failed: {0}")]
    ProviderEpoch(String),
    #[error("copy pending-ACK count and pending threshold must be representable")]
    InvalidAckThreshold,
    #[error(transparent)]
    Ack(#[from] AckError),
    #[error(transparent)]
    Stream(#[from] StreamError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 ACK snapshot"))]
pub(super) struct ReceiverAck {
    pub(super) received: Lsn,
    pub(super) applied: Lsn,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 batch observation")
)]
pub(super) struct BatchObservation {
    pub(super) admitted: usize,
    pub(super) duplicates: usize,
    pub(super) queue_full: bool,
    pub(super) stopped_on_gap: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct AckRequest {
    pub(super) check_pending_limit: bool,
}

/// These are owner obligations, not provider/transport successes.
#[must_use = "deliver ACK scheduling and provider epoch handoffs to their actual owners"]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 receiver handoffs")
)]
pub(super) struct ReceiverSignals {
    pub(super) ack: Option<AckRequest>,
    pub(super) update_epoch: Option<EpochBarrier>,
    pub(super) batch: BatchObservation,
}

#[must_use = "dispatch wakeups and finish rooted ACK callbacks outside the receiver guard"]
#[derive(Default)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 receiver effects")
)]
pub(super) struct ReceiverEffects {
    wakes: Vec<DispatchWake>,
    ack_work: Option<AckWork>,
    ack: Option<AckRequest>,
    update_epoch: Option<EpochBarrier>,
    batch: BatchObservation,
    fault: Option<String>,
}

#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 effect delivery"))]
impl ReceiverEffects {
    pub(super) fn epoch_request(&self) -> Option<EpochBarrier> {
        self.update_epoch
    }

    pub(super) fn dispatch(self) -> Result<ReceiverSignals, ReceiverError> {
        for wake in self.wakes {
            wake.dispatch();
        }
        if let Some(work) = self.ack_work {
            work.finish()?;
        }
        if let Some(fault) = self.fault {
            return Err(ReceiverError::ProviderEpoch(fault));
        }
        Ok(ReceiverSignals {
            ack: self.ack,
            update_epoch: self.update_epoch,
            batch: self.batch,
        })
    }
}

struct DispatchState {
    operations: BTreeMap<Lsn, ReceivedOperation>,
    provider_epoch: Option<Epoch>,
    full_barrier: Option<EpochBarrier>,
    provider_pending: Option<EpochBarrier>,
    staged: Vec<DispatchEntry>,
    require_ack: bool,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 secondary receiver")
)]
pub(super) struct SecondaryReceiver {
    config: ReceiverConfig,
    require_ack: bool,
    queue: Option<OperationQueue>,
    callback: Arc<Mutex<DispatchState>>,
    original_epochs: BTreeMap<Lsn, Epoch>,
    dispatch: SharedDispatchQueue,
    minimum_sending_epoch: Option<Epoch>,
    copy_epoch: Option<Epoch>,
    last_completed_ack: Lsn,
    retired: bool,
    cleanup_callbacks: Vec<ReceivedOperation>,
    eos: Option<ReceivedOperation>,
    eos_acked: bool,
}

#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 receiver API"))]
impl SecondaryReceiver {
    pub(super) fn new(config: ReceiverConfig) -> Self {
        let require_ack = config.has_persisted_state || config.require_service_ack;
        Self {
            config,
            require_ack,
            queue: None,
            callback: Arc::new(Mutex::new(DispatchState {
                operations: BTreeMap::new(),
                provider_epoch: None,
                full_barrier: None,
                provider_pending: None,
                staged: Vec::new(),
                require_ack,
            })),
            original_epochs: BTreeMap::new(),
            dispatch: SharedDispatchQueue::new(),
            minimum_sending_epoch: None,
            copy_epoch: None,
            last_completed_ack: 0,
            retired: false,
            cleanup_callbacks: Vec::new(),
            eos: None,
            eos_acked: !require_ack,
        }
    }

    pub(super) fn requires_service_ack(&self) -> bool {
        self.require_ack
    }

    pub(super) fn from_primary(
        config: ReceiverConfig,
        epoch: Epoch,
        transfer: QueueTransfer,
    ) -> Result<Self, ReceiverError> {
        Self::validate_epoch(epoch)?;
        let mut receiver = Self::new(config);
        let mut queue = transfer.queue.transfer(config.queue);
        queue.set_commit_callback(None);
        queue.complete();
        // Fresh Rust primary entries never need a service callback. Received
        // secondary entries must enforce the actual service's ACK mode.
        queue.set_require_service_ack(receiver.require_ack);
        queue.set_ignore_commit(!receiver.require_ack);
        receiver.queue = Some(queue);
        receiver.original_epochs = transfer.original_epochs;
        receiver.copy_epoch = Some(epoch);
        receiver.minimum_sending_epoch = Some(epoch);
        receiver.callback.lock().unwrap().provider_epoch = Some(epoch);
        receiver.enable_callback();
        receiver.prune_operations();
        Ok(receiver)
    }

    pub(super) fn take_for_primary(&mut self) -> Result<QueueTransfer, ReceiverError> {
        let queue = self.queue.as_ref().ok_or(ReceiverError::NotInitialized)?;
        if queue.progress().commit_head != queue.progress().complete_head {
            return Err(ReceiverError::InvalidEpochCompletion);
        }
        self.retired = true;
        let mut queue = self.queue.take().unwrap();
        queue.set_commit_callback(None);
        Ok(QueueTransfer {
            queue,
            original_epochs: std::mem::take(&mut self.original_epochs),
        })
    }

    pub(super) fn advance_external_epoch(&mut self, epoch: Epoch) -> Result<bool, ReceiverError> {
        self.ensure_active()?;
        Self::validate_epoch(epoch)?;
        let minimum = self.minimum_sending_epoch.unwrap_or(Epoch::new(0, 0));
        if epoch < minimum {
            return Err(ReceiverError::StaleSendingEpoch {
                sending: epoch,
                minimum,
            });
        }
        if epoch == minimum {
            return Ok(false);
        }
        self.discard_out_of_order();
        self.minimum_sending_epoch = Some(epoch);
        Ok(true)
    }

    pub(super) fn has_persisted_state(&self) -> bool {
        self.config.has_persisted_state
    }

    pub(super) fn dispatch_queue(&self) -> SharedDispatchQueue {
        self.dispatch.clone()
    }

    fn ensure_active(&self) -> Result<(), ReceiverError> {
        if self.retired {
            Err(ReceiverError::Closed)
        } else {
            Ok(())
        }
    }

    fn validate_epoch(epoch: Epoch) -> Result<(), ReceiverError> {
        if epoch.data_loss_number < 0 || epoch.configuration_number < 0 {
            Err(ReceiverError::InvalidEpoch)
        } else {
            Ok(())
        }
    }

    pub(super) fn initialize(
        &mut self,
        replication_start: Lsn,
        copy_epoch: Epoch,
    ) -> Result<(), ReceiverError> {
        self.ensure_active()?;
        if self.queue.is_some() {
            return Err(ReceiverError::AlreadyInitialized);
        }
        if replication_start <= 0 || replication_start == Lsn::MAX {
            return Err(ReceiverError::InvalidStart);
        }
        Self::validate_epoch(copy_epoch)?;
        let mut config = self.config.queue;
        if config.clean_on_complete {
            config.max_completed_count = 0;
            config.max_completed_bytes = 0;
        }
        self.queue = Some(OperationQueue::new(
            config,
            replication_start,
            self.require_ack,
            !self.require_ack,
        ));
        self.copy_epoch = Some(copy_epoch);
        self.minimum_sending_epoch = Some(copy_epoch);
        self.enable_callback();
        Ok(())
    }

    fn enable_callback(&mut self) {
        let callback = self.callback.clone();
        self.queue
            .as_mut()
            .expect("initialized receiver")
            .set_commit_callback(Some(Box::new(move |entry, progress| {
                let mut state = callback.lock().unwrap();
                let operation = state.operations[&entry.lsn()].clone();
                if operation.epoch() > state.provider_epoch.expect("established original epoch") {
                    let barrier = EpochBarrier {
                        epoch: operation.epoch(),
                        previous_epoch_last_lsn: entry.lsn() - 1,
                    };
                    state.provider_epoch = Some(barrier.epoch);
                    if state.require_ack && progress.complete_head != entry.lsn() {
                        assert!(state.full_barrier.is_none());
                        assert!(state.provider_pending.is_none());
                        state.full_barrier = Some(barrier);
                        return CallbackControl::Disable;
                    }
                    state.staged.push(DispatchEntry::UpdateEpoch(barrier));
                }
                state.staged.push(DispatchEntry::Operation(operation));
                CallbackControl::Keep
            })));
    }

    fn drain_dispatch(&mut self, effects: &mut ReceiverEffects) -> Result<(), ReceiverError> {
        let staged = std::mem::take(&mut self.callback.lock().unwrap().staged);
        if !staged.is_empty() {
            effects.wakes.push(self.dispatch.append(staged)?);
        }
        self.prune_operations();
        Ok(())
    }

    fn prune_operations(&mut self) {
        let Some(queue) = &self.queue else {
            return;
        };
        self.callback
            .lock()
            .unwrap()
            .operations
            .retain(|lsn, _| queue.get_operation(*lsn).is_some());
        self.original_epochs
            .retain(|lsn, _| queue.get_operation(*lsn).is_some());
    }

    pub(super) fn stream(&self) -> OperationStream {
        self.dispatch.stream()
    }

    pub(super) fn queue(&self) -> Option<&OperationQueue> {
        self.queue.as_ref()
    }

    pub(super) fn current_progress(&self) -> Result<Lsn, ReceiverError> {
        Ok(self
            .queue
            .as_ref()
            .ok_or(ReceiverError::NotInitialized)?
            .progress()
            .commit_head
            - 1)
    }

    pub(super) fn catchup_capability(&self) -> Result<Lsn, ReceiverError> {
        Ok(self
            .queue
            .as_ref()
            .ok_or(ReceiverError::NotInitialized)?
            .progress()
            .first_available_completed())
    }

    pub(super) fn provider_epoch(&self) -> Option<Epoch> {
        self.callback.lock().unwrap().provider_epoch
    }

    pub(super) fn has_pending_epoch(&self, barrier: EpochBarrier) -> bool {
        self.callback.lock().unwrap().provider_pending == Some(barrier)
    }

    pub(super) fn fail_pending_epoch(
        &mut self,
        barrier: EpochBarrier,
        owner: Arc<dyn Send + Sync>,
    ) -> Result<(), ReceiverError> {
        if !self.has_pending_epoch(barrier) {
            return Err(ReceiverError::InvalidEpochCompletion);
        }
        self.clean_queue(owner, true, true);
        self.callback.lock().unwrap().provider_pending = None;
        Ok(())
    }

    pub(super) fn minimum_sending_epoch(&self) -> Option<Epoch> {
        self.minimum_sending_epoch
    }

    pub(super) fn copy_epoch(&self) -> Option<Epoch> {
        self.copy_epoch
    }

    pub(super) fn capture_ack(&mut self) -> ReceiverAck {
        let ack = self.queue.as_ref().map_or(
            ReceiverAck {
                received: -1,
                applied: -1,
            },
            |queue| ReceiverAck {
                received: queue.progress().commit_head - 1,
                applied: queue.progress().complete_head - 1,
            },
        );
        self.last_completed_ack = ack.applied;
        ack
    }

    pub(super) fn pending_ack_progress(&self) -> Lsn {
        self.queue.as_ref().map_or(0, |queue| {
            queue.progress().complete_head - 1 - self.last_completed_ack
        })
    }

    /// Copy progress is supplied by its real owner; it is not assumed complete.
    pub(super) fn schedule_ack(
        &self,
        sender: &mut AckSender,
        request: AckRequest,
        copy_pending: Lsn,
        max_pending: usize,
        now: Duration,
    ) -> Result<Option<AckDispatch>, ReceiverError> {
        if copy_pending < 0 {
            return Err(ReceiverError::InvalidAckThreshold);
        }
        let threshold =
            Lsn::try_from(max_pending).map_err(|_| ReceiverError::InvalidAckThreshold)?;
        let pending = self
            .pending_ack_progress()
            .checked_add(copy_pending)
            .ok_or(ReceiverError::InvalidAckThreshold)?;
        let force =
            request.check_pending_limit && sender.force_on_pending_limit() && pending >= threshold;
        Ok(sender.schedule(force, now))
    }

    pub(super) fn receive(
        &mut self,
        sending_epoch: Epoch,
        operations: Vec<ReceivedOperation>,
        completed_hint: Option<Lsn>,
    ) -> Result<ReceiverEffects, ReceiverError> {
        self.ensure_active()?;
        let minimum = self
            .minimum_sending_epoch
            .ok_or(ReceiverError::NotInitialized)?;
        Self::validate_epoch(sending_epoch)?;
        if sending_epoch < minimum {
            return Err(ReceiverError::StaleSendingEpoch {
                sending: sending_epoch,
                minimum,
            });
        }
        if operations.is_empty() {
            return Err(ReceiverError::EmptyBatch);
        }
        if completed_hint.is_some_and(|hint| hint < 0 || hint == Lsn::MAX) {
            return Err(ReceiverError::InvalidCompletedHint);
        }
        for operation in &operations {
            let lsn = operation.queue_operation().lsn();
            Self::validate_epoch(operation.epoch())?;
            if lsn <= 0
                || lsn == Lsn::MAX
                || operation.is_end_of_stream()
                || operation.callback_required() != self.require_ack
                || operation.ack_state() != AckState::NotCalled
            {
                return Err(ReceiverError::InvalidOperation(lsn));
            }
        }
        self.minimum_sending_epoch = Some(sending_epoch);
        let mut effects = ReceiverEffects::default();
        let mut made_progress = false;
        let mut check_pending_limit = false;
        for operation in operations {
            let queue = self.queue.as_mut().unwrap();
            let lsn = operation.queue_operation().lsn();
            let inserted = {
                let mut state = self.callback.lock().unwrap();
                if state.provider_epoch.is_none() && lsn == queue.progress().retained_head {
                    state.provider_epoch = Some(operation.epoch().min(self.copy_epoch.unwrap()));
                }
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    state.operations.entry(lsn)
                {
                    entry.insert(operation.clone());
                    true
                } else {
                    false
                }
            };
            match queue.try_enqueue(operation.queue_operation().clone()) {
                Ok(()) => {
                    self.original_epochs.insert(lsn, operation.epoch());
                    effects.batch.admitted += 1;
                    check_pending_limit = false;
                    let progressed = if self.require_ack {
                        queue.commit()
                    } else {
                        let progressed = queue.complete();
                        check_pending_limit = progressed;
                        progressed
                    };
                    if !progressed {
                        effects.batch.stopped_on_gap = true;
                        effects.ack = made_progress.then_some(AckRequest {
                            check_pending_limit,
                        });
                        self.drain_dispatch(&mut effects)?;
                        return Ok(effects);
                    }
                    made_progress = true;
                }
                Err(error) => {
                    if inserted {
                        self.callback.lock().unwrap().operations.remove(&lsn);
                    }
                    match error {
                        EnqueueError::Duplicate => effects.batch.duplicates += 1,
                        EnqueueError::Full => {
                            effects.batch.queue_full = true;
                            break;
                        }
                    }
                }
            }
        }
        if let Some(hint) = completed_hint.filter(|hint| *hint != 0) {
            self.queue.as_mut().unwrap().update_complete_head(hint + 1);
        }
        effects.ack = Some(AckRequest {
            check_pending_limit,
        });
        self.drain_dispatch(&mut effects)?;
        Ok(effects)
    }

    pub(super) fn process_ack(
        &mut self,
        mut work: AckWork,
    ) -> Result<ReceiverEffects, ReceiverError> {
        let operation = work.operation().clone();
        let belongs = if operation.is_end_of_stream() {
            self.eos
                .as_ref()
                .is_some_and(|eos| eos.same_instance(&operation))
        } else {
            self.callback
                .lock()
                .unwrap()
                .operations
                .get(&operation.queue_operation().lsn())
                .is_some_and(|admitted| admitted.same_instance(&operation))
                || self
                    .cleanup_callbacks
                    .iter()
                    .any(|admitted| admitted.same_instance(&operation))
        };
        if !belongs {
            return Err(ReceiverError::ForeignAck);
        }
        work.start()?;
        self.cleanup_callbacks
            .retain(|admitted| !admitted.same_instance(&operation));
        let mut effects = ReceiverEffects {
            ack_work: Some(work),
            ..ReceiverEffects::default()
        };
        if self.retired {
            return Ok(effects);
        }
        if operation.is_end_of_stream() {
            self.eos_acked = true;
            return Ok(effects);
        }
        let queue = self.queue.as_mut().ok_or(ReceiverError::NotInitialized)?;
        if queue.complete() {
            effects.ack = Some(AckRequest {
                check_pending_limit: true,
            });
            let mut state = self.callback.lock().unwrap();
            if let Some(barrier) = state.full_barrier
                && queue.progress().complete_head - 1 == barrier.previous_epoch_last_lsn
            {
                state.full_barrier = None;
                state.provider_pending = Some(barrier);
                effects.update_epoch = Some(barrier);
            }
        }
        self.drain_dispatch(&mut effects)?;
        Ok(effects)
    }

    pub(super) fn complete_epoch(
        &mut self,
        barrier: EpochBarrier,
        result: Result<(), String>,
        close_in_progress: bool,
        owner: Arc<dyn Send + Sync>,
    ) -> Result<ReceiverEffects, ReceiverError> {
        self.ensure_active()?;
        let full = self.callback.lock().unwrap().provider_pending == Some(barrier);
        let stream = self.dispatch.blocked_epoch() == Some(barrier);
        if !full && !stream {
            return Err(ReceiverError::InvalidEpochCompletion);
        }
        let mut effects = ReceiverEffects::default();
        if let Err(error) = result {
            self.clean_queue(owner, true, true);
            self.retired = true;
            self.callback.lock().unwrap().provider_pending = None;
            effects
                .wakes
                .push(self.dispatch.abort(StreamError::Fault(format!(
                    "provider epoch update: {error}"
                ))));
            effects.fault = Some(error);
            return Ok(effects);
        }
        if full {
            self.callback.lock().unwrap().provider_pending = None;
            if close_in_progress {
                self.queue.as_mut().unwrap().discard_noncompleted();
            } else {
                self.enable_callback();
            }
        }
        if stream {
            effects.wakes.push(self.dispatch.finish_epoch(barrier)?);
        }
        self.drain_dispatch(&mut effects)?;
        Ok(effects)
    }

    pub(super) fn enqueue_eos(
        &mut self,
        owner: Arc<dyn Send + Sync>,
    ) -> Result<ReceiverEffects, ReceiverError> {
        self.ensure_active()?;
        if !self.config.enable_eos_ack {
            return Err(ReceiverError::EosDisabled);
        }
        let mut effects = ReceiverEffects::default();
        if self.eos.is_none() {
            let eos = ReceivedOperation::end_of_stream(self.require_ack);
            eos.ignore_ack_and_keep_owner(owner);
            effects.wakes.push(
                self.dispatch
                    .append(vec![DispatchEntry::Operation(eos.clone())])?,
            );
            self.eos = Some(eos);
        }
        Ok(effects)
    }

    pub(super) fn all_operations_acked(&self) -> bool {
        !self
            .queue
            .as_ref()
            .is_some_and(|queue| queue.progress().has_committed())
            && (self.eos.is_none() || self.eos_acked)
    }

    pub(super) fn discard_out_of_order(&mut self) {
        if let Some(queue) = &mut self.queue {
            if self.require_ack {
                queue.discard_noncommitted();
            } else {
                queue.discard_noncompleted();
            }
        }
        self.prune_operations();
    }

    /// Native queue cleanup only, not a stream drain or a role transition.
    pub(super) fn clean_queue(
        &mut self,
        owner: Arc<dyn Send + Sync>,
        clear_pending: bool,
        ignore_eos_ack: bool,
    ) {
        self.discard_out_of_order();
        if let Some(queue) = &mut self.queue
            && self.require_ack
        {
            let state = self.callback.lock().unwrap();
            for (_, operation) in state.operations.range(queue.progress().complete_head..) {
                if operation.ack_state() == AckState::ReadyToRun
                    && !self
                        .cleanup_callbacks
                        .iter()
                        .any(|pending| pending.same_instance(operation))
                {
                    self.cleanup_callbacks.push(operation.clone());
                }
                operation.ignore_ack_and_keep_owner(owner.clone());
            }
            if clear_pending {
                queue.reset(queue.progress().complete_head);
            }
        }
        if ignore_eos_ack && self.eos.is_some() {
            self.eos_acked = true;
        }
        self.prune_operations();
    }

    /// Explicit terminal containment; native graceful Close has additional
    /// provider/dispatch/role drains owned by the future SecondaryReplicator.
    pub(super) fn retire(&mut self, owner: Arc<dyn Send + Sync>) -> ReceiverEffects {
        self.clean_queue(owner, true, true);
        self.retired = true;
        self.callback.lock().unwrap().staged.clear();
        ReceiverEffects {
            wakes: vec![self.dispatch.abort(StreamError::Closed)],
            ..ReceiverEffects::default()
        }
    }
}

#[cfg(test)]
mod tests;
