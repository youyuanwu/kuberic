// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-component vectors derived from the pinned native sources in role.rs.

use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{FutureExt, StreamExt, stream};
use tokio::sync::Semaphore;

use crate::application::OperationDataStream;
use crate::replicator::v1::buffers::{BufferLimits, BufferPolicy};
use crate::replicator::v1::build_idle::{BuildCallbacks, BuildConfig};
use crate::replicator::v1::copy_async_operation::CopyConfig;
use crate::replicator::v1::operation_queue::{QueueConfig, QueueOperation};
use crate::replicator::v1::operation_stream::{EpochBarrier, OperationStream, StreamEvent};
use crate::replicator::v1::primary::{ReplicaConfiguration, ReplicaDescriptor};
use crate::replicator::v1::received_operation::ReceivedOperation;
use crate::replicator::v1::replica_manager::CompletionError;
use crate::replicator::v1::secondary_copy_receiver::SecondaryCopyConfig;
use crate::replicator::v1::secondary_receiver::ReceiverConfig;
use crate::replicator::v1::sender::SenderConfig;
use crate::replicator::v1::session::{SendIntent, SendKind};
use crate::replicator::v1::transport::wire;

mod data_loss;
mod ingress;
mod open;

struct Provider {
    epochs: Mutex<Vec<EpochBarrier>>,
    progress_calls: AtomicUsize,
    progress: Lsn,
    epochs_gate: Semaphore,
    progress_gate: Semaphore,
    fail_epoch: AtomicBool,
    fail_progress: AtomicBool,
    fail_copy: AtomicBool,
    copy_calls: AtomicUsize,
    copy_boundaries: Mutex<Vec<Lsn>>,
    copy_gate: Semaphore,
    data_loss_calls: AtomicUsize,
    data_loss_gate: Semaphore,
    state_changed: AtomicBool,
    fail_data_loss: AtomicBool,
    recovery_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Provider {
    fn new() -> Self {
        Self {
            epochs: Mutex::new(Vec::new()),
            progress_calls: AtomicUsize::new(0),
            progress: 0,
            epochs_gate: Semaphore::new(100),
            progress_gate: Semaphore::new(1),
            fail_epoch: AtomicBool::new(false),
            fail_progress: AtomicBool::new(false),
            fail_copy: AtomicBool::new(false),
            copy_calls: AtomicUsize::new(0),
            copy_boundaries: Mutex::new(Vec::new()),
            copy_gate: Semaphore::new(100),
            data_loss_calls: AtomicUsize::new(0),
            data_loss_gate: Semaphore::new(100),
            state_changed: AtomicBool::new(false),
            fail_data_loss: AtomicBool::new(false),
            recovery_hook: None,
        }
    }
}

#[async_trait]
impl StateProvider for Provider {
    async fn update_epoch(&self, epoch: Epoch, previous_epoch_last_lsn: Lsn) -> crate::Result<()> {
        self.epochs.lock().unwrap().push(EpochBarrier {
            epoch,
            previous_epoch_last_lsn,
        });
        self.epochs_gate.acquire().await.unwrap().forget();
        if self.fail_epoch.load(Ordering::SeqCst) {
            return Err(RuntimeError::InvalidReplication("epoch failure".into()));
        }
        Ok(())
    }
    async fn last_committed_lsn(&self) -> crate::Result<Lsn> {
        self.progress_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(hook) = &self.recovery_hook {
            hook();
        }
        self.progress_gate.acquire().await.unwrap().forget();
        if self.fail_progress.load(Ordering::SeqCst) {
            return Err(RuntimeError::InvalidReplication("progress failure".into()));
        }
        Ok(self.progress)
    }
    async fn get_copy_context(&self) -> crate::Result<OperationDataStream> {
        Ok(Box::pin(stream::empty()))
    }
    async fn get_copy_state(
        &self,
        up_to_lsn: Lsn,
        _: OperationDataStream,
    ) -> crate::Result<OperationDataStream> {
        self.copy_calls.fetch_add(1, Ordering::SeqCst);
        self.copy_boundaries.lock().unwrap().push(up_to_lsn);
        self.copy_gate.acquire().await.unwrap().forget();
        if self.fail_copy.load(Ordering::SeqCst) {
            return Err(RuntimeError::InvalidReplication(
                "copy acquisition failure".into(),
            ));
        }
        Ok(Box::pin(stream::iter([Ok(Bytes::from_static(b"copy"))])))
    }
    async fn on_data_loss(&self) -> crate::Result<bool> {
        self.data_loss_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(hook) = &self.recovery_hook {
            hook();
        }
        self.data_loss_gate.acquire().await.unwrap().forget();
        if self.fail_data_loss.load(Ordering::SeqCst) {
            return Err(RuntimeError::InvalidReplication("data-loss failure".into()));
        }
        Ok(self.state_changed.load(Ordering::SeqCst))
    }
}

fn epoch(n: i64) -> Epoch {
    Epoch::new(1, n)
}

fn route(id: i64) -> Route {
    Route {
        source: wire::Endpoint {
            partition: Bytes::from(vec![1; 16]),
            replica: 20,
            incarnation: Bytes::from(vec![2; 16]),
        },
        destination: wire::Endpoint {
            partition: Bytes::from(vec![1; 16]),
            replica: id,
            incarnation: Bytes::from(vec![3; 16]),
        },
        sender_address: "local".into(),
        remote_address: format!("replica-{id}"),
    }
}

fn queue() -> QueueConfig {
    QueueConfig {
        initial_capacity: 2,
        max_capacity: 16,
        max_bytes: 0,
        max_completed_count: 0,
        max_completed_bytes: 0,
        clean_on_complete: false,
    }
}

fn sender() -> SenderConfig {
    SenderConfig {
        initial_window: 4,
        max_window: 16,
        retry: Duration::from_secs(1),
        decay_factor: 0.5,
        decay_interval: Duration::from_secs(1),
    }
}

fn limits() -> BufferLimits {
    BufferLimits {
        max_segments: 4,
        max_payload_bytes: 1024,
    }
}

struct Rig {
    owner: RoleReplicator,
    provider: Arc<Provider>,
    sent: Arc<Mutex<Vec<SendIntent>>>,
    required: bool,
    eos: bool,
}

impl Rig {
    fn new(required: bool, eos: bool) -> Self {
        Self::with_provider(required, eos, Arc::new(Provider::new()), None)
    }
    fn with_provider(
        required: bool,
        eos: bool,
        provider: Arc<Provider>,
        cached: Option<Lsn>,
    ) -> Self {
        let mut rig = Self::unopened(required, eos, provider, cached);
        rig.owner.open(&MessageRegistry::default()).unwrap();
        rig
    }
    fn unopened(required: bool, eos: bool, provider: Arc<Provider>, cached: Option<Lsn>) -> Self {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let recorded = sent.clone();
        let owner = RoleReplicator::new(
            RoleConfig {
                primary: PrimaryConfig {
                    queue: queue(),
                    initial_progress: 0,
                    endpoint: route(10).source,
                    address: "local".into(),
                    epoch: epoch(1),
                    allow_multiple_quorum_set: true,
                    sender: sender(),
                    queue_policy:
                        crate::replicator::v1::replica_manager::QueuePolicyConfig::for_persisted(
                            required,
                        ),
                    send: Arc::new(move |intent| {
                        recorded.lock().unwrap().push(intent);
                        true
                    }),
                },
                secondary: SecondaryConfig {
                    replication: ReceiverConfig {
                        queue: queue(),
                        has_persisted_state: required,
                        require_service_ack: false,
                        enable_eos_ack: eos,
                    },
                    copy: SecondaryCopyConfig {
                        initial_capacity: 2,
                        max_capacity: 16,
                        require_service_ack: required,
                        enable_eos_ack: eos,
                        copy_done: false,
                    },
                    context: CopyConfig {
                        sender: sender(),
                        buffer_policy: BufferPolicy::RawSingle,
                        buffer_limits: limits(),
                    },
                    batch_ack: Duration::from_millis(10),
                    retry_ack: Duration::from_secs(1),
                    max_pending_acks: 4,
                    trace_interval: Duration::ZERO,
                },
                secondary_route: route(10),
                secondary_callbacks: SecondaryCallbacks {
                    ack: Arc::new(|_, _, _| true),
                    context: Arc::new(|_, _| true),
                    fault: Arc::new(|_| {}),
                },
                cached_progress: cached,
            },
            provider.clone(),
            Clock::new(),
        )
        .unwrap();
        Self {
            owner,
            provider,
            sent,
            required,
            eos,
        }
    }
    async fn primary(&mut self) -> PrimaryHandle {
        self.owner
            .begin_change_role(Role::Primary, epoch(1))
            .unwrap();
        self.owner.run().await.unwrap();
        self.owner.primary().unwrap()
    }
    fn secondary(&mut self) -> SecondaryHandle {
        self.owner
            .begin_change_role(Role::IdleSecondary, epoch(1))
            .unwrap();
        self.owner.secondary().unwrap()
    }
    fn start(&self, handle: &SecondaryHandle) {
        handle
            .process_start(
                &route(10)
                    .reversed()
                    .envelope(wire::envelope::Body::StartCopy(wire::StartCopy {
                        epoch: Some(wire::Epoch {
                            data_loss: 1,
                            configuration: 1,
                        }),
                        replica: 20,
                        replication_start: 1,
                    })),
            )
            .unwrap();
    }
    async fn finish_copy(&self, handle: &SecondaryHandle) {
        let mut copy = handle.copy_stream().unwrap();
        handle
            .receive_copy(
                ReceivedOperation::copy_end_of_stream(1, self.required).unwrap(),
                true,
            )
            .unwrap();
        if self.eos {
            let last = take(&mut copy).await;
            ack_copy(handle, &last);
        }
    }
}

async fn take(stream: &mut OperationStream) -> ReceivedOperation {
    match stream.next().await.unwrap().unwrap() {
        StreamEvent::Operation(operation) => operation,
        _ => panic!("expected a data operation, not an epoch marker"),
    }
}

fn ack_copy(handle: &SecondaryHandle, operation: &ReceivedOperation) {
    if let Some(work) = operation.acknowledge().unwrap() {
        handle.acknowledge_copy(work).unwrap();
    }
}

fn ack_replication(handle: &SecondaryHandle, operation: &ReceivedOperation) {
    if let Some(work) = operation.acknowledge().unwrap() {
        handle.acknowledge_replication(work).unwrap();
    }
}

fn configuration(handle: &PrimaryHandle, quorum: usize, peers: &[(i64, Lsn)]) {
    handle
        .update_current_configuration(ReplicaConfiguration {
            write_quorum: quorum,
            replicas: peers
                .iter()
                .map(|&(id, progress)| ReplicaDescriptor {
                    route: route(id),
                    remote_incarnation: id,
                    progress: Some(progress),
                    must_catchup: false,
                })
                .collect(),
        })
        .unwrap();
}

fn enqueue(handle: &PrimaryHandle) -> super::super::primary::PrimaryAdmission {
    handle
        .enqueue(
            Bytes::from_static(b"data"),
            BufferPolicy::RawSingle,
            limits(),
        )
        .unwrap()
}

fn ack_primary(handle: &PrimaryHandle, id: i64, lsn: Lsn) {
    handle
        .receive_ack(
            &route(id)
                .reversed()
                .envelope(wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
                    replication_received: lsn,
                    replication_applied: lsn,
                    copy_received: -1,
                    copy_applied: -1,
                    error: 0,
                })),
        )
        .unwrap();
}

fn operation(lsn: Lsn, original: Epoch, required: bool) -> ReceivedOperation {
    ReceivedOperation::new(
        QueueOperation::new(lsn, Bytes::from_static(b"data"), true),
        original,
        required,
    )
}

#[tokio::test]
async fn initial_primary_queries_provider_before_epoch_and_requires_configuration() {
    let mut rig = Rig::new(true, false);
    let handle = rig.primary().await;
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.provider.data_loss_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        *rig.provider.epochs.lock().unwrap(),
        [EpochBarrier {
            epoch: epoch(1),
            previous_epoch_last_lsn: 0
        }]
    );
    assert!(
        handle
            .enqueue(Bytes::new(), BufferPolicy::RawSingle, limits())
            .is_err()
    );
    configuration(&handle, 1, &[]);
    assert_eq!(enqueue(&handle).completion.await.unwrap(), Ok(()));
}

#[tokio::test]
async fn cached_progress_skips_provider_query_and_sets_issued_boundary() {
    let mut rig = Rig::with_provider(true, false, Arc::new(Provider::new()), Some(7));
    let handle = rig.primary().await;
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        rig.provider.epochs.lock().unwrap()[0].previous_epoch_last_lsn,
        7
    );
    configuration(&handle, 1, &[]);
    assert_eq!(enqueue(&handle).lsn, 8);
}

#[tokio::test]
async fn interrupted_initial_progress_keeps_one_actual_call_and_busy_admission() {
    let provider = Arc::new(Provider {
        progress_gate: Semaphore::new(0),
        ..Provider::new()
    });
    let mut rig = Rig::with_provider(true, false, provider, None);
    rig.owner
        .begin_change_role(Role::Primary, epoch(2))
        .unwrap();
    assert!(rig.owner.operation_pending());
    for _ in 0..3 {
        assert!(rig.owner.run().now_or_never().is_none());
    }
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        rig.owner.begin_update_epoch(epoch(3)),
        Err(RoleError::Busy)
    ));
    assert!(rig.owner.primary().is_err());
    rig.provider.progress_gate.add_permits(1);
    rig.owner.run().await.unwrap();
    assert_eq!(rig.owner.status(), RoleStatus::Ready(Role::Primary));
}

#[tokio::test]
async fn interrupted_primary_epoch_keeps_provider_call_and_publishes_role_only_after_completion() {
    let provider = Arc::new(Provider {
        epochs_gate: Semaphore::new(0),
        ..Provider::new()
    });
    let mut rig = Rig::with_provider(true, false, provider, Some(0));
    rig.owner
        .begin_change_role(Role::Primary, epoch(2))
        .unwrap();
    for _ in 0..3 {
        assert!(rig.owner.run().now_or_never().is_none());
    }
    assert_eq!(rig.provider.epochs.lock().unwrap().len(), 1);
    assert!(rig.owner.primary().is_err());
    rig.provider.epochs_gate.add_permits(1);
    rig.owner.run().await.unwrap();
    assert!(rig.owner.primary().is_ok());
}

#[tokio::test]
async fn primary_external_epoch_uses_issued_not_committed_tail_and_updates_session_sends() {
    let mut rig = Rig::new(true, false);
    let primary = rig.primary().await;
    configuration(&primary, 2, &[(10, 0)]);
    let mut first = enqueue(&primary);
    rig.owner.begin_update_epoch(epoch(2)).unwrap();
    rig.owner.run().await.unwrap();
    let second = enqueue(&primary);
    assert_eq!(
        rig.provider.epochs.lock().unwrap()[1].previous_epoch_last_lsn,
        1
    );
    assert!(first.completion.try_recv().is_err());
    assert_eq!(second.lsn, 2);
    let sent = rig.sent.lock().unwrap();
    assert!(sent.iter().any(|intent| matches!(intent.kind,
        SendKind::Replication { sending_epoch, original_epoch, .. }
        if sending_epoch == epoch(2) && original_epoch == epoch(2))));
}

#[tokio::test]
async fn duplicate_primary_epoch_still_calls_provider_and_stale_epoch_does_not() {
    let mut rig = Rig::new(true, false);
    rig.primary().await;
    rig.owner.begin_update_epoch(epoch(1)).unwrap();
    rig.owner.run().await.unwrap();
    assert_eq!(rig.provider.epochs.lock().unwrap().len(), 2);
    assert!(rig.owner.begin_update_epoch(epoch(0)).is_err());
    assert_eq!(rig.owner.status(), RoleStatus::Ready(Role::Primary));
    assert_eq!(rig.provider.epochs.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn provider_epoch_failure_faults_role_and_existing_primary_handle() {
    let mut rig = Rig::new(true, false);
    let handle = rig.primary().await;
    configuration(&handle, 1, &[]);
    rig.provider.fail_epoch.store(true, Ordering::SeqCst);
    rig.owner.begin_update_epoch(epoch(2)).unwrap();
    assert!(matches!(rig.owner.run().await, Err(RoleError::Provider(_))));
    assert_eq!(rig.owner.status(), RoleStatus::Faulted);
    assert!(rig.owner.fault().is_some());
    assert!(
        handle
            .enqueue(Bytes::new(), BufferPolicy::RawSingle, limits())
            .is_err()
    );
}

#[tokio::test]
async fn progress_failure_faults_without_creating_primary_or_calling_epoch() {
    let mut rig = Rig::new(true, false);
    rig.provider.fail_progress.store(true, Ordering::SeqCst);
    rig.owner
        .begin_change_role(Role::Primary, epoch(1))
        .unwrap();
    assert!(matches!(rig.owner.run().await, Err(RoleError::Provider(_))));
    assert_eq!(rig.owner.status(), RoleStatus::Faulted);
    assert!(rig.provider.epochs.lock().unwrap().is_empty());
    rig.owner.abort().unwrap();
    assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
}

#[tokio::test]
async fn required_ack_promotion_waits_actual_data_ack_and_preserves_queue_arc_and_epoch() {
    let mut rig = Rig::new(true, false);
    let secondary = rig.secondary();
    rig.start(&secondary);
    rig.finish_copy(&secondary).await;
    let mut stream = secondary.replication_stream().unwrap();
    let data = operation(1, epoch(1), true);
    let original = data.queue_operation().clone();
    secondary
        .receive_replication(epoch(1), vec![data.clone()], None)
        .unwrap();
    let delivered = take(&mut stream).await;
    rig.owner
        .begin_change_role(Role::Primary, epoch(2))
        .unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    ack_replication(&secondary, &delivered);
    rig.owner.run().await.unwrap();
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 0);
    assert!(
        secondary
            .receive_replication(epoch(2), vec![operation(2, epoch(2), true)], None)
            .is_err()
    );
    let primary = rig.owner.primary().unwrap();
    assert!(
        primary
            .enqueue(Bytes::new(), BufferPolicy::RawSingle, limits())
            .is_err()
    );
    configuration(&primary, 2, &[(10, 0)]);
    let sent = rig.sent.lock().unwrap();
    assert!(sent.iter().any(|intent| match &intent.kind {
        SendKind::Replication {
            operation,
            original_epoch,
            sending_epoch,
            ..
        } =>
            Arc::ptr_eq(operation, &original)
                && *original_epoch == epoch(1)
                && *sending_epoch == epoch(2),
        _ => false,
    }));
}

#[tokio::test]
async fn optimistic_promotion_waits_dispatch_consumption_not_application_ack() {
    let mut rig = Rig::new(false, false);
    let secondary = rig.secondary();
    rig.start(&secondary);
    rig.finish_copy(&secondary).await;
    let mut stream = secondary.replication_stream().unwrap();
    secondary
        .receive_replication(epoch(1), vec![operation(1, epoch(1), false)], None)
        .unwrap();
    rig.owner
        .begin_change_role(Role::Primary, epoch(2))
        .unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    let held = take(&mut stream).await;
    rig.owner.run().await.unwrap();
    assert_eq!(
        held.ack_state(),
        super::super::received_operation::AckState::NotCalled
    );
    assert!(rig.owner.primary().is_ok());
}

#[tokio::test]
async fn eos_promotion_waits_actual_eos_ack_in_addition_to_data_ack() {
    let mut rig = Rig::new(true, true);
    let secondary = rig.secondary();
    rig.start(&secondary);
    rig.finish_copy(&secondary).await;
    let mut stream = secondary.replication_stream().unwrap();
    secondary
        .receive_replication(epoch(1), vec![operation(1, epoch(1), true)], None)
        .unwrap();
    let data = take(&mut stream).await;
    rig.owner
        .begin_change_role(Role::Primary, epoch(2))
        .unwrap();
    let eos = take(&mut stream).await;
    assert!(eos.is_end_of_stream());
    ack_replication(&secondary, &data);
    assert!(rig.owner.run().now_or_never().is_none());
    ack_replication(&secondary, &eos);
    rig.owner.run().await.unwrap();
}

#[tokio::test]
async fn promotion_rejects_incomplete_copy_in_both_ack_and_eos_modes() {
    for required in [false, true] {
        for eos in [false, true] {
            let mut rig = Rig::new(required, eos);
            let secondary = rig.secondary();
            rig.start(&secondary);
            assert!(
                rig.owner
                    .begin_change_role(Role::Primary, epoch(2))
                    .is_err()
            );
            assert_eq!(rig.owner.status(), RoleStatus::Faulted);
            assert!(rig.provider.epochs.lock().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn demotion_and_repromotion_keep_original_queue_entries_without_scalar_reconstruction() {
    let mut rig = Rig::new(true, false);
    let primary = rig.primary().await;
    configuration(&primary, 2, &[(10, 0), (30, 0)]);
    let write = enqueue(&primary);
    ack_primary(&primary, 10, 1);
    assert_eq!(write.completion.await.unwrap(), Ok(()));
    let original = rig
        .sent
        .lock()
        .unwrap()
        .iter()
        .find_map(|intent| match &intent.kind {
            SendKind::Replication { operation, .. } => Some(operation.clone()),
            _ => None,
        })
        .unwrap();
    rig.owner
        .begin_change_role(Role::ActiveSecondary, epoch(2))
        .unwrap();
    rig.owner.run().await.unwrap();
    assert_eq!(rig.provider.epochs.lock().unwrap().len(), 1);
    let secondary = rig.owner.secondary().unwrap();
    let mut stream = secondary.replication_stream().unwrap();
    assert!(stream.next().now_or_never().is_none());
    rig.owner
        .begin_change_role(Role::Primary, epoch(3))
        .unwrap();
    rig.owner.run().await.unwrap();
    assert!(
        primary
            .enqueue(Bytes::new(), BufferPolicy::RawSingle, limits())
            .is_err()
    );
    rig.sent.lock().unwrap().clear();
    configuration(&rig.owner.primary().unwrap(), 2, &[(10, 0)]);
    assert!(
        rig.sent
            .lock()
            .unwrap()
            .iter()
            .any(|intent| match &intent.kind {
                SendKind::Replication {
                    operation,
                    original_epoch,
                    sending_epoch,
                    ..
                } =>
                    Arc::ptr_eq(operation, &original)
                        && *original_epoch == epoch(1)
                        && *sending_epoch == epoch(3),
                _ => false,
            })
    );
}

#[tokio::test]
async fn demotion_rejects_uncommitted_suffix_and_cancels_remaining_write_without_fake_ack() {
    let mut rig = Rig::new(true, false);
    let primary = rig.primary().await;
    configuration(&primary, 2, &[(10, 0)]);
    let write = enqueue(&primary);
    rig.owner
        .begin_change_role(Role::ActiveSecondary, epoch(2))
        .unwrap();
    assert!(rig.owner.run().await.is_err());
    assert_eq!(
        write.completion.await.unwrap(),
        Err(CompletionError::OperationCanceled)
    );
    assert_eq!(rig.owner.status(), RoleStatus::Faulted);
    assert!(rig.owner.secondary().is_err());
}

#[tokio::test]
async fn secondary_external_epoch_drains_ack_without_calling_provider_or_closing_stream() {
    let mut rig = Rig::new(true, false);
    let secondary = rig.secondary();
    rig.start(&secondary);
    rig.finish_copy(&secondary).await;
    let mut stream = secondary.replication_stream().unwrap();
    secondary
        .receive_replication(epoch(1), vec![operation(1, epoch(1), true)], None)
        .unwrap();
    let held = take(&mut stream).await;
    rig.owner.begin_update_epoch(epoch(2)).unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    ack_replication(&secondary, &held);
    rig.owner.run().await.unwrap();
    assert!(rig.provider.epochs.lock().unwrap().is_empty());
    assert!(stream.next().now_or_never().is_none());
    assert!(
        secondary
            .receive_replication(epoch(1), vec![operation(2, epoch(1), true)], None)
            .is_err()
    );
}

#[tokio::test]
async fn duplicate_secondary_epoch_is_immediate_and_stale_epoch_is_invalid() {
    let mut rig = Rig::new(true, false);
    let secondary = rig.secondary();
    rig.start(&secondary);
    rig.finish_copy(&secondary).await;
    rig.owner.begin_update_epoch(epoch(1)).unwrap();
    assert!(matches!(rig.owner.run().await, Err(RoleError::NoOperation)));
    assert!(rig.owner.begin_update_epoch(epoch(0)).is_err());
    assert_eq!(rig.owner.status(), RoleStatus::Ready(Role::IdleSecondary));
    assert!(rig.provider.epochs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn newer_secondary_external_epoch_during_copy_faults_without_provider_call() {
    let mut rig = Rig::new(true, false);
    let secondary = rig.secondary();
    rig.start(&secondary);
    assert!(matches!(
        rig.owner.begin_update_epoch(epoch(2)),
        Err(RoleError::Secondary(SecondaryError::IncompleteCopy))
    ));
    assert_eq!(rig.owner.status(), RoleStatus::Faulted);
    assert!(rig.provider.epochs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn idle_to_active_preserves_actual_secondary_and_rejects_reverse_edge() {
    let mut rig = Rig::new(true, false);
    let secondary = rig.secondary();
    rig.start(&secondary);
    rig.owner
        .begin_change_role(Role::ActiveSecondary, epoch(2))
        .unwrap();
    rig.owner.run().await.unwrap();
    assert_eq!(rig.owner.status(), RoleStatus::Ready(Role::ActiveSecondary));
    assert!(
        secondary
            .receive_copy(
                ReceivedOperation::copy_end_of_stream(1, true).unwrap(),
                true
            )
            .is_ok()
    );
    assert!(
        rig.owner
            .begin_change_role(Role::IdleSecondary, epoch(3))
            .is_err()
    );
}

#[tokio::test]
async fn role_none_is_terminal_and_no_operation_does_not_fault() {
    let mut rig = Rig::new(true, false);
    assert!(matches!(rig.owner.run().await, Err(RoleError::NoOperation)));
    assert_eq!(rig.owner.status(), RoleStatus::Opened);
    rig.owner.begin_change_role(Role::None, epoch(1)).unwrap();
    assert_eq!(rig.owner.status(), RoleStatus::Ready(Role::None));
    assert!(
        rig.owner
            .begin_change_role(Role::Primary, epoch(2))
            .is_err()
    );
}

#[tokio::test]
async fn same_role_primary_change_is_rejected_in_favor_of_external_epoch() {
    let mut rig = Rig::new(true, false);
    rig.primary().await;
    assert!(
        rig.owner
            .begin_change_role(Role::Primary, epoch(2))
            .is_err()
    );
    assert_eq!(rig.owner.status(), RoleStatus::Ready(Role::Primary));
}

#[tokio::test]
async fn abort_retains_outstanding_provider_epoch_until_its_actual_completion() {
    let provider = Arc::new(Provider {
        epochs_gate: Semaphore::new(0),
        ..Provider::new()
    });
    let mut rig = Rig::with_provider(true, false, provider, Some(0));
    rig.owner
        .begin_change_role(Role::Primary, epoch(1))
        .unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.owner.abort().unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    assert_eq!(rig.provider.epochs.lock().unwrap().len(), 1);
    rig.provider.epochs_gate.add_permits(1);
    assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
    assert_eq!(rig.owner.status(), RoleStatus::Aborted);
    assert!(
        rig.owner
            .begin_change_role(Role::Primary, epoch(2))
            .is_err()
    );
}

#[tokio::test]
async fn abort_interrupts_promotion_drain_without_creating_a_primary() {
    let mut rig = Rig::new(true, false);
    let secondary = rig.secondary();
    rig.start(&secondary);
    rig.finish_copy(&secondary).await;
    let mut stream = secondary.replication_stream().unwrap();
    secondary
        .receive_replication(epoch(1), vec![operation(1, epoch(1), true)], None)
        .unwrap();
    let _held = take(&mut stream).await;
    rig.owner
        .begin_change_role(Role::Primary, epoch(2))
        .unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.owner.abort().unwrap();
    assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
    assert!(rig.provider.epochs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn demoted_receiver_enforces_service_ack_for_new_data_not_only_inherited_history() {
    let mut rig = Rig::new(true, false);
    let primary = rig.primary().await;
    configuration(&primary, 1, &[]);
    rig.owner
        .begin_change_role(Role::ActiveSecondary, epoch(2))
        .unwrap();
    rig.owner.run().await.unwrap();
    let secondary = rig.owner.secondary().unwrap();
    let mut stream = secondary.replication_stream().unwrap();
    secondary
        .receive_replication(epoch(2), vec![operation(1, epoch(2), true)], None)
        .unwrap();
    let data = take(&mut stream).await;
    rig.owner
        .begin_change_role(Role::Primary, epoch(3))
        .unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    ack_replication(&secondary, &data);
    rig.owner.run().await.unwrap();
}

#[tokio::test]
async fn role_handoff_retains_pending_actual_build_provider_for_explicit_later_drain() {
    let provider = Arc::new(Provider {
        copy_gate: Semaphore::new(0),
        ..Provider::new()
    });
    let mut rig = Rig::with_provider(false, false, provider, Some(0));
    let primary = rig.primary().await;
    configuration(&primary, 1, &[]);
    primary
        .begin_build(
            BuildConfig {
                route: route(10),
                remote_incarnation: 10,
                copy: CopyConfig {
                    sender: sender(),
                    buffer_policy: BufferPolicy::RawSingle,
                    buffer_limits: limits(),
                },
                context: None,
            },
            rig.provider.clone(),
            BuildCallbacks {
                fence: None,
                replication: Arc::new(|_| true),
                copy: Arc::new(|_| true),
                context_ack: Arc::new(|_, _| true),
            },
        )
        .unwrap();
    primary
        .receive_ack(
            &route(10)
                .reversed()
                .envelope(wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
                    replication_received: 0,
                    replication_applied: 0,
                    copy_received: 0,
                    copy_applied: 0,
                    error: 0,
                })),
        )
        .unwrap();
    assert!(rig.owner.drive().now_or_never().is_none());
    assert_eq!(rig.provider.copy_calls.load(Ordering::SeqCst), 1);
    rig.owner
        .begin_change_role(Role::ActiveSecondary, epoch(2))
        .unwrap();
    rig.owner.run().await.unwrap();
    assert!(rig.owner.drain_retired_builds().now_or_never().is_none());
    assert_eq!(rig.provider.copy_calls.load(Ordering::SeqCst), 1);
    rig.provider.copy_gate.add_permits(1);
    rig.owner.drain_retired_builds().await.unwrap();
}

#[tokio::test]
async fn role_none_closes_actual_primary_and_retains_its_pending_write_outcome() {
    let mut rig = Rig::new(true, false);
    let primary = rig.primary().await;
    configuration(&primary, 2, &[(10, 0)]);
    let write = enqueue(&primary);
    rig.owner.begin_change_role(Role::None, epoch(2)).unwrap();
    rig.owner.run().await.unwrap();
    assert_eq!(rig.owner.status(), RoleStatus::Ready(Role::None));
    assert_eq!(
        write.completion.await.unwrap(),
        Err(CompletionError::OperationCanceled)
    );
    assert!(
        rig.owner
            .begin_change_role(Role::Primary, epoch(3))
            .is_err()
    );
}

#[tokio::test]
async fn aborted_provider_failure_is_retained_without_successful_role_publication() {
    let provider = Arc::new(Provider {
        epochs_gate: Semaphore::new(0),
        ..Provider::new()
    });
    let mut rig = Rig::with_provider(true, false, provider, Some(0));
    rig.owner
        .begin_change_role(Role::Primary, epoch(1))
        .unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.owner.abort().unwrap();
    rig.provider.fail_epoch.store(true, Ordering::SeqCst);
    rig.provider.epochs_gate.add_permits(1);
    assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
    assert!(matches!(rig.owner.fault(), Some(RoleError::Provider(_))));
}

#[tokio::test]
async fn optimistic_idle_to_active_drains_copy_dispatch_without_closing_it() {
    let mut rig = Rig::new(false, false);
    let secondary = rig.secondary();
    rig.start(&secondary);
    let mut copy = secondary.copy_stream().unwrap();
    secondary
        .receive_copy(operation(1, epoch(1), false), false)
        .unwrap();
    rig.owner
        .begin_change_role(Role::ActiveSecondary, epoch(2))
        .unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    let held = take(&mut copy).await;
    rig.owner.run().await.unwrap();
    assert_eq!(
        held.ack_state(),
        super::super::received_operation::AckState::NotCalled
    );
    assert!(copy.next().now_or_never().is_none());
}

#[tokio::test]
async fn invalid_destination_settings_are_rejected_before_any_role_or_provider_side_effect() {
    let mut rig = Rig::new(true, false);
    rig.owner.config.secondary.context.sender.initial_window = 3;
    let owner = RoleReplicator::new(rig.owner.config, rig.provider.clone(), Clock::new());
    assert!(matches!(owner, Err(RoleError::InvalidConfig)));
    assert!(rig.provider.epochs.lock().unwrap().is_empty());
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn pending_primary_epoch_keeps_driving_real_retry_timers_with_distinct_original_epoch() {
    let mut rig = Rig::new(true, false);
    let primary = rig.primary().await;
    configuration(&primary, 2, &[(10, 0)]);
    enqueue(&primary);
    let permits = rig.provider.epochs_gate.available_permits();
    rig.provider.epochs_gate.forget_permits(permits);
    rig.owner.begin_update_epoch(epoch(2)).unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.sent.lock().unwrap().clear();
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(rig.owner.run().now_or_never().is_none());
    assert!(
        rig.sent
            .lock()
            .unwrap()
            .iter()
            .any(|intent| matches!(intent.kind,
        SendKind::Replication { original_epoch, sending_epoch, .. }
            if original_epoch == epoch(1) && sending_epoch == epoch(2)))
    );
    assert_eq!(rig.provider.epochs.lock().unwrap().len(), 2);
    rig.provider.epochs_gate.add_permits(1);
    rig.owner.run().await.unwrap();
}

#[tokio::test]
async fn overflowing_provider_initial_progress_faults_without_invoking_epoch_or_panicking() {
    let provider = Arc::new(Provider {
        progress: Lsn::MAX,
        ..Provider::new()
    });
    let mut rig = Rig::with_provider(true, false, provider, None);
    rig.owner
        .begin_change_role(Role::Primary, epoch(1))
        .unwrap();
    assert!(rig.owner.run().await.is_err());
    assert_eq!(rig.owner.status(), RoleStatus::Faulted);
    assert!(rig.provider.epochs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn primary_reported_build_fault_prevents_demotion_instead_of_creating_healthy_secondary() {
    let mut rig = Rig::new(false, false);
    let primary = rig.primary().await;
    configuration(&primary, 1, &[]);
    rig.provider.fail_copy.store(true, Ordering::SeqCst);
    primary
        .begin_build(
            BuildConfig {
                route: route(10),
                remote_incarnation: 10,
                copy: CopyConfig {
                    sender: sender(),
                    buffer_policy: BufferPolicy::RawSingle,
                    buffer_limits: limits(),
                },
                context: None,
            },
            rig.provider.clone(),
            BuildCallbacks {
                fence: None,
                replication: Arc::new(|_| true),
                copy: Arc::new(|_| true),
                context_ack: Arc::new(|_, _| true),
            },
        )
        .unwrap();
    primary
        .receive_ack(
            &route(10)
                .reversed()
                .envelope(wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
                    replication_received: 0,
                    replication_applied: 0,
                    copy_received: 0,
                    copy_applied: 0,
                    error: 0,
                })),
        )
        .unwrap();
    assert!(rig.owner.drive().now_or_never().is_none());
    assert_eq!(rig.provider.copy_calls.load(Ordering::SeqCst), 1);
    assert!(rig.owner.begin_data_loss().is_err());
    assert_eq!(rig.owner.status(), RoleStatus::Ready(Role::Primary));
    assert_eq!(rig.provider.data_loss_calls.load(Ordering::SeqCst), 0);
    assert!(
        rig.owner
            .begin_change_role(Role::ActiveSecondary, epoch(2))
            .is_err()
    );
    assert_eq!(rig.owner.status(), RoleStatus::Faulted);
    assert!(rig.owner.secondary().is_err());
}
