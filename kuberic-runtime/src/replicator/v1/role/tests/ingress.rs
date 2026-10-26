// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-owner vectors derived from the production sources cited in ingress.rs.

use super::*;
use crate::replicator::v1::ingress::{Disposition, DropReason, IngressError};
use crate::replicator::v1::transport::{
    Carrier, CarrierLimits, Delivery, ExchangeService, OperationMetadata, ProtocolError,
    SendIntentAdapter, Terminal, connect,
};

fn inbound(body: wire::envelope::Body) -> wire::Envelope {
    route(10).reversed().envelope(body)
}

fn start() -> wire::Envelope {
    inbound(wire::envelope::Body::StartCopy(wire::StartCopy {
        epoch: Some(wire_epoch(1)),
        replica: 20,
        replication_start: 1,
    }))
}

fn wire_epoch(n: i64) -> wire::Epoch {
    wire::Epoch {
        data_loss: 1,
        configuration: n,
    }
}

fn request() -> wire::Envelope {
    inbound(wire::envelope::Body::RequestAck(wire::RequestAck {}))
}

fn fault(reason: &str) -> wire::Envelope {
    inbound(wire::envelope::Body::InduceFault(wire::InduceFault {
        target_replica: 20,
        target_incarnation: route(10).source.incarnation,
        reason: reason.into(),
    }))
}

fn replication(first: Lsn, sending: i64, items: &[&[u8]]) -> wire::Envelope {
    inbound(wire::envelope::Body::ReplicationOperation(
        wire::ReplicationOperation {
            metadata: Some(wire::Metadata {
                sequence: first,
                operation_type: 1,
                atomic_group: -1,
            }),
            original_epoch: Some(wire_epoch(1)),
            sending_epoch: Some(wire_epoch(sending)),
            buffers: Some(wire::Buffers {
                segments: items
                    .iter()
                    .map(|data| Bytes::copy_from_slice(data))
                    .collect(),
            }),
            first_sequence: first,
            last_sequence: first + items.len() as i64 - 1,
            last_in_batch: first + items.len() as i64 - 1,
            buffer_counts: vec![1; items.len()],
            completed_hint: 0,
        },
    ))
}

fn copy(n: Lsn, sending: i64, last: bool) -> wire::Envelope {
    inbound(wire::envelope::Body::CopyOperation(wire::CopyOperation {
        metadata: Some(wire::Metadata {
            sequence: n,
            operation_type: 1,
            atomic_group: -1,
        }),
        epoch: Some(wire_epoch(sending)),
        buffers: Some(wire::Buffers {
            segments: if last {
                vec![]
            } else {
                vec![Bytes::from_static(b"copy")]
            },
        }),
        replica: 20,
        is_last: last,
    }))
}

fn ack(id: i64, n: Lsn) -> wire::Envelope {
    route(id)
        .reversed()
        .envelope(wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
            replication_received: n,
            replication_applied: n,
            copy_received: -1,
            copy_applied: -1,
            error: 0,
        }))
}

#[tokio::test]
async fn role_gate_and_local_identity_are_shared_by_retained_ingress_handles() {
    let mut rig = Rig::new(false, false);
    let ingress = rig.owner.ingress();
    assert_eq!(
        ingress.dispatch(&start()).unwrap(),
        Disposition::Dropped(DropReason::Role)
    );
    let secondary = rig.secondary();
    assert_eq!(
        ingress.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::NotInitialized)
    );
    let mut wrong = start();
    wrong.destination.as_mut().unwrap().incarnation = Bytes::from(vec![9; 16]);
    assert!(matches!(
        ingress.dispatch(&wrong),
        Err(IngressError::Protocol(ProtocolError::WrongDestination))
    ));
    let mut wrong_partition = start();
    wrong_partition.source.as_mut().unwrap().partition = Bytes::from(vec![9; 16]);
    assert!(matches!(
        ingress.dispatch(&wrong_partition),
        Err(IngressError::Protocol(ProtocolError::WrongSource))
    ));
    assert_eq!(ingress.dispatch(&start()).unwrap(), Disposition::Processed);
    assert_eq!(
        ingress.dispatch(&ack(10, 0)).unwrap(),
        Disposition::Dropped(DropReason::Role)
    );
    rig.owner.begin_change_role(Role::None, epoch(2)).unwrap();
    assert_eq!(
        ingress.dispatch(&fault("closing")).unwrap(),
        Disposition::Dropped(DropReason::Role)
    );
    rig.owner.run().await.unwrap();
    assert!(secondary.reported_fault().unwrap().is_none());
    assert_eq!(
        ingress.dispatch(&start()).unwrap(),
        Disposition::Dropped(DropReason::Role)
    );
}

#[tokio::test]
async fn batches_drive_real_required_ack_stream_and_copy_terminal() {
    let mut rig = Rig::new(true, true);
    let secondary = rig.secondary();
    let ingress = rig.owner.ingress();
    ingress.dispatch(&start()).unwrap();
    let mut stream = secondary.replication_stream().unwrap();
    assert_eq!(
        ingress.dispatch(&replication(1, 1, &[b"a", b"b"])).unwrap(),
        Disposition::Processed
    );
    let first = take(&mut stream).await;
    let second = take(&mut stream).await;
    assert_eq!(first.queue_operation().data(), &Bytes::from_static(b"a"));
    assert_eq!(second.queue_operation().lsn(), 2);
    assert_eq!(first.epoch(), epoch(1));
    assert_eq!(
        first.ack_state(),
        crate::replicator::v1::received_operation::AckState::NotCalled
    );
    ack_replication(&secondary, &first);
    ack_replication(&secondary, &second);
    let mut copy_stream = secondary.copy_stream().unwrap();
    ingress.dispatch(&copy(1, 1, false)).unwrap();
    ingress.dispatch(&copy(2, 1, true)).unwrap();
    let data = take(&mut copy_stream).await;
    let eos = take(&mut copy_stream).await;
    assert_eq!(data.queue_operation().data(), &Bytes::from_static(b"copy"));
    assert!(eos.is_end_of_stream());
    assert_eq!(eos.queue_operation().lsn(), 2);
    ack_copy(&secondary, &data);
    ack_copy(&secondary, &eos);
    rig.owner.abort().unwrap();
    assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
}

#[tokio::test]
async fn malformed_batch_cannot_admit_prefix_and_framed_data_keeps_logical_size() {
    let mut rig = Rig::new(false, false);
    rig.owner.config.secondary.context.buffer_policy = BufferPolicy::FramedSegments;
    let secondary = rig.secondary();
    let ingress = rig.owner.ingress();
    ingress.dispatch(&start()).unwrap();
    let mut stream = secondary.replication_stream().unwrap();
    let mut malformed = replication(1, 1, &[b"a", b"b"]);
    let Some(wire::envelope::Body::ReplicationOperation(body)) = malformed.body.as_mut() else {
        unreachable!()
    };
    body.buffer_counts = vec![1, 2];
    assert!(matches!(
        ingress.dispatch(&malformed),
        Err(IngressError::Protocol(_))
    ));
    assert!(stream.next().now_or_never().is_none());
    let mut unsupported = replication(1, 1, &[b"atomic"]);
    let Some(wire::envelope::Body::ReplicationOperation(body)) = unsupported.body.as_mut() else {
        unreachable!()
    };
    body.metadata.as_mut().unwrap().atomic_group = 7;
    assert!(matches!(
        ingress.dispatch(&unsupported),
        Err(IngressError::Protocol(ProtocolError::Shape(_)))
    ));
    assert!(stream.next().now_or_never().is_none());
    let mut framed = replication(1, 1, &[b"", b"abc"]);
    let Some(wire::envelope::Body::ReplicationOperation(body)) = framed.body.as_mut() else {
        unreachable!()
    };
    body.buffer_counts = vec![2];
    body.last_sequence = 1;
    body.last_in_batch = 1;
    ingress.dispatch(&framed).unwrap();
    let data = take(&mut stream).await;
    assert_eq!(data.queue_operation().logical_bytes(), 3);
    assert_eq!(
        data.queue_operation().representation(),
        BufferPolicy::FramedSegments
    );
    assert_eq!(
        crate::replicator::v1::buffers::decode(
            data.queue_operation().data().clone(),
            BufferPolicy::FramedSegments,
            limits(),
        )
        .unwrap(),
        [Bytes::new(), Bytes::from_static(b"abc")]
    );
}

#[tokio::test(start_paused = true)]
async fn replication_changes_reply_route_but_request_and_context_ack_cannot() {
    let replies = Arc::new(Mutex::new(Vec::new()));
    let captured = replies.clone();
    let mut rig = Rig::new(false, false);
    rig.owner.config.secondary_callbacks.ack = Arc::new(move |route, _, _| {
        captured.lock().unwrap().push(route);
        true
    });
    rig.secondary();
    let ingress = rig.owner.ingress();
    ingress.dispatch(&start()).unwrap();
    let mut changed = replication(1, 2, &[b"a"]);
    changed.source.as_mut().unwrap().replica = 30;
    changed.source.as_mut().unwrap().incarnation = Bytes::from(vec![8; 16]);
    changed.sender_address = "new-primary".into();
    ingress.dispatch(&changed).unwrap();
    tokio::time::advance(Duration::from_millis(10)).await;
    assert!(rig.owner.drive().now_or_never().is_none());
    assert_eq!(
        replies.lock().unwrap().last().unwrap().destination,
        *changed.source.as_ref().unwrap()
    );
    assert_eq!(
        ingress.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::WrongPrimary)
    );
    let stale_ack = inbound(wire::envelope::Body::CopyContextAck(wire::CopyContextAck {
        sequence: 1,
        error: 0,
    }));
    assert_eq!(
        ingress.dispatch(&stale_ack).unwrap(),
        Disposition::Dropped(DropReason::WrongPrimary)
    );
    assert_eq!(
        ingress.dispatch(&replication(2, 1, &[b"stale"])).unwrap(),
        Disposition::Dropped(DropReason::StaleEpoch)
    );
    let mut fresh_request = request();
    fresh_request.source = changed.source;
    fresh_request.sender_address = changed.sender_address;
    assert_eq!(
        ingress.dispatch(&fresh_request).unwrap(),
        Disposition::Processed
    );
    // Copy is tied to the original copy epoch, not the new sending epoch/route.
    assert_eq!(
        ingress.dispatch(&copy(1, 1, true)).unwrap(),
        Disposition::Processed
    );
}

#[tokio::test]
async fn targeted_fault_requires_initialization_but_not_current_primary_and_is_once_only() {
    let reports = Arc::new(Mutex::new(Vec::new()));
    let captured = reports.clone();
    let mut rig = Rig::new(false, false);
    rig.owner.config.secondary_callbacks.fault =
        Arc::new(move |error| captured.lock().unwrap().push(error));
    let secondary = rig.secondary();
    let ingress = rig.owner.ingress();
    assert_eq!(
        ingress.dispatch(&fault("early")).unwrap(),
        Disposition::Dropped(DropReason::NotInitialized)
    );
    ingress.dispatch(&start()).unwrap();
    let mut wrong = fault("wrong target");
    let Some(wire::envelope::Body::InduceFault(body)) = wrong.body.as_mut() else {
        unreachable!()
    };
    body.target_incarnation = Bytes::from(vec![9; 16]);
    assert_eq!(
        ingress.dispatch(&wrong).unwrap(),
        Disposition::Dropped(DropReason::WrongReplica)
    );
    let mut old_primary = fault("queue pressure");
    old_primary.source.as_mut().unwrap().replica = 99;
    old_primary.sender_address = "old-primary".into();
    assert_eq!(
        ingress.dispatch(&old_primary).unwrap(),
        Disposition::FaultReported
    );
    assert_eq!(
        ingress.dispatch(&old_primary).unwrap(),
        Disposition::Dropped(DropReason::Faulted)
    );
    assert_eq!(reports.lock().unwrap().len(), 1);
    assert!(
        secondary
            .reported_fault()
            .unwrap()
            .unwrap()
            .to_string()
            .contains("queue pressure")
    );
    let mut stream = secondary.replication_stream().unwrap();
    assert!(
        stream.next().now_or_never().is_none(),
        "reporting does not fabricate EOS or stream cleanup"
    );
}

#[tokio::test]
async fn future_copy_and_start_epochs_report_real_fault_without_restarting_copy() {
    for future_start in [false, true] {
        let reports = Arc::new(AtomicUsize::new(0));
        let captured = reports.clone();
        let mut rig = Rig::new(false, false);
        rig.owner.config.secondary_callbacks.fault = Arc::new(move |_| {
            captured.fetch_add(1, Ordering::SeqCst);
        });
        let secondary = rig.secondary();
        let ingress = rig.owner.ingress();
        ingress.dispatch(&start()).unwrap();
        let mut future = if future_start {
            start()
        } else {
            copy(1, 2, false)
        };
        if let Some(wire::envelope::Body::StartCopy(body)) = future.body.as_mut() {
            body.epoch = Some(wire_epoch(2));
        }
        assert_eq!(
            ingress.dispatch(&future).unwrap(),
            Disposition::FaultReported
        );
        assert!(secondary.reported_fault().unwrap().is_some());
        assert_eq!(reports.load(Ordering::SeqCst), 1);
        assert_eq!(
            ingress.dispatch(&start()).unwrap(),
            Disposition::Dropped(DropReason::Faulted)
        );
    }
}

#[tokio::test]
async fn fault_callback_can_reenter_ingress_and_actual_secondary_close() {
    let slot = Arc::new(Mutex::new(None::<(MessageIngress, SecondaryHandle)>));
    let captured = slot.clone();
    let mut rig = Rig::new(false, false);
    rig.owner.config.secondary_callbacks.fault = Arc::new(move |_| {
        let (ingress, secondary) = captured.lock().unwrap().as_ref().unwrap().clone();
        assert_eq!(
            ingress.dispatch(&fault("duplicate")).unwrap(),
            Disposition::Dropped(DropReason::Faulted)
        );
        secondary.close().unwrap();
    });
    let secondary = rig.secondary();
    let ingress = rig.owner.ingress();
    *slot.lock().unwrap() = Some((ingress.clone(), secondary));
    ingress.dispatch(&start()).unwrap();
    assert_eq!(
        ingress.dispatch(&fault("first")).unwrap(),
        Disposition::FaultReported
    );
    assert!(
        rig.owner.drive().await.is_err(),
        "child shutdown remains an explicit role fault"
    );
}

#[tokio::test]
async fn primary_is_admitted_during_external_epoch_but_not_initial_progress_or_data_loss() {
    let provider = Arc::new(Provider {
        epochs_gate: Semaphore::new(0),
        progress_gate: Semaphore::new(0),
        ..Provider::new()
    });
    let mut rig = Rig::with_provider(false, false, provider, None);
    let ingress = rig.owner.ingress();
    rig.owner
        .begin_change_role(Role::Primary, epoch(1))
        .unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    assert_eq!(
        ingress.dispatch(&ack(10, 0)).unwrap(),
        Disposition::Dropped(DropReason::Role)
    );
    rig.provider.progress_gate.add_permits(1);
    assert!(rig.owner.run().now_or_never().is_none());
    // Native has entered Primary before the provider epoch callback completes.
    assert!(matches!(
        ingress.dispatch(&ack(10, 0)),
        Err(IngressError::Primary(PrimaryError::UnknownSession(10)))
    ));
    rig.provider.epochs_gate.add_permits(1);
    rig.owner.run().await.unwrap();
    let primary = rig.owner.primary().unwrap();
    configuration(&primary, 2, &[(10, 0)]);
    let write = enqueue(&primary);
    assert_eq!(
        ingress.dispatch(&ack(10, write.lsn)).unwrap(),
        Disposition::Processed
    );
    assert_eq!(write.completion.await.unwrap(), Ok(()));
    rig.owner.begin_data_loss().unwrap();
    assert_eq!(
        ingress.dispatch(&ack(10, 1)).unwrap(),
        Disposition::Dropped(DropReason::Role)
    );
    rig.owner.run().await.unwrap();
    assert_eq!(
        ingress.dispatch(&ack(10, 1)).unwrap(),
        Disposition::Processed
    );
}

#[tokio::test]
async fn idle_activation_keeps_ingress_open_until_actual_copy_dispatch_drains() {
    let mut rig = Rig::new(false, false);
    let secondary = rig.secondary();
    let ingress = rig.owner.ingress();
    ingress.dispatch(&start()).unwrap();
    let mut copy_stream = secondary.copy_stream().unwrap();
    ingress.dispatch(&copy(1, 1, false)).unwrap();
    ingress.dispatch(&copy(2, 1, true)).unwrap();
    rig.owner
        .begin_change_role(Role::ActiveSecondary, epoch(2))
        .unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    assert_eq!(
        ingress.dispatch(&request()).unwrap(),
        Disposition::Processed
    );
    take(&mut copy_stream).await;
    rig.owner.run().await.unwrap();
    assert_eq!(rig.owner.status(), RoleStatus::Ready(Role::ActiveSecondary));
}

#[tokio::test]
async fn direct_primary_close_accepts_final_quorum_ack_and_then_closes_ingress() {
    let mut rig = Rig::new(false, false);
    let primary = rig.primary().await;
    configuration(&primary, 2, &[(10, 0)]);
    let write = enqueue(&primary);
    let ingress = rig.owner.ingress();
    rig.owner.begin_close(Duration::from_secs(30)).unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    assert_eq!(
        ingress.dispatch(&ack(10, write.lsn)).unwrap(),
        Disposition::Processed
    );
    assert_eq!(write.completion.await.unwrap(), Ok(()));
    rig.owner.run().await.unwrap();
    assert_eq!(rig.owner.status(), RoleStatus::Closed);
    assert_eq!(
        ingress.dispatch(&ack(10, 1)).unwrap(),
        Disposition::Dropped(DropReason::Role)
    );
}

#[tokio::test]
async fn abort_seals_retained_ingress_immediately_without_fabricating_a_role_end() {
    let mut rig = Rig::new(false, false);
    rig.secondary();
    let ingress = rig.owner.ingress();
    ingress.dispatch(&start()).unwrap();
    rig.owner.abort().unwrap();
    assert_eq!(
        ingress.dispatch(&fault("after abort")).unwrap(),
        Disposition::Dropped(DropReason::Role)
    );
    assert_eq!(rig.owner.status(), RoleStatus::Aborting);
    assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
}

#[tokio::test]
async fn promotion_and_demotion_seal_old_roles_then_publish_real_new_receiver() {
    let mut rig = Rig::new(true, false);
    let primary = rig.primary().await;
    configuration(&primary, 1, &[]);
    let ingress = rig.owner.ingress();
    rig.owner
        .begin_change_role(Role::ActiveSecondary, epoch(2))
        .unwrap();
    assert_eq!(
        ingress.dispatch(&ack(10, 0)).unwrap(),
        Disposition::Dropped(DropReason::Role)
    );
    rig.owner.run().await.unwrap();
    let secondary = rig.owner.secondary().unwrap();
    let mut stream = secondary.replication_stream().unwrap();
    // Demotion inherits initialized queue/copy epoch; it must not require StartCopy.
    let mut data = replication(1, 2, &[b"new"]);
    let Some(wire::envelope::Body::ReplicationOperation(body)) = data.body.as_mut() else {
        unreachable!()
    };
    body.original_epoch = Some(wire_epoch(2));
    assert_eq!(ingress.dispatch(&data).unwrap(), Disposition::Processed);
    let data = take(&mut stream).await;
    rig.owner
        .begin_change_role(Role::Primary, epoch(3))
        .unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    assert_eq!(
        ingress.dispatch(&fault("old role")).unwrap(),
        Disposition::Dropped(DropReason::Role)
    );
    ack_replication(&secondary, &data);
    rig.owner.run().await.unwrap();
    assert_eq!(
        ingress.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::Role)
    );
}

#[tokio::test]
async fn secondary_direct_close_and_reported_primary_fault_reject_fresh_messages() {
    let mut rig = Rig::new(false, false);
    rig.secondary();
    let ingress = rig.owner.ingress();
    ingress.dispatch(&start()).unwrap();
    rig.owner.begin_close(Duration::ZERO).unwrap();
    assert_eq!(
        ingress.dispatch(&request()).unwrap(),
        Disposition::Dropped(DropReason::Role)
    );
    rig.owner.run().await.unwrap();
    assert_eq!(rig.owner.status(), RoleStatus::Closed);

    let mut rig = Rig::new(false, false);
    let primary = rig.primary().await;
    configuration(&primary, 2, &[(10, 0)]);
    let ingress = rig.owner.ingress();
    primary.report_provider_fault(Arc::new(RuntimeError::Application(
        "provider failed".into(),
    )));
    assert_eq!(
        ingress.dispatch(&ack(10, 0)).unwrap(),
        Disposition::Dropped(DropReason::Faulted)
    );
}

#[tokio::test]
async fn primary_context_dispatch_and_current_secondary_context_error_use_actual_owners() {
    use crate::replicator::v1::copy_context_receiver::CopyContextConfig;
    let mut rig = Rig::new(true, false);
    let primary = rig.primary().await;
    configuration(&primary, 1, &[]);
    let build = primary
        .begin_build(
            BuildConfig {
                route: route(10),
                remote_incarnation: 10,
                copy: CopyConfig {
                    sender: sender(),
                    buffer_policy: BufferPolicy::RawSingle,
                    buffer_limits: limits(),
                },
                context: Some(CopyContextConfig {
                    initial_queue_size: 2,
                    max_queue_size: 16,
                    batch_ack_interval: Duration::from_millis(10),
                    retry_interval: Duration::from_secs(1),
                    max_pending_acknowledgements: 4,
                    trace_interval: Duration::ZERO,
                }),
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
    let context = inbound(wire::envelope::Body::CopyContextOperation(
        wire::CopyContextOperation {
            metadata: Some(wire::Metadata {
                sequence: 1,
                operation_type: 2,
                atomic_group: -1,
            }),
            buffers: Some(wire::Buffers { segments: vec![] }),
            is_last: true,
        },
    ));
    assert_eq!(
        rig.owner.ingress().dispatch(&context).unwrap(),
        Disposition::Processed
    );
    assert!(rig.owner.drive().now_or_never().is_none());
    assert_eq!(rig.provider.copy_calls.load(Ordering::SeqCst), 1);
    assert!(
        build.result().is_none(),
        "context receipt alone is not Build completion"
    );
    rig.owner.abort().unwrap();
    assert!(matches!(rig.owner.run().await, Err(RoleError::Aborted)));
    rig.owner.drain_retired_builds().await.unwrap();

    let mut target = Rig::new(true, false);
    let secondary = target.secondary();
    let ingress = target.owner.ingress();
    ingress.dispatch(&start()).unwrap();
    assert!(target.owner.drive().now_or_never().is_none());
    let context_error = inbound(wire::envelope::Body::CopyContextAck(wire::CopyContextAck {
        sequence: 1,
        error: -7,
    }));
    assert_eq!(
        ingress.dispatch(&context_error).unwrap(),
        Disposition::Processed
    );
    assert!(
        secondary.reported_fault().unwrap().is_none(),
        "remote context error is not a provider fault"
    );
    target.owner.abort().unwrap();
    assert!(matches!(target.owner.run().await, Err(RoleError::Aborted)));
}

#[tokio::test(start_paused = true)]
async fn context_retry_uses_the_current_reply_identity_not_its_acquisition_route() {
    let sends = Arc::new(Mutex::new(Vec::new()));
    let captured = sends.clone();
    let mut rig = Rig::new(true, false);
    rig.owner.config.secondary_callbacks.context = Arc::new(move |route, _| {
        captured.lock().unwrap().push(route);
        true
    });
    rig.secondary();
    let ingress = rig.owner.ingress();
    ingress.dispatch(&start()).unwrap();
    let mut driving = Box::pin(rig.owner.drive());
    assert!(driving.as_mut().now_or_never().is_none());
    drop(driving);
    assert_eq!(
        sends.lock().unwrap().last().unwrap().destination.replica,
        10
    );
    let mut changed = replication(1, 2, &[b"new"]);
    changed.source.as_mut().unwrap().replica = 30;
    changed.sender_address = "replacement-primary".into();
    ingress.dispatch(&changed).unwrap();
    let mut driving = Box::pin(rig.owner.drive());
    assert!(driving.as_mut().now_or_never().is_none());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(driving.as_mut().now_or_never().is_none());
    drop(driving);
    let sent = sends.lock().unwrap();
    assert_eq!(
        sent.last().unwrap().destination,
        *changed.source.as_ref().unwrap()
    );
    assert_eq!(sent.last().unwrap().remote_address, changed.sender_address);
}

async fn deliver_until_fault(
    messages: &mut crate::replicator::v1::transport::Ingress,
    target: &MessageIngress,
) -> Disposition {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let Delivery::Message { envelope, .. } = messages.recv().await.unwrap() else {
                panic!("carrier rejected owner send")
            };
            let is_fault = matches!(envelope.body, Some(wire::envelope::Body::InduceFault(_)));
            let result = target.dispatch(&envelope).unwrap();
            if is_fault {
                return result;
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn actual_primary_pressure_notification_crosses_tonic_into_target_fault_owner() {
    let outbound = route(10);
    let carrier_limits = CarrierLimits {
        outbound_count: 64,
        outbound_bytes: 65536,
        inbound_count: 64,
        inbound_bytes: 65536,
        max_wire_bytes: 4096,
        buffers: limits(),
    };
    let (client, _client_ingress) = Carrier::new(outbound.clone(), carrier_limits).unwrap();
    let (server, mut messages) = Carrier::new(outbound.reversed(), carrier_limits).unwrap();
    let (binding, _) = server.bind().unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    let accepted = stream::unfold(tcp, |listener| async move {
        Some((listener.accept().await.map(|(socket, _)| socket), listener))
    });
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let listener = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(ExchangeService::new(binding).into_service(carrier_limits.max_wire_bytes))
            .serve_with_incoming_shutdown(accepted, async {
                let _ = stopped.await;
            }),
    );
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let (binding, _) = client.bind().unwrap();
    let driver = tokio::spawn(connect(binding, channel).await.unwrap().run());
    let reports = Arc::new(Mutex::new(Vec::new()));
    let captured = reports.clone();
    let mut config = Rig::new(true, false).owner.config;
    config.primary.endpoint = outbound.destination.clone();
    config.primary.address = outbound.remote_address.clone();
    config.secondary_route = outbound.reversed();
    config.secondary_callbacks.fault = Arc::new(move |fault| captured.lock().unwrap().push(fault));
    let mut target = RoleReplicator::new(config, Arc::new(Provider::new()), Clock::new()).unwrap();
    target.open(&MessageRegistry::default()).unwrap();
    target
        .begin_change_role(Role::IdleSecondary, epoch(1))
        .unwrap();
    let target_handle = target.secondary().unwrap();
    let target_ingress = target.ingress();
    let initialize = outbound.envelope(wire::envelope::Body::StartCopy(wire::StartCopy {
        epoch: Some(wire_epoch(1)),
        replica: 10,
        replication_start: 1,
    }));
    target_ingress.dispatch(&initialize).unwrap();

    let errors = Arc::new(Mutex::new(Vec::new()));
    let recorded_errors = errors.clone();
    let adapter_slot = Arc::new(Mutex::new(None::<SendIntentAdapter>));
    let send_slot = adapter_slot.clone();
    let client_send = client.clone();
    let send_route = outbound.clone();
    let notifications = Arc::new(AtomicUsize::new(0));
    let notified = notifications.clone();
    let mut source = Rig::new(true, false);
    source.owner.config.primary.queue.max_capacity = 8;
    source.owner.config.primary.send = Arc::new(move |intent| {
        if intent.identity.remote_endpoint == "replica-10" {
            if matches!(intent.kind, SendKind::InduceFault { .. }) {
                notified.fetch_add(1, Ordering::SeqCst);
            }
            let mut adapter = send_slot.lock().unwrap();
            adapter
                .get_or_insert_with(|| {
                    SendIntentAdapter::new(
                        intent.identity.clone(),
                        send_route.clone(),
                        BufferPolicy::RawSingle,
                        OperationMetadata {
                            operation_type: 1,
                            atomic_group: -1,
                        },
                        client_send.clone(),
                        Arc::new({
                            let errors = recorded_errors.clone();
                            move |error| errors.lock().unwrap().push(error)
                        }),
                    )
                    .unwrap()
                })
                .send(intent)
        } else {
            true
        }
    });
    let primary = source.primary().await;
    let sessions: Vec<_> = [10, 30]
        .into_iter()
        .map(|id| {
            let route = route(id);
            let session = crate::replicator::v1::session::ReplicationSession::new(
                crate::replicator::v1::session::SessionIdentity {
                    primary_endpoint: route.sender_address,
                    remote_endpoint: route.remote_address,
                    remote_incarnation: id,
                },
                epoch(1),
                0,
                sender(),
                source.owner.config.primary.send.clone(),
                source.owner.clock.now(),
            );
            session.open();
            session
        })
        .collect();
    primary
        .configure_established(
            crate::replicator::v1::replica_manager::EstablishedConfiguration {
                current: crate::replicator::v1::replica_manager::ReplicaSet {
                    write_quorum: 2,
                    members: [10, 30]
                        .into_iter()
                        .zip(&sessions)
                        .map(|(replica_id, session)| {
                            crate::replicator::v1::replica_manager::SessionMember {
                                replica_id,
                                session: session.clone(),
                                must_catchup: false,
                            }
                        })
                        .collect(),
                },
                previous: None,
                catchup_configuration: false,
            },
            vec![route(10), route(30)],
        )
        .unwrap();
    tokio::time::pause();
    let _writes: Vec<_> = (0..6).map(|_| enqueue(&primary)).collect();
    source.owner.ingress().dispatch(&ack(30, 6)).unwrap();
    let mut driving = Box::pin(source.owner.drive());
    assert!(driving.as_mut().now_or_never().is_none());
    tokio::time::advance(Duration::from_secs(3)).await;
    assert!(driving.as_mut().now_or_never().is_none());
    drop(driving);
    assert!(
        sessions[0].averages().0 > sessions[1].averages().0,
        "actual slow/fast receive averages: {:?} / {:?}",
        sessions[0].averages(),
        sessions[1].averages()
    );
    let mut policy = crate::replicator::v1::replica_manager::QueuePolicyConfig::for_persisted(true);
    policy.active_min_age = Duration::from_secs(2);
    policy.active_usage_percent = 80;
    primary.update_queue_policy(policy).unwrap();
    let _pressure = enqueue(&primary);
    assert!(
        sessions[0].active_faulted(),
        "actual manager must fault the slow active session"
    );
    let mut driving = Box::pin(source.owner.drive());
    assert!(driving.as_mut().now_or_never().is_none());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(driving.as_mut().now_or_never().is_none());
    drop(driving);
    tokio::time::resume();
    assert!(
        notifications.load(Ordering::SeqCst) > 0,
        "real primary policy must select a fault notification"
    );
    assert!(errors.lock().unwrap().is_empty());
    assert_eq!(
        deliver_until_fault(&mut messages, &target_ingress).await,
        Disposition::FaultReported
    );
    let delivered = notifications.load(Ordering::SeqCst);
    tokio::time::pause();
    let mut driving = Box::pin(source.owner.drive());
    assert!(driving.as_mut().now_or_never().is_none());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(driving.as_mut().now_or_never().is_none());
    drop(driving);
    tokio::time::resume();
    assert!(
        notifications.load(Ordering::SeqCst) > delivered,
        "accepted delivery does not cancel native fault retries"
    );
    assert_eq!(
        deliver_until_fault(&mut messages, &target_ingress).await,
        Disposition::Dropped(DropReason::Faulted)
    );
    assert_eq!(
        sessions[0].receive_lsn(0),
        0,
        "notification receipt is not a replication ACK"
    );
    assert_eq!(reports.lock().unwrap().len(), 1);
    assert!(target_handle.reported_fault().unwrap().is_some());
    assert!(errors.lock().unwrap().is_empty());
    source.owner.abort().unwrap();
    target.abort().unwrap();
    assert!(matches!(source.owner.run().await, Err(RoleError::Aborted)));
    assert!(matches!(target.run().await, Err(RoleError::Aborted)));
    client.shutdown();
    server.shutdown();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), driver)
            .await
            .unwrap()
            .unwrap(),
        Terminal::Shutdown
    );
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), listener)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
