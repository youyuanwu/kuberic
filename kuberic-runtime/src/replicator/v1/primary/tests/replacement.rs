// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.

use super::*;

fn descriptor(config: &BuildConfig) -> ReplicaDescriptor {
    ReplicaDescriptor {
        route: config.route.clone(),
        remote_incarnation: config.remote_incarnation,
        progress: None,
        must_catchup: false,
    }
}

fn old() -> ReplicaDescriptor {
    descriptor(&build_config(20, false))
}

fn fresh(persisted: bool) -> BuildConfig {
    let mut config = build_config(20, persisted);
    config.route.destination.incarnation = Bytes::from(vec![9; 16]);
    config.route.remote_address = "fresh-20".into();
    config.remote_incarnation = 120;
    config
}

fn set(member: ReplicaDescriptor) -> ReplicaConfiguration {
    ReplicaConfiguration {
        write_quorum: 2,
        replicas: vec![member],
    }
}

fn ack_on(route: &Route, replication: Lsn, copy: Lsn) -> wire::Envelope {
    route
        .reversed()
        .envelope(ack(20, replication, replication, copy).body.unwrap())
}

fn joint(rig: &Rig, fresh: &BuildConfig) -> Result<Vec<i64>, PrimaryError> {
    rig.handle
        .update_catchup_configuration(Some(set(old())), set(descriptor(fresh)))
}

fn finish(rig: &mut Rig, build: &BuildIdleHandle, fresh: &BuildConfig, applied: Lsn) {
    rig.handle.receive_ack(&ack_on(&fresh.route, 0, 0)).unwrap();
    rig.pending();
    rig.handle
        .receive_ack(&ack_on(&fresh.route, applied, 2))
        .unwrap();
    rig.pending();
    assert!(build.result().unwrap().result.is_ok());
}

fn configurations(rig: &Rig, old_root: &ReplicationSession, fresh_root: &ReplicationSession) {
    let manager = rig.primary.root.manager.lock().unwrap();
    let cc = manager.current_root(20).unwrap();
    assert!(cc.same_instance(fresh_root));
    let roots = manager.session_roots();
    assert_eq!(roots.len(), 2);
    assert!(roots.iter().any(|(_, root)| root.same_instance(old_root)));
    let (cc, pc) = manager.configuration_roots();
    let cc = cc.unwrap();
    let pc = pc.unwrap();
    assert_eq!(cc.write_quorum, 2);
    assert_eq!(pc.write_quorum, 2);
    assert_eq!(cc.members.len(), 1);
    assert_eq!(pc.members.len(), 1);
    assert!(pc.members[0].session.same_instance(old_root));
}

#[tokio::test(start_paused = true)]
async fn m3_fresh_copy_joint_cc_catchup_preserves_old_pending_promise_and_original_quorum() {
    let mut rig = Rig::new();
    let predecessor = rig.active(&[20], 2).remove(0);
    let mut prefix = rig.enqueue();
    rig.handle.receive_ack(&ack(20, 1, 1, -1)).unwrap();
    assert_eq!(prefix.completion.try_recv(), Ok(Ok(())));
    let mut held = rig.enqueue();
    let config = fresh(true);
    let provider = Arc::new(Provider::new(1));
    let copied = Arc::new(Mutex::new(Vec::new()));
    let received = copied.clone();
    let mut hooks = callbacks(&rig.sent);
    hooks.copy = Arc::new(move |action| {
        if let CopySendAction::Operation { operation, is_last } = action {
            received
                .lock()
                .unwrap()
                .push((operation.data().clone(), is_last));
        }
        true
    });
    let build = rig
        .handle
        .begin_replacement(old(), config.clone(), provider.clone(), hooks)
        .unwrap();
    let successor = build.session();
    assert!(!successor.same_instance(&predecessor));
    assert_eq!(successor.progress(0).applied, 0);
    assert!(!successor.is_promoted());
    assert!(!predecessor.transport_active());
    let health = rig.handle.current_transport_sessions().unwrap().unwrap();
    assert_eq!(health.write_quorum, 2);
    assert_eq!(health.members.len(), 1);
    assert_eq!(health.members[0].1, predecessor.identity());
    assert!(
        !health.members[0].2,
        "fresh nonvoting Build cannot heal CC transport health"
    );
    assert!(
        matches!(
            rig.sent
                .lock()
                .unwrap()
                .replication
                .iter()
                .find(|intent| intent.identity.remote_incarnation == config.remote_incarnation)
                .map(|intent| &intent.kind),
            Some(SendKind::StartCopy { .. })
        ),
        "the fresh native receiver must initialize before retained replication is dispatched"
    );
    assert!(
        rig.sent.lock().unwrap().replication.iter().any(|intent| {
            intent.identity.remote_incarnation == config.remote_incarnation
                && matches!(&intent.kind, SendKind::Replication {
                operation, original_epoch, ..
            } if operation.lsn() == held.lsn && *original_epoch == Epoch::new(1, 7))
        }),
        "the fresh sender owns actual retained pending replay"
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
    assert!(
        rig.handle
            .update_current_configuration(set(descriptor(&config)))
            .is_err()
    );
    assert!(
        joint(&rig, &config).is_err(),
        "unfinished copy cannot enter CC"
    );
    let mut item = context(20);
    if let Some(wire::envelope::Body::CopyContextOperation(body)) = item.body.as_mut() {
        body.is_last = false;
        body.buffers.as_mut().unwrap().segments = vec![Bytes::from_static(b"context")];
    }
    rig.handle
        .receive_context(&config.route.reversed().envelope(item.body.unwrap()))
        .unwrap();
    let mut eos = context(20);
    if let Some(wire::envelope::Body::CopyContextOperation(body)) = eos.body.as_mut() {
        body.metadata.as_mut().unwrap().sequence = 2;
    }
    rig.handle
        .receive_context(&config.route.reversed().envelope(eos.body.unwrap()))
        .unwrap();
    finish(&mut rig, &build, &config, 2);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.context_items.load(Ordering::SeqCst), 1);
    {
        let copied = copied.lock().unwrap();
        assert!(
            copied
                .iter()
                .any(|(data, last)| *data == Bytes::from_static(b"snapshot") && !*last)
        );
        assert!(copied.last().unwrap().1, "real copy EOS was emitted");
    }
    assert!(!successor.is_promoted(), "completed copy still has no vote");
    assert_eq!(held.completion.try_recv(), Err(TryRecvError::Empty));
    let mut after_ready = rig.enqueue();
    joint(&rig, &config).unwrap();
    configurations(&rig, &predecessor, &successor);
    let health = rig.handle.current_transport_sessions().unwrap().unwrap();
    assert_eq!(health.write_quorum, 2);
    assert_eq!(health.members.len(), 1);
    assert_eq!(health.members[0].1, successor.identity());
    assert!(health.members[0].2);
    assert_ne!(
        health.members[0].1,
        predecessor.identity(),
        "PC is not a second current vote"
    );
    assert!(successor.is_promoted());
    assert!(
        rig.handle
            .update_current_configuration(set(descriptor(&config)))
            .is_err(),
        "PC cannot disappear before actual cached CC catchup"
    );
    let mut wait = rig.handle.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    assert_eq!(wait.try_recv(), Err(TryRecvError::Empty));
    rig.handle
        .receive_ack(&ack_on(&config.route, 3, -1))
        .unwrap();
    assert_eq!(wait.try_recv(), Ok(Ok(())));
    assert_eq!(predecessor.progress(0).applied, 1);
    assert_eq!(held.completion.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(after_ready.completion.try_recv(), Err(TryRecvError::Empty));
    configurations(&rig, &predecessor, &successor);
    let mut tail = rig.enqueue();
    let mut all = rig.handle.begin_catchup(CatchupMode::All).unwrap();
    assert_eq!(all.try_recv(), Err(TryRecvError::Empty));
    rig.handle
        .update_current_configuration(set(descriptor(&config)))
        .unwrap();
    assert_eq!(held.completion.try_recv(), Ok(Ok(())));
    assert_eq!(after_ready.completion.try_recv(), Ok(Ok(())));
    assert_eq!(tail.completion.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(all.try_recv(), Err(TryRecvError::Empty));
    assert!(
        rig.handle.receive_ack(&ack(20, 4, 4, -1)).is_err(),
        "retired predecessor route cannot address the successor"
    );
    assert_eq!(tail.completion.try_recv(), Err(TryRecvError::Empty));
    rig.handle
        .receive_ack(&ack_on(&config.route, 4, -1))
        .unwrap();
    assert_eq!(tail.completion.try_recv(), Ok(Ok(())));
    assert_eq!(all.try_recv(), Ok(Ok(())));
    rig.handle.close(Duration::ZERO).unwrap();
    assert!(rig.primary.run().await.unwrap().result.is_ok());
    rig.primary.drain_builds().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn m3_old_ack_remove_and_exact_old_cancel_never_credit_or_cancel_the_fresh_owner() {
    let mut rig = Rig::new();
    let old_build = rig.finish_build(20);
    let predecessor = old_build.session();
    rig.configure(
        vec![SessionMember {
            replica_id: 20,
            session: predecessor.clone(),
            must_catchup: false,
        }],
        2,
    );
    let mut held = rig.enqueue();
    let config = fresh(false);
    let build = rig
        .handle
        .begin_replacement(
            old(),
            config.clone(),
            Arc::new(Provider::new(1)),
            callbacks(&rig.sent),
        )
        .unwrap();
    old_build.cancel().unwrap();
    old_build.cancel_for_removal().unwrap();
    assert!(rig.handle.receive_context(&context(20)).is_err());
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    assert!(
        rig.handle
            .remove_idle_with(20, move |_, _| {
                observed.store(true, Ordering::SeqCst);
                Ok(())
            })
            .is_err()
    );
    assert!(!called.load(Ordering::SeqCst));
    assert!(build.result().is_none());
    finish(&mut rig, &build, &config, 1);
    assert!(!build.session().is_promoted());
    assert_eq!(held.completion.try_recv(), Err(TryRecvError::Empty));
    let mut after_ready = rig.enqueue();
    joint(&rig, &config).unwrap();
    let mut wait = rig.handle.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    rig.handle.receive_ack(&ack(20, 2, 2, -1)).unwrap();
    assert_eq!(predecessor.progress(0).applied, 0);
    assert_eq!(build.session().progress(0).applied, 1);
    assert_eq!(wait.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(held.completion.try_recv(), Err(TryRecvError::Empty));
    let mut crossed = ack_on(&config.route, 2, -1);
    crossed.sender_address = old().route.remote_address;
    assert!(rig.handle.receive_ack(&crossed).is_err());
    assert_eq!(build.session().progress(0).applied, 1);
    rig.handle
        .receive_ack(&ack_on(&config.route, 2, -1))
        .unwrap();
    assert_eq!(wait.try_recv(), Ok(Ok(())));
    assert_eq!(held.completion.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(after_ready.completion.try_recv(), Err(TryRecvError::Empty));
    assert!(rig.handle.remove_idle(20).is_err());
    assert!(build.session().transport_active());
    rig.handle
        .update_current_configuration(set(descriptor(&config)))
        .unwrap();
    assert_eq!(held.completion.try_recv(), Ok(Ok(())));
    assert_eq!(after_ready.completion.try_recv(), Ok(Ok(())));
    rig.handle.close(Duration::ZERO).unwrap();
    assert!(rig.primary.run().await.unwrap().result.is_ok());
    rig.primary.drain_builds().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn m3_cold_denied_predecessor_is_not_a_completed_build_or_ack_authority() {
    let clock = Clock::new();
    let mut config = primary_config();
    config.initial_progress = 7;
    let primary = Primary::new(config, clock.clone()).unwrap();
    let handle = primary.handle();
    let mut rig = Rig {
        primary,
        handle,
        clock,
        sent: Arc::new(Mutex::new(Sent::default())),
    };
    let mut metadata = old();
    metadata.remote_incarnation = FROZEN_PREVIOUS_INCARNATION;
    rig.handle
        .restore_replacement_source(set(metadata.clone()), metadata.clone())
        .unwrap();
    assert_eq!(rig.handle.current_progress().unwrap(), 7);
    let predecessor = rig
        .primary
        .root
        .manager
        .lock()
        .unwrap()
        .current_root(20)
        .unwrap();
    assert!(!predecessor.transport_active());
    assert!(matches!(
        &rig.primary.root.state.lock().unwrap().bindings[&20].owner,
        SessionOwner::Established(_)
    ));
    assert!(rig.handle.receive_ack(&ack(20, 0, 0, -1)).is_err());
    let mut held = rig.enqueue();
    let config = fresh(false);
    let build = rig
        .handle
        .begin_replacement(
            metadata.clone(),
            config.clone(),
            Arc::new(Provider::new(1)),
            callbacks(&rig.sent),
        )
        .unwrap();
    finish(&mut rig, &build, &config, 8);
    assert_eq!(held.completion.try_recv(), Err(TryRecvError::Empty));
    let mut after_ready = rig.enqueue();
    rig.handle
        .update_catchup_configuration(Some(set(metadata)), set(descriptor(&config)))
        .unwrap();
    let mut wait = rig.handle.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    rig.handle
        .receive_ack(&ack_on(&config.route, 9, -1))
        .unwrap();
    assert_eq!(wait.try_recv(), Ok(Ok(())));
    assert_eq!(held.completion.try_recv(), Err(TryRecvError::Empty));
    assert!(rig.handle.receive_ack(&ack(20, 9, 9, -1)).is_err());
    assert_eq!(predecessor.progress(0).applied, -1);
    assert_eq!(rig.handle.current_progress().unwrap(), 7);
    rig.handle
        .update_current_configuration(set(descriptor(&config)))
        .unwrap();
    assert_eq!(held.completion.try_recv(), Ok(Ok(())));
    assert_eq!(after_ready.completion.try_recv(), Ok(Ok(())));
    assert_eq!(rig.handle.current_progress().unwrap(), 9);
    rig.handle.close(Duration::ZERO).unwrap();
    assert!(rig.primary.run().await.unwrap().result.is_ok());
    rig.primary.drain_builds().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn m3_replacement_epoch_policy_and_close_enumerate_both_actual_roots() {
    let mut rig = Rig::new();
    let predecessor = rig.active(&[20], 2).remove(0);
    let mut held = rig.enqueue();
    let config = fresh(false);
    let provider = Arc::new(Provider::new(1));
    let build = rig
        .handle
        .begin_replacement(old(), config.clone(), provider, callbacks(&rig.sent))
        .unwrap();
    rig.handle
        .receive_ack(&ack_on(&config.route, 0, 0))
        .unwrap();
    rig.pending();
    assert!(build.result().is_none());
    rig.handle.update_epoch(Epoch::new(1, 8)).unwrap();
    assert_eq!(predecessor.sending_epoch(), Epoch::new(1, 8));
    assert_eq!(build.session().sending_epoch(), Epoch::new(1, 8));
    let mut policy = QueuePolicyConfig::for_persisted(true);
    policy.restart_idle = true;
    policy.restart_active = false;
    policy.idle_usage_percent = 0;
    rig.handle.update_queue_policy(policy).unwrap();
    let mut second = rig.enqueue();
    assert_eq!(build.finish_copy_target().unwrap(), Some(2));
    assert!(build.session().idle_faulted());
    assert!(!predecessor.idle_faulted());
    assert!(!predecessor.active_faulted());
    let roots = rig.primary.root.manager.lock().unwrap().session_roots();
    assert_eq!(roots.len(), 2);
    assert!(roots[0].1.same_instance(&predecessor));
    assert!(roots[1].1.same_instance(&build.session()));
    rig.handle.close(Duration::ZERO).unwrap();
    assert_eq!(
        held.completion.try_recv(),
        Ok(Err(CompletionError::OperationCanceled))
    );
    assert_eq!(
        second.completion.try_recv(),
        Ok(Err(CompletionError::OperationCanceled))
    );
    assert!(!predecessor.transport_active());
    assert!(!build.session().transport_active());
    assert!(rig.primary.run().await.unwrap().result.is_ok());
    rig.primary.drain_builds().await.unwrap();
    assert!(build.result().unwrap().result.is_err());
}

#[tokio::test(start_paused = true)]
async fn m3_replacement_collector_retains_each_real_provider_until_its_root_leaves() {
    let mut rig = Rig::new();
    let old_provider = Arc::new(Provider::new(1));
    let old_weak = Arc::downgrade(&old_provider);
    let old_build = rig.begin(20, false, old_provider);
    rig.handle.receive_ack(&ack(20, 0, 0, 0)).unwrap();
    rig.pending();
    rig.handle.receive_ack(&ack(20, 0, 0, 2)).unwrap();
    rig.pending();
    assert!(old_build.result().unwrap().result.is_ok());
    rig.configure(
        vec![SessionMember {
            replica_id: 20,
            session: old_build.session(),
            must_catchup: false,
        }],
        2,
    );
    let config = fresh(false);
    let new_provider = Arc::new(Provider::new(1));
    let new_weak = Arc::downgrade(&new_provider);
    let build = rig
        .handle
        .begin_replacement(old(), config.clone(), new_provider, callbacks(&rig.sent))
        .unwrap();
    finish(&mut rig, &build, &config, 0);
    rig.primary.collect_builds();
    assert_eq!(rig.primary.retained.len(), 2);
    assert!(old_weak.upgrade().is_some());
    assert!(new_weak.upgrade().is_some());
    joint(&rig, &config).unwrap();
    let mut wait = rig.handle.begin_catchup(CatchupMode::WriteQuorum).unwrap();
    assert_eq!(wait.try_recv(), Ok(Ok(())));
    rig.primary.collect_builds();
    assert_eq!(rig.primary.retained.len(), 2);
    rig.handle
        .update_current_configuration(set(descriptor(&config)))
        .unwrap();
    rig.primary.collect_builds();
    assert_eq!(rig.primary.retained.len(), 1);
    assert!(old_weak.upgrade().is_none());
    assert!(new_weak.upgrade().is_some());
    rig.handle.close(Duration::ZERO).unwrap();
    assert!(rig.primary.run().await.unwrap().result.is_ok());
    rig.primary.drain_builds().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn m3_replacement_native_close_does_not_fabricate_pending_provider_drain() {
    let mut rig = Rig::new();
    let predecessor = rig.active(&[20], 2).remove(0);
    let mut held = rig.enqueue();
    let provider = Arc::new(Provider::new(0));
    let weak = Arc::downgrade(&provider);
    let config = fresh(false);
    let build = rig
        .handle
        .begin_replacement(
            old(),
            config.clone(),
            provider.clone(),
            callbacks(&rig.sent),
        )
        .unwrap();
    rig.handle
        .receive_ack(&ack_on(&config.route, 0, 0))
        .unwrap();
    rig.pending();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    rig.handle.close(Duration::ZERO).unwrap();
    assert!(rig.primary.run().await.unwrap().result.is_ok());
    assert_eq!(
        held.completion.try_recv(),
        Ok(Err(CompletionError::OperationCanceled))
    );
    assert!(!predecessor.transport_active());
    assert!(!build.session().transport_active());
    assert!(rig.primary.drain_builds().now_or_never().is_none());
    assert!(build.result().is_none());
    assert!(weak.upgrade().is_some());
    provider.gate.add_permits(1);
    drop(provider);
    rig.primary.drain_builds().await.unwrap();
    assert!(build.result().unwrap().result.is_err());
    drop(rig.primary);
    assert!(weak.upgrade().is_none());
}

#[tokio::test(start_paused = true)]
async fn m3_replacement_rejects_wrong_predecessor_prior_pc_and_parallel_membership_changes() {
    let mut rig = Rig::new();
    rig.active(&[20], 2);
    let config = fresh(false);
    let mut wrong = old();
    wrong.remote_incarnation += 1;
    assert!(
        rig.handle
            .begin_replacement(
                wrong,
                config.clone(),
                Arc::new(Provider::new(1)),
                callbacks(&rig.sent),
            )
            .is_err()
    );
    rig.handle
        .update_catchup_configuration(Some(set(old())), set(old()))
        .unwrap();
    assert!(
        rig.handle
            .begin_replacement(
                old(),
                config.clone(),
                Arc::new(Provider::new(1)),
                callbacks(&rig.sent),
            )
            .is_err()
    );
    rig.handle.update_current_configuration(set(old())).unwrap();
    let build = rig
        .handle
        .begin_replacement(
            old(),
            config.clone(),
            Arc::new(Provider::new(1)),
            callbacks(&rig.sent),
        )
        .unwrap();
    assert!(
        rig.handle
            .begin_build(
                build_config(30, false),
                Arc::new(Provider::new(1)),
                callbacks(&rig.sent),
            )
            .is_err()
    );
    assert!(
        rig.handle
            .begin_replacement(
                old(),
                config.clone(),
                Arc::new(Provider::new(1)),
                callbacks(&rig.sent),
            )
            .is_err()
    );
    finish(&mut rig, &build, &config, 0);
    let singleton = ReplicaConfiguration {
        write_quorum: 1,
        replicas: Vec::new(),
    };
    assert!(rig.handle.update_current_configuration(singleton).is_err());
    assert!(
        rig.handle
            .update_catchup_configuration(Some(set(descriptor(&config))), set(descriptor(&config)),)
            .is_err(),
        "new root cannot substitute for the old PC"
    );
    rig.handle.close(Duration::ZERO).unwrap();
    assert!(rig.primary.run().await.unwrap().result.is_ok());
    rig.primary.drain_builds().await.unwrap();
}
