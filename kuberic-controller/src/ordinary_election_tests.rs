use super::*;

fn observation(id: i64, last: i64, first: i64, history: i64) -> NativeStatusObservation {
    let (_, _, peer) = super::super::retained_member_tests::fixture();
    let mut scope = peer.goals[0].goal.scope.clone();
    scope.storage.replica.replica_id = ReplicaId::new(id);
    NativeStatusObservation {
        input: NativeLifecycleFacts {
            operation_id: OperationId::new(format!("round-{id}")),
            revision: 12,
            scope,
            state: NativeReplicaState::Ready,
            native_role: ReplicaRole::ActiveSecondary,
            application_role: Some(ReplicaRole::ActiveSecondary),
            applied_primary_fence: Some(Epoch::new(0, 12)),
            deactivation: Some(NativeDeactivationInfo {
                primary_fence: Epoch::new(0, history),
                catch_up_lsn: last,
            }),
        },
        epoch: NativeInspectionEpoch::Applied,
        progress: ProgressQueryResult::Value(last),
        capability: ProgressQueryResult::Value(first),
    }
}

fn decide(samples: &[(NativeStatusObservation, bool)], nominee: i64) -> Decision {
    let (previous, _, _) = super::super::retained_member_tests::fixture();
    let current = previous.clone();
    let replies = samples
        .iter()
        .enumerate()
        .map(|(index, (status, up))| Reply {
            identity: &previous.members[index].identity,
            status,
            up: *up,
        })
        .collect::<Vec<_>>();
    select(&previous, &current, ReplicaId::new(nominee), &replies)
}

#[test]
fn history_vetoes_equal_progress_and_no_missing_native_compatibility_fallback() {
    let mut samples = vec![
        (observation(1, 50, 0, 10), true),
        (observation(2, 50, 4, 11), true),
    ];
    assert_eq!(decide(&samples, 1), Decision::Primary(ReplicaId::new(2)));
    samples[1].0.input.deactivation = None;
    assert_eq!(decide(&samples, 1), Decision::MissingHistory);
    samples[1].0.progress = ProgressQueryResult::NotReady;
    assert_eq!(decide(&samples, 1), Decision::Waiting);
}

#[test]
fn deactivation_support_requires_its_catchup_boundary_and_preserves_highest_down() {
    let mut samples = vec![
        (observation(1, 40, 4, 10), true),
        (observation(2, 50, 0, 11), false),
    ];
    assert_eq!(decide(&samples, 1), Decision::HighestDown);
    samples[1]
        .0
        .input
        .deactivation
        .as_mut()
        .unwrap()
        .catch_up_lsn = 51;
    assert_eq!(decide(&samples, 1), Decision::HighestDown);
    samples[1].1 = true;
    assert_eq!(decide(&samples, 1), Decision::Primary(ReplicaId::new(2)));
}

#[test]
fn first_lsn_optimization_requires_local_highest_lagger_and_positive_earlier_range() {
    let mut samples = vec![
        (observation(1, 50, 35, 10), true),
        (observation(2, 50, 5, 10), true),
        (observation(3, 10, 0, 10), true),
    ];
    assert_eq!(decide(&samples, 1), Decision::Primary(ReplicaId::new(2)));
    samples[2].0.progress = ProgressQueryResult::Value(50);
    assert_eq!(decide(&samples, 1), Decision::Primary(ReplicaId::new(1)));
    samples[2].0.progress = ProgressQueryResult::Value(10);
    samples[0].0.capability = ProgressQueryResult::Value(8);
    assert_eq!(decide(&samples, 1), Decision::Primary(ReplicaId::new(1)));
    samples[0].0.capability = ProgressQueryResult::Value(0);
    samples[1].0.capability = ProgressQueryResult::Value(0);
    assert_eq!(decide(&samples, 1), Decision::Primary(ReplicaId::new(1)));
}

#[test]
fn pc_and_cc_read_quorums_are_separate_from_write_quorum_and_members_are_not_aliased() {
    let (original, _, _) = super::super::retained_member_tests::fixture();
    let mut previous = original.clone();
    previous.members.truncate(2);
    previous.write_quorum = 2;
    let current = ConfigurationDescriptor::new(
        previous.epoch,
        ReplicaId::new(2),
        vec![
            ConfigurationMember {
                identity: original.members[1].identity.clone(),
                role: ReplicaRole::Primary,
            },
            original.members[2].clone(),
        ],
        2,
    );
    let sample = observation(2, 30, 0, 10);
    let mut replies = vec![Reply {
        identity: &previous.members[1].identity,
        status: &sample,
        up: true,
    }];
    assert_eq!(
        select(&previous, &current, ReplicaId::new(2), &replies),
        Decision::Primary(ReplicaId::new(2))
    );
    replies[0].identity = &previous.members[0].identity;
    assert_eq!(
        select(&previous, &current, ReplicaId::new(1), &replies),
        Decision::Waiting
    );
}

#[test]
fn round_binding_checks_every_selected_input_dimension_and_epoch_program() {
    let (_, _, peer) = super::super::retained_member_tests::fixture();
    let mut goal = peer.goals[0].goal.clone();
    goal.desired = ReplicaGoalKind::Discover;
    goal.scope.configuration.replicas.clear();
    let mut status = observation(2, 41, 0, 11);
    status.input.operation_id = goal.operation_id.clone();
    status.input.revision = goal.revision;
    status.input.scope = goal.scope.clone();
    status.input.applied_primary_fence = Some(goal.scope.epoch);
    assert!(inspection_matches(&goal, &status));
    for field in 0..7 {
        let mut crossed = status.clone();
        match field {
            0 => crossed.input.operation_id = OperationId::new("other-round"),
            1 => crossed.input.revision += 1,
            2 => crossed.input.scope.process_session_id = ProcessSessionId::new("other-process"),
            3 => crossed.input.scope.creation_id = ReplicatorCreationId::new("other-creation"),
            4 => crossed.input.scope.epoch.configuration_number += 1,
            5 => crossed.input.scope.history.data_loss_number += 1,
            _ => crossed.input.scope.configuration.current.write_quorum += 1,
        }

        assert!(!inspection_matches(&goal, &crossed));
    }
    status.epoch = NativeInspectionEpoch::Failed("actual epoch failed".into());
    assert!(!inspection_matches(&goal, &status));
    status.input.state = NativeReplicaState::StandBy;
    status.epoch = NativeInspectionEpoch::StandBy;
    assert!(!inspection_matches(&goal, &status));
    status.input.native_role = ReplicaRole::None;
    assert!(inspection_matches(&goal, &status));
}

#[test]
fn valid_opened_higher_prefix_can_win_without_secondary_readiness_or_a_replay_range() {
    let mut opened = observation(2, 51, 0, 10);
    opened.input.state = NativeReplicaState::StandBy;
    opened.input.native_role = ReplicaRole::None;
    opened.input.application_role = None;
    opened.input.applied_primary_fence = None;
    opened.epoch = NativeInspectionEpoch::StandBy;
    let mut samples = vec![(observation(1, 50, 1, 10), true), (opened, true)];
    assert_eq!(decide(&samples, 1), Decision::Primary(ReplicaId::new(2)));
    samples[1].1 = false;
    assert_eq!(decide(&samples, 1), Decision::HighestDown);
    samples[1].0.progress = ProgressQueryResult::Failed("actual native query error".into());
    assert_eq!(decide(&samples, 1), Decision::Waiting);
}

#[test]
fn retained_canonical_result_rejects_crossed_round_unaccepted_outcome_and_noncanonical_json() {
    let (_, _, peer) = super::super::retained_member_tests::fixture();
    let mut goal = peer.goals[0].goal.clone();
    goal.desired = ReplicaGoalKind::Discover;
    goal.scope.configuration.replicas.clear();
    let mut status = observation(2, 41, 0, 11);
    status.input.operation_id = goal.operation_id.clone();
    status.input.revision = goal.revision;
    status.input.scope = goal.scope.clone();
    let mut retained = RetainedReplicaGoal::new(goal.clone()).unwrap();
    retained.native_status = Some(serde_json::to_string(&status).unwrap());
    assert!(retained.validate().is_err());
    retained.outcome = Some(ReplicaGoalObservation {
        goal,
        status: ReplicaGoalStatus::Achieved,
        selected_call: None,
        receipts: 1,
        host_settled: true,
        completed_handoff: None,
    });
    retained.validate().unwrap();
    assert_eq!(retained.native_status().unwrap(), Some(status.clone()));
    let encoded = serde_json::to_vec(&retained).unwrap();
    assert_eq!(
        serde_json::from_slice::<RetainedReplicaGoal>(&encoded).unwrap(),
        retained
    );
    retained.native_status = Some(serde_json::to_string_pretty(&status).unwrap());
    assert!(retained.validate().is_err());
    status.input.scope.creation_id = ReplicatorCreationId::new("crossed");
    retained.native_status = Some(serde_json::to_string(&status).unwrap());
    assert!(retained.validate().is_err());
}
