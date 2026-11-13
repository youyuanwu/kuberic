// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation of SecondaryReplicator.cpp:393-550,862-1123,1624-2083,
// 2235-2280, SecondaryReplicator.CloseAsyncOperation.cpp:120-188,327-367,
// CloseAsyncOperation2.cpp:116-421 and DrainQueueAsyncOperation.cpp:56-342,
// promotion branches in both Close operations and
// SecondaryReplicator.UpdateEpochAsyncOperation.cpp:31-155,
// SecondaryReplicator.cpp:553-619 (copy-gated progress/capability queries),
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d,
// src/prod/src/Reliability/Replication/. See sf-v1-port-provenance.md.

//! Private secondary lifetime, promotion drains and direct external epochs.
//! The role owner supplies queue handoff; no production activation is implied.
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
use super::replication_queue_manager::QueueTransfer;
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
    #[error("secondary progress is unavailable before copy completes")]
    NotReady,
    #[error("StateProvider EndUpdateEpoch: {0}")]
    Provider(Arc<RuntimeError>),
    #[error("provider epoch callback is already outstanding")]
    EpochPending,
    #[error("copy is incomplete for this role/epoch transition")]
    IncompleteCopy,
    #[error("queue transfer requires a successfully drained promotion")]
    InvalidTransfer,
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

impl SecondaryConfig {
    pub(super) fn is_valid(self) -> bool {
        let require_ack =
            self.replication.has_persisted_state || self.replication.require_service_ack;
        self.batch_ack < self.retry_ack
            && self.replication.queue.is_valid()
            && self.copy.is_valid()
            && self.context.sender.is_valid()
            && self.copy.require_service_ack == require_ack
            && self.copy.enable_eos_ack == self.replication.enable_eos_ack
            && self.max_pending_acks <= Lsn::MAX as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SecondaryAck {
    pub(super) replication: ReceiverAck,
    pub(super) copy: SecondaryCopyAck,
    pub(super) error: i32,
}

#[derive(Clone)]
pub(super) struct SecondaryCallbacks {
    pub(super) ack: Arc<dyn Fn(Route, SecondaryAck, bool) -> bool + Send + Sync>,
    pub(super) context: Arc<dyn Fn(Route, CopySendAction) -> bool + Send + Sync>,
    pub(super) fault: Arc<dyn Fn(SecondaryError) + Send + Sync>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Open,
    Closing,
    Aborting,
    Closed,
    Aborted,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DriveUntil {
    Terminal,
    ExternalEpoch,
    CopyDispatch,
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
    promoting: bool,
    external_epoch: bool,
    transferred: bool,
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
    reply_route: OnceLock<Arc<Mutex<Route>>>,
    context: OnceLock<CopySenderHandle>,
    config: SecondaryConfig,
    clock: Clock,
    wake: Notify,
    send_ack: Arc<dyn Fn(Route, SecondaryAck, bool) -> bool + Send + Sync>,
    report_fault: Arc<dyn Fn(SecondaryError) + Send + Sync>,
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
        let route = self.reply_route.get().unwrap().lock().unwrap().clone();
        (self.send_ack)(route, ack, trace)
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
        let draining = eos || state.promoting;
        if !aborted && draining && !state.failed_epoch_drain {
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
        if (aborted || (draining && !failed_drain)) && state.context_acquiring {
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
            streams_drained: !aborted && draining && !failed_drain,
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

    pub(super) fn current_progress(&self) -> Result<Lsn, SecondaryError> {
        self.query_progress(false)
    }

    /// Non-mutating availability for routine host telemetry. The protected
    /// native query below deliberately retains its NotReady/fault side effect.
    pub(super) fn progress_query_readiness(&self) -> Result<bool, SecondaryError> {
        let root = self.root()?;
        let state = root.state.lock().unwrap();
        Root::admit(&state)?;
        Ok(!state.copy.is_copy_in_progress() && root.replication.lock().unwrap().queue().is_some())
    }

    pub(super) fn catchup_capability(&self) -> Result<Lsn, SecondaryError> {
        self.query_progress(true)
    }

    fn query_progress(&self, capability: bool) -> Result<Lsn, SecondaryError> {
        let root = self.root()?;
        let result = {
            let mut state = root.state.lock().unwrap();
            Root::admit(&state)?;
            let replication = root.replication.lock().unwrap();
            if state.copy.is_copy_in_progress() || replication.queue().is_none() {
                state.fault = Some(SecondaryError::NotReady);
                Err(SecondaryError::NotReady)
            } else if capability {
                replication
                    .catchup_capability()
                    .map_err(SecondaryError::from)
            } else {
                replication.current_progress().map_err(SecondaryError::from)
            }
        };
        if matches!(result, Err(SecondaryError::NotReady)) {
            (root.report_fault)(SecondaryError::NotReady);
            root.wake.notify_one();
        }
        result
    }

    pub(super) fn copy_stream(&self) -> Result<OperationStream, SecondaryError> {
        Ok(self.root()?.copy_dispatch.stream())
    }

    pub(super) fn replication_stream(&self) -> Result<OperationStream, SecondaryError> {
        Ok(self.root()?.replication_dispatch.stream())
    }

    pub(super) fn process_start(&self, envelope: &wire::Envelope) -> Result<bool, SecondaryError> {
        self.start(envelope, true)
    }

    fn start(&self, envelope: &wire::Envelope, bound: bool) -> Result<bool, SecondaryError> {
        let root = self.root()?;
        Root::admit(&root.state.lock().unwrap())?;
        let establishment = root.establishment.get().unwrap();
        let initialized = if bound {
            establishment.process_start(envelope)?
        } else {
            establishment.process_start_message(envelope)?
        };
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
        self.receive_replication_from(epoch, operations, completed_hint, None)
    }

    fn receive_replication_from(
        &self,
        epoch: Epoch,
        operations: Vec<ReceivedOperation>,
        completed_hint: Option<Lsn>,
        primary: Option<Route>,
    ) -> Result<(), SecondaryError> {
        let root = self.root()?;
        let effects = {
            let state = root.state.lock().unwrap();
            Root::admit(&state)?;
            let effects =
                root.replication
                    .lock()
                    .unwrap()
                    .receive(epoch, operations, completed_hint)?;
            if let Some(primary) = primary {
                root.establishment.get().unwrap().update_primary(primary)?;
            }
            effects
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
        self.begin_stop(false, false)
    }

    pub(super) fn abort(&self) -> Result<(), SecondaryError> {
        self.begin_stop(true, false)
    }

    pub(super) fn close_for_primary(&self) -> Result<(), SecondaryError> {
        self.begin_stop(false, true)
    }

    pub(super) fn begin_external_epoch(&self, epoch: Epoch) -> Result<bool, SecondaryError> {
        let root = self.root()?;
        let mut state = root.state.lock().unwrap();
        Root::admit(&state)?;
        if state.external_epoch {
            return Err(SecondaryError::EpochPending);
        }
        if !root
            .replication
            .lock()
            .unwrap()
            .advance_external_epoch(epoch)?
        {
            return Ok(false);
        }
        if state.copy.is_copy_in_progress() {
            state.fault.get_or_insert(SecondaryError::IncompleteCopy);
            return Err(SecondaryError::IncompleteCopy);
        }
        state.external_epoch = true;
        drop(state);
        root.wake.notify_one();
        Ok(true)
    }

    fn begin_stop(&self, abort: bool, promotion: bool) -> Result<(), SecondaryError> {
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
            if promotion {
                Root::admit(&state)?;
                if root.replication.lock().unwrap().queue().is_none()
                    || (state.copy.is_copy_in_progress()
                        && (!state.copy.all_operations_received()
                            || (!root.config.copy.enable_eos_ack
                                && root.config.copy.require_service_ack)))
                {
                    state.fault.get_or_insert(SecondaryError::IncompleteCopy);
                    return Err(SecondaryError::IncompleteCopy);
                }
            }
            state.promoting = promotion && !abort;
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
            if abort || (!eos && !promotion) {
                replication.clean_queue(owner.clone(), true, true);
                copies.push(state.copy.clean_queue_on_close(&owner, true));
                state.copy.close_copy_operation_queue();
                wakes.push(root.replication_dispatch.abort(StreamError::Closed));
                wakes.push(root.copy_dispatch.abort(StreamError::Closed));
                state.ack.close(root.clock.now())
            } else {
                state.wait_copy = state.copy.is_copy_in_progress();
                if !promotion && state.wait_copy && !state.copy.all_operations_received() {
                    wakes.push(root.replication_dispatch.clear_pending());
                }
                if eos {
                    replications.push(replication.enqueue_eos(owner.clone())?);
                    if state.copy.is_copy_in_progress() {
                        copies.push(state.copy.enqueue_eos(&owner)?);
                    }
                }
                if !promotion && state.wait_copy && !state.copy.all_operations_received() {
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
    send_context: Arc<dyn Fn(Route, CopySendAction) -> bool + Send + Sync>,
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
        Self::create(route, config, provider, callbacks, clock, None)
    }

    fn create(
        route: Route,
        config: SecondaryConfig,
        provider: Arc<dyn StateProvider>,
        callbacks: SecondaryCallbacks,
        clock: Clock,
        transfer: Option<(Epoch, QueueTransfer)>,
    ) -> Result<Self, SecondaryError> {
        if !config.is_valid() {
            return Err(SecondaryError::InvalidConfig);
        }
        let replication = Arc::new(Mutex::new(match transfer {
            Some((epoch, transfer)) => {
                SecondaryReceiver::from_primary(config.replication, epoch, transfer)?
            }
            None => SecondaryReceiver::new(config.replication),
        }));
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
                    promoting: false,
                    external_epoch: false,
                    transferred: false,
                }),
                establishment: OnceLock::new(),
                reply_route: OnceLock::new(),
                context: OnceLock::new(),
                config,
                clock: clock.clone(),
                wake: Notify::new(),
                send_ack: callbacks.ack,
                report_fault: callbacks.fault,
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
        assert!(
            root.reply_route
                .set(establishment.handle().reply_route()?)
                .is_ok()
        );
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

    pub(super) fn from_primary(
        route: Route,
        mut config: SecondaryConfig,
        epoch: Epoch,
        transfer: QueueTransfer,
        provider: Arc<dyn StateProvider>,
        callbacks: SecondaryCallbacks,
        clock: Clock,
    ) -> Result<Self, SecondaryError> {
        config.copy.copy_done = true;
        Self::create(
            route,
            config,
            provider,
            callbacks,
            clock,
            Some((epoch, transfer)),
        )
    }

    pub(super) fn take_for_primary(&mut self) -> Result<QueueTransfer, SecondaryError> {
        let mut state = self.root.state.lock().unwrap();
        if let Some(fault) = &state.fault {
            return Err(fault.clone());
        }
        if state.phase != Phase::Closed
            || !state.promoting
            || state.transferred
            || !state
                .end
                .as_ref()
                .is_some_and(|end| end.result.is_ok() && end.streams_drained)
        {
            return Err(SecondaryError::InvalidTransfer);
        }
        let transfer = self.root.replication.lock().unwrap().take_for_primary()?;
        state.transferred = true;
        Ok(transfer)
    }

    pub(super) async fn wait_external_epoch(&mut self) -> Result<(), SecondaryError> {
        match self.drive(DriveUntil::ExternalEpoch).await? {
            None => Ok(()),
            Some(end) => end.result.and(Err(SecondaryError::Closed)),
        }
    }

    pub(super) async fn wait_for_active(&mut self) -> Result<(), SecondaryError> {
        if self.root.config.copy.require_service_ack {
            return Ok(());
        }
        match self.drive(DriveUntil::CopyDispatch).await? {
            None => Ok(()),
            Some(end) => end.result.and(Err(SecondaryError::Closed)),
        }
    }

    pub(super) async fn drain_context(&mut self) -> Result<(), SecondaryError> {
        if self.context_done || !self.root.state.lock().unwrap().context_active {
            return Ok(());
        }
        let result = Self::drive_context(
            &mut self.establishment,
            &self.root,
            self.send_context.clone(),
        )
        .await;
        self.context_done = true;
        self.root.state.lock().unwrap().context_result = Some(result.clone());
        match result {
            Ok(())
            | Err(EstablishError::Cancelled | EstablishError::Copy(CopyError::Cancelled)) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    async fn drive_context(
        establishment: &mut SecondaryEstablishment,
        root: &Root,
        send: Arc<dyn Fn(Route, CopySendAction) -> bool + Send + Sync>,
    ) -> Result<(), EstablishError> {
        let primary = establishment.handle().reply_route()?;
        let send = Arc::new(move |action| {
            let route = primary.lock().unwrap().clone();
            send(route, action)
        });
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
        Ok(self
            .drive(DriveUntil::Terminal)
            .await?
            .expect("ordinary drive returns only a terminal end"))
    }

    async fn drive(&mut self, until: DriveUntil) -> Result<Option<SecondaryEnd>, SecondaryError> {
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
                return Ok(Some(end));
            }
            if until != DriveUntil::Terminal {
                let mut state = root.state.lock().unwrap();
                if let Some(fault) = &state.fault {
                    return Err(fault.clone());
                }
                if until == DriveUntil::CopyDispatch && root.copy_dispatch.snapshot().drained {
                    return Ok(None);
                }
                if until == DriveUntil::ExternalEpoch
                    && state.external_epoch
                    && root.replication_dispatch.snapshot().drained
                    && root.replication.lock().unwrap().all_operations_acked()
                    && state.pending_epoch.is_none()
                    && state.running_epoch.is_none()
                {
                    state.external_epoch = false;
                    return Ok(None);
                }
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

mod ingress;
