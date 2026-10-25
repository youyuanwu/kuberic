#![cfg(feature = "testing")]

#[path = "support/ordinary_cluster.rs"]
mod ordinary_cluster;

use std::time::Duration;

use kuberic_runtime::RuntimeError;
use kuberic_runtime::engine::DurableState;
use kuberic_runtime::protocol::public_operations::{PublicV1CallKind, ReplicaGoalStatus};
use kuberic_runtime::protocol::types::AccessStatus;
use ordinary_cluster::Fixture;

async fn fixture(name: &str) -> Fixture {
    let fixture = Fixture::start(name, 3).await;
    fixture.replicas[0]
        .service
        .replicate_put("baseline".into(), "committed".into())
        .await
        .unwrap();
    assert_committed(&fixture, "baseline", "committed").await;
    fixture
}

async fn assert_committed(fixture: &Fixture, key: &str, value: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if fixture.replicas[0]
                .service
                .get(key)
                .await
                .ok()
                .flatten()
                .as_deref()
                == Some(value)
                && fixture
                    .replicas
                    .iter()
                    .all(|replica| replica.service.persistence().get(key).as_deref() == Some(value))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual application commits did not converge");
    for replica in &fixture.replicas[1..] {
        assert!(matches!(
            replica.service.get(key).await,
            Err(RuntimeError::ReadClosed(AccessStatus::NotPrimary))
        ));
    }
}

async fn pending_is_hidden(fixture: &mut Fixture) {
    let primary = &fixture.replicas[0];
    assert_eq!(primary.service.get("visible").await.unwrap(), None);
    assert_eq!(
        primary.service.get("baseline").await.unwrap().as_deref(),
        Some("committed")
    );
    let durable = primary
        .service
        .persistence()
        .durable_progress()
        .await
        .unwrap();
    assert_eq!((durable.applied_lsn, durable.committed_lsn), (2, 1));
    let snapshot = fixture.snapshot(0).await;
    assert_eq!(snapshot.read_status, AccessStatus::Granted);
    assert!(!snapshot.withdrawn);
}

async fn gated_write(
    fixture: &Fixture,
) -> (
    kvstore2::v1_test_bridge::CommitGate,
    tokio::task::JoinHandle<kuberic_runtime::Result<i64>>,
) {
    let mut gate = fixture.replicas[0].service.pause_next_commit().unwrap();
    let primary = fixture.replicas[0].service.clone();
    let write = tokio::spawn(async move {
        primary
            .replicate_put("visible".into(), "pending".into())
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), gate.entered())
        .await
        .unwrap();
    (gate, write)
}

#[tokio::test]
async fn sf15_trait_close_retains_cancelled_consumer_drain() {
    use kuberic_runtime::StatefulServiceReplica;
    let mut fixture = fixture("sf15-close").await;
    let service = fixture.replicas[1].service.clone();
    assert!(service.retained_consumer_count().await > 0);
    let gate = service.pause_next_stream_exit();
    let closing = service.clone();
    let close = tokio::spawn(async move { StatefulServiceReplica::close(closing.as_ref()).await });
    tokio::time::timeout(Duration::from_secs(5), gate.entered())
        .await
        .unwrap();
    assert!(
        !close.is_finished(),
        "Close returned before the actual consumer exit"
    );
    close.abort();
    assert!(close.await.unwrap_err().is_cancelled());
    let closing = service.clone();
    let mut retry =
        tokio::spawn(async move { StatefulServiceReplica::close(closing.as_ref()).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut retry)
            .await
            .is_err()
    );
    gate.release();
    tokio::time::timeout(Duration::from_secs(5), retry)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(service.retained_consumer_count().await, 0);
    StatefulServiceReplica::close(service.as_ref())
        .await
        .unwrap();
    let reference = fixture.replicas[1].reference.clone();
    fixture.cluster.stop(&reference).await.unwrap();
    fixture.finish(Ok(())).await;
}

#[tokio::test]
async fn sf01_primary_local_commit_hides_speculation_after_native_completion() {
    let mut fixture = fixture("sf01-local").await;
    let (gate, write) = gated_write(&fixture).await;
    pending_is_hidden(&mut fixture).await;
    assert!(
        !write.is_finished(),
        "native completion is not client success"
    );
    gate.release();
    assert_eq!(write.await.unwrap().unwrap(), 2);
    assert_committed(&fixture, "visible", "pending").await;
    fixture.finish(Ok(())).await;
}

#[tokio::test]
async fn sf01_secondary_ack_does_not_publish_the_primary_application_view() {
    let mut fixture = fixture("sf01-secondary").await;
    let (gate, write) = gated_write(&fixture).await;
    pending_is_hidden(&mut fixture).await;
    assert!(!write.is_finished());
    assert!(fixture.replicas[1..].iter().any(|replica| {
        replica.service.persistence().get("visible").as_deref() == Some("pending")
    }));
    for replica in &fixture.replicas[1..] {
        assert!(matches!(
            replica.service.get("visible").await,
            Err(RuntimeError::ReadClosed(AccessStatus::NotPrimary))
        ));
    }
    gate.release();
    write.await.unwrap().unwrap();
    assert_committed(&fixture, "visible", "pending").await;
    fixture.finish(Ok(())).await;
}

#[tokio::test]
async fn sf01_dropped_client_retains_service_owned_commit_and_observation() {
    let mut fixture = fixture("sf01-drop").await;
    let (gate, write) = gated_write(&fixture).await;
    pending_is_hidden(&mut fixture).await;
    write.abort();
    assert!(write.await.unwrap_err().is_cancelled());
    gate.release();
    assert_committed(&fixture, "visible", "pending").await;
    assert_eq!(
        fixture.replicas[0]
            .service
            .replicate_put("later".into(), "live".into())
            .await
            .unwrap(),
        3
    );
    fixture.finish(Ok(())).await;
}

#[tokio::test]
async fn sf01_abort_during_local_commit_never_late_grants_success() {
    let mut fixture = fixture("sf01-abort").await;
    let (gate, write) = gated_write(&fixture).await;
    pending_is_hidden(&mut fixture).await;
    let terminal = fixture.terminal(0, PublicV1CallKind::Abort).await;
    let fenced = fixture.snapshot(0).await;
    assert!(fenced.publication.is_none());
    assert_ne!(fenced.read_status, AccessStatus::Granted);
    assert_ne!(fenced.write_status, AccessStatus::Granted);
    gate.release();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), write)
            .await
            .unwrap()
            .unwrap(),
        Err(RuntimeError::AdmittedOperationFailed { lsn: 2, .. })
    ));
    assert_eq!(
        fixture.settled(0, &terminal).await,
        ReplicaGoalStatus::Achieved
    );
    assert!(!fixture.snapshot(0).await.provider_writer_held);
    let service = &fixture.replicas[0].service;
    assert_eq!(
        service
            .persistence()
            .durable_progress()
            .await
            .unwrap()
            .committed_lsn,
        1
    );
    assert!(service.get("visible").await.is_err());
    assert!(
        service
            .replicate_put("late".into(), "forbidden".into())
            .await
            .is_err()
    );
    fixture.finish(Ok(())).await;
}

#[tokio::test]
async fn sf01_physical_quorum_loss_cannot_complete_or_publish_an_admitted_write() {
    let mut fixture = fixture("sf01-quorum").await;
    for index in [1, 2] {
        let reference = fixture.replicas[index].reference.clone();
        fixture.cluster.stop(&reference).await.unwrap();
    }
    let primary = fixture.replicas[0].service.clone();
    let mut write = tokio::spawn(async move {
        primary
            .replicate_put("visible".into(), "pending".into())
            .await
    });
    let completed = match tokio::time::timeout(Duration::from_millis(150), &mut write).await {
        Ok(result) => {
            assert!(
                result.unwrap().is_err(),
                "physical quorum loss acknowledged a write"
            );
            true
        }
        Err(_) => false,
    };
    let service = &fixture.replicas[0].service;
    assert_eq!(
        service
            .persistence()
            .durable_progress()
            .await
            .unwrap()
            .committed_lsn,
        1
    );
    if let Ok(value) = service.get("visible").await {
        assert_eq!(value, None);
    }
    let terminal = fixture.terminal(0, PublicV1CallKind::Abort).await;
    if !completed {
        assert!(
            tokio::time::timeout(Duration::from_secs(5), write)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }
    assert_eq!(
        fixture.settled(0, &terminal).await,
        ReplicaGoalStatus::Achieved
    );
    assert!(fixture.replicas[0].service.get("visible").await.is_err());
    fixture.finish(Ok(())).await;
}

#[tokio::test]
async fn sf11_acknowledged_prefix_6_through_10_survives_ambiguous_11_without_history_fabrication() {
    let mut fixture = fixture("sf11-prefix").await;
    for lsn in 2..=10 {
        assert_eq!(
            fixture.replicas[0]
                .service
                .replicate_put(format!("prefix-{lsn}"), format!("value-{lsn}"))
                .await
                .unwrap(),
            lsn
        );
    }
    let before = fixture.snapshot(0).await;
    let mut gate = fixture.replicas[0].service.pause_next_commit().unwrap();
    let service = fixture.replicas[0].service.clone();
    let write = tokio::spawn(async move {
        service
            .replicate_put("ambiguous-11".into(), "not-acknowledged".into())
            .await
    });
    gate.entered().await;
    let durable = fixture.replicas[0]
        .service
        .persistence()
        .durable_progress()
        .await
        .unwrap();
    assert_eq!((durable.applied_lsn, durable.committed_lsn), (11, 10));
    let terminal = fixture.terminal(0, PublicV1CallKind::Abort).await;
    gate.release();
    assert!(matches!(
        write.await.unwrap(),
        Err(RuntimeError::AdmittedOperationFailed { lsn: 11, .. })
    ));
    assert_eq!(
        fixture.settled(0, &terminal).await,
        ReplicaGoalStatus::Achieved
    );
    let after = fixture.snapshot(0).await;
    assert_eq!(after.history, before.history);
    assert_eq!(after.storage, before.storage);
    assert!(after.withdrawn && after.publication.is_none() && !after.provider_writer_held);
    let service = &fixture.replicas[0].service;
    assert_eq!(
        service
            .persistence()
            .durable_progress()
            .await
            .unwrap()
            .committed_lsn,
        10
    );
    for lsn in 6..=10 {
        assert_eq!(
            service.persistence().get(&format!("prefix-{lsn}")),
            Some(format!("value-{lsn}"))
        );
    }
    assert!(service.get("ambiguous-11").await.is_err());
    assert!(
        service
            .replicate_put("late".into(), "forbidden".into())
            .await
            .is_err()
    );
    fixture.finish(Ok(())).await;
}

fn contains_failure(error: &(dyn std::error::Error + 'static), expected: &str) -> bool {
    error.to_string().contains(expected)
        || error
            .source()
            .is_some_and(|source| contains_failure(source, expected))
}

#[tokio::test]
async fn primary_commit_failure_contains_the_ambiguous_native_quorum_success() {
    let mut fixture = fixture("sf01-local-failure").await;
    let mut commit = fixture.replicas[0].service.pause_next_commit().unwrap();
    let source = fixture.replicas[0].service.clone();
    let write = tokio::spawn(async move {
        source
            .replicate_put("visible".into(), "not-acknowledged".into())
            .await
    });
    commit.entered().await;
    assert!(!write.is_finished());
    assert!(fixture.replicas[1..].iter().any(|replica| {
        replica.service.persistence().get("visible").as_deref() == Some("not-acknowledged")
    }));
    fixture.replicas[0].service.fail_next_persist();
    commit.release();
    let error = write.await.unwrap().unwrap_err();
    assert!(
        matches!(&error, RuntimeError::AdmittedOperationFailed { lsn: 2, reason }
        if reason.contains("injected persistence failure")),
        "{error}"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let observed = fixture.snapshot(0).await;
            if observed.publication.is_none() {
                assert_ne!(observed.read_status, AccessStatus::Granted);
                assert_ne!(observed.write_status, AccessStatus::Granted);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let service = &fixture.replicas[0].service;
    assert_eq!(
        service
            .persistence()
            .durable_progress()
            .await
            .unwrap()
            .committed_lsn,
        1
    );
    assert!(service.get("visible").await.is_err());
    assert!(
        service
            .replicate_put("late".into(), "forbidden".into())
            .await
            .is_err()
    );
    let error = fixture
        .cluster
        .finish(Ok::<(), std::io::Error>(()))
        .await
        .unwrap_err();
    assert!(
        error
            .sources()
            .any(|source| contains_failure(source, "injected persistence failure")),
        "actual cleanup hid the failed commit: {error}"
    );
}

#[tokio::test]
async fn secondary_persistence_failure_never_acknowledges_service_work() {
    let mut fixture = Fixture::start("sf01-target-failure", 2).await;
    fixture.replicas[0]
        .service
        .replicate_put("baseline".into(), "committed".into())
        .await
        .unwrap();
    fixture.replicas[1].service.fail_next_persist();
    let source = fixture.replicas[0].service.clone();
    let mut write = tokio::spawn(async move {
        source
            .replicate_put("visible".into(), "not-acknowledged".into())
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(40), &mut write)
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fixture.snapshot(1).await.publication.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(fixture.replicas[1].service.get("visible").await.is_err());
    let terminal = fixture.terminal(0, PublicV1CallKind::Abort).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), write)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(
        fixture.settled(0, &terminal).await,
        ReplicaGoalStatus::Achieved
    );
    assert_eq!(
        fixture.replicas[0]
            .service
            .persistence()
            .durable_progress()
            .await
            .unwrap()
            .committed_lsn,
        1
    );
    let error = fixture
        .cluster
        .finish(Ok::<(), std::io::Error>(()))
        .await
        .unwrap_err();
    assert!(
        error
            .sources()
            .any(|source| contains_failure(source, "injected persistence failure")),
        "actual cleanup hid the failed secondary persistence: {error}"
    );
}
