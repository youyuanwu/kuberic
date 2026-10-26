// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-component vectors derived from the pinned BuildIdle/ReplicaManager
// production sources cited in build_idle.rs; not native test execution.

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{FutureExt, StreamExt, stream};
use tokio::sync::{Semaphore, oneshot::error::TryRecvError};

use crate::application::OperationDataStream;
use crate::protocol::types::Epoch;
use crate::replicator::v1::buffers::{BufferLimits, BufferPolicy};
use crate::replicator::v1::operation_queue::QueueConfig;
use crate::replicator::v1::replica_manager::{
    CompletionError, CompletionReceiver, ConfigurationError, EstablishedConfiguration, ReplicaSet,
};
use crate::replicator::v1::sender::SenderConfig;
use crate::replicator::v1::session::SendKind;

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

fn copy_config() -> CopyConfig {
    CopyConfig {
        sender: SenderConfig {
            initial_window: 4,
            max_window: 8,
            retry: Duration::from_secs(1),
            decay_factor: 0.5,
            decay_interval: Duration::from_secs(1),
        },
        buffer_policy: BufferPolicy::RawSingle,
        buffer_limits: BufferLimits {
            max_segments: 8,
            max_payload_bytes: 1024,
        },
    }
}

fn config(id: i64, persisted: bool) -> BuildConfig {
    BuildConfig {
        route: route(id),
        remote_incarnation: 7,
        copy: copy_config(),
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

fn manager(initial: Lsn, quorum: usize) -> Arc<Mutex<ReplicaManager>> {
    let mut manager = ReplicaManager::new(
        QueueConfig {
            initial_capacity: 4,
            max_capacity: 64,
            max_bytes: 0,
            max_completed_count: 0,
            max_completed_bytes: 0,
            clean_on_complete: true,
        },
        initial,
        10,
        "primary".into(),
        Epoch::new(1, 7),
        true,
    )
    .unwrap();
    manager
        .configure_established(EstablishedConfiguration {
            current: ReplicaSet {
                write_quorum: quorum,
                members: Vec::new(),
            },
            previous: None,
            catchup_configuration: false,
        })
        .unwrap()
        .dispatch(Duration::ZERO)
        .unwrap();
    Arc::new(Mutex::new(manager))
}

fn enqueue(manager: &Arc<Mutex<ReplicaManager>>) -> (Lsn, CompletionReceiver) {
    let admission = manager
        .lock()
        .unwrap()
        .enqueue(
            Bytes::from_static(b"replication"),
            BufferPolicy::RawSingle,
            copy_config().buffer_limits,
        )
        .unwrap();
    let lsn = admission.operation.operation.lsn();
    admission.effects.dispatch(Duration::ZERO).unwrap();
    (lsn, admission.completion)
}

fn remove(manager: &Arc<Mutex<ReplicaManager>>, id: i64) -> Result<(), ManagerError> {
    let effects = manager.lock().unwrap().remove_idle(id)?;
    effects.dispatch(Duration::ZERO)
}

fn close(manager: &Arc<Mutex<ReplicaManager>>) -> Result<(), ManagerError> {
    let effects = manager.lock().unwrap().close();
    effects.dispatch(Duration::ZERO)
}

fn ack(id: i64, received: Lsn, applied: Lsn, copy: Lsn, error: i32) -> wire::Envelope {
    route(id)
        .reversed()
        .envelope(wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
            replication_received: received,
            replication_applied: applied,
            copy_received: copy,
            copy_applied: copy,
            error,
        }))
}

fn context(id: i64, lsn: Lsn, last: bool, data: &'static [u8]) -> wire::Envelope {
    route(id)
        .reversed()
        .envelope(wire::envelope::Body::CopyContextOperation(
            wire::CopyContextOperation {
                metadata: Some(wire::Metadata {
                    sequence: lsn,
                    operation_type: 1,
                    atomic_group: -1,
                }),
                buffers: Some(wire::Buffers {
                    segments: vec![Bytes::from_static(data)],
                }),
                is_last: last,
            },
        ))
}

struct Provider {
    calls: AtomicUsize,
    boundary: Mutex<Option<Lsn>>,
    contexts: Mutex<Vec<Bytes>>,
    gate: Semaphore,
    acquisition_error: bool,
    enumeration_error: bool,
    hook: Option<Arc<dyn Fn() + Send + Sync>>,
    poll_gate: Option<Arc<Semaphore>>,
}

impl Provider {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            boundary: Mutex::new(None),
            contexts: Mutex::new(Vec::new()),
            gate: Semaphore::new(1),
            acquisition_error: false,
            enumeration_error: false,
            hook: None,
            poll_gate: None,
        }
    }
}

#[async_trait]
impl StateProvider for Provider {
    async fn update_epoch(&self, _: Epoch, _: Lsn) -> crate::Result<()> {
        panic!("BuildIdle must not invent epoch transitions")
    }

    async fn last_committed_lsn(&self) -> crate::Result<Lsn> {
        panic!("BuildIdle must capture the actual primary queue boundary")
    }

    async fn get_copy_context(&self) -> crate::Result<OperationDataStream> {
        panic!("source context must not replace target context")
    }

    async fn get_copy_state(
        &self,
        boundary: Lsn,
        mut context: OperationDataStream,
    ) -> crate::Result<OperationDataStream> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.boundary.lock().unwrap() = Some(boundary);
        if let Some(hook) = &self.hook {
            hook();
        }
        self.gate.acquire().await.unwrap().forget();
        if self.acquisition_error {
            return Err(RuntimeError::Application("GetCopyState failed".into()));
        }
        while let Some(item) = context.next().await {
            self.contexts.lock().unwrap().push(item?);
        }
        let poll_gate = self.poll_gate.clone();
        let error = self.enumeration_error;
        let first = stream::once(async move {
            if let Some(gate) = poll_gate {
                gate.acquire().await.unwrap().forget();
            }
            if error {
                Err(RuntimeError::Application("EndGetNext failed".into()))
            } else {
                Ok(Bytes::from_static(b"snapshot"))
            }
        });
        Ok(Box::pin(first))
    }

    async fn on_data_loss(&self) -> crate::Result<bool> {
        panic!("BuildIdle must not perform data-loss recovery")
    }
}

#[derive(Default)]
struct Sent {
    replication: Vec<SendIntent>,
    copy: Vec<(Lsn, bool)>,
    context_acks: Vec<CopyContextAck>,
}

fn callbacks(sent: &Arc<Mutex<Sent>>) -> BuildCallbacks {
    let replication = sent.clone();
    let copy = sent.clone();
    let context = sent.clone();
    BuildCallbacks {
        fence: None,
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
        context_ack: Arc::new(move |ack, _| {
            context.lock().unwrap().context_acks.push(ack);
            true
        }),
    }
}

struct Rig {
    manager: Arc<Mutex<ReplicaManager>>,
    provider: Arc<Provider>,
    sent: Arc<Mutex<Sent>>,
    build: BuildIdle,
    handle: BuildIdleHandle,
}

impl Rig {
    fn new(persisted: bool) -> Self {
        Self::with(manager(0, 2), persisted, Provider::new())
    }

    fn with(manager: Arc<Mutex<ReplicaManager>>, persisted: bool, provider: Provider) -> Self {
        let provider = Arc::new(provider);
        let sent = Arc::new(Mutex::new(Sent::default()));
        let build = BuildIdle::begin(
            &manager,
            config(20, persisted),
            provider.clone(),
            callbacks(&sent),
            Clock::new(),
        )
        .unwrap();
        let handle = build.handle();
        Self {
            manager,
            provider,
            sent,
            build,
            handle,
        }
    }

    fn establish(&self, persisted: bool) {
        if persisted {
            self.handle
                .receive_context(&context(20, 1, true, b""))
                .unwrap();
        } else {
            self.handle.receive_ack(&ack(20, 0, 0, 0, 0)).unwrap();
        }
    }

    fn pending(&mut self) {
        assert!(self.build.run().now_or_never().is_none());
        assert!(self.handle.result().is_none());
    }
}

#[tokio::test]
async fn admission_freezes_provider_boundary_and_replays_actual_pending_operations() {
    let manager = manager(7, 2);
    for expected in 8..=9 {
        assert_eq!(enqueue(&manager).0, expected);
    }
    let mut rig = Rig::with(manager, false, Provider::new());
    {
        let sent = rig.sent.lock().unwrap();
        let replay: Vec<_> = sent
            .replication
            .iter()
            .filter_map(|intent| match &intent.kind {
                SendKind::Replication {
                    operation,
                    original_epoch,
                    ..
                } => Some((operation.lsn(), *original_epoch)),
                _ => None,
            })
            .collect();
        assert_eq!(replay, [(8, Epoch::new(1, 7)), (9, Epoch::new(1, 7))]);
        assert!(matches!(
            sent.replication.last().unwrap().kind,
            SendKind::StartCopy {
                replication_start: 8,
                ..
            }
        ));
    }
    rig.pending();
    assert_eq!(rig.provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(enqueue(&rig.manager).0, 10);
    rig.establish(false);
    rig.pending();
    assert_eq!(*rig.provider.boundary.lock().unwrap(), Some(7));
    assert_eq!(rig.handle.finish_copy_target().unwrap(), Some(10));
    assert_eq!(rig.sent.lock().unwrap().copy, [(1, false), (2, true)]);
    rig.handle.receive_ack(&ack(20, 10, 9, 2, 0)).unwrap();
    rig.pending();
    rig.handle.receive_ack(&ack(20, 10, 10, 2, 0)).unwrap();
    let end = rig.build.run().await.unwrap();
    assert!(end.result.is_ok());
    assert!(end.fault.is_none());
    assert!(rig.handle.0.session.start_copy_deadline().is_none());
    assert_eq!(enqueue(&rig.manager).0, 11);
    assert!(rig.handle.0.session.retry_deadline().is_some());
}

#[tokio::test]
async fn persisted_context_reaches_provider_and_success_keeps_idle_membership() {
    let mut rig = Rig::new(true);
    rig.handle
        .receive_context(&context(20, 1, false, b"target"))
        .unwrap();
    rig.pending();
    assert_eq!(rig.provider.calls.load(Ordering::SeqCst), 1);
    rig.handle
        .receive_context(&context(20, 2, true, b""))
        .unwrap();
    rig.pending();
    assert_eq!(
        *rig.provider.contexts.lock().unwrap(),
        [Bytes::from_static(b"target")]
    );
    rig.handle.receive_ack(&ack(20, 0, 0, 2, 0)).unwrap();
    assert!(rig.build.run().await.unwrap().result.is_ok());
    assert!(rig.build.run().await.unwrap().result.is_ok());
    assert_eq!(rig.provider.calls.load(Ordering::SeqCst), 1);
    assert!(rig.manager.lock().unwrap().build_handle(20).is_some());
    assert_eq!(enqueue(&rig.manager).0, 1);
    assert!(rig.sent.lock().unwrap().replication.iter().any(|intent| {
        matches!(&intent.kind, SendKind::Replication { operation, .. } if operation.lsn() == 1)
    }));
    rig.handle.cancel().unwrap();
    assert!(rig.handle.result().unwrap().result.is_ok());
    remove(&rig.manager, 20).unwrap();
    assert!(rig.manager.lock().unwrap().build_handle(20).is_none());
}

#[tokio::test]
async fn registry_and_start_sender_exist_before_reentrant_pending_replay_removal() {
    let manager = manager(0, 2);
    enqueue(&manager);
    let weak = Arc::downgrade(&manager);
    let mut hooks = callbacks(&Arc::new(Mutex::new(Sent::default())));
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    hooks.replication = Arc::new(move |intent| {
        if called.fetch_add(1, Ordering::SeqCst) == 0 {
            assert!(matches!(intent.kind, SendKind::Replication { .. }));
            let manager = weak.upgrade().unwrap();
            let handle = manager.lock().unwrap().build_handle(20).unwrap();
            assert!(handle.0.session.start_copy_deadline().is_some());
            remove(&manager, 20).unwrap();
        } else {
            assert!(matches!(intent.kind, SendKind::StartCopy { .. }));
        }
        true
    });
    let mut build = BuildIdle::begin(
        &manager,
        config(20, false),
        Arc::new(Provider::new()),
        hooks,
        Clock::new(),
    )
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(matches!(
        build.run().await.unwrap().result,
        Err(BuildError::Establishment(EstablishError::Cancelled))
    ));
    assert!(build.handle().0.session.retry_deadline().is_none());
    assert!(build.handle().0.session.start_copy_deadline().is_none());
}

#[tokio::test]
async fn reentrant_start_response_can_establish_without_manager_guard_deadlock() {
    let manager = manager(0, 2);
    let weak = Arc::downgrade(&manager);
    let mut hooks = callbacks(&Arc::new(Mutex::new(Sent::default())));
    hooks.replication = Arc::new(move |intent| {
        if matches!(intent.kind, SendKind::StartCopy { .. }) {
            let manager = weak.upgrade().unwrap();
            let handle = manager.lock().unwrap().build_handle(20).unwrap();
            handle.receive_ack(&ack(20, 0, 0, 0, 0)).unwrap();
        }
        true
    });
    let provider = Arc::new(Provider::new());
    let mut build = BuildIdle::begin(
        &manager,
        config(20, false),
        provider.clone(),
        hooks,
        Clock::new(),
    )
    .unwrap();
    assert!(build.run().now_or_never().is_none());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn duplicate_admission_does_not_replace_existing_session_or_send() {
    let rig = Rig::new(false);
    let sent = Arc::new(Mutex::new(Sent::default()));
    let attempt = BuildIdle::begin(
        &rig.manager,
        config(20, false),
        Arc::new(Provider::new()),
        callbacks(&sent),
        Clock::new(),
    );
    assert!(
        matches!(attempt, Err(BuildError::Manager(error)) if matches!(*error, ManagerError::Configuration(ConfigurationError::IdleAlreadyExists(20))))
    );
    assert!(Arc::ptr_eq(
        &rig.handle.0,
        &rig.manager.lock().unwrap().build_handle(20).unwrap().0
    ));
    assert!(sent.lock().unwrap().replication.is_empty());
}

#[tokio::test]
async fn invalid_admission_is_atomic_before_membership_and_callbacks() {
    let manager = manager(0, 2);
    let valid_context = config(20, true).context.unwrap();
    for variant in 0..8 {
        let mut config = config(20, false);
        match variant {
            0 => config.route.destination.replica = 10,
            1 => config.route.source.replica = 999,
            2 => config.route.sender_address = "wrong-primary".into(),
            3 => config.route.destination.partition = Bytes::from(vec![8; 16]),
            4 => config.remote_incarnation = -1,
            5 => config.route.remote_address = "primary".into(),
            6 => config.copy.sender.initial_window = 0,
            _ => {
                config.context = Some(CopyContextConfig {
                    initial_queue_size: 0,
                    ..valid_context
                })
            }
        }
        let sent = Arc::new(Mutex::new(Sent::default()));
        assert!(
            BuildIdle::begin(
                &manager,
                config,
                Arc::new(Provider::new()),
                callbacks(&sent),
                Clock::new()
            )
            .is_err(),
            "variant {variant}"
        );
        assert!(manager.lock().unwrap().build_handle(20).is_none());
        assert!(sent.lock().unwrap().replication.is_empty());
    }
}

#[tokio::test]
async fn closed_manager_rejects_admission_without_sends() {
    let manager = manager(0, 2);
    close(&manager).unwrap();
    let sent = Arc::new(Mutex::new(Sent::default()));
    assert!(
        matches!(BuildIdle::begin(&manager, config(20, false), Arc::new(Provider::new()), callbacks(&sent), Clock::new()),
        Err(BuildError::Manager(error)) if matches!(*error, ManagerError::ObjectClosed))
    );
    assert!(sent.lock().unwrap().replication.is_empty());
}

#[tokio::test]
async fn acquisition_failure_records_get_copy_state_fault_and_blocks_later_builds() {
    let mut provider = Provider::new();
    provider.acquisition_error = true;
    let mut rig = Rig::with(manager(0, 2), false, provider);
    rig.establish(false);
    let end = rig.build.run().await.unwrap();
    assert!(matches!(
        end.result,
        Err(BuildError::Establishment(EstablishError::Provider {
            description: "GetCopyState",
            ..
        }))
    ));
    assert_eq!(end.fault.unwrap().description, "GetCopyState");
    assert_eq!(
        rig.manager
            .lock()
            .unwrap()
            .build_fault()
            .unwrap()
            .description,
        "GetCopyState"
    );
    assert!(rig.handle.0.session.start_copy_deadline().is_none());
    assert!(
        matches!(BuildIdle::begin(&rig.manager, config(30, false), Arc::new(Provider::new()), callbacks(&rig.sent), Clock::new()),
        Err(BuildError::Manager(error)) if matches!(*error, ManagerError::ReportedFault { .. }))
    );
}

#[tokio::test]
async fn enumeration_failure_records_end_get_next_not_acquisition_fault() {
    let mut provider = Provider::new();
    provider.enumeration_error = true;
    let mut rig = Rig::with(manager(0, 2), false, provider);
    rig.establish(false);
    let end = rig.build.run().await.unwrap();
    assert!(matches!(
        end.result,
        Err(BuildError::Copy(CopyError::Provider(_)))
    ));
    assert_eq!(end.fault.unwrap().description, "EndGetNext");
    assert_eq!(
        rig.manager
            .lock()
            .unwrap()
            .build_fault()
            .unwrap()
            .description,
        "EndGetNext"
    );
}

#[tokio::test]
async fn remote_establishment_failure_is_not_reported_as_provider_fault() {
    let mut rig = Rig::new(true);
    rig.handle.receive_ack(&ack(20, 0, 0, 0, -7)).unwrap();
    let end = rig.build.run().await.unwrap();
    assert!(matches!(
        end.result,
        Err(BuildError::Establishment(EstablishError::Remote(-7)))
    ));
    assert!(end.fault.is_none());
    assert!(rig.manager.lock().unwrap().build_fault().is_none());
    assert_eq!(rig.provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelled_build_waits_for_establishment_then_skips_provider() {
    let mut rig = Rig::new(false);
    rig.handle.cancel().unwrap();
    rig.pending();
    assert!(rig.handle.0.session.start_copy_deadline().is_some());
    rig.establish(false);
    let end = rig.build.run().await.unwrap();
    assert!(matches!(
        end.result,
        Err(BuildError::Establishment(EstablishError::Cancelled))
    ));
    assert_eq!(rig.provider.calls.load(Ordering::SeqCst), 0);
    assert!(rig.manager.lock().unwrap().build_handle(20).is_some());
}

#[tokio::test]
async fn removal_before_establishment_finishes_without_provider_acquisition() {
    let mut rig = Rig::new(true);
    remove(&rig.manager, 20).unwrap();
    assert!(matches!(
        rig.build.run().await.unwrap().result,
        Err(BuildError::Establishment(EstablishError::Cancelled))
    ));
    assert_eq!(rig.provider.calls.load(Ordering::SeqCst), 0);
    assert!(rig.handle.0.session.start_copy_deadline().is_none());
    assert!(matches!(
        rig.handle.receive_context(&context(20, 1, true, b"")),
        Err(BuildError::Establishment(EstablishError::Cancelled))
    ));
}

#[tokio::test]
async fn interrupted_acquisition_is_retained_and_provider_is_called_once() {
    let provider = Provider::new();
    provider.gate.acquire().await.unwrap().forget();
    let mut rig = Rig::with(manager(0, 2), false, provider);
    rig.establish(false);
    rig.pending();
    rig.pending();
    assert_eq!(rig.provider.calls.load(Ordering::SeqCst), 1);
    rig.provider.gate.add_permits(1);
    rig.pending();
    rig.handle.receive_ack(&ack(20, 0, 0, 2, 0)).unwrap();
    assert!(rig.build.run().await.unwrap().result.is_ok());
}

#[tokio::test]
async fn removal_does_not_pretend_pending_acquisition_drained() {
    let provider = Provider::new();
    provider.gate.acquire().await.unwrap().forget();
    let mut rig = Rig::with(manager(0, 2), false, provider);
    rig.establish(false);
    rig.pending();
    remove(&rig.manager, 20).unwrap();
    rig.pending();
    rig.provider.gate.add_permits(1);
    assert!(matches!(
        rig.build.run().await.unwrap().result,
        Err(BuildError::Establishment(EstablishError::Cancelled))
    ));
    assert_eq!(rig.provider.calls.load(Ordering::SeqCst), 1);
    assert!(rig.sent.lock().unwrap().copy.is_empty());
}

#[tokio::test]
async fn pending_provider_failure_keeps_precedence_and_fault_after_removal() {
    let mut provider = Provider::new();
    provider.acquisition_error = true;
    provider.gate.acquire().await.unwrap().forget();
    let mut rig = Rig::with(manager(0, 2), false, provider);
    rig.establish(false);
    rig.pending();
    remove(&rig.manager, 20).unwrap();
    rig.provider.gate.add_permits(1);
    let end = rig.build.run().await.unwrap();
    assert!(matches!(
        end.result,
        Err(BuildError::Establishment(EstablishError::Provider { .. }))
    ));
    assert_eq!(end.fault.unwrap().description, "GetCopyState");
}

#[tokio::test]
async fn build_cancel_breaks_provider_context_consumption_without_session_removal() {
    let mut rig = Rig::new(true);
    rig.handle
        .receive_context(&context(20, 1, false, b"target"))
        .unwrap();
    rig.pending();
    let (lsn, mut completion) = enqueue(&rig.manager);
    rig.handle.cancel().unwrap();
    let end = rig.build.run().await.unwrap();
    assert!(matches!(
        end.result,
        Err(BuildError::Establishment(EstablishError::Provider { .. }))
    ));
    assert_eq!(end.fault.unwrap().description, "GetCopyState");
    {
        let mut manager = rig.manager.lock().unwrap();
        assert!(manager.build_handle(20).is_some());
        assert!(matches!(
            manager.enqueue(
                Bytes::from_static(b"replication"),
                BufferPolicy::RawSingle,
                copy_config().buffer_limits,
            ),
            Err(ManagerError::ReportedFault {
                description: "GetCopyState",
                ..
            })
        ));
        assert_eq!(manager.queue().last_sequence_number(), lsn);
    }
    assert!(matches!(completion.try_recv(), Err(TryRecvError::Empty)));
    assert!(rig.handle.0.session.retry_deadline().is_some());
}

#[tokio::test]
async fn build_cancel_after_acquisition_does_not_replace_session_cancel_copy() {
    let poll_gate = Arc::new(Semaphore::new(0));
    let mut provider = Provider::new();
    provider.poll_gate = Some(poll_gate.clone());
    let mut rig = Rig::with(manager(0, 2), false, provider);
    rig.establish(false);
    rig.pending();
    rig.handle.cancel().unwrap();
    rig.pending();
    poll_gate.add_permits(1);
    rig.pending();
    rig.handle.receive_ack(&ack(20, 0, 0, 2, 0)).unwrap();
    assert!(rig.build.run().await.unwrap().result.is_ok());
}

#[tokio::test]
async fn removal_during_inflight_enumeration_finishes_copy_without_claiming_drain() {
    let mut provider = Provider::new();
    provider.poll_gate = Some(Arc::new(Semaphore::new(0)));
    let mut rig = Rig::with(manager(0, 2), false, provider);
    rig.establish(false);
    rig.pending();
    assert!(
        rig.handle
            .0
            .copy
            .snapshot()
            .unwrap()
            .enumeration
            .unwrap()
            .provider_in_flight
    );
    remove(&rig.manager, 20).unwrap();
    let end = rig.build.run().await.unwrap();
    assert!(matches!(
        end.result,
        Err(BuildError::Copy(CopyError::Cancelled))
    ));
    assert!(end.fault.is_none());
    assert!(
        rig.handle
            .0
            .copy
            .snapshot()
            .unwrap()
            .enumeration
            .unwrap()
            .provider_in_flight
    );
}

#[tokio::test]
async fn removal_releases_actual_idle_retention_without_success_from_membership_change() {
    let mut rig = Rig::with(manager(0, 1), false, Provider::new());
    rig.establish(false);
    rig.pending();
    assert_eq!(rig.handle.finish_copy_target().unwrap(), Some(0));
    for expected in 1..=3 {
        let (lsn, mut completion) = enqueue(&rig.manager);
        assert_eq!(lsn, expected);
        assert_eq!(completion.try_recv(), Ok(Ok(())));
    }
    rig.handle.receive_ack(&ack(20, 3, 0, 0, 0)).unwrap();
    assert_eq!(rig.manager.lock().unwrap().queue().operation_count(), 3);
    remove(&rig.manager, 20).unwrap();
    assert_eq!(rig.manager.lock().unwrap().queue().operation_count(), 0);
    assert!(matches!(
        rig.build.run().await.unwrap().result,
        Err(BuildError::Copy(CopyError::Cancelled))
    ));
}

#[tokio::test]
async fn stale_removed_build_cannot_cancel_or_cleanup_replacement_with_same_id() {
    let mut old = Rig::new(false);
    remove(&old.manager, 20).unwrap();
    let mut new = Rig::with(old.manager.clone(), false, Provider::new());
    old.handle.cancel().unwrap();
    assert!(old.build.run().await.unwrap().result.is_err());
    assert!(Arc::ptr_eq(
        &new.handle.0,
        &new.manager.lock().unwrap().build_handle(20).unwrap().0
    ));
    assert!(new.handle.0.session.start_copy_deadline().is_some());
    assert!(old.handle.receive_ack(&ack(20, 0, 0, 0, 0)).is_err());
    new.establish(false);
    new.pending();
    new.handle.receive_ack(&ack(20, 0, 0, 2, 0)).unwrap();
    assert!(new.build.run().await.unwrap().result.is_ok());
}

#[tokio::test]
async fn manager_close_cancels_waiters_and_all_idle_builds_without_false_quorum_success() {
    let manager = manager(0, 2);
    let (_, mut completion) = enqueue(&manager);
    assert_eq!(completion.try_recv(), Err(TryRecvError::Empty));
    let mut first = Rig::with(manager.clone(), false, Provider::new());
    let mut second = BuildIdle::begin(
        &manager,
        config(30, false),
        Arc::new(Provider::new()),
        callbacks(&first.sent),
        Clock::new(),
    )
    .unwrap();
    close(&manager).unwrap();
    assert_eq!(
        completion.try_recv(),
        Ok(Err(CompletionError::OperationCanceled))
    );
    assert!(first.build.run().await.unwrap().result.is_err());
    assert!(second.run().await.unwrap().result.is_err());
    assert!(first.handle.0.session.retry_deadline().is_none());
    close(&manager).unwrap();
}

#[tokio::test]
async fn reentrant_provider_can_remove_idle_without_locking_or_success_fallback() {
    let manager = manager(0, 2);
    let mut provider = Provider::new();
    let weak = Arc::downgrade(&manager);
    provider.hook = Some(Arc::new(move || {
        remove(&weak.upgrade().unwrap(), 20).unwrap()
    }));
    let mut rig = Rig::with(manager, false, provider);
    rig.establish(false);
    assert!(matches!(
        rig.build.run().await.unwrap().result,
        Err(BuildError::Establishment(EstablishError::Cancelled))
    ));
}

#[tokio::test]
async fn impossible_copy_ack_is_rejected_before_poisoning_session_cache() {
    let mut rig = Rig::new(false);
    rig.establish(false);
    rig.pending();
    assert!(matches!(
        rig.handle.receive_ack(&ack(20, 0, 0, 3, 0)),
        Err(BuildError::Copy(CopyError::InvalidAck))
    ));
    assert!(!rig.handle.0.session.is_processing());
    rig.handle.receive_ack(&ack(20, 0, 0, 2, 0)).unwrap();
    assert!(rig.build.run().await.unwrap().result.is_ok());
}

#[tokio::test]
async fn rejected_identity_and_shape_do_not_mutate_session_or_establish() {
    let mut rig = Rig::new(false);
    for variant in 0..4 {
        let mut envelope = ack(20, 0, 0, 0, 0);
        match variant {
            0 => envelope.source.as_mut().unwrap().incarnation = Bytes::from(vec![8; 16]),
            1 => envelope.sender_address = "imposter".into(),
            2 => envelope.destination.as_mut().unwrap().replica += 1,
            _ => {
                let Some(wire::envelope::Body::ReplicationAck(body)) = envelope.body.as_mut()
                else {
                    unreachable!()
                };
                body.replication_applied = 1;
            }
        }
        assert!(rig.handle.receive_ack(&envelope).is_err());
        assert!(!rig.handle.0.session.is_processing());
    }
    rig.pending();
    assert_eq!(rig.provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn driver_runs_actual_start_copy_and_replication_retry_deadlines() {
    let manager = manager(0, 2);
    enqueue(&manager);
    let mut rig = Rig::with(manager, false, Provider::new());
    rig.pending();
    let before = rig.sent.lock().unwrap().replication.len();
    tokio::time::advance(Duration::from_secs(2)).await;
    rig.pending();
    assert!(rig.sent.lock().unwrap().replication.len() > before);
    assert!(
        rig.sent
            .lock()
            .unwrap()
            .replication
            .iter()
            .filter(|intent| matches!(intent.kind, SendKind::StartCopy { .. }))
            .count()
            >= 2
    );
    rig.establish(false);
    rig.handle.receive_ack(&ack(20, 1, 0, 0, 0)).unwrap();
    rig.pending();
    let starts = rig
        .sent
        .lock()
        .unwrap()
        .replication
        .iter()
        .filter(|intent| matches!(intent.kind, SendKind::StartCopy { .. }))
        .count();
    tokio::time::advance(Duration::from_secs(2)).await;
    rig.pending();
    assert_eq!(
        rig.sent
            .lock()
            .unwrap()
            .replication
            .iter()
            .filter(|intent| matches!(intent.kind, SendKind::StartCopy { .. }))
            .count(),
        starts
    );
}

#[tokio::test]
async fn dropped_manager_at_eof_is_explicit_failure_not_a_zero_target() {
    let mut rig = Rig::new(false);
    rig.establish(false);
    let manager = std::mem::replace(&mut rig.manager, manager(0, 2));
    drop(manager);
    let end = rig.build.run().await.unwrap();
    assert!(matches!(
        end.result,
        Err(BuildError::Copy(CopyError::ReplicationTarget(_)))
    ));
    assert!(end.fault.is_none());
}

#[tokio::test]
async fn abandoned_driver_is_explicit_and_does_not_prevent_other_close_cancellation() {
    let manager = manager(0, 2);
    let abandoned = Rig::with(manager.clone(), false, Provider::new());
    let handle = abandoned.handle.clone();
    drop(abandoned);
    assert!(matches!(handle.cancel(), Err(EstablishError::OwnerDropped)));
    let mut live = BuildIdle::begin(
        &manager,
        config(30, false),
        Arc::new(Provider::new()),
        callbacks(&Arc::new(Mutex::new(Sent::default()))),
        Clock::new(),
    )
    .unwrap();
    assert!(matches!(
        close(&manager),
        Err(ManagerError::BuildLifecycle(error)) if error.contains("owner was dropped")
    ));
    assert!(live.run().await.unwrap().result.is_err());
    assert!(live.handle().0.session.start_copy_deadline().is_none());
}

#[tokio::test]
async fn registry_does_not_strongly_retain_manager_and_completed_metadata_survives_driver() {
    let mut rig = Rig::new(false);
    let weak = Arc::downgrade(&rig.manager);
    rig.establish(false);
    rig.pending();
    rig.handle.receive_ack(&ack(20, 0, 0, 2, 0)).unwrap();
    assert!(rig.build.run().await.unwrap().result.is_ok());
    let handle = rig.handle.clone();
    let manager = rig.manager.clone();
    drop(rig);
    assert_eq!(handle.finish_copy_target().unwrap(), Some(0));
    assert_eq!(enqueue(&manager).0, 1);
    remove(&manager, 20).unwrap();
    assert!(handle.result().unwrap().result.is_ok());
    drop(manager);
    assert!(weak.upgrade().is_none());
}

#[tokio::test]
async fn context_cleanup_callback_can_reenter_manager_removal() {
    let manager = manager(0, 2);
    let weak = Arc::downgrade(&manager);
    let called = Arc::new(AtomicUsize::new(0));
    let count = called.clone();
    let mut hooks = callbacks(&Arc::new(Mutex::new(Sent::default())));
    hooks.context_ack = Arc::new(move |_, _| {
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            remove(&weak.upgrade().unwrap(), 20).unwrap();
        }
        true
    });
    let mut build = BuildIdle::begin(
        &manager,
        config(20, true),
        Arc::new(Provider::new()),
        hooks,
        Clock::new(),
    )
    .unwrap();
    build
        .handle()
        .receive_context(&context(20, 1, true, b""))
        .unwrap();
    assert!(called.load(Ordering::SeqCst) > 0);
    assert!(build.run().await.unwrap().result.is_err());
}

#[tokio::test]
async fn close_during_ack_dispatch_does_not_abandon_remaining_handoffs() {
    let manager = manager(0, 2);
    for _ in 0..6 {
        enqueue(&manager);
    }
    let enabled = Arc::new(AtomicUsize::new(0));
    let called = enabled.clone();
    let weak = Arc::downgrade(&manager);
    let mut hooks = callbacks(&Arc::new(Mutex::new(Sent::default())));
    hooks.replication = Arc::new(move |_| {
        if called.swap(0, Ordering::SeqCst) != 0 {
            close(&weak.upgrade().unwrap()).unwrap();
        }
        true
    });
    let mut build = BuildIdle::begin(
        &manager,
        config(20, false),
        Arc::new(Provider::new()),
        hooks,
        Clock::new(),
    )
    .unwrap();
    let handle = build.handle();
    enabled.store(1, Ordering::SeqCst);
    assert!(matches!(handle.receive_ack(&ack(20, 1, 1, 0, 0)),
        Err(BuildError::Manager(error)) if matches!(*error, ManagerError::ObjectClosed)));
    assert!(!handle.0.session.is_processing());
    assert!(build.run().await.unwrap().result.is_err());
}

#[tokio::test]
async fn close_retains_pending_provider_acquisition_until_actual_completion() {
    let provider = Provider::new();
    provider.gate.acquire().await.unwrap().forget();
    let mut rig = Rig::with(manager(0, 2), false, provider);
    rig.establish(false);
    rig.pending();
    close(&rig.manager).unwrap();
    rig.pending();
    rig.provider.gate.add_permits(1);
    assert!(matches!(
        rig.build.run().await.unwrap().result,
        Err(BuildError::Establishment(EstablishError::Cancelled))
    ));
}

#[tokio::test]
async fn exhausted_build_boundary_rejects_without_membership_or_start_sends() {
    let manager = manager(Lsn::MAX - 1, 2);
    let sent = Arc::new(Mutex::new(Sent::default()));
    assert!(
        matches!(BuildIdle::begin(&manager, config(20, false), Arc::new(Provider::new()), callbacks(&sent), Clock::new()),
        Err(BuildError::Manager(error)) if matches!(*error, ManagerError::Configuration(ConfigurationError::MissingCurrentReplay(20))))
    );
    assert!(manager.lock().unwrap().build_handle(20).is_none());
    assert!(sent.lock().unwrap().replication.is_empty());
}

#[tokio::test]
async fn concurrent_build_faults_preserve_first_primary_fault_and_individual_end() {
    let manager = manager(0, 2);
    let mut provider = Provider::new();
    provider.acquisition_error = true;
    let mut first = Rig::with(manager.clone(), false, provider);
    let mut provider = Provider::new();
    provider.enumeration_error = true;
    let mut second = BuildIdle::begin(
        &manager,
        config(30, false),
        Arc::new(provider),
        callbacks(&first.sent),
        Clock::new(),
    )
    .unwrap();
    first.establish(false);
    second.handle().receive_ack(&ack(30, 0, 0, 0, 0)).unwrap();
    assert_eq!(
        first.build.run().await.unwrap().fault.unwrap().description,
        "GetCopyState"
    );
    assert_eq!(
        second.run().await.unwrap().fault.unwrap().description,
        "EndGetNext"
    );
    assert_eq!(
        manager.lock().unwrap().build_fault().unwrap().description,
        "GetCopyState"
    );
}

#[tokio::test]
async fn ack_beyond_issued_tail_is_rejected_before_session_or_manager_mutation() {
    let mut rig = Rig::new(false);
    assert!(rig.handle.receive_ack(&ack(20, 1, 1, 0, 0)).is_err());
    assert_eq!(rig.handle.0.session.progress(0).received, 0);
    assert!(!rig.handle.0.session.is_processing());
    rig.pending();
    assert_eq!(rig.provider.calls.load(Ordering::SeqCst), 0);
    enqueue(&rig.manager);
    rig.establish(false);
    rig.pending();
    rig.handle.receive_ack(&ack(20, 1, 1, 2, 0)).unwrap();
    assert!(rig.build.run().await.unwrap().result.is_ok());
}

#[tokio::test]
async fn unknown_copy_terminal_cannot_establish_or_cache_all_copy_before_enumeration() {
    let mut rig = Rig::new(false);
    assert!(rig.handle.receive_ack(&ack(20, 0, 0, -1, 0)).is_err());
    rig.pending();
    assert_eq!(rig.provider.calls.load(Ordering::SeqCst), 0);
    rig.establish(false);
    rig.pending();
    rig.handle.receive_ack(&ack(20, 0, 0, -1, 0)).unwrap();
    assert!(rig.build.run().await.unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn established_build_keeps_start_copy_retries_during_pending_provider_acquisition() {
    let provider = Provider::new();
    provider.gate.acquire().await.unwrap().forget();
    let mut rig = Rig::with(manager(0, 2), false, provider);
    rig.establish(false);
    let handle = rig.handle.clone();
    assert!(
        handle
            .0
            .session
            .on_start_copy_timer(Duration::ZERO)
            .is_none()
    );
    let run = rig.build.run();
    tokio::pin!(run);
    assert!(run.as_mut().now_or_never().is_none());
    assert!(handle.0.session.start_copy_deadline().is_some());
    let before = rig.sent.lock().unwrap().replication.len();
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(run.as_mut().now_or_never().is_none());
    assert!(rig.sent.lock().unwrap().replication.len() > before);
    assert_eq!(rig.provider.calls.load(Ordering::SeqCst), 1);
    assert!(
        handle
            .0
            .session
            .on_start_copy_timer(handle.0.clock.now())
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn driver_resamples_new_context_ack_deadline_while_provider_is_pending() {
    let mut rig = Rig::new(true);
    rig.handle
        .receive_context(&context(20, 1, false, b"target"))
        .unwrap();
    let handle = rig.handle.clone();
    let run = rig.build.run();
    tokio::pin!(run);
    assert!(run.as_mut().now_or_never().is_none());
    // Closing StartCopy isolates the newly installed context ACK deadline.
    handle.0.session.close_start_copy();
    handle.wake_timers();
    assert!(run.as_mut().now_or_never().is_none());
    let before = rig.sent.lock().unwrap().context_acks.len();
    handle
        .receive_context(&context(20, 2, false, b"more"))
        .unwrap();
    assert!(run.as_mut().now_or_never().is_none());
    tokio::time::advance(Duration::from_millis(20)).await;
    assert!(run.as_mut().now_or_never().is_none());
    assert!(rig.sent.lock().unwrap().context_acks.len() > before);
}

#[tokio::test(start_paused = true)]
async fn driver_resamples_new_replication_retry_after_idle_enqueue() {
    let provider = Provider::new();
    provider.gate.acquire().await.unwrap().forget();
    let mut rig = Rig::with(manager(0, 2), false, provider);
    rig.establish(false);
    rig.handle.0.session.close_start_copy();
    let run = rig.build.run();
    tokio::pin!(run);
    assert!(run.as_mut().now_or_never().is_none());
    enqueue(&rig.manager);
    assert!(run.as_mut().now_or_never().is_none());
    let before = rig.sent.lock().unwrap().replication.len();
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(run.as_mut().now_or_never().is_none());
    assert!(rig.sent.lock().unwrap().replication.len() > before);
}
