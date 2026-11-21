use kuberic_tests::fm as support;

use std::time::Duration;

use fube_node::control::wire;
use fube_ra::protocol::*;
use fubst::serde::ObjectArray;
use kuberic_fm::{Phase, State};

use support::*;

fn start_failover(state: &mut State, config: &kuberic_fm::Config) -> kuberic_fm::Outbound {
    let replies = serving(state);
    let committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
    state
        .observe(
            1,
            &replica_up_message(&current_app_host_loss(config, &committed, 1), Owner::Fm).unwrap(),
        )
        .unwrap();
    state.reconcile(Duration::ZERO).unwrap().pop().unwrap()
}

#[test]
fn candidate_redirection_retains_pc_and_requires_new_candidate_completion_before_repair_and_handoff(
) {
    for handoff in [false, true] {
        let config = config();
        let mut state = State::new(config.clone()).unwrap();
        bootstrap(&mut state, &config);
        let replies = serving(&mut state);
        let mut committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
        if handoff {
            state
                .request_switchover("1", committed.unit.current.configuration, Duration::ZERO)
                .unwrap();
            let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
            let reply = completion(&command.message);
            state.observe(0, &reply).unwrap();
            committed = decode_do_reconfiguration_reply(&reply).unwrap().0;
        }
        let lost = if handoff { 0 } else { 1 };
        state
            .observe(
                lost,
                &replica_up_message(&current_app_host_loss(&config, &committed, lost), Owner::Fm)
                    .unwrap(),
            )
            .unwrap();
        let initial = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
        let intent = decode_do_reconfiguration(&initial.message).unwrap().0;
        let selected = 2;
        let report = change_configuration_message(
            &candidate_change(&intent, &config.nodes[selected].identity.native().unwrap()),
            Owner::Fm,
        )
        .unwrap();
        let ack = state.observe(initial.node, &report).unwrap().unwrap();
        let (ack_body, generation) = decode_change_configuration_reply(&ack).unwrap();
        assert_eq!(generation, config.generation().unwrap());
        assert_eq!(ack_body.unit.previous, intent.configuration.unit.previous);
        assert_eq!(
            ack_body.unit.current.configuration,
            intent.configuration.unit.current.configuration + 0x1_0000_0001
        );
        assert_eq!(state.phase(), Phase::Reconfiguring);
        assert_eq!(
            state.status().configuration_epoch,
            Some(committed.unit.current.configuration)
        );
        assert_eq!(
            state.status().primary_node_id,
            config.nodes[lost].identity.id
        );
        assert!(state.status().service_endpoint.is_none());
        let redirected = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
        assert_eq!(redirected.node, selected);
        let next = decode_do_reconfiguration(&redirected.message).unwrap().0;
        assert_eq!(next.configuration.unit, ack_body.unit);
        assert_eq!(
            next.configuration.unit.current.data_loss,
            committed.unit.current.data_loss
        );
        for replica in &next.configuration.replicas.0 {
            let before = intent
                .configuration
                .replicas
                .0
                .iter()
                .find(|r| r.node == replica.node)
                .unwrap();
            let mut expected = before.clone();
            if expected.current_role == 4 {
                expected.current_role = 3;
            }
            if expected.node == config.nodes[selected].identity.native().unwrap() {
                expected.current_role = 4;
            }
            assert_eq!(replica, &expected);
            assert_eq!((replica.first_lsn, replica.last_lsn), (-1, -1));
        }
        let replay = state.observe(initial.node, &report).unwrap().unwrap();
        assert_message_eq(&replay, &ack);
        state
            .observe(initial.node, &completion(&initial.message))
            .unwrap();
        let old_reply = completed_configuration(&intent);
        let endpoint = AddInstance {
            unit: intent.configuration.unit.clone(),
            service: intent.configuration.service.clone(),
            replica: old_reply
                .replicas
                .0
                .iter()
                .find(|r| r.current_role == 4)
                .unwrap()
                .clone(),
        };
        assert!(state
            .observe(
                initial.node,
                &replica_endpoint_updated_message(&endpoint, Owner::Fm).unwrap()
            )
            .unwrap()
            .is_some());
        assert!(state.status().service_endpoint.is_none());
        assert_eq!(
            state.reconcile(Duration::from_millis(100)).unwrap(),
            vec![redirected.clone()]
        );
        state
            .observe(selected, &completion(&redirected.message))
            .unwrap();
        assert_eq!(state.phase(), Phase::Restoring);
        assert_eq!(
            state.status().primary_node_id,
            config.nodes[selected].identity.id
        );
        let replies = restore(&mut state, Duration::from_millis(100));
        let repaired = decode_do_reconfiguration_reply(&replies[2].1).unwrap().0;
        state.observe(initial.node, &report).unwrap();
        state
            .observe(initial.node, &completion(&initial.message))
            .unwrap();
        assert_eq!(state.phase(), Phase::Serving);
        let fresh = repaired
            .replicas
            .0
            .iter()
            .find(|r| r.node == config.nodes[lost].identity.native().unwrap())
            .unwrap();
        assert!(fresh.id > config.nodes[lost].replica_id);
        state
            .request_switchover(
                &config.nodes[lost].identity.id,
                repaired.unit.current.configuration,
                Duration::from_millis(100),
            )
            .unwrap();
        let command = state
            .reconcile(Duration::from_millis(100))
            .unwrap()
            .pop()
            .unwrap();
        state.observe(lost, &completion(&command.message)).unwrap();
        state.request_shutdown(Duration::from_millis(100)).unwrap();
        let mut deleted = Vec::new();
        for _ in 0..3 {
            let command = state
                .reconcile(Duration::from_millis(100))
                .unwrap()
                .pop()
                .unwrap();
            state
                .observe(command.node, &completion(&command.message))
                .unwrap();
            deleted.push(command.node);
        }
        assert_eq!(deleted.last(), Some(&lost));
        for node in 0..3 {
            state.disconnected(node).unwrap();
        }
        assert_eq!(state.phase(), Phase::Stopped);
    }
}

#[test]
fn candidate_change_rejects_changed_authority_and_ambiguous_or_ineligible_progress() {
    let config = config();
    for fault in 0..18 {
        let mut state = State::new(config.clone()).unwrap();
        bootstrap(&mut state, &config);
        let command = start_failover(&mut state, &config);
        let intent = decode_do_reconfiguration(&command.message).unwrap().0;
        let mut report = candidate_change(&intent, &config.nodes[2].identity.native().unwrap());
        let selected = report
            .replicas
            .0
            .iter()
            .position(|r| r.last_lsn >= 0)
            .unwrap();
        let mut source = command.node;
        let mut owner = Owner::Fm;
        match fault {
            0 => source = 2,
            1 => report.unit.current.configuration += 1,
            2 => report.unit.previous.configuration += 1,
            3 => report.unit.current.data_loss += 1,
            4 => report.unit.minimum = 1,
            5 => report.service.update_version += 1,
            6 => report.replicas.0[selected].instance += 1,
            7 => report.replicas.0[0] = report.replicas.0[selected].clone(),
            8 => report.replicas.0[selected]
                .replication_endpoint
                .push_str("/changed"),
            9 => report.replicas.0[selected].service_endpoint = "unconfirmed-writer".into(),
            10 => report.replicas.0[selected].current_role = 4,
            11 => report.replicas.0[selected].up = false,
            12 => report.replicas.0[selected].state = 1,
            13 => report.replicas.0[selected].first_lsn = -1,
            14 => {
                for r in &mut report.replicas.0 {
                    r.first_lsn = -1;
                    r.last_lsn = -1;
                }
            }
            15 => {
                report.replicas.0[0].first_lsn = 1;
                report.replicas.0[0].last_lsn = 99;
            }
            16 => report.replicas.0[0].first_lsn = 1,
            _ => owner = Owner::Fmm,
        }
        assert!(
            state
                .observe(
                    source,
                    &change_configuration_message(&report, owner).unwrap()
                )
                .is_err(),
            "fault {fault}"
        );
        assert_eq!(state.phase(), Phase::ControlUnknown);
        assert!(state.status().service_endpoint.is_none());
    }
}

#[test]
fn redirected_completion_never_accepts_a_new_epoch_reply_from_the_old_candidate() {
    let config = config();
    let mut state = State::new(config.clone()).unwrap();
    bootstrap(&mut state, &config);
    let original = start_failover(&mut state, &config);
    let request = decode_do_reconfiguration(&original.message).unwrap().0;
    state
        .observe(
            original.node,
            &change_configuration_message(
                &candidate_change(&request, &config.nodes[2].identity.native().unwrap()),
                Owner::Fm,
            )
            .unwrap(),
        )
        .unwrap();
    let redirected = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
    assert!(state
        .observe(original.node, &completion(&redirected.message))
        .is_err());
    assert_eq!(state.phase(), Phase::ControlUnknown);
    assert!(state.status().service_endpoint.is_none());
}

#[test]
fn redirection_budget_and_replica_admission_follow_each_new_repaired_failover() {
    let config = config();
    let mut state = State::new(config.clone()).unwrap();
    bootstrap(&mut state, &config);
    let replies = serving(&mut state);
    let mut committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
    for _ in 0..35 {
        let lost = config
            .nodes
            .iter()
            .position(|node| node.identity.id == state.status().primary_node_id)
            .unwrap();
        state
            .observe(
                lost,
                &replica_up_message(&current_app_host_loss(&config, &committed, lost), Owner::Fm)
                    .unwrap(),
            )
            .unwrap();
        let original = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
        let selected = (0..3)
            .find(|node| *node != lost && *node != original.node)
            .unwrap();
        let request = decode_do_reconfiguration(&original.message).unwrap().0;
        state
            .observe(
                original.node,
                &change_configuration_message(
                    &candidate_change(&request, &config.nodes[selected].identity.native().unwrap()),
                    Owner::Fm,
                )
                .unwrap(),
            )
            .unwrap();
        let redirected = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
        assert_eq!(redirected.node, selected);
        state
            .observe(selected, &completion(&redirected.message))
            .unwrap();
        let restored = restore(&mut state, Duration::ZERO);
        committed = decode_do_reconfiguration_reply(&restored[2].1).unwrap().0;
        assert_eq!(
            state.status().primary_node_id,
            config.nodes[selected].identity.id
        );
    }
}

#[test]
fn candidate_redirection_duplicates_preserve_deadlines_and_new_rounds_are_bounded() {
    let config = config();
    for exhaust in [false, true] {
        let mut state = State::new(config.clone()).unwrap();
        bootstrap(&mut state, &config);
        let mut command = start_failover(&mut state, &config);
        let mut retained = None;
        for round in 0..=config.nodes.len() {
            let intent = decode_do_reconfiguration(&command.message).unwrap().0;
            let selected = if command.node == 0 { 2 } else { 0 };
            let report = change_configuration_message(
                &candidate_change(&intent, &config.nodes[selected].identity.native().unwrap()),
                Owner::Fm,
            )
            .unwrap();
            let source = command.node;
            let result = state.observe(source, &report);
            if exhaust && round == config.nodes.len() {
                assert!(result.unwrap_err().to_string().contains("budget exhausted"));
                break;
            }
            let next = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
            let ack = result.unwrap().unwrap();
            assert_message_eq(&state.observe(source, &report).unwrap().unwrap(), &ack);
            command = next;
            if !exhaust {
                retained = Some((source, report));
                break;
            }
        }
        if let Some((source, report)) = retained {
            for now in (100..config.operation_timeout_ms).step_by(100) {
                state.observe(source, &report).unwrap();
                assert_eq!(
                    state.reconcile(Duration::from_millis(now)).unwrap(),
                    vec![command.clone()]
                );
            }
            assert!(state
                .reconcile(Duration::from_millis(config.operation_timeout_ms))
                .is_err());
        }
        assert_eq!(state.phase(), Phase::ControlUnknown);
        assert!(state.status().service_endpoint.is_none());
    }
}

#[test]
fn candidate_change_is_not_admitted_outside_an_issued_primary_failover() {
    for phase in 0..4 {
        let config = config();
        let mut state = State::new(config.clone()).unwrap();
        bootstrap(&mut state, &config);
        let replies = serving(&mut state);
        let committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
        let mut request = DoReconfiguration {
            configuration: Configuration {
                unit: committed.unit.clone(),
                service: config.service.native(),
                replicas: committed.replicas.clone(),
            },
            phase0_duration: i64::MAX,
        };
        let mut source = 0;
        if phase == 1 {
            state
                .request_switchover("1", committed.unit.current.configuration, Duration::ZERO)
                .unwrap();
            let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
            source = command.node;
            request = decode_do_reconfiguration(&command.message).unwrap().0;
        } else if phase >= 2 {
            let lost = if phase == 2 { 0 } else { 1 };
            state
                .observe(
                    lost,
                    &replica_up_message(
                        &current_app_host_loss(&config, &committed, lost),
                        Owner::Fm,
                    )
                    .unwrap(),
                )
                .unwrap();
            if phase == 2 {
                let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
                source = command.node;
                request = decode_do_reconfiguration(&command.message).unwrap().0;
            }
        }
        let report = change_configuration_message(
            &candidate_change(&request, &config.nodes[2].identity.native().unwrap()),
            Owner::Fm,
        )
        .unwrap();
        assert!(state.observe(source, &report).is_err(), "phase {phase}");
        assert_eq!(state.phase(), Phase::ControlUnknown);
        assert!(state.status().service_endpoint.is_none());
    }
}

#[test]
fn planned_switchover_retries_old_primary_and_only_accepts_final_candidate_completion() {
    for candidate in [0, 2] {
        let config = config();
        let mut state = State::new(config.clone()).unwrap();
        bootstrap(&mut state, &config);
        let original = serving(&mut state);
        let committed = decode_do_reconfiguration_reply(&original[3].1).unwrap().0;
        let epoch = committed.unit.current.configuration;
        let target = &config.nodes[candidate].identity.id;
        state
            .request_switchover(target, epoch, Duration::ZERO)
            .unwrap();
        assert_eq!(state.phase(), Phase::Switching);
        assert!(state.status().service_endpoint.is_none());
        assert_eq!(state.status().primary_node_id, config.primary_node_id);
        let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
        assert_eq!(command.node, 1);
        let request = decode_do_reconfiguration(&command.message).unwrap().0;
        assert_eq!(request.configuration.unit.previous, committed.unit.current);
        assert_eq!(
            request.configuration.unit.current.data_loss,
            committed.unit.current.data_loss
        );
        assert_eq!(
            request.configuration.unit.current.configuration,
            committed.unit.current.configuration + 0x1_0000_0001
        );
        assert_eq!(request.configuration.replicas.0.len(), 3);
        let candidate_node = config.nodes[candidate].identity.native().unwrap();
        assert!(request.configuration.replicas.0.iter().all(|replica| {
            replica.current_role == if replica.node == candidate_node { 4 } else { 3 }
                && replica.up
                && replica.state == 2
        }));
        state
            .request_switchover(target, epoch, Duration::ZERO)
            .unwrap();
        assert_eq!(
            state.reconcile(Duration::from_millis(100)).unwrap(),
            vec![command.clone()]
        );
        let final_reply = completed_configuration(&request);
        let endpoint = AddInstance {
            unit: request.configuration.unit.clone(),
            service: request.configuration.service.clone(),
            replica: final_reply
                .replicas
                .0
                .iter()
                .find(|r| r.node == candidate_node)
                .unwrap()
                .clone(),
        };
        assert!(state
            .observe(
                candidate,
                &replica_endpoint_updated_message(&endpoint, Owner::Fm).unwrap()
            )
            .unwrap()
            .is_some());
        assert_eq!(state.phase(), Phase::Switching);
        assert!(state.status().service_endpoint.is_none());
        state.observe(1, &original[3].1).unwrap();
        let reply = do_reconfiguration_reply_message(&final_reply, Owner::Fm).unwrap();
        state.observe(candidate, &reply).unwrap();
        state.observe(candidate, &reply).unwrap();
        assert_eq!(state.phase(), Phase::Serving);
        assert_eq!(&state.status().primary_node_id, target);
        assert!(state.status().switchover.unwrap().completed);
        state
            .request_switchover(target, epoch, Duration::from_millis(100))
            .unwrap();
        assert!(state
            .reconcile(Duration::from_millis(100))
            .unwrap()
            .is_empty());
        assert!(state
            .request_switchover(&config.primary_node_id, epoch, Duration::from_millis(100))
            .is_err());
        state.request_shutdown(Duration::from_millis(100)).unwrap();
        let mut deleted = Vec::new();
        for _ in 0..3 {
            let deletion = state
                .reconcile(Duration::from_millis(100))
                .unwrap()
                .pop()
                .unwrap();
            assert_eq!(
                decode_delete_replica(&deletion.message)
                    .unwrap()
                    .0
                    .replica
                    .0
                    .unit,
                final_reply.unit
            );
            deleted.push(deletion.node);
            state
                .observe(deletion.node, &completion(&deletion.message))
                .unwrap();
        }
        assert_eq!(deleted.last(), Some(&candidate));
    }
}

#[test]
fn switchover_rejects_unqualified_starts_and_invalid_or_missing_native_completion() {
    let config = config();
    let mut fresh = State::new(config.clone()).unwrap();
    assert!(fresh.request_switchover("1", 0, Duration::ZERO).is_err());
    bootstrap(&mut fresh, &config);
    let replies = serving(&mut fresh);
    let epoch = fresh.status().configuration_epoch.unwrap();
    for target in ["2", "foreign"] {
        assert!(fresh
            .request_switchover(target, epoch, Duration::ZERO)
            .is_err());
        assert_eq!(fresh.phase(), Phase::Serving);
    }
    let committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
    fresh
        .observe(
            0,
            &replica_up_message(&app_host_loss(&config, &committed.unit, 0), Owner::Fm).unwrap(),
        )
        .unwrap();
    assert!(fresh
        .request_switchover("3", epoch, Duration::ZERO)
        .is_err());
    let removal = fresh.reconcile(Duration::ZERO).unwrap().pop().unwrap();
    fresh
        .observe(removal.node, &completion(&removal.message))
        .unwrap();
    restore(&mut fresh, Duration::ZERO);
    assert!(fresh
        .request_switchover("3", epoch, Duration::ZERO)
        .is_err());
    let repaired_epoch = fresh.status().configuration_epoch.unwrap();
    fresh
        .request_switchover("3", repaired_epoch, Duration::ZERO)
        .unwrap();

    for fault in 0..11 {
        let mut state = State::new(config.clone()).unwrap();
        bootstrap(&mut state, &config);
        serving(&mut state);
        state
            .request_switchover("1", epoch, Duration::ZERO)
            .unwrap();
        let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
        let request = decode_do_reconfiguration(&command.message).unwrap().0;
        let mut reply = completed_configuration(&request);
        let mut source = 0;
        match fault {
            0 => source = 1,
            1 => reply.error = -1,
            2 => reply.unit.current.configuration += 1,
            3 => reply.replicas.0[0].instance += 1,
            4 => reply.replicas.0[0] = reply.replicas.0[1].clone(),
            5 => reply.replicas.0[0]
                .replication_endpoint
                .push_str("/changed"),
            6 => reply
                .replicas
                .0
                .iter_mut()
                .find(|r| r.current_role == 4)
                .unwrap()
                .service_endpoint
                .clear(),
            7 => {
                reply
                    .replicas
                    .0
                    .iter_mut()
                    .find(|r| r.current_role == 3)
                    .unwrap()
                    .service_endpoint = "stale-writer".into()
            }
            8 => {
                assert!(state
                    .reconcile(Duration::from_millis(config.operation_timeout_ms))
                    .is_err());
                assert_eq!(state.phase(), Phase::ControlUnknown);
                continue;
            }
            _ => {
                if fault == 10 {
                    state
                        .observe(
                            0,
                            &do_reconfiguration_reply_message(&reply, Owner::Fm).unwrap(),
                        )
                        .unwrap();
                }
                assert!(state
                    .observe(
                        2,
                        &replica_up_message(&app_host_loss(&config, &committed.unit, 2), Owner::Fm)
                            .unwrap()
                    )
                    .is_err());
                assert_eq!(state.phase(), Phase::ControlUnknown);
                continue;
            }
        }
        assert!(
            state
                .observe(
                    source,
                    &do_reconfiguration_reply_message(&reply, Owner::Fm).unwrap()
                )
                .is_err(),
            "fault {fault}"
        );
        assert_eq!(state.phase(), Phase::ControlUnknown);
        assert!(state.status().service_endpoint.is_none());
    }
}

#[test]
fn completed_handoff_recovers_each_lost_role_then_transfers_to_the_fresh_replica() {
    for candidate in [0, 2] {
        for victim in 0..3 {
            let config = config();
            let mut state = State::new(config.clone()).unwrap();
            bootstrap(&mut state, &config);
            serving(&mut state);
            let first_epoch = state.status().configuration_epoch.unwrap();
            state
                .request_switchover(
                    &config.nodes[candidate].identity.id,
                    first_epoch,
                    Duration::ZERO,
                )
                .unwrap();
            let handoff = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
            let reply = completion(&handoff.message);
            state.observe(candidate, &reply).unwrap();
            let membership = decode_do_reconfiguration_reply(&reply).unwrap().0;
            let predecessor = membership
                .replicas
                .0
                .iter()
                .find(|replica| replica.node == config.nodes[victim].identity.native().unwrap())
                .unwrap();
            state
                .observe(
                    victim,
                    &replica_up_message(
                        &current_app_host_loss(&config, &membership, victim),
                        Owner::Fm,
                    )
                    .unwrap(),
                )
                .unwrap();
            assert_eq!(
                state.phase(),
                if victim == candidate {
                    Phase::Reconfiguring
                } else {
                    Phase::Restoring
                }
            );
            let removal = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
            state.observe(candidate, &reply).unwrap();
            let removed = completion(&removal.message);
            state.observe(removal.node, &removed).unwrap();
            let restored = restore(&mut state, Duration::ZERO);
            let repaired = decode_do_reconfiguration_reply(&restored[2].1).unwrap().0;
            let fresh = repaired
                .replicas
                .0
                .iter()
                .find(|replica| replica.node == predecessor.node)
                .unwrap();
            assert!(fresh.id > predecessor.id && fresh.instance > predecessor.instance);
            let epoch = repaired.unit.current.configuration;
            state
                .request_switchover(&config.nodes[victim].identity.id, epoch, Duration::ZERO)
                .unwrap();
            let next = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
            let intent = decode_do_reconfiguration(&next.message).unwrap().0;
            assert!(intent.configuration.replicas.0.iter().any(|replica| {
                replica.node == fresh.node
                    && replica.id == fresh.id
                    && replica.instance == fresh.instance
                    && replica.current_role == 4
            }));
            for (source, old) in [
                (candidate, &reply),
                (removal.node, &removed),
                (restored[2].0, &restored[2].1),
            ] {
                state.observe(source, old).unwrap();
                assert_eq!(state.phase(), Phase::Switching);
                assert!(state.status().service_endpoint.is_none());
            }
            assert!(state
                .request_switchover(
                    &config.nodes[candidate].identity.id,
                    first_epoch,
                    Duration::ZERO
                )
                .is_err());
            state.observe(victim, &completion(&next.message)).unwrap();
            assert_eq!(state.phase(), Phase::Serving);
            assert_eq!(
                state.status().primary_node_id,
                config.nodes[victim].identity.id
            );
            assert_eq!(
                state
                    .status()
                    .switchover
                    .unwrap()
                    .previous_configuration_epoch,
                epoch
            );
        }
    }
}

#[test]
fn repeated_reverse_handoffs_use_current_epochs_without_a_replay_history_lifetime_limit() {
    let config = config();
    let mut state = State::new(config.clone()).unwrap();
    bootstrap(&mut state, &config);
    serving(&mut state);
    let mut primary = 1;
    for candidate in [0, 1, 2, 0].into_iter().cycle().take(40) {
        if candidate == primary {
            continue;
        }
        let epoch = state.status().configuration_epoch.unwrap();
        state
            .request_switchover(&config.nodes[candidate].identity.id, epoch, Duration::ZERO)
            .unwrap();
        let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
        assert_eq!(command.node, primary);
        let intent = decode_do_reconfiguration(&command.message).unwrap().0;
        assert_eq!(intent.configuration.unit.previous.configuration, epoch);
        assert!(state
            .request_switchover(&config.nodes[primary].identity.id, epoch, Duration::ZERO)
            .is_err());
        let reply = completion(&command.message);
        state.observe(candidate, &reply).unwrap();
        state.observe(candidate, &reply).unwrap();
        state
            .request_switchover(&config.nodes[candidate].identity.id, epoch, Duration::ZERO)
            .unwrap();
        assert!(state.reconcile(Duration::ZERO).unwrap().is_empty());
        assert_eq!(
            state.status().configuration_epoch,
            Some(epoch + 0x1_0000_0001)
        );
        primary = candidate;
    }
}

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
        actions.push(wire::action(&outgoing.message).unwrap());
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
    let split = reply.body().len() / 2;
    let body = furpc::Body::from_segments(vec![
        reply.body().slice(0..split).unwrap(),
        reply.body().slice(split..reply.body().len()).unwrap(),
    ])
    .unwrap();
    let mut duplicate = furpc::Message::builder().body(body);
    for header in reply.headers().records() {
        duplicate = duplicate.append_header(header.to_owned());
    }
    state
        .observe(original[0].node, &duplicate.build().unwrap())
        .unwrap();
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
            let mut builder = furpc::Message::builder().body(message.body().clone());
            for header in message.headers().records() {
                builder = builder.append_header(header.to_owned());
            }
            message = builder
                .append_header(generation.header().unwrap())
                .build()
                .unwrap();
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
    assert_eq!(wire::action(&request.message).unwrap(), "AddPrimary");
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
fn verified_primary_loss_reconfigures_original_quorum_and_deletes_each_retained_epoch() {
    let config = config();
    let mut state = State::new(config.clone()).unwrap();
    bootstrap(&mut state, &config);
    let replies = serving(&mut state);
    let committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
    let report = app_host_loss(&config, &committed.unit, 1);
    let loss = replica_up_message(&report, Owner::Fm).unwrap();
    assert!(state.observe(1, &loss).unwrap().is_some());
    assert_eq!(state.phase(), Phase::Reconfiguring);
    assert!(state.status().service_endpoint.is_none());
    let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
    assert_eq!(command.node, 0);
    let request = decode_do_reconfiguration(&command.message).unwrap().0;
    assert_eq!(request.configuration.unit.previous, committed.unit.current);
    assert_eq!(
        request.configuration.unit.current.data_loss,
        committed.unit.current.data_loss
    );
    assert_eq!(
        request.configuration.unit.current.configuration,
        committed.unit.current.configuration + 0x1_0000_0001
    );
    assert!(request
        .configuration
        .replicas
        .0
        .iter()
        .all(|replica| replica.first_lsn == -1 && replica.last_lsn == -1));
    state.observe(1, &loss).unwrap();
    assert_eq!(
        state.reconcile(Duration::from_millis(100)).unwrap(),
        vec![command.clone()]
    );
    state.observe(0, &completion(&command.message)).unwrap();
    assert_eq!(state.phase(), Phase::Restoring);
    assert_eq!(state.status().primary_node_id, config.nodes[0].identity.id);
    assert_eq!(
        state.status().service_endpoint.as_deref(),
        Some("client://node-10/opaque")
    );
    state.observe(1, &loss).unwrap();
    let replies = restore(&mut state, Duration::from_millis(100));
    let retired = decode_delete_replica_reply(&replies[0].1).unwrap().0;
    assert_eq!(retired.unit, committed.unit);
    let fresh = decode_add_replica_reply(&replies[1].1).unwrap().0;
    assert!(
        fresh.replica.id
            > config
                .nodes
                .iter()
                .map(|node| node.replica_id)
                .max()
                .unwrap()
    );
    assert!(fresh.replica.instance > config.nodes[1].replica_instance);
    let restored = decode_do_reconfiguration_reply(&replies[2].1).unwrap().0;
    assert_eq!(restored.replicas.0.len(), config.nodes.len());
    assert_eq!(
        restored.unit.current.configuration,
        request.configuration.unit.current.configuration + 1
    );
    state.observe(1, &loss).unwrap();
    assert!(state
        .reconcile(Duration::from_millis(100))
        .unwrap()
        .is_empty());
    state.request_shutdown(Duration::from_millis(100)).unwrap();
    for target in [1, 2, 0] {
        let deletion = state
            .reconcile(Duration::from_millis(100))
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(deletion.node, target);
        let body = decode_delete_replica(&deletion.message).unwrap().0;
        assert!(!body.deletion.0.force);
        assert_eq!(body.replica.0.unit.current, restored.unit.current);
        state
            .observe(target, &completion(&deletion.message))
            .unwrap();
    }
    for node in 0..3 {
        state.disconnected(node).unwrap();
    }
    assert_eq!(state.phase(), Phase::Stopped);
}

#[test]
fn restored_original_node_can_fail_over_again_without_reusing_a_retired_replica() {
    let config = config();
    let mut state = State::new(config.clone()).unwrap();
    bootstrap(&mut state, &config);
    let replies = serving(&mut state);
    let mut committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
    for node in [1, 0, 1] {
        let mut report = app_host_loss(&config, &committed.unit, node);
        let replica = committed
            .replicas
            .0
            .iter()
            .find(|replica| replica.node == config.nodes[node].identity.native().unwrap())
            .unwrap()
            .clone();
        report.dropped.0[0].replicas.0[0].description.id = replica.id;
        report.dropped.0[0].replicas.0[0].description.instance = replica.instance;
        state
            .observe(node, &replica_up_message(&report, Owner::Fm).unwrap())
            .unwrap();
        let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
        state
            .observe(command.node, &completion(&command.message))
            .unwrap();
        let replies = restore(&mut state, Duration::ZERO);
        committed = decode_do_reconfiguration_reply(&replies[2].1).unwrap().0;
        let fresh = committed
            .replicas
            .0
            .iter()
            .find(|replica| replica.node == config.nodes[node].identity.native().unwrap())
            .unwrap();
        assert!(fresh.id > replica.id && fresh.instance > replica.instance);
        assert_eq!(
            committed.unit.current.data_loss,
            report.dropped.0[0].unit.current.data_loss
        );
    }
}

#[test]
fn repair_requires_old_deletion_fresh_build_and_exact_final_configuration() {
    for (node, invalid) in (0..3).flat_map(|invalid| [0, 1].map(|node| (node, invalid))) {
        let config = config();
        let mut state = State::new(config.clone()).unwrap();
        bootstrap(&mut state, &config);
        let replies = serving(&mut state);
        let committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
        state
            .observe(
                node,
                &replica_up_message(&app_host_loss(&config, &committed.unit, node), Owner::Fm)
                    .unwrap(),
            )
            .unwrap();
        let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
        state
            .observe(command.node, &completion(&command.message))
            .unwrap();
        for stage in 0..=invalid {
            let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
            let mut reply = completion(&command.message);
            if stage == invalid {
                reply = match stage {
                    0 => {
                        let mut body = decode_delete_replica_reply(&reply).unwrap().0;
                        body.error = -1;
                        delete_replica_reply_message(&body, Owner::Fm).unwrap()
                    }
                    1 => {
                        let mut body = decode_add_replica_reply(&reply).unwrap().0;
                        body.replica.id = config.nodes[node].replica_id;
                        add_replica_reply_message(&body, Owner::Fm).unwrap()
                    }
                    _ => {
                        let mut body = decode_do_reconfiguration_reply(&reply).unwrap().0;
                        body.unit.current.configuration += 1;
                        do_reconfiguration_reply_message(&body, Owner::Fm).unwrap()
                    }
                };
                assert!(state.observe(command.node, &reply).is_err());
            } else {
                state.observe(command.node, &reply).unwrap();
            }
        }
        assert_eq!(state.phase(), Phase::ControlUnknown);
        assert!(state.status().service_endpoint.is_none());
    }
}

#[test]
fn invalid_loss_history_and_lost_survivor_never_grant_promotion() {
    for (node, invalid) in (1..6).flat_map(|invalid| [0, 1].map(|node| (node, invalid))) {
        let config = config();
        let mut state = State::new(config.clone()).unwrap();
        bootstrap(&mut state, &config);
        let replies = serving(&mut state);
        let committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
        let mut report = app_host_loss(&config, &committed.unit, node);
        match invalid {
            1 => report.dropped.0[0].unit.current.configuration += 1,
            2 => report.dropped.0[0].replicas.0[0].description.instance += 1,
            3 => report.dropped.0[0].replicas.0[0].description.up = true,
            _ => {}
        }
        let result = state.observe(node, &replica_up_message(&report, Owner::Fm).unwrap());
        if invalid == 4 {
            result.unwrap();
            assert!(state.disconnected(2).is_err());
        } else if invalid == 5 {
            result.unwrap();
            let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
            let reply = completion(&command.message);
            state.observe(command.node, &reply).unwrap();
            let committed = decode_do_reconfiguration_reply(&reply).unwrap().0;
            let report = app_host_loss(&config, &committed.unit, command.node);
            assert!(state
                .observe(
                    command.node,
                    &replica_up_message(&report, Owner::Fm).unwrap()
                )
                .is_err());
        } else {
            assert!(result.is_err());
        }
        assert_eq!(state.phase(), Phase::ControlUnknown);
        assert!(state.status().service_endpoint.is_none());
        assert!(state.reconcile(Duration::ZERO).is_err());
    }
}

#[test]
fn repeated_secondary_loss_reports_do_not_extend_any_repair_stage_deadline() {
    for stage in 0..4 {
        let config = config();
        let mut state = State::new(config.clone()).unwrap();
        bootstrap(&mut state, &config);
        let replies = serving(&mut state);
        let committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
        let report =
            replica_up_message(&current_app_host_loss(&config, &committed, 0), Owner::Fm).unwrap();
        state.observe(0, &report).unwrap();
        for _ in 0..stage {
            let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
            state
                .observe(command.node, &completion(&command.message))
                .unwrap();
        }
        state.reconcile(Duration::ZERO).unwrap();
        state.observe(0, &report).unwrap();
        assert!(state
            .reconcile(Duration::from_millis(config.operation_timeout_ms))
            .is_err());
        assert_eq!(state.phase(), Phase::ControlUnknown);
        assert!(state.status().service_endpoint.is_none());
    }
}

#[test]
fn secondary_loss_removes_exact_predecessor_and_repairs_without_changing_primary() {
    let config = config();
    let mut state = State::new(config.clone()).unwrap();
    bootstrap(&mut state, &config);
    let replies = serving(&mut state);
    let committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
    let endpoint = state.status().service_endpoint;
    let report = current_app_host_loss(&config, &committed, 0);
    let loss = replica_up_message(&report, Owner::Fm).unwrap();
    state.observe(0, &loss).unwrap();
    assert_eq!(state.phase(), Phase::Restoring);
    assert_eq!(state.status().restoring_node_id.as_deref(), Some("1"));
    assert_eq!(state.status().service_endpoint, endpoint);
    let removal = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
    assert_eq!(removal.node, 1);
    let request = decode_do_reconfiguration(&removal.message).unwrap().0;
    assert_eq!(request.configuration.unit.previous, committed.unit.current);
    assert_eq!(
        request.configuration.unit.current.configuration,
        committed.unit.current.configuration + 1
    );
    assert_eq!(
        request.configuration.unit.current.data_loss,
        committed.unit.current.data_loss
    );
    for replica in &request.configuration.replicas.0 {
        let previous = committed
            .replicas
            .0
            .iter()
            .find(|old| old.node == replica.node)
            .unwrap();
        assert_eq!(replica.previous_role, previous.current_role);
        if replica.node == config.nodes[0].identity.native().unwrap() {
            assert_eq!(replica.current_role, 1);
            assert!(!replica.up);
            assert_eq!(replica.state, 3);
        } else {
            assert_eq!(replica.current_role, previous.current_role);
            assert_eq!(replica.service_endpoint, previous.service_endpoint);
        }
    }
    state.observe(0, &loss).unwrap();
    assert_eq!(
        state.reconcile(Duration::from_millis(100)).unwrap(),
        vec![removal.clone()]
    );
    let reduced = completion(&removal.message);
    state.observe(1, &reduced).unwrap();
    let deletion = state
        .reconcile(Duration::from_millis(100))
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(deletion.node, 0);
    assert_eq!(
        decode_delete_replica(&deletion.message)
            .unwrap()
            .0
            .replica
            .0
            .unit,
        committed.unit
    );
    state.observe(1, &reduced).unwrap();
    state.observe(0, &loss).unwrap();
    assert_eq!(state.status().service_endpoint, endpoint);
    state.observe(0, &completion(&deletion.message)).unwrap();
    let build = state
        .reconcile(Duration::from_millis(100))
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(build.node, 1);
    let fresh = decode_add_replica(&build.message).unwrap().0;
    assert!(
        fresh.replica.id
            > config
                .nodes
                .iter()
                .map(|node| node.replica_id)
                .max()
                .unwrap()
    );
    state.observe(1, &completion(&build.message)).unwrap();
    assert_eq!(state.phase(), Phase::Restoring);
    assert_eq!(state.status().service_endpoint, endpoint);
    let admission = state
        .reconcile(Duration::from_millis(100))
        .unwrap()
        .pop()
        .unwrap();
    let reply = completion(&admission.message);
    state.observe(1, &reply).unwrap();
    assert_eq!(state.phase(), Phase::Serving);
    assert_eq!(state.status().primary_node_id, "2");
    assert_eq!(state.status().service_endpoint, endpoint);
    let restored = decode_do_reconfiguration_reply(&reply).unwrap().0;
    assert_eq!(restored.replicas.0.len(), 3);
    assert_eq!(
        restored.unit.current.configuration,
        committed.unit.current.configuration + 2
    );
    state.observe(0, &loss).unwrap();
    assert!(state
        .reconcile(Duration::from_millis(100))
        .unwrap()
        .is_empty());
    state.request_shutdown(Duration::from_millis(100)).unwrap();
    let deletion = state
        .reconcile(Duration::from_millis(100))
        .unwrap()
        .pop()
        .unwrap();
    let target = decode_delete_replica(&deletion.message)
        .unwrap()
        .0
        .replica
        .0;
    assert_eq!(target.unit, restored.unit);
    assert_eq!(target.replica.id, fresh.replica.id);
    assert_eq!(target.replica.instance, fresh.replica.instance);
}

#[test]
fn secondary_removal_requires_exact_survivors_and_preserved_primary_endpoint() {
    for invalid in 0..4 {
        let config = config();
        let mut state = State::new(config.clone()).unwrap();
        bootstrap(&mut state, &config);
        let replies = serving(&mut state);
        let committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
        state
            .observe(
                0,
                &replica_up_message(&current_app_host_loss(&config, &committed, 0), Owner::Fm)
                    .unwrap(),
            )
            .unwrap();
        let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
        let mut reply =
            completed_configuration(&decode_do_reconfiguration(&command.message).unwrap().0);
        match invalid {
            0 => reply.replicas.0[0].service_endpoint.push_str("/changed"),
            1 => reply.replicas.0[1] = reply.replicas.0[0].clone(),
            2 => reply.unit.current.configuration += 1,
            3 => reply.error = -1,
            _ => unreachable!(),
        }
        assert!(state
            .observe(
                command.node,
                &do_reconfiguration_reply_message(&reply, Owner::Fm).unwrap()
            )
            .is_err());
        assert_eq!(state.phase(), Phase::ControlUnknown);
        assert!(state.status().service_endpoint.is_none());
    }
}

#[test]
fn another_application_loss_during_each_secondary_repair_stage_blocks() {
    for stage in 0..4 {
        for other in [1, 2] {
            let config = config();
            let mut state = State::new(config.clone()).unwrap();
            bootstrap(&mut state, &config);
            let replies = serving(&mut state);
            let mut committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
            state
                .observe(
                    0,
                    &replica_up_message(&current_app_host_loss(&config, &committed, 0), Owner::Fm)
                        .unwrap(),
                )
                .unwrap();
            for _ in 0..stage {
                let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
                let reply = completion(&command.message);
                if wire::action(&command.message).unwrap() == "DoReconfiguration" {
                    committed = decode_do_reconfiguration_reply(&reply).unwrap().0;
                }
                state.observe(command.node, &reply).unwrap();
            }
            assert!(state
                .observe(
                    other,
                    &replica_up_message(
                        &current_app_host_loss(&config, &committed, other),
                        Owner::Fm
                    )
                    .unwrap()
                )
                .is_err());
            assert_eq!(state.phase(), Phase::ControlUnknown);
            assert!(state.status().service_endpoint.is_none());
        }
    }
}

#[test]
fn secondary_repair_outlives_replay_history_and_can_be_followed_by_primary_failover() {
    let config = config();
    let mut state = State::new(config.clone()).unwrap();
    bootstrap(&mut state, &config);
    let replies = serving(&mut state);
    let mut committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
    let original = current_app_host_loss(&config, &committed, 0);
    for cycle in 0..35 {
        let node = if cycle == 34 { 1 } else { cycle % 2 * 2 };
        let report = current_app_host_loss(&config, &committed, node);
        let message = replica_up_message(&report, Owner::Fm).unwrap();
        state.observe(node, &message).unwrap();
        let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
        state
            .observe(command.node, &completion(&command.message))
            .unwrap();
        let replies = restore(&mut state, Duration::ZERO);
        committed = decode_do_reconfiguration_reply(&replies[2].1).unwrap().0;
        let fresh = committed
            .replicas
            .0
            .iter()
            .find(|replica| replica.node == config.nodes[node].identity.native().unwrap())
            .unwrap();
        assert!(fresh.id > report.dropped.0[0].replicas.0[0].description.id);
        state.observe(node, &message).unwrap();
        assert!(state.reconcile(Duration::ZERO).unwrap().is_empty());
        assert_eq!(
            state.status().primary_node_id,
            if cycle == 34 { "1" } else { "2" }
        );
    }
    assert!(state
        .observe(0, &replica_up_message(&original, Owner::Fm).unwrap())
        .is_err());
    assert_eq!(state.phase(), Phase::ControlUnknown);
}

#[test]
fn repeated_loss_reports_do_not_extend_a_missing_failover_completion_deadline() {
    let config = config();
    let mut state = State::new(config.clone()).unwrap();
    bootstrap(&mut state, &config);
    let replies = serving(&mut state);
    let committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
    let report =
        replica_up_message(&app_host_loss(&config, &committed.unit, 1), Owner::Fm).unwrap();
    state.observe(1, &report).unwrap();
    state.reconcile(Duration::ZERO).unwrap();
    state.observe(1, &report).unwrap();
    assert!(state
        .reconcile(Duration::from_millis(config.operation_timeout_ms))
        .is_err());
    assert_eq!(state.phase(), Phase::ControlUnknown);
    assert!(state.status().service_endpoint.is_none());
}

#[test]
fn failover_completion_requires_exact_epoch_membership_quorum_and_success() {
    for invalid in 0..5 {
        let config = config();
        let mut state = State::new(config.clone()).unwrap();
        bootstrap(&mut state, &config);
        let replies = serving(&mut state);
        let committed = decode_do_reconfiguration_reply(&replies[3].1).unwrap().0;
        state
            .observe(
                1,
                &replica_up_message(&app_host_loss(&config, &committed.unit, 1), Owner::Fm)
                    .unwrap(),
            )
            .unwrap();
        let command = state.reconcile(Duration::ZERO).unwrap().pop().unwrap();
        let mut reply =
            completed_configuration(&decode_do_reconfiguration(&command.message).unwrap().0);
        match invalid {
            0 => reply.unit.current.configuration += 1,
            1 => {
                reply.replicas.0.pop();
            }
            2 => reply.replicas.0[1] = reply.replicas.0[0].clone(),
            3 => reply.replicas.0[0].instance += 1,
            4 => reply.error = -1,
            _ => unreachable!(),
        }
        assert!(state
            .observe(
                command.node,
                &do_reconfiguration_reply_message(&reply, Owner::Fm).unwrap()
            )
            .is_err());
        assert_eq!(state.phase(), Phase::ControlUnknown);
        assert!(state.status().service_endpoint.is_none());
    }
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
