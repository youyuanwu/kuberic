// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-component vectors derived from the pinned production sources in
// primary.rs, not native binary execution or native differential comparison.

use super::*;

use std::sync::atomic::AtomicUsize;
use std::sync::{Barrier, mpsc};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, stream};
use tokio::sync::{Semaphore, oneshot::error::TryRecvError};

use crate::application::OperationDataStream;
use crate::error::RuntimeError;
use crate::replicator::v1::copy_async_operation::{CopyConfig, CopySendAction};
use crate::replicator::v1::copy_context_receiver::CopyContextConfig;
use crate::replicator::v1::replica_manager::{CompletionError, ReplicaSet, SessionMember};
use crate::replicator::v1::sender::SenderConfig;
use crate::replicator::v1::session::{SendIntent, SendKind, SessionIdentity};

mod configuration;

fn route(id: i64) -> Route {
    Route {
        source: wire::Endpoint {
            partition: Bytes::from(vec![1; 16]),
            replica: 10,
            incarnation: Bytes::from(vec![2; 16]),
        },
        destination: wire::Endpoint {
            partition: Bytes::from(vec![1; 16]),
            replica: id,
            incarnation: Bytes::from(vec![3; 16]),
        },
        sender_address: "primary".into(),
        remote_address: format!("replica-{id}"),
    }
}

fn sender() -> SenderConfig {
    SenderConfig {
        initial_window: 2,
        max_window: 8,
        retry: Duration::from_secs(1),
        decay_factor: 0.5,
        decay_interval: Duration::from_secs(1),
    }
}

fn limits() -> BufferLimits {
    BufferLimits {
        max_segments: 8,
        max_payload_bytes: 1024,
    }
}

fn build_config(id: i64, persisted: bool) -> BuildConfig {
    BuildConfig {
        route: route(id),
        remote_incarnation: id,
        copy: CopyConfig {
            sender: sender(),
            buffer_policy: BufferPolicy::RawSingle,
            buffer_limits: limits(),
        },
        context: persisted.then_some(CopyContextConfig {
            initial_queue_size: 4,
            max_queue_size: 8,
            batch_ack_interval: Duration::from_millis(10),
            retry_interval: Duration::from_secs(1),
            max_pending_acknowledgements: 4,
            trace_interval: Duration::ZERO,
        }),
    }
}

fn primary_config() -> PrimaryConfig {
    PrimaryConfig {
        queue: QueueConfig {
            initial_capacity: 4,
            max_capacity: 64,
            max_bytes: 0,
            max_completed_count: 0,
            max_completed_bytes: 0,
            clean_on_complete: true,
        },
        initial_progress: 0,
        endpoint: route(20).source,
        address: "primary".into(),
        epoch: Epoch::new(1, 7),
        allow_multiple_quorum_set: true,
        sender: sender(),
        send: Arc::new(|_| panic!("isolated fixture must supply a session callback")),
    }
}

fn ack(id: i64, received: Lsn, applied: Lsn, copy: Lsn) -> wire::Envelope {
    route(id)
        .reversed()
        .envelope(wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
            replication_received: received,
            replication_applied: applied,
            copy_received: copy,
            copy_applied: copy,
            error: 0,
        }))
}

fn context(id: i64) -> wire::Envelope {
    route(id)
        .reversed()
        .envelope(wire::envelope::Body::CopyContextOperation(
            wire::CopyContextOperation {
                metadata: Some(wire::Metadata {
                    sequence: 1,
                    operation_type: 1,
                    atomic_group: -1,
                }),
                buffers: Some(wire::Buffers {
                    segments: Vec::new(),
                }),
                is_last: true,
            },
        ))
}

#[derive(Default)]
struct Sent {
    replication: Vec<SendIntent>,
    copy: Vec<(Lsn, bool)>,
    context_acks: usize,
}

fn callbacks(sent: &Arc<Mutex<Sent>>) -> BuildCallbacks {
    let replication = sent.clone();
    let copy = sent.clone();
    let context = sent.clone();
    BuildCallbacks {
        replication: Arc::new(move |intent| {
            replication.lock().unwrap().replication.push(intent);
            true
        }),
        copy: Arc::new(move |action| {
            if let CopySendAction::Operation { operation, is_last } = action {
                copy.lock().unwrap().copy.push((operation.lsn(), is_last));
            }
            true
        }),
        context_ack: Arc::new(move |_, _| {
            context.lock().unwrap().context_acks += 1;
            true
        }),
    }
}

struct Provider {
    calls: AtomicUsize,
    gate: Semaphore,
    failure: bool,
    context_items: AtomicUsize,
    hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Provider {
    fn new(permits: usize) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            gate: Semaphore::new(permits),
            failure: false,
            context_items: AtomicUsize::new(0),
            hook: None,
        }
    }
}

#[async_trait]
impl StateProvider for Provider {
    async fn update_epoch(&self, _: Epoch, _: Lsn) -> crate::Result<()> {
        panic!("not a role/epoch owner")
    }
    async fn last_committed_lsn(&self) -> crate::Result<Lsn> {
        panic!("must use the actual queue")
    }
    async fn get_copy_context(&self) -> crate::Result<OperationDataStream> {
        panic!("not the target")
    }
    async fn get_copy_state(
        &self,
        _: Lsn,
        mut context: OperationDataStream,
    ) -> crate::Result<OperationDataStream> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(hook) = &self.hook {
            hook();
        }
        self.gate.acquire().await.unwrap().forget();
        while let Some(item) = context.next().await {
            item?;
            self.context_items.fetch_add(1, Ordering::SeqCst);
        }
        if self.failure {
            return Err(RuntimeError::Application(
                "actual GetCopyState failure".into(),
            ));
        }
        Ok(Box::pin(stream::iter([Ok(Bytes::from_static(
            b"snapshot",
        ))])))
    }
    async fn on_data_loss(&self) -> crate::Result<bool> {
        panic!("not data-loss recovery")
    }
}

struct Rig {
    primary: Primary,
    handle: PrimaryHandle,
    clock: Clock,
    sent: Arc<Mutex<Sent>>,
}

impl Rig {
    fn new() -> Self {
        let clock = Clock::new();
        let primary = Primary::new(primary_config(), clock.clone()).unwrap();
        let handle = primary.handle();
        let mut rig = Self {
            primary,
            handle,
            clock,
            sent: Arc::new(Mutex::new(Sent::default())),
        };
        rig.configure(Vec::new(), 2);
        rig
    }

    fn configure(&mut self, members: Vec<SessionMember>, quorum: usize) {
        let routes = members
            .iter()
            .map(|member| route(member.replica_id))
            .collect();
        self.handle
            .configure_established(
                EstablishedConfiguration {
                    current: ReplicaSet {
                        write_quorum: quorum,
                        members,
                    },
                    previous: None,
                    catchup_configuration: false,
                },
                routes,
            )
            .unwrap();
    }

    fn session(
        &self,
        id: i64,
        send: Arc<dyn Fn(SendIntent) -> bool + Send + Sync>,
    ) -> ReplicationSession {
        let route = route(id);
        let session = ReplicationSession::new(
            SessionIdentity {
                primary_endpoint: route.sender_address,
                remote_endpoint: route.remote_address,
                remote_incarnation: id,
            },
            Epoch::new(1, 7),
            0,
            sender(),
            send,
            self.clock.now(),
        );
        session.open();
        session
    }

    fn active(&mut self, ids: &[i64], quorum: usize) -> Vec<ReplicationSession> {
        let sessions: Vec<_> = ids
            .iter()
            .map(|id| self.session(*id, callbacks(&self.sent).replication))
            .collect();
        self.configure(
            ids.iter()
                .zip(&sessions)
                .map(|(id, session)| SessionMember {
                    replica_id: *id,
                    session: session.clone(),
                    must_catchup: false,
                })
                .collect(),
            quorum,
        );
        sessions
    }

    fn enqueue(&self) -> PrimaryAdmission {
        self.handle
            .enqueue(
                Bytes::from_static(b"replication"),
                BufferPolicy::RawSingle,
                limits(),
            )
            .unwrap()
    }

    fn pending(&mut self) {
        assert!(self.primary.run().now_or_never().is_none());
    }

    fn begin(&self, id: i64, persisted: bool, provider: Arc<Provider>) -> BuildIdleHandle {
        self.handle
            .begin_build(build_config(id, persisted), provider, callbacks(&self.sent))
            .unwrap()
    }

    fn finish_build(&mut self, id: i64) -> BuildIdleHandle {
        let build = self.begin(id, false, Arc::new(Provider::new(1)));
        self.handle.receive_ack(&ack(id, 0, 0, 0)).unwrap();
        self.pending();
        assert_eq!(self.sent.lock().unwrap().copy.last(), Some(&(2, true)));
        self.handle.receive_ack(&ack(id, 0, 0, 2)).unwrap();
        self.pending();
        assert!(build.result().unwrap().result.is_ok());
        build
    }
}

#[tokio::test(start_paused = true)]
async fn finished_build_keeps_actual_replication_retries_and_quorum_ack_completion() {
    let mut rig = Rig::new();
    let build = rig.finish_build(20);
    // Explicit caller-established role seam, not an implementation of promotion.
    rig.configure(
        vec![SessionMember {
            replica_id: 20,
            session: build.session(),
            must_catchup: false,
        }],
        2,
    );
    rig.sent.lock().unwrap().replication.clear();
    let mut admission = rig.enqueue();
    assert_eq!(admission.lsn, 1);
    assert_eq!(admission.completion.try_recv(), Err(TryRecvError::Empty));
    rig.pending();
    tokio::time::advance(Duration::from_secs(1)).await;
    rig.pending();
    let sends = rig.sent.lock().unwrap().replication.iter().filter(|intent|
        matches!(&intent.kind, SendKind::Replication { operation, .. } if operation.lsn() == 1)).count();
    assert_eq!(sends, 2);
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
    assert!(!build.session().is_processing());
    assert!(build.result().unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn finished_idle_build_retains_timer_without_quorum_vote() {
    let mut rig = Rig::new();
    rig.finish_build(20);
    let mut admission = rig.enqueue();
    rig.pending();
    tokio::time::advance(Duration::from_secs(1)).await;
    rig.pending();
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(admission.completion.try_recv(), Err(TryRecvError::Empty));
    assert!(
        rig.sent
            .lock()
            .unwrap()
            .replication
            .iter()
            .filter(|intent| matches!(intent.kind, SendKind::Replication { .. }))
            .count()
            >= 2
    );
}

#[tokio::test(start_paused = true)]
async fn receive_only_ack_keeps_request_ack_timer_until_application_ack() {
    let mut rig = Rig::new();
    rig.active(&[20], 2);
    let mut admission = rig.enqueue();
    rig.handle.receive_ack(&ack(20, 1, 0, -200)).unwrap();
    rig.pending();
    tokio::time::advance(Duration::from_secs(1)).await;
    rig.pending();
    assert!(
        rig.sent
            .lock()
            .unwrap()
            .replication
            .iter()
            .any(|intent| matches!(intent.kind, SendKind::RequestAck))
    );
    assert_eq!(admission.completion.try_recv(), Err(TryRecvError::Empty));
    rig.handle.receive_ack(&ack(20, 1, 1, -200)).unwrap();
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
}

#[tokio::test(start_paused = true)]
async fn continuously_pinned_driver_observes_new_replication_deadline() {
    let mut rig = Rig::new();
    rig.active(&[20], 2);
    let drive = rig.primary.run();
    tokio::pin!(drive);
    assert!(drive.as_mut().now_or_never().is_none());
    rig.handle
        .enqueue(Bytes::new(), BufferPolicy::RawSingle, limits())
        .unwrap();
    assert!(drive.as_mut().now_or_never().is_none());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(drive.as_mut().now_or_never().is_none());
    assert_eq!(rig.sent.lock().unwrap().replication.len(), 2);
    rig.handle.close(Duration::ZERO).unwrap();
    assert!(drive.await.unwrap().result.is_ok());
}

#[tokio::test]
async fn build_is_published_before_reentrant_start_ack_and_context_ingress() {
    let mut rig = Rig::new();
    let handle = rig.handle.clone();
    let mut hooks = callbacks(&rig.sent);
    hooks.replication = Arc::new(move |intent| {
        if matches!(intent.kind, SendKind::StartCopy { .. }) {
            handle.receive_context(&context(20)).unwrap();
        }
        true
    });
    let provider = Arc::new(Provider::new(1));
    let build = rig
        .handle
        .begin_build(build_config(20, true), provider.clone(), hooks)
        .unwrap();
    rig.pending();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    rig.handle.receive_ack(&ack(20, 0, 0, 2)).unwrap();
    rig.pending();
    assert!(build.result().unwrap().result.is_ok());
}

#[tokio::test]
async fn close_from_start_send_cancels_published_build_and_blocks_new_admission() {
    let mut rig = Rig::new();
    let handle = rig.handle.clone();
    let mut hooks = callbacks(&rig.sent);
    hooks.replication = Arc::new(move |_| {
        handle.close(Duration::ZERO).unwrap();
        true
    });
    let provider = Arc::new(Provider::new(1));
    let build = rig
        .handle
        .begin_build(build_config(20, false), provider.clone(), hooks)
        .unwrap();
    assert!(rig.primary.run().await.unwrap().result.is_ok());
    rig.primary.drain_builds().await.unwrap();
    assert!(build.result().unwrap().result.is_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        rig.handle
            .enqueue(Bytes::new(), BufferPolicy::RawSingle, limits()),
        Err(PrimaryError::Closed)
    ));
}

#[tokio::test]
async fn close_cancels_pending_writes_and_catchup_before_membership_can_fake_success() {
    let mut rig = Rig::new();
    rig.active(&[20], 2);
    let admission = rig.enqueue();
    let catchup = rig.handle.begin_catchup(CatchupMode::All).unwrap();
    rig.handle.close(Duration::ZERO).unwrap();
    assert_eq!(
        admission.completion.await.unwrap(),
        Err(CompletionError::OperationCanceled)
    );
    assert_eq!(catchup.await.unwrap(), Err(CompletionError::ObjectClosed));
    let end = rig.primary.run().await.unwrap();
    assert!(end.result.is_ok());
    assert!(!end.aborted);
}

#[tokio::test(start_paused = true)]
async fn close_quorum_prewait_accepts_real_ack_and_finishes_without_timeout() {
    let mut rig = Rig::new();
    rig.active(&[20], 2);
    let admission = rig.enqueue();
    rig.handle.close(Duration::from_secs(5)).unwrap();
    rig.pending();
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
    let end = rig.primary.run().await.unwrap();
    assert!(end.result.is_ok());
    assert!(!end.quorum_wait_expired);
}

#[tokio::test(start_paused = true)]
async fn close_quorum_timeout_still_closes_and_cancels_without_fabricated_ack() {
    let mut rig = Rig::new();
    rig.active(&[20], 2);
    let admission = rig.enqueue();
    rig.handle.close(Duration::from_secs(5)).unwrap();
    rig.pending();
    tokio::time::advance(Duration::from_secs(5)).await;
    let end = rig.primary.run().await.unwrap();
    assert!(end.result.is_ok());
    assert!(end.quorum_wait_expired);
    assert_eq!(
        admission.completion.await.unwrap(),
        Err(CompletionError::OperationCanceled)
    );
}

#[tokio::test]
async fn close_with_no_pending_replicates_does_not_wait_for_timeout() {
    let mut rig = Rig::new();
    rig.handle.close(Duration::MAX).unwrap();
    assert!(!rig.primary.run().await.unwrap().quorum_wait_expired);
}

#[tokio::test]
async fn abort_preempts_quorum_wait_and_is_not_ordinary_close_success() {
    let mut rig = Rig::new();
    rig.active(&[20], 2);
    let admission = rig.enqueue();
    rig.handle.close(Duration::from_secs(500)).unwrap();
    rig.pending();
    rig.handle.abort().unwrap();
    let end = rig.primary.run().await.unwrap();
    assert!(end.aborted);
    assert!(matches!(end.result, Err(PrimaryError::Aborted)));
    assert!(!end.quorum_wait_expired);
    assert_eq!(
        admission.completion.await.unwrap(),
        Err(CompletionError::OperationCanceled)
    );
}

#[tokio::test]
async fn pending_provider_acquisition_survives_interrupted_driver_and_primary_close() {
    let mut rig = Rig::new();
    let provider = Arc::new(Provider::new(0));
    let build = rig.begin(20, false, provider.clone());
    rig.handle.receive_ack(&ack(20, 0, 0, 0)).unwrap();
    rig.pending();
    rig.pending();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    rig.handle.close(Duration::ZERO).unwrap();
    assert!(rig.primary.run().await.unwrap().result.is_ok());
    assert!(build.result().is_none());
    assert!(rig.primary.drain_builds().now_or_never().is_none());
    provider.gate.add_permits(1);
    rig.primary.drain_builds().await.unwrap();
    assert!(build.result().unwrap().result.is_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn provider_can_reenter_abort_without_primary_or_manager_guard() {
    let mut rig = Rig::new();
    let handle = rig.handle.clone();
    let mut provider = Provider::new(1);
    provider.hook = Some(Arc::new(move || {
        handle.abort().unwrap();
    }));
    let build = rig.begin(20, false, Arc::new(provider));
    rig.handle.receive_ack(&ack(20, 0, 0, 0)).unwrap();
    assert!(rig.primary.run().await.unwrap().aborted);
    rig.primary.drain_builds().await.unwrap();
    assert!(build.result().unwrap().result.is_err());
}

#[tokio::test]
async fn actual_provider_failure_remains_build_fault_not_successful_completion() {
    let mut rig = Rig::new();
    let mut provider = Provider::new(1);
    provider.failure = true;
    let build = rig.begin(20, false, Arc::new(provider));
    rig.handle.receive_ack(&ack(20, 0, 0, 0)).unwrap();
    rig.pending();
    let end = build.result().unwrap();
    assert!(end.result.is_err());
    assert_eq!(end.fault.unwrap().description, "GetCopyState");
    assert!(
        rig.handle
            .begin_build(
                build_config(30, false),
                Arc::new(Provider::new(1)),
                callbacks(&rig.sent)
            )
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn removed_finished_build_stops_timer_and_rejects_old_ingress() {
    let mut rig = Rig::new();
    let build = rig.finish_build(20);
    rig.enqueue();
    rig.handle.remove_idle(20).unwrap();
    let selected = rig.sent.lock().unwrap().replication.len();
    tokio::time::advance(Duration::from_secs(5)).await;
    rig.pending();
    assert_eq!(rig.sent.lock().unwrap().replication.len(), selected);
    assert!(build.session().retry_deadline().is_none());
    assert!(matches!(
        rig.handle.receive_ack(&ack(20, 1, 1, -1)),
        Err(PrimaryError::UnknownSession(20))
    ));
}

#[tokio::test]
async fn removing_acquiring_build_preserves_actual_call_until_explicit_drain() {
    let mut rig = Rig::new();
    let provider = Arc::new(Provider::new(0));
    let build = rig.begin(20, false, provider.clone());
    rig.handle.receive_ack(&ack(20, 0, 0, 0)).unwrap();
    rig.pending();
    rig.handle.remove_idle(20).unwrap();
    assert!(rig.primary.drain_builds().now_or_never().is_none());
    provider.gate.add_permits(1);
    rig.primary.drain_builds().await.unwrap();
    assert!(build.result().unwrap().result.is_err());
}

#[tokio::test]
async fn wrong_endpoint_incarnation_address_and_version_do_not_advance_sender() {
    let mut rig = Rig::new();
    let session = rig.active(&[20], 2).remove(0);
    let mut admission = rig.enqueue();
    for index in 0..5 {
        let mut envelope = ack(20, 1, 1, -1);
        match index {
            0 => envelope.source.as_mut().unwrap().incarnation = Bytes::from(vec![9; 16]),
            1 => envelope.destination.as_mut().unwrap().replica = 99,
            2 => envelope.sender_address = "forged".into(),
            3 => envelope.version = 2,
            _ => envelope.message_id.clear(),
        }
        assert!(rig.handle.receive_ack(&envelope).is_err());
    }
    assert_eq!(session.progress(0).received, 0);
    assert_eq!(admission.completion.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn impossible_replication_or_copy_progress_is_rejected_before_ack_coalescing() {
    let mut rig = Rig::new();
    let build = rig.begin(20, false, Arc::new(Provider::new(1)));
    assert!(rig.handle.receive_ack(&ack(20, 1, 1, 0)).is_err());
    assert!(rig.handle.receive_ack(&ack(20, 0, 0, 1)).is_err());
    assert!(!build.session().is_processing());
    rig.handle.receive_ack(&ack(20, 0, 0, 0)).unwrap();
    rig.pending();
    rig.handle.receive_ack(&ack(20, 0, 0, 2)).unwrap();
    rig.pending();
    assert!(build.result().unwrap().result.is_ok());
}

#[tokio::test]
async fn active_context_is_rejected_and_copy_error_does_not_fault_active_replication() {
    let mut rig = Rig::new();
    rig.active(&[20], 2);
    let admission = rig.enqueue();
    assert!(matches!(
        rig.handle.receive_context(&context(20)),
        Err(PrimaryError::NotBuilding(20))
    ));
    let mut envelope = ack(20, 1, 1, -100);
    if let Some(wire::envelope::Body::ReplicationAck(body)) = &mut envelope.body {
        body.error = 42;
    }
    rig.handle.receive_ack(&envelope).unwrap();
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
}

#[tokio::test]
async fn duplicate_and_regressive_acks_finish_processor_without_duplicate_completion() {
    let mut rig = Rig::new();
    let session = rig.active(&[20], 2).remove(0);
    let admission = rig.enqueue();
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    rig.handle.receive_ack(&ack(20, 0, 0, -1)).unwrap();
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
    assert!(!session.is_processing());
}

#[tokio::test]
async fn failed_route_configuration_is_atomic_and_preserves_live_session() {
    let mut rig = Rig::new();
    let session = rig.active(&[20], 2).remove(0);
    let mut wrong = route(20);
    wrong.destination.incarnation = Bytes::from(vec![8; 16]);
    assert!(matches!(
        rig.handle.configure_established(
            EstablishedConfiguration {
                current: ReplicaSet {
                    write_quorum: 2,
                    members: vec![SessionMember {
                        replica_id: 20,
                        session,
                        must_catchup: false
                    }]
                },
                previous: None,
                catchup_configuration: false,
            },
            vec![wrong]
        ),
        Err(PrimaryError::InvalidBinding(20))
    ));
    let admission = rig.enqueue();
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
}

#[tokio::test]
async fn closed_primary_rejects_build_config_context_ack_and_catchup_ingress() {
    let mut rig = Rig::new();
    rig.handle.close(Duration::ZERO).unwrap();
    assert!(matches!(
        rig.handle.begin_build(
            build_config(20, false),
            Arc::new(Provider::new(1)),
            callbacks(&rig.sent)
        ),
        Err(PrimaryError::Closed)
    ));
    assert!(matches!(
        rig.handle.configure_established(
            EstablishedConfiguration {
                current: ReplicaSet {
                    write_quorum: 1,
                    members: Vec::new()
                },
                previous: None,
                catchup_configuration: false,
            },
            Vec::new()
        ),
        Err(PrimaryError::Closed)
    ));
    assert!(matches!(
        rig.handle.receive_ack(&ack(20, 0, 0, 0)),
        Err(PrimaryError::Closed)
    ));
    assert!(matches!(
        rig.handle.receive_context(&context(20)),
        Err(PrimaryError::Closed)
    ));
    assert!(matches!(
        rig.handle.begin_catchup(CatchupMode::All),
        Err(PrimaryError::Closed)
    ));
    assert!(rig.primary.run().await.unwrap().result.is_ok());
}

#[tokio::test]
async fn owner_abandonment_is_explicit_and_does_not_claim_close_or_keep_provider_alive() {
    let rig = Rig::new();
    let provider = Arc::new(Provider::new(0));
    let weak = Arc::downgrade(&provider);
    rig.begin(20, false, provider.clone());
    drop(provider);
    let handle = rig.handle.clone();
    drop(rig);
    assert!(weak.upgrade().is_none());
    assert!(matches!(
        handle.close(Duration::ZERO),
        Err(PrimaryError::OwnerDropped)
    ));
}

#[tokio::test]
async fn close_waits_selected_send_delivery_without_waiting_client_completion_consumption() {
    let mut rig = Rig::new();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let enter = entered.clone();
    let unblock = release.clone();
    let session = rig.session(
        20,
        Arc::new(move |_| {
            enter.wait();
            unblock.wait();
            true
        }),
    );
    rig.configure(
        vec![SessionMember {
            replica_id: 20,
            session,
            must_catchup: false,
        }],
        2,
    );
    let handle = rig.handle.clone();
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        tx.send(
            handle
                .enqueue(Bytes::new(), BufferPolicy::RawSingle, limits())
                .unwrap(),
        )
        .unwrap();
    });
    entered.wait();
    rig.handle.close(Duration::ZERO).unwrap();
    rig.pending();
    release.wait();
    worker.join().unwrap();
    let admission = rx.recv().unwrap();
    assert!(rig.primary.run().await.unwrap().result.is_ok());
    assert_eq!(
        admission.completion.await.unwrap(),
        Err(CompletionError::OperationCanceled)
    );
}

#[tokio::test]
async fn reentrant_ack_during_send_can_complete_real_quorum_without_owner_guard() {
    let mut rig = Rig::new();
    let handle = rig.handle.clone();
    let session = rig.session(
        20,
        Arc::new(move |intent| {
            if let SendKind::Replication { operation, .. } = intent.kind {
                handle
                    .receive_ack(&ack(20, operation.lsn(), operation.lsn(), -1))
                    .unwrap();
            }
            true
        }),
    );
    rig.configure(
        vec![SessionMember {
            replica_id: 20,
            session,
            must_catchup: false,
        }],
        2,
    );
    assert_eq!(rig.enqueue().completion.await.unwrap(), Ok(()));
}

#[tokio::test(start_paused = true)]
async fn selected_timer_send_can_reenter_close_and_driver_observes_terminal_state() {
    let mut rig = Rig::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    let handle = rig.handle.clone();
    let session = rig.session(
        20,
        Arc::new(move |_| {
            if called.fetch_add(1, Ordering::SeqCst) == 1 {
                handle.close(Duration::ZERO).unwrap();
            }
            true
        }),
    );
    rig.configure(
        vec![SessionMember {
            replica_id: 20,
            session,
            must_catchup: false,
        }],
        2,
    );
    let admission = rig.enqueue();
    rig.pending();
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(rig.primary.run().await.unwrap().result.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        admission.completion.await.unwrap(),
        Err(CompletionError::OperationCanceled)
    );
}

#[tokio::test]
async fn reentrant_newer_ack_coalesces_and_completes_all_ordered_handoffs() {
    let mut rig = Rig::new();
    let handle = rig.handle.clone();
    let session = rig.session(
        20,
        Arc::new(move |intent| {
            if let SendKind::Replication { operation, .. } = intent.kind
                && operation.lsn() == 3
            {
                handle.receive_ack(&ack(20, 3, 3, -1)).unwrap();
            }
            true
        }),
    );
    rig.configure(
        vec![SessionMember {
            replica_id: 20,
            session: session.clone(),
            must_catchup: false,
        }],
        2,
    );
    let admissions: Vec<_> = (0..3).map(|_| rig.enqueue()).collect();
    rig.handle.receive_ack(&ack(20, 1, 0, -1)).unwrap();
    for admission in admissions {
        assert_eq!(admission.completion.await.unwrap(), Ok(()));
    }
    assert!(!session.is_processing());
    assert_eq!(session.progress(0).applied, 3);
}

#[tokio::test(start_paused = true)]
async fn shared_current_previous_session_has_one_timer_and_both_quorum_votes() {
    let mut rig = Rig::new();
    let session = rig.session(20, callbacks(&rig.sent).replication);
    let member = SessionMember {
        replica_id: 20,
        session,
        must_catchup: false,
    };
    rig.handle
        .configure_established(
            EstablishedConfiguration {
                current: ReplicaSet {
                    write_quorum: 2,
                    members: vec![member.clone()],
                },
                previous: Some(ReplicaSet {
                    write_quorum: 2,
                    members: vec![member],
                }),
                catchup_configuration: false,
            },
            vec![route(20)],
        )
        .unwrap();
    let admission = rig.enqueue();
    rig.pending();
    tokio::time::advance(Duration::from_secs(1)).await;
    rig.pending();
    assert_eq!(rig.sent.lock().unwrap().replication.len(), 2);
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
}

#[tokio::test(start_paused = true)]
async fn configuration_removal_closes_old_active_timer_and_rejects_stale_route() {
    let mut rig = Rig::new();
    let session = rig.active(&[20], 2).remove(0);
    let admission = rig.enqueue();
    rig.configure(Vec::new(), 1);
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
    assert!(session.retry_deadline().is_none());
    assert!(matches!(
        rig.handle.receive_ack(&ack(20, 1, 1, -1)),
        Err(PrimaryError::UnknownSession(20))
    ));
    let sent = rig.sent.lock().unwrap().replication.len();
    tokio::time::advance(Duration::from_secs(2)).await;
    rig.pending();
    assert_eq!(rig.sent.lock().unwrap().replication.len(), sent);
}

#[tokio::test]
async fn duplicate_or_extra_route_configuration_does_not_publish_partial_bindings() {
    let rig = Rig::new();
    for routes in [vec![route(20)], vec![route(20), route(20)]] {
        assert!(matches!(
            rig.handle.configure_established(
                EstablishedConfiguration {
                    current: ReplicaSet {
                        write_quorum: 2,
                        members: Vec::new()
                    },
                    previous: None,
                    catchup_configuration: false,
                },
                routes
            ),
            Err(PrimaryError::InvalidBinding(20))
        ));
    }
    assert!(matches!(
        rig.handle.receive_ack(&ack(20, 0, 0, -1)),
        Err(PrimaryError::UnknownSession(20))
    ));
}

#[tokio::test]
async fn invalid_local_endpoint_or_cross_partition_build_is_rejected_before_sends() {
    let mut config = primary_config();
    config.endpoint.incarnation = Bytes::new();
    assert!(matches!(
        Primary::new(config, Clock::new()),
        Err(PrimaryError::Protocol(_))
    ));
    let rig = Rig::new();
    let mut config = build_config(20, false);
    config.route.destination.partition = Bytes::from(vec![9; 16]);
    assert!(matches!(
        rig.handle
            .begin_build(config, Arc::new(Provider::new(1)), callbacks(&rig.sent)),
        Err(PrimaryError::InvalidBinding(20))
    ));
    assert!(rig.sent.lock().unwrap().replication.is_empty());
}

#[tokio::test]
async fn bound_ingress_still_rejects_wrong_body_and_impossible_active_progress() {
    let mut rig = Rig::new();
    let session = rig.active(&[20], 2).remove(0);
    let admission = rig.enqueue();
    assert!(rig.handle.receive_ack(&context(20)).is_err());
    assert!(rig.handle.receive_ack(&ack(20, 2, 2, -1)).is_err());
    assert!(rig.handle.receive_ack(&ack(20, 0, 1, -1)).is_err());
    assert!(!session.is_processing());
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
}

#[tokio::test(start_paused = true)]
async fn quorum_prewait_continues_actual_retry_and_request_ack_delivery() {
    let mut rig = Rig::new();
    rig.active(&[20], 2);
    let admission = rig.enqueue();
    rig.handle.close(Duration::from_secs(5)).unwrap();
    rig.pending();
    tokio::time::advance(Duration::from_secs(1)).await;
    rig.pending();
    assert_eq!(rig.sent.lock().unwrap().replication.len(), 2);
    rig.handle.receive_ack(&ack(20, 1, 0, -1)).unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    rig.pending();
    assert!(matches!(
        rig.sent.lock().unwrap().replication.last().unwrap().kind,
        SendKind::RequestAck
    ));
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert!(rig.primary.run().await.unwrap().result.is_ok());
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
}

#[tokio::test(start_paused = true)]
async fn queue_full_send_result_is_not_an_ack_or_terminal_fault() {
    let mut rig = Rig::new();
    let attempts = Arc::new(AtomicUsize::new(0));
    let calls = attempts.clone();
    let session = rig.session(
        20,
        Arc::new(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            false
        }),
    );
    rig.configure(
        vec![SessionMember {
            replica_id: 20,
            session,
            must_catchup: false,
        }],
        2,
    );
    let mut admission = rig.enqueue();
    rig.pending();
    tokio::time::advance(Duration::from_secs(1)).await;
    rig.pending();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(admission.completion.try_recv(), Err(TryRecvError::Empty));
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
}

#[tokio::test]
async fn no_configuration_does_not_consume_lsn_and_primary_only_completes_actual_queue() {
    let clock = Clock::new();
    let primary = Primary::new(primary_config(), clock).unwrap();
    let handle = primary.handle();
    assert!(
        handle
            .enqueue(Bytes::new(), BufferPolicy::RawSingle, limits())
            .is_err()
    );
    handle
        .configure_established(
            EstablishedConfiguration {
                current: ReplicaSet {
                    write_quorum: 1,
                    members: Vec::new(),
                },
                previous: None,
                catchup_configuration: false,
            },
            Vec::new(),
        )
        .unwrap();
    let admission = handle
        .enqueue(Bytes::new(), BufferPolicy::RawSingle, limits())
        .unwrap();
    assert_eq!(admission.lsn, 1);
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
}

#[tokio::test]
async fn active_built_session_ignores_copy_error_and_impossible_copy_fields() {
    let mut rig = Rig::new();
    let build = rig.finish_build(20);
    rig.configure(
        vec![SessionMember {
            replica_id: 20,
            session: build.session(),
            must_catchup: false,
        }],
        2,
    );
    let admission = rig.enqueue();
    let mut envelope = ack(20, 1, 1, -200);
    if let Some(wire::envelope::Body::ReplicationAck(body)) = &mut envelope.body {
        body.error = 42;
    }
    rig.handle.receive_ack(&envelope).unwrap();
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
    assert!(build.result().unwrap().result.is_ok());
}

#[tokio::test]
async fn removed_provider_and_completed_build_are_released_after_real_callback() {
    let mut rig = Rig::new();
    let provider = Arc::new(Provider::new(0));
    let weak = Arc::downgrade(&provider);
    let build = rig.begin(20, false, provider.clone());
    rig.handle.receive_ack(&ack(20, 0, 0, 0)).unwrap();
    rig.pending();
    rig.handle.remove_idle(20).unwrap();
    provider.gate.add_permits(1);
    drop(provider);
    assert!(weak.upgrade().is_some());
    rig.primary.drain_builds().await.unwrap();
    assert!(build.result().unwrap().result.is_err());
    assert!(weak.upgrade().is_none());
}

#[tokio::test]
async fn replaced_replica_id_cannot_be_rebound_by_old_provider_completion() {
    let mut rig = Rig::new();
    let provider = Arc::new(Provider::new(0));
    let old = rig.begin(20, false, provider.clone());
    rig.handle.receive_ack(&ack(20, 0, 0, 0)).unwrap();
    rig.pending();
    rig.handle.remove_idle(20).unwrap();
    let mut config = build_config(20, false);
    config.route.destination.incarnation = Bytes::from(vec![8; 16]);
    let new = rig
        .handle
        .begin_build(config, Arc::new(Provider::new(1)), callbacks(&rig.sent))
        .unwrap();
    provider.gate.add_permits(1);
    rig.pending();
    assert!(old.result().unwrap().result.is_err());
    assert!(new.result().is_none());
    assert!(matches!(
        rig.handle.receive_ack(&ack(20, 0, 0, 0)),
        Err(PrimaryError::Protocol(ProtocolError::WrongSource))
    ));
    rig.handle.abort().unwrap();
    rig.primary.drain_builds().await.unwrap();
    assert!(new.result().unwrap().result.is_err());
}

#[tokio::test]
async fn abort_after_ordinary_close_is_independently_observable_and_idempotent() {
    let mut rig = Rig::new();
    rig.handle.close(Duration::ZERO).unwrap();
    assert!(rig.primary.run().await.unwrap().result.is_ok());
    rig.handle.abort().unwrap();
    rig.handle.abort().unwrap();
    let end = rig.primary.run().await.unwrap();
    assert!(end.aborted);
    assert!(matches!(end.result, Err(PrimaryError::Aborted)));
}

#[tokio::test]
async fn send_can_reenter_abort_and_selected_ack_processor_finishes_all_handoffs() {
    let mut rig = Rig::new();
    let handle = rig.handle.clone();
    let session = rig.session(
        20,
        Arc::new(move |intent| {
            if let SendKind::Replication { operation, .. } = intent.kind
                && operation.lsn() == 3
            {
                handle.abort().unwrap();
            }
            true
        }),
    );
    rig.configure(
        vec![SessionMember {
            replica_id: 20,
            session: session.clone(),
            must_catchup: false,
        }],
        2,
    );
    let admissions: Vec<_> = (0..3).map(|_| rig.enqueue()).collect();
    rig.handle.receive_ack(&ack(20, 1, 0, -1)).unwrap();
    assert!(!session.is_processing());
    assert!(rig.primary.run().await.unwrap().aborted);
    for admission in admissions {
        assert_eq!(
            admission.completion.await.unwrap(),
            Err(CompletionError::OperationCanceled)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn infinite_quorum_prewait_keeps_retrying_without_deadline_overflow() {
    let mut rig = Rig::new();
    rig.active(&[20], 2);
    let admission = rig.enqueue();
    rig.handle.close(Duration::MAX).unwrap();
    rig.pending();
    tokio::time::advance(Duration::from_secs(1)).await;
    rig.pending();
    assert_eq!(rig.sent.lock().unwrap().replication.len(), 2);
    rig.handle.abort().unwrap();
    assert!(rig.primary.run().await.unwrap().aborted);
    assert_eq!(
        admission.completion.await.unwrap(),
        Err(CompletionError::OperationCanceled)
    );
    let mut no_sessions = Rig::new();
    let admission = no_sessions.enqueue();
    no_sessions.handle.close(Duration::MAX).unwrap();
    no_sessions.pending();
    no_sessions.handle.abort().unwrap();
    assert!(no_sessions.primary.run().await.unwrap().aborted);
    assert_eq!(
        admission.completion.await.unwrap(),
        Err(CompletionError::OperationCanceled)
    );
}

#[tokio::test]
async fn previous_configuration_lookup_precedes_retained_idle_copy_ingress() {
    let mut rig = Rig::new();
    let build = rig.finish_build(20);
    rig.handle
        .configure_established(
            EstablishedConfiguration {
                current: ReplicaSet {
                    write_quorum: 1,
                    members: Vec::new(),
                },
                previous: Some(ReplicaSet {
                    write_quorum: 2,
                    members: vec![SessionMember {
                        replica_id: 20,
                        session: build.session(),
                        must_catchup: false,
                    }],
                }),
                catchup_configuration: false,
            },
            vec![route(20)],
        )
        .unwrap();
    assert!(matches!(
        rig.handle.receive_context(&context(20)),
        Err(PrimaryError::NotBuilding(20))
    ));
    let admission = rig.enqueue();
    rig.handle.receive_ack(&ack(20, 1, 1, -200)).unwrap();
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
    rig.handle.remove_idle(20).unwrap();
    let admission = rig.enqueue();
    rig.handle.receive_ack(&ack(20, 2, 2, -200)).unwrap();
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
    assert!(build.result().unwrap().result.is_ok());
}
