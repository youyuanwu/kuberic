// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.

use super::*;
use crate::replicator::v1::ingress::{Disposition, DropReason};
use crate::replicator::v1::secondary_receiver::ReceiverError;

fn retained(progress: Lsn, eos: bool) -> Rig {
    Rig::with_provider(
        true,
        eos,
        Arc::new(Provider {
            progress,
            ..Provider::new()
        }),
        Some(2),
    )
}

async fn restore(rig: &mut Rig) -> SecondaryHandle {
    rig.owner
        .begin_restore_retained_secondary(epoch(1))
        .unwrap();
    rig.owner.run().await.unwrap();
    rig.owner.secondary().unwrap()
}

fn fresh_route() -> Route {
    let mut fresh = route(30);
    fresh.destination.incarnation = Bytes::from(vec![8; 16]);
    fresh.remote_address = "new-primary".into();
    fresh
}

fn replication(route: &Route, lsn: Lsn, original: i64, sending: i64) -> wire::Envelope {
    route
        .reversed()
        .envelope(wire::envelope::Body::ReplicationOperation(
            wire::ReplicationOperation {
                metadata: Some(wire::Metadata {
                    sequence: lsn,
                    operation_type: 1,
                    atomic_group: -1,
                }),
                original_epoch: Some(wire::Epoch {
                    data_loss: 1,
                    configuration: original,
                }),
                sending_epoch: Some(wire::Epoch {
                    data_loss: 1,
                    configuration: sending,
                }),
                buffers: Some(wire::Buffers {
                    segments: vec![Bytes::from_static(b"new-data")],
                }),
                first_sequence: lsn,
                last_sequence: lsn,
                last_in_batch: lsn,
                buffer_counts: vec![1],
                completed_hint: 0,
            },
        ))
}

#[tokio::test]
async fn retained_constructor_reads_real_provider_ignores_cache_and_installs_native_epoch() {
    let mut rig = retained(7, true);
    rig.owner.begin_get_opened_progress().unwrap();
    assert!(matches!(rig.owner.opened_progress_result(), Some(Ok(2))));
    let handle = restore(&mut rig).await;
    assert_eq!(rig.owner.status(), RoleStatus::Ready(Role::ActiveSecondary));
    assert_eq!(handle.current_progress().unwrap(), 7);
    assert_eq!(handle.catchup_capability().unwrap(), 0);
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *rig.provider.epochs.lock().unwrap(),
        [EpochBarrier {
            epoch: epoch(1),
            previous_epoch_last_lsn: 7,
        }]
    );
    assert_eq!(rig.provider.context_calls.load(Ordering::SeqCst), 0);
    assert_eq!(rig.provider.copy_calls.load(Ordering::SeqCst), 0);
    assert_eq!(rig.provider.data_loss_calls.load(Ordering::SeqCst), 0);
    assert!(rig.sent.lock().unwrap().is_empty());
    assert!(handle.copy_stream().unwrap().next().await.is_none());
    let mut stream = handle.replication_stream().unwrap();
    assert!(stream.next().now_or_never().is_none());
    assert!(matches!(
        handle.transport_identity(),
        Err(SecondaryError::RetainedTransport)
    ));
    assert!(matches!(
        handle.receive_replication(epoch(1), vec![operation(8, epoch(1), true)], None),
        Err(SecondaryError::RetainedTransport)
    ));
    assert!(matches!(
        handle.continue_transport(&route(10), epoch(1)),
        Err(SecondaryError::RetainedTransport)
    ));
    assert!(matches!(
        rig.owner.begin_restore_retained_secondary(epoch(1)),
        Err(RoleError::InvalidState)
    ));
    assert_eq!(handle.current_progress().unwrap(), 7);
    rig.owner.abort().unwrap();
    assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
}

#[tokio::test]
async fn retained_provider_failures_never_install_a_receiver_or_publish_a_role() {
    for fail_progress in [true, false] {
        let mut rig = retained(7, false);
        rig.provider
            .fail_progress
            .store(fail_progress, Ordering::SeqCst);
        rig.provider
            .fail_epoch
            .store(!fail_progress, Ordering::SeqCst);
        rig.owner
            .begin_restore_retained_secondary(epoch(1))
            .unwrap();
        assert!(matches!(
            rig.owner.run().await,
            Err(RoleError::Secondary(SecondaryError::Provider(_)))
        ));
        assert_eq!(rig.owner.status(), RoleStatus::Faulted);
        assert!(rig.owner.secondary.is_none());
        assert!(rig.owner.secondary().is_err());
        assert!(rig.owner.primary().is_err());
        assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            rig.provider.epochs.lock().unwrap().len(),
            usize::from(!fail_progress)
        );
        assert!(
            rig.owner
                .begin_restore_retained_secondary(epoch(1))
                .is_err()
        );
    }
}

#[tokio::test]
async fn retained_invalid_boundaries_and_epochs_reject_without_epoch_effects() {
    for progress in [Lsn::MIN, -1, Lsn::MAX - 1, Lsn::MAX] {
        let mut rig = retained(progress, false);
        rig.owner
            .begin_restore_retained_secondary(epoch(1))
            .unwrap();
        assert!(matches!(
            rig.owner.run().await,
            Err(RoleError::Secondary(SecondaryError::InvalidRetainedState))
        ));
        assert!(rig.provider.epochs.lock().unwrap().is_empty());
        assert!(rig.owner.secondary.is_none());
    }
    let mut rig = retained(7, false);
    for invalid in [Epoch::new(-1, 1), Epoch::new(1, -1)] {
        assert!(matches!(
            rig.owner.begin_restore_retained_secondary(invalid),
            Err(RoleError::InvalidConfig)
        ));
    }
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 0);
    assert_eq!(rig.owner.status(), RoleStatus::Opened);
    let mut volatile = Rig::new(false, false);
    assert!(matches!(
        volatile.owner.begin_restore_retained_secondary(epoch(1)),
        Err(RoleError::InvalidConfig)
    ));
    assert_eq!(volatile.provider.progress_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn retained_empty_provider_prefix_is_a_real_empty_checkpoint_not_a_copy() {
    let mut rig = retained(0, false);
    let handle = restore(&mut rig).await;
    assert_eq!(handle.current_progress().unwrap(), 0);
    assert_eq!(handle.catchup_capability().unwrap(), 0);
    handle
        .bind_retained_transport(&fresh_route(), epoch(1))
        .unwrap();
    let mut stream = handle.replication_stream().unwrap();
    handle
        .receive_replication(epoch(1), vec![operation(1, epoch(1), true)], None)
        .unwrap();
    let first = take(&mut stream).await;
    ack_replication(&handle, &first);
    assert_eq!(handle.current_progress().unwrap(), 1);
    assert_eq!(handle.catchup_capability().unwrap(), 1);
    rig.owner.abort().unwrap();
    assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
}

#[tokio::test]
async fn retained_pending_read_and_epoch_repoll_once_and_keep_every_other_request_busy() {
    let provider = Arc::new(Provider {
        progress: 7,
        progress_gate: Semaphore::new(0),
        epochs_gate: Semaphore::new(0),
        ..Provider::new()
    });
    let mut rig = Rig::with_provider(true, true, provider, Some(2));
    rig.owner
        .begin_restore_retained_secondary(epoch(1))
        .unwrap();
    for _ in 0..3 {
        assert!(rig.owner.run().now_or_never().is_none());
    }
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 1);
    assert!(rig.provider.epochs.lock().unwrap().is_empty());
    assert!(matches!(
        rig.owner.begin_restore_retained_secondary(epoch(2)),
        Err(RoleError::Busy)
    ));
    assert!(matches!(
        rig.owner.begin_change_role(Role::Primary, epoch(2)),
        Err(RoleError::Busy)
    ));
    rig.provider.progress_gate.add_permits(1);
    for _ in 0..3 {
        assert!(rig.owner.run().now_or_never().is_none());
    }
    assert_eq!(rig.provider.epochs.lock().unwrap().len(), 1);
    assert!(rig.owner.secondary.is_none());
    assert!(rig.owner.secondary().is_err());
    assert!(matches!(
        rig.owner.begin_update_epoch(epoch(2)),
        Err(RoleError::Busy)
    ));
    rig.provider.epochs_gate.add_permits(1);
    rig.owner.run().await.unwrap();
    assert_eq!(
        rig.owner.secondary().unwrap().current_progress().unwrap(),
        7
    );
}

#[tokio::test]
async fn retained_abort_joins_only_started_callbacks_and_never_constructs_an_owner() {
    for during_epoch in [false, true] {
        let provider = Arc::new(Provider {
            progress: 7,
            progress_gate: Semaphore::new(usize::from(during_epoch)),
            epochs_gate: Semaphore::new(0),
            ..Provider::new()
        });
        let mut rig = Rig::with_provider(true, true, provider, None);
        rig.owner
            .begin_restore_retained_secondary(epoch(1))
            .unwrap();
        assert!(rig.owner.run().now_or_never().is_none());
        rig.owner.abort().unwrap();
        assert!(rig.owner.run().now_or_never().is_none());
        if during_epoch {
            rig.provider.fail_epoch.store(true, Ordering::SeqCst);
            rig.provider.epochs_gate.add_permits(1);
        } else {
            rig.provider.progress_gate.add_permits(1);
        }
        assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
        assert!(rig.owner.secondary.is_none());
        assert_eq!(rig.owner.status(), RoleStatus::Aborted);
        assert_eq!(
            rig.provider.epochs.lock().unwrap().len(),
            usize::from(during_epoch)
        );
        if during_epoch {
            assert!(matches!(
                rig.owner.fault(),
                Some(RoleError::Secondary(SecondaryError::Provider(_)))
            ));
        }
    }
}

#[tokio::test]
async fn retained_constructor_cannot_replace_a_copy_receiver_or_a_live_primary() {
    let mut created = Rig::unopened(true, false, Arc::new(Provider::new()), None);
    assert!(matches!(
        created.owner.begin_restore_retained_secondary(epoch(1)),
        Err(RoleError::InvalidState)
    ));
    for initialized in [false, true] {
        let mut rig = Rig::new(true, false);
        let secondary = rig.secondary();
        if initialized {
            rig.start(&secondary);
            rig.finish_copy(&secondary).await;
        }
        assert!(matches!(
            rig.owner.begin_restore_retained_secondary(epoch(1)),
            Err(RoleError::InvalidState)
        ));
        assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 0);
        assert!(rig.provider.epochs.lock().unwrap().is_empty());
    }
    let mut rig = Rig::new(true, false);
    let primary = rig.primary().await;
    configuration(&primary, 1, &[]);
    let before = primary.current_progress().unwrap();
    assert!(matches!(
        rig.owner.begin_restore_retained_secondary(epoch(2)),
        Err(RoleError::InvalidState)
    ));
    assert_eq!(primary.current_progress().unwrap(), before);
}

#[tokio::test]
async fn retained_fresh_binding_is_once_exact_epoch_and_rejects_old_source_and_copy() {
    let mut rig = retained(7, true);
    let handle = restore(&mut rig).await;
    let ingress = rig.owner.ingress();
    let fresh = fresh_route();
    assert!(ingress.dispatch(&replication(&route(10), 8, 1, 1)).is_err());
    for crossed in 0..4 {
        let mut wrong = fresh.clone();
        match crossed {
            0 => wrong.source.incarnation = Bytes::from(vec![9; 16]),
            1 => wrong.sender_address = "another-local-owner".into(),
            2 => wrong.destination.partition = Bytes::from(vec![9; 16]),
            _ => wrong.destination.incarnation = Bytes::new(),
        }
        assert!(handle.bind_retained_transport(&wrong, epoch(1)).is_err());
    }
    assert!(handle.bind_retained_transport(&fresh, epoch(0)).is_err());
    assert!(handle.bind_retained_transport(&fresh, epoch(2)).is_err());
    rig.owner.begin_update_epoch(epoch(2)).unwrap();
    rig.owner.run().await.unwrap();
    assert!(handle.bind_retained_transport(&fresh, epoch(1)).is_err());
    handle.bind_retained_transport(&fresh, epoch(2)).unwrap();
    assert_eq!(handle.transport_identity().unwrap().1, 1);
    assert!(handle.bind_retained_transport(&fresh, epoch(2)).is_err());
    let old_start = route(10)
        .reversed()
        .envelope(wire::envelope::Body::StartCopy(wire::StartCopy {
            epoch: Some(wire::Epoch {
                data_loss: 1,
                configuration: 2,
            }),
            replica: 20,
            replication_start: 1,
        }));
    assert!(matches!(
        handle.process_start(&old_start),
        Err(SecondaryError::RetainedCopy)
    ));
    assert!(matches!(
        handle.receive_copy(
            ReceivedOperation::copy_end_of_stream(1, true).unwrap(),
            true
        ),
        Err(SecondaryError::RetainedCopy)
    ));
    assert!(ingress.dispatch(&replication(&route(10), 8, 1, 2)).is_err());
    assert_eq!(handle.current_progress().unwrap(), 7);
    assert_eq!(
        ingress.dispatch(&replication(&fresh, 8, 1, 1)).unwrap(),
        Disposition::Dropped(DropReason::StaleEpoch)
    );
    let mut stream = handle.replication_stream().unwrap();
    assert_eq!(
        ingress.dispatch(&replication(&fresh, 8, 1, 2)).unwrap(),
        Disposition::Processed
    );
    let delivered = take(&mut stream).await;
    assert_eq!(delivered.queue_operation().lsn(), 8);
    assert_eq!(
        delivered.queue_operation().data(),
        &Bytes::from_static(b"new-data")
    );
    ack_replication(&handle, &delivered);
    assert_eq!(handle.current_progress().unwrap(), 8);
    assert_eq!(handle.catchup_capability().unwrap(), 8);
    assert_eq!(rig.provider.epochs.lock().unwrap().len(), 1);
    rig.owner.abort().unwrap();
    assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
}

#[tokio::test]
async fn retained_new_original_epoch_uses_actual_provider_stream_barrier_then_consumes_data() {
    let mut rig = retained(7, true);
    let handle = restore(&mut rig).await;
    rig.owner.begin_update_epoch(epoch(2)).unwrap();
    rig.owner.run().await.unwrap();
    handle
        .bind_retained_transport(&fresh_route(), epoch(2))
        .unwrap();
    let mut stream = handle.replication_stream().unwrap();
    rig.owner
        .ingress()
        .dispatch(&replication(&fresh_route(), 8, 2, 2))
        .unwrap();
    let barrier = match stream.next().await.unwrap().unwrap() {
        StreamEvent::UpdateEpoch(barrier) => barrier,
        _ => panic!("expected actual provider epoch barrier"),
    };
    assert_eq!(barrier.epoch, epoch(2));
    assert_eq!(barrier.previous_epoch_last_lsn, 7);
    assert!(stream.next().now_or_never().is_none());
    handle.update_stream_epoch(barrier).unwrap();
    assert!(rig.owner.drive().now_or_never().is_none());
    let data = take(&mut stream).await;
    ack_replication(&handle, &data);
    assert_eq!(handle.current_progress().unwrap(), 8);
    assert_eq!(
        rig.provider.epochs.lock().unwrap().as_slice(),
        [
            EpochBarrier {
                epoch: epoch(1),
                previous_epoch_last_lsn: 7,
            },
            barrier,
        ]
    );
    rig.owner.abort().unwrap();
    assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
}

#[tokio::test]
async fn retained_promotion_waits_real_local_eos_and_data_ack_then_preserves_native_boundary() {
    let mut rig = retained(7, true);
    let handle = restore(&mut rig).await;
    handle
        .bind_retained_transport(&fresh_route(), epoch(1))
        .unwrap();
    let mut stream = handle.replication_stream().unwrap();
    handle
        .receive_replication(epoch(1), vec![operation(8, epoch(1), true)], None)
        .unwrap();
    let data = take(&mut stream).await;
    rig.owner
        .begin_change_role(Role::Primary, epoch(2))
        .unwrap();
    let eos = take(&mut stream).await;
    assert!(eos.is_end_of_stream());
    assert!(rig.owner.run().now_or_never().is_none());
    ack_replication(&handle, &data);
    assert!(rig.owner.run().now_or_never().is_none());
    ack_replication(&handle, &eos);
    rig.owner.run().await.unwrap();
    let primary = rig.owner.primary().unwrap();
    assert_eq!(primary.current_progress().unwrap(), 8);
    assert!(
        primary
            .enqueue(Bytes::new(), BufferPolicy::RawSingle, limits())
            .is_err()
    );
    configuration(&primary, 1, &[]);
    let write = enqueue(&primary);
    assert_eq!(write.lsn, 9);
    assert_eq!(write.completion.await.unwrap(), Ok(()));
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.provider.copy_calls.load(Ordering::SeqCst), 0);
    assert_eq!(rig.provider.context_calls.load(Ordering::SeqCst), 0);
    rig.owner
        .begin_change_role(Role::ActiveSecondary, epoch(3))
        .unwrap();
    rig.owner.run().await.unwrap();
    let demoted = rig.owner.secondary().unwrap();
    assert_eq!(demoted.current_progress().unwrap(), 9);
    let mut resumed = demoted.replication_stream().unwrap();
    demoted
        .receive_replication(epoch(3), vec![operation(10, epoch(2), true)], None)
        .unwrap();
    let next = take(&mut resumed).await;
    assert_eq!(next.queue_operation().lsn(), 10);
    ack_replication(&demoted, &next);
    rig.owner.abort().unwrap();
    assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
}

#[tokio::test]
async fn retained_old_ack_object_cannot_complete_new_native_operations() {
    let mut old = Rig::new(true, false);
    let predecessor = old.secondary();
    old.start(&predecessor);
    old.finish_copy(&predecessor).await;
    let old_data = operation(1, epoch(1), true);
    predecessor
        .receive_replication(epoch(1), vec![old_data.clone()], None)
        .unwrap();
    let work = old_data.acknowledge().unwrap().unwrap();
    let mut new = retained(7, true);
    let restored = restore(&mut new).await;
    assert!(matches!(
        restored.acknowledge_replication(work),
        Err(SecondaryError::Receiver(error)) if matches!(error.as_ref(), ReceiverError::ForeignAck)
    ));
    assert_eq!(restored.current_progress().unwrap(), 7);
    assert_eq!(restored.catchup_capability().unwrap(), 0);
    old.owner.abort().unwrap();
    assert!(matches!(old.owner.run().await, Err(RoleError::Aborted)));
    new.owner.abort().unwrap();
    assert!(matches!(new.owner.run().await, Err(RoleError::Aborted)));
}
