// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation of ReplicatorState.cpp:39-126,
// Replicator.ChangeRoleAsyncOperation.cpp:37-514 and
// Replicator.UpdateEpochAsyncOperation.cpp:34-145, microsoft/service-fabric
// @3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d, Reliability/Replication.
// Data loss: Replicator.OnDatalossAsyncOperation.cpp:30-157,
// ReplicatorState.cpp:129-139,177-186, ReplicationQueueManager.cpp:148-160.
// Open/recovery: Replicator.OpenAsyncOperation.cpp:29-81,
// Replicator.cpp:296-367, ComProxyStateProvider.cpp:63-79,
// Replicator.CloseAsyncOperation.cpp:79-100, Replicator.cpp:178-253.

//! Private, explicitly driven role/epoch/recovery owner with typed ingress, not
//! a public facade or production host. A cancelled run retains selected work.
//! Old primary build providers stay rooted separately; role completion is not
//! arbitrary GetCopyState/GetNext drainage. No task or engine is activated.

#![cfg_attr(not(test), expect(dead_code, reason = "unselected V1 role owner"))]

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;

use crate::application::Lsn;
use crate::error::RuntimeError;
use crate::protocol::types::Epoch;
use crate::traits::StateProvider;

use super::async_runtime::Clock;
use super::ingress::{
    MessageIngress, MessageRegistration, MessageRegistry, PublishedEndpoint, RegistrationError,
    Target,
};
use super::primary::{Primary, PrimaryConfig, PrimaryError, PrimaryHandle};
use super::secondary::{
    RetainedCheckpoint, RetainedProgress, Secondary, SecondaryCallbacks, SecondaryConfig,
    SecondaryError, SecondaryHandle,
};
use super::transport::Route;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    None,
    Primary,
    IdleSecondary,
    ActiveSecondary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RoleStatus {
    Created,
    Opened,
    Changing(Role),
    Ready(Role),
    CheckingDataLoss,
    Closing(Role),
    Closed,
    Faulted,
    Aborting,
    Aborted,
}

#[derive(Clone, Debug, thiserror::Error)]
pub(super) enum RoleError {
    #[error("role/epoch/recovery operation already outstanding")]
    Busy,
    #[error("role transition is not admitted from {0:?} to {1:?}")]
    InvalidTransition(RoleStatus, Role),
    #[error("role owner has no admitted role/epoch/recovery operation")]
    NoOperation,
    #[error("role owner is not in the requested stable role")]
    InvalidState,
    #[error("inconsistent local identity or invalid role-owner settings")]
    InvalidConfig,
    #[error("role owner was aborted")]
    Aborted,
    #[error("StateProvider role/recovery callback: {0}")]
    Provider(Arc<RuntimeError>),
    #[error(transparent)]
    Registration(#[from] RegistrationError),
    #[error(transparent)]
    Primary(#[from] PrimaryError),
    #[error(transparent)]
    Secondary(#[from] SecondaryError),
}

pub(super) struct RoleConfig {
    pub(super) primary: PrimaryConfig,
    pub(super) secondary: SecondaryConfig,
    /// Local endpoint is source, as for Secondary::new.
    pub(super) secondary_route: Route,
    pub(super) secondary_callbacks: SecondaryCallbacks,
    pub(super) cached_progress: Option<Lsn>,
}

#[derive(Clone, Debug)]
pub(super) struct DataLossEnd {
    pub(super) state_changed: bool,
    pub(super) result: Result<(), RoleError>,
}

struct DataLossRecovery {
    state_changed: bool,
    progress: crate::Result<Option<Lsn>>,
}

#[derive(Clone, Copy)]
enum Step {
    OpenedProgress,
    InitialPrimary(Epoch),
    RetainedProgress,
    RestoreRetainedSecondary,
    Promote(Epoch),
    Demote(Epoch),
    ActivateSecondary,
    Close(Role),
    DirectClose(Role),
    PrimaryEpoch(Option<Role>),
    SecondaryEpoch,
    DataLoss,
    Abort,
}

pub(super) struct RoleReplicator {
    config: RoleConfig,
    provider: Arc<dyn StateProvider>,
    clock: Clock,
    status: RoleStatus,
    pending: Option<Step>,
    provider_call: Option<BoxFuture<'static, crate::Result<Lsn>>>,
    retained_progress_call: Option<BoxFuture<'static, Result<RetainedProgress, SecondaryError>>>,
    retained_call: Option<BoxFuture<'static, Result<RetainedCheckpoint, SecondaryError>>>,
    opened_progress: Option<Result<Lsn, RoleError>>,
    data_loss_call: Option<BoxFuture<'static, DataLossRecovery>>,
    data_loss_failure: Option<RoleError>,
    data_loss_end: Option<DataLossEnd>,
    primary: Option<Primary>,
    secondary: Option<Secondary>,
    retired_primaries: Vec<Primary>,
    retired_secondaries: Vec<Secondary>,
    fault: Option<RoleError>,
    ingress: MessageIngress,
    registration: Option<MessageRegistration>,
}

async fn read_provider_progress(provider: &dyn StateProvider) -> crate::Result<Lsn> {
    // Native ComProxyStateProvider treats every successful negative LSN as empty.
    provider.last_committed_lsn().await.map(|lsn| lsn.max(0))
}

impl RoleReplicator {
    pub(super) fn new(
        config: RoleConfig,
        provider: Arc<dyn StateProvider>,
        clock: Clock,
    ) -> Result<Self, RoleError> {
        config
            .secondary_route
            .validate()
            .map_err(PrimaryError::from)?;
        if config.primary.endpoint != config.secondary_route.source
            || config.primary.address != config.secondary_route.sender_address
            || !config.primary.queue.is_valid()
            || !config.secondary.is_valid()
            || config.secondary.copy.copy_done
            || !config.primary.sender.is_valid()
            || config
                .cached_progress
                .is_some_and(|lsn| lsn < 0 || lsn == Lsn::MAX)
        {
            return Err(RoleError::InvalidConfig);
        }
        let ingress = MessageIngress::new(
            config.secondary_route.clone(),
            config.secondary.context.buffer_limits,
        );
        Ok(Self {
            config,
            provider,
            clock,
            status: RoleStatus::Created,
            pending: None,
            provider_call: None,
            retained_progress_call: None,
            retained_call: None,
            opened_progress: None,
            data_loss_call: None,
            data_loss_failure: None,
            data_loss_end: None,
            primary: None,
            secondary: None,
            retired_primaries: Vec::new(),
            retired_secondaries: Vec::new(),
            fault: None,
            ingress,
            registration: None,
        })
    }

    pub(super) fn open(
        &mut self,
        registry: &MessageRegistry,
    ) -> Result<PublishedEndpoint, RoleError> {
        if self.status != RoleStatus::Created {
            return Err(RoleError::InvalidState);
        }
        self.registration = Some(registry.register(self.ingress.clone())?);
        self.status = RoleStatus::Opened;
        Ok(self.ingress.published_endpoint())
    }

    pub(super) fn begin_get_opened_progress(&mut self) -> Result<(), RoleError> {
        self.check_idle()?;
        if self.status != RoleStatus::Opened {
            return Err(RoleError::InvalidState);
        }
        self.opened_progress = self.config.cached_progress.map(Ok);
        if self.opened_progress.is_none() {
            let provider = self.provider.clone();
            self.provider_call = Some(Box::pin(async move {
                read_provider_progress(provider.as_ref()).await
            }));
            self.pending = Some(Step::OpenedProgress);
        }
        Ok(())
    }

    pub(super) fn opened_progress_result(&self) -> Option<&Result<Lsn, RoleError>> {
        self.opened_progress.as_ref()
    }

    pub(super) fn ingress(&self) -> MessageIngress {
        self.ingress.clone()
    }

    fn refresh_ingress(&self) {
        let target = match (self.status, self.pending) {
            (RoleStatus::Ready(Role::Primary) | RoleStatus::Closing(Role::Primary), _)
            | (
                RoleStatus::Changing(Role::Primary),
                Some(Step::PrimaryEpoch(Some(Role::Primary))),
            ) => Target::Primary(
                self.primary
                    .as_ref()
                    .expect("admitted primary owner")
                    .handle(),
            ),
            (RoleStatus::Ready(Role::IdleSecondary | Role::ActiveSecondary), _)
            | (RoleStatus::Changing(Role::ActiveSecondary), Some(Step::ActivateSecondary)) => {
                Target::Secondary(
                    self.secondary
                        .as_ref()
                        .expect("admitted secondary owner")
                        .handle(),
                )
            }
            _ => Target::None,
        };
        self.ingress.set_target(target);
    }

    pub(super) fn status(&self) -> RoleStatus {
        self.status
    }

    pub(super) fn fault(&self) -> Option<&RoleError> {
        self.fault.as_ref()
    }

    pub(super) fn operation_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn data_loss_result(&self) -> Option<&DataLossEnd> {
        self.data_loss_end.as_ref()
    }

    pub(super) fn primary(&self) -> Result<PrimaryHandle, RoleError> {
        if self.status != RoleStatus::Ready(Role::Primary) {
            return Err(RoleError::InvalidState);
        }
        Ok(self
            .primary
            .as_ref()
            .ok_or(RoleError::InvalidState)?
            .handle())
    }

    pub(super) fn secondary(&self) -> Result<SecondaryHandle, RoleError> {
        if !matches!(
            self.status,
            RoleStatus::Ready(Role::IdleSecondary | Role::ActiveSecondary)
        ) {
            return Err(RoleError::InvalidState);
        }
        Ok(self
            .secondary
            .as_ref()
            .ok_or(RoleError::InvalidState)?
            .handle())
    }

    fn check_idle(&self) -> Result<(), RoleError> {
        if self.pending.is_some() {
            Err(RoleError::Busy)
        } else {
            Ok(())
        }
    }

    /// Only a host-qualified, physically exclusive retained creation may select
    /// this cold path. Neither a cached LSN nor a previous remote ACK is input.
    pub(super) fn begin_restore_retained_secondary(
        &mut self,
        epoch: Epoch,
    ) -> Result<(), RoleError> {
        self.check_idle()?;
        if self.status != RoleStatus::Opened
            || self.primary.is_some()
            || self.secondary.is_some()
            || !self.retired_primaries.is_empty()
            || !self.retired_secondaries.is_empty()
        {
            return Err(RoleError::InvalidState);
        }
        if epoch.data_loss_number < 0
            || epoch.configuration_number < 0
            || !self.config.secondary.replication.has_persisted_state
            || self.config.secondary.copy.copy_done
        {
            return Err(RoleError::InvalidConfig);
        }
        self.opened_progress = None;
        self.retained_progress_call = Some(Box::pin(Secondary::read_retained_progress(
            self.provider.clone(),
            epoch,
        )));
        self.pending = Some(Step::RetainedProgress);
        self.status = RoleStatus::Changing(Role::ActiveSecondary);
        self.refresh_ingress();
        Ok(())
    }

    pub(super) fn begin_change_role(&mut self, role: Role, epoch: Epoch) -> Result<(), RoleError> {
        self.check_idle()?;
        if epoch.data_loss_number < 0 || epoch.configuration_number < 0 {
            return Err(RoleError::InvalidConfig);
        }
        let step = match (self.status, role) {
            (RoleStatus::Opened, Role::Primary) => {
                let provider = self.provider.clone();
                let cached = self.config.cached_progress;
                self.provider_call = Some(Box::pin(async move {
                    match cached {
                        Some(progress) => Ok(progress),
                        None => read_provider_progress(provider.as_ref()).await,
                    }
                }));
                Some(Step::InitialPrimary(epoch))
            }
            (RoleStatus::Opened, Role::IdleSecondary | Role::ActiveSecondary) => {
                self.secondary = Some(Secondary::new(
                    self.config.secondary_route.clone(),
                    self.config.secondary,
                    self.provider.clone(),
                    self.config.secondary_callbacks.clone(),
                    self.clock.clone(),
                )?);
                None
            }
            (RoleStatus::Opened | RoleStatus::Faulted, Role::None) => None,
            (RoleStatus::Ready(Role::Primary), Role::ActiveSecondary) => {
                if let Err(error) = self
                    .primary
                    .as_ref()
                    .unwrap()
                    .handle()
                    .close_for_secondary()
                {
                    return self.fail(error.into());
                }
                Some(Step::Demote(epoch))
            }
            (RoleStatus::Ready(Role::IdleSecondary | Role::ActiveSecondary), Role::Primary) => {
                if let Err(error) = self
                    .secondary
                    .as_ref()
                    .unwrap()
                    .handle()
                    .close_for_primary()
                {
                    return self.fail(error.into());
                }
                Some(Step::Promote(epoch))
            }
            (RoleStatus::Ready(Role::IdleSecondary), Role::ActiveSecondary) => {
                Some(Step::ActivateSecondary)
            }
            (
                RoleStatus::Ready(
                    old @ (Role::Primary | Role::IdleSecondary | Role::ActiveSecondary),
                ),
                Role::None,
            ) => {
                let result = if old == Role::Primary {
                    self.primary
                        .as_ref()
                        .unwrap()
                        .handle()
                        .close(Duration::ZERO)
                        .map_err(RoleError::from)
                } else {
                    self.secondary
                        .as_ref()
                        .unwrap()
                        .handle()
                        .close()
                        .map_err(RoleError::from)
                };
                if let Err(error) = result {
                    return self.fail(error);
                }
                Some(Step::Close(old))
            }
            _ => return Err(RoleError::InvalidTransition(self.status, role)),
        };
        self.pending = step;
        self.status = if step.is_some() {
            RoleStatus::Changing(role)
        } else {
            RoleStatus::Ready(role)
        };
        self.refresh_ingress();
        Ok(())
    }

    fn prepare_primary_epoch(&mut self, epoch: Epoch, role: Option<Role>) -> Result<(), RoleError> {
        let boundary = self
            .primary
            .as_ref()
            .unwrap()
            .handle()
            .update_epoch(epoch)?;
        let provider = self.provider.clone();
        self.provider_call = Some(Box::pin(async move {
            provider.update_epoch(epoch, boundary).await?;
            Ok(boundary)
        }));
        self.pending = Some(Step::PrimaryEpoch(role));
        self.refresh_ingress();
        Ok(())
    }

    pub(super) fn begin_update_epoch(&mut self, epoch: Epoch) -> Result<(), RoleError> {
        self.check_idle()?;
        match self.status {
            RoleStatus::Ready(Role::Primary) => self.prepare_primary_epoch(epoch, None),
            RoleStatus::Ready(Role::IdleSecondary | Role::ActiveSecondary) => {
                match self
                    .secondary
                    .as_ref()
                    .unwrap()
                    .handle()
                    .begin_external_epoch(epoch)
                {
                    Ok(true) => {
                        self.pending = Some(Step::SecondaryEpoch);
                        Ok(())
                    }
                    Ok(false) => Ok(()),
                    Err(error @ SecondaryError::IncompleteCopy) => self.fail(error.into()),
                    Err(error) => Err(error.into()),
                }
            }
            _ => Err(RoleError::InvalidState),
        }
    }

    pub(super) fn begin_data_loss(&mut self) -> Result<(), RoleError> {
        self.check_idle()?;
        if self.status != RoleStatus::Ready(Role::Primary) {
            return Err(RoleError::InvalidState);
        }
        self.primary.as_ref().unwrap().begin_data_loss()?;
        let provider = self.provider.clone();
        self.data_loss_call = Some(Box::pin(async move {
            match provider.on_data_loss().await {
                Ok(state_changed) => DataLossRecovery {
                    state_changed,
                    progress: if state_changed {
                        read_provider_progress(provider.as_ref()).await.map(Some)
                    } else {
                        Ok(None)
                    },
                },
                Err(error) => DataLossRecovery {
                    state_changed: false,
                    progress: Err(error),
                },
            }
        }));
        self.data_loss_end = None;
        self.data_loss_failure = None;
        self.pending = Some(Step::DataLoss);
        self.status = RoleStatus::CheckingDataLoss;
        self.refresh_ingress();
        Ok(())
    }

    fn complete_data_loss(
        &mut self,
        recovery: DataLossRecovery,
        aborting: bool,
    ) -> Result<(), RoleError> {
        let failure = self.data_loss_failure.take();
        let mut result = recovery
            .progress
            .map_err(|error| RoleError::Provider(Arc::new(error)))
            .and_then(|progress| {
                // Native updates its cached recovery progress before attempting reset.
                if let Some(progress) = progress {
                    self.config.cached_progress = Some(progress);
                }
                if let Some(error) = failure {
                    return Err(error);
                }
                if aborting {
                    return Err(RoleError::Aborted);
                }
                match progress {
                    Some(progress) => self
                        .primary
                        .as_ref()
                        .unwrap()
                        .reset_replication_queue(progress)
                        .map_err(RoleError::from),
                    None => Ok(()),
                }
            });
        if !aborting {
            let restore = self.primary.as_ref().unwrap().end_data_loss();
            self.status = if let Err(error) = &restore {
                self.fault.get_or_insert(error.clone().into());
                RoleStatus::Faulted
            } else {
                RoleStatus::Ready(Role::Primary)
            };
            // End restores the role even on callback/reset failure; the original
            // error wins over a simultaneous failure to restore primary admission.
            result = result.and(restore.map_err(RoleError::from));
            self.pending = None;
            self.refresh_ingress();
        }
        self.data_loss_end = Some(DataLossEnd {
            state_changed: recovery.state_changed,
            result: result.clone(),
        });
        result
    }

    async fn run_data_loss(&mut self) -> Result<(), RoleError> {
        let call = self.data_loss_call.as_mut().unwrap();
        let recovery = if self.data_loss_failure.is_some() {
            call.await
        } else {
            tokio::select! {
                recovery = &mut *call => recovery,
                end = self.primary.as_mut().unwrap().run() => {
                    let result = end.map_err(RoleError::from)
                        .and_then(|end| end.result.map_err(RoleError::from));
                    self.data_loss_failure = Some(result.err().unwrap_or(RoleError::InvalidState));
                    call.await
                }
            }
        };
        self.data_loss_call = None;
        self.complete_data_loss(recovery, false)
    }

    fn fail<T>(&mut self, error: RoleError) -> Result<T, RoleError> {
        self.status = RoleStatus::Faulted;
        self.pending = None;
        self.fault.get_or_insert(error.clone());
        self.refresh_ingress();
        Err(error)
    }

    pub(super) fn begin_close(&mut self, quorum_wait: Duration) -> Result<(), RoleError> {
        self.check_idle()?;
        if !matches!(
            self.status,
            RoleStatus::Opened | RoleStatus::Ready(_) | RoleStatus::Faulted
        ) {
            return Err(RoleError::InvalidState);
        }
        let (role, result) = if let Some(primary) = &self.primary {
            (
                Role::Primary,
                primary.handle().close(quorum_wait).map_err(RoleError::from),
            )
        } else if let Some(secondary) = &self.secondary {
            (
                Role::ActiveSecondary,
                secondary.handle().close().map_err(RoleError::from),
            )
        } else {
            self.status = RoleStatus::Closed;
            self.refresh_ingress();
            self.registration = None;
            return Ok(());
        };
        if let Err(error) = result {
            self.registration = None;
            return self.fail(error);
        }
        self.pending = Some(Step::DirectClose(role));
        self.status = RoleStatus::Closing(role);
        self.refresh_ingress();
        Ok(())
    }

    pub(super) fn abort(&mut self) -> Result<(), RoleError> {
        if self.status == RoleStatus::Aborted {
            return Ok(());
        }
        if let Some(primary) = &self.primary {
            primary.handle().abort()?;
        }
        if let Some(secondary) = &self.secondary {
            secondary.handle().abort()?;
        }
        if matches!(self.pending, Some(Step::OpenedProgress)) {
            self.opened_progress = Some(Err(RoleError::Aborted));
        }
        self.status = RoleStatus::Aborting;
        self.pending = Some(Step::Abort);
        self.refresh_ingress();
        self.registration = None;
        Ok(())
    }

    pub(super) async fn run(&mut self) -> Result<(), RoleError> {
        if self.pending.is_none() {
            return Err(RoleError::NoOperation);
        }
        let data_loss = matches!(self.pending, Some(Step::DataLoss));
        let opened_progress = matches!(self.pending, Some(Step::OpenedProgress));
        let direct_close = matches!(self.pending, Some(Step::DirectClose(_)));
        let result = self.run_inner().await;
        if direct_close {
            self.registration = None;
        }
        match result {
            Err(error) if !data_loss && !opened_progress && self.status != RoleStatus::Aborted => {
                self.fail(error)
            }
            result => result,
        }
    }

    async fn run_inner(&mut self) -> Result<(), RoleError> {
        loop {
            let step = self.pending.ok_or(RoleError::NoOperation)?;
            match step {
                Step::OpenedProgress => {
                    let result = self
                        .provider_call
                        .as_mut()
                        .unwrap()
                        .await
                        .map_err(|error| RoleError::Provider(Arc::new(error)));
                    self.provider_call = None;
                    if let Ok(progress) = result {
                        self.config.cached_progress = Some(progress);
                    }
                    self.opened_progress = Some(result.clone());
                    self.pending = None;
                    return result.map(|_| ());
                }
                Step::InitialPrimary(epoch) => {
                    let result = self.provider_call.as_mut().unwrap().await;
                    self.provider_call = None;
                    let progress = result.map_err(|error| RoleError::Provider(Arc::new(error)))?;
                    self.config.cached_progress = Some(progress);
                    let mut config = self.config.primary.clone();
                    config.initial_progress = progress;
                    config.epoch = epoch;
                    self.primary = Some(Primary::new(config, self.clock.clone())?);
                    self.prepare_primary_epoch(epoch, Some(Role::Primary))?;
                }
                Step::RetainedProgress => {
                    let progress = self.retained_progress_call.as_mut().unwrap().await;
                    self.retained_progress_call = None;
                    self.retained_call =
                        Some(Box::pin(Secondary::confirm_retained_checkpoint(progress?)));
                    self.pending = Some(Step::RestoreRetainedSecondary);
                }
                Step::RestoreRetainedSecondary => {
                    let checkpoint = self.retained_call.as_mut().unwrap().await;
                    self.retained_call = None;
                    self.secondary = Some(Secondary::from_retained(
                        self.config.secondary_route.clone(),
                        self.config.secondary,
                        checkpoint?,
                        self.config.secondary_callbacks.clone(),
                        self.clock.clone(),
                    )?);
                    self.status = RoleStatus::Ready(Role::ActiveSecondary);
                    self.pending = None;
                    self.refresh_ingress();
                    return Ok(());
                }
                Step::Promote(epoch) => {
                    let secondary = self.secondary.as_mut().unwrap();
                    secondary.run().await?.result?;
                    let transfer = secondary.take_for_primary()?;
                    let mut config = self.config.primary.clone();
                    config.epoch = epoch;
                    self.primary = Some(Primary::from_secondary(
                        config,
                        self.clock.clone(),
                        transfer,
                    )?);
                    self.retired_secondaries
                        .push(self.secondary.take().unwrap());
                    self.prepare_primary_epoch(epoch, Some(Role::Primary))?;
                }
                Step::Demote(epoch) => {
                    let primary = self.primary.as_mut().unwrap();
                    primary.run().await?.result?;
                    let transfer =
                        primary.take_for_secondary(self.config.secondary.replication.queue)?;
                    self.secondary = Some(Secondary::from_primary(
                        self.config.secondary_route.clone(),
                        self.config.secondary,
                        epoch,
                        transfer,
                        self.provider.clone(),
                        self.config.secondary_callbacks.clone(),
                        self.clock.clone(),
                    )?);
                    self.retired_primaries.push(self.primary.take().unwrap());
                    self.status = RoleStatus::Ready(Role::ActiveSecondary);
                    self.pending = None;
                    self.refresh_ingress();
                    return Ok(());
                }
                Step::ActivateSecondary => {
                    self.secondary.as_mut().unwrap().wait_for_active().await?;
                    self.status = RoleStatus::Ready(Role::ActiveSecondary);
                    self.pending = None;
                    self.refresh_ingress();
                    return Ok(());
                }
                Step::Close(old) | Step::DirectClose(old) => {
                    let direct = matches!(step, Step::DirectClose(_));
                    if old == Role::Primary {
                        self.primary.as_mut().unwrap().run().await?.result?;
                        self.retired_primaries.push(self.primary.take().unwrap());
                    } else {
                        self.secondary.as_mut().unwrap().run().await?.result?;
                        self.retired_secondaries
                            .push(self.secondary.take().unwrap());
                    }
                    self.status = if direct {
                        RoleStatus::Closed
                    } else {
                        RoleStatus::Ready(Role::None)
                    };
                    self.pending = None;
                    self.refresh_ingress();
                    return Ok(());
                }
                Step::PrimaryEpoch(role) => {
                    let result = tokio::select! {
                        result = self.provider_call.as_mut().unwrap() => result,
                        end = self.primary.as_mut().unwrap().run() => {
                            end?.result?;
                            return Err(RoleError::InvalidState);
                        }
                    };
                    self.provider_call = None;
                    if let Err(error) = result {
                        let error = Arc::new(error);
                        self.primary
                            .as_ref()
                            .unwrap()
                            .handle()
                            .report_provider_fault(error.clone());
                        return Err(RoleError::Provider(error));
                    }
                    if let Some(role) = role {
                        self.status = RoleStatus::Ready(role);
                    }
                    self.pending = None;
                    self.refresh_ingress();
                    return Ok(());
                }
                Step::SecondaryEpoch => {
                    self.secondary
                        .as_mut()
                        .unwrap()
                        .wait_external_epoch()
                        .await?;
                    self.pending = None;
                    self.refresh_ingress();
                    return Ok(());
                }
                Step::DataLoss => return self.run_data_loss().await,
                Step::Abort => {
                    if let Some(call) = self.retained_progress_call.as_mut() {
                        let result = call.await;
                        self.retained_progress_call = None;
                        if let Err(error) = result {
                            self.fault.get_or_insert(error.into());
                        }
                    }
                    if let Some(call) = self.retained_call.as_mut() {
                        let result = call.await;
                        self.retained_call = None;
                        if let Err(error) = result {
                            self.fault.get_or_insert(error.into());
                        }
                    }
                    if let Some(call) = self.provider_call.as_mut() {
                        let result = call.await;
                        self.provider_call = None;
                        if let Err(error) = result {
                            self.fault
                                .get_or_insert(RoleError::Provider(Arc::new(error)));
                        }
                    }
                    if let Some(call) = self.data_loss_call.as_mut() {
                        let recovery = call.await;
                        self.data_loss_call = None;
                        if let Err(error) = self.complete_data_loss(recovery, true)
                            && !matches!(error, RoleError::Aborted)
                        {
                            self.fault.get_or_insert(error);
                        }
                    }
                    if let Some(primary) = &mut self.primary {
                        primary.run().await?;
                    }
                    if let Some(secondary) = &mut self.secondary {
                        secondary.run().await?;
                    }
                    self.status = RoleStatus::Aborted;
                    self.pending = None;
                    self.refresh_ingress();
                    return Err(RoleError::Aborted);
                }
            }
        }
    }

    /// Drive established secondary context/provider/ACK work between requests.
    /// Terminal child shutdown is explicit, not a successful role operation.
    pub(super) async fn drive(&mut self) -> Result<(), RoleError> {
        self.check_idle()?;
        if !matches!(
            self.status,
            RoleStatus::Ready(Role::Primary | Role::IdleSecondary | Role::ActiveSecondary)
        ) {
            return Err(RoleError::InvalidState);
        }
        let result = if let Some(primary) = &mut self.primary {
            match primary.run().await {
                Ok(end) => end.result.map_err(RoleError::from),
                Err(error) => Err(error.into()),
            }
        } else if let Some(secondary) = &mut self.secondary {
            match secondary.run().await {
                Ok(end) => end.result.map_err(RoleError::from),
                Err(error) => Err(error.into()),
            }
        } else {
            return Err(RoleError::InvalidState);
        };
        self.fail(result.err().unwrap_or(RoleError::InvalidState))
    }

    pub(super) async fn drain_retired_builds(&mut self) -> Result<(), RoleError> {
        for primary in &mut self.retired_primaries {
            primary.drain_builds().await?;
        }
        Ok(())
    }

    pub(super) fn take_retired_primaries(&mut self) -> Vec<Primary> {
        std::mem::take(&mut self.retired_primaries)
    }

    pub(super) fn take_retired_secondaries(&mut self) -> Vec<Secondary> {
        std::mem::take(&mut self.retired_secondaries)
    }

    pub(super) async fn drain_builds(&mut self) -> Result<(), RoleError> {
        if let Some(primary) = &mut self.primary {
            primary.drain_builds().await?;
        }
        self.drain_retired_builds().await
    }
}

#[cfg(test)]
mod tests;
