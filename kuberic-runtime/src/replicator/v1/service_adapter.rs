// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided facade composition from ComReplicator.cpp:219-313,
// Replicator.cpp:296-562,889-907, Replicator.ReplicateAsyncOperation.cpp:31-98,
// ComReplicator.ReplicateOperation.cpp:23-74 and
// ComOperationStream.GetOperationOperation.cpp:23-72, microsoft/service-fabric
// @3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d, Reliability/Replication.

//! Private, unselected service adapter. The caller retains and drives this
//! owner; no task or public factory is installed. Dropped control futures leave
//! their selected operation in the owner: call `resume`, not Begin again.
//! Assignment/quorum completion never commits service state or grants access.
//! The injected access callback is a host seam, not a serving-status policy.
//! Drop is abnormal abort containment, not provider/build drainage or Close.

#![cfg_attr(not(test), expect(dead_code, reason = "unselected V1 service adapter"))]

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;

use crate::application::{Lsn, OperationData};
use crate::protocol::types::Epoch;
use crate::traits::StateProvider;

use super::async_runtime::Clock;
use super::buffers::{self, BufferError, BufferLimits, BufferPolicy};
use super::build_idle::{BuildCallbacks, BuildConfig, BuildError, BuildIdleHandle};
use super::ingress::{MessageRegistry, PublishedEndpoint};
use super::operation_stream::{OperationStream, StreamError, StreamEvent};
use super::primary::{PrimaryError, PrimaryHandle, ReplicaConfiguration};
use super::received_operation::{AckError, ReceivedOperation};
use super::replica_manager::{CatchupMode, CompletionError, CompletionReceiver};
use super::role::{DataLossEnd, Role, RoleConfig, RoleError, RoleReplicator, RoleStatus};
use super::secondary::{SecondaryError, SecondaryHandle};

mod facade;
mod production;
mod production_transport;

pub(crate) use production::create as create_production;
pub(crate) use production_transport::{PUBLIC_V1_DATA_SERVICES, PublicV1DataServiceManifest};

pub(crate) use facade::{Facade, FacadeSetup, FacadeState, matches_binding};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LifecycleObservation {
    Opened,
    Ready(crate::protocol::types::ReplicaRole),
    Closed,
    Faulted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WriteAccess {
    Granted,
    ReconfigurationPending,
    NoWriteQuorum,
    NotPrimary,
}

#[derive(Clone, Debug, thiserror::Error)]
pub(super) enum AdapterError {
    #[error("service adapter owner was dropped")]
    OwnerDropped,
    #[error("service operation is not admitted in {0:?}")]
    InvalidState(RoleStatus),
    #[error("partition write access: {0:?}")]
    WriteAccess(WriteAccess),
    #[error("selected operation completion: {0:?}")]
    Completion(CompletionError),
    #[error("selected operation completion sender was abandoned")]
    CompletionDropped,
    #[error(transparent)]
    Role(#[from] RoleError),
    #[error(transparent)]
    Primary(#[from] PrimaryError),
    #[error(transparent)]
    Secondary(#[from] SecondaryError),
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error(transparent)]
    Buffer(#[from] BufferError),
    #[error(transparent)]
    Stream(#[from] StreamError),
    #[error(transparent)]
    Ack(#[from] AckError),
}

#[derive(Clone)]
enum Target {
    Unavailable(RoleStatus),
    Primary(PrimaryHandle),
    Secondary(SecondaryHandle),
}

pub(super) struct ServiceReplicator {
    owner: RoleReplicator,
    target: Arc<Mutex<Target>>,
    service: ServiceHandle,
    provider: Arc<dyn StateProvider>,
    registry: MessageRegistry,
    control: Option<Control>,
    observe: Option<Arc<dyn Fn(LifecycleObservation) + Send + Sync>>,
}

#[derive(Clone, Copy)]
enum Control {
    ChangeRole,
    UpdateEpoch,
    Progress,
    DataLoss,
    Close,
    Abort,
}

#[derive(Clone, Debug)]
pub(super) enum ControlResult {
    Completed,
    Progress(Lsn),
    DataLoss(DataLossEnd),
}

impl ServiceReplicator {
    pub(super) fn new(
        config: RoleConfig,
        provider: Arc<dyn StateProvider>,
        clock: Clock,
        registry: MessageRegistry,
        write_access: Arc<dyn Fn() -> WriteAccess + Send + Sync>,
    ) -> Result<Self, AdapterError> {
        let target = Arc::new(Mutex::new(Target::Unavailable(RoleStatus::Created)));
        let service = ServiceHandle {
            target: Arc::downgrade(&target),
            aborted: Arc::new(AtomicBool::new(false)),
            policy: config.secondary.context.buffer_policy,
            limits: config.secondary.context.buffer_limits,
            write_access: Arc::new(Mutex::new(write_access)),
            write_admission: None,
            #[cfg(test)]
            before_assignment: None,
        };
        Ok(Self {
            owner: RoleReplicator::new(config, provider.clone(), clock)?,
            target,
            service,
            provider,
            registry,
            control: None,
            observe: None,
        })
    }

    pub(super) fn service(&self) -> ServiceHandle {
        self.service.clone()
    }

    pub(super) fn status(&self) -> RoleStatus {
        self.owner.status()
    }

    fn refresh(&self) {
        if let Some(observe) = &self.observe {
            use crate::protocol::types::ReplicaRole;
            let observation = match self.owner.status() {
                RoleStatus::Opened => Some(LifecycleObservation::Opened),
                RoleStatus::Ready(role) => Some(LifecycleObservation::Ready(match role {
                    Role::None => ReplicaRole::None,
                    Role::Primary => ReplicaRole::Primary,
                    Role::IdleSecondary => ReplicaRole::IdleSecondary,
                    Role::ActiveSecondary => ReplicaRole::ActiveSecondary,
                })),
                RoleStatus::Closed | RoleStatus::Aborted => Some(LifecycleObservation::Closed),
                RoleStatus::Faulted => Some(LifecycleObservation::Faulted),
                _ => None,
            };
            if let Some(observation) = observation {
                observe(observation);
            }
        }
        let target = if self.service.aborted.load(Ordering::Acquire) {
            Target::Unavailable(RoleStatus::Aborting)
        } else if let Ok(primary) = self.owner.primary() {
            Target::Primary(primary)
        } else if let Ok(secondary) = self.owner.secondary() {
            Target::Secondary(secondary)
        } else {
            Target::Unavailable(self.owner.status())
        };
        *self.target.lock().unwrap() = target;
    }

    fn select(
        &mut self,
        control: Control,
        begin: impl FnOnce(&mut RoleReplicator) -> Result<(), RoleError>,
    ) -> Result<(), AdapterError> {
        if self.control.is_some() {
            return Err(RoleError::Busy.into());
        }
        let result = begin(&mut self.owner);
        self.refresh();
        result?;
        self.control = Some(control);
        Ok(())
    }

    pub(super) async fn open(&mut self) -> Result<PublishedEndpoint, AdapterError> {
        let result = self.owner.open(&self.registry);
        self.refresh();
        Ok(result?)
    }

    pub(super) async fn change_role(
        &mut self,
        epoch: Epoch,
        role: Role,
    ) -> Result<(), AdapterError> {
        self.select(Control::ChangeRole, |owner| {
            owner.begin_change_role(role, epoch)
        })?;
        self.resume().await.map(|_| ())
    }

    pub(super) async fn update_epoch(&mut self, epoch: Epoch) -> Result<(), AdapterError> {
        self.select(Control::UpdateEpoch, |owner| {
            owner.begin_update_epoch(epoch)
        })?;
        self.resume().await.map(|_| ())
    }

    pub(super) async fn current_progress(&mut self) -> Result<Lsn, AdapterError> {
        if self.control.is_some() {
            return Err(RoleError::Busy.into());
        }
        if self.status() == RoleStatus::Opened {
            self.select(Control::Progress, RoleReplicator::begin_get_opened_progress)?;
            match self.resume().await? {
                ControlResult::Progress(progress) => Ok(progress),
                _ => unreachable!("selected progress query"),
            }
        } else {
            match self.service.target()? {
                Target::Primary(primary) => Ok(primary.current_progress()?),
                Target::Secondary(secondary) => Ok(secondary.current_progress()?),
                Target::Unavailable(status) => Err(AdapterError::InvalidState(status)),
            }
        }
    }

    pub(super) fn catchup_capability(&self) -> Result<Lsn, AdapterError> {
        match self.service.target()? {
            Target::Primary(primary) => Ok(primary.catchup_capability()?),
            Target::Secondary(secondary) => Ok(secondary.catchup_capability()?),
            Target::Unavailable(RoleStatus::Opened) => Ok(0),
            Target::Unavailable(status) => Err(AdapterError::InvalidState(status)),
        }
    }

    pub(super) async fn on_data_loss(&mut self) -> Result<DataLossEnd, AdapterError> {
        self.select(Control::DataLoss, RoleReplicator::begin_data_loss)?;
        match self.resume().await? {
            ControlResult::DataLoss(end) => Ok(end),
            _ => unreachable!("selected data loss"),
        }
    }

    pub(super) async fn close(&mut self, quorum_wait: Duration) -> Result<(), AdapterError> {
        self.select(Control::Close, |owner| owner.begin_close(quorum_wait))?;
        self.resume().await.map(|_| ())
    }

    pub(super) fn abort(&mut self) -> Result<(), AdapterError> {
        let result = self.owner.abort();
        self.refresh();
        result?;
        self.control = Some(Control::Abort);
        Ok(())
    }

    /// Finish a previously selected control call, including one whose waiter
    /// was dropped. Data-loss preserves its changed flag even on End failure.
    pub(super) async fn resume(&mut self) -> Result<ControlResult, AdapterError> {
        let control = self.control.ok_or(RoleError::NoOperation)?;
        let result = if self.owner.operation_pending() {
            self.owner.run().await
        } else {
            Ok(())
        };
        self.control = None;
        self.refresh();
        if matches!(control, Control::DataLoss) {
            return Ok(ControlResult::DataLoss(
                self.owner
                    .data_loss_result()
                    .expect("data-loss End")
                    .clone(),
            ));
        }
        if matches!(control, Control::Abort) {
            result?;
            return Err(RoleError::Aborted.into());
        }
        result?;
        Ok(if matches!(control, Control::Progress) {
            ControlResult::Progress(
                self.owner
                    .opened_progress_result()
                    .expect("progress End")
                    .clone()?,
            )
        } else {
            ControlResult::Completed
        })
    }

    /// Drive established timers, provider barriers and builds between control
    /// calls. Dropping this future retains selected work in the actual owners.
    pub(super) async fn drive(&mut self) -> Result<(), AdapterError> {
        if self.control.is_some() {
            return Err(RoleError::Busy.into());
        }
        let result = self.owner.drive().await;
        self.refresh();
        Ok(result?)
    }

    pub(super) fn update_catchup_configuration(
        &self,
        previous: Option<ReplicaConfiguration>,
        current: ReplicaConfiguration,
    ) -> Result<Vec<i64>, AdapterError> {
        Ok(self
            .owner
            .primary()?
            .update_catchup_configuration(previous, current)?)
    }

    pub(super) fn update_current_configuration(
        &self,
        current: ReplicaConfiguration,
    ) -> Result<Vec<i64>, AdapterError> {
        Ok(self
            .owner
            .primary()?
            .update_current_configuration(current)?)
    }

    pub(super) fn wait_for_catchup(&self, mode: CatchupMode) -> Result<Completion, AdapterError> {
        Ok(Completion::new(self.owner.primary()?.begin_catchup(mode)?))
    }

    pub(super) fn build_replica(
        &self,
        config: BuildConfig,
        callbacks: BuildCallbacks,
    ) -> Result<BuildOperation, AdapterError> {
        Ok(BuildOperation(self.owner.primary()?.begin_build(
            config,
            self.provider.clone(),
            callbacks,
        )?))
    }

    pub(super) fn remove_replica(&self, id: i64) -> Result<(), AdapterError> {
        Ok(self.owner.primary()?.remove_idle(id)?)
    }

    pub(super) async fn drain_retired_builds(&mut self) -> Result<(), AdapterError> {
        Ok(self.owner.drain_retired_builds().await?)
    }
}

impl Drop for ServiceReplicator {
    fn drop(&mut self) {
        if !matches!(self.status(), RoleStatus::Closed | RoleStatus::Aborted)
            && let Err(error) = self.owner.abort()
        {
            tracing::warn!(%error, "abandoned private V1 adapter abort failed");
        }
    }
}

#[derive(Clone)]
pub(super) struct ServiceHandle {
    target: Weak<Mutex<Target>>,
    aborted: Arc<AtomicBool>,
    policy: BufferPolicy,
    limits: BufferLimits,
    write_access: Arc<Mutex<Arc<dyn Fn() -> WriteAccess + Send + Sync>>>,
    write_admission: Option<Arc<Mutex<()>>>,
    #[cfg(test)]
    before_assignment: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl ServiceHandle {
    pub(super) fn idle_replica_ids(&self) -> Result<Vec<i64>, AdapterError> {
        match self.target()? {
            Target::Primary(primary) => Ok(primary.idle_replica_ids()?),
            Target::Secondary(_)
            | Target::Unavailable(
                RoleStatus::Opened | RoleStatus::Created | RoleStatus::Ready(Role::None),
            ) => Ok(Vec::new()),
            Target::Unavailable(status) => Err(AdapterError::InvalidState(status)),
        }
    }

    fn target(&self) -> Result<Target, AdapterError> {
        if self.aborted.load(Ordering::Acquire) {
            return Err(RoleError::Aborted.into());
        }
        let target = self.target.upgrade().ok_or(AdapterError::OwnerDropped)?;
        Ok(target.lock().unwrap().clone())
    }

    pub(super) fn begin_replicate(&self, data: OperationData) -> Result<Replication, AdapterError> {
        buffers::decode(data.clone(), self.policy, self.limits)?;
        let admission = self
            .write_admission
            .as_ref()
            .map(|gate| gate.lock().unwrap());
        let access = self.write_access.lock().unwrap().clone();
        match access() {
            WriteAccess::Granted => {}
            denied => return Err(AdapterError::WriteAccess(denied)),
        }
        #[cfg(test)]
        if let Some(pause) = &self.before_assignment {
            pause();
        }

        match self.target()? {
            Target::Primary(primary) => {
                let admitted =
                    primary.enqueue_with_lease(data, self.policy, self.limits, admission)?;
                Ok(Replication {
                    lsn: admitted.lsn,
                    completion: Completion::new(admitted.completion),
                })
            }
            Target::Secondary(_) => Err(AdapterError::WriteAccess(WriteAccess::NotPrimary)),
            Target::Unavailable(status) => Err(AdapterError::InvalidState(status)),
        }
    }

    pub(super) fn progress_query_readiness(&self) -> Result<bool, AdapterError> {
        match self.target()? {
            Target::Secondary(secondary) => Ok(secondary.progress_query_readiness()?),
            Target::Primary(_) | Target::Unavailable(_) => Ok(true),
        }
    }

    fn stream(&self, kind: StreamKind) -> Result<ServiceStream, AdapterError> {
        let secondary = match self.target()? {
            Target::Secondary(secondary) => secondary,
            Target::Primary(_) => {
                return Err(AdapterError::InvalidState(RoleStatus::Ready(Role::Primary)));
            }
            Target::Unavailable(status) => return Err(AdapterError::InvalidState(status)),
        };
        let stream = match kind {
            StreamKind::Copy => secondary.copy_stream()?,
            StreamKind::Replication => secondary.replication_stream()?,
        };
        Ok(ServiceStream {
            stream,
            secondary,
            kind,
            terminated: false,
        })
    }

    pub(super) fn copy_stream(&self) -> Result<ServiceStream, AdapterError> {
        self.stream(StreamKind::Copy)
    }

    pub(super) fn replication_stream(&self) -> Result<ServiceStream, AdapterError> {
        self.stream(StreamKind::Replication)
    }
}

#[must_use = "assignment is not quorum completion or service-local commit"]
pub(super) struct Replication {
    pub(super) lsn: Lsn,
    pub(super) completion: Completion,
}

#[must_use = "dropping a completion waiter does not retract admitted work"]
pub(super) struct Completion {
    receiver: CompletionReceiver,
    result: Option<Result<(), AdapterError>>,
}

impl Completion {
    fn new(receiver: CompletionReceiver) -> Self {
        Self {
            receiver,
            result: None,
        }
    }

    pub(super) async fn wait(&mut self) -> Result<(), AdapterError> {
        if self.result.is_none() {
            self.result = Some(
                (&mut self.receiver)
                    .await
                    .map_err(|_| AdapterError::CompletionDropped)
                    .and_then(|result| result.map_err(AdapterError::Completion)),
            );
        }
        self.result.as_ref().unwrap().clone()
    }
}

#[must_use = "the adapter owner must drive the actual build until End"]
pub(super) struct BuildOperation(BuildIdleHandle);

impl BuildOperation {
    pub(super) async fn wait(&self) -> Result<(), AdapterError> {
        Ok(self.0.wait().await?.result?)
    }

    pub(super) fn cancel(&self) -> Result<(), AdapterError> {
        self.0.cancel().map_err(BuildError::from)?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum StreamKind {
    Copy,
    Replication,
}

pub(super) struct ServiceStream {
    stream: OperationStream,
    secondary: SecondaryHandle,
    kind: StreamKind,
    terminated: bool,
}

impl ServiceStream {
    pub(super) fn report_fault(&self, message: String) -> Result<(), AdapterError> {
        Ok(self.secondary.report_stream_fault(message)?)
    }
}

impl Stream for ServiceStream {
    type Item = Result<ServiceOperation, AdapterError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.terminated {
            return Poll::Ready(None);
        }
        loop {
            match Pin::new(&mut this.stream).poll_next(cx) {
                Poll::Ready(Some(Ok(StreamEvent::Operation(operation)))) => {
                    return Poll::Ready(Some(Ok(ServiceOperation {
                        operation,
                        secondary: this.secondary.clone(),
                        kind: this.kind,
                    })));
                }
                Poll::Ready(Some(Ok(StreamEvent::UpdateEpoch(barrier)))) => {
                    if let Err(error) = this.secondary.update_stream_epoch(barrier) {
                        this.terminated = true;
                        return Poll::Ready(Some(Err(error.into())));
                    }
                }
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(error.into()))),
                Poll::Ready(None) => {
                    this.terminated = true;
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct ServiceOperation {
    operation: ReceivedOperation,
    secondary: SecondaryHandle,
    kind: StreamKind,
}

impl ServiceOperation {
    pub(super) fn lsn(&self) -> Lsn {
        self.operation.queue_operation().lsn()
    }

    pub(super) fn data(&self) -> &OperationData {
        self.operation.queue_operation().data()
    }

    pub(super) fn epoch(&self) -> Epoch {
        self.operation.epoch()
    }

    pub(super) fn is_end_of_stream(&self) -> bool {
        self.operation.is_end_of_stream()
    }

    pub(super) fn acknowledge(&self) -> Result<(), AdapterError> {
        if let Some(work) = self.operation.acknowledge()? {
            match self.kind {
                StreamKind::Copy => self.secondary.acknowledge_copy(work)?,
                StreamKind::Replication => self.secondary.acknowledge_replication(work)?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
