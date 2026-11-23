use kuberic_tests::fm as support;

use std::time::Duration;

use anyhow::Result;
use fube_node::control::wire::{self, Link};
use fube_ra::protocol::*;
use furpc::Message;
use kuberic_fm::{Config, Phase, State, Status};
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch},
    time::Instant,
};

async fn connect(config: &Config, node: usize, instance: u64) -> Link {
    let deadline = Instant::now() + Duration::from_secs(3);
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket
        .bind((*config.nodes[node].address.ip(), 0).into())
        .unwrap();
    let stream = tokio::time::timeout_at(deadline, socket.connect(config.fm.address.into()))
        .await
        .unwrap()
        .unwrap();
    let channel = wire::initiate(stream, wire::Role::Node, deadline)
        .await
        .unwrap();
    let mut identity = config.nodes[node].identity.native().unwrap();
    identity.instance = instance;
    Link::new(
        channel,
        identity,
        config.fm.identity.native().unwrap(),
        config.generation().unwrap(),
    )
    .unwrap()
}

async fn receive(link: &Link) -> Message {
    let incoming = link
        .receive(Instant::now() + Duration::from_secs(3))
        .await
        .unwrap()
        .expect("native command deadline");
    incoming.message
}

async fn wait_status(receiver: &mut watch::Receiver<Status>, predicate: impl Fn(&Status) -> bool) {
    tokio::time::timeout(Duration::from_secs(3), receiver.wait_for(predicate))
        .await
        .unwrap()
        .unwrap();
}

async fn receive_action(link: &Link, action: &str, superseded: &[&Message]) -> Message {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let message = receive(link).await;
            let stale = superseded.iter().any(|old| {
                old.body()
                    .segments()
                    .flatten()
                    .eq(message.body().segments().flatten())
                    && wire::action(old).unwrap() == wire::action(&message).unwrap()
            });
            if !stale && wire::action(&message).unwrap() == action {
                return message;
            }
            assert!(
                stale,
                "unexpected native action: {:?}",
                wire::action(&message).unwrap()
            );
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn native_channels_drive_scripted_bootstrap_retry_create_and_owned_teardown() {
    scripted_lifecycle(false).await;
}

#[tokio::test]
async fn native_channels_route_scripted_candidate_changes_then_repair_and_delete_final_membership()
{
    scripted_lifecycle(true).await;
}

async fn scripted_lifecycle(redirect: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = support::config();
    config.fm.address = match listener.local_addr().unwrap() {
        std::net::SocketAddr::V4(address) => address,
        _ => unreachable!(),
    };
    let (stop, stopped) = oneshot::channel();
    let (status, mut observed) = watch::channel(State::new(config.clone()).unwrap().status());
    let fm = tokio::spawn(kuberic_fm::run(
        config.clone(),
        listener,
        async {
            stopped.await?;
            Ok(())
        },
        move |value| {
            status.send_replace(value.clone());
            Ok(())
        },
    ));
    let mut peers = Vec::new();
    for node in 0..3 {
        let link = connect(&config, node, 1).await;
        let (ack, generation) = link
            .node_up(&decode_node_up(&support::node_up(&config, node)).unwrap())
            .await
            .unwrap();
        ack.validate_fresh_activated(&FabricVersionInstance::native_default())
            .unwrap();
        assert_eq!(generation, config.generation().unwrap());
        let report = ReplicaUp {
            last: true,
            ..ReplicaUp::default()
        };
        link.send(replica_up_message(&report, Owner::Fm).unwrap())
            .await
            .unwrap();
        assert_eq!(
            decode_replica_up_reply(&receive(&link).await).unwrap().0,
            report
        );
        peers.push(link);
    }
    let mut committed = None;
    let mut initial_configuration = None;
    let mut completed_requests = vec![Vec::<Message>::new(); 3];
    for step in 0..4 {
        let request = receive(&peers[1]).await;
        if step == 1 || step == 3 {
            let retry = receive(&peers[1]).await;
            assert!(retry
                .body()
                .segments()
                .flatten()
                .eq(request.body().segments().flatten()));
            assert_eq!(
                wire::action(&retry).unwrap(),
                wire::action(&request).unwrap()
            );
        }
        assert!(observed.borrow().service_endpoint.is_none());
        let reply = support::completion(&request);
        completed_requests[1].push(request.clone());
        if step == 3 {
            committed = Some(decode_do_reconfiguration_reply(&reply).unwrap().0);
            initial_configuration = Some(request.clone());
        }
        peers[1].send(reply).await.unwrap();
    }
    wait_status(&mut observed, |status| status.phase == Phase::Serving).await;
    assert_eq!(
        observed.borrow().service_endpoint.as_deref(),
        Some("client://node-20/opaque")
    );
    if redirect {
        let committed = committed.unwrap();
        let loss = support::current_app_host_loss(&config, &committed, 1);
        peers[1]
            .send(replica_up_message(&loss, Owner::Fm).unwrap())
            .await
            .unwrap();
        let ack = receive_action(
            &peers[1],
            "ReplicaUpReply",
            &[initial_configuration.as_ref().unwrap()],
        )
        .await;
        assert_eq!(decode_replica_up_reply(&ack).unwrap().0, loss);
        let original = receive(&peers[0]).await;
        completed_requests[0].push(original.clone());
        let request = decode_do_reconfiguration(&original).unwrap().0;
        let report = change_configuration_message(
            &support::candidate_change(&request, &config.nodes[2].identity.native().unwrap()),
            Owner::Fm,
        )
        .unwrap();
        peers[0].send(report.clone()).await.unwrap();
        let ack = receive_action(&peers[0], "ChangeConfigurationReply", &[&original]).await;
        let (receipt, generation) = decode_change_configuration_reply(&ack).unwrap();
        assert_eq!(generation, config.generation().unwrap());
        let redirected = receive(&peers[2]).await;
        completed_requests[2].push(redirected.clone());
        let next = decode_do_reconfiguration(&redirected).unwrap().0;
        assert_eq!(next.configuration.unit, receipt.unit);
        assert_eq!(
            next.configuration.unit.previous,
            request.configuration.unit.previous
        );
        assert_eq!(
            next.configuration.unit.current.configuration,
            request.configuration.unit.current.configuration + 0x1_0000_0001
        );
        peers[0].send(support::completion(&original)).await.unwrap();
        peers[0].send(report).await.unwrap();
        let replay = receive_action(&peers[0], "ChangeConfigurationReply", &[&original]).await;
        assert_eq!(
            decode_change_configuration_reply(&replay).unwrap().0,
            receipt
        );
        assert_eq!(observed.borrow().phase, Phase::Reconfiguring);
        assert_eq!(observed.borrow().primary_node_id, "2");
        assert!(observed.borrow().service_endpoint.is_none());
        peers[2]
            .send(support::completion(&redirected))
            .await
            .unwrap();
        wait_status(&mut observed, |status| status.phase == Phase::Restoring).await;
        assert_eq!(observed.borrow().primary_node_id, "3");
        let deletion = receive(&peers[1]).await;
        completed_requests[1].push(deletion.clone());
        assert_eq!(wire::action(&deletion).unwrap(), "DeleteReplica");
        peers[1].send(support::completion(&deletion)).await.unwrap();
        let build = receive_action(&peers[2], "AddReplica", &[&redirected]).await;
        completed_requests[2].push(build.clone());
        peers[2].send(support::completion(&build)).await.unwrap();
        let admission =
            receive_action(&peers[2], "DoReconfiguration", &[&redirected, &build]).await;
        completed_requests[2].push(admission.clone());
        peers[2]
            .send(support::completion(&admission))
            .await
            .unwrap();
        wait_status(&mut observed, |status| status.phase == Phase::Serving).await;
        assert_eq!(observed.borrow().primary_node_id, "3");
    }
    stop.send(()).unwrap();
    let order = if redirect { [0, 1, 2] } else { [0, 2, 1] };
    for (count, node) in order.into_iter().enumerate() {
        let superseded = completed_requests[node].iter().collect::<Vec<_>>();
        let request = receive_action(&peers[node], "DeleteReplica", &superseded).await;
        assert_eq!(wire::action(&request).unwrap(), "DeleteReplica");
        assert!(observed.borrow().service_endpoint.is_none());
        peers[node]
            .send(support::completion(&request))
            .await
            .unwrap();
        wait_status(&mut observed, |status| status.deleted_nodes > count).await;
        peers[node].close().await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(3), fm)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(observed.borrow().phase, Phase::Stopped);
}

#[tokio::test]
async fn native_source_incarnation_mismatch_fails_without_placement_or_reconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = support::config();
    config.fm.address = match listener.local_addr().unwrap() {
        std::net::SocketAddr::V4(address) => address,
        _ => unreachable!(),
    };
    let (status, observed) = watch::channel(State::new(config.clone()).unwrap().status());
    let fm = tokio::spawn(kuberic_fm::run(
        config.clone(),
        listener,
        std::future::pending::<Result<()>>(),
        move |value| {
            status.send_replace(value.clone());
            Ok(())
        },
    ));
    let peer = connect(&config, 0, 2).await;
    assert!(peer
        .node_up(&decode_node_up(&support::node_up(&config, 0)).unwrap())
        .await
        .is_err());
    let error = tokio::time::timeout(Duration::from_secs(4), fm)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(format!("{error:#}").contains("incarnation"));
    assert_eq!(observed.borrow().phase, Phase::ControlUnknown);
    assert!(observed.borrow().service_endpoint.is_none());
    peer.close().await.unwrap();
}
