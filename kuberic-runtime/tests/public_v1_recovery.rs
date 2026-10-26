#![cfg(feature = "testing")]

#[path = "../../examples/kvstore2/tests/support/ordinary_recovery.rs"]
mod ordinary_recovery;

use std::path::PathBuf;

#[tokio::test]
async fn native_transient_fault_restarts_same_storage_under_exact_session_renewal() {
    ordinary_recovery::fresh_restart_build_and_corrective_empty_replacement().await;
}

#[tokio::test]
async fn retained_secondary_promotion_preserves_acknowledged_bytes_and_replaces_service_location() {
    ordinary_recovery::real_provider_restart_settlement_and_handoff().await;
}

#[tokio::test]
async fn native_controller_unreachable_survivor_fences_until_exact_return_or_explicit_retirement() {
    let mut fixture =
        ordinary_recovery::ordinary_cluster::Fixture::start("native-unreachable-survivor", 3).await;
    let result = async {
        let primary = fixture.replicas[0].reference.clone();
        let unreachable = fixture.replicas[2].reference.clone();
        let old = fixture.replicas[0].service.clone();
        old.replicate_put("unreachable-tail".into(), "acknowledged".into()).await.unwrap();
        for replica in &fixture.replicas[1..] {
            assert_eq!(replica.service.persistence().get("unreachable-tail").as_deref(), Some("acknowledged"));
            assert!(replica.service.get("unreachable-tail").await.is_err());
        }
        fixture.cluster.stop(&primary).await?;
        assert!(old.get("unreachable-tail").await.is_err());
        assert!(old.replicate_put("late".into(), "forbidden".into()).await.is_err());
        let isolation = fixture.cluster.isolate(&[unreachable]).await?;
        for _ in 0..3 {
            fixture.cluster.step().await?;
            let observed = fixture.cluster.observe().await?;
            for (_, snapshot) in observed.snapshots() {
                assert_ne!(snapshot.read_status, kuberic_runtime::protocol::types::AccessStatus::Granted);
                assert_ne!(snapshot.write_status, kuberic_runtime::protocol::types::AccessStatus::Granted);
                assert!(snapshot.publication.as_ref().is_none_or(|publication|
                    publication.role != kuberic_runtime::protocol::types::ReplicaRole::Primary));
            }
        }
        fixture.cluster.heal(isolation).await?;
        eprintln!("actual ordinary data, primary containment and unknown-survivor fencing passed; exact survivor returned, requesting recovery");
        let topology = fixture.cluster.settle_members(3).await?;
        assert_ne!(topology.primary().identity(), primary.identity());
        assert_eq!(fixture.cluster.service(topology.primary()).await?.get("unreachable-tail").await.unwrap().as_deref(), Some("acknowledged"));
        Ok::<(), kuberic_controller::test_util::TestError>(())
    }.await;
    fixture.finish(result).await;
}

#[tokio::test]
async fn native_swap_freezes_specific_quorum_and_conservative_all_fallback_inputs() {
    let mut fixture =
        ordinary_recovery::ordinary_cluster::Fixture::start("native-planned-handoff", 3).await;
    let result = async {
        let primary = fixture.replicas[0].reference.clone();
        let target = fixture.replicas[1].reference.clone();
        let source = fixture.replicas[0].service.clone();
        source.replicate_put("handoff-tail".into(), "acknowledged".into()).await.unwrap();
        let observed = fixture.cluster.observe().await?;
        let capability = observed.report(&primary).unwrap().extensions.catch_up_specific_quorum;
        assert_eq!(capability, Some(true));
        assert_eq!(fixture.replicas[1].service.persistence().get("handoff-tail").as_deref(), Some("acknowledged"));
        eprintln!("actual ordinary copy, quorum data and declared native capability passed; requesting planned primary handoff");
        let topology = fixture.cluster.switchover(&target).await?;
        assert_eq!(topology.primary().identity(), target.identity());
        assert!(source.replicate_put("old-primary".into(), "forbidden".into()).await.is_err());
        assert_eq!(fixture.cluster.service(&target).await?.get("handoff-tail").await.unwrap().as_deref(), Some("acknowledged"));
        Ok::<(), kuberic_controller::test_util::TestError>(())
    }.await;
    fixture.finish(result).await;
}

#[tokio::test]
async fn production_config_has_no_protocol_selector_or_application_commit_oracle() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let process = std::fs::read_to_string(manifest.join("src/host/process.rs")).unwrap();
    let config = process
        .split("pub struct ReplicaProcessConfig")
        .nth(1)
        .unwrap()
        .split('}')
        .next()
        .unwrap();
    assert!(!config.contains("preview"));
    assert!(!manifest.join("src/replicator/stateless/mod.rs").exists());
    let replicator = std::fs::read_to_string(manifest.join("src/replicator/mod.rs")).unwrap();
    assert!(!replicator.contains("mod stateless;"));
    for removed in [
        "NativeRuntimeControl",
        "CustomReplicatorControl",
        "with_custom_control",
        "outbound_build_authority",
    ] {
        assert!(!replicator.contains(removed), "{removed}");
    }
    let production =
        std::fs::read_to_string(manifest.join("src/replicator/v1/service_adapter/production.rs"))
            .unwrap();
    for forbidden in [
        "ManagedReplicaStore",
        "DurableState",
        "LocalWriteJournal",
        "ReplicationProgressStore",
    ] {
        assert!(!production.contains(forbidden), "{forbidden}");
    }
}
