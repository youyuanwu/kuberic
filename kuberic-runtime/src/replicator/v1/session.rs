// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Selected RemoteSession.h/.cpp and ReplicationSession.h/.cpp responsibilities:
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d
// src/prod/src/Reliability/Replication/. See sf-v1-port-provenance.md.

//! Bounded, unselected session coordination, NOT a complete ReplicationSession.
//! No carrier, demultiplexer, copy/build/quorum/role owner or slow-fault timer.
//! Copy/primary handoffs are explicit obligations, never successful stubs.
//! Short transitions own a mutex; every external effect executes after release.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::application::Lsn;
use crate::protocol::types::Epoch;

use super::operation_queue::QueueOperation;
use super::sender::{
    ReliableOperationSender, SendAction, SendBatch, SenderConfig, SenderOperation, SenderProgress,
};

/// Fixed logical identity supplied by the future demultiplexing owner. Merely
/// storing it does not validate inbound transport identity or role admission.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 session identity")
)]
pub(super) struct SessionIdentity {
    pub(super) primary_endpoint: String,
    pub(super) remote_endpoint: String,
    pub(super) remote_incarnation: i64,
}

#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 send intent"))]
pub(super) enum SendKind {
    StartCopy {
        replication_start: Lsn,
        epoch: Epoch,
    },
    Replication {
        operation: Arc<QueueOperation>,
        original_epoch: Epoch,
        sending_epoch: Epoch,
        completed_hint: Lsn,
    },
    // Native RequestAck has no operation epoch or completed-hint body.
    RequestAck,
}

#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 send intent"))]
pub(super) struct SendIntent {
    pub(super) identity: SessionIdentity,
    pub(super) kind: SendKind,
    /// None is infinite expiration, not a guessed carrier timeout.
    pub(super) expires_after: Option<Duration>,
}

/// The later carrier must map ONLY send-queue-full to false. This callback is
/// not a transport implementation, fault reporter, receipt or application ACK.
type SendCallback = Arc<dyn Fn(SendIntent) -> bool + Send + Sync>;
type ProgressCallback = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AckSnapshot {
    replication_received: Lsn,
    replication_applied: Lsn,
    copy_received: Lsn,
    copy_applied: Lsn,
    process_copy: bool,
}

#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 ACK ingress"))]
pub(super) struct AckUpdate {
    pub(super) replication_received: Lsn,
    pub(super) replication_applied: Lsn,
    pub(super) copy_received: Lsn,
    pub(super) copy_applied: Lsn,
    pub(super) process_copy: bool,
    pub(super) message_id: String,
}

struct State {
    identity: SessionIdentity,
    sending_epoch: Epoch,
    retry: Duration,
    active: bool,
    sender: ReliableOperationSender,
    // Establishment owns admission; the session's ACK processor closes retries.
    start_copy: Option<ReliableOperationSender>,
    send: SendCallback,
    cached: AckSnapshot,
    processing: bool,
    latest_message_id: String,
}

#[derive(Clone)]
#[cfg_attr(not(test), expect(dead_code, reason = "unselected bounded V1 session"))]
pub(super) struct ReplicationSession {
    state: Arc<Mutex<State>>,
}

#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 session API"))]
impl ReplicationSession {
    pub(super) fn new(
        identity: SessionIdentity,
        sending_epoch: Epoch,
        last_ack: Lsn,
        config: SenderConfig,
        send: SendCallback,
        now: Duration,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                identity,
                sending_epoch,
                retry: config.retry,
                active: false,
                sender: ReliableOperationSender::new(config, last_ack, now),
                start_copy: None,
                send,
                cached: AckSnapshot {
                    replication_received: 0,
                    replication_applied: 0,
                    copy_received: 0,
                    copy_applied: 0,
                    process_copy: true,
                },
                processing: false,
                latest_message_id: String::new(),
            })),
        }
    }

    pub(super) fn open(&self) {
        let mut state = self.state.lock().unwrap();
        assert!(!state.active);
        state.sender.open();
        state.active = true;
    }

    pub(super) fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.active = false;
        state.sender.close();
        if let Some(start_copy) = state.start_copy.as_mut() {
            start_copy.close();
        }
        // No processor drain, ACK-ingress rejection, CancelCopy or task abort.
    }

    pub(super) fn identity(&self) -> SessionIdentity {
        self.state.lock().unwrap().identity.clone()
    }

    pub(super) fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    pub(super) fn update_sending_epoch(&self, epoch: Epoch) {
        let mut state = self.state.lock().unwrap();
        assert!(epoch >= state.sending_epoch);
        state.sending_epoch = epoch;
    }

    pub(super) fn install_start_copy_sender(&self, sender: ReliableOperationSender) {
        let mut state = self.state.lock().unwrap();
        assert!(state.start_copy.is_none());
        state.start_copy = Some(sender);
    }

    pub(super) fn start_copy(
        &self,
        config: SenderConfig,
        replication_start: Lsn,
        now: Duration,
    ) -> SessionSendBatch {
        let mut state = self.state.lock().unwrap();
        assert!(state.active && state.start_copy.is_none());
        let mut sender = ReliableOperationSender::new(config, 0, now);
        sender.open();
        let batch = sender.add(
            SenderOperation {
                operation: QueueOperation::new(
                    replication_start,
                    crate::application::OperationData::new(),
                    false,
                ),
                original_epoch: state.sending_epoch,
            },
            0,
            now,
        );
        state.start_copy = Some(sender);
        SessionSendBatch {
            session: self.clone(),
            batch,
            start_copy: true,
        }
    }

    pub(super) fn start_copy_deadline(&self) -> Option<Duration> {
        self.state
            .lock()
            .unwrap()
            .start_copy
            .as_ref()
            .and_then(ReliableOperationSender::deadline)
    }

    pub(super) fn on_start_copy_timer(&self, now: Duration) -> Option<SessionSendBatch> {
        let batch = self
            .state
            .lock()
            .unwrap()
            .start_copy
            .as_mut()?
            .on_timer(now);
        Some(SessionSendBatch {
            session: self.clone(),
            batch,
            start_copy: true,
        })
    }

    pub(super) fn close_start_copy(&self) {
        let sender = self.state.lock().unwrap().start_copy.take();
        if let Some(mut sender) = sender {
            sender.close();
        }
    }

    pub(super) fn add(
        &self,
        operation: SenderOperation,
        hint: Lsn,
        now: Duration,
    ) -> SessionSendBatch {
        let batch = self.state.lock().unwrap().sender.add(operation, hint, now);
        self.root_batch(batch)
    }

    pub(super) fn add_replay(
        &self,
        operations: Vec<SenderOperation>,
        hint: Lsn,
        now: Duration,
    ) -> SessionSendBatch {
        let batch = self
            .state
            .lock()
            .unwrap()
            .sender
            .add_replay(operations, hint, now);
        self.root_batch(batch)
    }

    pub(super) fn on_timer(&self, now: Duration) -> SessionSendBatch {
        let batch = self.state.lock().unwrap().sender.on_timer(now);
        self.root_batch(batch)
    }

    pub(super) fn retry_deadline(&self) -> Option<Duration> {
        self.state.lock().unwrap().sender.deadline()
    }

    fn root_batch(&self, batch: SendBatch) -> SessionSendBatch {
        SessionSendBatch {
            session: self.clone(),
            batch,
            start_copy: false,
        }
    }

    pub(super) fn progress(&self, completed_hint: Lsn) -> SenderProgress {
        self.state.lock().unwrap().sender.progress(completed_hint)
    }

    pub(super) fn receive_lsn(&self, completed_hint: Lsn) -> Lsn {
        self.progress(completed_hint).received.max(0)
    }

    pub(super) fn apply_lsn(&self, completed_hint: Lsn) -> Lsn {
        self.progress(completed_hint).applied.max(0)
    }

    /// The copy owner supplies its known finish target; no target is selected
    /// or build completed by this view.
    pub(super) fn idle_progress(&self, finish_copy_target: Option<Lsn>) -> Lsn {
        self.progress(0).idle_progress(finish_copy_target)
    }

    pub(super) fn averages(&self) -> (Duration, Duration) {
        self.state.lock().unwrap().sender.averages()
    }

    pub(super) fn reset_average_statistics(&self, now: Duration) {
        self.state
            .lock()
            .unwrap()
            .sender
            .reset_average_statistics(now);
    }

    pub(super) fn is_processing(&self) -> bool {
        self.state.lock().unwrap().processing
    }

    /// None means duplicate or coalesced, not completed processing. There is
    /// deliberately no active check: native ACK processing can outlive close.
    pub(super) fn update_ack(
        &self,
        update: AckUpdate,
        progress_callback: Option<ProgressCallback>,
    ) -> Option<AckProcessor> {
        let mut state = self.state.lock().unwrap();
        let cached = &mut state.cached;
        let mut changed = false;
        if update.replication_received > cached.replication_received
            || update.replication_applied > cached.replication_applied
        {
            // Replace the PAIR, not independent max. Sender is monotone later.
            cached.replication_received = update.replication_received;
            cached.replication_applied = update.replication_applied;
            cached.process_copy = update.process_copy;
            changed = true;
        }
        if update.process_copy {
            for (current, incoming) in [
                (&mut cached.copy_received, update.copy_received),
                (&mut cached.copy_applied, update.copy_applied),
            ] {
                if *current != -1 && (incoming > *current || incoming == -1) {
                    *current = incoming;
                    cached.process_copy = true;
                    changed = true;
                }
            }
        } else {
            cached.process_copy = false; // Even a replication-only duplicate.
        }
        if !changed {
            return None;
        }
        state.latest_message_id = update.message_id;
        if state.processing {
            return None;
        }
        state.processing = true;
        let mut snapshot = state.cached;
        snapshot.process_copy = update.process_copy;
        Some(AckProcessor {
            session: self.clone(),
            snapshot,
            callback: progress_callback,
            stage: Stage::Replication,
            replication_advanced: false,
        })
    }
}

/// Retains the session and its callback throughout outside-guard dispatch.
#[cfg_attr(not(test), expect(dead_code, reason = "unselected rooted V1 sends"))]
pub(super) struct SessionSendBatch {
    session: ReplicationSession,
    batch: SendBatch,
    start_copy: bool,
}

#[cfg_attr(not(test), expect(dead_code, reason = "unselected rooted V1 sends"))]
impl SessionSendBatch {
    pub(super) fn dispatch(self) {
        let session = self.session;
        self.batch.dispatch(|action, hint| {
            let (intent, send) = {
                let state = session.state.lock().unwrap();
                let (kind, expires_after) = match action {
                    SendAction::Operation(entry) if self.start_copy => (
                        SendKind::StartCopy {
                            replication_start: entry.operation.lsn(),
                            epoch: state.sending_epoch,
                        },
                        None,
                    ),
                    SendAction::Operation(entry) => (
                        SendKind::Replication {
                            operation: entry.operation,
                            original_epoch: entry.original_epoch,
                            sending_epoch: state.sending_epoch,
                            completed_hint: hint,
                        },
                        Some(state.retry),
                    ),
                    SendAction::RequestAck => {
                        assert!(!self.start_copy, "StartCopy never requests an ACK");
                        (SendKind::RequestAck, None)
                    }
                };
                (
                    SendIntent {
                        identity: state.identity.clone(),
                        kind,
                        expires_after,
                    },
                    state.send.clone(),
                )
            };
            send(intent) // No guard, even if callback reenters or closes session.
        });
    }
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected primary notification")
)]
pub(super) struct ProgressNotification(ProgressCallback);

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected primary notification")
)]
impl ProgressNotification {
    pub(super) fn dispatch(self) {
        (self.0)();
    }
}

/// These are ordered HANDOFFS. Copy effects are unresolved until an actual
/// future owner consumes them. They are not copy/quorum completion results.
#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 ACK handoffs"))]
pub(super) enum AckAction {
    ReplicationSends(SessionSendBatch),
    CopyReplicationGate {
        copy_applied: Lsn,
        replication_applied: Lsn,
    },
    PrimaryProgress(ProgressNotification),
    CopyAck {
        received: Lsn,
        applied: Lsn,
    },
}

enum Stage {
    Replication,
    CopyGate,
    Primary,
    Copy,
    Finish,
}

/// Move-only rooted processor. Must be continued through all handoffs, including
/// after close. Dropping it is not a cancellation/success protocol: the session
/// stays processing rather than silently declaring unfinished work complete.
#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 ACK processor"))]
pub(super) struct AckProcessor {
    session: ReplicationSession,
    snapshot: AckSnapshot,
    callback: Option<ProgressCallback>,
    stage: Stage,
    replication_advanced: bool,
}

#[must_use = "deliver the handoff outside guards and continue the rooted processor"]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 ACK continuation")
)]
pub(super) struct AckHandoff {
    action: AckAction,
    continuation: AckProcessor,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "unselected V1 ACK continuation")
)]
impl AckHandoff {
    /// Delivery acknowledgement only, not successful future-owner execution.
    /// Async integration can own the handoff across an await; no guard is held.
    pub(super) fn deliver(self, deliver: impl FnOnce(AckAction)) -> AckProcessor {
        deliver(self.action);
        self.continuation
    }
}

#[cfg_attr(not(test), expect(dead_code, reason = "unselected V1 ACK processor"))]
impl AckProcessor {
    pub(super) fn next(mut self, now: Duration) -> Option<AckHandoff> {
        loop {
            let action = match self.stage {
                Stage::Replication => {
                    let result = {
                        let mut state = self.session.state.lock().unwrap();
                        if self.snapshot.process_copy
                            && self.snapshot.replication_received >= 0
                            && let Some(start_copy) = state.start_copy.as_mut()
                        {
                            start_copy.close();
                        }
                        state.sender.on_ack(
                            self.snapshot.replication_received,
                            self.snapshot.replication_applied,
                            now,
                        )
                    };
                    self.replication_advanced = result.advanced;
                    self.stage = Stage::CopyGate;
                    AckAction::ReplicationSends(self.session.root_batch(result.sends))
                }
                Stage::CopyGate => {
                    // Sample AFTER replication dispatch; it can reenter.
                    let replication_applied = self.session.progress(0).applied;
                    self.stage = Stage::Primary;
                    AckAction::CopyReplicationGate {
                        copy_applied: self.snapshot.copy_applied,
                        replication_applied,
                    }
                }
                Stage::Primary => {
                    self.stage = Stage::Copy;
                    if self.replication_advanced
                        && let Some(callback) = self.callback.as_ref()
                    {
                        AckAction::PrimaryProgress(ProgressNotification(callback.clone()))
                    } else {
                        continue;
                    }
                }
                Stage::Copy => {
                    self.stage = Stage::Finish;
                    if !self.snapshot.process_copy {
                        continue;
                    }
                    AckAction::CopyAck {
                        received: self.snapshot.copy_received,
                        applied: self.snapshot.copy_applied,
                    }
                }
                Stage::Finish => {
                    let mut state = self.session.state.lock().unwrap();
                    let latest = state.cached;
                    let newer_copy = |old: Lsn, new: Lsn| old != -1 && (new > old || new == -1);
                    let newer = latest.replication_received > self.snapshot.replication_received
                        || latest.replication_applied > self.snapshot.replication_applied
                        || newer_copy(self.snapshot.copy_received, latest.copy_received)
                        || newer_copy(self.snapshot.copy_applied, latest.copy_applied);
                    if !newer {
                        // Atomic with update_ack's processor-launch decision.
                        state.processing = false;
                        return None;
                    }
                    self.snapshot = latest;
                    self.stage = Stage::Replication;
                    continue;
                }
            };
            return Some(AckHandoff {
                action,
                continuation: self,
            });
        }
    }
}

#[cfg(test)]
mod tests;
