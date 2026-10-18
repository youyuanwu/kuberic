// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-component vectors from CopySender.cpp:56-406,478-581,
// RemoteSession.cpp:367-385,585-615 and
// PrimaryReplicator.BuildIdleAsyncOperation.cpp:111-166,
// microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d.

use super::*;

use std::pin::Pin;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt, stream};

use crate::protocol::types::Epoch;
use crate::replicator::v1::buffers::{BufferLimits, BufferPolicy};
use crate::replicator::v1::copy_context_receiver::{CopyContextConfig, CopyContextReceiver};
use crate::replicator::v1::operation_queue::{QueueConfig, QueueOperation};
use crate::replicator::v1::operation_stream::StreamEvent;
use crate::replicator::v1::received_operation::ReceivedOperation;
use crate::replicator::v1::replication_queue_manager::ReplicationQueueManager;
use crate::replicator::v1::secondary_copy_receiver::{SecondaryCopyConfig, SecondaryCopyReceiver};
use crate::replicator::v1::sender::SenderConfig;
use crate::replicator::v1::session::{AckAction, AckUpdate, ReplicationSession, SessionIdentity};

fn config(capacity: usize) -> CopyConfig {
    CopyConfig {
        sender: SenderConfig {
            initial_window: capacity,
            max_window: capacity,
            retry: Duration::from_secs(1),
            decay_factor: 0.5,
            decay_interval: Duration::from_secs(1),
        },
        buffer_policy: BufferPolicy::RawSingle,
        buffer_limits: BufferLimits {
            max_segments: 16,
            max_payload_bytes: 1024,
        },
    }
}

struct Provider {
    remaining: usize,
    polls: Arc<AtomicUsize>,
}

impl Stream for Provider {
    type Item = crate::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        if self.remaining == 0 {
            Poll::Ready(None)
        } else {
            self.remaining -= 1;
            Poll::Ready(Some(Ok(Bytes::from_static(b"copy"))))
        }
    }
}

struct Rig {
    sender: CopySender,
    handle: CopySenderHandle,
    latest: Arc<AtomicI64>,
    captures: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
    sent: Arc<Mutex<Vec<(Lsn, bool)>>>,
}

impl Rig {
    fn new(items: usize, capacity: usize, wait: bool, latest: Lsn) -> Self {
        let latest = Arc::new(AtomicI64::new(latest));
        let captures = Arc::new(AtomicUsize::new(0));
        let polls = Arc::new(AtomicUsize::new(0));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut sender = CopySender::new(wait);
        let handle = sender.handle();
        let get_latest = latest.clone();
        let captured = captures.clone();
        let sends = sent.clone();
        sender
            .begin(
                config(capacity),
                Box::pin(Provider {
                    remaining: items,
                    polls: polls.clone(),
                }),
                Arc::new(move |action| {
                    if let CopySendAction::Operation { operation, is_last } = action {
                        sends.lock().unwrap().push((operation.lsn(), is_last));
                    }
                    true
                }),
                Arc::new(move || {
                    captured.fetch_add(1, Ordering::SeqCst);
                    Ok(get_latest.load(Ordering::SeqCst))
                }),
                Clock::new(),
            )
            .unwrap();
        Self {
            sender,
            handle,
            latest,
            captures,
            polls,
            sent,
        }
    }

    fn empty(target: Lsn) -> Self {
        Self::new(0, 4, true, target)
    }

    fn snapshot(&self) -> CopySenderSnapshot {
        self.handle.snapshot().unwrap()
    }

    async fn pending(&mut self) {
        assert!(poll_drive(&mut self.sender).await.is_pending());
    }

    fn copy_ack(&self, received: Lsn, applied: Lsn) {
        assert!(self.handle.process_copy_ack(received, applied).unwrap());
    }

    fn gate(&self, applied: Lsn) -> bool {
        self.handle.process_replication_ack(-1, applied).unwrap()
    }
}

async fn poll_drive(sender: &mut CopySender) -> Poll<Result<(), CopyError>> {
    let future = sender.drive();
    tokio::pin!(future);
    futures::poll!(future)
}

fn session() -> ReplicationSession {
    let session = ReplicationSession::new(
        SessionIdentity {
            primary_endpoint: "primary/1".into(),
            remote_endpoint: "secondary/2".into(),
            remote_incarnation: 2,
        },
        Epoch::new(1, 1),
        0,
        config(4).sender,
        Arc::new(|_| true),
        Duration::ZERO,
    );
    session.open();
    session
}

fn queue() -> ReplicationQueueManager {
    ReplicationQueueManager::new(
        QueueConfig {
            initial_capacity: 8,
            max_capacity: 8,
            max_bytes: 0,
            max_completed_count: 0,
            max_completed_bytes: 0,
            clean_on_complete: true,
        },
        0,
    )
}

fn issue(queue: &mut ReplicationQueueManager, session: &ReplicationSession) -> Lsn {
    let operation = queue
        .enqueue(
            Bytes::from_static(b"replication"),
            Epoch::new(1, 1),
            BufferPolicy::RawSingle,
            config(4).buffer_limits,
        )
        .unwrap();
    let lsn = operation.operation.lsn();
    session.add(operation, 0, Duration::ZERO).dispatch();
    lsn
}

fn deliver_ack(
    session: &ReplicationSession,
    handle: &CopySenderHandle,
    received: Lsn,
    applied: Lsn,
    copy_received: Lsn,
    copy_applied: Lsn,
) -> Vec<&'static str> {
    let mut processor = session
        .update_ack(
            AckUpdate {
                replication_received: received,
                replication_applied: applied,
                copy_received,
                copy_applied,
                process_copy: true,
                message_id: "actual-copy-build-ack".into(),
            },
            Some(Arc::new(|| {})),
        )
        .unwrap();
    let mut events = Vec::new();
    while let Some(handoff) = processor.next(Duration::ZERO) {
        processor = handoff.deliver(|action| match action {
            AckAction::ReplicationSends(batch) => {
                events.push("replication");
                batch.dispatch();
            }
            AckAction::CopyReplicationGate {
                copy_applied,
                replication_applied,
            } => {
                events.push("gate");
                handle
                    .process_replication_ack(copy_applied, replication_applied)
                    .unwrap();
            }
            AckAction::PrimaryProgress(notification) => {
                events.push("primary");
                notification.dispatch();
            }
            AckAction::CopyAck { received, applied } => {
                events.push("copy");
                handle.process_copy_ack(received, applied).unwrap();
            }
        });
    }
    events
}

#[tokio::test(start_paused = true)]
async fn drive_and_end_require_begin() {
    let mut sender = CopySender::new(true);
    assert!(matches!(sender.drive().await, Err(CopyError::NotStarted)));
    assert!(matches!(sender.handle().end(), Err(CopyError::NotStarted)));
    assert_eq!(
        sender.handle().snapshot().unwrap().phase,
        CopyPhase::NotStarted
    );
}

#[tokio::test(start_paused = true)]
async fn empty_eof_sets_both_targets_without_completing_build() {
    let mut rig = Rig::empty(7);
    assert_eq!(rig.snapshot().phase, CopyPhase::Started);
    rig.pending().await;
    let snapshot = rig.snapshot();
    assert_eq!(snapshot.phase, CopyPhase::LsnSet);
    assert_eq!(snapshot.last_copy, Some(1));
    assert_eq!(snapshot.replication_target, Some(7));
    assert!(snapshot.result.is_none());
    assert_eq!(rig.sent.lock().unwrap().as_slice(), [(1, true)]);
    assert_eq!(rig.captures.load(Ordering::SeqCst), 1);
    assert!(matches!(rig.handle.end(), Err(CopyError::NotComplete)));
}

#[tokio::test(start_paused = true)]
async fn preterminal_replication_ack_does_not_freeze_build_gate() {
    let mut rig = Rig::empty(5);
    assert!(!rig.handle.process_replication_ack(0, 99).unwrap());
    rig.pending().await;
    assert!(!rig.handle.process_replication_ack(0, 99).unwrap());
    rig.copy_ack(-1, -1);
    assert_eq!(rig.snapshot().phase, CopyPhase::LsnSet);
    assert!(rig.snapshot().result.is_none());
    assert!(rig.gate(5));
    assert!(rig.handle.end().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn final_copy_first_waits_for_replication_target() {
    let mut rig = Rig::empty(5);
    rig.pending().await;
    rig.copy_ack(-1, -1);
    assert!(rig.snapshot().enumeration.unwrap().ready_to_complete);
    assert!(!rig.gate(4));
    assert!(rig.snapshot().result.is_none());
    assert!(rig.gate(5));
    assert!(matches!(
        poll_drive(&mut rig.sender).await,
        Poll::Ready(Ok(()))
    ));
    assert!(rig.handle.end().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn replication_gate_first_waits_for_final_copy_application() {
    let mut rig = Rig::new(1, 4, true, 5);
    rig.pending().await;
    assert!(rig.handle.process_replication_ack(2, 5).unwrap());
    assert_eq!(rig.snapshot().phase, CopyPhase::ReplCompleted);
    assert!(rig.snapshot().result.is_none());
    rig.copy_ack(2, 1);
    assert!(rig.snapshot().result.is_none());
    rig.copy_ack(2, 2);
    assert!(rig.handle.end().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn actual_session_processor_delivers_replication_gate_primary_then_copy() {
    let session = session();
    let mut queue = queue();
    issue(&mut queue, &session);
    let mut rig = Rig::empty(1);
    rig.pending().await;
    assert_eq!(
        deliver_ack(&session, &rig.handle, 1, 1, -1, -1),
        ["replication", "gate", "primary", "copy"]
    );
    assert!(!session.is_processing());
    assert!(rig.handle.end().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn zero_replication_target_still_requires_terminal_copy_proof() {
    let mut rig = Rig::empty(0);
    rig.pending().await;
    assert!(!rig.handle.process_replication_ack(0, 0).unwrap());
    assert!(rig.snapshot().result.is_none());
    assert!(rig.gate(0));
    assert!(rig.snapshot().result.is_none());
    rig.copy_ack(-1, -1);
    assert!(rig.handle.end().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn terminal_receive_sentinel_suppresses_retries_without_application_or_success() {
    let mut rig = Rig::new(1, 4, true, 3);
    rig.pending().await;
    rig.copy_ack(-1, 0);
    let copy = rig.snapshot().enumeration.unwrap();
    assert_eq!(copy.queued, 2);
    assert!(!copy.ready_to_complete);
    let before = rig.sent.lock().unwrap().len();
    tokio::time::advance(Duration::from_secs(2)).await;
    rig.pending().await;
    assert_eq!(rig.sent.lock().unwrap().len(), before);
    assert!(rig.snapshot().result.is_none());
}

#[tokio::test(start_paused = true)]
async fn applied_sentinel_with_zero_receive_preserves_skip_sender_branch() {
    let mut rig = Rig::empty(2);
    rig.pending().await;
    rig.copy_ack(0, -1);
    let copy = rig.snapshot().enumeration.unwrap();
    assert!(copy.ready_to_complete);
    assert_eq!(copy.queued, 0);
    assert!(rig.snapshot().result.is_none());
    assert!(rig.gate(2));
    assert!(rig.handle.end().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn receive_sentinel_and_partial_applied_ack_keep_distinct_cursors() {
    let mut rig = Rig::new(2, 4, true, 3);
    rig.pending().await;
    rig.copy_ack(-1, 1);
    let copy = rig.snapshot().enumeration.unwrap();
    assert_eq!(copy.last_admitted, 3);
    assert_eq!(copy.queued, 2);
    assert!(!copy.ready_to_complete);
    assert!(rig.snapshot().result.is_none());
}

#[tokio::test(start_paused = true)]
async fn sentinel_without_known_eof_is_rejected_without_progress() {
    let mut rig = Rig::new(8, 2, true, 3);
    rig.pending().await;
    assert_eq!(rig.snapshot().last_copy, None);
    for pair in [(-1, 0), (0, -1), (-1, -1)] {
        assert!(matches!(
            rig.handle.process_copy_ack(pair.0, pair.1),
            Err(CopyError::InvalidAck)
        ));
    }
    assert_eq!(rig.snapshot().enumeration.unwrap().queued, 2);
}

#[tokio::test(start_paused = true)]
async fn invalid_copy_ack_is_explicit_and_does_not_detach_build() {
    let mut rig = Rig::empty(1);
    rig.pending().await;
    for pair in [(-2, 0), (0, -2), (2, 1), (1, 2)] {
        assert!(matches!(
            rig.handle.process_copy_ack(pair.0, pair.1),
            Err(CopyError::InvalidAck)
        ));
    }
    assert!(!rig.handle.process_copy_ack(0, 0).unwrap());
    assert!(rig.gate(1));
    rig.copy_ack(-1, -1);
    assert!(rig.handle.end().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn duplicate_and_regressive_copy_acks_do_not_regress_queue() {
    let mut rig = Rig::new(2, 4, true, 3);
    rig.pending().await;
    rig.copy_ack(2, 2);
    rig.copy_ack(2, 2);
    rig.copy_ack(1, 1);
    assert_eq!(rig.snapshot().enumeration.unwrap().queued, 1);
    rig.copy_ack(-1, -1);
    assert!(rig.gate(3));
    assert!(rig.handle.end().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn parked_eof_captures_once_before_terminal_can_be_sent_or_acked() {
    let mut rig = Rig::new(2, 2, true, 3);
    rig.pending().await;
    assert_eq!(rig.snapshot().last_copy, Some(3));
    assert_eq!(rig.snapshot().enumeration.unwrap().parked, Some(3));
    assert_eq!(rig.captures.load(Ordering::SeqCst), 1);
    assert!(matches!(
        rig.handle.process_copy_ack(-1, -1),
        Err(CopyError::InvalidAck)
    ));
    rig.latest.store(8, Ordering::SeqCst);
    rig.copy_ack(1, 1);
    rig.pending().await;
    assert_eq!(rig.snapshot().replication_target, Some(3));
    assert_eq!(rig.captures.load(Ordering::SeqCst), 1);
    assert!(rig.sent.lock().unwrap().contains(&(3, true)));
    rig.copy_ack(-1, -1);
    assert!(rig.gate(3));
    assert!(rig.handle.end().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn eof_reads_real_primary_issued_tail_not_creation_time_or_committed_snapshot() {
    let session = session();
    let queue = Arc::new(Mutex::new(queue()));
    issue(&mut queue.lock().unwrap(), &session);
    let mut sender = CopySender::new(true);
    let source = queue.clone();
    sender
        .begin(
            config(4),
            Box::pin(stream::empty()),
            Arc::new(|_| true),
            Arc::new(move || Ok(source.lock().unwrap().last_sequence_number())),
            Clock::new(),
        )
        .unwrap();
    issue(&mut queue.lock().unwrap(), &session);
    assert_eq!(queue.lock().unwrap().current_progress(), 0);
    assert!(poll_drive(&mut sender).await.is_pending());
    let handle = sender.handle();
    assert_eq!(handle.snapshot().unwrap().replication_target, Some(2));
    issue(&mut queue.lock().unwrap(), &session);
    assert_eq!(handle.snapshot().unwrap().replication_target, Some(2));
    handle.process_copy_ack(-1, -1).unwrap();
    assert!(!handle.process_replication_ack(-1, 1).unwrap());
    assert!(handle.process_replication_ack(-1, 2).unwrap());
    assert!(handle.end().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn retarget_before_eof_is_noop_and_negative_target_is_error() {
    let mut rig = Rig::empty(3);
    assert!(
        !rig.handle
            .update_last_replication_operation_during_copy(9)
            .unwrap()
    );
    assert!(matches!(
        rig.handle.update_last_replication_operation_during_copy(-1),
        Err(CopyError::InvalidReplicationProgress)
    ));
    rig.pending().await;
    assert_eq!(rig.snapshot().replication_target, Some(3));
}

#[tokio::test(start_paused = true)]
async fn pending_retarget_only_increases_and_old_target_no_longer_completes() {
    let mut rig = Rig::empty(3);
    rig.pending().await;
    assert!(
        !rig.handle
            .update_last_replication_operation_during_copy(2)
            .unwrap()
    );
    assert!(
        !rig.handle
            .update_last_replication_operation_during_copy(3)
            .unwrap()
    );
    assert!(
        rig.handle
            .update_last_replication_operation_during_copy(8)
            .unwrap()
    );
    rig.copy_ack(-1, -1);
    assert!(!rig.gate(3));
    assert!(!rig.gate(7));
    assert!(rig.gate(8));
    assert!(rig.handle.end().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn satisfied_replication_gate_cannot_be_reopened_by_retarget() {
    let mut rig = Rig::empty(3);
    rig.pending().await;
    assert!(rig.gate(3));
    assert!(
        !rig.handle
            .update_last_replication_operation_during_copy(8)
            .unwrap()
    );
    assert_eq!(rig.snapshot().replication_target, Some(3));
    rig.copy_ack(-1, -1);
    assert!(rig.handle.end().unwrap().result.is_ok());
    assert!(
        !rig.handle
            .update_last_replication_operation_during_copy(9)
            .unwrap()
    );
}

#[tokio::test(start_paused = true)]
async fn actual_idle_sender_and_primary_queue_release_more_retention_after_retarget() {
    let session = session();
    let mut queue = queue();
    issue(&mut queue, &session);
    issue(&mut queue, &session);
    let mut rig = Rig::empty(queue.last_sequence_number());
    rig.pending().await;
    issue(&mut queue, &session);
    issue(&mut queue, &session);
    deliver_ack(&session, &rig.handle, 4, 0, 0, 0);
    let before = session.idle_progress(rig.snapshot().replication_target);
    assert_eq!(before, 2);
    queue.update_queue(Some(4), Some(before)).unwrap();
    assert_eq!(queue.operation_count(), 2);
    assert!(
        rig.handle
            .update_last_replication_operation_during_copy(queue.last_sequence_number())
            .unwrap()
    );
    let after = session.idle_progress(rig.snapshot().replication_target);
    assert_eq!(after, 4);
    queue.update_queue(Some(4), Some(after)).unwrap();
    assert_eq!(queue.operation_count(), 0);
    assert_eq!(rig.snapshot().phase, CopyPhase::LsnSet);
    assert!(rig.snapshot().result.is_none());
    deliver_ack(&session, &rig.handle, 4, 4, -1, -1);
    assert!(rig.handle.end().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn context_mode_needs_only_final_copy_and_has_no_replication_target() {
    let mut rig = Rig::new(1, 4, false, 99);
    rig.pending().await;
    assert_eq!(rig.snapshot().phase, CopyPhase::ReplCompleted);
    assert_eq!(rig.snapshot().replication_target, None);
    rig.copy_ack(-1, 1);
    assert!(rig.snapshot().result.is_none());
    rig.copy_ack(-1, -1);
    assert!(rig.handle.end().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn context_retarget_is_explicitly_unsupported() {
    let mut rig = Rig::new(0, 4, false, 0);
    rig.pending().await;
    assert!(matches!(
        rig.handle.update_last_replication_operation_during_copy(5),
        Err(CopyError::ReplicationNotRequired)
    ));
}

#[tokio::test(start_paused = true)]
async fn disable_before_eof_blocks_success_but_not_copy_queue_release() {
    let mut rig = Rig::empty(3);
    assert!(rig.handle.try_disable_build_completion().unwrap());
    rig.pending().await;
    assert!(!rig.gate(3));
    rig.copy_ack(-1, -1);
    assert_eq!(rig.snapshot().enumeration.unwrap().queued, 0);
    assert!(rig.snapshot().result.is_none());
    rig.handle.cancel().unwrap();
    assert!(matches!(
        rig.handle.end().unwrap().result,
        Err(CopyError::Cancelled)
    ));
}

#[tokio::test(start_paused = true)]
async fn disable_after_replication_gate_still_blocks_copy_completion() {
    let mut rig = Rig::empty(3);
    rig.pending().await;
    assert!(rig.gate(3));
    assert!(rig.handle.try_disable_build_completion().unwrap());
    rig.copy_ack(-1, -1);
    assert!(rig.snapshot().result.is_none());
    rig.handle.finish(CopyError::Cancelled).unwrap();
    assert!(matches!(
        rig.handle.end().unwrap().result,
        Err(CopyError::Cancelled)
    ));
}

#[tokio::test(start_paused = true)]
async fn already_completed_async_operation_cannot_be_disabled_before_or_after_end() {
    let mut rig = Rig::empty(0);
    rig.pending().await;
    assert!(rig.gate(0));
    rig.copy_ack(-1, -1);
    assert_eq!(rig.snapshot().phase, CopyPhase::ReplCompleted);
    assert!(!rig.handle.try_disable_build_completion().unwrap());
    rig.handle.end().unwrap();
    assert_eq!(rig.snapshot().phase, CopyPhase::Completed);
    assert!(!rig.handle.try_disable_build_completion().unwrap());
}

#[tokio::test(start_paused = true)]
async fn duplicate_disable_returns_explicit_error() {
    let rig = Rig::empty(0);
    assert!(rig.handle.try_disable_build_completion().unwrap());
    assert!(matches!(
        rig.handle.try_disable_build_completion(),
        Err(CopyError::AlreadyDisabled)
    ));
}

struct DropHook(Arc<dyn Fn() + Send + Sync>);

impl AsRef<[u8]> for DropHook {
    fn as_ref(&self) -> &[u8] {
        b"reentrant-payload"
    }
}

impl Drop for DropHook {
    fn drop(&mut self) {
        (self.0)();
    }
}

#[tokio::test(start_paused = true)]
async fn selected_success_racing_disable_fails_and_faults_at_end_outside_guards() {
    let mut sender = CopySender::new(true);
    let handle = sender.handle();
    let reenter = handle.clone();
    let drops = Arc::new(AtomicUsize::new(0));
    let dropped = drops.clone();
    let payload = Bytes::from_owner(DropHook(Arc::new(move || {
        assert!(reenter.snapshot().unwrap().result.is_none());
        assert!(reenter.try_disable_build_completion().unwrap());
        dropped.fetch_add(1, Ordering::SeqCst);
    })));
    sender
        .begin(
            config(4),
            Box::pin(stream::iter([Ok(payload)])),
            Arc::new(|_| true),
            Arc::new(|| Ok(3)),
            Clock::new(),
        )
        .unwrap();
    assert!(poll_drive(&mut sender).await.is_pending());
    assert!(handle.process_replication_ack(-1, 3).unwrap());
    handle.process_copy_ack(-1, -1).unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(handle.snapshot().unwrap().result.unwrap().is_ok());
    let end = handle.end().unwrap();
    assert!(matches!(
        end.result,
        Err(CopyError::BuildCompletionDisabled)
    ));
    let fault = end.fault.unwrap();
    assert_eq!(fault.description, DISABLED_BUILD_FAULT);
    assert!(
        matches!(&*fault.error, RuntimeError::InvalidReplication(message) if message == DISABLED_BUILD_FAULT)
    );
    assert!(matches!(
        handle.end().unwrap().result,
        Err(CopyError::BuildCompletionDisabled)
    ));
}

#[tokio::test(start_paused = true)]
async fn provider_failure_keeps_end_get_next_fault_not_build_fault() {
    let mut sender = CopySender::new(true);
    sender
        .begin(
            config(4),
            Box::pin(stream::iter([Err(RuntimeError::Application(
                "provider".into(),
            ))])),
            Arc::new(|_| true),
            Arc::new(|| panic!("no EOF on error")),
            Clock::new(),
        )
        .unwrap();
    assert!(matches!(sender.drive().await, Err(CopyError::Provider(_))));
    let end = sender.handle().end().unwrap();
    assert!(matches!(end.result, Err(CopyError::Provider(_))));
    assert_eq!(end.fault.unwrap().description, "EndGetNext");
}

#[tokio::test(start_paused = true)]
async fn eof_target_lookup_failure_completes_without_inventing_provider_fault() {
    let mut sender = CopySender::new(true);
    sender
        .begin(
            config(4),
            Box::pin(stream::empty()),
            Arc::new(|_| true),
            Arc::new(|| Err(RuntimeError::Closed)),
            Clock::new(),
        )
        .unwrap();
    assert!(
        matches!(sender.drive().await, Err(CopyError::ReplicationTarget(error)) if matches!(*error, RuntimeError::Closed))
    );
    let end = sender.handle().end().unwrap();
    assert!(matches!(end.result, Err(CopyError::ReplicationTarget(_))));
    assert!(end.fault.is_none());
    assert_eq!(
        sender.handle().snapshot().unwrap().phase,
        CopyPhase::Completed
    );
}

#[tokio::test(start_paused = true)]
async fn negative_eof_target_is_not_a_successful_empty_build() {
    let mut rig = Rig::empty(-1);
    assert!(matches!(
        rig.sender.drive().await,
        Err(CopyError::InvalidReplicationProgress)
    ));
    let end = rig.handle.end().unwrap();
    assert!(matches!(
        end.result,
        Err(CopyError::InvalidReplicationProgress)
    ));
    assert!(end.fault.is_none());
}

#[tokio::test(start_paused = true)]
async fn finish_before_begin_skips_provider_and_returns_stored_error() {
    let mut sender = CopySender::new(true);
    let handle = sender.handle();
    handle.finish(CopyError::Cancelled).unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let dropped = drops.clone();
    let payload = Bytes::from_owner(DropHook(Arc::new(move || {
        dropped.fetch_add(1, Ordering::SeqCst);
    })));
    sender
        .begin(
            config(4),
            Box::pin(stream::iter([Ok(payload)])),
            Arc::new(|_| panic!("stale copy must not send")),
            Arc::new(|| panic!("stale copy must not enumerate")),
            Clock::new(),
        )
        .unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    handle.finish(CopyError::InvalidAck).unwrap();
    assert!(matches!(sender.drive().await, Err(CopyError::Cancelled)));
    assert!(matches!(
        handle.end().unwrap().result,
        Err(CopyError::Cancelled)
    ));
}

#[tokio::test(start_paused = true)]
async fn finish_operation_is_immediate_even_with_pending_get_next() {
    let mut sender = CopySender::new(true);
    sender
        .begin(
            config(4),
            Box::pin(stream::pending()),
            Arc::new(|_| true),
            Arc::new(|| Ok(0)),
            Clock::new(),
        )
        .unwrap();
    assert!(poll_drive(&mut sender).await.is_pending());
    sender.handle().finish(CopyError::Cancelled).unwrap();
    assert!(matches!(sender.drive().await, Err(CopyError::Cancelled)));
    assert!(matches!(
        sender.handle().end().unwrap().result,
        Err(CopyError::Cancelled)
    ));
}

#[tokio::test(start_paused = true)]
async fn enumeration_cancel_waits_for_inflight_provider_and_discards_returned_data() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut sender = CopySender::new(true);
    sender
        .begin(
            config(4),
            Box::pin(stream::poll_fn(move |cx| rx.poll_recv(cx))),
            Arc::new(|_| panic!("cancelled item must not send")),
            Arc::new(|| panic!("cancelled EOF must not capture")),
            Clock::new(),
        )
        .unwrap();
    assert!(poll_drive(&mut sender).await.is_pending());
    sender.handle().cancel().unwrap();
    assert!(poll_drive(&mut sender).await.is_pending());
    tx.send(Ok(Bytes::new())).unwrap();
    assert!(matches!(sender.drive().await, Err(CopyError::Cancelled)));
    assert!(sender.handle().end().unwrap().fault.is_none());
}

#[tokio::test(start_paused = true)]
async fn late_finish_cannot_overwrite_success_and_end_is_idempotent() {
    let mut rig = Rig::empty(0);
    rig.pending().await;
    assert!(rig.gate(0));
    rig.copy_ack(-1, -1);
    rig.handle.finish(CopyError::Cancelled).unwrap();
    assert!(rig.handle.end().unwrap().result.is_ok());
    assert!(rig.handle.end().unwrap().result.is_ok());
    assert!(!rig.handle.process_copy_ack(-1, -1).unwrap());
}

#[tokio::test(start_paused = true)]
async fn begin_cannot_repeat_even_after_end() {
    let mut rig = Rig::empty(0);
    rig.pending().await;
    rig.handle.finish(CopyError::Cancelled).unwrap();
    rig.handle.end().unwrap();
    assert!(matches!(
        rig.sender.begin(
            config(4),
            Box::pin(stream::empty()),
            Arc::new(|_| true),
            Arc::new(|| Ok(0)),
            Clock::new()
        ),
        Err(CopyError::AlreadyStarted)
    ));
    assert_eq!(rig.polls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn invalid_config_does_not_consume_begin_lifecycle() {
    let mut sender = CopySender::new(true);
    assert!(matches!(
        sender.begin(
            config(1),
            Box::pin(stream::empty()),
            Arc::new(|_| true),
            Arc::new(|| Ok(0)),
            Clock::new()
        ),
        Err(CopyError::InvalidConfig)
    ));
    assert_eq!(
        sender.handle().snapshot().unwrap().phase,
        CopyPhase::NotStarted
    );
    sender
        .begin(
            config(4),
            Box::pin(stream::empty()),
            Arc::new(|_| true),
            Arc::new(|| Ok(0)),
            Clock::new(),
        )
        .unwrap();
    assert!(poll_drive(&mut sender).await.is_pending());
}

#[tokio::test(start_paused = true)]
async fn selected_retry_roots_copy_sender_through_owner_drop_and_end() {
    let mut rig = Rig::empty(0);
    rig.pending().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    let batch = rig
        .sender
        .operation
        .as_ref()
        .unwrap()
        .handle()
        .on_timer()
        .unwrap();
    let handle = rig.handle.clone();
    drop(rig.sender);
    assert_eq!(handle.snapshot().unwrap().last_copy, Some(1));
    handle.finish(CopyError::Cancelled).unwrap();
    assert!(matches!(
        handle.end().unwrap().result,
        Err(CopyError::Cancelled)
    ));
    batch.dispatch();
    assert!(matches!(handle.snapshot(), Err(CopyError::OwnerDropped)));
}

#[tokio::test(start_paused = true)]
async fn provider_poll_and_eof_getter_can_reenter_without_owner_guard() {
    let mut sender = CopySender::new(true);
    let poll_handle = sender.handle();
    let eof_handle = sender.handle();
    sender
        .begin(
            config(4),
            Box::pin(stream::poll_fn(move |_| {
                assert_eq!(poll_handle.snapshot().unwrap().phase, CopyPhase::Started);
                Poll::Ready(None)
            })),
            Arc::new(|_| true),
            Arc::new(move || {
                assert_eq!(eof_handle.snapshot().unwrap().phase, CopyPhase::Started);
                assert!(eof_handle.try_disable_build_completion().unwrap());
                Ok(2)
            }),
            Clock::new(),
        )
        .unwrap();
    assert!(poll_drive(&mut sender).await.is_pending());
    assert!(sender.handle().snapshot().unwrap().disabled);
}

#[tokio::test(start_paused = true)]
async fn send_queue_full_is_retryable_not_ack_or_build_success() {
    let mut sender = CopySender::new(true);
    let sends = Arc::new(AtomicUsize::new(0));
    let sent = sends.clone();
    sender
        .begin(
            config(4),
            Box::pin(stream::empty()),
            Arc::new(move |_| {
                sent.fetch_add(1, Ordering::SeqCst);
                false
            }),
            Arc::new(|| Ok(0)),
            Clock::new(),
        )
        .unwrap();
    assert!(poll_drive(&mut sender).await.is_pending());
    let before = sends.load(Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(poll_drive(&mut sender).await.is_pending());
    assert!(sends.load(Ordering::SeqCst) > before);
    assert!(sender.handle().snapshot().unwrap().result.is_none());
}

#[tokio::test]
async fn real_executor_wakes_and_completes_after_ordered_ack_handoffs() {
    let mut sender = CopySender::new(true);
    let handle = sender.handle();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let tx = Mutex::new(Some(tx));
    sender
        .begin(
            config(4),
            Box::pin(stream::empty()),
            Arc::new(move |action| {
                if let CopySendAction::Operation { is_last: true, .. } = action
                    && let Some(tx) = tx.lock().unwrap().take()
                {
                    tx.send(()).unwrap();
                }
                true
            }),
            Arc::new(|| Ok(0)),
            Clock::new(),
        )
        .unwrap();
    let driver = tokio::spawn(async move {
        sender.drive().await.unwrap();
        sender
    });
    rx.await.unwrap();
    handle.process_replication_ack(-1, 0).unwrap();
    handle.process_copy_ack(-1, -1).unwrap();
    let sender = tokio::time::timeout(Duration::from_secs(2), driver)
        .await
        .unwrap()
        .unwrap();
    assert!(sender.handle().end().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn malformed_replication_gate_does_not_transition_state() {
    let mut rig = Rig::empty(3);
    rig.pending().await;
    assert!(matches!(
        rig.handle.process_replication_ack(-2, 3),
        Err(CopyError::InvalidAck)
    ));
    assert!(matches!(
        rig.handle.process_replication_ack(-1, -1),
        Err(CopyError::InvalidReplicationProgress)
    ));
    assert_eq!(rig.snapshot().phase, CopyPhase::LsnSet);
}

#[tokio::test(start_paused = true)]
async fn parked_terminal_cannot_latch_replication_gate_from_impossible_ack() {
    let mut rig = Rig::new(2, 2, true, 3);
    rig.pending().await;
    for copy_applied in [-1, 3] {
        assert!(matches!(
            rig.handle.process_replication_ack(copy_applied, 3),
            Err(CopyError::InvalidAck)
        ));
    }
    assert_eq!(rig.snapshot().phase, CopyPhase::LsnSet);
    assert!(
        rig.handle
            .update_last_replication_operation_during_copy(8)
            .unwrap()
    );
    rig.copy_ack(1, 1);
    assert!(!rig.gate(3));
    assert!(rig.gate(8));
    rig.copy_ack(-1, -1);
    assert!(rig.handle.end().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn progress_and_averages_delegate_to_actual_copy_sender_without_defaults() {
    let mut rig = Rig::new(2, 4, true, 2);
    rig.pending().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    rig.copy_ack(2, 1);
    let progress = rig.handle.progress().unwrap().unwrap();
    assert_eq!((progress.received, progress.applied), (2, 1));
    assert_eq!(progress.received_not_applied, 1);
    let (received, applied) = rig.handle.averages().unwrap().unwrap();
    assert!(received > Duration::ZERO);
    assert!(applied > Duration::ZERO);
    rig.copy_ack(-1, -1);
    assert!(rig.gate(2));
    rig.handle.end().unwrap();
    assert!(rig.handle.progress().unwrap().is_none());
    assert!(rig.handle.averages().unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn first_error_completion_cannot_be_overwritten_by_build_success() {
    let mut rig = Rig::empty(0);
    rig.pending().await;
    rig.handle.finish(CopyError::Cancelled).unwrap();
    assert!(!rig.gate(0));
    assert!(!rig.handle.process_copy_ack(-1, -1).unwrap());
    assert!(!rig.handle.try_disable_build_completion().unwrap());
    assert!(matches!(
        rig.handle.end().unwrap().result,
        Err(CopyError::Cancelled)
    ));
}

#[tokio::test(start_paused = true)]
async fn inflight_provider_error_retains_native_precedence_after_enumeration_cancel() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut sender = CopySender::new(true);
    sender
        .begin(
            config(4),
            Box::pin(stream::poll_fn(move |cx| rx.poll_recv(cx))),
            Arc::new(|_| panic!("provider failure must not send")),
            Arc::new(|| panic!("provider failure is not EOF")),
            Clock::new(),
        )
        .unwrap();
    assert!(poll_drive(&mut sender).await.is_pending());
    sender.handle().cancel().unwrap();
    tx.send(Err(RuntimeError::Application("late-provider-error".into())))
        .unwrap();
    assert!(matches!(sender.drive().await, Err(CopyError::Provider(_))));
    assert_eq!(
        sender.handle().end().unwrap().fault.unwrap().description,
        "EndGetNext"
    );
}

fn received(operation: &QueueOperation, is_last: bool, required: bool) -> ReceivedOperation {
    if is_last {
        ReceivedOperation::copy_end_of_stream(operation.lsn(), required).unwrap()
    } else {
        ReceivedOperation::new(
            QueueOperation::new(operation.lsn(), operation.data().clone(), true),
            Epoch::new(0, 0),
            required,
        )
    }
}

#[tokio::test(start_paused = true)]
async fn actual_secondary_copy_ack_sentinels_complete_build_in_both_service_ack_modes() {
    for required in [false, true] {
        let receiver = Arc::new(Mutex::new(
            SecondaryCopyReceiver::new(SecondaryCopyConfig {
                initial_capacity: 4,
                max_capacity: 4,
                require_service_ack: required,
                enable_eos_ack: true,
                copy_done: false,
            })
            .unwrap(),
        ));
        let target = receiver.clone();
        let mut sender = CopySender::new(true);
        sender
            .begin(
                config(4),
                Box::pin(stream::iter([
                    Ok(Bytes::new()),
                    Ok(Bytes::from_static(b"copy")),
                ])),
                Arc::new(move |action| {
                    if let CopySendAction::Operation { operation, is_last } = action {
                        let effects = target
                            .lock()
                            .unwrap()
                            .receive(received(&operation, is_last, required), is_last);
                        let _ = effects.dispatch().unwrap();
                    }
                    true
                }),
                Arc::new(|| Ok(7)),
                Clock::new(),
            )
            .unwrap();
        assert!(poll_drive(&mut sender).await.is_pending());
        let mut stream = receiver.lock().unwrap().stream();
        for lsn in 1..=3 {
            let Some(Ok(StreamEvent::Operation(item))) = stream.next().await else {
                panic!("missing real copy receiver operation");
            };
            assert_eq!(item.queue_operation().lsn(), lsn);
            assert_eq!(item.is_end_of_stream(), lsn == 3);
            if lsn == 1 {
                assert!(item.queue_operation().data().is_empty());
                assert!(!item.is_end_of_stream());
            }
            if required {
                let effects = receiver
                    .lock()
                    .unwrap()
                    .process_ack(item.acknowledge().unwrap().unwrap());
                let _ = effects.dispatch().unwrap();
            }
        }
        assert!(stream.next().await.is_none());
        let ack = receiver.lock().unwrap().capture_ack();
        assert_eq!((ack.received, ack.applied), (-1, -1));
        let handle = sender.handle();
        assert!(!handle.process_replication_ack(ack.applied, 6).unwrap());
        assert!(handle.process_copy_ack(ack.received, ack.applied).unwrap());
        assert!(handle.snapshot().unwrap().result.is_none());
        assert!(handle.process_replication_ack(ack.applied, 7).unwrap());
        assert!(handle.end().unwrap().result.is_ok());
    }
}

#[tokio::test(start_paused = true)]
async fn actual_context_receiver_numeric_terminal_ack_completes_without_replication() {
    let receiver = Arc::new(Mutex::new(
        CopyContextReceiver::new(CopyContextConfig {
            initial_queue_size: 4,
            max_queue_size: 4,
            batch_ack_interval: Duration::ZERO,
            retry_interval: Duration::from_secs(1),
            max_pending_acknowledgements: 1,
            trace_interval: Duration::ZERO,
        })
        .unwrap(),
    ));
    let acks = Arc::new(Mutex::new(Vec::new()));
    let observed = acks.clone();
    let send_ack: Arc<dyn Fn(_, _) -> bool + Send + Sync> = Arc::new(move |ack, _| {
        observed.lock().unwrap().push(ack);
        true
    });
    receiver
        .lock()
        .unwrap()
        .open(&send_ack, Duration::ZERO)
        .unwrap();
    let target = receiver.clone();
    let mut sender = CopySender::new(false);
    sender
        .begin(
            config(4),
            Box::pin(stream::iter([Ok(Bytes::new())])),
            Arc::new(move |action| {
                if let CopySendAction::Operation { operation, is_last } = action {
                    let effects = target.lock().unwrap().process(
                        received(&operation, is_last, false),
                        is_last,
                        Duration::ZERO,
                    );
                    effects.dispatch().unwrap();
                }
                true
            }),
            Arc::new(|| Ok(0)),
            Clock::new(),
        )
        .unwrap();
    assert!(poll_drive(&mut sender).await.is_pending());
    let mut stream = receiver.lock().unwrap().stream();
    assert_eq!(stream.next().await.unwrap().unwrap(), Bytes::new());
    assert!(stream.next().await.is_none());
    let ack = *acks.lock().unwrap().last().unwrap();
    assert_eq!((ack.completed, ack.error_code), (2, 0));
    let handle = sender.handle();
    assert!(
        handle
            .process_copy_ack(ack.completed, ack.completed)
            .unwrap()
    );
    assert!(handle.end().unwrap().result.is_ok());
    receiver
        .lock()
        .unwrap()
        .close(0, Duration::ZERO)
        .dispatch()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn concurrent_retarget_and_replication_gate_have_one_serialized_outcome() {
    for _ in 0..16 {
        let mut rig = Rig::empty(3);
        rig.pending().await;
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let start = barrier.clone();
        let retarget = rig.handle.clone();
        let updater = std::thread::spawn(move || {
            start.wait();
            retarget
                .update_last_replication_operation_during_copy(8)
                .unwrap()
        });
        let gate = rig.handle.clone();
        let completer = std::thread::spawn(move || {
            barrier.wait();
            gate.process_replication_ack(-1, 3).unwrap()
        });
        let updated = updater.join().unwrap();
        let gated = completer.join().unwrap();
        assert_ne!(updated, gated);
        let snapshot = rig.snapshot();
        if updated {
            assert_eq!(snapshot.phase, CopyPhase::LsnSet);
            assert_eq!(snapshot.replication_target, Some(8));
            assert!(rig.gate(8));
        } else {
            assert_eq!(snapshot.phase, CopyPhase::ReplCompleted);
            assert_eq!(snapshot.replication_target, Some(3));
        }
        rig.copy_ack(-1, -1);
        assert!(rig.handle.end().unwrap().result.is_ok());
    }
}
