// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-owner vectors from Replicator.OnDatalossAsyncOperation.cpp:30-157,
// Replicator.cpp:659-708,799-838, ReplicationQueueManager.cpp:148-160 and
// PublicApi.Test.cpp:1349-1383 at the pinned revision in role.rs.

use super::*;
use crate::replicator::v1::replica_manager::{CatchupMode, ManagerError};
use crate::replicator::v1::replication_queue_manager::QueueNormalizationError;
use tokio::sync::oneshot::error::TryRecvError;

fn recovery(progress: Lsn, changed: bool, permits: usize) -> Rig {
    Rig::with_provider(
        true,
        false,
        Arc::new(Provider {
            progress,
            state_changed: AtomicBool::new(changed),
            data_loss_gate: Semaphore::new(permits),
            ..Provider::new()
        }),
        Some(7),
    )
}

fn finished(rig: &Rig, changed: bool) {
    let end = rig.owner.data_loss_result().unwrap();
    assert_eq!(end.state_changed, changed);
    assert!(end.result.is_ok());
    assert_eq!(rig.owner.status(), RoleStatus::Ready(Role::Primary));
    assert!(!rig.owner.operation_pending());
    assert!(rig.owner.fault().is_none());
}

fn pending_reset(error: &RoleError) -> bool {
    matches!(
        error,
        RoleError::Primary(PrimaryError::Manager(error))
        if matches!(error.as_ref(), ManagerError::QueueNormalization(
            QueueNormalizationError::PendingOperations))
    )
}

#[tokio::test]
async fn data_loss_requires_stable_primary_and_serializes_other_role_operations() {
    let mut rig = Rig::new(true, false);
    assert!(matches!(
        rig.owner.begin_data_loss(),
        Err(RoleError::InvalidState)
    ));
    let secondary = rig.secondary();
    assert!(matches!(
        rig.owner.begin_data_loss(),
        Err(RoleError::InvalidState)
    ));
    rig.start(&secondary);
    rig.finish_copy(&secondary).await;
    rig.owner
        .begin_change_role(Role::Primary, epoch(2))
        .unwrap();
    assert!(matches!(rig.owner.begin_data_loss(), Err(RoleError::Busy)));
    rig.owner.run().await.unwrap();
    rig.owner.begin_data_loss().unwrap();
    assert_eq!(rig.owner.status(), RoleStatus::CheckingDataLoss);
    assert!(rig.owner.data_loss_result().is_none());
    assert!(matches!(rig.owner.begin_data_loss(), Err(RoleError::Busy)));
    assert!(matches!(
        rig.owner.begin_update_epoch(epoch(3)),
        Err(RoleError::Busy)
    ));
    assert!(matches!(
        rig.owner.begin_change_role(Role::None, epoch(3)),
        Err(RoleError::Busy)
    ));
    assert!(matches!(rig.owner.drive().await, Err(RoleError::Busy)));
    assert!(rig.owner.primary().is_err());
    assert!(rig.owner.secondary().is_err());
    rig.owner.run().await.unwrap();
    finished(&rig, false);
    rig.owner.begin_change_role(Role::None, epoch(3)).unwrap();
    rig.owner.run().await.unwrap();
    assert!(matches!(
        rig.owner.begin_data_loss(),
        Err(RoleError::InvalidState)
    ));
    assert!(rig.owner.data_loss_result().unwrap().result.is_ok());
    assert_eq!(rig.provider.data_loss_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn checking_data_loss_gates_previously_issued_primary_handles_and_ingress() {
    let mut rig = recovery(0, false, 0);
    let primary = rig.primary().await;
    configuration(&primary, 2, &[(10, 7)]);
    rig.owner.begin_data_loss().unwrap();
    assert!(matches!(
        primary.enqueue(Bytes::new(), BufferPolicy::RawSingle, limits()),
        Err(PrimaryError::CheckingDataLoss)
    ));
    assert!(matches!(
        primary.update_current_configuration(ReplicaConfiguration {
            write_quorum: 1,
            replicas: Vec::new()
        }),
        Err(PrimaryError::CheckingDataLoss)
    ));
    assert!(matches!(
        primary.update_catchup_configuration(
            None,
            ReplicaConfiguration {
                write_quorum: 1,
                replicas: Vec::new()
            }
        ),
        Err(PrimaryError::CheckingDataLoss)
    ));
    assert!(matches!(
        primary.begin_catchup(CatchupMode::All),
        Err(PrimaryError::CheckingDataLoss)
    ));
    assert!(matches!(
        primary.remove_idle(10),
        Err(PrimaryError::CheckingDataLoss)
    ));
    assert!(matches!(
        primary.update_epoch(epoch(2)),
        Err(PrimaryError::CheckingDataLoss)
    ));
    assert!(matches!(
        primary.close_for_secondary(),
        Err(PrimaryError::CheckingDataLoss)
    ));
    assert!(matches!(
        primary.begin_build(
            BuildConfig {
                route: route(30),
                remote_incarnation: 30,
                copy: CopyConfig {
                    sender: sender(),
                    buffer_policy: BufferPolicy::RawSingle,
                    buffer_limits: limits(),
                },
                context: None,
            },
            rig.provider.clone(),
            BuildCallbacks {
                replication: Arc::new(|_| true),
                copy: Arc::new(|_| true),
                context_ack: Arc::new(|_, _| true),
            },
        ),
        Err(PrimaryError::CheckingDataLoss)
    ));
    let envelope = route(10)
        .reversed()
        .envelope(wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
            replication_received: 7,
            replication_applied: 7,
            copy_received: -1,
            copy_applied: -1,
            error: 0,
        }));
    assert!(matches!(
        primary.receive_ack(&envelope),
        Err(PrimaryError::CheckingDataLoss)
    ));
    assert!(matches!(
        primary.receive_context(&envelope),
        Err(PrimaryError::CheckingDataLoss)
    ));
    assert!(rig.owner.run().now_or_never().is_none());
    rig.provider.data_loss_gate.add_permits(1);
    rig.owner.run().await.unwrap();
    ack_primary(&primary, 10, 7);
    finished(&rig, false);
}

#[tokio::test(start_paused = true)]
async fn unchanged_recovery_preserves_pending_queue_identity_progress_and_retry_driving() {
    let mut rig = recovery(24, false, 0);
    let primary = rig.primary().await;
    configuration(&primary, 2, &[(10, 7)]);
    let mut held = enqueue(&primary);
    assert_eq!(held.lsn, 8);
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
    rig.owner.begin_data_loss().unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.sent.lock().unwrap().clear();
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(rig.owner.run().now_or_never().is_none());
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
                        && *sending_epoch == epoch(1),
                _ => false,
            })
    );
    rig.provider.fail_progress.store(true, Ordering::SeqCst);
    rig.provider.data_loss_gate.add_permits(1);
    rig.owner.run().await.unwrap();
    finished(&rig, false);
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 0);
    assert_eq!(rig.owner.config.cached_progress, Some(7));
    assert_eq!(held.completion.try_recv(), Err(TryRecvError::Empty));
    ack_primary(&primary, 10, 8);
    assert_eq!(held.completion.await.unwrap(), Ok(()));
    assert_eq!(enqueue(&primary).lsn, 9);
    assert_eq!(original.data().as_ref(), b"data");
}

#[tokio::test]
async fn changed_recovery_uses_actual_higher_lower_equal_and_empty_provider_progress() {
    for progress in [24, 2, 7, 0] {
        let mut rig = recovery(progress, true, 1);
        let primary = rig.primary().await;
        rig.owner.begin_data_loss().unwrap();
        rig.owner.run().await.unwrap();
        finished(&rig, true);
        assert_eq!(rig.provider.data_loss_calls.load(Ordering::SeqCst), 1);
        assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 1);
        assert_eq!(rig.owner.config.cached_progress, Some(progress));
        rig.owner.begin_update_epoch(epoch(2)).unwrap();
        rig.owner.run().await.unwrap();
        assert_eq!(
            rig.provider.epochs.lock().unwrap()[1].previous_epoch_last_lsn,
            progress
        );
        configuration(&primary, 1, &[]);
        let write = enqueue(&primary);
        assert_eq!(write.lsn, progress + 1);
        assert_eq!(write.completion.await.unwrap(), Ok(()));
    }
}

#[tokio::test]
async fn interrupted_callback_and_progress_query_are_each_retained_once() {
    let mut rig = recovery(24, true, 0);
    rig.provider.progress_gate.forget_permits(1);
    rig.primary().await;
    rig.owner.begin_data_loss().unwrap();
    for _ in 0..3 {
        assert!(rig.owner.run().now_or_never().is_none());
    }
    assert_eq!(rig.provider.data_loss_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 0);
    rig.provider.data_loss_gate.add_permits(1);
    for _ in 0..3 {
        assert!(rig.owner.run().now_or_never().is_none());
    }
    assert!(rig.owner.data_loss_result().is_none());
    assert_eq!(rig.owner.config.cached_progress, Some(7));
    assert_eq!(rig.provider.data_loss_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 1);
    rig.provider.progress_gate.add_permits(1);
    rig.owner.run().await.unwrap();
    finished(&rig, true);
    assert_eq!(rig.owner.config.cached_progress, Some(24));
}

#[tokio::test]
async fn data_loss_callback_failure_restores_primary_and_preserves_original_error() {
    let mut rig = recovery(24, true, 2);
    let primary = rig.primary().await;
    rig.provider.fail_data_loss.store(true, Ordering::SeqCst);
    rig.owner.begin_data_loss().unwrap();
    let error = rig.owner.run().await.unwrap_err();
    let RoleError::Provider(original) = error else {
        panic!("provider failure expected")
    };
    let end = rig.owner.data_loss_result().unwrap();
    assert!(!end.state_changed);
    assert!(
        matches!(&end.result, Err(RoleError::Provider(error)) if Arc::ptr_eq(error, &original))
    );
    assert_eq!(rig.owner.status(), RoleStatus::Ready(Role::Primary));
    assert!(rig.owner.fault().is_none());
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 0);
    assert_eq!(rig.owner.config.cached_progress, Some(7));
    configuration(&primary, 1, &[]);
    assert_eq!(enqueue(&primary).completion.await.unwrap(), Ok(()));
    rig.provider.fail_data_loss.store(false, Ordering::SeqCst);
    rig.owner.begin_data_loss().unwrap();
    assert!(rig.owner.data_loss_result().is_none());
    rig.owner.run().await.unwrap();
    finished(&rig, true);
}

#[tokio::test]
async fn progress_failure_keeps_changed_flag_and_old_queue_without_faulting_role() {
    let mut rig = recovery(24, true, 1);
    let primary = rig.primary().await;
    rig.provider.fail_progress.store(true, Ordering::SeqCst);
    rig.owner.begin_data_loss().unwrap();
    let error = rig.owner.run().await.unwrap_err();
    assert!(
        matches!(&error, RoleError::Provider(error) if error.to_string().contains("progress failure"))
    );
    let end = rig.owner.data_loss_result().unwrap();
    assert!(end.state_changed);
    assert!(end.result.is_err());
    assert_eq!(rig.owner.status(), RoleStatus::Ready(Role::Primary));
    assert!(rig.owner.fault().is_none());
    assert_eq!(rig.owner.config.cached_progress, Some(7));
    configuration(&primary, 1, &[]);
    assert_eq!(enqueue(&primary).lsn, 8);
}

#[tokio::test]
async fn invalid_recovered_progress_cannot_reset_queue_or_overflow_assignment() {
    for progress in [-1, Lsn::MAX] {
        let mut rig = recovery(progress, true, 1);
        let primary = rig.primary().await;
        rig.owner.begin_data_loss().unwrap();
        let error = rig.owner.run().await.unwrap_err();
        assert!(matches!(
            error,
            RoleError::Primary(PrimaryError::Manager(error))
                if matches!(error.as_ref(), ManagerError::QueueNormalization(
                    QueueNormalizationError::InvalidInitialProgress))
        ));
        assert!(rig.owner.data_loss_result().unwrap().state_changed);
        assert_eq!(rig.owner.status(), RoleStatus::Ready(Role::Primary));
        assert_eq!(rig.owner.config.cached_progress, Some(progress));
        configuration(&primary, 1, &[]);
        assert_eq!(enqueue(&primary).lsn, 8);
    }
}

#[tokio::test]
async fn changed_recovery_rejects_uncommitted_work_without_canceling_or_acknowledging_it() {
    let mut rig = recovery(0, true, 1);
    let primary = rig.primary().await;
    configuration(&primary, 2, &[(10, 7)]);
    let mut held = enqueue(&primary);
    rig.owner.begin_data_loss().unwrap();
    assert!(pending_reset(&rig.owner.run().await.unwrap_err()));
    assert!(rig.owner.data_loss_result().unwrap().state_changed);
    assert_eq!(rig.owner.status(), RoleStatus::Ready(Role::Primary));
    assert_eq!(rig.owner.config.cached_progress, Some(0));
    assert_eq!(held.completion.try_recv(), Err(TryRecvError::Empty));
    ack_primary(&primary, 10, 8);
    assert_eq!(held.completion.await.unwrap(), Ok(()));
    assert_eq!(enqueue(&primary).lsn, 9);
}

#[tokio::test]
async fn changed_recovery_also_rejects_committed_but_not_completed_queue_history() {
    let mut rig = recovery(0, true, 2);
    let primary = rig.primary().await;
    configuration(&primary, 2, &[(10, 7), (30, 7)]);
    let held = enqueue(&primary);
    ack_primary(&primary, 10, 8);
    assert_eq!(held.completion.await.unwrap(), Ok(()));
    rig.owner.begin_data_loss().unwrap();
    assert!(pending_reset(&rig.owner.run().await.unwrap_err()));
    ack_primary(&primary, 30, 8);
    configuration(&primary, 1, &[]);
    rig.provider.progress_gate.add_permits(1);
    rig.owner.begin_data_loss().unwrap();
    rig.owner.run().await.unwrap();
    finished(&rig, true);
    assert_eq!(enqueue(&primary).lsn, 1);
}

#[tokio::test]
async fn both_provider_callbacks_can_reenter_primary_without_held_owner_locks() {
    let held_handle = Arc::new(Mutex::new(None::<PrimaryHandle>));
    let reentrant = held_handle.clone();
    let mut rig = Rig::with_provider(
        true,
        false,
        Arc::new(Provider {
            progress: 24,
            state_changed: AtomicBool::new(true),
            recovery_hook: Some(Arc::new(move || {
                let primary = reentrant.lock().unwrap().clone().unwrap();
                assert!(matches!(
                    primary.enqueue(Bytes::new(), BufferPolicy::RawSingle, limits()),
                    Err(PrimaryError::CheckingDataLoss)
                ));
            })),
            ..Provider::new()
        }),
        Some(7),
    );
    *held_handle.lock().unwrap() = Some(rig.primary().await);
    rig.owner.begin_data_loss().unwrap();
    rig.owner.run().await.unwrap();
    finished(&rig, true);
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn abort_retains_recovery_and_changed_progress_query_without_successful_reset() {
    for changed in [false, true] {
        let mut rig = recovery(24, changed, 0);
        let primary = rig.primary().await;
        rig.provider.progress_gate.forget_permits(1);
        rig.owner.begin_data_loss().unwrap();
        assert!(rig.owner.run().now_or_never().is_none());
        rig.owner.abort().unwrap();
        assert!(rig.owner.run().now_or_never().is_none());
        rig.provider.data_loss_gate.add_permits(1);
        if changed {
            assert!(rig.owner.run().now_or_never().is_none());
            assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 1);
            rig.provider.progress_gate.add_permits(1);
        }
        assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
        let end = rig.owner.data_loss_result().unwrap();
        assert_eq!(end.state_changed, changed);
        assert!(matches!(end.result, Err(RoleError::Aborted)));
        assert_eq!(rig.owner.status(), RoleStatus::Aborted);
        assert_eq!(rig.provider.data_loss_calls.load(Ordering::SeqCst), 1);
        assert!(rig.owner.fault().is_none());
        assert!(matches!(
            primary.update_epoch(epoch(2)),
            Err(PrimaryError::Closed)
        ));
        assert!(matches!(
            rig.owner.begin_data_loss(),
            Err(RoleError::InvalidState)
        ));
    }
}

#[tokio::test]
async fn abort_preserves_callback_or_progress_failure_in_the_explicit_end_result() {
    for fail_progress in [false, true] {
        let mut rig = recovery(24, true, 0);
        rig.primary().await;
        rig.provider
            .fail_data_loss
            .store(!fail_progress, Ordering::SeqCst);
        rig.provider
            .fail_progress
            .store(fail_progress, Ordering::SeqCst);
        rig.owner.begin_data_loss().unwrap();
        assert!(rig.owner.run().now_or_never().is_none());
        rig.owner.abort().unwrap();
        rig.provider.data_loss_gate.add_permits(1);
        assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
        let end = rig.owner.data_loss_result().unwrap();
        assert_eq!(end.state_changed, fail_progress);
        let Err(RoleError::Provider(original)) = &end.result else {
            panic!("provider failure expected")
        };
        assert!(
            matches!(rig.owner.fault(), Some(RoleError::Provider(error)) if Arc::ptr_eq(error, original))
        );
        assert_eq!(rig.owner.status(), RoleStatus::Aborted);
    }
}

#[tokio::test]
async fn original_callback_error_wins_over_concurrent_primary_close_restoration_failure() {
    let mut rig = recovery(24, true, 0);
    let primary = rig.primary().await;
    rig.provider.fail_data_loss.store(true, Ordering::SeqCst);
    rig.owner.begin_data_loss().unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    primary.close(Duration::ZERO).unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    assert!(rig.owner.data_loss_result().is_none());
    rig.provider.data_loss_gate.add_permits(1);
    assert!(matches!(rig.owner.run().await, Err(RoleError::Provider(_))));
    assert_eq!(rig.owner.status(), RoleStatus::Faulted);
    assert!(matches!(
        rig.owner.fault(),
        Some(RoleError::Primary(PrimaryError::Closed))
    ));
    assert!(!rig.owner.data_loss_result().unwrap().state_changed);
}

#[tokio::test]
async fn data_loss_is_rejected_for_active_secondary_faulted_and_aborted_roles() {
    let mut secondary = Rig::new(true, false);
    secondary
        .owner
        .begin_change_role(Role::ActiveSecondary, epoch(1))
        .unwrap();
    assert!(matches!(
        secondary.owner.begin_data_loss(),
        Err(RoleError::InvalidState)
    ));
    assert_eq!(secondary.provider.data_loss_calls.load(Ordering::SeqCst), 0);

    let mut faulted = Rig::new(true, false);
    faulted.provider.fail_epoch.store(true, Ordering::SeqCst);
    faulted
        .owner
        .begin_change_role(Role::Primary, epoch(1))
        .unwrap();
    assert!(faulted.owner.run().await.is_err());
    assert!(matches!(
        faulted.owner.begin_data_loss(),
        Err(RoleError::InvalidState)
    ));
    assert_eq!(faulted.provider.data_loss_calls.load(Ordering::SeqCst), 0);
    faulted.owner.abort().unwrap();
    assert!(matches!(faulted.owner.run().await, Err(RoleError::Aborted)));
    assert!(matches!(
        faulted.owner.begin_data_loss(),
        Err(RoleError::InvalidState)
    ));
}

#[tokio::test]
async fn provider_reentrant_abort_cannot_publish_a_successful_recovery() {
    let held_handle = Arc::new(Mutex::new(None::<PrimaryHandle>));
    let reentrant = held_handle.clone();
    let mut rig = Rig::with_provider(
        true,
        false,
        Arc::new(Provider {
            progress: 24,
            state_changed: AtomicBool::new(true),
            recovery_hook: Some(Arc::new(move || {
                reentrant.lock().unwrap().clone().unwrap().abort().unwrap();
            })),
            ..Provider::new()
        }),
        Some(7),
    );
    *held_handle.lock().unwrap() = Some(rig.primary().await);
    rig.owner.begin_data_loss().unwrap();
    assert!(rig.owner.run().await.is_err());
    let end = rig.owner.data_loss_result().unwrap();
    assert!(end.state_changed);
    assert!(end.result.is_err());
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.owner.config.cached_progress, Some(24));
    assert_eq!(rig.owner.status(), RoleStatus::Faulted);
}
