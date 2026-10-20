// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-owner vectors from the pinned facade/lifetime sources in
// service_adapter.rs. No native harness or production host is selected.

use super::*;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{FutureExt, StreamExt, stream};
use tokio::sync::Semaphore;

use crate::application::OperationDataStream;
use crate::error::RuntimeError;

use super::super::copy_async_operation::{CopyConfig, CopySendAction};
use super::super::copy_context_receiver::CopyContextConfig;
use super::super::ingress::{Disposition, DropReason};
use super::super::operation_queue::QueueConfig;
use super::super::primary::{PrimaryConfig, ReplicaDescriptor};
use super::super::replica_manager::QueuePolicyConfig;
use super::super::secondary::{SecondaryCallbacks, SecondaryConfig};
use super::super::secondary_copy_receiver::SecondaryCopyConfig;
use super::super::secondary_receiver::ReceiverConfig;
use super::super::sender::SenderConfig;
use super::super::session::{SendIntent, SendKind};
use super::super::transport::{Route, wire};

mod facade;
mod host;
mod kv;

struct Provider {
    progress: AtomicI64,
    progress_calls: AtomicUsize,
    progress_gate: Semaphore,
    epoch_gate: Semaphore,
    epochs: Mutex<Vec<(Epoch, Lsn)>>,
    loss_gate: Semaphore,
    loss_calls: AtomicUsize,
    changed: AtomicBool,
    fail_progress: AtomicBool,
    panic_progress: AtomicBool,
    fail_epoch: AtomicBool,
    copy_gate: Semaphore,
    copy_calls: AtomicUsize,
    copy_resumed: AtomicUsize,
    epoch_callback: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    context_gate: Semaphore,
    context_calls: AtomicUsize,
    context_resumed: AtomicUsize,
    fail_context: AtomicBool,
}

impl Provider {
    fn new() -> Self {
        Self {
            progress: AtomicI64::new(0),
            progress_calls: AtomicUsize::new(0),
            progress_gate: Semaphore::new(100),
            epoch_gate: Semaphore::new(100),
            epochs: Mutex::new(Vec::new()),
            loss_gate: Semaphore::new(100),
            loss_calls: AtomicUsize::new(0),
            changed: AtomicBool::new(false),
            fail_progress: AtomicBool::new(false),
            panic_progress: AtomicBool::new(false),
            fail_epoch: AtomicBool::new(false),
            copy_gate: Semaphore::new(100),
            copy_calls: AtomicUsize::new(0),
            copy_resumed: AtomicUsize::new(0),
            epoch_callback: Mutex::new(None),
            context_gate: Semaphore::new(100),
            context_calls: AtomicUsize::new(0),
            context_resumed: AtomicUsize::new(0),
            fail_context: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl StateProvider for Provider {
    async fn update_epoch(&self, epoch: Epoch, boundary: Lsn) -> crate::Result<()> {
        self.epochs.lock().unwrap().push((epoch, boundary));
        let callback = self.epoch_callback.lock().unwrap().clone();
        if let Some(callback) = callback {
            callback();
        }
        self.epoch_gate.acquire().await.unwrap().forget();
        if self.fail_epoch.load(Ordering::SeqCst) {
            return Err(RuntimeError::InvalidReplication("provider epoch".into()));
        }
        Ok(())
    }

    async fn last_committed_lsn(&self) -> crate::Result<Lsn> {
        self.progress_calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.panic_progress.load(Ordering::SeqCst),
            "provider panic"
        );
        self.progress_gate.acquire().await.unwrap().forget();
        if self.fail_progress.load(Ordering::SeqCst) {
            return Err(RuntimeError::InvalidReplication("provider progress".into()));
        }
        Ok(self.progress.load(Ordering::SeqCst))
    }

    async fn get_copy_context(&self) -> crate::Result<OperationDataStream> {
        self.context_calls.fetch_add(1, Ordering::SeqCst);
        self.context_gate.acquire().await.unwrap().forget();
        self.context_resumed.fetch_add(1, Ordering::SeqCst);
        if self.fail_context.load(Ordering::SeqCst) {
            return Err(RuntimeError::Application("provider context".into()));
        }
        Ok(Box::pin(stream::iter([Ok(Bytes::from_static(b"context"))])))
    }

    async fn get_copy_state(
        &self,
        _: Lsn,
        mut context: OperationDataStream,
    ) -> crate::Result<OperationDataStream> {
        self.copy_calls.fetch_add(1, Ordering::SeqCst);
        self.copy_gate.acquire().await.unwrap().forget();
        self.copy_resumed.fetch_add(1, Ordering::SeqCst);
        while let Some(item) = context.next().await {
            assert_eq!(item?, Bytes::from_static(b"context"));
        }
        Ok(Box::pin(stream::iter([Ok(Bytes::from_static(
            b"snapshot",
        ))])))
    }

    async fn on_data_loss(&self) -> crate::Result<bool> {
        self.loss_calls.fetch_add(1, Ordering::SeqCst);
        self.loss_gate.acquire().await.unwrap().forget();
        Ok(self.changed.load(Ordering::SeqCst))
    }
}

fn epoch(n: i64) -> Epoch {
    Epoch::new(1, n)
}

fn endpoint(id: i64) -> wire::Endpoint {
    wire::Endpoint {
        partition: Bytes::from(vec![1; 16]),
        replica: id,
        incarnation: Bytes::from(vec![id as u8; 16]),
    }
}

fn route(from: i64, to: i64) -> Route {
    Route {
        source: endpoint(from),
        destination: endpoint(to),
        sender_address: format!("replica-{from}"),
        remote_address: format!("replica-{to}"),
    }
}

fn wire_epoch(epoch: Epoch) -> wire::Epoch {
    wire::Epoch {
        data_loss: epoch.data_loss_number,
        configuration: epoch.configuration_number,
    }
}

fn metadata(lsn: Lsn) -> wire::Metadata {
    wire::Metadata {
        sequence: lsn,
        operation_type: 1,
        atomic_group: -1,
    }
}

fn replication(
    lsn: Lsn,
    original: Epoch,
    sending: Epoch,
    data: Bytes,
    hint: Lsn,
) -> wire::envelope::Body {
    wire::envelope::Body::ReplicationOperation(wire::ReplicationOperation {
        metadata: Some(metadata(lsn)),
        original_epoch: Some(wire_epoch(original)),
        sending_epoch: Some(wire_epoch(sending)),
        buffers: Some(wire::Buffers {
            segments: vec![data],
        }),
        first_sequence: lsn,
        last_sequence: lsn,
        last_in_batch: lsn,
        buffer_counts: vec![1],
        completed_hint: hint,
    })
}

#[derive(Clone, Default)]
struct Bus(Arc<Mutex<VecDeque<wire::Envelope>>>);

impl Bus {
    fn push(&self, route: &Route, body: wire::envelope::Body) {
        self.0.lock().unwrap().push_back(route.envelope(body));
    }

    fn send(&self, from: i64, intent: SendIntent) -> bool {
        let to = intent.identity.remote_incarnation;
        let body = match intent.kind {
            SendKind::StartCopy {
                replication_start,
                epoch,
            } => wire::envelope::Body::StartCopy(wire::StartCopy {
                epoch: Some(wire_epoch(epoch)),
                replica: to,
                replication_start,
            }),
            SendKind::Replication {
                operation,
                original_epoch,
                sending_epoch,
                completed_hint,
            } => replication(
                operation.lsn(),
                original_epoch,
                sending_epoch,
                operation.data().clone(),
                completed_hint,
            ),
            SendKind::RequestAck => wire::envelope::Body::RequestAck(wire::RequestAck {}),
            SendKind::InduceFault { reason } => {
                wire::envelope::Body::InduceFault(wire::InduceFault {
                    reason,
                    target_replica: to,
                    target_incarnation: endpoint(to).incarnation,
                })
            }
        };
        self.push(&route(from, to), body);
        true
    }

    fn copy(&self, route: &Route, action: CopySendAction, context: bool) -> bool {
        match action {
            CopySendAction::Operation { operation, is_last } => {
                let buffers = Some(wire::Buffers {
                    segments: if is_last {
                        vec![]
                    } else {
                        vec![operation.data().clone()]
                    },
                });
                let body = if context {
                    wire::envelope::Body::CopyContextOperation(wire::CopyContextOperation {
                        metadata: Some(metadata(operation.lsn())),
                        buffers,
                        is_last,
                    })
                } else {
                    wire::envelope::Body::CopyOperation(wire::CopyOperation {
                        metadata: Some(metadata(operation.lsn())),
                        epoch: Some(wire_epoch(epoch(1))),
                        buffers,
                        replica: route.destination.replica,
                        is_last,
                    })
                };
                self.push(route, body);
            }
            CopySendAction::RequestAck => {
                self.push(route, wire::envelope::Body::RequestAck(wire::RequestAck {}))
            }
        }
        true
    }

    fn deliver(&self, registry: &MessageRegistry) {
        loop {
            let message = self.0.lock().unwrap().pop_front();
            let Some(message) = message else { break };
            assert_eq!(registry.dispatch(&message).unwrap(), Disposition::Processed);
        }
    }
}

fn queue() -> QueueConfig {
    QueueConfig {
        initial_capacity: 2,
        max_capacity: 8,
        max_bytes: 0,
        max_completed_count: 0,
        max_completed_bytes: 0,
        clean_on_complete: false,
    }
}

fn sender() -> SenderConfig {
    SenderConfig {
        initial_window: 4,
        max_window: 8,
        retry: Duration::from_secs(1),
        decay_factor: 0.5,
        decay_interval: Duration::from_secs(1),
    }
}

fn copy_config() -> CopyConfig {
    CopyConfig {
        sender: sender(),
        buffer_policy: BufferPolicy::RawSingle,
        buffer_limits: BufferLimits {
            max_segments: 4,
            max_payload_bytes: 64,
        },
    }
}

struct Node<P = Provider> {
    adapter: ServiceReplicator,
    service: ServiceHandle,
    provider: Arc<P>,
    access: Arc<AtomicU8>,
    faults: Arc<Mutex<Vec<SecondaryError>>>,
    bus: Bus,
    registry: MessageRegistry,
    id: i64,
}

impl Node {
    fn new(id: i64, persisted: bool, eos: bool, bus: Bus, registry: MessageRegistry) -> Self {
        Self::with_provider(id, persisted, eos, bus, registry, Arc::new(Provider::new()))
    }
}

impl<P: StateProvider + 'static> Node<P> {
    fn with_provider(
        id: i64,
        persisted: bool,
        eos: bool,
        bus: Bus,
        registry: MessageRegistry,
        provider: Arc<P>,
    ) -> Self {
        let send = bus.clone();
        let acks = bus.clone();
        let contexts = bus.clone();
        let faults = Arc::new(Mutex::new(Vec::new()));
        let reported = faults.clone();
        let access = Arc::new(AtomicU8::new(0));
        let granted = access.clone();
        let adapter = ServiceReplicator::new(
            RoleConfig {
                primary: PrimaryConfig {
                    queue: queue(),
                    initial_progress: 0,
                    endpoint: endpoint(id),
                    address: format!("replica-{id}"),
                    epoch: epoch(1),
                    allow_multiple_quorum_set: true,
                    sender: sender(),
                    queue_policy: QueuePolicyConfig::for_persisted(persisted),
                    send: Arc::new(move |intent| send.send(id, intent)),
                },
                secondary: SecondaryConfig {
                    replication: ReceiverConfig {
                        queue: queue(),
                        has_persisted_state: persisted,
                        require_service_ack: false,
                        enable_eos_ack: eos,
                    },
                    copy: SecondaryCopyConfig {
                        initial_capacity: 2,
                        max_capacity: 8,
                        require_service_ack: persisted,
                        enable_eos_ack: eos,
                        copy_done: false,
                    },
                    context: copy_config(),
                    batch_ack: Duration::ZERO,
                    retry_ack: Duration::from_secs(1),
                    max_pending_acks: 4,
                    trace_interval: Duration::ZERO,
                },
                secondary_route: route(id, if id == 1 { 2 } else { 1 }),
                secondary_callbacks: SecondaryCallbacks {
                    ack: Arc::new(move |route, ack, _| {
                        acks.push(
                            &route,
                            wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
                                replication_received: ack.replication.received,
                                replication_applied: ack.replication.applied,
                                copy_received: ack.copy.received,
                                copy_applied: ack.copy.applied,
                                error: ack.error,
                            }),
                        );
                        true
                    }),
                    context: Arc::new(move |route, action| contexts.copy(&route, action, true)),
                    fault: Arc::new(move |fault| reported.lock().unwrap().push(fault)),
                },
                cached_progress: None,
            },
            provider.clone(),
            Clock::new(),
            registry.clone(),
            Arc::new(move || match granted.load(Ordering::SeqCst) {
                0 => WriteAccess::Granted,
                1 => WriteAccess::ReconfigurationPending,
                2 => WriteAccess::NoWriteQuorum,
                _ => WriteAccess::NotPrimary,
            }),
        )
        .unwrap();
        let service = adapter.service();
        Self {
            adapter,
            service,
            provider,
            access,
            faults,
            bus,
            registry,
            id,
        }
    }

    async fn primary(&mut self) {
        self.adapter.open().await.unwrap();
        self.adapter
            .change_role(epoch(1), Role::Primary)
            .await
            .unwrap();
        self.adapter
            .update_current_configuration(configuration(self.id, 1, &[]))
            .unwrap();
    }

    async fn secondary(&mut self) {
        self.adapter.open().await.unwrap();
        self.adapter
            .change_role(epoch(1), Role::IdleSecondary)
            .await
            .unwrap();
    }

    fn start(&self) {
        let message =
            route(1, self.id).envelope(wire::envelope::Body::StartCopy(wire::StartCopy {
                epoch: Some(wire_epoch(epoch(1))),
                replica: self.id,
                replication_start: 1,
            }));
        assert_eq!(
            self.registry.dispatch(&message).unwrap(),
            Disposition::Processed
        );
    }

    fn receive(&self, body: wire::envelope::Body) {
        assert_eq!(
            self.registry
                .dispatch(&route(1, self.id).envelope(body))
                .unwrap(),
            Disposition::Processed
        );
    }

    fn end_copy(&self) {
        self.receive(wire::envelope::Body::CopyOperation(wire::CopyOperation {
            metadata: Some(metadata(1)),
            epoch: Some(wire_epoch(epoch(1))),
            replica: self.id,
            buffers: Some(wire::Buffers { segments: vec![] }),
            is_last: true,
        }));
    }

    fn build(&self, to: i64, persisted: bool) -> BuildOperation {
        let (config, callbacks) = self.build_parts(to, persisted);
        self.adapter.build_replica(config, callbacks).unwrap()
    }

    fn build_parts(&self, to: i64, persisted: bool) -> (BuildConfig, BuildCallbacks) {
        let send = self.bus.clone();
        let copies = self.bus.clone();
        let contexts = self.bus.clone();
        let from = self.id;
        (
            BuildConfig {
                route: route(from, to),
                remote_incarnation: to,
                copy: copy_config(),
                context: persisted.then_some(CopyContextConfig {
                    initial_queue_size: 2,
                    max_queue_size: 8,
                    batch_ack_interval: Duration::ZERO,
                    retry_interval: Duration::from_secs(1),
                    max_pending_acknowledgements: 4,
                    trace_interval: Duration::ZERO,
                }),
            },
            BuildCallbacks {
                replication: Arc::new(move |intent| send.send(from, intent)),
                copy: Arc::new(move |action| copies.copy(&route(from, to), action, false)),
                context_ack: Arc::new(move |ack, _| {
                    contexts.push(
                        &route(from, to),
                        wire::envelope::Body::CopyContextAck(wire::CopyContextAck {
                            sequence: ack.completed,
                            error: ack.error_code,
                        }),
                    );
                    true
                }),
            },
        )
    }
}

fn configuration(from: i64, quorum: usize, peers: &[(i64, Lsn)]) -> ReplicaConfiguration {
    ReplicaConfiguration {
        write_quorum: quorum,
        replicas: peers
            .iter()
            .map(|&(id, progress)| ReplicaDescriptor {
                route: route(from, id),
                remote_incarnation: id,
                progress: Some(progress),
                must_catchup: false,
            })
            .collect(),
    }
}

fn lone() -> Node {
    Node::new(1, true, false, Bus::default(), MessageRegistry::default())
}

async fn take(stream: &mut ServiceStream) -> ServiceOperation {
    stream.next().await.unwrap().unwrap()
}

#[tokio::test]
async fn opened_query_is_lazy_resumable_cached_and_errors_are_not_success() {
    let provider = Arc::new(Provider {
        progress_gate: Semaphore::new(0),
        ..Provider::new()
    });
    provider.progress.store(-17, Ordering::SeqCst);
    let mut node = Node::with_provider(
        1,
        true,
        false,
        Bus::default(),
        MessageRegistry::default(),
        provider,
    );
    let published = node.adapter.open().await.unwrap();
    assert_eq!(published.endpoint, endpoint(1));
    assert_eq!(published.address, "replica-1");
    assert_eq!(node.provider.progress_calls.load(Ordering::SeqCst), 0);
    assert_eq!(node.adapter.catchup_capability().unwrap(), 0);
    assert!(node.adapter.current_progress().now_or_never().is_none());
    assert!(node.adapter.resume().now_or_never().is_none());
    assert!(matches!(
        node.adapter.close(Duration::ZERO).await,
        Err(AdapterError::Role(RoleError::Busy))
    ));
    assert_eq!(node.provider.progress_calls.load(Ordering::SeqCst), 1);
    node.provider.progress_gate.add_permits(1);
    assert!(matches!(
        node.adapter.resume().await.unwrap(),
        ControlResult::Progress(0)
    ));
    assert_eq!(node.adapter.current_progress().await.unwrap(), 0);
    node.adapter
        .change_role(epoch(1), Role::Primary)
        .await
        .unwrap();
    assert_eq!(node.provider.progress_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        node.adapter.resume().await,
        Err(AdapterError::Role(RoleError::NoOperation))
    ));

    let mut node = lone();
    node.adapter.open().await.unwrap();
    node.provider.fail_progress.store(true, Ordering::SeqCst);
    assert!(matches!(
        node.adapter.current_progress().await,
        Err(AdapterError::Role(RoleError::Provider(_)))
    ));
    assert_eq!(node.adapter.status(), RoleStatus::Opened);
    node.provider.fail_progress.store(false, Ordering::SeqCst);
    assert_eq!(node.adapter.current_progress().await.unwrap(), 0);
    assert_eq!(node.provider.progress_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cancelled_role_epoch_and_data_loss_waiters_retain_actual_callbacks_and_results() {
    let provider = Arc::new(Provider {
        epoch_gate: Semaphore::new(0),
        ..Provider::new()
    });
    let mut node = Node::with_provider(
        1,
        true,
        false,
        Bus::default(),
        MessageRegistry::default(),
        provider,
    );
    node.adapter.open().await.unwrap();
    assert!(
        node.adapter
            .change_role(epoch(1), Role::Primary)
            .now_or_never()
            .is_none()
    );
    assert!(node.service.begin_replicate(Bytes::new()).is_err());
    assert!(node.adapter.resume().now_or_never().is_none());
    node.provider.epoch_gate.add_permits(1);
    node.adapter.resume().await.unwrap();
    assert_eq!(
        node.provider.epochs.lock().unwrap().as_slice(),
        &[(epoch(1), 0)]
    );
    node.adapter
        .update_current_configuration(configuration(1, 1, &[]))
        .unwrap();
    let mut write = node
        .service
        .begin_replicate(Bytes::from_static(b"one"))
        .unwrap();
    write.completion.wait().await.unwrap();
    assert!(node.adapter.update_epoch(epoch(2)).now_or_never().is_none());
    assert!(node.adapter.resume().now_or_never().is_none());
    assert_eq!(
        node.provider.epochs.lock().unwrap().last(),
        Some(&(epoch(2), 1))
    );
    node.provider.epoch_gate.add_permits(1);
    node.adapter.resume().await.unwrap();
    assert_eq!(node.provider.epochs.lock().unwrap().len(), 2);

    node.provider
        .loss_gate
        .acquire_many(100)
        .await
        .unwrap()
        .forget();
    node.provider.changed.store(true, Ordering::SeqCst);
    node.provider.progress.store(7, Ordering::SeqCst);
    assert!(node.adapter.on_data_loss().now_or_never().is_none());
    assert!(node.service.begin_replicate(Bytes::new()).is_err());
    node.provider.loss_gate.add_permits(1);
    let ControlResult::DataLoss(end) = node.adapter.resume().await.unwrap() else {
        panic!("data loss")
    };
    assert!(end.state_changed);
    end.result.unwrap();
    assert_eq!(node.provider.loss_calls.load(Ordering::SeqCst), 1);
    assert_eq!(node.service.begin_replicate(Bytes::new()).unwrap().lsn, 8);

    node.provider.loss_gate.add_permits(1);
    node.provider.fail_progress.store(true, Ordering::SeqCst);
    let end = node.adapter.on_data_loss().await.unwrap();
    assert!(end.state_changed);
    assert!(matches!(end.result, Err(RoleError::Provider(_))));
    assert_eq!(node.adapter.status(), RoleStatus::Ready(Role::Primary));
}

#[tokio::test]
async fn access_precedes_role_and_failed_validation_or_admission_does_not_consume_lsn() {
    let mut node = lone();
    for (value, expected) in [
        (1, WriteAccess::ReconfigurationPending),
        (2, WriteAccess::NoWriteQuorum),
        (3, WriteAccess::NotPrimary),
    ] {
        node.access.store(value, Ordering::SeqCst);
        assert!(
            matches!(node.service.begin_replicate(Bytes::new()), Err(AdapterError::WriteAccess(actual)) if actual == expected)
        );
    }
    assert!(matches!(
        node.service.begin_replicate(Bytes::from(vec![0; 65])),
        Err(AdapterError::Buffer(BufferError::TooLarge))
    ));
    node.access.store(0, Ordering::SeqCst);
    node.adapter.open().await.unwrap();
    node.adapter
        .change_role(epoch(1), Role::Primary)
        .await
        .unwrap();
    assert!(matches!(
        node.service.begin_replicate(Bytes::new()),
        Err(AdapterError::Primary(_))
    ));
    node.adapter
        .update_current_configuration(configuration(1, 2, &[]))
        .unwrap();
    for expected in 1..=8 {
        let write = node.service.begin_replicate(Bytes::new()).unwrap();
        assert_eq!(write.lsn, expected);
        drop(write);
    }
    assert!(matches!(
        node.service.begin_replicate(Bytes::new()),
        Err(AdapterError::Primary(_))
    ));
    node.adapter
        .update_current_configuration(configuration(1, 1, &[]))
        .unwrap();
    assert_eq!(node.service.begin_replicate(Bytes::new()).unwrap().lsn, 9);
}

#[tokio::test]
async fn abandoned_completion_is_explicit_and_wait_cancellation_is_resumable() {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let mut completion = Completion::new(receiver);
    assert!(completion.wait().now_or_never().is_none());
    drop(sender);
    assert!(matches!(
        completion.wait().await,
        Err(AdapterError::CompletionDropped)
    ));
    assert!(matches!(
        completion.wait().await,
        Err(AdapterError::CompletionDropped)
    ));
    let mut node = lone();
    node.primary().await;
    let mut write = node.service.begin_replicate(Bytes::new()).unwrap();
    write.completion.wait().await.unwrap();
    write.completion.wait().await.unwrap();
    assert_eq!(node.provider.progress.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn shared_streams_ack_copy_and_replication_without_rewind_or_progress_arguments() {
    let mut node = Node::new(2, true, false, Bus::default(), MessageRegistry::default());
    node.secondary().await;
    let mut copy = node.service.copy_stream().unwrap();
    assert!(copy.next().now_or_never().is_none());
    node.start();
    node.end_copy();
    assert!(copy.next().await.is_none());
    assert!(node.service.copy_stream().unwrap().next().await.is_none());
    assert!(copy.report_fault("unsupported".into()).is_err());

    let mut first = node.service.replication_stream().unwrap();
    let mut second = node.service.replication_stream().unwrap();
    node.receive(replication(
        1,
        epoch(1),
        epoch(1),
        Bytes::from_static(b"one"),
        0,
    ));
    node.receive(replication(
        2,
        epoch(1),
        epoch(1),
        Bytes::from_static(b"two"),
        0,
    ));
    let one = take(&mut first).await;
    let two = take(&mut second).await;
    assert_eq!((one.lsn(), two.lsn()), (1, 2));
    assert_eq!(one.data().as_ref(), b"one");
    assert_eq!(one.epoch(), epoch(1));
    assert!(!one.is_end_of_stream());
    assert_eq!(node.adapter.current_progress().await.unwrap(), 2);
    assert!(
        node.service
            .replication_stream()
            .unwrap()
            .next()
            .now_or_never()
            .is_none()
    );
    two.acknowledge().unwrap();
    assert_eq!(node.adapter.catchup_capability().unwrap(), 0);
    one.acknowledge().unwrap();
    assert_eq!(node.adapter.catchup_capability().unwrap(), 1);
    assert!(matches!(
        one.clone().acknowledge(),
        Err(AdapterError::Ack(AckError::InvalidOperation))
    ));
    node.adapter.close(Duration::ZERO).await.unwrap();
    assert!(first.next().await.unwrap().is_err());
    assert!(second.next().await.unwrap().is_err());
    assert!(first.next().await.is_none());
    assert!(second.next().await.is_none());
    assert!(node.service.replication_stream().is_err());
}

#[tokio::test]
async fn secondary_queries_before_copy_report_not_ready_once_without_synthetic_progress() {
    for capability in [false, true] {
        let mut node = Node::new(2, true, false, Bus::default(), MessageRegistry::default());
        node.secondary().await;
        let result = if capability {
            node.adapter.catchup_capability()
        } else {
            node.adapter.current_progress().await
        };
        assert!(matches!(
            result,
            Err(AdapterError::Secondary(SecondaryError::NotReady))
        ));
        assert_eq!(node.faults.lock().unwrap().len(), 1);
        assert!(matches!(
            node.adapter.current_progress().await,
            Err(AdapterError::Secondary(SecondaryError::Fault(_)))
        ));
        assert_eq!(node.faults.lock().unwrap().len(), 1);
        assert!(matches!(
            node.registry
                .dispatch(
                    &route(1, 2).envelope(wire::envelope::Body::RequestAck(wire::RequestAck {}))
                )
                .unwrap(),
            Disposition::Dropped(DropReason::Faulted)
        ));
    }
}

#[tokio::test]
async fn hidden_stream_epoch_marker_waits_for_one_retained_provider_callback() {
    let provider = Arc::new(Provider {
        epoch_gate: Semaphore::new(0),
        ..Provider::new()
    });
    let mut node = Node::with_provider(
        2,
        true,
        false,
        Bus::default(),
        MessageRegistry::default(),
        provider,
    );
    node.secondary().await;
    node.start();
    node.end_copy();
    let mut stream = node.service.replication_stream().unwrap();
    node.receive(replication(
        1,
        epoch(2),
        epoch(2),
        Bytes::from_static(b"new-epoch"),
        0,
    ));
    assert!(stream.next().now_or_never().is_none());
    assert!(node.adapter.drive().now_or_never().is_none());
    assert!(node.adapter.drive().now_or_never().is_none());
    assert_eq!(
        node.provider.epochs.lock().unwrap().as_slice(),
        &[(epoch(2), 0)]
    );
    assert!(stream.next().now_or_never().is_none());
    node.provider.epoch_gate.add_permits(1);
    assert!(node.adapter.drive().now_or_never().is_none());
    let operation = take(&mut stream).await;
    assert_eq!(operation.data().as_ref(), b"new-epoch");
    operation.acknowledge().unwrap();
    node.adapter.update_epoch(epoch(3)).await.unwrap();
    assert_eq!(node.provider.epochs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn eos_close_is_resumable_and_waits_for_real_service_eos_ack() {
    let mut node = Node::new(2, true, true, Bus::default(), MessageRegistry::default());
    node.secondary().await;
    node.start();
    let mut copy = node.service.copy_stream().unwrap();
    let mut replication = node.service.replication_stream().unwrap();
    node.end_copy();
    let copy_end = take(&mut copy).await;
    assert!(copy_end.is_end_of_stream());
    assert!(node.adapter.close(Duration::ZERO).now_or_never().is_none());
    assert!(node.service.replication_stream().is_err());
    let end = take(&mut replication).await;
    assert!(end.is_end_of_stream());
    end.acknowledge().unwrap();
    assert!(node.adapter.resume().now_or_never().is_none());
    copy_end.acknowledge().unwrap();
    node.adapter.resume().await.unwrap();
    assert_eq!(node.adapter.status(), RoleStatus::Closed);
    assert!(copy.next().await.is_none());
    assert!(replication.next().await.is_none());
}

#[tokio::test]
async fn abort_and_abandonment_seal_handles_streams_and_pending_assignments() {
    let mut primary = lone();
    primary.primary().await;
    primary
        .adapter
        .update_current_configuration(configuration(1, 2, &[]))
        .unwrap();
    let mut pending = primary.service.begin_replicate(Bytes::new()).unwrap();
    primary.adapter.abort().unwrap();
    assert!(matches!(
        primary.adapter.resume().await,
        Err(AdapterError::Role(RoleError::Aborted))
    ));
    primary.adapter.abort().unwrap();
    assert!(matches!(
        primary.adapter.resume().await,
        Err(AdapterError::Role(RoleError::Aborted))
    ));
    assert!(matches!(
        pending.completion.wait().await,
        Err(AdapterError::Completion(CompletionError::OperationCanceled))
    ));
    assert!(primary.service.begin_replicate(Bytes::new()).is_err());
    drop(primary.adapter);
    assert!(matches!(
        primary.service.begin_replicate(Bytes::new()),
        Err(AdapterError::OwnerDropped)
    ));

    let mut secondary = Node::new(2, true, false, Bus::default(), MessageRegistry::default());
    secondary.secondary().await;
    secondary.start();
    secondary.end_copy();
    let mut stream = secondary.service.replication_stream().unwrap();
    secondary.receive(replication(1, epoch(1), epoch(1), Bytes::new(), 0));
    let retained = take(&mut stream).await;
    drop(secondary.adapter);
    retained.acknowledge().unwrap();
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    assert!(matches!(
        secondary.service.copy_stream(),
        Err(AdapterError::OwnerDropped)
    ));
}

#[tokio::test]
async fn abort_during_initial_recovery_does_not_publish_late_success() {
    let provider = Arc::new(Provider {
        progress_gate: Semaphore::new(0),
        ..Provider::new()
    });
    let mut node = Node::with_provider(
        1,
        true,
        false,
        Bus::default(),
        MessageRegistry::default(),
        provider,
    );
    node.adapter.open().await.unwrap();
    assert!(
        node.adapter
            .change_role(epoch(1), Role::Primary)
            .now_or_never()
            .is_none()
    );
    node.adapter.abort().unwrap();
    assert!(node.adapter.resume().now_or_never().is_none());
    node.provider.progress_gate.add_permits(1);
    assert!(matches!(
        node.adapter.resume().await,
        Err(AdapterError::Role(RoleError::Aborted))
    ));
    assert!(node.service.begin_replicate(Bytes::new()).is_err());
    assert_eq!(node.provider.progress_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn abandoned_owner_wakes_build_waiters_with_error_instead_of_inventing_end_or_hanging() {
    let mut node = lone();
    node.primary().await;
    let build = node.build(2, false);
    let first = build.wait();
    let second = build.wait();
    tokio::pin!(first, second);
    assert!(futures::poll!(&mut first).is_pending());
    assert!(futures::poll!(&mut second).is_pending());
    drop(node.adapter);
    let (first, second) = tokio::join!(first, second);
    assert!(matches!(
        first,
        Err(AdapterError::Build(BuildError::OwnerDropped))
    ));
    assert!(matches!(
        second,
        Err(AdapterError::Build(BuildError::OwnerDropped))
    ));

    let mut node = lone();
    node.primary().await;
    node.adapter
        .update_current_configuration(configuration(1, 2, &[]))
        .unwrap();
    let mut write = node.service.begin_replicate(Bytes::new()).unwrap();
    drop(node.adapter);
    assert!(matches!(
        write.completion.wait().await,
        Err(AdapterError::Completion(CompletionError::OperationCanceled))
    ));
}

struct MemoryService {
    visible: BTreeMap<Lsn, Bytes>,
    commit_gate: Semaphore,
    fail_commit: bool,
}

impl MemoryService {
    async fn finish(&mut self, write: &mut Replication, data: Bytes) -> crate::Result<Lsn> {
        write
            .completion
            .wait()
            .await
            .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))?;
        self.commit_gate.acquire().await.unwrap().forget();
        if self.fail_commit || write.lsn != self.visible.len() as Lsn + 1 {
            return Err(RuntimeError::InvalidReplication(
                "service commit rejected".into(),
            ));
        }
        self.visible.insert(write.lsn, data);
        Ok(write.lsn)
    }
}

#[tokio::test]
async fn helper_free_copy_replication_and_service_commit_use_actual_end_to_end_owners() {
    let registry = MessageRegistry::default();
    let bus = Bus::default();
    let mut source = Node::new(1, true, false, bus.clone(), registry.clone());
    let mut target = Node::new(2, true, false, bus.clone(), registry.clone());
    source.primary().await;
    target.secondary().await;
    let mut copy = target.service.copy_stream().unwrap();
    let mut replication = target.service.replication_stream().unwrap();
    let build = source.build(2, true);
    assert!(build.wait().now_or_never().is_none());
    bus.deliver(&registry);
    assert!(target.adapter.drive().now_or_never().is_none());
    bus.deliver(&registry);
    assert!(source.adapter.drive().now_or_never().is_none());
    bus.deliver(&registry);
    let snapshot = take(&mut copy).await;
    assert_eq!(snapshot.data().as_ref(), b"snapshot");
    assert!(build.wait().now_or_never().is_none());
    snapshot.acknowledge().unwrap();
    assert!(copy.next().await.is_none());
    bus.deliver(&registry);
    assert!(source.adapter.drive().now_or_never().is_none());
    build.wait().await.unwrap();
    build.wait().await.unwrap();
    assert_eq!(source.provider.copy_calls.load(Ordering::SeqCst), 1);
    source
        .adapter
        .update_catchup_configuration(None, configuration(1, 2, &[(2, 0)]))
        .unwrap();
    target
        .adapter
        .change_role(epoch(1), Role::ActiveSecondary)
        .await
        .unwrap();

    let mut first = source
        .service
        .begin_replicate(Bytes::from_static(b"one"))
        .unwrap();
    let mut second = source
        .service
        .begin_replicate(Bytes::from_static(b"two"))
        .unwrap();
    assert_eq!((first.lsn, second.lsn), (1, 2));
    assert!(first.completion.wait().now_or_never().is_none());
    assert!(second.completion.wait().now_or_never().is_none());
    assert_eq!(source.adapter.current_progress().await.unwrap(), 0);
    let mut catchup = source.adapter.wait_for_catchup(CatchupMode::All).unwrap();
    assert!(catchup.wait().now_or_never().is_none());
    bus.deliver(&registry);
    let one = take(&mut replication).await;
    let two = take(&mut replication).await;
    assert_eq!(one.data().as_ref(), b"one");
    assert_eq!(two.data().as_ref(), b"two");
    two.acknowledge().unwrap();
    bus.deliver(&registry);
    assert!(first.completion.wait().now_or_never().is_none());
    one.acknowledge().unwrap();
    bus.deliver(&registry);
    first.completion.wait().await.unwrap();
    second.completion.wait().await.unwrap();
    catchup.wait().await.unwrap();
    source
        .adapter
        .update_current_configuration(configuration(1, 2, &[(2, 2)]))
        .unwrap();
    assert_eq!(source.adapter.current_progress().await.unwrap(), 2);
    assert_eq!(source.adapter.catchup_capability().unwrap(), 0);

    let mut service = MemoryService {
        visible: BTreeMap::new(),
        commit_gate: Semaphore::new(0),
        fail_commit: false,
    };
    assert!(
        service
            .finish(&mut first, Bytes::from_static(b"one"))
            .now_or_never()
            .is_none()
    );
    assert!(service.visible.is_empty());
    service.commit_gate.add_permits(1);
    assert!(
        service
            .finish(&mut second, Bytes::from_static(b"two"))
            .await
            .is_err()
    );
    assert!(service.visible.is_empty());
    service.commit_gate.add_permits(1);
    assert_eq!(
        service
            .finish(&mut first, Bytes::from_static(b"one"))
            .await
            .unwrap(),
        1
    );
    service.commit_gate.add_permits(1);
    service.fail_commit = true;
    assert!(
        service
            .finish(&mut second, Bytes::from_static(b"two"))
            .await
            .is_err()
    );
    assert_eq!(service.visible.len(), 1);
    assert!(!service.visible.contains_key(&2));
    assert_eq!(source.provider.progress.load(Ordering::SeqCst), 0);

    let third = source
        .service
        .begin_replicate(Bytes::from_static(b"three"))
        .unwrap();
    assert_eq!(third.lsn, 3);
    drop(third);
    bus.deliver(&registry);
    take(&mut replication).await.acknowledge().unwrap();
    bus.deliver(&registry);
    assert_eq!(source.adapter.current_progress().await.unwrap(), 3);
    let mut fourth = source
        .service
        .begin_replicate(Bytes::from_static(b"four"))
        .unwrap();
    assert!(
        source
            .adapter
            .close(Duration::from_secs(5))
            .now_or_never()
            .is_none()
    );
    assert!(source.service.begin_replicate(Bytes::new()).is_err());
    bus.deliver(&registry);
    take(&mut replication).await.acknowledge().unwrap();
    bus.deliver(&registry);
    source.adapter.resume().await.unwrap();
    fourth.completion.wait().await.unwrap();
    target.adapter.close(Duration::ZERO).await.unwrap();
    source.adapter.drain_retired_builds().await.unwrap();
}

#[tokio::test]
async fn build_cancel_and_remove_are_real_observable_outcomes_not_waiter_drop() {
    for remove in [false, true] {
        let registry = MessageRegistry::default();
        let bus = Bus::default();
        let mut node = Node::new(1, false, false, bus.clone(), registry.clone());
        let mut target = Node::new(2, false, false, bus.clone(), registry.clone());
        node.primary().await;
        target.secondary().await;
        let build = node.build(2, false);
        assert!(build.wait().now_or_never().is_none());
        if remove {
            node.adapter.remove_replica(2).unwrap();
        } else {
            build.cancel().unwrap();
        }
        assert!(node.adapter.drive().now_or_never().is_none());
        if !remove {
            assert!(build.wait().now_or_never().is_none());
            bus.deliver(&registry);
            assert!(node.adapter.drive().now_or_never().is_none());
        }
        assert!(build.wait().await.is_err());
        assert_eq!(node.provider.copy_calls.load(Ordering::SeqCst), 0);
        if !remove {
            node.adapter.remove_replica(2).unwrap();
        }
        node.adapter.close(Duration::ZERO).await.unwrap();
        node.adapter.drain_retired_builds().await.unwrap();
    }
}

#[tokio::test]
async fn role_handoffs_refresh_the_same_service_handle_and_preserve_real_queue_progress() {
    let mut node = lone();
    node.primary().await;
    let mut write = node
        .service
        .begin_replicate(Bytes::from_static(b"one"))
        .unwrap();
    write.completion.wait().await.unwrap();
    assert!(node.service.copy_stream().is_err());
    node.adapter
        .change_role(epoch(2), Role::ActiveSecondary)
        .await
        .unwrap();
    assert!(matches!(
        node.service.begin_replicate(Bytes::new()),
        Err(AdapterError::WriteAccess(WriteAccess::NotPrimary))
    ));
    assert_eq!(node.adapter.current_progress().await.unwrap(), 1);
    assert!(node.service.copy_stream().unwrap().next().await.is_none());
    node.adapter
        .change_role(epoch(3), Role::Primary)
        .await
        .unwrap();
    assert!(node.service.replication_stream().is_err());
    node.adapter
        .update_current_configuration(configuration(1, 1, &[]))
        .unwrap();
    assert_eq!(node.service.begin_replicate(Bytes::new()).unwrap().lsn, 2);
    node.adapter
        .change_role(epoch(3), Role::None)
        .await
        .unwrap();
    assert!(node.service.begin_replicate(Bytes::new()).is_err());
    node.adapter.close(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn volatile_dispatch_completes_before_dequeue_and_provider_failure_seals_primary_admission() {
    let mut node = Node::new(2, false, false, Bus::default(), MessageRegistry::default());
    node.secondary().await;
    node.start();
    node.end_copy();
    node.receive(replication(
        1,
        epoch(1),
        epoch(1),
        Bytes::from_static(b"volatile"),
        0,
    ));
    assert!(node.bus.0.lock().unwrap().iter().any(|message| matches!(
        &message.body, Some(wire::envelope::Body::ReplicationAck(ack)) if ack.replication_applied == 1
    )));
    let mut stream = node.service.replication_stream().unwrap();
    let operation = take(&mut stream).await;
    operation.acknowledge().unwrap();
    assert!(operation.acknowledge().is_err());

    let mut node = lone();
    node.primary().await;
    node.provider.fail_epoch.store(true, Ordering::SeqCst);
    assert!(matches!(
        node.adapter.update_epoch(epoch(2)).await,
        Err(AdapterError::Role(RoleError::Provider(_)))
    ));
    assert_eq!(node.adapter.status(), RoleStatus::Faulted);
    assert!(node.service.begin_replicate(Bytes::new()).is_err());
    assert!(node.adapter.current_progress().await.is_err());
}
