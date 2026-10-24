// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-owner vectors from OpenAsyncOperation.cpp:29-81,
// Replicator.cpp:296-367, ComProxyStateProvider.cpp:63-79 and the lifecycle
// sources pinned in role.rs. No native binary or production host is exercised.

use super::*;
use crate::replicator::v1::copy_async_operation::CopySendAction;
use crate::replicator::v1::copy_context_receiver::CopyContextConfig;
use crate::replicator::v1::ingress::{Disposition, DropReason};
use crate::replicator::v1::replica_manager::{ConfigurationError, ManagerError};

fn request() -> wire::Envelope {
    route(10)
        .reversed()
        .envelope(wire::envelope::Body::RequestAck(wire::RequestAck {}))
}

fn created(provider: Arc<Provider>) -> Rig {
    Rig::unopened(true, false, provider, None)
}

#[tokio::test]
async fn created_open_registers_full_identity_once_without_querying_provider() {
    let registry = MessageRegistry::default();
    let mut rig = created(Arc::new(Provider::new()));
    assert_eq!(rig.owner.status(), RoleStatus::Created);
    assert!(
        rig.owner
            .begin_change_role(Role::Primary, epoch(1))
            .is_err()
    );
    assert!(rig.owner.begin_get_opened_progress().is_err());
    assert!(rig.owner.begin_close(Duration::ZERO).is_err());
    assert_eq!(
        registry.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::Unregistered)
    );
    let published = rig.owner.open(&registry).unwrap();
    assert_eq!(published.address, "local");
    assert_eq!(published.endpoint, route(10).source);
    assert_eq!(rig.owner.status(), RoleStatus::Opened);
    assert!(rig.owner.open(&registry).is_err());
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 0);
    assert!(rig.provider.epochs.lock().unwrap().is_empty());
    assert_eq!(
        registry.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::Role)
    );
    let mut malformed = request();
    malformed.destination.as_mut().unwrap().incarnation = Bytes::new();
    assert!(registry.dispatch(&malformed).is_err());
    rig.secondary();
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        registry.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::NotInitialized)
    );
}

#[tokio::test]
async fn duplicate_registration_cannot_replace_owner_and_drop_seals_retained_ingress() {
    let registry = MessageRegistry::default();
    let mut first = created(Arc::new(Provider::new()));
    let mut second = created(Arc::new(Provider::new()));
    first.owner.open(&registry).unwrap();
    first.secondary();
    let retained = first.owner.ingress();
    assert!(matches!(
        second.owner.open(&registry),
        Err(RoleError::Registration(
            RegistrationError::DuplicateEndpoint
        ))
    ));
    assert_eq!(second.owner.status(), RoleStatus::Created);
    assert_eq!(
        registry.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::NotInitialized)
    );
    drop(first);
    assert_eq!(
        registry.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::Unregistered)
    );
    assert_eq!(
        retained.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::Role)
    );
    second.owner.open(&registry).unwrap();
    second.secondary();
    assert_eq!(
        registry.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::NotInitialized)
    );
}

#[tokio::test]
async fn role_none_keeps_registration_until_close_and_closed_owner_cannot_reopen() {
    let registry = MessageRegistry::default();
    let mut rig = created(Arc::new(Provider::new()));
    rig.owner.open(&registry).unwrap();
    rig.owner.begin_change_role(Role::None, epoch(1)).unwrap();
    assert_eq!(
        registry.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::Role)
    );
    rig.owner.begin_close(Duration::ZERO).unwrap();
    assert_eq!(rig.owner.status(), RoleStatus::Closed);
    assert_eq!(
        registry.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::Unregistered)
    );
    assert!(rig.owner.open(&registry).is_err());
}

#[tokio::test]
async fn created_abort_is_admitted_and_opened_abort_unregisters_before_late_provider_end() {
    let registry = MessageRegistry::default();
    let mut rig = created(Arc::new(Provider::new()));
    rig.owner.abort().unwrap();
    assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
    assert!(rig.owner.open(&registry).is_err());

    let provider = Arc::new(Provider {
        progress_gate: Semaphore::new(0),
        ..Provider::new()
    });
    let mut rig = created(provider);
    rig.owner.open(&registry).unwrap();
    rig.owner.begin_get_opened_progress().unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    rig.owner.abort().unwrap();
    assert_eq!(
        registry.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::Unregistered)
    );
    assert!(rig.owner.run().now_or_never().is_none());
    rig.provider.progress_gate.add_permits(1);
    assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
    assert!(matches!(
        rig.owner.opened_progress_result(),
        Some(Err(RoleError::Aborted))
    ));
    assert_eq!(rig.owner.config.cached_progress, None);
    assert!(rig.owner.primary.is_none());
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn opened_query_retains_one_callback_and_caches_progress_for_initial_primary() {
    let provider = Arc::new(Provider {
        progress: 7,
        progress_gate: Semaphore::new(0),
        ..Provider::new()
    });
    let mut rig = Rig::with_provider(true, false, provider, None);
    rig.owner.begin_get_opened_progress().unwrap();
    for _ in 0..3 {
        assert!(rig.owner.run().now_or_never().is_none());
    }
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 1);
    assert!(rig.owner.opened_progress_result().is_none());
    assert!(matches!(
        rig.owner.begin_change_role(Role::Primary, epoch(1)),
        Err(RoleError::Busy)
    ));
    assert!(matches!(
        rig.owner.begin_close(Duration::ZERO),
        Err(RoleError::Busy)
    ));
    rig.provider.progress_gate.add_permits(1);
    rig.owner.run().await.unwrap();
    assert!(matches!(rig.owner.opened_progress_result(), Some(Ok(7))));
    rig.owner.begin_get_opened_progress().unwrap();
    assert!(!rig.owner.operation_pending());
    assert!(matches!(rig.owner.opened_progress_result(), Some(Ok(7))));
    let primary = rig.primary().await;
    assert!(matches!(
        rig.owner.begin_get_opened_progress(),
        Err(RoleError::InvalidState)
    ));
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        rig.provider.epochs.lock().unwrap()[0].previous_epoch_last_lsn,
        7
    );
    configuration(&primary, 1, &[]);
    assert_eq!(enqueue(&primary).lsn, 8);
}

#[tokio::test]
async fn query_failure_is_returned_without_fault_or_cache_and_can_be_retried() {
    let provider = Arc::new(Provider {
        progress: 7,
        fail_progress: AtomicBool::new(true),
        ..Provider::new()
    });
    let mut rig = Rig::with_provider(true, false, provider, None);
    rig.owner.begin_get_opened_progress().unwrap();
    assert!(
        matches!(rig.owner.run().await, Err(RoleError::Provider(error))
        if matches!(error.as_ref(), RuntimeError::InvalidReplication(message)
            if message == "progress failure"))
    );
    assert!(matches!(
        rig.owner.opened_progress_result(),
        Some(Err(RoleError::Provider(_)))
    ));
    assert_eq!(rig.owner.status(), RoleStatus::Opened);
    assert!(rig.owner.fault().is_none());
    assert_eq!(rig.owner.config.cached_progress, None);
    rig.provider.fail_progress.store(false, Ordering::SeqCst);
    rig.provider.progress_gate.add_permits(1);
    rig.owner.begin_get_opened_progress().unwrap();
    rig.owner.run().await.unwrap();
    assert!(matches!(rig.owner.opened_progress_result(), Some(Ok(7))));
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn successful_negative_provider_progress_is_empty_in_query_initial_and_data_loss_paths() {
    for progress in [Lsn::MIN, -7, -1, 0] {
        for query in [false, true] {
            let provider = Arc::new(Provider {
                progress,
                ..Provider::new()
            });
            let mut rig = Rig::with_provider(true, false, provider, None);
            if query {
                rig.owner.begin_get_opened_progress().unwrap();
                rig.owner.run().await.unwrap();
                assert!(matches!(rig.owner.opened_progress_result(), Some(Ok(0))));
            }
            let primary = rig.primary().await;
            assert_eq!(rig.owner.config.cached_progress, Some(0));
            assert_eq!(
                rig.provider.epochs.lock().unwrap()[0].previous_epoch_last_lsn,
                0
            );
            rig.provider.state_changed.store(true, Ordering::SeqCst);
            rig.provider.progress_gate.add_permits(1);
            rig.owner.begin_data_loss().unwrap();
            rig.owner.run().await.unwrap();
            assert_eq!(rig.owner.config.cached_progress, Some(0));
            configuration(&primary, 1, &[]);
            assert_eq!(enqueue(&primary).lsn, 1);
        }
    }
}

#[tokio::test]
async fn initial_progress_failure_faults_but_does_not_publish_primary_and_close_unregisters() {
    let registry = MessageRegistry::default();
    let mut rig = created(Arc::new(Provider {
        fail_progress: AtomicBool::new(true),
        ..Provider::new()
    }));
    rig.owner.open(&registry).unwrap();
    rig.owner
        .begin_change_role(Role::Primary, epoch(1))
        .unwrap();
    assert!(matches!(rig.owner.run().await, Err(RoleError::Provider(_))));
    assert_eq!(rig.owner.status(), RoleStatus::Faulted);
    assert!(rig.owner.primary.is_none());
    assert!(rig.provider.epochs.lock().unwrap().is_empty());
    rig.owner.begin_close(Duration::ZERO).unwrap();
    assert_eq!(
        registry.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::Unregistered)
    );
}

#[tokio::test]
async fn direct_close_keeps_registered_ack_ingress_through_actual_quorum_prewait() {
    let registry = MessageRegistry::default();
    let mut rig = created(Arc::new(Provider::new()));
    rig.owner.open(&registry).unwrap();
    let primary = rig.primary().await;
    configuration(&primary, 2, &[(10, 0)]);
    let write = enqueue(&primary);
    rig.owner.begin_close(Duration::from_secs(30)).unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    let ack = route(10)
        .reversed()
        .envelope(wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
            replication_received: write.lsn,
            replication_applied: write.lsn,
            copy_received: -1,
            copy_applied: -1,
            error: 0,
        }));
    assert_eq!(registry.dispatch(&ack).unwrap(), Disposition::Processed);
    assert_eq!(write.completion.await.unwrap(), Ok(()));
    rig.owner.run().await.unwrap();
    assert_eq!(rig.owner.status(), RoleStatus::Closed);
    assert_eq!(
        registry.dispatch(&ack).unwrap(),
        Disposition::Dropped(DropReason::Unregistered)
    );
}

#[tokio::test]
async fn failed_actual_secondary_close_also_unregisters_processor() {
    let registry = MessageRegistry::default();
    let mut rig = Rig::unopened(true, true, Arc::new(Provider::new()), None);
    rig.owner.open(&registry).unwrap();
    let secondary = rig.secondary();
    rig.start(&secondary);
    secondary
        .report_stream_fault("failed service drain".into())
        .unwrap();
    rig.owner.begin_close(Duration::ZERO).unwrap();
    assert!(rig.owner.run().await.is_err());
    assert_eq!(rig.owner.status(), RoleStatus::Faulted);
    assert_eq!(
        registry.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::Unregistered)
    );
    rig.owner.abort().unwrap();
    assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
}

#[tokio::test]
async fn provider_and_fault_callbacks_can_reenter_actual_registry_outside_its_guard() {
    let registry = MessageRegistry::default();
    let called = registry.clone();
    let provider = Arc::new(Provider {
        recovery_hook: Some(Arc::new(move || {
            assert_eq!(
                called.dispatch(&request()).unwrap(),
                Disposition::Dropped(DropReason::Role)
            );
        })),
        ..Provider::new()
    });
    let mut rig = created(provider);
    let reported = registry.clone();
    let reports = Arc::new(AtomicUsize::new(0));
    let count = reports.clone();
    rig.owner.config.secondary_callbacks.fault = Arc::new(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            reported.dispatch(&request()).unwrap(),
            Disposition::Dropped(DropReason::Faulted)
        );
    });
    rig.owner.open(&registry).unwrap();
    rig.owner.begin_get_opened_progress().unwrap();
    rig.owner.run().await.unwrap();
    let secondary = rig.secondary();
    rig.start(&secondary);
    let fault = route(10)
        .reversed()
        .envelope(wire::envelope::Body::InduceFault(wire::InduceFault {
            target_replica: 20,
            target_incarnation: route(10).source.incarnation,
            reason: "reentrant report".into(),
        }));
    assert_eq!(
        registry.dispatch(&fault).unwrap(),
        Disposition::FaultReported
    );
    assert_eq!(reports.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn restarted_primary_has_no_old_suffix_and_explicit_build_restores_replica() {
    let provider = Arc::new(Provider {
        progress: 7,
        ..Provider::new()
    });
    let mut old = Rig::with_provider(true, false, provider.clone(), Some(0));
    let old_primary = old.primary().await;
    configuration(&old_primary, 1, &[]);
    for lsn in 1..=7 {
        let write = enqueue(&old_primary);
        assert_eq!(write.lsn, lsn);
        assert_eq!(write.completion.await.unwrap(), Ok(()));
    }
    configuration(&old_primary, 2, &[(10, 7)]);
    let interrupted = enqueue(&old_primary);
    old.owner.abort().unwrap();
    assert!(matches!(old.owner.run().await, Err(RoleError::Aborted)));
    assert_eq!(
        interrupted.completion.await.unwrap(),
        Err(CompletionError::OperationCanceled)
    );

    let registry = MessageRegistry::default();
    let mut restarted = created(provider.clone());
    let old_endpoint = restarted.owner.config.primary.endpoint.clone();
    restarted.owner.config.primary.endpoint.incarnation = Bytes::from(vec![8; 16]);
    restarted.owner.config.secondary_route.source = restarted.owner.config.primary.endpoint.clone();
    // Reconstruct with fresh ingress identity, never by copying the old queue.
    restarted.owner =
        RoleReplicator::new(restarted.owner.config, provider.clone(), Clock::new()).unwrap();
    restarted.owner.open(&registry).unwrap();
    restarted
        .owner
        .begin_change_role(Role::Primary, epoch(2))
        .unwrap();
    restarted.owner.run().await.unwrap();
    let primary = restarted.owner.primary().unwrap();
    assert_eq!(provider.progress_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider
            .epochs
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .previous_epoch_last_lsn,
        7
    );
    let outbound = restarted.owner.config.secondary_route.clone();
    let mut target = created(Arc::new(Provider::new()));
    target.owner.config.primary.endpoint = outbound.destination.clone();
    target.owner.config.primary.address = outbound.remote_address.clone();
    target.owner.config.secondary_route = outbound.reversed();
    target.owner.config.secondary.batch_ack = Duration::ZERO;
    let replies = registry.clone();
    target.owner.config.secondary_callbacks.ack = Arc::new(move |route, ack, _| {
        let message = route.envelope(wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
            replication_received: ack.replication.received,
            replication_applied: ack.replication.applied,
            copy_received: ack.copy.received,
            copy_applied: ack.copy.applied,
            error: ack.error,
        }));
        assert_eq!(replies.dispatch(&message).unwrap(), Disposition::Processed);
        true
    });
    let contexts = registry.clone();
    target.owner.config.secondary_callbacks.context = Arc::new(move |route, action| {
        let CopySendAction::Operation { operation, is_last } = action else {
            panic!("context retry is outside this untimed journey")
        };
        let message = route.envelope(wire::envelope::Body::CopyContextOperation(
            wire::CopyContextOperation {
                metadata: Some(wire::Metadata {
                    sequence: operation.lsn(),
                    operation_type: 1,
                    atomic_group: -1,
                }),
                buffers: Some(wire::Buffers {
                    segments: if is_last {
                        vec![]
                    } else {
                        vec![operation.data().clone()]
                    },
                }),
                is_last,
            },
        ));
        assert_eq!(contexts.dispatch(&message).unwrap(), Disposition::Processed);
        true
    });
    target.owner =
        RoleReplicator::new(target.owner.config, target.provider.clone(), Clock::new()).unwrap();
    target.owner.open(&registry).unwrap();
    let secondary = target.secondary();
    let descriptor = || ReplicaDescriptor {
        route: outbound.clone(),
        remote_incarnation: 10,
        progress: Some(0),
        must_catchup: false,
    };
    assert!(matches!(
        primary.update_current_configuration(ReplicaConfiguration {
            write_quorum: 2,
            replicas: vec![descriptor()],
        }),
        Err(PrimaryError::Manager(error)) if matches!(
            error.as_ref(), ManagerError::Configuration(ConfigurationError::MissingCurrentReplay(10))
        )
    ));
    assert!(restarted.sent.lock().unwrap().is_empty());
    let mut old_message = request();
    old_message.destination = Some(old_endpoint);
    assert_eq!(
        registry.dispatch(&old_message).unwrap(),
        Disposition::Dropped(DropReason::Unregistered)
    );

    configuration(&primary, 2, &[]);
    let copies = Arc::new(Mutex::new(Vec::new()));
    let copied = copies.clone();
    let context_replies = registry.clone();
    let context_route = outbound.clone();
    let build = primary
        .begin_build(
            BuildConfig {
                route: outbound.clone(),
                remote_incarnation: 10,
                copy: restarted.owner.config.secondary.context,
                context: Some(CopyContextConfig {
                    initial_queue_size: 2,
                    max_queue_size: 16,
                    batch_ack_interval: Duration::from_millis(10),
                    retry_interval: Duration::from_secs(1),
                    max_pending_acknowledgements: 4,
                    trace_interval: Duration::ZERO,
                }),
            },
            provider.clone(),
            BuildCallbacks {
                fence: None,
                replication: restarted.owner.config.primary.send.clone(),
                copy: Arc::new(move |action| {
                    if let CopySendAction::Operation { operation, is_last } = action {
                        copied.lock().unwrap().push((operation, is_last));
                    }
                    true
                }),
                context_ack: Arc::new(move |ack, _| {
                    let message = context_route.envelope(wire::envelope::Body::CopyContextAck(
                        wire::CopyContextAck {
                            sequence: ack.completed,
                            error: ack.error_code,
                        },
                    ));
                    assert_eq!(
                        context_replies.dispatch(&message).unwrap(),
                        Disposition::Processed
                    );
                    true
                }),
            },
        )
        .unwrap();
    assert!(restarted.sent.lock().unwrap().iter().any(|intent| matches!(
        intent.kind, SendKind::StartCopy { replication_start: 8, epoch: e } if e == epoch(2)
    )));
    let start = outbound.envelope(wire::envelope::Body::StartCopy(wire::StartCopy {
        epoch: Some(wire::Epoch {
            data_loss: 1,
            configuration: 2,
        }),
        replica: 10,
        replication_start: 8,
    }));
    assert_eq!(registry.dispatch(&start).unwrap(), Disposition::Processed);
    assert!(target.owner.drive().now_or_never().is_none());
    assert!(restarted.owner.drive().now_or_never().is_none());
    assert_eq!(*provider.copy_boundaries.lock().unwrap(), [7]);
    let mut copy_stream = secondary.copy_stream().unwrap();
    for (operation, is_last) in copies.lock().unwrap().drain(..) {
        let message = outbound.envelope(wire::envelope::Body::CopyOperation(wire::CopyOperation {
            metadata: Some(wire::Metadata {
                sequence: operation.lsn(),
                operation_type: 1,
                atomic_group: -1,
            }),
            epoch: Some(wire::Epoch {
                data_loss: 1,
                configuration: 2,
            }),
            buffers: Some(wire::Buffers {
                segments: if is_last {
                    vec![]
                } else {
                    vec![operation.data().clone()]
                },
            }),
            replica: 10,
            is_last,
        }));
        assert_eq!(registry.dispatch(&message).unwrap(), Disposition::Processed);
    }
    let copied = take(&mut copy_stream).await;
    assert_eq!(copied.queue_operation().data().as_ref(), b"copy");
    ack_copy(&secondary, &copied);
    assert!(copy_stream.next().await.is_none());
    let request = outbound.envelope(wire::envelope::Body::RequestAck(wire::RequestAck {}));
    registry.dispatch(&request).unwrap();
    assert!(restarted.owner.drive().now_or_never().is_none());
    assert!(build.result().unwrap().result.is_ok());
    primary
        .update_current_configuration(ReplicaConfiguration {
            write_quorum: 2,
            replicas: vec![descriptor()],
        })
        .unwrap();
    let write = enqueue(&primary);
    assert_eq!(write.lsn, 8);
    let mut replication = secondary.replication_stream().unwrap();
    let (operation, original_epoch, sending_epoch, hint) = restarted
        .sent
        .lock()
        .unwrap()
        .iter()
        .find_map(|intent| match &intent.kind {
            SendKind::Replication {
                operation,
                original_epoch,
                sending_epoch,
                completed_hint,
            } => Some((
                operation.clone(),
                *original_epoch,
                *sending_epoch,
                *completed_hint,
            )),
            _ => None,
        })
        .unwrap();
    assert_eq!(original_epoch, epoch(2));
    let message = outbound.envelope(wire::envelope::Body::ReplicationOperation(
        wire::ReplicationOperation {
            metadata: Some(wire::Metadata {
                sequence: operation.lsn(),
                operation_type: 1,
                atomic_group: -1,
            }),
            original_epoch: Some(wire::Epoch {
                data_loss: original_epoch.data_loss_number,
                configuration: original_epoch.configuration_number,
            }),
            sending_epoch: Some(wire::Epoch {
                data_loss: sending_epoch.data_loss_number,
                configuration: sending_epoch.configuration_number,
            }),
            buffers: Some(wire::Buffers {
                segments: vec![operation.data().clone()],
            }),
            first_sequence: operation.lsn(),
            last_sequence: operation.lsn(),
            last_in_batch: operation.lsn(),
            buffer_counts: vec![1],
            completed_hint: hint,
        },
    ));
    assert_eq!(registry.dispatch(&message).unwrap(), Disposition::Processed);
    let replicated = take(&mut replication).await;
    ack_replication(&secondary, &replicated);
    registry.dispatch(&request).unwrap();
    assert_eq!(write.completion.await.unwrap(), Ok(()));
    assert!(
        restarted
            .sent
            .lock()
            .unwrap()
            .iter()
            .all(|intent| match &intent.kind {
                SendKind::Replication { operation, .. } => operation.lsn() >= 8,
                _ => true,
            })
    );
}
