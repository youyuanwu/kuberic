use super::*;
use std::sync::atomic::AtomicUsize;

// This observer only forwards negative admission, never data or progress.
// The actual selected target listener must produce the returned status.
struct RejectionObserver {
    address: String,
    expected: serde_json::Value,
    rejected: tokio::sync::mpsc::UnboundedSender<tonic::Code>,
}

#[tonic::async_trait]
impl native_wire::carrier_server::Carrier for RejectionObserver {
    type ExchangeStream = HungCarrierStream;

    async fn exchange(
        &self,
        request: Request<tonic::Streaming<native_wire::Envelope>>,
    ) -> std::result::Result<tonic::Response<Self::ExchangeStream>, tonic::Status> {
        let metadata = request.metadata().clone();
        let bytes = metadata
            .get_bin("kuberic-native-binding-bin")
            .unwrap()
            .to_bytes()
            .unwrap();
        let actual: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(actual, self.expected);
        let mut client = native_wire::carrier_client::CarrierClient::connect(self.address.clone())
            .await
            .unwrap();
        let mut forwarded = Request::new(stream::pending::<native_wire::Envelope>());
        *forwarded.metadata_mut() = metadata;
        let status = client.exchange(forwarded).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::PermissionDenied);
        self.rejected.send(status.code()).unwrap();
        Err(status)
    }
}

fn configuration(source: &Node, peers: &[&Node]) -> ConfigurationDescriptor {
    ConfigurationDescriptor::new(
        EPOCH,
        source.identity.replica_id,
        std::iter::once(ConfigurationMember {
            identity: source.identity.clone(),
            role: ReplicaRole::Primary,
        })
        .chain(peers.iter().map(|peer| ConfigurationMember {
            identity: peer.identity.clone(),
            role: ReplicaRole::ActiveSecondary,
        }))
        .collect(),
        2,
    )
}

async fn public_rejection_order(reject_first: bool) {
    let root = root();
    let mut pair = Pair::start(root.path(), 1).await;
    let mut third = Node::start(
        &root.path().join("third"),
        &pair.source.history.resource_uid,
        3,
    )
    .await;
    third.open(ReplicaRole::IdleSecondary, EPOCH).await;
    pair.source.build(&third, &pair.configuration).await;
    third
        .command(PublicOperationProgram::Role {
            epoch: EPOCH,
            role: ReplicaRole::ActiveSecondary,
        })
        .await
        .unwrap();
    third
        .command(PublicOperationProgram::Epoch { epoch: EPOCH })
        .await
        .unwrap();
    let old = configuration(&pair.source, &[&third]);
    third
        .command(PublicOperationProgram::Configuration(PublicConfiguration {
            current: old.clone(),
            previous: None,
            replicas: vec![pair.source.descriptor(ReplicaRole::Primary).await],
        }))
        .await
        .unwrap();
    let current = configuration(&pair.source, &[&pair.target, &third]);
    assert_ne!(old.configuration_id, current.configuration_id);
    let expected = serde_json::json!({
        "history": pair.source.history,
        "epoch": EPOCH,
        "currentConfigurationId": current.configuration_id,
        "previousConfigurationId": null,
        "primary": NativeEndpoint::from_report(&pair.source.report().await),
        "secondary": NativeEndpoint::from_report(&third.report().await),
        "build": null,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let observer_address = format!("http://{}", listener.local_addr().unwrap());
    let (rejected, mut rejections) = tokio::sync::mpsc::unbounded_channel();
    let (shutdown, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(native_wire::carrier_server::CarrierServer::new(
                RejectionObserver {
                    address: third.report().await.replication_address.unwrap(),
                    expected,
                    rejected,
                },
            ))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = shutdown_rx.await;
                },
            ),
    );
    let first = Arc::new(Gate::default());
    first.hold();
    let attempts = Arc::new(AtomicUsize::new(0));
    let retry = Arc::new(Notify::new());
    let isolated = third.identity.instance_id.to_string();
    let route =
        TestingTcpRoute::register_with_kind(pair.source.identity.instance_id.to_string(), {
            let first = first.clone();
            let attempts = attempts.clone();
            let retry = retry.clone();
            move |kind, peer, canonical| {
                if kind != TestingTcpRouteKind::Replication || peer != isolated {
                    return Box::pin(async move { Ok(canonical) });
                }
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                let first = first.clone();
                let retry = retry.clone();
                let destination = observer_address.clone();
                Box::pin(async move {
                    if attempt == 0 {
                        first.wait().await;
                        Ok(destination)
                    } else {
                        assert_eq!(attempt, 1, "retry must stay held");
                        retry.notify_one();
                        std::future::pending().await
                    }
                })
            }
        })
        .unwrap();
    pair.target
        .command(PublicOperationProgram::Configuration(PublicConfiguration {
            current: current.clone(),
            previous: None,
            replicas: vec![
                pair.source.descriptor(ReplicaRole::Primary).await,
                third.descriptor(ReplicaRole::ActiveSecondary).await,
            ],
        }))
        .await
        .unwrap();
    let peers = vec![
        pair.target.descriptor(ReplicaRole::ActiveSecondary).await,
        third.descriptor(ReplicaRole::ActiveSecondary).await,
    ];
    pair.source
        .command(PublicOperationProgram::Configuration(PublicConfiguration {
            current: current.clone(),
            previous: None,
            replicas: peers.clone(),
        }))
        .await
        .unwrap();
    pair.configuration = current.clone();
    tokio::time::timeout(Duration::from_secs(10), first.entered.notified())
        .await
        .unwrap();
    let mut operations = pair.target.service.control_operations();
    let mut write = pair
        .source
        .service
        .begin_write(Bytes::from_static(b"ordered-rejection"))
        .await
        .unwrap();
    assert_eq!(write.lsn, 2);
    let operation = delivered(&mut operations).await;
    assert_eq!(
        (operation.kind, operation.lsn, operation.end_of_stream),
        (OperationKind::Replication, 2, false)
    );
    let mut marked = peers;
    marked[0].must_catchup = true;
    let catchup_intent = pair
        .source
        .intent(PublicOperationProgram::CatchUp {
            configuration: PublicConfiguration {
                current,
                previous: None,
                replicas: marked,
            },
            mode: PublicCatchUpMode::WriteQuorum,
        })
        .await;
    let handle = pair.source.handle();
    pair.source
        .service
        .provider
        .catchup_boundary
        .store(write.lsn as u64, Ordering::Release);
    let mut catchup = tokio::spawn(async move { handle.execute_public_v1(catchup_intent).await });
    tokio::time::timeout(
        Duration::from_secs(10),
        pair.source.service.provider.catchup_entered.notified(),
    )
    .await
    .unwrap();
    assert!(matches!(
        write.outcome.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    if reject_first {
        first.release();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), rejections.recv())
                .await
                .unwrap(),
            Some(tonic::Code::PermissionDenied)
        );
        // Attempt 2 is source-side: it follows consuming the exact rejection.
        tokio::time::timeout(Duration::from_secs(10), retry.notified())
            .await
            .unwrap();
        pair.source.service.provider.catchup_probe.notify_one();
        tokio::time::timeout(
            Duration::from_secs(10),
            pair.source.service.provider.catchup_entered.notified(),
        )
        .await
        .unwrap();
        assert!(matches!(
            write.outcome.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }
    pair.target
        .service
        .provider
        .persist_received(&operation)
        .await
        .unwrap();
    operation.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(10), &mut catchup)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), write.wait())
            .await
            .unwrap()
            .unwrap(),
        2
    );
    if !reject_first {
        first.release();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), rejections.recv())
                .await
                .unwrap(),
            Some(tonic::Code::PermissionDenied)
        );
        tokio::time::timeout(Duration::from_secs(10), retry.notified())
            .await
            .unwrap();
    }
    let next = pair
        .source
        .service
        .begin_write(Bytes::from_static(b"after-rejection"))
        .await
        .unwrap();
    let operation = delivered(&mut operations).await;
    assert_eq!(operation.lsn, 3);
    pair.target
        .service
        .provider
        .persist_received(&operation)
        .await
        .unwrap();
    operation.acknowledge().unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), next.wait())
            .await
            .unwrap()
            .unwrap(),
        3
    );
    assert_eq!(
        pair.source.service.values().await.unwrap(),
        pair.target.service.provider.values()
    );
    assert_eq!(third.service.provider.values().len(), 1);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    pair.source
        .command(PublicOperationProgram::Close)
        .await
        .unwrap();
    assert_eq!(
        pair.source
            .handle()
            .public_v1_raw_retained_task_cardinality()
            .unwrap(),
        (0, 0)
    );
    drop(route);
    shutdown.send(()).unwrap();
    server.await.unwrap().unwrap();
    pair.source.stop().await;
    pair.target.stop().await;
    third.stop().await;
}

#[tokio::test]
async fn dispensable_remote_rejection_before_healthy_apply_preserves_public_quorum() {
    public_rejection_order(true).await;
}

#[tokio::test]
async fn dispensable_remote_rejection_after_healthy_apply_preserves_public_quorum() {
    public_rejection_order(false).await;
}

#[tokio::test]
async fn local_source_build_fault_rejects_new_public_native_entries() {
    for failure_kind in [1, 2] {
        let root = root();
        let mut pair = Pair::start(root.path(), 1).await;
        let mut third = Node::start(
            &root.path().join("third"),
            &pair.source.history.resource_uid,
            3,
        )
        .await;
        third.open(ReplicaRole::IdleSecondary, EPOCH).await;
        let primary = pair.source.service.primary.read().await.clone().unwrap();
        primary
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::WriteQuorum)
            .await
            .unwrap();
        pair.source
            .service
            .provider
            .copy_failure
            .store(failure_kind, Ordering::Release);
        let (intent, _) = pair.source.build_intent(&third, &pair.configuration).await;
        assert!(
            pair.source
                .handle()
                .execute_public_v1(intent)
                .await
                .is_err()
        );
        assert!(
            pair.source
                .service
                .provider
                .copy_calls
                .load(Ordering::SeqCst)
                >= 2
        );
        assert!(primary.current_progress().await.is_err());
        assert!(
            primary
                .wait_for_catch_up_quorum(ReplicaSetQuorumMode::WriteQuorum)
                .await
                .is_err()
        );
        let state = pair.source.service.state.read().await.clone().unwrap();
        assert!(
            state
                .begin_replicate(Bytes::from_static(b"must-not-assign"))
                .is_err()
        );
        assert_eq!(pair.source.service.provider.values().len(), 1);
        pair.source
            .command(PublicOperationProgram::Close)
            .await
            .unwrap();
        pair.source.stop().await;
        pair.target.stop().await;
        third.stop().await;
    }
}
