// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation of RemoteSession.EstablishCopyAsyncOperation.cpp,
// RemoteSession.cpp:252-365,683-756, PrimaryReplicator.cpp:550-705,
// PrimaryReplicator.BuildIdleAsyncOperation.cpp:84-166,185-214 and
// SecondaryReplicator.cpp:915-1123, microsoft/service-fabric
// @3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d, Reliability/Replication/.

//! Private establishment/acquisition owners, not full BuildIdle or role owners.
//! A bound Route represents an already-admitted idle session, not a role lookup.
//! Callers drive acquisition, copy enumeration, context ACK timers and session
//! ACK handoffs explicitly. Acquiring/beginning copy is not completing a build.
//! Provider futures survive interrupted driver polls; dropping the owner is not
//! cancellation/drain. Handles are weak and callbacks execute outside guards.

#![cfg_attr(not(test), expect(dead_code, reason = "unselected V1 establishment"))]

use std::sync::{Arc, Mutex, Weak};

use futures::{StreamExt, future::BoxFuture, stream};
use tokio::sync::Notify;

use crate::application::{Lsn, OperationDataStream};
use crate::error::RuntimeError;
use crate::protocol::types::Epoch;
use crate::traits::StateProvider;

use super::async_runtime::Clock;
use super::buffers;
use super::copy_async_operation::{CopyConfig, CopyEnd, CopyError, CopySendAction};
use super::copy_context_receiver::{
    ContextObservation, CopyContextAck, CopyContextConfig, CopyContextError, CopyContextReceiver,
};
use super::copy_sender::{CopySender, CopySenderHandle};
use super::operation_queue::QueueOperation;
use super::received_operation::ReceivedOperation;
use super::secondary_receiver::{ReceiverConfig, ReceiverError, SecondaryReceiver};
use super::session::{AckProcessor, AckUpdate, ReplicationSession};
use super::transport::{ProtocolError, Route, wire};

type CopySend = Arc<dyn Fn(CopySendAction) -> bool + Send + Sync>;
type ContextAckSend = Arc<dyn Fn(CopyContextAck, bool) -> bool + Send + Sync>;
type Acquire = BoxFuture<'static, crate::Result<OperationDataStream>>;

#[derive(Clone, Debug, thiserror::Error)]
pub(super) enum EstablishError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Buffer(#[from] buffers::BufferError),
    #[error(transparent)]
    Context(#[from] CopyContextError),
    #[error(transparent)]
    Copy(#[from] CopyError),
    #[error("secondary replication initialization failed: {0}")]
    Receiver(Arc<ReceiverError>),
    #[error("provider {description} acquisition failed: {error}")]
    Provider {
        description: &'static str,
        error: Arc<RuntimeError>,
    },
    #[error("copy establishment has not started")]
    NotStarted,
    #[error("copy establishment may only start once")]
    AlreadyStarted,
    #[error("copy establishment was cancelled or closed")]
    Cancelled,
    #[error("copy establishment owner was dropped")]
    OwnerDropped,
    #[error("StartCopy belongs to another replica")]
    WrongReplica,
    #[error("StartCopy epoch precedes the established epoch")]
    StaleEpoch,
    #[error("secondary received StartCopy with higher epoch than expected")]
    UnexpectedEpoch,
    #[error("invalid StartCopy epoch or replication start")]
    InvalidStart,
    #[error("remote copy/context failure {0}")]
    Remote(i32),
}

fn inbound(route: &Route, envelope: &wire::Envelope) -> Result<(), EstablishError> {
    route.check(envelope, true)?;
    if envelope.version != 1 {
        return Err(ProtocolError::UnsupportedVersion(envelope.version).into());
    }
    if envelope.message_id.is_empty() {
        return Err(ProtocolError::Missing("message_id").into());
    }
    Ok(())
}

fn stream_error(error: CopyContextError) -> RuntimeError {
    RuntimeError::InvalidReplication(error.to_string())
}

struct PrimaryState {
    start: Option<Lsn>,
    established: Option<Result<(), EstablishError>>,
    closed: bool,
    context: Option<CopyContextReceiver>,
}

struct PrimaryRoot {
    state: Mutex<PrimaryState>,
    wake: Notify,
    route: Route,
    session: ReplicationSession,
    copy: CopySenderHandle,
    clock: Clock,
    config: CopyConfig,
}

impl PrimaryRoot {
    fn complete(&self, result: Result<(), EstablishError>) {
        {
            let mut state = self.state.lock().unwrap();
            if state.established.is_none() && !state.closed {
                state.established = Some(result);
            }
        }
        self.wake.notify_one();
    }

    fn cleanup_context(&self, error: i32) -> Result<(), EstablishError> {
        let effects = {
            let mut state = self.state.lock().unwrap();
            state.context.as_mut().map(|context| {
                // Native success cleanup cannot imply receipt of missing data.
                let error = if error == 0 && !context.all_operations_received() {
                    -1
                } else {
                    error
                };
                context.close(error, self.clock.now())
            })
        };
        if let Some(effects) = effects {
            effects.dispatch()?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct PrimaryEstablishmentHandle(Weak<PrimaryRoot>);

impl PrimaryEstablishmentHandle {
    fn root(&self) -> Result<Arc<PrimaryRoot>, EstablishError> {
        self.0.upgrade().ok_or(EstablishError::OwnerDropped)
    }

    pub(super) fn cancel(&self) -> Result<(), EstablishError> {
        let root = self.root()?;
        root.state.lock().unwrap().closed = true;
        root.cleanup_context(-1)?;
        root.copy.finish(CopyError::Cancelled)?;
        root.session.close_start_copy();
        root.wake.notify_one();
        Ok(())
    }

    pub(super) fn receive_context(
        &self,
        envelope: &wire::Envelope,
    ) -> Result<ContextObservation, EstablishError> {
        let root = self.root()?;
        inbound(&root.route, envelope)?;
        let Some(wire::envelope::Body::CopyContextOperation(body)) = &envelope.body else {
            return Err(ProtocolError::Shape("expected copy context").into());
        };
        let metadata = body
            .metadata
            .as_ref()
            .ok_or(ProtocolError::Missing("metadata"))?;
        let segments = &body
            .buffers
            .as_ref()
            .ok_or(ProtocolError::Missing("buffers"))?
            .segments;
        let data = if body.is_last {
            if buffers::payload_len(segments, root.config.buffer_limits)? != 0 {
                return Err(ProtocolError::Shape("nonempty context terminal").into());
            }
            crate::application::OperationData::new()
        } else {
            buffers::encode(
                segments,
                root.config.buffer_policy,
                root.config.buffer_limits,
            )?
        };
        let operation = ReceivedOperation::new(
            QueueOperation::new(metadata.sequence, data, true),
            Epoch::new(0, 0),
            false,
        );
        let effects = {
            let mut state = root.state.lock().unwrap();
            if state.closed {
                return Err(EstablishError::Cancelled);
            }
            if state.start.is_none() {
                return Err(EstablishError::NotStarted);
            }
            state
                .context
                .as_mut()
                .ok_or(ProtocolError::Shape("nonpersisted context"))?
                .process(operation, body.is_last, root.clock.now())
        };
        let observation = effects.dispatch()?;
        root.complete(Ok(()));
        Ok(observation)
    }

    /// Return the actual session processor; the caller must deliver its ordered
    /// primary/copy handoffs outside guards, not merely discard this obligation.
    pub(super) fn receive_ack(
        &self,
        envelope: &wire::Envelope,
    ) -> Result<Option<AckProcessor>, EstablishError> {
        let root = self.root()?;
        inbound(&root.route, envelope)?;
        let Some(wire::envelope::Body::ReplicationAck(body)) = &envelope.body else {
            return Err(ProtocolError::Shape("expected replication ACK").into());
        };
        if body.replication_received < 0
            || body.replication_applied < 0
            || body.replication_applied > body.replication_received
            || body.copy_received < -1
            || body.copy_applied < -1
        {
            return Err(ProtocolError::Shape("invalid ACK progress").into());
        }
        let persisted = {
            let state = root.state.lock().unwrap();
            if state.closed {
                return Err(EstablishError::Cancelled);
            }
            if state.start.is_none() {
                return Err(EstablishError::NotStarted);
            }
            state.context.is_some()
        };
        if body.error != 0 {
            root.complete(Err(EstablishError::Remote(body.error)));
            root.copy.finish(CopyError::Remote(body.error))?;
            root.cleanup_context(body.error)?;
            root.session.close_start_copy();
        } else if !persisted {
            root.complete(Ok(()));
        }
        Ok(root.session.update_ack(
            AckUpdate {
                replication_received: body.replication_received,
                replication_applied: body.replication_applied,
                copy_received: body.copy_received,
                copy_applied: body.copy_applied,
                process_copy: true,
                message_id: envelope.message_id.clone(),
            },
            None,
        ))
    }

    pub(super) fn context_ack_timer(&self) -> Result<(), EstablishError> {
        let root = self.root()?;
        let effects = root
            .state
            .lock()
            .unwrap()
            .context
            .as_mut()
            .map(|context| context.on_ack_timer(root.clock.now()));
        if let Some(effects) = effects {
            effects.dispatch()?;
        }
        Ok(())
    }

    pub(super) fn context_ack_deadline(
        &self,
    ) -> Result<Option<std::time::Duration>, EstablishError> {
        let root = self.root()?;
        let deadline = root
            .state
            .lock()
            .unwrap()
            .context
            .as_ref()
            .and_then(CopyContextReceiver::ack_deadline);
        Ok(deadline)
    }
}

pub(super) struct PrimaryEstablishment {
    root: Arc<PrimaryRoot>,
    context: Option<OperationDataStream>,
    acquisition: Option<Acquire>,
    acquisition_result: Option<Result<(), EstablishError>>,
    provider: Arc<dyn StateProvider>,
    copy: CopySender,
}

impl PrimaryEstablishment {
    pub(super) fn new(
        route: Route,
        session: ReplicationSession,
        config: CopyConfig,
        context_config: Option<CopyContextConfig>,
        provider: Arc<dyn StateProvider>,
        send_context_ack: ContextAckSend,
        clock: Clock,
    ) -> Result<Self, EstablishError> {
        route.validate()?;
        config.validate()?;
        let (context_receiver, context): (_, OperationDataStream) =
            if let Some(config) = context_config {
                let mut receiver = CopyContextReceiver::new(config)?;
                receiver.open(&send_context_ack, clock.now())?;
                let context = Box::pin(receiver.stream().map(|item| item.map_err(stream_error)));
                (Some(receiver), context)
            } else {
                (None, Box::pin(stream::empty()))
            };
        let copy = CopySender::new(true);
        Ok(Self {
            root: Arc::new(PrimaryRoot {
                state: Mutex::new(PrimaryState {
                    start: None,
                    established: None,
                    closed: false,
                    context: context_receiver,
                }),
                wake: Notify::new(),
                route,
                session,
                copy: copy.handle(),
                clock,
                config,
            }),
            context: Some(context),
            acquisition: None,
            acquisition_result: None,
            provider,
            copy,
        })
    }

    pub(super) fn handle(&self) -> PrimaryEstablishmentHandle {
        PrimaryEstablishmentHandle(Arc::downgrade(&self.root))
    }

    /// Caller supplies the frozen classic committed boundary + 1, not latest
    /// issued progress or the EOF replication completion target.
    pub(super) fn begin(&mut self, replication_start: Lsn) -> Result<(), EstablishError> {
        if replication_start <= 0 || replication_start == Lsn::MAX {
            return Err(EstablishError::InvalidStart);
        }
        let batch = {
            let mut state = self.root.state.lock().unwrap();
            if state.closed {
                return Err(EstablishError::Cancelled);
            }
            if state.start.is_some() {
                return Err(EstablishError::AlreadyStarted);
            }
            state.start = Some(replication_start);
            self.root.session.start_copy(
                self.root.config.sender,
                replication_start,
                self.root.clock.now(),
            )
        };
        batch.dispatch();
        Ok(())
    }

    /// Only acquires the provider stream and begins the actual CopySender.
    /// The caller must subsequently drive copy and deliver its ACK handoffs.
    pub(super) async fn acquire_copy(
        &mut self,
        send: CopySend,
        latest_issued: Arc<dyn Fn() -> crate::Result<Lsn> + Send + Sync>,
    ) -> Result<(), EstablishError> {
        if let Some(result) = &self.acquisition_result {
            return result.clone();
        }
        let start = loop {
            let wake = self.root.wake.notified();
            {
                let state = self.root.state.lock().unwrap();
                if state.closed && self.acquisition.is_none() {
                    return Err(EstablishError::Cancelled);
                }
                let start = state.start.ok_or(EstablishError::NotStarted)?;
                if let Some(result) = &state.established {
                    result.clone()?;
                    break start;
                }
            }
            let deadline = self.root.session.start_copy_deadline();
            tokio::select! {
                () = wake => {}
                () = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep(deadline.saturating_sub(self.root.clock.now())).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(batch) = self.root.session.on_start_copy_timer(self.root.clock.now()) {
                        batch.dispatch();
                    }
                }
            }
        };
        if self.acquisition.is_none() {
            let provider = self.provider.clone();
            let context = self.context.take().expect("single provider acquisition");
            self.acquisition = Some(Box::pin(async move {
                provider.get_copy_state(start - 1, context).await
            }));
        }
        let result = self.acquisition.as_mut().unwrap().await;
        self.acquisition = None;
        let result = match result {
            Err(error) => Err(EstablishError::Provider {
                description: "GetCopyState",
                error: Arc::new(error),
            }),
            Ok(provider) => {
                let closed = self.root.state.lock().unwrap().closed;
                if closed {
                    Err(EstablishError::Cancelled)
                } else {
                    let root = self.root.clone();
                    let send = Arc::new(move |action| {
                        let _root = &root;
                        send(action)
                    });
                    self.copy
                        .begin(
                            self.root.config,
                            provider,
                            send,
                            latest_issued,
                            self.root.clock.clone(),
                        )
                        .map_err(EstablishError::from)
                }
            }
        };
        if result.is_err() {
            self.root.cleanup_context(-1)?;
            self.root.session.close_start_copy();
        }
        self.acquisition_result = Some(result.clone());
        result
    }

    pub(super) fn copy(&mut self) -> &mut CopySender {
        &mut self.copy
    }

    pub(super) fn end_copy(&self) -> Result<CopyEnd, EstablishError> {
        let end = self.copy.handle().end()?;
        self.root.session.close_start_copy();
        self.root
            .cleanup_context(if end.result.is_ok() { 0 } else { -1 })?;
        Ok(end)
    }
}

struct SecondaryState {
    active: bool,
    epoch: Option<Epoch>,
    receiver: SecondaryReceiver,
    error: i32,
}

struct SecondaryRoot {
    state: Mutex<SecondaryState>,
    route: Route,
    copy: CopySenderHandle,
    force_ack: Arc<dyn Fn(i32) + Send + Sync>,
}

#[derive(Clone)]
pub(super) struct SecondaryEstablishmentHandle(Weak<SecondaryRoot>);

impl SecondaryEstablishmentHandle {
    fn root(&self) -> Result<Arc<SecondaryRoot>, EstablishError> {
        self.0.upgrade().ok_or(EstablishError::OwnerDropped)
    }

    /// Inactive admission containment only, not receiver drain or role Close.
    pub(super) fn close(&self) -> Result<(), EstablishError> {
        let root = self.root()?;
        root.state.lock().unwrap().active = false;
        root.copy.finish(CopyError::Cancelled)?;
        Ok(())
    }

    pub(super) fn process_start(&self, envelope: &wire::Envelope) -> Result<bool, EstablishError> {
        let root = self.root()?;
        inbound(&root.route, envelope)?;
        let Some(wire::envelope::Body::StartCopy(body)) = &envelope.body else {
            return Err(ProtocolError::Shape("expected StartCopy").into());
        };
        if body.replica != root.route.source.replica {
            return Err(EstablishError::WrongReplica);
        }
        let epoch = body
            .epoch
            .as_ref()
            .ok_or(ProtocolError::Missing("StartCopy epoch"))?;
        let epoch = Epoch::new(epoch.data_loss, epoch.configuration);
        if epoch.data_loss_number < 0
            || epoch.configuration_number < 0
            || body.replication_start <= 0
            || body.replication_start == Lsn::MAX
        {
            return Err(EstablishError::InvalidStart);
        }
        let (initialized, error) = {
            let mut state = root.state.lock().unwrap();
            if !state.active {
                return Err(EstablishError::Cancelled);
            }
            if let Some(established) = state.epoch {
                if epoch < established {
                    return Err(EstablishError::StaleEpoch);
                }
                if epoch > established {
                    return Err(EstablishError::UnexpectedEpoch);
                }
                (false, state.error)
            } else {
                state
                    .receiver
                    .initialize(body.replication_start, epoch)
                    .map_err(|error| EstablishError::Receiver(Arc::new(error)))?;
                state.epoch = Some(epoch);
                (true, state.error)
            }
        };
        (root.force_ack)(error);
        Ok(initialized)
    }

    pub(super) fn process_context_ack(
        &self,
        envelope: &wire::Envelope,
    ) -> Result<(), EstablishError> {
        let root = self.root()?;
        inbound(&root.route, envelope)?;
        let Some(wire::envelope::Body::CopyContextAck(body)) = &envelope.body else {
            return Err(ProtocolError::Shape("expected context ACK").into());
        };
        {
            let state = root.state.lock().unwrap();
            if !state.active {
                return Err(EstablishError::Cancelled);
            }
            if state.epoch.is_none() {
                return Err(EstablishError::NotStarted);
            }
        }
        if body.error != 0 {
            root.copy.finish(CopyError::Remote(body.error))?;
            return Err(EstablishError::Remote(body.error));
        }
        root.copy.process_copy_ack(body.sequence, body.sequence)?;
        Ok(())
    }
}

pub(super) struct SecondaryEstablishment {
    root: Arc<SecondaryRoot>,
    provider: Arc<dyn StateProvider>,
    persisted: bool,
    config: CopyConfig,
    clock: Clock,
    acquisition: Option<Acquire>,
    acquisition_result: Option<Result<(), EstablishError>>,
    copy: CopySender,
}

impl SecondaryEstablishment {
    pub(super) fn new(
        route: Route,
        receiver: ReceiverConfig,
        config: CopyConfig,
        provider: Arc<dyn StateProvider>,
        force_ack: Arc<dyn Fn(i32) + Send + Sync>,
        clock: Clock,
    ) -> Result<Self, EstablishError> {
        route.validate()?;
        config.validate()?;
        let copy = CopySender::new(false);
        Ok(Self {
            root: Arc::new(SecondaryRoot {
                state: Mutex::new(SecondaryState {
                    active: true,
                    epoch: None,
                    receiver: SecondaryReceiver::new(receiver),
                    error: 0,
                }),
                route,
                copy: copy.handle(),
                force_ack,
            }),
            persisted: receiver.has_persisted_state,
            provider,
            config,
            clock,
            acquisition: None,
            acquisition_result: None,
            copy,
        })
    }

    pub(super) fn handle(&self) -> SecondaryEstablishmentHandle {
        SecondaryEstablishmentHandle(Arc::downgrade(&self.root))
    }

    pub(super) async fn acquire_context(&mut self, send: CopySend) -> Result<(), EstablishError> {
        if let Some(result) = &self.acquisition_result {
            return result.clone();
        }
        {
            let state = self.root.state.lock().unwrap();
            if !state.active && self.acquisition.is_none() {
                return Err(EstablishError::Cancelled);
            }
            if state.epoch.is_none() {
                return Err(EstablishError::NotStarted);
            }
        }
        if !self.persisted {
            self.acquisition_result = Some(Ok(()));
            return Ok(());
        }
        if self.acquisition.is_none() {
            let provider = self.provider.clone();
            self.acquisition = Some(Box::pin(async move { provider.get_copy_context().await }));
        }
        let result = self.acquisition.as_mut().unwrap().await;
        self.acquisition = None;
        let result = match result {
            Err(error) => {
                let active = {
                    let mut state = self.root.state.lock().unwrap();
                    state.error = -1; // Private error token, not an HRESULT mapping.
                    state.active
                };
                if active {
                    (self.root.force_ack)(-1);
                }
                Err(EstablishError::Provider {
                    description: "GetCopyContext",
                    error: Arc::new(error),
                })
            }
            Ok(provider) => {
                let active = self.root.state.lock().unwrap().active;
                if !active {
                    Err(EstablishError::Cancelled)
                } else {
                    let root = self.root.clone();
                    let send = Arc::new(move |action| {
                        let _root = &root;
                        send(action)
                    });
                    self.copy
                        .begin(
                            self.config,
                            provider,
                            send,
                            Arc::new(|| Ok(0)),
                            self.clock.clone(),
                        )
                        .map_err(EstablishError::from)
                }
            }
        };
        self.acquisition_result = Some(result.clone());
        result
    }

    pub(super) fn copy(&mut self) -> &mut CopySender {
        &mut self.copy
    }
}

#[cfg(test)]
mod tests;
