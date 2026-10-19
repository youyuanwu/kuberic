// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation of ReplicatorState.cpp:39-126,
// Replicator.ChangeRoleAsyncOperation.cpp:37-514 and
// Replicator.UpdateEpochAsyncOperation.cpp:34-145, microsoft/service-fabric
// @3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d, Reliability/Replication.

//! Private, explicitly driven role/epoch owner, not a public facade or host
//! demultiplexer. A cancelled run retains the selected operation/provider call.
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
use super::primary::{Primary, PrimaryConfig, PrimaryError, PrimaryHandle};
use super::secondary::{
    Secondary, SecondaryCallbacks, SecondaryConfig, SecondaryError, SecondaryHandle,
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
    Opened,
    Changing(Role),
    Ready(Role),
    Faulted,
    Aborting,
    Aborted,
}

#[derive(Clone, Debug, thiserror::Error)]
pub(super) enum RoleError {
    #[error("role/epoch operation already outstanding")]
    Busy,
    #[error("role transition is not admitted from {0:?} to {1:?}")]
    InvalidTransition(RoleStatus, Role),
    #[error("role owner has no admitted role/epoch operation")]
    NoOperation,
    #[error("role owner is not in the requested stable role")]
    InvalidState,
    #[error("inconsistent local identity or invalid role-owner settings")]
    InvalidConfig,
    #[error("role owner was aborted")]
    Aborted,
    #[error("StateProvider role callback: {0}")]
    Provider(Arc<RuntimeError>),
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

#[derive(Clone, Copy)]
enum Step {
    InitialPrimary(Epoch),
    Promote(Epoch),
    Demote(Epoch),
    ActivateSecondary,
    Close(Role),
    PrimaryEpoch(Option<Role>),
    SecondaryEpoch,
    Abort,
}

pub(super) struct RoleReplicator {
    config: RoleConfig,
    provider: Arc<dyn StateProvider>,
    clock: Clock,
    status: RoleStatus,
    pending: Option<Step>,
    provider_call: Option<BoxFuture<'static, crate::Result<Lsn>>>,
    primary: Option<Primary>,
    secondary: Option<Secondary>,
    retired_primaries: Vec<Primary>,
    retired_secondaries: Vec<Secondary>,
    fault: Option<RoleError>,
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
        Ok(Self {
            config,
            provider,
            clock,
            status: RoleStatus::Opened,
            pending: None,
            provider_call: None,
            primary: None,
            secondary: None,
            retired_primaries: Vec::new(),
            retired_secondaries: Vec::new(),
            fault: None,
        })
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
                        None => provider.last_committed_lsn().await,
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

    fn fail<T>(&mut self, error: RoleError) -> Result<T, RoleError> {
        self.status = RoleStatus::Faulted;
        self.pending = None;
        self.fault.get_or_insert(error.clone());
        Err(error)
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
        self.status = RoleStatus::Aborting;
        self.pending = Some(Step::Abort);
        Ok(())
    }

    pub(super) async fn run(&mut self) -> Result<(), RoleError> {
        if self.pending.is_none() {
            return Err(RoleError::NoOperation);
        }
        let result = self.run_inner().await;
        match result {
            Err(error) if self.status != RoleStatus::Aborted => self.fail(error),
            result => result,
        }
    }

    async fn run_inner(&mut self) -> Result<(), RoleError> {
        loop {
            let step = self.pending.ok_or(RoleError::NoOperation)?;
            match step {
                Step::InitialPrimary(epoch) => {
                    let result = self.provider_call.as_mut().unwrap().await;
                    self.provider_call = None;
                    let progress = result.map_err(|error| RoleError::Provider(Arc::new(error)))?;
                    let mut config = self.config.primary.clone();
                    config.initial_progress = progress;
                    config.epoch = epoch;
                    self.primary = Some(Primary::new(config, self.clock.clone())?);
                    self.prepare_primary_epoch(epoch, Some(Role::Primary))?;
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
                    return Ok(());
                }
                Step::ActivateSecondary => {
                    self.secondary.as_mut().unwrap().wait_for_active().await?;
                    self.status = RoleStatus::Ready(Role::ActiveSecondary);
                    self.pending = None;
                    return Ok(());
                }
                Step::Close(old) => {
                    if old == Role::Primary {
                        self.primary.as_mut().unwrap().run().await?.result?;
                        self.retired_primaries.push(self.primary.take().unwrap());
                    } else {
                        self.secondary.as_mut().unwrap().run().await?.result?;
                        self.retired_secondaries
                            .push(self.secondary.take().unwrap());
                    }
                    self.status = RoleStatus::Ready(Role::None);
                    self.pending = None;
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
                    return Ok(());
                }
                Step::SecondaryEpoch => {
                    self.secondary
                        .as_mut()
                        .unwrap()
                        .wait_external_epoch()
                        .await?;
                    self.pending = None;
                    return Ok(());
                }
                Step::Abort => {
                    if let Some(call) = self.provider_call.as_mut() {
                        let result = call.await;
                        self.provider_call = None;
                        if let Err(error) = result {
                            self.fault
                                .get_or_insert(RoleError::Provider(Arc::new(error)));
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
}

#[cfg(test)]
mod tests;
