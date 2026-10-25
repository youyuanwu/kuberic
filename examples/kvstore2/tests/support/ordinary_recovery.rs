#[path = "ordinary_cluster.rs"]
pub(super) mod ordinary_cluster;

use kuberic_runtime::protocol::public_operations::{PublicV1CallKind, ReplicaGoalStatus};
use kuberic_runtime::protocol::types::AccessStatus;
use ordinary_cluster::Fixture;

#[allow(dead_code, reason = "selected by the KV recovery integration binary")]
pub async fn singleton_restart() {
    let mut fixture = Fixture::start("m2-singleton-recovery", 1).await;
    let result = async {
        let source = fixture.replicas[0].service.clone();
        source
            .replicate_put("retained".into(), "acknowledged".into())
            .await
            .unwrap();
        let old = fixture.replicas[0].reference.clone();
        let before = fixture.snapshot(0).await;
        let fresh = fixture.cluster.request_restart(&old).await?;
        assert_eq!(fresh.storage_incarnation(), old.storage_incarnation());
        assert_ne!(fresh.process_session_id(), old.process_session_id());
        let observation = fixture.cluster.observe().await?;
        let snapshot = observation.snapshot(&fresh).unwrap();
        assert_eq!(snapshot.history, before.history);
        assert_ne!(snapshot.write_status, AccessStatus::Granted);
        assert_ne!(snapshot.read_status, AccessStatus::Granted);
        let reopened = fixture.cluster.service(&fresh).await?;
        assert!(reopened.get("retained").await.is_err());
        assert!(
            reopened
                .replicate_put("early".into(), "forbidden".into())
                .await
                .is_err()
        );
        let topology = fixture.cluster.settle_members(1).await?;
        assert_eq!(
            topology.primary().process_session_id(),
            fresh.process_session_id()
        );
        assert_eq!(
            reopened.get("retained").await.unwrap().as_deref(),
            Some("acknowledged")
        );
        reopened
            .replicate_put("after".into(), "acknowledged".into())
            .await
            .unwrap();
        assert_eq!(
            reopened.get("after").await.unwrap().as_deref(),
            Some("acknowledged")
        );
        assert!(source.get("retained").await.is_err());
        // The convenience helper also preserves the configured singleton cardinality.
        let again = fixture.cluster.restart(topology.primary()).await?;
        assert_eq!(again.storage_incarnation(), old.storage_incarnation());
        let service = fixture.cluster.service(&again).await?;
        assert_eq!(
            service.get("retained").await.unwrap().as_deref(),
            Some("acknowledged")
        );
        assert_eq!(
            service.get("after").await.unwrap().as_deref(),
            Some("acknowledged")
        );
        Ok(())
    }
    .await;
    fixture.finish(result).await;
}

#[allow(dead_code, reason = "selected by the KV recovery integration binary")]
pub async fn retained_secondary_restart() {
    secondary_restart(false).await;
}

#[allow(dead_code, reason = "selected by the KV recovery integration binary")]
pub async fn retired_secondary_recovery() {
    secondary_restart(true).await;
}

async fn secondary_restart(retire: bool) {
    let mut fixture = Fixture::start("m2-secondary-recovery", 3).await;
    let result = async {
        let source = fixture.replicas[0].service.clone();
        let primary = fixture.replicas[0].reference.clone();
        let old = fixture.replicas[1].reference.clone();
        let old_service = fixture.replicas[1].service.clone();
        source.replicate_put("retained".into(), "acknowledged".into()).await.unwrap();
        let before = fixture.snapshot(1).await;
        let prior_sessions = fixture.cluster.native_transport_observation(&primary).await?;
        let gate = kvstore2::state::CopyGate::enabled(
            fixture.cluster.application_root(&primary).await?.join(".recovery-copy-gate")).unwrap();
        gate.hold().unwrap();
        let fresh = fixture.cluster.request_restart(&old).await?;
        for _ in 0..40 {
            fixture.cluster.step().await?;
            let native = fixture.cluster.native_transport_observation(&primary).await?;
            if native["sessions"]["entries"].as_array().unwrap().iter().any(|entry| {
                entry["binding"]["recovery"] == true && entry["phase"] == "Active"
            }) { break; }
        }
        let active = fixture.cluster.native_transport_observation(&primary).await?;
        assert!(active["sessions"]["entries"].as_array().unwrap().iter().any(|entry| {
            entry["binding"]["recovery"] == true && entry["phase"] == "Active"
        }));
        let reopened = fixture.cluster.service(&fresh).await?;
        let held = fixture.cluster.observe().await?;
        let snapshot = held.snapshot(&fresh).unwrap();
        assert!(held.set().status.as_ref().unwrap().public_v1_topology.as_ref().unwrap()
            .sessions.iter().any(|session| session.process_session_id == *old.process_session_id()));
        assert_eq!(snapshot.history, before.history);
        assert_eq!(snapshot.storage, before.storage);
        assert_ne!(snapshot.process_session_id, before.process_session_id);
        assert_ne!(snapshot.creation_id, before.creation_id);
        assert_ne!(snapshot.write_status, AccessStatus::Granted);
        assert_ne!(snapshot.read_status, AccessStatus::Granted);
        assert!(snapshot.publication.as_ref().is_some_and(|p| {
            p.stage == kuberic_runtime::protocol::public_operations::ReplicaPublicationStage::DiscoveryPending
        }));
        assert_eq!(reopened.persistence().get("retained").as_deref(), Some("acknowledged"));
        assert!(reopened.get("retained").await.is_err());
        assert!(reopened.replicate_put("early".into(), "forbidden".into()).await.is_err());
        if retire {
            let root = fixture.cluster.application_root(&fresh).await?;
            let bytes = std::fs::read(root.join("state.json")).unwrap();
            for _ in 0..20 { fixture.cluster.step().await?; }
            let abort = fixture.cluster.retain_ordinary_terminal(&primary, PublicV1CallKind::Abort).await?;
            loop {
                fixture.cluster.step().await?;
                if fixture.cluster.native_transport_observation(&primary).await?["sessions"]["closed"] == true {
                    break;
                }
            }
            gate.release().unwrap();
            assert_eq!(fixture.settled(0, &abort).await, ReplicaGoalStatus::Achieved);
            assert!(!fixture.snapshot(0).await.provider_writer_held);
            let uncertain = fixture.cluster.observe().await?;
            let receiver = uncertain.snapshot(&fresh).unwrap();
            assert_ne!(receiver.read_status, AccessStatus::Granted);
            assert_ne!(receiver.write_status, AccessStatus::Granted);
            if !receiver.withdrawn {
                let target_abort = fixture.cluster.retain_ordinary_terminal(&fresh, PublicV1CallKind::Abort).await?;
                loop {
                    fixture.cluster.step().await?;
                    let state = fixture.cluster.observe().await?;
                    if state.snapshot(&fresh).unwrap().goals.iter().any(|entry| {
                        entry.goal == target_abort && entry.status == ReplicaGoalStatus::Achieved
                    }) { break; }
                }
            }
            for _ in 0..40 {
                fixture.cluster.step().await?;
                let observation = fixture.cluster.observe().await?;
                if observation.snapshot(&fresh).unwrap().withdrawn
                    && !observation.snapshot(&fresh).unwrap().provider_writer_held { break; }
            }
            let state = fixture.cluster.observe().await?;
            let target = state.snapshot(&fresh).unwrap();
            assert!(target.withdrawn);
            assert!(!target.provider_writer_held);
            assert_eq!(std::fs::read(root.join("state.json")).unwrap(), bytes);
            assert_eq!(reopened.persistence().get("retained").as_deref(), Some("acknowledged"));
            assert!(reopened.get("retained").await.is_err());
            let native = fixture.cluster.native_transport_observation(&fresh).await?;
            let entries = native["sessions"]["entries"].as_array().unwrap();
            assert_eq!(entries.len(), 1);
            assert!(entries.iter().all(|entry| entry["admitted"] == false && entry["phase"] != "Completed"));
            return Ok(());
        }
        gate.release().unwrap();
        let transfer = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let native = fixture.cluster.native_transport_observation(&primary).await?;
                if native["sessions"]["entries"].as_array().unwrap().iter().any(|entry| {
                    entry["binding"]["recovery"] == true && entry["phase"] == "Completed"
                }) { break; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Ok::<_, kuberic_controller::test_util::TestError>(())
        }).await;
        match transfer {
            Ok(result) => result?,
            Err(error) => {
                let source = fixture.cluster.native_transport_observation(&primary).await;
                let target = fixture.cluster.native_transport_observation(&fresh).await;
                eprintln!("recovery stalled source: {source:?}");
                eprintln!("recovery stalled target: {target:?}");
                return Err(kuberic_controller::test_util::TestError::with_source(
                    kuberic_controller::test_util::ErrorKind::Deadline,
                    "retained-secondary native transfer did not complete after releasing copy",
                    error,
                ));
            }
        }
        let completed = fixture.cluster.native_transport_observation(&primary).await?;
        assert!(completed["recoveries"].as_array().unwrap().iter()
            .flat_map(|owner| owner.as_array().unwrap()).any(|transfer| {
                transfer["replica"] == old.replica_id() && transfer["complete"] == true
                    && transfer["voting"] == false
            }), "actual completed native transfer must remain nonvoting before ordinary configuration");
        let topology = fixture.cluster.settle_members(3).await?;
        assert_eq!(topology.primary(), &primary);
        let current = topology.secondaries().find(|r| r.replica_id() == old.replica_id()).unwrap();
        assert_eq!(current.process_session_id(), fresh.process_session_id());
        source.replicate_put("after".into(), "acknowledged".into()).await.unwrap();
        assert_eq!(source.get("retained").await.unwrap().as_deref(), Some("acknowledged"));
        assert_eq!(source.get("after").await.unwrap().as_deref(), Some("acknowledged"));
        for member in topology.secondaries() {
            let service = fixture.cluster.service(member).await?;
            assert_eq!(service.persistence().get("retained").as_deref(), Some("acknowledged"));
            assert_eq!(service.persistence().get("after").as_deref(), Some("acknowledged"));
        }
        let native = fixture.cluster.native_transport_observation(&primary).await?;
        let entries = native["sessions"]["entries"].as_array().unwrap();
        assert!(entries.iter().any(|entry| entry["binding"]["recovery"] == true
            && entry["phase"] == "Completed"
            && entry["binding"]["target"]["scope"]["processSessionId"] == fresh.process_session_id().as_str()));
        for old_entry in prior_sessions["sessions"]["entries"].as_array().unwrap().iter().filter(|entry| {
            entry["binding"]["target"]["scope"]["processSessionId"] == old.process_session_id().as_str()
        }) {
            assert!(entries.iter().any(|entry| entry["binding"] == old_entry["binding"]
                && entry["phase"] == "Retired" && entry["admitted"] == false));
        }
        assert!(old_service.get("retained").await.is_err());
        assert!(old_service.replicate_put("late".into(), "forbidden".into()).await.is_err());
        Ok(())
    }.await;
    fixture.finish(result).await;
}

pub async fn fresh_restart_build_and_corrective_empty_replacement() {
    let mut fixture = Fixture::start("kv-fresh-restart", 1).await;
    let result = async {
        let secondary = fresh_restart_growth(&mut fixture).await?;
        eprintln!("actual fresh growth retained/later KV copy and admission passed; requesting M3 secondary replacement");
        fixture.cluster.delete_replica(&secondary).await?;
        let replacement = fixture.cluster.settle_members(2).await?;
        let fresh = replacement.secondaries().next().unwrap().clone();
        assert_ne!(fresh.identity(), secondary.identity());
        assert_eq!(
            fixture.cluster.service(&fresh).await?.persistence().get("retained").as_deref(),
            Some("acknowledged")
        );
        Ok::<(), kuberic_controller::test_util::TestError>(())
    }.await;
    fixture.finish(result).await;
}

#[allow(dead_code, reason = "selected by the KV recovery integration binary")]
pub async fn predecessor_close_fresh_growth() {
    let mut fixture = Fixture::start("m2-predecessor-close-growth", 1).await;
    let result = fresh_restart_growth(&mut fixture).await.map(|_| ());
    fixture.finish(result).await;
}

async fn fresh_restart_growth(
    fixture: &mut Fixture,
) -> kuberic_controller::test_util::Result<kuberic_controller::test_util::ReplicaRef> {
    fixture.replicas[0]
        .service
        .replicate_put("retained".into(), "acknowledged".into())
        .await
        .unwrap();
    let old = fixture.replicas[0].reference.clone();
    let before = fixture.snapshot(0).await;
    let close = fixture.terminal(0, PublicV1CallKind::Close).await;
    assert_eq!(
        fixture.settled(0, &close).await,
        ReplicaGoalStatus::Achieved
    );
    assert!(!fixture.snapshot(0).await.provider_writer_held);
    eprintln!(
        "actual singleton acknowledged data and ordinary Close/writer settlement passed; requesting production restart"
    );
    let current = fixture.cluster.request_restart(&old).await?;
    assert_eq!(current.storage_incarnation(), old.storage_incarnation());
    assert_ne!(current.process_session_id(), old.process_session_id());
    assert_ne!(
        current.replicator_creation_id(),
        old.replicator_creation_id()
    );
    let reopened = fixture.cluster.service(&current).await?;
    assert_eq!(
        reopened.persistence().get("retained").as_deref(),
        Some("acknowledged")
    );
    let observation = fixture.cluster.observe().await?;
    let snapshot = observation.snapshot(&current).unwrap();
    assert_eq!(snapshot.history, before.history);
    assert_ne!(snapshot.read_status, AccessStatus::Granted);
    assert_ne!(snapshot.write_status, AccessStatus::Granted);
    assert!(
        reopened
            .replicate_put("early".into(), "forbidden".into())
            .await
            .is_err()
    );

    fixture.cluster.settle_members(1).await?;
    assert_eq!(
        reopened.get("retained").await.unwrap().as_deref(),
        Some("acknowledged")
    );
    reopened
        .replicate_put("later".into(), "acknowledged".into())
        .await
        .unwrap();
    fixture.cluster.scale_to(2).await?;
    let topology = fixture.cluster.settle_members(2).await?;
    let secondary = topology.secondaries().next().unwrap().clone();
    let copied = fixture.cluster.service(&secondary).await?;
    assert_eq!(
        copied.persistence().get("retained").as_deref(),
        Some("acknowledged")
    );
    assert_eq!(
        copied.persistence().get("later").as_deref(),
        Some("acknowledged")
    );
    let observation = fixture.cluster.observe().await?;
    let source = observation.snapshot(&current).unwrap();
    let target = observation.snapshot(&secondary).unwrap();
    assert!(
        source
            .goals
            .iter()
            .any(|entry| { entry.goal == close && entry.status == ReplicaGoalStatus::Achieved })
    );
    assert!(
        observation
            .set()
            .status
            .as_ref()
            .unwrap()
            .replica_goals
            .iter()
            .any(|entry| {
                entry.goal == close
                    && entry
                        .outcome
                        .as_ref()
                        .is_some_and(|outcome| outcome.status == ReplicaGoalStatus::Achieved)
            })
    );
    assert_eq!(source.write_status, AccessStatus::Granted);
    assert_ne!(target.read_status, AccessStatus::Granted);
    assert_ne!(target.write_status, AccessStatus::Granted);
    let builds = source
        .remote_operations
        .iter()
        .filter(|operation| {
            operation.selection.target.scope.storage == target.storage
                && operation.selection.source.scope.process_session_id == source.process_session_id
                && operation.selection.source.scope.creation_id == source.creation_id
                && operation.selection.target.scope.process_session_id == target.process_session_id
                && operation.selection.target.scope.creation_id == target.creation_id
        })
        .collect::<Vec<_>>();
    assert_eq!(builds.len(), 1);
    assert!(builds[0].accepted);
    let retained = observation
        .set()
        .status
        .as_ref()
        .unwrap()
        .replica_goals
        .iter()
        .find(|entry| entry.goal.operation_id == builds[0].operation_id)
        .unwrap();
    assert!(matches!(&retained.goal.desired,
        kuberic_runtime::protocol::public_operations::ReplicaGoalKind::Build { remote }
            if **remote == builds[0].selection));
    assert_eq!(
        retained.outcome.as_ref().unwrap().status,
        ReplicaGoalStatus::Achieved
    );
    assert!(source.goals.iter().any(|entry| {
        entry.goal == retained.goal
            && entry.status == ReplicaGoalStatus::Achieved
            && entry.receipts > 0
    }));
    reopened
        .replicate_put("after-growth".into(), "acknowledged".into())
        .await
        .unwrap();
    assert_eq!(
        copied.persistence().get("after-growth").as_deref(),
        Some("acknowledged")
    );
    eprintln!(
        "actual fresh growth: achieved predecessor Close retained; exact fresh Build accepted, retained/later KV values copied, target admitted with closed access and continuing replication"
    );
    Ok(secondary)
}

pub async fn real_provider_restart_settlement_and_handoff() {
    let started = std::time::Instant::now();
    let mut fixture = Fixture::start("kv-retained-handoff", 3).await;
    let result = async {
        let primary = fixture.replicas[0].reference.clone();
        let source = fixture.replicas[0].service.clone();
        source.replicate_put("handoff".into(), "acknowledged".into()).await.unwrap();
        let target = fixture.replicas[1].reference.clone();
        let target_service = fixture.replicas[1].service.clone();
        assert_eq!(target_service.persistence().get("handoff").as_deref(), Some("acknowledged"));
        assert!(target_service.get("handoff").await.is_err());
        eprintln!("actual ordinary KV copy, quorum write and closed secondary access passed; requesting production restart");
        let restart = fixture.cluster.restart(&target).await;
        if restart.is_err() {
            for (side, reference) in [("source", &primary), ("target", &target)] {
                match fixture.cluster.native_transport_observation(reference).await {
                    Ok(native) => eprintln!("automatic restart {side}: {native}"),
                    Err(error) => eprintln!("automatic restart {side} observation failed: {error}"),
                }
            }
        }
        let current = restart?;
        assert!(target_service.get("handoff").await.is_err());
        assert!(target_service.replicate_put("late".into(), "forbidden".into()).await.is_err());
        assert_eq!(current.storage_incarnation(), target.storage_incarnation());
        assert_ne!(current.process_session_id(), target.process_session_id());
        let reopened = fixture.cluster.service(&current).await?;
        assert_eq!(reopened.persistence().get("handoff").as_deref(), Some("acknowledged"));
        fixture.cluster.settle_members(3).await?;
        eprintln!("automatic restart prefix elapsed: {:?}", started.elapsed());
        eprintln!("actual retained secondary restart, fresh session, durable data and ordinary readmission passed; requesting M3 handoff");
        let topology = fixture.cluster.switchover(&current).await?;
        assert_eq!(topology.primary().identity(), current.identity());
        assert_eq!(reopened.get("handoff").await.unwrap().as_deref(), Some("acknowledged"));
        assert!(source.replicate_put("old-primary".into(), "forbidden".into()).await.is_err());
        assert_ne!(topology.primary().identity(), primary.identity());
        reopened.replicate_put("after-handoff".into(), "committed".into()).await.unwrap();
        Ok::<(), kuberic_controller::test_util::TestError>(())
    }.await;
    fixture.finish(result).await;
}

#[allow(
    dead_code,
    reason = "shared helper selected by the KV integration binary"
)]
pub async fn original_primary_loss_promotes_survivor_and_fences_return() {
    let mut fixture = Fixture::start("kv-primary-loss", 3).await;
    let result = async {
        let old = fixture.replicas[0].reference.clone();
        let source = fixture.replicas[0].service.clone();
        source.replicate_put("failover".into(), "acknowledged".into()).await.unwrap();
        for replica in &fixture.replicas[1..] {
            assert_eq!(replica.service.persistence().get("failover").as_deref(), Some("acknowledged"));
            assert!(replica.service.get("failover").await.is_err());
        }
        fixture.cluster.stop(&old).await?;
        assert!(source.get("failover").await.is_err());
        assert!(source.replicate_put("old".into(), "forbidden".into()).await.is_err());
        eprintln!("actual ordinary copy, acknowledged quorum data and stopped-primary access fencing passed; requesting survivor election");
        let topology = fixture.cluster.settle_members(2).await?;
        assert_ne!(topology.primary().identity(), old.identity());
        let promoted = fixture.cluster.service(topology.primary()).await?;
        assert_eq!(promoted.get("failover").await.unwrap().as_deref(), Some("acknowledged"));
        promoted.replicate_put("new".into(), "acknowledged".into()).await.unwrap();
        let returned = fixture.cluster.restart(&old).await?;
        assert_ne!(returned.process_session_id(), old.process_session_id());
        assert!(fixture.cluster.service(&returned).await?.get("new").await.is_err());
        fixture.cluster.settle_members(3).await?;
        assert_eq!(
            fixture.cluster.service(&returned).await?.persistence().get("new").as_deref(),
            Some("acknowledged")
        );
        Ok::<(), kuberic_controller::test_util::TestError>(())
    }.await;
    fixture.finish(result).await;
}
