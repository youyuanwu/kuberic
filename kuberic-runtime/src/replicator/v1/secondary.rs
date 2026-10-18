// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation of SecondaryReplicator.cpp:393-550,862-1123,1624-2083,
// 2235-2280, SecondaryReplicator.CloseAsyncOperation.cpp:120-188,327-367,
// CloseAsyncOperation2.cpp:116-421 and DrainQueueAsyncOperation.cpp:56-342,
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d,
// src/prod/src/Reliability/Replication/. See sf-v1-port-provenance.md.

//! Private secondary lifetime, not role switching or production activation.
//! Typed data ingress assumes route/epoch admission by its caller. StartCopy/context
//! ACKs use the existing bound establishment. The caller retains and drives
//! this owner; no autonomous task is spawned. Interrupted polls retain provider
//! calls. Dropping the owner is abandonment, never cancellation or drain.

#![cfg_attr(not(test), expect(dead_code, reason = "unselected V1 secondary owner"))]

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use futures::future::{BoxFuture, pending};
use tokio::sync::Notify;

use crate::application::Lsn;
use crate::error::RuntimeError;
use crate::protocol::types::Epoch;
use crate::traits::StateProvider;

use super::ack::{AckDispatch, AckSender};
use super::async_runtime::Clock;
use super::copy_async_operation::{CopyConfig, CopyError, CopySendAction};
use super::copy_establishment::{
    EstablishError, SecondaryEstablishment, SecondaryEstablishmentHandle,
};
use super::copy_sender::CopySenderHandle;
use super::operation_stream::{EpochBarrier, OperationStream, SharedDispatchQueue, StreamError};
use super::received_operation::AckWork;
use super::received_operation::ReceivedOperation;
use super::secondary_copy_receiver::{
    SecondaryCopyAck, SecondaryCopyConfig, SecondaryCopyError, SecondaryCopyReceiver,
};
use super::secondary_receiver::{ReceiverAck, ReceiverConfig, ReceiverError, SecondaryReceiver};
use super::transport::{Route, wire};

#[derive(Clone, Debug, thiserror::Error)]
pub(super) enum SecondaryError {
    #[error("invalid secondary ACK settings or inconsistent receiver ACK modes")]
    InvalidConfig,
    #[error("secondary is closing or closed")]
    Closed,
    #[error("secondary owner was dropped without draining")]
    OwnerDropped,
    #[error("secondary was aborted, not gracefully drained")]
    Aborted,
    #[error("secondary has a reported fault: {0}")]
    Fault(String),
    #[error("StateProvider EndUpdateEpoch: {0}")]
    Provider(Arc<RuntimeError>),
    #[error("provider epoch callback is already outstanding")]
    EpochPending,
    #[error(transparent)]
    Establish(#[from] EstablishError),
    #[error("replication receiver: {0}")]
    Receiver(Arc<ReceiverError>),
    #[error("copy receiver: {0}")]
    CopyReceiver(Arc<SecondaryCopyError>),
    #[error(transparent)]
    Copy(#[from] CopyError),
    #[error(transparent)]
    Stream(#[from] StreamError),
}

impl From<ReceiverError> for SecondaryError {
    fn from(error: ReceiverError) -> Self {
        Self::Receiver(Arc::new(error))
    }
}

impl From<SecondaryCopyError> for SecondaryError {
    fn from(error: SecondaryCopyError) -> Self {
        Self::CopyReceiver(Arc::new(error))
    }
}

#[derive(Clone, Copy)]
pub(super) struct SecondaryConfig {
    pub(super) replication: ReceiverConfig,
    pub(super) copy: SecondaryCopyConfig,
    pub(super) context: CopyConfig,
    pub(super) batch_ack: Duration,
    pub(super) retry_ack: Duration,
    pub(super) max_pending_acks: usize,
    pub(super) trace_interval: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SecondaryAck {
    pub(super) replication: ReceiverAck,
    pub(super) copy: SecondaryCopyAck,
    pub(super) error: i32,
}

pub(super) struct SecondaryCallbacks {
    pub(super) ack: Arc<dyn Fn(SecondaryAck, bool) -> bool + Send + Sync>,
    pub(super) context: Arc<dyn Fn(CopySendAction) -> bool + Send + Sync>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Open,
    Closing,
    Aborting,
    Closed,
    Aborted,
}

#[derive(Clone, Debug)]
pub(super) struct SecondaryEnd {
    pub(super) result: Result<(), SecondaryError>,
    pub(super) aborted: bool,
    /// Required initialized streams drained; uninitialized streams use native
    /// abort bypass. This says nothing about an in-flight provider GetNext.
    pub(super) streams_drained: bool,
    pub(super) fault: Option<SecondaryError>,
    pub(super) context_result: Option<Result<(), EstablishError>>,
}

struct State {
    phase: Phase,
    copy: SecondaryCopyReceiver,
    ack: AckSender,
    ack_error: i32,
    context_requested: bool,
    context_active: bool,
    context_acquiring: bool,
    context_result: Option<Result<(), EstablishError>>,
    pending_epoch: Option<EpochBarrier>,
    running_epoch: Option<EpochBarrier>,
    suppress_epoch: bool,
    wait_copy: bool,
    fault: Option<SecondaryError>,
    drain_fault: bool,
    failed_epoch_drain: bool,
    end: Option<SecondaryEnd>,
}

// Cleanup callbacks retain the real provider, but not a root->receiver->root
// cycle. The explicit owner must remain alive for late ingress and driving.
struct CallbackLifetime {
    _provider: Arc<dyn StateProvider>,
}

struct Root {
    state: Mutex<State>,
    replication: Arc<Mutex<SecondaryReceiver>>,
    copy_dispatch: SharedDispatchQueue,
    replication_dispatch: SharedDispatchQueue,
    establishment: OnceLock<SecondaryEstablishmentHandle>,
    context: OnceLock<CopySenderHandle>,
    config: SecondaryConfig,
    clock: Clock,
    wake: Notify,
    send_ack: Arc<dyn Fn(SecondaryAck, bool) -> bool + Send + Sync>,
    lifetime: Arc<CallbackLifetime>,
}

impl Root {
    fn admit(state: &State) -> Result<(), SecondaryError> {
        if state.phase != Phase::Open {
            return Err(SecondaryError::Closed);
        }
        if let Some(fault) = &state.fault {
            return Err(SecondaryError::Fault(fault.to_string()));
        }
        Ok(())
    }

    fn force_ack(&self, error: i32) {
        let action = {
            let mut state = self.state.lock().unwrap();
            if error != 0 && !state.copy.is_copy_in_progress() {
                return;
            }
            if state.ack_error == 0 {
                state.ack_error = error;
            }
            state.ack.schedule(true, self.clock.now())
        };
        if let Some(action) = action {
            action.dispatch();
        }
        self.wake.notify_one();
    }

    fn send_ack(&self, trace: bool) -> bool {
        let ack = {
            let mut state = self.state.lock().unwrap();
            SecondaryAck {
                replication: self.replication.lock().unwrap().capture_ack(),
                copy: state.copy.capture_ack(),
                error: state.ack_error,
            }
        };
        (self.send_ack)(ack, trace)
    }

    fn schedule(&self, check_pending: bool) -> Result<(), SecondaryError> {
        let action = {
            let mut state = self.state.lock().unwrap();
            let pending = state
                .copy
                .pending_ack_progress()
                .checked_add(self.replication.lock().unwrap().pending_ack_progress())
                .ok_or(ReceiverError::InvalidAckThreshold)?;
            let force = check_pending
                && state.ack.force_on_pending_limit()
                && pending
                    >= Lsn::try_from(self.config.max_pending_acks)
                        .map_err(|_| ReceiverError::InvalidAckThreshold)?;
            state.ack.schedule(force, self.clock.now())
        };
        if let Some(action) = action {
            action.dispatch();
        }
        self.wake.notify_one();
        Ok(())
    }

    fn progress_close(&self) -> Option<(SecondaryEnd, Option<AckDispatch>)> {
        let mut state = self.state.lock().unwrap();
        if let Some(end) = &state.end {
            return Some((end.clone(), None));
        }
        if !matches!(state.phase, Phase::Closing | Phase::Aborting) {
            return None;
        }
        let aborted = state.phase == Phase::Aborting;
        let eos = self.config.copy.enable_eos_ack;
        let mut failed_drain = !aborted && state.failed_epoch_drain;
        if !aborted && eos && !state.failed_epoch_drain {
            if state.wait_copy {
                if !self.copy_dispatch.snapshot().drained || !state.copy.all_operations_acked() {
                    return None;
                }
                failed_drain = state.drain_fault;
                state.wait_copy = false;
            }
            if !failed_drain {
                if !self.replication_dispatch.snapshot().drained
                    || !self.replication.lock().unwrap().all_operations_acked()
                {
                    return None;
                }
                failed_drain = state.drain_fault;
            }
        }
        if (aborted || (eos && !failed_drain)) && state.context_acquiring {
            return None;
        }
        state.suppress_epoch = true;
        if !failed_drain && (state.pending_epoch.is_some() || state.running_epoch.is_some()) {
            return None;
        }
        let end = SecondaryEnd {
            result: if aborted {
                Err(SecondaryError::Aborted)
            } else if failed_drain {
                Err(state.fault.clone().expect("faulted drain has a fault"))
            } else {
                Ok(())
            },
            aborted,
            streams_drained: !aborted && eos && !failed_drain,
            fault: state.fault.clone(),
            context_result: state.context_result.clone(),
        };
        state.phase = if aborted {
            Phase::Aborted
        } else {
            Phase::Closed
        };
        state.end = Some(end.clone());
        Some((end, state.ack.close(self.clock.now())))
    }
}

#[derive(Clone)]
pub(super) struct SecondaryHandle(Weak<Root>);

impl SecondaryHandle {
    fn root(&self) -> Result<Arc<Root>, SecondaryError> {
        self.0.upgrade().ok_or(SecondaryError::OwnerDropped)
    }

    pub(super) fn copy_stream(&self) -> Result<OperationStream, SecondaryError> {
        Ok(self.root()?.copy_dispatch.stream())
    }

    pub(super) fn replication_stream(&self) -> Result<OperationStream, SecondaryError> {
        Ok(self.root()?.replication_dispatch.stream())
    }

    pub(super) fn process_start(&self, envelope: &wire::Envelope) -> Result<bool, SecondaryError> {
        let root = self.root()?;
        Root::admit(&root.state.lock().unwrap())?;
        let initialized = root.establishment.get().unwrap().process_start(envelope)?;
        let mut state = root.state.lock().unwrap();
        if initialized && state.phase == Phase::Open {
            state.context_requested = true;
        }
        drop(state);
        root.wake.notify_one();
        Ok(initialized)
    }

    pub(super) fn process_context_ack(
        &self,
        envelope: &wire::Envelope,
    ) -> Result<(), SecondaryError> {
        let root = self.root()?;
        Root::admit(&root.state.lock().unwrap())?;
        Ok(root
            .establishment
            .get()
            .unwrap()
            .process_context_ack(envelope)?)
    }

    pub(super) fn receive_replication(
        &self,
        epoch: Epoch,
        operations: Vec<ReceivedOperation>,
        completed_hint: Option<Lsn>,
    ) -> Result<(), SecondaryError> {
        let root = self.root()?;
        let effects = {
            let state = root.state.lock().unwrap();
            Root::admit(&state)?;
            root.replication
                .lock()
                .unwrap()
                .receive(epoch, operations, completed_hint)?
        };
        let signals = effects.dispatch()?;
        if let Some(request) = signals.ack {
            root.schedule(request.check_pending_limit)?;
        }
        Ok(())
    }

    pub(super) fn receive_copy(
        &self,
        operation: ReceivedOperation,
        is_last: bool,
    ) -> Result<(), SecondaryError> {
        let root = self.root()?;
        let effects = {
            let mut state = root.state.lock().unwrap();
            Root::admit(&state)?;
            if root.replication.lock().unwrap().queue().is_none() {
                return Err(ReceiverError::NotInitialized.into());
            }
            state.copy.receive(operation, is_last)
        };
        let signals = effects.dispatch()?;
        if signals.check_copy_sender {
            root.context.get().unwrap().finish(CopyError::Cancelled)?;
        }
        if let Some(request) = signals.ack {
            root.schedule(request.check_pending_limit)?;
        }
        root.wake.notify_one();
        Ok(())
    }

    pub(super) fn acknowledge_replication(&self, work: AckWork) -> Result<(), SecondaryError> {
        let root = self.root()?;
        let effects = {
            let mut state = root.state.lock().unwrap();
            let effects = root.replication.lock().unwrap().process_ack(work)?;
            if let Some(barrier) = effects.epoch_request() {
                // Publish before finishing work: owner destruction can reenter Close.
                assert!(state.pending_epoch.is_none() && state.running_epoch.is_none());
                state.pending_epoch = Some(barrier);
            }
            effects
        };
        let signals = effects.dispatch()?;
        root.wake.notify_one();
        if let Some(request) = signals.ack {
            root.schedule(request.check_pending_limit)?;
        }
        Ok(())
    }

    pub(super) fn acknowledge_copy(&self, work: AckWork) -> Result<(), SecondaryError> {
        let root = self.root()?;
        let effects = root.state.lock().unwrap().copy.process_ack(work);
        let signals = effects.dispatch()?;
        root.wake.notify_one();
        if signals.check_copy_sender {
            root.context.get().unwrap().finish(CopyError::Cancelled)?;
        }
        if let Some(request) = signals.ack {
            root.schedule(request.check_pending_limit)?;
        }
        Ok(())
    }

    pub(super) fn update_stream_epoch(&self, barrier: EpochBarrier) -> Result<(), SecondaryError> {
        let root = self.root()?;
        let mut state = root.state.lock().unwrap();
        if state.suppress_epoch {
            return Err(SecondaryError::Closed);
        }
        if state.pending_epoch.is_some() || state.running_epoch.is_some() {
            return Err(SecondaryError::EpochPending);
        }
        if root.replication_dispatch.blocked_epoch() != Some(barrier) {
            return Err(ReceiverError::InvalidEpochCompletion.into());
        }
        state.pending_epoch = Some(barrier);
        drop(state);
        root.wake.notify_one();
        Ok(())
    }

    pub(super) fn report_stream_fault(&self, message: String) -> Result<(), SecondaryError> {
        let root = self.root()?;
        if !root.config.copy.enable_eos_ack {
            return Err(ReceiverError::EosDisabled.into());
        }
        let effects = {
            let mut state = root.state.lock().unwrap();
            if matches!(state.phase, Phase::Closed | Phase::Aborted) {
                return Err(SecondaryError::Closed);
            }
            state.fault.get_or_insert(SecondaryError::Fault(message));
            state.drain_fault = true;
            root.replication
                .lock()
                .unwrap()
                .clean_queue(root.lifetime.clone(), true, false);
            state
                .copy
                .on_stream_report_fault(&(root.lifetime.clone() as Arc<dyn Send + Sync>))
        };
        let signals = effects.dispatch()?;
        if let Some(request) = signals.ack {
            root.schedule(request.check_pending_limit)?;
        }
        root.wake.notify_one();
        Ok(())
    }

    pub(super) fn close(&self) -> Result<(), SecondaryError> {
        self.begin_stop(false)
    }

    pub(super) fn abort(&self) -> Result<(), SecondaryError> {
        self.begin_stop(true)
    }

    fn begin_stop(&self, abort: bool) -> Result<(), SecondaryError> {
        let root = self.root()?;
        let mut wakes = Vec::new();
        let mut copies = Vec::new();
        let mut replications = Vec::new();
        let ack = {
            let mut state = root.state.lock().unwrap();
            if (abort && matches!(state.phase, Phase::Aborting | Phase::Aborted))
                || (!abort && state.phase != Phase::Open)
            {
                return Ok(());
            }
            state.phase = if abort {
                Phase::Aborting
            } else {
                Phase::Closing
            };
            state.end = None;
            state.context_requested = false;
            let owner = root.lifetime.clone() as Arc<dyn Send + Sync>;
            let eos = root.config.copy.enable_eos_ack;
            let mut replication = root.replication.lock().unwrap();
            if abort || !eos {
                replication.clean_queue(owner.clone(), true, true);
                copies.push(state.copy.clean_queue_on_close(&owner, true));
                state.copy.close_copy_operation_queue();
                wakes.push(root.replication_dispatch.abort(StreamError::Closed));
                wakes.push(root.copy_dispatch.abort(StreamError::Closed));
                state.ack.close(root.clock.now())
            } else {
                state.wait_copy = state.copy.is_copy_in_progress();
                if state.wait_copy && !state.copy.all_operations_received() {
                    wakes.push(root.replication_dispatch.clear_pending());
                }
                replications.push(replication.enqueue_eos(owner.clone())?);
                if state.copy.is_copy_in_progress() {
                    copies.push(state.copy.enqueue_eos(&owner)?);
                }
                if state.wait_copy && !state.copy.all_operations_received() {
                    replication.clean_queue(owner.clone(), true, false);
                }
                if !root.copy_dispatch.snapshot().initialized {
                    copies.push(state.copy.clean_queue_on_close(&owner, true));
                    wakes.push(root.copy_dispatch.abort(StreamError::Closed));
                } else {
                    wakes.push(root.copy_dispatch.close());
                }
                if !root.replication_dispatch.snapshot().initialized {
                    replication.clean_queue(owner, true, true);
                    wakes.push(root.replication_dispatch.abort(StreamError::Closed));
                } else {
                    wakes.push(root.replication_dispatch.close());
                }
                None
            }
        };
        // Stop acquisition admission and cancel context sending, not pending calls.
        let establishment = root.establishment.get().unwrap().close();
        for wake in wakes {
            wake.dispatch();
        }
        let mut result = establishment.map_err(SecondaryError::from);
        for effects in replications {
            if let Err(error) = effects.dispatch() {
                result = result.and(Err(error.into()));
            }
        }
        for effects in copies {
            if let Err(error) = effects.dispatch() {
                result = result.and(Err(error.into()));
            }
        }
        if let Some(ack) = ack {
            ack.dispatch();
        }
        root.wake.notify_one();
        result
    }
}

pub(super) struct Secondary {
    root: Arc<Root>,
    establishment: SecondaryEstablishment,
    provider: Arc<dyn StateProvider>,
    send_context: Arc<dyn Fn(CopySendAction) -> bool + Send + Sync>,
    context_done: bool,
    epoch: Option<(EpochBarrier, BoxFuture<'static, crate::Result<()>>)>,
}

impl Secondary {
    pub(super) fn new(
        route: Route,
        config: SecondaryConfig,
        provider: Arc<dyn StateProvider>,
        callbacks: SecondaryCallbacks,
        clock: Clock,
    ) -> Result<Self, SecondaryError> {
        let require_ack =
            config.replication.has_persisted_state || config.replication.require_service_ack;
        if config.batch_ack >= config.retry_ack
            || config.copy.require_service_ack != require_ack
            || config.copy.enable_eos_ack != config.replication.enable_eos_ack
            || config.max_pending_acks > Lsn::MAX as usize
        {
            return Err(SecondaryError::InvalidConfig);
        }
        let replication = Arc::new(Mutex::new(SecondaryReceiver::new(config.replication)));
        let copy = SecondaryCopyReceiver::new(config.copy)?;
        let root = Arc::new_cyclic(|weak: &Weak<Root>| {
            let callback_root = weak.clone();
            let mut ack = AckSender::new(
                config.batch_ack,
                config.retry_ack,
                config.max_pending_acks,
                config.trace_interval,
            );
            ack.open(
                Arc::new(move |trace| {
                    callback_root
                        .upgrade()
                        .is_some_and(|root| root.send_ack(trace))
                }),
                clock.now(),
            );
            Root {
                copy_dispatch: copy.dispatch_queue(),
                replication_dispatch: replication.lock().unwrap().dispatch_queue(),
                replication: replication.clone(),
                state: Mutex::new(State {
                    phase: Phase::Open,
                    copy,
                    ack,
                    ack_error: 0,
                    context_requested: false,
                    context_active: false,
                    context_acquiring: false,
                    context_result: None,
                    pending_epoch: None,
                    running_epoch: None,
                    suppress_epoch: false,
                    wait_copy: false,
                    fault: None,
                    drain_fault: false,
                    failed_epoch_drain: false,
                    end: None,
                }),
                establishment: OnceLock::new(),
                context: OnceLock::new(),
                config,
                clock: clock.clone(),
                wake: Notify::new(),
                send_ack: callbacks.ack,
                lifetime: Arc::new(CallbackLifetime {
                    _provider: provider.clone(),
                }),
            }
        });
        let weak = Arc::downgrade(&root);
        let fault_root = weak.clone();
        let mut establishment = SecondaryEstablishment::with_receiver(
            route,
            replication,
            config.context,
            provider.clone(),
            Arc::new(move |error| {
                if let Some(root) = weak.upgrade() {
                    root.force_ack(error);
                }
            }),
            Some(Arc::new(move |error| {
                if let Some(root) = fault_root.upgrade() {
                    root.state.lock().unwrap().fault.get_or_insert(error.into());
                    root.wake.notify_one();
                }
            })),
            clock,
        )?;
        assert!(root.establishment.set(establishment.handle()).is_ok());
        assert!(root.context.set(establishment.copy().handle()).is_ok());
        Ok(Self {
            root,
            establishment,
            provider,
            send_context: callbacks.context,
            context_done: false,
            epoch: None,
        })
    }

    pub(super) fn handle(&self) -> SecondaryHandle {
        SecondaryHandle(Arc::downgrade(&self.root))
    }

    async fn drive_context(
        establishment: &mut SecondaryEstablishment,
        root: &Root,
        send: Arc<dyn Fn(CopySendAction) -> bool + Send + Sync>,
    ) -> Result<(), EstablishError> {
        let acquisition = establishment.acquire_context(send).await;
        let acquired = {
            let mut state = root.state.lock().unwrap();
            std::mem::replace(&mut state.context_acquiring, false)
        };
        if acquired {
            root.wake.notify_one();
        }
        acquisition?;
        if !root.config.replication.has_persisted_state {
            return Ok(());
        }
        let driven = establishment.copy().drive().await;
        let end = establishment.copy().handle().end()?;
        driven?;
        end.result.map_err(EstablishError::from)
    }

    fn finish_epoch(
        &mut self,
        barrier: EpochBarrier,
        result: crate::Result<()>,
    ) -> Result<(), SecondaryError> {
        let error = result.err().map(Arc::new);
        let mut wakes = Vec::new();
        let mut copy_effects = Vec::new();
        let mut replication_effects = Vec::new();
        {
            let mut state = self.root.state.lock().unwrap();
            let mut replication = self.root.replication.lock().unwrap();
            let closing = state.phase != Phase::Open;
            let stream = self.root.replication_dispatch.blocked_epoch() == Some(barrier);
            let full = replication.has_pending_epoch(barrier);
            let owner = self.root.lifetime.clone() as Arc<dyn Send + Sync>;
            if let Some(error) = &error {
                state
                    .fault
                    .get_or_insert(SecondaryError::Provider(error.clone()));
            }
            if full && error.is_some() {
                if self.root.config.copy.enable_eos_ack && !closing {
                    replication_effects.push(replication.enqueue_eos(owner.clone())?);
                    if state.copy.is_copy_in_progress() {
                        copy_effects.push(state.copy.enqueue_eos(&owner)?);
                    }
                }
                replication.fail_pending_epoch(barrier, owner.clone())?;
                copy_effects.push(state.copy.clean_queue_on_update_epoch_failure(&owner));
                state.failed_epoch_drain = true;
            } else if full || stream {
                // A stream marker reports the provider fault separately, then
                // resumes dequeue. Only an ACK barrier owns queue-failure cleanup.
                replication_effects.push(replication.complete_epoch(
                    barrier,
                    Ok(()),
                    closing,
                    owner,
                )?);
            } else if !closing {
                return Err(ReceiverError::InvalidEpochCompletion.into());
            }
            state.running_epoch = None;
            if state.failed_epoch_drain && closing {
                wakes.push(self.root.replication_dispatch.clear_pending());
            }
        }
        for wake in wakes {
            wake.dispatch();
        }
        let mut result = Ok(());
        for effects in replication_effects {
            if let Err(error) = effects.dispatch() {
                result = result.and(Err(error.into()));
            }
        }
        for effects in copy_effects {
            if let Err(error) = effects.dispatch() {
                result = result.and(Err(error.into()));
            }
        }
        self.root.wake.notify_one();
        result
    }

    pub(super) async fn run(&mut self) -> Result<SecondaryEnd, SecondaryError> {
        let root = self.root.clone();
        let copy_changed = root.copy_dispatch.notifier();
        let replication_changed = root.replication_dispatch.notifier();
        loop {
            let wake = root.wake.notified();
            let copy_wake = copy_changed.notified();
            let replication_wake = replication_changed.notified();
            tokio::pin!(wake, copy_wake, replication_wake);
            wake.as_mut().enable();
            copy_wake.as_mut().enable();
            replication_wake.as_mut().enable();
            if let Some((end, ack)) = root.progress_close() {
                if let Some(ack) = ack {
                    ack.dispatch();
                }
                return Ok(end);
            }
            let (context, deadline) = {
                let mut state = root.state.lock().unwrap();
                if self.epoch.is_none()
                    && let Some(barrier) = state.pending_epoch.take()
                {
                    state.running_epoch = Some(barrier);
                    let provider = self.provider.clone();
                    self.epoch = Some((
                        barrier,
                        Box::pin(async move {
                            provider
                                .update_epoch(barrier.epoch, barrier.previous_epoch_last_lsn)
                                .await
                        }),
                    ));
                }
                if state.context_requested && !self.context_done {
                    state.context_active = true;
                    state.context_acquiring = true;
                    state.context_requested = false;
                }
                (
                    state.context_active && !self.context_done,
                    state.ack.deadline(),
                )
            };
            let epoch = async {
                match &mut self.epoch {
                    Some((barrier, future)) => (*barrier, future.await),
                    None => pending().await,
                }
            };
            let timer = async {
                match deadline {
                    Some(deadline) => {
                        tokio::time::sleep(deadline.saturating_sub(root.clock.now())).await
                    }
                    None => pending().await,
                }
            };
            tokio::select! {
                result = Self::drive_context(&mut self.establishment, &root, self.send_context.clone()), if context => {
                    self.context_done = true;
                    {
                        let mut state = root.state.lock().unwrap();
                        state.context_result = Some(result.clone());
                        if let Err(error @ EstablishError::Copy(CopyError::Provider(_))) = &result {
                            state.fault.get_or_insert(error.clone().into());
                        }
                    }
                    if matches!(result, Err(EstablishError::Copy(CopyError::Remote(_))))
                        && root.state.lock().unwrap().phase == Phase::Open
                        && root.state.lock().unwrap().copy.is_copy_in_progress()
                    {
                        root.force_ack(-1);
                    }
                }
                (barrier, result) = epoch => {
                    self.epoch = None;
                    self.finish_epoch(barrier, result)?;
                }
                _ = wake.as_mut() => {}
                _ = copy_wake.as_mut() => {}
                _ = replication_wake.as_mut() => {}
                _ = timer => {
                    let action = root.state.lock().unwrap().ack.on_timer(root.clock.now());
                    if let Some(action) = action { action.dispatch(); }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
