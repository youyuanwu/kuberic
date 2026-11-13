mod support;

use std::time::Duration;

use anyhow::Result;
use fube_node::control::wire::{self, Link};
use fube_ra::protocol::*;
use fube_transport::{message::Message, Endpoint};
use kuberic_fm::{Config, Phase, State, Status};
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch},
    time::Instant,
};

async fn connect(config: &Config, node: usize, instance: u64) -> Link {
    let channel =
        Endpoint::unsecured_tcp(config.fm.address.into(), wire::composition(false).unwrap())
            .unwrap()
            .local_ip((*config.nodes[node].address.ip()).into())
            .channel_limits(wire::limits())
            .connect_until(Instant::now() + Duration::from_secs(3))
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
    let mut incoming = link
        .receive(Instant::now() + Duration::from_secs(3))
        .await
        .unwrap()
        .expect("native command deadline");
    if let Some(handoff) = incoming.handoff.as_mut() {
        handoff.acknowledge();
    }
    incoming.message
}

async fn wait_status(receiver: &mut watch::Receiver<Status>, predicate: impl Fn(&Status) -> bool) {
    tokio::time::timeout(Duration::from_secs(3), receiver.wait_for(predicate))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn native_channels_drive_scripted_bootstrap_retry_create_and_owned_teardown() {
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
        let reply = link.request(support::node_up(&config, node)).await.unwrap();
        let (ack, generation) = decode_node_up_ack(&reply).unwrap();
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
    for step in 0..4 {
        let request = receive(&peers[1]).await;
        if step == 1 || step == 3 {
            let retry = receive(&peers[1]).await;
            assert_eq!(retry.body, request.body);
            assert_eq!(
                retry.properties().unwrap().action,
                request.properties().unwrap().action
            );
        }
        assert!(observed.borrow().service_endpoint.is_none());
        peers[1].send(support::completion(&request)).await.unwrap();
    }
    wait_status(&mut observed, |status| status.phase == Phase::Serving).await;
    assert_eq!(
        observed.borrow().service_endpoint.as_deref(),
        Some("client://node-20/opaque")
    );
    stop.send(()).unwrap();
    for (count, node) in [0, 2, 1].into_iter().enumerate() {
        let request = receive(&peers[node]).await;
        assert_eq!(request.properties().unwrap().action, "DeleteReplica");
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
    assert!(peer.request(support::node_up(&config, 0)).await.is_err());
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
