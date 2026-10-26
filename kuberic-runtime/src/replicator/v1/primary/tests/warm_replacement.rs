// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.

use super::*;

#[tokio::test(start_paused = true)]
async fn m3_hot_replacement_restore_verifies_actual_roots_without_seeding_or_releasing_promises() {
    let mut rig = Rig::new();
    let predecessor = rig.active(&[20], 2).remove(0);
    let mut acknowledged = rig.enqueue();
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(acknowledged.completion.try_recv(), Ok(Ok(())));
    let config = build_config(20, false);
    let descriptor = ReplicaDescriptor {
        route: config.route.clone(),
        remote_incarnation: config.remote_incarnation,
        progress: None,
        must_catchup: false,
    };
    let original = ReplicaConfiguration {
        write_quorum: 2,
        replicas: vec![descriptor.clone()],
    };
    rig.handle
        .update_catchup_configuration(None, original.clone())
        .unwrap();
    let mut completed_wait = rig.handle.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    assert_eq!(completed_wait.try_recv(), Ok(Ok(())));
    let mut held = rig.enqueue();
    rig.handle
        .restore_replacement_source(original.clone(), descriptor.clone())
        .unwrap();
    assert_eq!(held.completion.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(predecessor.progress(0).applied, 1);
    assert!(
        rig.primary
            .root
            .manager
            .lock()
            .unwrap()
            .current_root(20)
            .unwrap()
            .same_instance(&predecessor)
    );

    let mut invented = original.clone();
    invented.replicas[0].progress = Some(held.lsn + 100);
    rig.handle
        .restore_replacement_source(invented, descriptor.clone())
        .unwrap();
    assert_eq!(predecessor.progress(0).applied, 1);
    assert_eq!(held.completion.try_recv(), Err(TryRecvError::Empty));

    let mut contracted = original.clone();
    contracted.write_quorum = 1;
    assert!(
        rig.handle
            .restore_replacement_source(contracted, descriptor.clone())
            .is_err()
    );
    let mut frozen = descriptor.clone();
    frozen.remote_incarnation = FROZEN_PREVIOUS_INCARNATION;
    assert!(
        rig.handle
            .restore_replacement_source(original.clone(), frozen)
            .is_err()
    );
    let mut crossed = original.clone();
    crossed.replicas[0].route.remote_address = "not-the-current-root".into();
    assert!(
        rig.handle
            .restore_replacement_source(crossed, descriptor.clone())
            .is_err()
    );
    let mut changed = original;
    changed.replicas[0].remote_incarnation += 1;
    assert!(
        rig.handle
            .restore_replacement_source(changed, descriptor)
            .is_err()
    );
    assert!(
        rig.primary
            .root
            .manager
            .lock()
            .unwrap()
            .current_root(20)
            .unwrap()
            .same_instance(&predecessor)
    );
    assert_eq!(held.completion.try_recv(), Err(TryRecvError::Empty));
    rig.handle
        .receive_ack(&ack(20, held.lsn, held.lsn, -1))
        .unwrap();
    assert_eq!(held.completion.try_recv(), Ok(Ok(())));
}
