// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.

use super::*;

async fn retained(config: SecondaryConfig) -> Rig {
    let provider = Arc::new(Provider {
        retained_progress: Some(7),
        epoch_gate: Semaphore::new(100),
        ..Provider::new()
    });
    let progress = Secondary::read_retained_progress(provider.clone(), Epoch::new(1, 7))
        .await
        .unwrap();
    let checkpoint = Secondary::confirm_retained_checkpoint(progress)
        .await
        .unwrap();
    let acks = Arc::new(Mutex::new(Vec::new()));
    let recorded = acks.clone();
    let context_sends = Arc::new(AtomicUsize::new(0));
    let contexts = context_sends.clone();
    let owner = Secondary::from_retained(
        route().reversed(),
        config,
        checkpoint,
        SecondaryCallbacks {
            ack: Arc::new(move |_, ack, _| {
                recorded.lock().unwrap().push(ack);
                true
            }),
            context: Arc::new(move |_, _| {
                contexts.fetch_add(1, Ordering::SeqCst);
                true
            }),
            fault: Arc::new(|_| {}),
        },
        Clock::new(),
    )
    .unwrap();
    Rig {
        handle: owner.handle(),
        owner,
        provider,
        acks,
        context_sends,
        required: config.copy.require_service_ack,
    }
}

fn fresh_route() -> Route {
    let mut fresh = route().reversed();
    fresh.destination.replica = 30;
    fresh.destination.incarnation = Bytes::from(vec![8; 16]);
    fresh.remote_address = "fresh-primary".into();
    fresh
}

#[tokio::test]
async fn retained_checkpoint_constructs_actual_empty_queue_without_copy_or_context_success() {
    let mut rig = retained(config(true, false, true)).await;
    {
        let state = rig.owner.root.state.lock().unwrap();
        assert_eq!(state.retained_bound, Some(false));
        assert!(!state.copy.is_copy_in_progress());
        assert!(!state.copy.all_operations_received());
        assert!(!state.copy.enqueued_end_of_stream());
        assert!(state.copy.all_operations_acked());
        assert!(!state.context_requested && !state.context_active && !state.context_acquiring);
        assert!(state.context_result.is_none());
    }
    {
        let receiver = rig.owner.root.replication.lock().unwrap();
        let queue = receiver.queue().unwrap();
        assert_eq!(queue.operation_count(), 0);
        assert_eq!(queue.progress().retained_head, 8);
        assert_eq!(queue.progress().complete_head, 8);
        assert_eq!(queue.progress().commit_head, 8);
        assert_eq!(queue.progress().tail, 8);
        assert_eq!(receiver.pending_ack_progress(), 0);
        assert_eq!(receiver.provider_epoch(), Some(Epoch::new(1, 7)));
    }
    assert!(!rig.owner.root.copy_dispatch.snapshot().initialized);
    assert!(rig.handle.copy_stream().unwrap().next().await.is_none());
    assert!(
        !rig.owner
            .root
            .state
            .lock()
            .unwrap()
            .copy
            .enqueued_end_of_stream()
    );
    let mut replication = rig.handle.replication_stream().unwrap();
    assert!(replication.next().now_or_never().is_none());
    assert!(rig.owner.run().now_or_never().is_none());
    assert_eq!(rig.provider.progress_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.provider.contexts.load(Ordering::SeqCst), 0);
    assert_eq!(rig.context_sends.load(Ordering::SeqCst), 0);
    assert!(rig.acks.lock().unwrap().is_empty());
    rig.handle.abort().unwrap();
    assert!(rig.owner.run().await.unwrap().aborted);
}

#[tokio::test(start_paused = true)]
async fn retained_unbound_ack_requests_and_close_cannot_emit_provider_prefix_on_old_route() {
    let mut rig = retained(config(true, false, true)).await;
    rig.owner.root.force_ack(0);
    rig.owner.root.schedule(true).unwrap();
    assert!(!rig.owner.root.send_ack(false));
    assert!(
        rig.owner
            .root
            .state
            .lock()
            .unwrap()
            .ack
            .deadline()
            .is_none()
    );
    assert!(rig.acks.lock().unwrap().is_empty());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(rig.owner.run().now_or_never().is_none());
    assert!(rig.acks.lock().unwrap().is_empty());
    assert!(matches!(
        rig.handle.process_start(&start()),
        Err(SecondaryError::RetainedCopy)
    ));
    let context_ack =
        route().envelope(wire::envelope::Body::CopyContextAck(wire::CopyContextAck {
            sequence: 0,
            error: 0,
        }));
    assert!(matches!(
        rig.handle.process_context_ack(&context_ack),
        Err(SecondaryError::RetainedCopy)
    ));
    rig.handle.close().unwrap();
    let end = rig.owner.run().await.unwrap();
    assert!(end.result.is_ok());
    assert!(rig.acks.lock().unwrap().is_empty());
    assert!(
        rig.owner
            .root
            .state
            .lock()
            .unwrap()
            .context_result
            .is_none()
    );
}

#[tokio::test]
async fn retained_close_waits_real_local_replication_eos_without_any_remote_nonce() {
    let mut rig = retained(config(true, false, true)).await;
    let mut replication = rig.handle.replication_stream().unwrap();
    rig.handle.close().unwrap();
    assert!(rig.owner.run().now_or_never().is_none());
    let eos = take(&mut replication).await;
    assert!(eos.is_end_of_stream());
    assert!(rig.owner.run().now_or_never().is_none());
    ack_replication(&rig.handle, &eos);
    let end = rig.owner.run().await.unwrap();
    assert!(end.result.is_ok() && end.streams_drained && !end.aborted);
    assert!(replication.next().await.is_none());
    assert!(rig.acks.lock().unwrap().is_empty());
    assert!(
        !rig.owner
            .root
            .state
            .lock()
            .unwrap()
            .copy
            .enqueued_end_of_stream()
    );
}

#[tokio::test]
async fn retained_binding_has_new_native_identity_and_no_initial_ack_credit() {
    let predecessor = Rig::new(config(true, false, true));
    let old_identity = predecessor.handle.transport_identity().unwrap();
    let mut rig = retained(config(true, false, true)).await;
    let fresh = fresh_route();
    assert!(matches!(
        rig.handle
            .reform_transport(&route().reversed(), &fresh, Epoch::new(1, 7), true),
        Err(SecondaryError::RetainedTransport)
    ));
    rig.handle
        .bind_retained_transport(&fresh, Epoch::new(1, 7))
        .unwrap();
    let identity = rig.handle.transport_identity().unwrap();
    assert_ne!(identity.0, old_identity.0);
    assert_eq!(identity.1, 1);
    assert!(rig.acks.lock().unwrap().is_empty());
    let mut stream = rig.handle.replication_stream().unwrap();
    assert!(matches!(
        rig.handle.receive_replication_from(
            Epoch::new(1, 7),
            vec![operation(8, 7, true)],
            None,
            Some(route().reversed()),
        ),
        Err(SecondaryError::RetainedTransport)
    ));
    rig.handle
        .receive_replication_from(
            Epoch::new(1, 7),
            vec![operation(8, 7, true)],
            None,
            Some(fresh.clone()),
        )
        .unwrap();
    let data = take(&mut stream).await;
    assert_eq!(rig.handle.current_progress().unwrap(), 8);
    assert_eq!(rig.handle.catchup_capability().unwrap(), 0);
    ack_replication(&rig.handle, &data);
    rig.owner.root.force_ack(0);
    {
        let sent = rig.acks.lock().unwrap();
        assert_eq!(sent.last().unwrap().replication.received, 8);
        assert_eq!(sent.last().unwrap().replication.applied, 8);
        assert_eq!(sent.last().unwrap().copy.received, -1);
        assert_eq!(sent.last().unwrap().copy.applied, -1);
        assert_eq!(sent.last().unwrap().transport_owner, identity.0);
        assert_eq!(sent.last().unwrap().transport_generation, identity.1);
    }
    assert_eq!(
        rig.owner
            .root
            .reply_route
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .destination,
        fresh.destination
    );
    rig.handle.abort().unwrap();
    assert!(rig.owner.run().await.unwrap().aborted);
}

#[tokio::test]
async fn retained_transport_initializer_cannot_repurpose_normal_copy_receiver() {
    let rig = Rig::new(config(true, false, true));
    rig.start();
    assert!(matches!(
        rig.handle
            .bind_retained_transport(&fresh_route(), Epoch::new(1, 7)),
        Err(SecondaryError::RetainedTransport)
    ));
    assert!(
        rig.owner
            .root
            .state
            .lock()
            .unwrap()
            .copy
            .is_copy_in_progress()
    );
}
