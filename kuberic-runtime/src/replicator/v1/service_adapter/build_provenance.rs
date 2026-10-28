use crate::protocol::public_operations::*;
use crate::protocol::types::ReplicaRole;
use crate::replicator::ReplicatorFactoryContext;
use crate::{Result, RuntimeError};

use super::engine_session::{Binding, Endpoint};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CheckedBuild {
    pub(super) goal: ReplicaGoal,
}

impl CheckedBuild {
    pub(super) fn nonce(&self) -> Result<String> {
        let remote = self
            .goal
            .remote()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let bytes = serde_json::to_vec(&(
            &self.goal.operation_id,
            self.goal.intent().input_digest,
            remote.generation,
            &remote.source,
            &remote.target,
        ))
        .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))?;
        Ok(uuid::Uuid::from_bytes(super::engine_session::guid(
            b"kuberic-ordinary23-native-build-operation",
            &bytes,
        ))
        .to_string())
    }
}

fn exact_owner(snapshot: &ReplicaAgentSnapshot, publication: &ReplicaPublication) -> bool {
    !snapshot.withdrawn
        && snapshot.format == ORDINARY_ACTION_FORMAT
        && snapshot.history == publication.scope.history
        && snapshot.storage == publication.scope.storage
        && snapshot.process_session_id == publication.scope.process_session_id
        && snapshot.creation_id == publication.scope.creation_id
        && snapshot.publication.as_ref() == Some(publication)
}

pub(super) async fn check(
    context: &ReplicatorFactoryContext,
    binding: &Binding,
) -> Result<CheckedBuild> {
    let source = context
        .native_operation_snapshot(binding.history(), &binding.primary().identity)
        .await?;
    let target = context
        .native_operation_snapshot(binding.history(), &binding.secondary().identity)
        .await?;
    check_snapshots(binding, &source, &target)
}

pub(super) async fn select(
    context: &ReplicatorFactoryContext,
    binding: &mut Binding,
) -> Result<CheckedBuild> {
    let source = context
        .native_operation_snapshot(binding.history(), &binding.primary().identity)
        .await?;
    let target = context
        .native_operation_snapshot(binding.history(), &binding.secondary().identity)
        .await?;
    let mut selected = source.goals.iter().filter(|observation| {
        observation.goal.remote().is_some_and(|remote| {
            remote.source == binding.source && remote.target == binding.target
        }) && matches!(observation.goal.desired, ReplicaGoalKind::Build { .. })
            && ((observation.status == ReplicaGoalStatus::Pending
                && observation.selected_call.as_deref() == Some("Build"))
                || (observation.status == ReplicaGoalStatus::Achieved
                    && observation.selected_call.is_none()))
    });
    let goal = selected
        .next()
        .ok_or(RuntimeError::AuthorityNotAdmitted)?
        .goal
        .clone();
    if selected.next().is_some() {
        return Err(RuntimeError::AuthorityNotAdmitted);
    }
    binding.nonce = CheckedBuild { goal }.nonce()?;
    check_snapshots(binding, &source, &target)
}

pub(super) fn check_snapshots(
    binding: &Binding,
    source: &ReplicaAgentSnapshot,
    target: &ReplicaAgentSnapshot,
) -> Result<CheckedBuild> {
    #[cfg(test)]
    if std::env::var_os("KUBERIC_M3_NATIVE_SCOPE_DIAGNOSTICS").is_some() {
        eprintln!(
            "Build provenance exact={}/{} source={:?} target={:?} operations={:?}/{:?}",
            exact_owner(source, &binding.source),
            exact_owner(target, &binding.target),
            source.native_lifecycle.as_ref().map(|f| (
                f.state,
                f.native_role,
                f.applied_primary_fence
            )),
            target.native_lifecycle.as_ref().map(|f| (
                f.state,
                f.native_role,
                f.applied_primary_fence
            )),
            source
                .goals
                .iter()
                .map(|g| (&g.goal.operation_id, &g.status, &g.selected_call))
                .collect::<Vec<_>>(),
            target
                .goals
                .iter()
                .map(|g| (
                    &g.goal.operation_id,
                    &g.status,
                    &g.selected_call,
                    g.host_settled
                ))
                .collect::<Vec<_>>()
        );
    }
    let reject = || RuntimeError::AuthorityNotAdmitted;
    if !exact_owner(source, &binding.source) || !exact_owner(target, &binding.target) {
        return Err(reject());
    }
    let mut builds = source.goals.iter().filter(|observation| {
        observation.goal.scope == binding.source.scope
            && matches!(observation.goal.desired, ReplicaGoalKind::Build { .. })
            && observation.goal.remote().is_some_and(|remote| {
                remote.source == binding.source && remote.target == binding.target
            })
            && ((observation.status == ReplicaGoalStatus::Pending
                && observation.selected_call.as_deref() == Some("Build"))
                || (observation.status == ReplicaGoalStatus::Achieved
                    && observation.selected_call.is_none()))
    });
    let selected = builds.next().ok_or_else(reject)?;
    if builds.next().is_some()
        || source.goals.iter().any(|other| {
            other.goal.scope.process_session_id == source.process_session_id
                && other.goal.scope.creation_id == source.creation_id
                && other.goal.revision > selected.goal.revision
                && other.goal.remote().is_none_or(|remote| {
                    remote.target.scope.storage.replica == binding.target.scope.storage.replica
                })
        })
    {
        return Err(reject());
    }
    selected
        .goal
        .validate()
        .map_err(|error| RuntimeError::AuthorityMismatch(error.into()))?;
    let remote = selected.goal.remote().ok_or_else(reject)?;
    if remote.source != binding.source || remote.target != binding.target {
        return Err(reject());
    }
    let checked = CheckedBuild {
        goal: selected.goal.clone(),
    };
    if checked.nonce()? != binding.nonce {
        return Err(reject());
    }
    let prepare = target
        .goals
        .iter()
        .filter(|observation| {
            observation.goal.scope.process_session_id == target.process_session_id
                && observation.goal.scope.creation_id == target.creation_id
        })
        .max_by_key(|observation| observation.goal.revision)
        .ok_or_else(reject)?;
    if prepare.goal.scope != binding.target.scope
        || prepare.status != ReplicaGoalStatus::Achieved
        || remote.original_member.is_some() && !prepare.host_settled
        || prepare.selected_call.is_some()
        || !matches!(
            prepare.goal.desired,
            ReplicaGoalKind::Prepare {
                role: ReplicaRole::IdleSecondary,
                ..
            }
        )
        || remote
            .original_member
            .as_deref()
            .is_some_and(|goal| goal != &prepare.goal)
        || binding.original_member_idle() != remote.original_member.is_some()
    {
        return Err(reject());
    }
    let source_facts = source.native_lifecycle.as_ref().ok_or_else(reject)?;
    let target_facts = target.native_lifecycle.as_ref().ok_or_else(reject)?;
    if source_facts.scope != binding.source.scope
        || source_facts.state != NativeReplicaState::Ready
        || source_facts.native_role != ReplicaRole::Primary
        || source_facts.application_role != Some(ReplicaRole::Primary)
        || source_facts.applied_primary_fence != Some(binding.epoch())
        || target_facts.scope != binding.target.scope
        || target_facts.state != NativeReplicaState::InBuild
        || target_facts.native_role != ReplicaRole::IdleSecondary
        || target_facts.application_role != Some(ReplicaRole::IdleSecondary)
        || Endpoint::publication(&binding.source) == Endpoint::publication(&binding.target)
    {
        return Err(reject());
    }
    Ok(checked)
}

#[cfg(test)]
mod tests {
    use super::super::engine_session::{
        Sessions,
        replacement_tests::{description, fixture},
    };
    use super::*;
    use crate::protocol::types::*;

    fn synthetic_original_member() -> (Binding, ReplicaAgentSnapshot, ReplicaAgentSnapshot) {
        let mut binding = fixture();
        let target_identity = binding.secondary().identity;
        let member = binding
            .source
            .scope
            .configuration
            .current
            .members
            .iter_mut()
            .find(|member| member.identity.replica_id == target_identity.replica_id)
            .unwrap();
        member.identity = target_identity;
        binding.source.scope.configuration.current.configuration_id =
            binding.source.scope.configuration.current.expected_id();
        binding.source.scope.configuration.previous = None;
        let mut idle = description(&binding.target, ReplicaRole::ActiveSecondary);
        idle.is_up = false;
        idle.is_ready = false;
        idle.current_progress = crate::application::INVALID_LSN;
        idle.catch_up_capability = crate::application::INVALID_LSN;
        binding.source.scope.configuration.replicas = vec![idle];
        binding.target.scope.configuration.current =
            binding.source.scope.configuration.current.clone();
        binding.target.scope.configuration.previous = None;
        binding.target.scope.configuration.replicas =
            vec![description(&binding.source, ReplicaRole::Primary)];
        let mut inspect_scope = binding.target.scope.clone();
        inspect_scope.configuration.replicas.clear();
        let inspection = ReplicaGoal {
            format: ORDINARY_ACTION_FORMAT,
            identity: PublicV1Identity::new(1),
            operation_id: OperationId::new("synthetic-excluded-original-inspection"),
            revision: 1,
            scope: inspect_scope,
            desired: ReplicaGoalKind::Discover,
        };
        let prepare = ReplicaGoal {
            format: ORDINARY_ACTION_FORMAT,
            identity: PublicV1Identity::new(1),
            operation_id: OperationId::new("synthetic-original-idle"),
            revision: 2,
            scope: binding.target.scope.clone(),
            desired: ReplicaGoalKind::Prepare {
                role: ReplicaRole::IdleSecondary,
                rebuild: Some(Box::new(OriginalMemberRebuild {
                    inspection: Box::new(inspection),
                    source: binding.source.clone(),
                })),
            },
        };
        let goal = ReplicaGoal {
            format: ORDINARY_ACTION_FORMAT,
            identity: PublicV1Identity::new(1),
            operation_id: OperationId::new("synthetic-current-build"),
            revision: 3,
            scope: binding.source.scope.clone(),
            desired: ReplicaGoalKind::Build {
                remote: Box::new(RemoteReplicaSelection {
                    source: binding.source.clone(),
                    target: binding.target.clone(),
                    target_control_address: "http://synthetic-agent:1".into(),
                    generation: 7,
                    original_member: Some(Box::new(prepare.clone())),
                }),
            },
        };
        goal.validate().unwrap();
        binding.nonce = CheckedBuild { goal: goal.clone() }.nonce().unwrap();
        let snapshot = |publication: &ReplicaPublication, goal: ReplicaGoal, state, role| {
            ReplicaAgentSnapshot {
                format: ORDINARY_ACTION_FORMAT,
                identity: PublicV1Identity::new(1),
                history: publication.scope.history.clone(),
                storage: publication.scope.storage.clone(),
                open_mode: OpenMode::Existing,
                read_status: AccessStatus::NotPrimary,
                write_status: AccessStatus::NotPrimary,
                withdrawn: false,
                provider_writer_held: false,
                process_session_id: publication.scope.process_session_id.clone(),
                creation_id: publication.scope.creation_id.clone(),
                publication: Some(publication.clone()),
                goals: vec![ReplicaGoalObservation {
                    goal: goal.clone(),
                    status: ReplicaGoalStatus::Achieved,
                    selected_call: None,
                    receipts: 1,
                    host_settled: true,
                    completed_handoff: None,
                }],
                remote_operations: vec![],
                progress: ProgressQueryResult::NotQueried("synthetic".into()),
                capability: ProgressQueryResult::NotQueried("synthetic".into()),
                native_status: None,
                native_lifecycle: Some(NativeLifecycleFacts {
                    operation_id: goal.operation_id,
                    revision: goal.revision,
                    scope: publication.scope.clone(),
                    state,
                    native_role: role,
                    application_role: Some(role),
                    applied_primary_fence: Some(publication.scope.epoch),
                    deactivation: Some(NativeDeactivationInfo {
                        primary_fence: publication.scope.epoch,
                        catch_up_lsn: 0,
                    }),
                }),
                accepted_revision_high_watermark: 3,
                retained_recovery_exit: None,
                retained_member_qualification: None,
                reconstruction_dependencies: vec![],
            }
        };
        let mut source = snapshot(
            &binding.source,
            goal,
            NativeReplicaState::Ready,
            ReplicaRole::Primary,
        );
        source.goals[0].status = ReplicaGoalStatus::Pending;
        source.goals[0].selected_call = Some("Build".into());
        let target = snapshot(
            &binding.target,
            prepare,
            NativeReplicaState::InBuild,
            ReplicaRole::IdleSecondary,
        );
        (binding, source, target)
    }

    #[test]
    fn synthetic_original_member_decode_is_not_active_or_a_reservation_permission() {
        let (binding, source, target) = synthetic_original_member();
        let decoded: Binding =
            serde_json::from_slice(&serde_json::to_vec(&binding).unwrap()).unwrap();
        assert_eq!(decoded, binding);
        assert!(decoded.original_member_idle());
        assert!(!decoded.admitted_reconfiguration());
        let sessions = Sessions::default();
        assert!(sessions.reserve(&decoded).is_err());
        assert!(sessions.proposal(decoded.clone()).is_err());
        let checked = check_snapshots(&decoded, &source, &target).unwrap();
        sessions.associate_build(&decoded, &checked).unwrap();
        sessions.reserve(&decoded).unwrap();
        assert!(
            sessions.active(&decoded).is_err(),
            "observation is not native activation"
        );
        let remote = source.goals[0].goal.remote().unwrap();
        let mut outside = remote.clone();
        outside.original_member = None;
        assert!(
            outside.validate().is_err(),
            "normal outside-Build validator is unchanged"
        );
        let mut pending = decoded.clone();
        pending.target.stage = ReplicaPublicationStage::DiscoveryPending;
        pending.target.role = ReplicaRole::None;
        let pending: Binding =
            serde_json::from_slice(&serde_json::to_vec(&pending).unwrap()).unwrap();
        assert!(
            !pending.admitted_reconfiguration(),
            "logical membership alone is not Active classification"
        );
        assert!(pending.validate().is_err());
        assert!(Sessions::default().reserve(&pending).is_err());
        let mut reform = pending;
        reform.reformation = true;
        assert!(
            Sessions::default()
                .reformation_predecessor(&reform)
                .is_err(),
            "a warm purpose cannot manufacture its completed original root"
        );
    }

    #[test]
    fn synthetic_original_member_both_end_context_rejects_cross_nonce_digest_generation_purpose_and_owner()
     {
        let (binding, source, target) = synthetic_original_member();
        assert!(check_snapshots(&binding, &source, &target).is_ok());
        let mut wrong_nonce = binding.clone();
        wrong_nonce.nonce = uuid::Uuid::new_v4().to_string();
        assert!(check_snapshots(&wrong_nonce, &source, &target).is_err());
        for mutation in 0..7 {
            let mut source = source.clone();
            let mut target = target.clone();
            match mutation {
                0 => source.goals[0].goal.operation_id = OperationId::new("crossed-operation"),
                1 => {
                    let ReplicaGoalKind::Build { remote } = &mut source.goals[0].goal.desired
                    else {
                        unreachable!()
                    };
                    remote.generation += 1;
                }
                2 => {
                    target.goals[0].goal.desired = ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::ActiveSecondary,
                        catch_up: None,
                    }
                }
                3 => target.creation_id = ReplicatorCreationId::new("old-owner"),
                4 => source.native_lifecycle.as_mut().unwrap().state = NativeReplicaState::InBuild,
                5 => {
                    target.native_lifecycle.as_mut().unwrap().native_role =
                        ReplicaRole::ActiveSecondary
                }
                6 => source.goals[0].selected_call = None,
                _ => unreachable!(),
            }
            assert!(
                check_snapshots(&binding, &source, &target).is_err(),
                "synthetic negative {mutation}"
            );
        }
    }

    #[test]
    fn synthetic_build_context_selects_exact_remote_not_an_unrelated_higher_revision_build() {
        let (binding, mut source, target) = synthetic_original_member();
        let mut other = source.goals[0].clone();
        other.goal.operation_id = OperationId::new("other-actual-build-selection");
        other.goal.revision += 1;
        let ReplicaGoalKind::Build { remote } = &mut other.goal.desired else {
            unreachable!()
        };
        remote.original_member = None;
        remote.target.scope.storage.replica.replica_id = ReplicaId::new(3);
        remote.target.scope.configuration = remote.source.scope.configuration.clone();
        other.goal.validate().unwrap();
        source.goals.push(other);
        assert!(check_snapshots(&binding, &source, &target).is_ok());
        let mut ambiguous = source.goals[0].clone();
        ambiguous.goal.operation_id = OperationId::new("crossed-same-remote-operation");
        ambiguous.goal.revision += 2;
        source.goals.push(ambiguous);
        assert!(check_snapshots(&binding, &source, &target).is_err());
    }
}
