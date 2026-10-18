// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Source-guided translation of CopySender.h/.cpp, EOF target capture from
// PrimaryReplicator.BuildIdleAsyncOperation.cpp and ordered RemoteSession
// handoffs, microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d,
// src/prod/src/Reliability/Replication/. See sf-v1-port-provenance.md.

//! Private copy/build completion owner, not provider acquisition, StartCopy,
//! full BuildIdle/role orchestration or slow-replica fault policy. The caller
//! delivers ordered session replication-gate then copy-ACK handoffs. Enumeration
//! invokes the supplied latest-issued getter at EOF, outside all owner guards.
//! Handles are weak; selected enumeration sends retain this callback owner.

#![cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 copy build gates")
)]

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use crate::application::{Lsn, OperationDataStream};
use crate::error::RuntimeError;

use super::async_runtime::Clock;
use super::copy_async_operation::{
    CopyAsyncOperation, CopyConfig, CopyEnd, CopyError, CopyFault, CopyHandle, CopySendAction,
    CopySnapshot,
};
use super::sender::SenderProgress;

const DISABLED_BUILD_FAULT: &str =
    "Build for replica completed successfully, in spite of disabling it due to slow progress";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CopyPhase {
    NotStarted,
    Started,
    LsnSet,
    ReplCompleted,
    Completed,
}

struct CopyState {
    phase: CopyPhase,
    wait_for_replication_acks: bool,
    last_copy: Option<Lsn>,
    replication_target: Option<Lsn>,
}

impl CopyState {
    fn is_last(&self, lsn: Lsn) -> bool {
        matches!(
            self.phase,
            CopyPhase::LsnSet | CopyPhase::ReplCompleted | CopyPhase::Completed
        ) && self.last_copy == Some(lsn)
    }
}

struct State {
    copy: CopyState,
    begun: bool,
    active: bool,
    disabled: bool,
    early_error: Option<CopyError>,
    ended: Option<CopyEnd>,
}

struct Root {
    state: Mutex<State>,
    // Weak enumeration handle: its last callback owns this root, not vice versa.
    operation: OnceLock<CopyHandle>,
}

#[derive(Clone, Debug)]
pub(super) struct CopySenderSnapshot {
    pub(super) phase: CopyPhase,
    pub(super) last_copy: Option<Lsn>,
    pub(super) replication_target: Option<Lsn>,
    pub(super) disabled: bool,
    pub(super) enumeration: Option<CopySnapshot>,
    pub(super) result: Option<Result<(), CopyError>>,
}

#[derive(Clone)]
pub(super) struct CopySenderHandle {
    root: Weak<Root>,
}

impl CopySenderHandle {
    fn root(&self) -> Result<Arc<Root>, CopyError> {
        self.root.upgrade().ok_or(CopyError::OwnerDropped)
    }

    pub(super) fn snapshot(&self) -> Result<CopySenderSnapshot, CopyError> {
        let root = self.root()?;
        let enumeration = root.operation.get().map(CopyHandle::snapshot).transpose()?;
        let state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
        Ok(CopySenderSnapshot {
            phase: state.copy.phase,
            last_copy: state.copy.last_copy,
            replication_target: state.copy.replication_target,
            disabled: state.disabled,
            result: state
                .ended
                .as_ref()
                .map(|end| end.result.clone())
                .or_else(|| enumeration.as_ref().and_then(|copy| copy.result.clone()))
                .or_else(|| state.early_error.clone().map(Err)),
            enumeration,
        })
    }

    pub(super) fn progress(&self) -> Result<Option<SenderProgress>, CopyError> {
        let root = self.root()?;
        let active = root.state.lock().map_err(|_| CopyError::Poisoned)?.active;
        if active {
            root.operation.get().map(CopyHandle::progress).transpose()
        } else {
            Ok(None)
        }
    }

    pub(super) fn averages(&self) -> Result<Option<(Duration, Duration)>, CopyError> {
        let root = self.root()?;
        let active = root.state.lock().map_err(|_| CopyError::Poisoned)?.active;
        if active {
            root.operation.get().map(CopyHandle::averages).transpose()
        } else {
            Ok(None)
        }
    }

    fn on_last_enumerator_copy_op(
        &self,
        copy_lsn: Lsn,
        replication_lsn: Lsn,
    ) -> Result<(), CopyError> {
        if replication_lsn < 0 {
            return Err(CopyError::InvalidReplicationProgress);
        }
        let root = self.root()?;
        let mut state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
        if state.copy.phase == CopyPhase::Completed {
            return Ok(());
        }
        assert_eq!(state.copy.phase, CopyPhase::Started);
        state.copy.last_copy = Some(copy_lsn);
        if state.copy.wait_for_replication_acks {
            state.copy.replication_target = Some(replication_lsn);
            state.copy.phase = CopyPhase::LsnSet;
        } else {
            state.copy.phase = CopyPhase::ReplCompleted;
        }
        Ok(())
    }

    pub(super) fn update_last_replication_operation_during_copy(
        &self,
        replication_lsn: Lsn,
    ) -> Result<bool, CopyError> {
        if replication_lsn < 0 {
            return Err(CopyError::InvalidReplicationProgress);
        }
        let root = self.root()?;
        let mut state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
        if !state.copy.wait_for_replication_acks {
            return Err(CopyError::ReplicationNotRequired);
        }
        if state.copy.phase != CopyPhase::LsnSet
            || state
                .copy
                .replication_target
                .is_none_or(|target| target >= replication_lsn)
        {
            return Ok(false);
        }
        state.copy.replication_target = Some(replication_lsn);
        Ok(true)
    }

    /// Consume the session's CopyReplicationGate handoff BEFORE its CopyAck.
    /// A replication ACK before copy terminal is not a completed build gate.
    pub(super) fn process_replication_ack(
        &self,
        copy_applied: Lsn,
        replication_applied: Lsn,
    ) -> Result<bool, CopyError> {
        if copy_applied < -1 {
            return Err(CopyError::InvalidAck);
        }
        if replication_applied < 0 {
            return Err(CopyError::InvalidReplicationProgress);
        }
        let root = self.root()?;
        let snapshot = root.operation.get().map(CopyHandle::snapshot).transpose()?;
        let operation = {
            let mut state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
            if !state.active
                || state.disabled
                || (copy_applied != -1 && !state.copy.is_last(copy_applied))
                || state.copy.phase != CopyPhase::LsnSet
            {
                return Ok(false);
            }
            if state.copy.last_copy.unwrap()
                > snapshot.as_ref().expect("active enumeration").last_admitted
            {
                return Err(CopyError::InvalidAck);
            }
            if replication_applied < state.copy.replication_target.unwrap() {
                return Ok(false);
            }
            state.copy.phase = CopyPhase::ReplCompleted;
            root.operation.get().cloned().expect("active enumeration")
        };
        operation.try_complete_with_success_if_copy_done()?;
        Ok(true)
    }

    /// Native -1 means all copy, zero means no update; ordinary empty payloads
    /// do not imply either. Transport/session identity admission remains outside.
    pub(super) fn process_copy_ack(&self, received: Lsn, applied: Lsn) -> Result<bool, CopyError> {
        if received < -1 || applied < -1 {
            return Err(CopyError::InvalidAck);
        }
        let root = self.root()?;
        let Some(operation) = root.operation.get() else {
            return Ok(false);
        };
        let snapshot = operation.snapshot()?;
        let (received, applied, is_last, complete) = {
            let mut state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
            if !state.active || state.copy.phase == CopyPhase::Completed {
                return Ok(false);
            }
            let normalize = |lsn| {
                if lsn == -1 {
                    state.copy.last_copy.ok_or(CopyError::InvalidAck)
                } else {
                    Ok(lsn)
                }
            };
            let received = normalize(received)?;
            let applied = normalize(applied)?;
            if received > snapshot.last_admitted
                || applied > snapshot.last_admitted
                || (received != 0 && received < applied)
            {
                return Err(CopyError::InvalidAck);
            }
            if received == 0 && applied == 0 {
                return Ok(false);
            }
            let is_last = state.copy.is_last(applied);
            let complete =
                is_last && state.copy.phase == CopyPhase::ReplCompleted && !state.disabled;
            if complete {
                state.active = false;
            }
            (received, applied, is_last, complete)
        };
        operation.update_progress(received, applied, is_last)?;
        if complete {
            operation.try_complete_with_success_if_copy_done()?;
        }
        Ok(true)
    }

    pub(super) fn try_disable_build_completion(&self) -> Result<bool, CopyError> {
        let root = self.root()?;
        let completed = root
            .operation
            .get()
            .map(CopyHandle::snapshot)
            .transpose()?
            .is_some_and(|copy| copy.result.is_some());
        let mut state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
        if state.disabled {
            return Err(CopyError::AlreadyDisabled);
        }
        if completed || state.copy.phase == CopyPhase::Completed {
            return Ok(false);
        }
        state.disabled = true;
        Ok(true)
    }

    pub(super) fn finish(&self, error: CopyError) -> Result<(), CopyError> {
        let root = self.root()?;
        {
            let mut state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
            state.active = false;
            if !state.begun {
                state.early_error = Some(error.clone());
            }
        }
        if let Some(operation) = root.operation.get() {
            operation.complete_error(error)?;
        }
        Ok(())
    }

    /// Enumeration cancellation preserves its native in-flight GetNext timing.
    /// Session FinishOperation is a separate, immediate error completion.
    pub(super) fn cancel(&self) -> Result<(), CopyError> {
        let root = self.root()?;
        match root.operation.get() {
            Some(operation) => operation.cancel(),
            None => self.finish(CopyError::Cancelled),
        }
    }

    pub(super) fn end(&self) -> Result<CopyEnd, CopyError> {
        let root = self.root()?;
        {
            let state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
            if !state.begun {
                return Err(CopyError::NotStarted);
            }
            if let Some(end) = &state.ended {
                return Ok(end.clone());
            }
        }
        let mut end = match root.operation.get() {
            Some(operation) => operation.end()?,
            None => CopyEnd {
                result: Err(root
                    .state
                    .lock()
                    .map_err(|_| CopyError::Poisoned)?
                    .early_error
                    .clone()
                    .expect("Begin without enumeration has a stored failure")),
                fault: None,
            },
        };
        let mut state = root.state.lock().map_err(|_| CopyError::Poisoned)?;
        if let Some(end) = &state.ended {
            return Ok(end.clone());
        }
        if end.result.is_ok() {
            assert!(matches!(
                state.copy.phase,
                CopyPhase::LsnSet | CopyPhase::ReplCompleted
            ));
        }
        state.copy.phase = CopyPhase::Completed;
        state.active = false;
        if end.result.is_ok() && state.disabled {
            end.result = Err(CopyError::BuildCompletionDisabled);
            end.fault = Some(CopyFault {
                error: Arc::new(RuntimeError::InvalidReplication(
                    DISABLED_BUILD_FAULT.into(),
                )),
                description: DISABLED_BUILD_FAULT,
            });
        }
        state.ended = Some(end.clone());
        Ok(end)
    }
}

pub(super) struct CopySender {
    root: Arc<Root>,
    operation: Option<CopyAsyncOperation>,
}

impl CopySender {
    pub(super) fn new(wait_for_replication_acks: bool) -> Self {
        Self {
            root: Arc::new(Root {
                state: Mutex::new(State {
                    copy: CopyState {
                        phase: CopyPhase::NotStarted,
                        wait_for_replication_acks,
                        last_copy: None,
                        replication_target: None,
                    },
                    begun: false,
                    active: false,
                    disabled: false,
                    early_error: None,
                    ended: None,
                }),
                operation: OnceLock::new(),
            }),
            operation: None,
        }
    }

    pub(super) fn handle(&self) -> CopySenderHandle {
        CopySenderHandle {
            root: Arc::downgrade(&self.root),
        }
    }

    pub(super) fn begin(
        &mut self,
        config: CopyConfig,
        provider: OperationDataStream,
        send: Arc<dyn Fn(CopySendAction) -> bool + Send + Sync>,
        latest_issued: Arc<dyn Fn() -> crate::Result<Lsn> + Send + Sync>,
        clock: Clock,
    ) -> Result<(), CopyError> {
        {
            let mut state = self.root.state.lock().map_err(|_| CopyError::Poisoned)?;
            if state.begun {
                return Err(CopyError::AlreadyStarted);
            }
            if state.early_error.is_some() {
                state.begun = true;
                return Ok(());
            }
        }
        let last_root = self.root.clone();
        let last = Arc::new(move |copy_lsn| {
            let handle = CopySenderHandle {
                root: Arc::downgrade(&last_root),
            };
            let result = latest_issued()
                .map_err(|error| CopyError::ReplicationTarget(Arc::new(error)))
                .and_then(|replication_lsn| {
                    handle.on_last_enumerator_copy_op(copy_lsn, replication_lsn)
                });
            if let Err(error) = result {
                handle
                    .finish(error)
                    .expect("EOF failure completion must retain an unpoisoned owner");
            }
        });
        let operation = CopyAsyncOperation::new(config, provider, send, last, clock)?;
        assert!(self.root.operation.set(operation.handle()).is_ok());
        self.operation = Some(operation);
        let error = {
            let mut state = self.root.state.lock().map_err(|_| CopyError::Poisoned)?;
            state.begun = true;
            state.copy.phase = CopyPhase::Started;
            state.active = state.early_error.is_none();
            state.early_error.clone()
        };
        if let Some(error) = error {
            self.handle().finish(error)?;
        }
        Ok(())
    }

    pub(super) async fn drive(&mut self) -> Result<(), CopyError> {
        match &mut self.operation {
            Some(operation) => operation.drive().await,
            None => {
                let state = self.root.state.lock().map_err(|_| CopyError::Poisoned)?;
                if !state.begun {
                    return Err(CopyError::NotStarted);
                }
                Err(state
                    .early_error
                    .clone()
                    .expect("Begin without enumeration has a stored failure"))
            }
        }
    }
}

#[cfg(test)]
mod tests;
