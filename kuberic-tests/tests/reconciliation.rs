use kuberic_tests::fm as support;

use std::time::Duration;

use fube_ra::protocol::*;
use fube_transport::serialization::serde::ObjectArray;
use kuberic_fm::{Phase, State};

use support::*;

#[test]
fn create_waits_for_semantic_replies_and_publishes_only_final_native_configuration() {
    let config = config();
    let mut state = State::new(config.clone()).unwrap();
    assert!(state.reconcile(Duration::ZERO).unwrap().is_empty());
    bootstrap(&mut state, &config);

    let mut actions = Vec::new();
    for step in 0..4 {
        let now = Duration::from_millis(step * 2);
        let outgoing = state.reconcile(now).unwrap().pop().unwrap();
        actions.push(outgoing.message.properties().unwrap().action);
        assert_eq!(
            outgoing.node, 1,
            "all create/build/configuration work targets the selected primary"
        );
        assert!(state.status().service_endpoint.is_none());
        assert!(state
            .reconcile(now + Duration::from_millis(1))
            .unwrap()
            .is_empty());
        let reply = completion(&outgoing.message);
        state.observe(outgoing.node, &reply).unwrap();
    }
    assert_eq!(
        actions,
        [
            "AddPrimary",
            "AddReplica",
            "AddReplica",
            "DoReconfiguration"
        ]
    );
    assert_eq!(state.phase(), Phase::Serving);
    assert_eq!(
        state.status().service_endpoint.as_deref(),
        Some("client://node-20/opaque")
    );
    assert_eq!(
        state.status().replication_endpoint.as_deref(),
        Some("opaque/replica=20/incarnation=original")
    );
}

#[test]
fn retry_is_exact_and_completed_duplicates_do_not_advance_the_next_operation() {
    let config = config();
    let mut state = State::new(config.clone()).unwrap();
    bootstrap(&mut state, &config);
    let original = state.reconcile(Duration::ZERO).unwrap();
    assert!(state
        .reconcile(Duration::from_millis(99))
        .unwrap()
        .is_empty());
    assert_eq!(
        state.reconcile(Duration::from_millis(100)).unwrap(),
        original
    );
    let reply = completion(&original[0].message);
    state.observe(original[0].node, &reply).unwrap();
    let build = state.reconcile(Duration::from_millis(100)).unwrap();
    state.observe(original[0].node, &reply).unwrap();
    assert_eq!(state.phase(), Phase::Building);
    assert_eq!(state.reconcile(Duration::from_millis(200)).unwrap(), build);
    let (body, _) = decode_add_primary_reply(&reply).unwrap();
    assert!(state
        .observe(
            original[0].node,
            &add_primary_reply_message(&body, Owner::Fmm).unwrap()
        )
        .is_err());
    assert_eq!(state.phase(), Phase::ControlUnknown);
}

#[test]
fn invalid_or_failed_completion_is_sticky_and_never_grants_serving() {
    for invalid in 0..5 {
        let config = config();
        let mut state = State::new(config.clone()).unwrap();
        bootstrap(&mut state, &config);
        let outgoing = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
        let request = decode_add_primary(&outgoing.message).unwrap().0;
        let mut reply = ready_reply(&request);
        let mut source = outgoing.node;
        match invalid {
            0 => source = 0,
            1 => reply.replica.instance += 1,
            2 => reply.unit.current.configuration += 1,
            3 => reply.error = -1,
            4 => reply.replica.state = 1,
            _ => unreachable!(),
        }
        assert!(state
            .observe(
                source,
                &add_primary_reply_message(&reply, Owner::Fm).unwrap()
            )
            .is_err());
        assert_eq!(state.phase(), Phase::ControlUnknown);
        assert!(state.reconcile(Duration::from_millis(100)).is_err());
        assert!(state.status().service_endpoint.is_none());
        assert!(state
            .observe(outgoing.node, &completion(&outgoing.message))
            .is_err());
    }
}

#[test]
fn changed_generation_and_nonempty_bootstrap_inventory_are_not_recovery() {
    let config = config();
    for generation_change in [false, true] {
        let mut state = State::new(config.clone()).unwrap();
        let mut message = node_up(&config, 0);
        if generation_change {
            let mut generation = config.generation().unwrap();
            generation.number.generation += 1;
            message.headers.push(generation.header().unwrap());
        } else {
            state.observe(0, &message).unwrap();
            let report = ReplicaUp {
                replicas: ObjectArray(vec![FailoverUnitInfo::default()]),
                last: true,
                ..ReplicaUp::default()
            };
            message = replica_up_message(&report, Owner::Fm).unwrap();
        }
        assert!(state.observe(0, &message).is_err());
        assert_eq!(state.phase(), Phase::ControlUnknown);
    }
}

#[test]
fn final_completion_must_preserve_exact_membership_and_endpoints() {
    for changed_member in [false, true] {
        let config = config();
        let mut state = State::new(config.clone()).unwrap();
        bootstrap(&mut state, &config);
        for _ in 0..3 {
            let outgoing = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
            state
                .observe(outgoing.node, &completion(&outgoing.message))
                .unwrap();
        }
        let outgoing = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
        let mut reply =
            completed_configuration(&decode_do_reconfiguration(&outgoing.message).unwrap().0);
        if changed_member {
            reply.replicas.0[1] = reply.replicas.0[0].clone();
        } else {
            reply.replicas.0[0].service_endpoint.push_str("/changed");
        }
        assert!(state
            .observe(
                outgoing.node,
                &do_reconfiguration_reply_message(&reply, Owner::Fm).unwrap()
            )
            .is_err());
        assert!(state.status().service_endpoint.is_none());
    }
}

#[test]
fn type_notifications_are_acknowledged_but_are_not_a_placement_prerequisite() {
    let config = config();
    let mut state = State::new(config.clone()).unwrap();
    bootstrap(&mut state, &config);
    let request = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
    assert_eq!(request.message.properties().unwrap().action, "AddPrimary");
    let service = config.service.native();
    let body = ServiceTypeNotification {
        infos: ObjectArray(vec![ServiceTypeInfo {
            versioned: VersionedServiceType {
                id: service.service_type,
                version: service.version,
            },
            code_package: "Code".into(),
        }]),
    };
    let response = state
        .observe(1, &service_type_notification_message(&body).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        decode_service_type_notification_reply(&response).unwrap().0,
        body
    );
    assert_eq!(state.phase(), Phase::Creating);
    assert!(state.status().service_endpoint.is_none());
}

#[test]
fn shutdown_requires_delete_replies_then_original_channel_disconnects() {
    let config = config();
    let mut state = State::new(config.clone()).unwrap();
    bootstrap(&mut state, &config);
    serving(&mut state);
    state.request_shutdown(Duration::ZERO).unwrap();
    assert!(state.status().service_endpoint.is_none());
    let mut targets = Vec::new();
    for _ in 0..3 {
        let outgoing = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
        let request = decode_delete_replica(&outgoing.message).unwrap().0;
        assert!(!request.deletion.0.force);
        targets.push(outgoing.node);
        state
            .observe(outgoing.node, &completion(&outgoing.message))
            .unwrap();
    }
    assert_eq!(targets, [0, 2, 1]);
    assert_eq!(state.phase(), Phase::AwaitingNodeShutdown);
    for node in targets {
        state.disconnected(node).unwrap();
    }
    assert_eq!(state.phase(), Phase::Stopped);
}

#[test]
fn lost_original_channel_withdraws_publication_without_promotion() {
    let config = config();
    let mut state = State::new(config.clone()).unwrap();
    bootstrap(&mut state, &config);
    serving(&mut state);
    assert!(state.disconnected(1).is_err());
    assert_eq!(state.phase(), Phase::ControlUnknown);
    assert!(state.status().service_endpoint.is_none());
    assert!(state.reconcile(Duration::from_millis(100)).is_err());
}

#[test]
fn missing_reply_and_early_shutdown_fail_explicitly() {
    let config = config();
    let mut state = State::new(config.clone()).unwrap();
    bootstrap(&mut state, &config);
    state.reconcile(Duration::ZERO).unwrap();
    assert!(state
        .reconcile(Duration::from_millis(config.operation_timeout_ms))
        .is_err());
    assert_eq!(state.phase(), Phase::ControlUnknown);

    let mut state = State::new(config).unwrap();
    assert!(state.request_shutdown(Duration::ZERO).is_err());
    assert_eq!(state.phase(), Phase::ControlUnknown);
}

#[test]
fn configuration_rejects_unsupported_or_ambiguous_control_topology() {
    let base = config();
    let mut cases = Vec::new();
    let mut invalid = base.clone();
    invalid.nodes[0].address.set_port(0);
    cases.push(invalid);
    let mut invalid = base.clone();
    invalid.nodes[0].address.set_ip(*base.nodes[1].address.ip());
    cases.push(invalid);
    let mut invalid = base.clone();
    invalid.nodes[0].identity.id = "f".into();
    cases.push(invalid);
    let mut invalid = base.clone();
    invalid.nodes[0].replica_id = base.nodes[1].replica_id;
    cases.push(invalid);
    let mut invalid = base.clone();
    invalid.nodes.pop();
    cases.push(invalid);
    let mut invalid = base;
    invalid.primary_node_id = "4".into();
    cases.push(invalid);
    for invalid in cases {
        assert!(State::new(invalid).is_err());
    }
}
