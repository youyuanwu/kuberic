#[path = "ordinary_cluster.rs"]
pub(super) mod ordinary_cluster;

use kuberic_runtime::protocol::public_operations::{PublicV1CallKind, ReplicaGoalStatus};
use kuberic_runtime::protocol::types::AccessStatus;
use ordinary_cluster::Fixture;

pub async fn fresh_restart_build_and_corrective_empty_replacement() {
    let mut fixture = Fixture::start("kv-fresh-restart", 1).await;
    let result = async {
        fixture.replicas[0]
            .service
            .replicate_put("retained".into(), "acknowledged".into())
            .await
            .unwrap();
        let old = fixture.replicas[0].reference.clone();
        let before = fixture.snapshot(0).await;
        let close = fixture.terminal(0, PublicV1CallKind::Close).await;
        assert_eq!(fixture.settled(0, &close).await, ReplicaGoalStatus::Achieved);
        assert!(!fixture.snapshot(0).await.provider_writer_held);
        eprintln!("actual singleton acknowledged data and ordinary Close/writer settlement passed; requesting production restart");
        let current = fixture.cluster.restart(&old).await?;
        assert_eq!(current.storage_incarnation(), old.storage_incarnation());
        assert_ne!(current.process_session_id(), old.process_session_id());
        assert_ne!(current.replicator_creation_id(), old.replicator_creation_id());
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
        assert!(reopened.replicate_put("early".into(), "forbidden".into()).await.is_err());

        fixture.cluster.settle_members(1).await?;
        assert_eq!(reopened.get("retained").await.unwrap().as_deref(), Some("acknowledged"));
        reopened.replicate_put("later".into(), "acknowledged".into()).await.unwrap();
        fixture.cluster.scale_to(2).await?;
        let topology = fixture.cluster.settle_members(2).await?;
        let secondary = topology.secondaries().next().unwrap().clone();
        let copied = fixture.cluster.service(&secondary).await?;
        assert_eq!(copied.persistence().get("retained").as_deref(), Some("acknowledged"));
        assert_eq!(copied.persistence().get("later").as_deref(), Some("acknowledged"));
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

pub async fn real_provider_restart_settlement_and_handoff() {
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
        let current = fixture.cluster.restart(&target).await?;
        assert!(target_service.get("handoff").await.is_err());
        assert!(target_service.replicate_put("late".into(), "forbidden".into()).await.is_err());
        assert_eq!(current.storage_incarnation(), target.storage_incarnation());
        assert_ne!(current.process_session_id(), target.process_session_id());
        let reopened = fixture.cluster.service(&current).await?;
        assert_eq!(reopened.persistence().get("handoff").as_deref(), Some("acknowledged"));
        fixture.cluster.settle_members(3).await?;
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
