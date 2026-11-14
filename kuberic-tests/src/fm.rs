use std::time::Duration;

use fube_ra::protocol::*;
use fube_transport::{message::Message, serialization::serde::ObjectArray};
use kuberic_fm::{Config, Fm, Identity, Placement, ServiceConfig, State};

pub fn config() -> Config {
    Config {
        schema_version: 1,
        deployment: "11111111-1111-4111-8111-111111111111".into(),
        fm: Fm {
            identity: Identity {
                id: "f".into(),
                instance: 1,
            },
            address: "127.0.0.1:19001".parse().unwrap(),
            generation: 1,
        },
        nodes: (1..=3)
            .map(|id| Placement {
                identity: Identity {
                    id: format!("{id:x}"),
                    instance: 1,
                },
                address: format!("127.0.0.{}:19002", id + 1).parse().unwrap(),
                replica_id: id * 10,
                replica_instance: 1,
            })
            .collect(),
        partition: "22222222-2222-4222-8222-222222222222".into(),
        primary_node_id: "2".into(),
        service: ServiceConfig {
            application: "fabric:/External".into(),
            application_type: "HostingLocalStatefulApp".into(),
            application_number: 1,
            package: "LocalService".into(),
            service_type: "LocalStatefulType".into(),
            name: "fabric:/External/Singleton".into(),
            package_version_instance: 1,
        },
        retry_interval_ms: 100,
        operation_timeout_ms: 5_000,
        shutdown_timeout_ms: 5_000,
    }
}

pub fn node_up(config: &Config, node: usize) -> Message {
    node_up_message(
        &NodeUp::fresh_stateless(
            format!("node{}", node + 1),
            "fube-node".into(),
            config.nodes[node].address.into(),
            FabricVersionInstance::native_default(),
        )
        .unwrap(),
    )
    .unwrap()
}

pub fn bootstrap(state: &mut State, config: &Config) {
    for node in 0..3 {
        let response = state
            .observe(node, &node_up(config, node))
            .unwrap()
            .unwrap();
        let (ack, generation) = decode_node_up_ack(&response).unwrap();
        ack.validate_fresh_activated(&FabricVersionInstance::native_default())
            .unwrap();
        assert_eq!(generation, config.generation().unwrap());
        let report = ReplicaUp {
            last: true,
            ..ReplicaUp::default()
        };
        let response = state
            .observe(node, &replica_up_message(&report, Owner::Fm).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(decode_replica_up_reply(&response).unwrap().0, report);
    }
}

pub fn ready_reply(request: &AddInstance) -> AddInstanceReply {
    let mut replica = request.replica.clone();
    replica.state = 2;
    replica.replication_endpoint = format!("opaque/replica={}/incarnation=original", replica.id);
    if replica.current_role == 4 {
        replica.service_endpoint = format!("client://node-{}/opaque", replica.id);
    }
    AddInstanceReply {
        unit: request.unit.clone(),
        replica,
        error: 0,
    }
}

pub fn completed_configuration(request: &DoReconfiguration) -> ConfigurationReply {
    let mut unit = request.configuration.unit.clone();
    unit.previous = Epoch::default();
    let mut replicas = request.configuration.replicas.0.clone();
    for replica in &mut replicas {
        replica.previous_role = 1;
        if replica.current_role == 4 && replica.service_endpoint.is_empty() {
            replica.service_endpoint = format!("client://node-{}/opaque", replica.id);
        }
    }
    replicas.retain(|replica| replica.current_role >= 3);
    ConfigurationReply {
        unit,
        replicas: ObjectArray(replicas),
        error: 0,
    }
}

pub fn app_host_loss(config: &Config, unit: &FailoverUnit, node: usize) -> ReplicaUp {
    let placement = &config.nodes[node];
    ReplicaUp {
        dropped: ObjectArray(vec![FailoverUnitInfo {
            service: config.service.native(),
            unit: unit.clone(),
            replicas: ObjectArray(vec![ReplicaInfo {
                description: Replica {
                    node: placement.identity.native().unwrap(),
                    id: placement.replica_id,
                    instance: placement.replica_instance,
                    up: false,
                    state: 3,
                    ..Replica::default()
                },
                intermediate_role: 1,
            }]),
            ..FailoverUnitInfo::default()
        }]),
        ..ReplicaUp::default()
    }
}

pub fn completion(message: &Message) -> Message {
    match message.properties().unwrap().action.as_str() {
        "AddPrimary" => add_primary_reply_message(
            &ready_reply(&decode_add_primary(message).unwrap().0),
            Owner::Fm,
        )
        .unwrap(),
        "AddReplica" => add_replica_reply_message(
            &ready_reply(&decode_add_replica(message).unwrap().0),
            Owner::Fm,
        )
        .unwrap(),
        "DoReconfiguration" => do_reconfiguration_reply_message(
            &completed_configuration(&decode_do_reconfiguration(message).unwrap().0),
            Owner::Fm,
        )
        .unwrap(),
        "DeleteReplica" => {
            let request = decode_delete_replica(message).unwrap().0.replica.0;
            delete_replica_reply_message(
                &AddInstanceReply {
                    unit: request.unit,
                    replica: Replica {
                        node: request.replica.node,
                        id: request.replica.id,
                        instance: request.replica.instance,
                        up: false,
                        state: 3,
                        ..Replica::default()
                    },
                    error: 0,
                },
                Owner::Fm,
            )
            .unwrap()
        }
        action => panic!("unexpected scripted command {action}"),
    }
}

pub fn serving(state: &mut State) -> Vec<(usize, Message)> {
    let mut replies = Vec::new();
    for _ in 0..4 {
        let outgoing = state.reconcile(Duration::ZERO).unwrap();
        assert_eq!(outgoing.len(), 1);
        let outgoing = &outgoing[0];
        let reply = completion(&outgoing.message);
        state.observe(outgoing.node, &reply).unwrap();
        replies.push((outgoing.node, reply));
    }

    replies
}

pub fn restore(state: &mut State, now: Duration) -> Vec<(usize, Message)> {
    let mut replies = Vec::new();
    for _ in 0..3 {
        assert_eq!(state.phase(), kuberic_fm::Phase::Restoring);
        let command = state.reconcile(now).unwrap().pop().unwrap();
        let reply = completion(&command.message);
        state.observe(command.node, &reply).unwrap();
        replies.push((command.node, reply));
    }
    assert_eq!(state.phase(), kuberic_fm::Phase::Serving);
    replies
}
