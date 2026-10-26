// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.

use super::*;

#[tokio::test]
async fn m3_native_joint_replacement_cannot_finish_held_client_work_behind_public_access_fence() {
    let mut node = lone();
    node.primary().await;
    node.adapter
        .update_current_configuration(configuration(1, 2, &[(2, 0)]))
        .unwrap();
    let primary = node.adapter.owner.primary().unwrap();
    let mut held = node
        .service
        .begin_replicate(Bytes::from_static(b"held"))
        .unwrap();
    node.access.store(1, Ordering::SeqCst);
    assert!(matches!(
        node.service.begin_replicate(Bytes::from_static(b"denied")),
        Err(AdapterError::WriteAccess(
            WriteAccess::ReconfigurationPending
        ))
    ));
    let predecessor = configuration(1, 2, &[(2, 0)]).replicas.remove(0);
    let mut config = node.build_parts(2, false).0;
    config.route.destination.incarnation = Bytes::from(vec![22; 16]);
    config.route.remote_address = "fresh-2".into();
    config.remote_incarnation = 22;
    let successor = ReplicaDescriptor {
        route: config.route.clone(),
        remote_incarnation: config.remote_incarnation,
        progress: None,
        must_catchup: false,
    };
    let copied = Arc::new(Mutex::new(Vec::new()));
    let observed = copied.clone();
    let build = primary
        .begin_replacement(
            predecessor.clone(),
            config.clone(),
            node.provider.clone(),
            BuildCallbacks {
                fence: None,
                replication: Arc::new(|_| true),
                copy: Arc::new(move |action| {
                    if let CopySendAction::Operation { operation, is_last } = action {
                        observed
                            .lock()
                            .unwrap()
                            .push((operation.data().clone(), is_last));
                    }
                    true
                }),
                context_ack: Arc::new(|_, _| true),
            },
        )
        .unwrap();
    let ack = |applied, copy| {
        config
            .route
            .reversed()
            .envelope(wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
                replication_received: applied,
                replication_applied: applied,
                copy_received: copy,
                copy_applied: copy,
                error: 0,
            }))
    };
    primary.receive_ack(&ack(0, 0)).unwrap();
    assert!(node.adapter.drive().now_or_never().is_none());
    assert_eq!(node.provider.copy_calls.load(Ordering::SeqCst), 1);
    assert!(
        copied
            .lock()
            .unwrap()
            .iter()
            .any(|(data, last)| { data == &Bytes::from_static(b"snapshot") && !*last })
    );
    assert!(copied.lock().unwrap().last().unwrap().1);
    primary.receive_ack(&ack(1, 2)).unwrap();
    assert!(node.adapter.drive().now_or_never().is_none());
    assert!(build.result().unwrap().result.is_ok());
    assert!(!build.session().is_promoted());
    assert!(held.completion.wait().now_or_never().is_none());
    let cc = || ReplicaConfiguration {
        write_quorum: 2,
        replicas: vec![successor.clone()],
    };
    primary
        .update_catchup_configuration(
            Some(ReplicaConfiguration {
                write_quorum: 2,
                replicas: vec![predecessor],
            }),
            cc(),
        )
        .unwrap();
    let mut catchup = primary.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    assert_eq!(catchup.try_recv(), Ok(Ok(())));
    primary.receive_ack(&ack(1, -1)).unwrap();
    assert!(held.completion.wait().now_or_never().is_none());
    assert_eq!(primary.current_progress().unwrap(), 0);
    assert!(matches!(
        node.service.begin_replicate(Bytes::new()),
        Err(AdapterError::WriteAccess(
            WriteAccess::ReconfigurationPending
        ))
    ));
    primary.update_current_configuration(cc()).unwrap();
    held.completion.wait().await.unwrap();
    assert_eq!(primary.current_progress().unwrap(), 1);
    assert_eq!(node.access.load(Ordering::SeqCst), 1);
    node.adapter.close(Duration::ZERO).await.unwrap();
    node.adapter.drain_retired_builds().await.unwrap();
}
