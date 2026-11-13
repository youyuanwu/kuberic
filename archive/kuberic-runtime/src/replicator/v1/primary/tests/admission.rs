use super::*;

#[tokio::test]
async fn assignment_lease_ends_before_blocked_reentrant_send_callback() {
    let mut rig = Rig::new();
    let gate = Arc::new(Mutex::new(()));
    let callback_gate = gate.clone();
    let handle = rig.handle.clone();
    let (entered, observed) = mpsc::channel();
    let (release, held) = mpsc::channel();
    let held = Mutex::new(held);
    let session = rig.session(
        20,
        Arc::new(move |intent| {
            if let SendKind::Replication { operation, .. } = intent.kind {
                assert!(
                    callback_gate.try_lock().is_ok(),
                    "host fence must not await callbacks"
                );
                handle
                    .receive_ack(&ack(20, operation.lsn(), operation.lsn(), -1))
                    .unwrap();
                entered.send(()).unwrap();
                held.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
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
    let handle = rig.handle.clone();
    let writer_gate = gate.clone();
    let writer = std::thread::spawn(move || {
        let lease = writer_gate.lock().unwrap();
        handle
            .enqueue_with_lease(Bytes::new(), BufferPolicy::RawSingle, limits(), lease)
            .unwrap()
    });
    observed.recv_timeout(Duration::from_secs(5)).unwrap();
    // A fence can acquire the very same lease while the foreign callback is held.
    assert!(gate.try_lock().is_ok());
    release.send(()).unwrap();
    let admission = writer.join().unwrap();
    assert_eq!(admission.completion.await.unwrap(), Ok(()));
    rig.handle.close(Duration::ZERO).unwrap();
    rig.primary.run().await.unwrap();
}
