// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation of SecondaryCopyReceiver.h/.cpp, microsoft/
// service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d,
// src/prod/src/Reliability/Replication/. See sf-v1-port-provenance.md.

//! Private, caller-serialized copy receiver, not a copy/build or provider owner.
//! Dispatch effects outside the caller's guard. Cleanup closes queue admission;
//! it does not drain the stream, report a host fault, or complete a role change.
//! Copy-sender and ACK signals are obligations for their actual owners.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::application::Lsn;

use super::operation_queue::{CallbackControl, EnqueueError, OperationQueue, QueueConfig};
#[cfg(test)]
use super::operation_stream::OperationStream;
use super::operation_stream::{DispatchEntry, DispatchWake, SharedDispatchQueue, StreamError};
use super::received_operation::{AckError, AckState, AckWork, ReceivedOperation};

#[derive(Clone, Copy, Debug)]
pub(super) struct SecondaryCopyConfig {
    pub(super) initial_capacity: usize,
    pub(super) max_capacity: usize,
    pub(super) require_service_ack: bool,
    pub(super) enable_eos_ack: bool,
    pub(super) copy_done: bool,
}

impl SecondaryCopyConfig {
    pub(super) fn is_valid(self) -> bool {
        self.initial_capacity > 1
            && self.initial_capacity.is_power_of_two()
            && self.max_capacity.is_power_of_two()
            && self.initial_capacity <= self.max_capacity
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(super) enum SecondaryCopyError {
    #[error("copy queue capacities must be powers of two, with 1 < initial <= maximum")]
    InvalidCapacity,
    #[error("retained local initialization cannot claim a completed remote copy")]
    InvalidInitialization,
    #[error("operation {0} is not fresh copy data or the configured terminal marker")]
    InvalidOperation(Lsn),
    #[error("ACK does not belong to a committed operation of this copy receiver")]
    ForeignAck,
    #[error("copy EOS ACK mode is disabled")]
    EosDisabled,
    #[error(transparent)]
    Ack(#[from] AckError),
    #[error(transparent)]
    Stream(#[from] StreamError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SecondaryCopyAck {
    pub(super) received: Lsn,
    pub(super) applied: Lsn,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum CopyAdmission {
    #[default]
    None,
    Admitted,
    Duplicate,
    Full,
    TerminalOnly,
    AdmissionClosed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CopyAckRequest {
    pub(super) check_pending_limit: bool,
}

/// No transport send, copy-sender completion, host fault report or drain occurs here.
#[must_use = "deliver copy ACK scheduling and copy-sender checks to their actual owners"]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SecondaryCopySignals {
    pub(super) ack: Option<CopyAckRequest>,
    pub(super) check_copy_sender: bool,
    pub(super) progress_done: bool,
    pub(super) dispatch_closed: bool,
    pub(super) admission: CopyAdmission,
}

#[must_use = "dispatch wakeups, rejected inputs, errors and rooted ACK finishes outside the caller's guard"]
#[derive(Default)]
pub(super) struct SecondaryCopyEffects {
    wakes: Vec<DispatchWake>,
    ack_work: Option<AckWork>,
    retired_ack_work: Option<AckWork>,
    retired_operations: Vec<ReceivedOperation>,
    retired_roots: Vec<Arc<dyn Send + Sync>>,
    signals: SecondaryCopySignals,
    error: Option<SecondaryCopyError>,
}

impl SecondaryCopyEffects {
    pub(super) fn dispatch(self) -> Result<SecondaryCopySignals, SecondaryCopyError> {
        for wake in self.wakes {
            wake.dispatch();
        }
        if let Some(work) = self.ack_work {
            work.finish()?;
        }
        drop(self.retired_ack_work);
        drop(self.retired_operations);
        drop(self.retired_roots);
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.signals),
        }
    }
}

struct CopyDispatchState {
    // Identity lookup only: OperationQueue owns ordering and admission.
    operations: BTreeMap<Lsn, ReceivedOperation>,
    staged: Vec<DispatchEntry>,
}

struct RootedCallback {
    operation: ReceivedOperation,
    root: Arc<dyn Send + Sync>,
}

pub(super) struct SecondaryCopyReceiver {
    config: SecondaryCopyConfig,
    queue: Option<OperationQueue>,
    callback: Arc<Mutex<CopyDispatchState>>,
    dispatch: SharedDispatchQueue,
    last_copy_lsn: Lsn,
    all_received: bool,
    queue_closed: bool,
    last_completed_ack: Lsn,
    terminal_enqueued: bool,
    eos_acked: bool,
    stream_faulted: bool,
    eos: Option<ReceivedOperation>,
    cleanup_callbacks: Vec<ReceivedOperation>,
    rooted_callbacks: Vec<RootedCallback>,
}

impl SecondaryCopyReceiver {
    pub(super) fn new(config: SecondaryCopyConfig) -> Result<Self, SecondaryCopyError> {
        Self::create(config, false)
    }

    pub(super) fn for_retained_state(
        config: SecondaryCopyConfig,
    ) -> Result<Self, SecondaryCopyError> {
        if config.copy_done {
            return Err(SecondaryCopyError::InvalidInitialization);
        }
        Self::create(config, true)
    }

    fn create(config: SecondaryCopyConfig, retained: bool) -> Result<Self, SecondaryCopyError> {
        if !config.is_valid() {
            return Err(SecondaryCopyError::InvalidCapacity);
        }
        let callback = Arc::new(Mutex::new(CopyDispatchState {
            operations: BTreeMap::new(),
            staged: Vec::new(),
        }));
        let dispatch = SharedDispatchQueue::new();
        let queue = if config.copy_done || retained {
            // This newly constructed queue cannot have readers or external wakers.
            dispatch.close().dispatch();
            None
        } else {
            let mut queue = OperationQueue::new(
                QueueConfig {
                    initial_capacity: config.initial_capacity,
                    max_capacity: config.max_capacity,
                    max_bytes: 0,
                    max_completed_count: 0,
                    max_completed_bytes: 0,
                    clean_on_complete: true,
                },
                1,
                config.require_service_ack,
                !config.require_service_ack,
            );
            let state = callback.clone();
            queue.set_commit_callback(Some(Box::new(move |entry, _| {
                let mut state = state.lock().unwrap();
                let operation = state.operations[&entry.lsn()].clone();
                state.staged.push(DispatchEntry::Operation(operation));
                CallbackControl::Keep
            })));
            Some(queue)
        };
        Ok(Self {
            config,
            queue,
            callback,
            dispatch,
            last_copy_lsn: -1,
            all_received: config.copy_done,
            queue_closed: config.copy_done || retained,
            last_completed_ack: 0,
            terminal_enqueued: false,
            eos_acked: !config.require_service_ack,
            stream_faulted: false,
            eos: None,
            cleanup_callbacks: Vec::new(),
            rooted_callbacks: Vec::new(),
        })
    }

    #[cfg(test)]
    pub(super) fn stream(&self) -> OperationStream {
        self.dispatch.stream()
    }

    pub(super) fn dispatch_queue(&self) -> SharedDispatchQueue {
        self.dispatch.clone()
    }

    #[cfg(any(test, feature = "testing"))]
    pub(super) fn queue(&self) -> Option<&OperationQueue> {
        self.queue.as_ref()
    }

    pub(super) fn all_operations_received(&self) -> bool {
        self.all_received
    }

    pub(super) fn is_copy_in_progress(&self) -> bool {
        self.queue.is_some()
    }

    #[cfg(test)]
    pub(super) fn copy_queue_closed(&self) -> bool {
        self.queue_closed
    }

    #[cfg(test)]
    pub(super) fn stream_faulted(&self) -> bool {
        self.stream_faulted
    }

    #[cfg(test)]
    pub(super) fn enqueued_end_of_stream(&self) -> bool {
        self.terminal_enqueued
    }

    pub(super) fn all_operations_acked(&self) -> bool {
        !self
            .queue
            .as_ref()
            .is_some_and(|queue| queue.progress().has_committed())
            && (!self.terminal_enqueued || self.eos_acked)
    }

    pub(super) fn capture_ack(&mut self) -> SecondaryCopyAck {
        let ack = self.queue.as_ref().map_or(
            SecondaryCopyAck {
                received: -1,
                applied: -1,
            },
            |queue| SecondaryCopyAck {
                received: if self.all_received {
                    -1
                } else {
                    queue.progress().commit_head - 1
                },
                applied: queue.progress().complete_head - 1,
            },
        );
        self.last_completed_ack = ack.applied;
        ack
    }

    pub(super) fn pending_ack_progress(&self) -> Lsn {
        if self.last_completed_ack == -1 {
            0
        } else {
            self.queue.as_ref().map_or_else(
                || self.last_copy_lsn - self.last_completed_ack + 1,
                |queue| queue.progress().complete_head - 1 - self.last_completed_ack,
            )
        }
    }

    fn prune_operations(&mut self, effects: &mut SecondaryCopyEffects) {
        let mut state = self.callback.lock().unwrap();
        let retired = state
            .operations
            .keys()
            .copied()
            .filter(|lsn| {
                !self
                    .queue
                    .as_ref()
                    .is_some_and(|queue| queue.get_operation(*lsn).is_some())
            })
            .collect::<Vec<_>>();
        for lsn in retired {
            effects
                .retired_operations
                .push(state.operations.remove(&lsn).unwrap());
        }
    }

    fn drain_dispatch(
        &mut self,
        effects: &mut SecondaryCopyEffects,
    ) -> Result<(), SecondaryCopyError> {
        let entries = std::mem::take(&mut self.callback.lock().unwrap().staged);
        if !entries.is_empty() {
            effects.wakes.push(self.dispatch.append(entries)?);
        }
        self.prune_operations(effects);
        Ok(())
    }

    fn validate_operation(
        &self,
        operation: &ReceivedOperation,
        is_last: bool,
    ) -> Result<(), SecondaryCopyError> {
        let lsn = operation.queue_operation().lsn();
        if lsn <= 0
            || lsn == Lsn::MAX
            || operation.epoch().data_loss_number < 0
            || operation.epoch().configuration_number < 0
            || operation.callback_required() != self.config.require_service_ack
            || operation.ack_state() != AckState::NotCalled
            || !operation.queue_operation().needs_ack()
            || (operation.is_end_of_stream() && !is_last)
            || (is_last && self.config.enable_eos_ack && !operation.is_end_of_stream())
            || (operation.is_end_of_stream() && operation.queue_operation().logical_bytes() != 0)
        {
            Err(SecondaryCopyError::InvalidOperation(lsn))
        } else {
            Ok(())
        }
    }

    /// Validation errors and rejected input retirement are delivered by dispatch.
    pub(super) fn receive(
        &mut self,
        operation: ReceivedOperation,
        is_last: bool,
    ) -> SecondaryCopyEffects {
        let mut effects = SecondaryCopyEffects::default();
        if let Err(error) = self.validate_operation(&operation, is_last) {
            effects.retired_operations.push(operation);
            effects.error = Some(error);
            return effects;
        }
        if is_last {
            self.last_copy_lsn = operation.queue_operation().lsn();
        }
        if self.all_received || self.queue_closed {
            effects.signals.admission = CopyAdmission::AdmissionClosed;
            effects.signals.ack = Some(CopyAckRequest {
                check_pending_limit: false,
            });
            effects.retired_operations.push(operation);
            return effects;
        }

        let is_empty_terminal = is_last
            && operation.queue_operation().logical_bytes() == 0
            && !self.config.enable_eos_ack;
        let mut check_pending_limit = false;
        if is_empty_terminal {
            // Legacy empty terminal consumes a protocol LSN, not a stream item.
            self.last_copy_lsn -= 1;
            self.all_received =
                self.queue.as_ref().unwrap().progress().commit_head - 1 == self.last_copy_lsn;
            effects.signals.admission = CopyAdmission::TerminalOnly;
            effects.retired_operations.push(operation);
        } else {
            let lsn = operation.queue_operation().lsn();
            let queue = self.queue.as_mut().unwrap();
            match queue.try_enqueue(operation.queue_operation().clone()) {
                Ok(()) => {
                    self.callback
                        .lock()
                        .unwrap()
                        .operations
                        .insert(lsn, operation.clone());
                    if operation.is_end_of_stream()
                        && let Some(retired) = self.eos.replace(operation)
                    {
                        effects.retired_operations.push(retired);
                    }
                    effects.signals.admission = CopyAdmission::Admitted;
                    let progressed = if self.config.require_service_ack {
                        queue.commit()
                    } else {
                        let progressed = queue.complete();
                        check_pending_limit = progressed;
                        progressed
                    };
                    if !progressed {
                        return effects;
                    }
                    effects.signals.progress_done = true;
                    self.all_received = queue.progress().commit_head - 1 == self.last_copy_lsn;
                }
                Err(error) => {
                    effects.signals.admission = match error {
                        EnqueueError::Duplicate => CopyAdmission::Duplicate,
                        EnqueueError::Full => CopyAdmission::Full,
                    };
                    effects.retired_operations.push(operation);
                }
            }
        }

        if let Err(error) = self.drain_dispatch(&mut effects) {
            effects.error = Some(error);
            return effects;
        }
        if self.all_received {
            effects.wakes.push(self.dispatch.close());
            effects.signals.dispatch_closed = true;
            effects.signals.progress_done = true;
            self.terminal_enqueued = true;
            if !self.queue.as_ref().unwrap().progress().has_pending() {
                self.queue = None;
                self.prune_operations(&mut effects);
                effects.signals.check_copy_sender = true;
                check_pending_limit = true;
            }
        }
        effects.signals.ack = Some(CopyAckRequest {
            check_pending_limit,
        });
        effects
    }

    /// A rejected token is retired by dispatch without starting its callback.
    pub(super) fn process_ack(&mut self, mut work: AckWork) -> SecondaryCopyEffects {
        let operation = work.operation().clone();
        let lsn = operation.queue_operation().lsn();
        let retained_callback = self
            .cleanup_callbacks
            .iter()
            .any(|admitted| admitted.same_instance(&operation));
        let live = self
            .callback
            .lock()
            .unwrap()
            .operations
            .get(&lsn)
            .is_some_and(|admitted| admitted.same_instance(&operation))
            && self
                .queue
                .as_ref()
                .is_some_and(|queue| lsn < queue.progress().commit_head);
        let standalone_eos = operation.is_end_of_stream()
            && lsn == 0
            && self
                .eos
                .as_ref()
                .is_some_and(|eos| eos.same_instance(&operation));
        if !live && !retained_callback && !standalone_eos {
            return SecondaryCopyEffects {
                retired_ack_work: Some(work),
                error: Some(SecondaryCopyError::ForeignAck),
                ..SecondaryCopyEffects::default()
            };
        }
        if let Err(error) = work.start() {
            return SecondaryCopyEffects {
                retired_ack_work: Some(work),
                error: Some(error.into()),
                ..SecondaryCopyEffects::default()
            };
        }
        let mut effects = SecondaryCopyEffects {
            ack_work: Some(work),
            ..SecondaryCopyEffects::default()
        };
        self.cleanup_callbacks
            .retain(|admitted| !admitted.same_instance(&operation));
        if let Some(index) = self
            .rooted_callbacks
            .iter()
            .position(|rooted| rooted.operation.same_instance(&operation))
        {
            effects
                .retired_roots
                .push(self.rooted_callbacks.swap_remove(index).root);
        }
        if operation.is_end_of_stream() {
            self.eos_acked = true;
        }
        if let Some(queue) = self.queue.as_mut()
            && queue.complete()
        {
            if (self.all_received || self.queue_closed) && !queue.progress().has_pending() {
                if self.all_received {
                    self.queue = None;
                }
                effects.signals.check_copy_sender = true;
            }
            effects.signals.ack = Some(CopyAckRequest {
                check_pending_limit: true,
            });
        }
        self.prune_operations(&mut effects);
        effects
    }

    pub(super) fn close_copy_operation_queue(&mut self) {
        self.queue_closed = true;
    }

    pub(super) fn discard_out_of_order(&mut self) -> SecondaryCopyEffects {
        let mut effects = SecondaryCopyEffects::default();
        if let Some(queue) = &mut self.queue {
            if self.config.require_service_ack {
                queue.discard_noncommitted();
            } else {
                queue.discard_noncompleted();
            }
        }
        self.prune_operations(&mut effects);
        effects
    }

    fn root_callback(
        &mut self,
        operation: &ReceivedOperation,
        owner: &Arc<dyn Send + Sync>,
        effects: &mut SecondaryCopyEffects,
    ) {
        if !operation.callback_required() {
            // Only a fresh local EOS reaches this branch: optimistic cleanup
            // never re-roots, and enqueue_eos is terminal_enqueued-idempotent.
            operation.ignore_ack_and_keep_owner(owner.clone());
            return;
        }
        // Preserve an old root through replacement: its destructor may reenter.
        if let Some(index) = self
            .rooted_callbacks
            .iter()
            .position(|rooted| rooted.operation.same_instance(operation))
        {
            effects
                .retired_roots
                .push(self.rooted_callbacks.swap_remove(index).root);
        }
        operation.ignore_ack_and_keep_owner(owner.clone());
        if matches!(
            operation.ack_state(),
            AckState::NotCalled | AckState::ReadyToRun | AckState::Running
        ) {
            self.rooted_callbacks.push(RootedCallback {
                operation: operation.clone(),
                root: owner.clone(),
            });
            if !self
                .cleanup_callbacks
                .iter()
                .any(|pending| pending.same_instance(operation))
            {
                self.cleanup_callbacks.push(operation.clone());
            }
        }
    }

    fn clean_queue(
        &mut self,
        owner: &Arc<dyn Send + Sync>,
        ignore_eos_ack: bool,
    ) -> SecondaryCopyEffects {
        let mut effects = self.discard_out_of_order();
        if self.queue.is_some() {
            if self.config.require_service_ack {
                let operations = self
                    .callback
                    .lock()
                    .unwrap()
                    .operations
                    .values()
                    .cloned()
                    .collect::<Vec<_>>();
                for operation in operations {
                    self.root_callback(&operation, owner, &mut effects);
                }
                let queue = self.queue.as_mut().unwrap();
                queue.reset(queue.progress().complete_head);
            }
            self.queue_closed = true;
        }
        if ignore_eos_ack && self.terminal_enqueued {
            // Ignoring the wait must not cancel the EOS callback itself.
            self.eos_acked = true;
        }
        self.prune_operations(&mut effects);
        effects
    }

    pub(super) fn clean_queue_on_close(
        &mut self,
        owner: &Arc<dyn Send + Sync>,
        ignore_eos_ack: bool,
    ) -> SecondaryCopyEffects {
        self.clean_queue(owner, ignore_eos_ack)
    }

    pub(super) fn clean_queue_on_update_epoch_failure(
        &mut self,
        owner: &Arc<dyn Send + Sync>,
    ) -> SecondaryCopyEffects {
        self.clean_queue(owner, true)
    }

    pub(super) fn on_stream_report_fault(
        &mut self,
        owner: &Arc<dyn Send + Sync>,
    ) -> SecondaryCopyEffects {
        self.stream_faulted = true;
        let effects = self.clean_queue(owner, false);
        self.close_copy_operation_queue();
        effects
    }

    /// Local marker only; the host still owns admission closure and stream EOF.
    pub(super) fn enqueue_eos(
        &mut self,
        owner: &Arc<dyn Send + Sync>,
    ) -> Result<SecondaryCopyEffects, SecondaryCopyError> {
        if !self.config.enable_eos_ack {
            return Err(SecondaryCopyError::EosDisabled);
        }
        let mut effects = SecondaryCopyEffects::default();
        if !self.terminal_enqueued {
            let eos = ReceivedOperation::end_of_stream(self.config.require_service_ack);
            effects.wakes.push(
                self.dispatch
                    .append(vec![DispatchEntry::Operation(eos.clone())])?,
            );
            self.root_callback(&eos, owner, &mut effects);
            if let Some(retired) = self.eos.replace(eos) {
                effects.retired_operations.push(retired);
            }
            self.terminal_enqueued = true;
        }
        Ok(effects)
    }
}

#[cfg(test)]
mod tests;
