use super::*;

pub(super) async fn scalar_progress(node: &Node) -> i64 {
    node.service
        .control
        .read()
        .await
        .clone()
        .unwrap()
        .current_progress()
        .await
        .unwrap()
}

#[tokio::test]
async fn sf_d02_native_quorum_progress_is_not_a_service_commit_or_caller_cancellation_oracle() {
    let root = root();
    for outcome in ["success", "cancel", "epoch"] {
        let pair = Pair::start(&root.path().join(outcome), 0).await;
        pair.source.service.provider.commit.hold();
        let write = pair
            .source
            .service
            .begin_write(Bytes::from_static(b"sf-d02-value"))
            .await
            .unwrap();
        assert_eq!(write.lsn, 1);
        pair.source.service.provider.commit.entered.notified().await;
        assert_eq!(scalar_progress(&pair.source).await, 1);
        assert_eq!(scalar_progress(&pair.target).await, 1);
        assert_eq!(
            pair.target.service.provider.values(),
            [Bytes::from_static(b"sf-d02-value")]
        );
        assert!(
            pair.source.service.values().await.unwrap().is_empty(),
            "native quorum exposed a value before the service committed it"
        );
        let report = pair.source.report().await;
        assert_eq!(report.progress.current(), Some(1));
        assert!(matches!(
            report.progress.catch_up_capability,
            ProgressQueryResult::Value(_)
        ));
        if outcome == "epoch" {
            pair.source
                .command(PublicOperationProgram::Epoch {
                    epoch: Epoch::new(0, 2),
                })
                .await
                .unwrap();
            pair.source.service.provider.commit.release();
            assert!(write.wait().await.is_err());
            assert!(pair.source.service.provider.values().is_empty());
        } else if outcome == "cancel" {
            drop(write);
            pair.source.service.provider.commit.release();
            eventually(|| pair.source.service.provider.values().len() == 1).await;
            assert_eq!(
                pair.source.service.values().await.unwrap(),
                [Bytes::from_static(b"sf-d02-value")]
            );
        } else {
            pair.source.service.provider.commit.release();
            assert_eq!(write.wait().await.unwrap(), 1);
            assert_eq!(
                pair.source.service.values().await.unwrap(),
                [Bytes::from_static(b"sf-d02-value")]
            );
        }
        pair.stop().await;
    }
}

#[tokio::test]
async fn sf_d02_secondary_receive_progress_is_contiguous_but_does_not_replace_service_ack() {
    let root = root();
    let pair = Pair::start(root.path(), 0).await;
    let mut operations = pair.target.service.control_operations();
    let address = pair.target.report().await.replication_address.unwrap();
    let binding = binding(&pair).await;
    let frames = vec![
        replication_frame(&pair, 3, Bytes::from_static(b"three")).await,
        replication_frame(&pair, 1, Bytes::from_static(b"one")).await,
    ];
    let first = exchange(
        address.clone(),
        &binding,
        &format!("Bearer {TOKEN}"),
        frames,
    )
    .await
    .unwrap();
    let one = delivered(&mut operations).await;
    assert_eq!(one.lsn, 1);
    pair.target
        .service
        .provider
        .persist_received(&one)
        .await
        .unwrap();
    assert_eq!(scalar_progress(&pair.target).await, 1);
    one.acknowledge().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(40), operations.recv())
            .await
            .is_err(),
        "out-of-order three escaped the native queue"
    );
    let two = replication_frame(&pair, 2, Bytes::from_static(b"two")).await;
    let mut second = exchange(address, &binding, &format!("Bearer {TOKEN}"), vec![two])
        .await
        .unwrap();
    for expected in [2, 3] {
        let operation = delivered(&mut operations).await;
        assert_eq!(operation.lsn, expected);
        pair.target
            .service
            .provider
            .persist_received(&operation)
            .await
            .unwrap();
        operation.acknowledge().unwrap();
    }
    applied(&mut second, 3).await;
    assert_eq!(scalar_progress(&pair.target).await, 3);
    assert_eq!(
        pair.target.service.provider.values(),
        [
            Bytes::from_static(b"one"),
            Bytes::from_static(b"two"),
            Bytes::from_static(b"three")
        ]
    );
    let report = pair.target.report().await;
    assert_eq!(report.progress.current(), Some(3));
    assert_eq!(report.read_status, AccessStatus::NotPrimary);
    assert_eq!(report.write_status, AccessStatus::NotPrimary);
    assert!(pair.target.service.values().await.is_err());
    drop(first);
    drop(second);
    pair.stop().await;
}

#[tokio::test]
async fn sf11_acknowledged_prefix_6_through_10_survives_ambiguous_11_without_history_fabrication() {
    let root = root();
    let pair = Pair::start(root.path(), 5).await;
    let mut operations = pair.target.service.control_operations();
    for lsn in 6..=10 {
        let value = Bytes::from(format!("value-{lsn}"));
        let write = pair
            .source
            .service
            .begin_write(value.clone())
            .await
            .unwrap();
        let operation = delivered(&mut operations).await;
        assert_eq!(operation.lsn, lsn);
        assert_eq!(operation.data, value);
        pair.target
            .service
            .provider
            .persist_received(&operation)
            .await
            .unwrap();
        operation.acknowledge().unwrap();
        assert_eq!(write.wait().await.unwrap(), lsn);
    }
    pair.source.service.provider.commit.hold();
    let write = pair
        .source
        .service
        .begin_write(Bytes::from_static(b"ambiguous-eleven"))
        .await
        .unwrap();
    let operation = delivered(&mut operations).await;
    assert_eq!(operation.lsn, 11);
    pair.target
        .service
        .provider
        .persist_received(&operation)
        .await
        .unwrap();
    operation.acknowledge().unwrap();
    pair.source.service.provider.commit.entered.notified().await;
    assert_eq!(pair.source.service.values().await.unwrap().len(), 10);
    assert_eq!(scalar_progress(&pair.source).await, 11);
    assert_eq!(pair.target.service.provider.values().len(), 11);
    let before = std::fs::read(pair.source.service.provider.root.join("service.json")).unwrap();
    let reopened = Provider::new(pair.source.service.provider.root.clone()).unwrap();
    assert_eq!(reopened.values().len(), 10);
    assert!(
        reopened
            .commit(6, Bytes::from_static(b"divergent-six"))
            .is_err()
    );
    assert!(
        reopened
            .commit(12, Bytes::from_static(b"missing-eleven"))
            .is_err()
    );
    assert_eq!(
        std::fs::read(pair.source.service.provider.root.join("service.json")).unwrap(),
        before
    );
    drop(write);
    pair.source.service.provider.commit.release();
    eventually(|| pair.source.service.provider.values().len() == 11).await;
    assert_eq!(
        pair.source.service.provider.values(),
        pair.target.service.provider.values()
    );
    pair.stop().await;
}
