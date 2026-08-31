use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use kuberic_core::types::{
    Epoch, Lsn, ReplicaDeactivationInfo, ReplicaElectionConfiguration, ReplicaId,
    ReplicaInstanceId, Role, StableReplicaElectionMetadata, StableReplicaSnapshot,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElectionMember {
    pub id: ReplicaId,
    pub instance_id: ReplicaInstanceId,
    pub role: Role,
    pub dropped: bool,
    pub last_known: Option<StableReplicaElectionMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElectionConfiguration {
    pub members: Vec<ElectionMember>,
    pub write_quorum: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElectionObservation {
    pub instance_id: ReplicaInstanceId,
    pub epoch: Epoch,
    pub role: Role,
    pub healthy: bool,
    pub current_lsn: Lsn,
    pub committed_lsn: Lsn,
    pub first_retained_lsn: Option<Lsn>,
    pub deactivation: Option<ReplicaDeactivationInfo>,
    pub configuration: Option<ReplicaElectionConfiguration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationExclusion {
    Dropped,
    IncarnationMismatch,
    EpochMismatch,
    Unhealthy,
    IneligibleRole,
    UnknownProgress,
    InvalidProgressRange,
    ConfigurationMismatch,
}

impl ObservationExclusion {
    fn is_transient(self) -> bool {
        matches!(
            self,
            Self::IncarnationMismatch
                | Self::EpochMismatch
                | Self::Unhealthy
                | Self::UnknownProgress
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuorumAssessment {
    pub member_count: usize,
    pub required: usize,
    pub accepted: Vec<ReplicaId>,
    pub outstanding: Vec<ReplicaId>,
    pub excluded: Vec<(ReplicaId, ObservationExclusion)>,
}

impl QuorumAssessment {
    pub fn satisfied(&self) -> bool {
        self.accepted.len() >= self.required
    }

    pub fn can_still_be_satisfied(&self) -> bool {
        self.accepted.len() + self.outstanding.len() >= self.required
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedCandidate {
    pub id: ReplicaId,
    pub instance_id: ReplicaInstanceId,
    pub deactivation: ReplicaDeactivationInfo,
    pub current_lsn: Lsn,
    pub committed_lsn: Lsn,
    pub first_retained_lsn: Lsn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElectionDecision {
    Proceed {
        candidate: RankedCandidate,
        previous: Option<QuorumAssessment>,
        current: QuorumAssessment,
    },
    WaitForBestCandidate {
        possible_candidates: Vec<ReplicaId>,
        previous: Option<QuorumAssessment>,
        current: QuorumAssessment,
    },
    WaitForQuorum {
        previous: Option<QuorumAssessment>,
        current: QuorumAssessment,
    },
    DataLossRequired {
        candidate: RankedCandidate,
        previous: Option<QuorumAssessment>,
        current: QuorumAssessment,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElectionInput {
    pub expected_epoch: Epoch,
    pub failed_primary_id: ReplicaId,
    pub required_catch_up_lsn: Lsn,
    pub previous: Option<ElectionConfiguration>,
    pub current: ElectionConfiguration,
    pub observations: BTreeMap<ReplicaId, ElectionObservation>,
}

pub fn evaluate(input: &ElectionInput) -> Result<ElectionDecision, String> {
    validate_configuration(&input.current, "current")?;
    if let Some(previous) = &input.previous {
        validate_configuration(previous, "previous")?;
    }

    let accepted = accepted_observations(input);
    let current = assess_configuration(&input.current, input, &accepted);
    let previous = input
        .previous
        .as_ref()
        .map(|configuration| assess_configuration(configuration, input, &accepted));
    let candidate = match select_candidate(input, &accepted) {
        Ok(candidate) => candidate,
        Err(error) => {
            let members = all_members(input);
            let observed: BTreeSet<_> = input
                .observations
                .iter()
                .filter_map(|(id, observation)| {
                    let member = members.get(id)?;
                    let terminal = observation_exclusion(member, observation, input)
                        .is_some_and(|reason| !reason.is_transient());
                    (accepted.contains_key(id) || terminal).then_some(*id)
                })
                .collect();
            let mut possible_candidates = all_members(input)
                .into_values()
                .filter(|member| {
                    member.id != input.failed_primary_id
                        && !member.dropped
                        && !observed.contains(&member.id)
                })
                .map(|member| member.id)
                .collect::<Vec<_>>();
            possible_candidates.sort_unstable();
            if possible_candidates.is_empty() {
                return Err(error);
            }
            return Ok(ElectionDecision::WaitForBestCandidate {
                possible_candidates,
                previous,
                current,
            });
        }
    };

    let possible_candidates = possibly_better_outstanding(input, &accepted, &candidate);
    if !possible_candidates.is_empty() {
        return Ok(ElectionDecision::WaitForBestCandidate {
            possible_candidates,
            previous,
            current,
        });
    }

    let all_satisfied =
        current.satisfied() && previous.as_ref().is_none_or(QuorumAssessment::satisfied);
    if all_satisfied {
        return Ok(ElectionDecision::Proceed {
            candidate,
            previous,
            current,
        });
    }

    let can_restore = current.can_still_be_satisfied()
        && previous
            .as_ref()
            .is_none_or(QuorumAssessment::can_still_be_satisfied);
    if can_restore {
        return Ok(ElectionDecision::WaitForQuorum { previous, current });
    }

    Ok(ElectionDecision::DataLossRequired {
        candidate,
        previous,
        current,
    })
}

pub fn build_target(
    input: &ElectionInput,
    decision: &ElectionDecision,
    state_changed: bool,
) -> Result<(ReplicaId, Vec<StableReplicaSnapshot>, u32), String> {
    let candidate = match decision {
        ElectionDecision::Proceed { candidate, .. }
        | ElectionDecision::DataLossRequired { candidate, .. } => candidate,
        ElectionDecision::WaitForBestCandidate { .. } | ElectionDecision::WaitForQuorum { .. } => {
            return Err("cannot build a target for a waiting election".to_string());
        }
    };
    let accepted = accepted_observations(input);
    let freshest_deactivation = accepted
        .iter()
        .filter(|(id, _)| **id != input.failed_primary_id)
        .filter_map(|(_, observation)| observation.deactivation.map(|info| info.epoch))
        .max()
        .ok_or_else(|| "no accepted deactivation evidence".to_string())?;

    let mut members = Vec::new();
    for member in &input.current.members {
        let Some(observation) = accepted.get(&member.id) else {
            continue;
        };
        let Some(deactivation) = observation.deactivation else {
            continue;
        };
        let Some(first_retained_lsn) = observation.first_retained_lsn else {
            continue;
        };
        if deactivation.epoch != freshest_deactivation
            || (member.id != candidate.id && first_retained_lsn > input.required_catch_up_lsn)
            || member.id == input.failed_primary_id
        {
            continue;
        }
        if state_changed && member.id != candidate.id {
            continue;
        }
        members.push(StableReplicaSnapshot {
            id: member.id,
            instance_id: observation.instance_id.clone(),
            role: if member.id == candidate.id {
                Role::Primary
            } else {
                Role::ActiveSecondary
            },
            election_metadata: Some(StableReplicaElectionMetadata {
                current_lsn: observation.current_lsn,
                committed_lsn: observation.committed_lsn,
                first_retained_lsn,
                deactivation_epoch: deactivation.epoch,
                deactivation_catch_up_lsn: deactivation.catch_up_lsn,
            }),
        });
    }
    if !members.iter().any(|member| member.id == candidate.id) {
        return Err("confirmed candidate is absent from target".to_string());
    }
    members.sort_by_key(|member| member.id);
    let write_quorum = members.len() as u32 / 2 + 1;
    Ok((candidate.id, members, write_quorum))
}

pub(crate) fn accepted_observations(
    input: &ElectionInput,
) -> BTreeMap<ReplicaId, ElectionObservation> {
    let all_members = all_members(input);
    input
        .observations
        .iter()
        .filter_map(|(id, observation)| {
            let member = all_members.get(id)?;
            observation_exclusion(member, observation, input)
                .is_none()
                .then(|| (*id, observation.clone()))
        })
        .collect()
}

fn assess_configuration(
    configuration: &ElectionConfiguration,
    input: &ElectionInput,
    accepted: &BTreeMap<ReplicaId, ElectionObservation>,
) -> QuorumAssessment {
    let mut accepted_ids = Vec::new();
    let mut outstanding = Vec::new();
    let mut excluded = Vec::new();
    for member in &configuration.members {
        if member.dropped {
            excluded.push((member.id, ObservationExclusion::Dropped));
            continue;
        }
        match input.observations.get(&member.id) {
            None => outstanding.push(member.id),
            Some(_) if accepted.contains_key(&member.id) => accepted_ids.push(member.id),
            Some(observation) => {
                let reason = observation_exclusion(member, observation, input)
                    .unwrap_or(ObservationExclusion::ConfigurationMismatch);
                excluded.push((member.id, reason));
                if reason.is_transient() {
                    outstanding.push(member.id);
                }
            }
        }
    }
    accepted_ids.sort_unstable();
    outstanding.sort_unstable();
    excluded.sort_by_key(|(id, _)| *id);
    QuorumAssessment {
        member_count: configuration.members.len(),
        required: configuration.members.len().div_ceil(2),
        accepted: accepted_ids,
        outstanding,
        excluded,
    }
}

fn observation_exclusion(
    member: &ElectionMember,
    observation: &ElectionObservation,
    input: &ElectionInput,
) -> Option<ObservationExclusion> {
    if member.dropped {
        return Some(ObservationExclusion::Dropped);
    }
    if member.instance_id != observation.instance_id {
        return Some(ObservationExclusion::IncarnationMismatch);
    }
    if !matches!(observation.role, Role::Primary | Role::ActiveSecondary) {
        return Some(ObservationExclusion::IneligibleRole);
    }
    if observation.epoch != input.expected_epoch {
        return Some(ObservationExclusion::EpochMismatch);
    }
    if !observation.healthy {
        return Some(ObservationExclusion::Unhealthy);
    }
    let Some(first_retained_lsn) = observation.first_retained_lsn else {
        return Some(ObservationExclusion::UnknownProgress);
    };
    if first_retained_lsn > observation.committed_lsn
        || observation.committed_lsn > observation.current_lsn
    {
        return Some(ObservationExclusion::InvalidProgressRange);
    }
    if !configuration_matches(input, observation.configuration.as_ref()) {
        return Some(ObservationExclusion::ConfigurationMismatch);
    }
    None
}

fn configuration_matches(
    input: &ElectionInput,
    observed: Option<&ReplicaElectionConfiguration>,
) -> bool {
    let Some(observed) = observed else {
        return false;
    };
    status_matches_configuration(&observed.current, &input.current)
        && match (&observed.previous, &input.previous) {
            (None, None) => true,
            (Some(status), Some(configuration)) => {
                status_matches_configuration(status, configuration)
            }
            _ => false,
        }
}

fn status_matches_configuration(
    status: &kuberic_core::types::ReplicaConfigurationStatus,
    configuration: &ElectionConfiguration,
) -> bool {
    if status.write_quorum != configuration.write_quorum
        || status.members.len() != configuration.members.len()
    {
        return false;
    }
    let mut actual = status
        .members
        .iter()
        .map(|member| {
            (
                member.id,
                member.instance_id.as_str().to_string(),
                member.role as u8,
            )
        })
        .collect::<Vec<_>>();
    let mut expected = configuration
        .members
        .iter()
        .map(|member| {
            (
                member.id,
                member.instance_id.as_str().to_string(),
                member.role as u8,
            )
        })
        .collect::<Vec<_>>();
    actual.sort();
    expected.sort();
    actual == expected
}

fn select_candidate(
    input: &ElectionInput,
    accepted: &BTreeMap<ReplicaId, ElectionObservation>,
) -> Result<RankedCandidate, String> {
    let freshest_deactivation = accepted
        .iter()
        .filter(|(id, _)| **id != input.failed_primary_id)
        .filter_map(|(_, observation)| observation.deactivation.map(|info| info.epoch))
        .max()
        .ok_or_else(|| "no candidate has deactivation evidence".to_string())?;
    let candidates = accepted
        .iter()
        .filter(|(id, observation)| {
            **id != input.failed_primary_id
                && observation
                    .deactivation
                    .is_some_and(|info| info.epoch == freshest_deactivation)
        })
        .map(|(id, observation)| RankedCandidate {
            id: *id,
            instance_id: observation.instance_id.clone(),
            deactivation: observation.deactivation.unwrap(),
            current_lsn: observation.current_lsn,
            committed_lsn: observation.committed_lsn,
            first_retained_lsn: observation.first_retained_lsn.unwrap(),
        })
        .collect::<Vec<_>>();
    let capable = candidates
        .iter()
        .filter(|candidate| candidate.first_retained_lsn <= input.required_catch_up_lsn)
        .cloned()
        .collect::<Vec<_>>();
    let ranked = if capable.is_empty() {
        candidates
    } else {
        capable
    };
    ranked
        .into_iter()
        .max_by(compare_candidates)
        .ok_or_else(|| "no reachable catch-up-capable candidate".to_string())
}

fn compare_candidates(left: &RankedCandidate, right: &RankedCandidate) -> Ordering {
    left.deactivation
        .epoch
        .cmp(&right.deactivation.epoch)
        .then(left.current_lsn.cmp(&right.current_lsn))
        .then(right.first_retained_lsn.cmp(&left.first_retained_lsn))
        .then(right.id.cmp(&left.id))
}

fn possibly_better_outstanding(
    input: &ElectionInput,
    accepted: &BTreeMap<ReplicaId, ElectionObservation>,
    candidate: &RankedCandidate,
) -> Vec<ReplicaId> {
    let members = all_members(input);
    let observed: BTreeSet<_> = input
        .observations
        .iter()
        .filter_map(|(id, observation)| {
            let member = members.get(id)?;
            let terminal = observation_exclusion(member, observation, input)
                .is_some_and(|reason| !reason.is_transient());
            (accepted.contains_key(id) || terminal).then_some(*id)
        })
        .collect();
    let mut possible = all_members(input)
        .into_values()
        .filter(|member| {
            !member.dropped
                && member.id != input.failed_primary_id
                && !observed.contains(&member.id)
                && member.last_known.as_ref().is_none_or(|metadata| {
                    compare_last_known(member.id, metadata, candidate) == Ordering::Greater
                })
        })
        .map(|member| member.id)
        .collect::<Vec<_>>();
    possible.sort_unstable();

    possible
}

fn compare_last_known(
    member_id: ReplicaId,
    metadata: &StableReplicaElectionMetadata,
    candidate: &RankedCandidate,
) -> Ordering {
    metadata
        .deactivation_epoch
        .cmp(&candidate.deactivation.epoch)
        .then(metadata.current_lsn.cmp(&candidate.current_lsn))
        .then(
            candidate
                .first_retained_lsn
                .cmp(&metadata.first_retained_lsn),
        )
        .then(candidate.id.cmp(&member_id))
}

fn all_members(input: &ElectionInput) -> BTreeMap<ReplicaId, ElectionMember> {
    let mut members = BTreeMap::new();
    if let Some(previous) = &input.previous {
        for member in &previous.members {
            members.insert(member.id, member.clone());
        }
    }
    for member in &input.current.members {
        members.insert(member.id, member.clone());
    }
    members
}

fn validate_configuration(configuration: &ElectionConfiguration, name: &str) -> Result<(), String> {
    if configuration.members.is_empty() {
        return Err(format!("{name} election configuration is empty"));
    }
    let mut ids = BTreeSet::new();
    for member in &configuration.members {
        if member.instance_id.as_str().is_empty() {
            return Err(format!(
                "{name} election member {} has empty incarnation",
                member.id
            ));
        }
        if !ids.insert(member.id) {
            return Err(format!(
                "{name} election configuration repeats replica {}",
                member.id
            ));
        }
    }
    if configuration.write_quorum == 0
        || configuration.write_quorum as usize > configuration.members.len()
    {
        return Err(format!(
            "{name} election configuration has invalid write quorum {}",
            configuration.write_quorum
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuberic_core::types::{
        ReplicaConfigurationMemberStatus, ReplicaConfigurationMode, ReplicaConfigurationStatus,
    };

    fn member(id: i64) -> ElectionMember {
        ElectionMember {
            id,
            instance_id: ReplicaInstanceId::new(format!("instance-{id}")),
            role: if id == 1 {
                Role::Primary
            } else {
                Role::ActiveSecondary
            },
            dropped: false,
            last_known: Some(StableReplicaElectionMetadata {
                current_lsn: id * 10,
                committed_lsn: id * 10,
                first_retained_lsn: 0,
                deactivation_epoch: Epoch::new(1, 4),
                deactivation_catch_up_lsn: id * 10,
            }),
        }
    }

    fn config(ids: &[i64]) -> ElectionConfiguration {
        ElectionConfiguration {
            members: ids.iter().copied().map(member).collect(),
            write_quorum: ids.len() as u32 / 2 + 1,
        }
    }

    fn reported(configuration: &ElectionConfiguration) -> ReplicaConfigurationStatus {
        ReplicaConfigurationStatus {
            mode: ReplicaConfigurationMode::Current,
            members: configuration
                .members
                .iter()
                .map(|member| ReplicaConfigurationMemberStatus {
                    id: member.id,
                    instance_id: member.instance_id.clone(),
                    role: member.role,
                })
                .collect(),
            write_quorum: configuration.write_quorum,
        }
    }

    fn observation(
        id: i64,
        current: &ElectionConfiguration,
        previous: Option<&ElectionConfiguration>,
        lsn: i64,
    ) -> ElectionObservation {
        ElectionObservation {
            instance_id: ReplicaInstanceId::new(format!("instance-{id}")),
            epoch: Epoch::new(1, 4),
            role: Role::ActiveSecondary,
            healthy: true,
            current_lsn: lsn,
            committed_lsn: lsn,
            first_retained_lsn: Some(0),
            deactivation: Some(ReplicaDeactivationInfo {
                epoch: Epoch::new(1, 4),
                catch_up_lsn: lsn,
            }),
            configuration: Some(ReplicaElectionConfiguration {
                previous: previous.map(reported),
                current: reported(current),
            }),
        }
    }

    fn input(ids: &[i64]) -> ElectionInput {
        let mut current = config(ids);
        if let Some(failed) = current.members.iter_mut().find(|member| member.id == 1) {
            failed.dropped = true;
        }
        ElectionInput {
            expected_epoch: Epoch::new(1, 4),
            failed_primary_id: 1,
            required_catch_up_lsn: 0,
            previous: None,
            current: current.clone(),
            observations: ids
                .iter()
                .copied()
                .filter(|id| *id != 1)
                .map(|id| (id, observation(id, &current, None, id * 10)))
                .collect(),
        }
    }

    #[test]
    fn current_quorum_proceeds_with_best_primary() {
        let input = input(&[1, 2, 3]);
        let ElectionDecision::Proceed {
            candidate, current, ..
        } = evaluate(&input).unwrap()
        else {
            panic!("expected proceed");
        };
        assert_eq!(candidate.id, 3);
        assert_eq!(current.member_count, 3);
        assert_eq!(current.required, 2);
        assert_eq!(current.accepted, vec![2, 3]);
    }

    #[test]
    fn previous_and_current_quorums_count_overlap_independently() {
        let mut previous = config(&[1, 2, 3]);
        previous.members[0].dropped = true;
        let current = config(&[2, 3, 4]);
        let mut input = ElectionInput {
            expected_epoch: Epoch::new(1, 4),
            failed_primary_id: 1,
            required_catch_up_lsn: 0,
            previous: Some(previous.clone()),
            current: current.clone(),
            observations: BTreeMap::new(),
        };
        for id in [2, 3] {
            input
                .observations
                .insert(id, observation(id, &current, Some(&previous), id * 10));
        }
        input
            .observations
            .insert(4, observation(4, &current, Some(&previous), 40));
        let ElectionDecision::Proceed {
            previous, current, ..
        } = evaluate(&input).unwrap()
        else {
            panic!("overlap should satisfy both quorums");
        };
        assert_eq!(previous.unwrap().accepted, vec![2, 3]);
        assert_eq!(current.accepted, vec![2, 3, 4]);
    }

    #[test]
    fn stale_deactivation_counts_for_quorum_but_not_candidate() {
        let mut input = input(&[1, 2, 3]);
        input.observations.get_mut(&2).unwrap().deactivation = Some(ReplicaDeactivationInfo {
            epoch: Epoch::new(1, 3),
            catch_up_lsn: 20,
        });
        input.observations.get_mut(&2).unwrap().current_lsn = 100;
        let ElectionDecision::Proceed {
            candidate, current, ..
        } = evaluate(&input).unwrap()
        else {
            panic!("stale deactivation should not destroy quorum");
        };
        assert_eq!(current.accepted, vec![2, 3]);
        assert_eq!(candidate.id, 3);
    }

    #[test]
    fn terminal_role_and_invalid_progress_stay_in_denominator() {
        let mut input = input(&[1, 2, 3, 4, 5]);
        input.observations.get_mut(&2).unwrap().role = Role::None;
        input.observations.get_mut(&3).unwrap().first_retained_lsn = Some(31);
        let ElectionDecision::DataLossRequired { current, .. } = evaluate(&input).unwrap() else {
            panic!("conclusively excluded responses should require data loss");
        };
        assert_eq!(current.member_count, 5);
        assert_eq!(current.required, 3);
        assert_eq!(current.accepted, vec![4, 5]);
        assert_eq!(current.excluded.len(), 3);
    }

    #[test]
    fn transient_unhealthy_replica_can_restore_quorum() {
        let mut input = input(&[1, 2, 3]);
        input.observations.get_mut(&3).unwrap().healthy = false;
        input.current.members[2].last_known = Some(StableReplicaElectionMetadata {
            current_lsn: 1,
            committed_lsn: 1,
            first_retained_lsn: 0,
            deactivation_epoch: Epoch::new(1, 3),
            deactivation_catch_up_lsn: 1,
        });
        let ElectionDecision::WaitForQuorum { current, .. } = evaluate(&input).unwrap() else {
            panic!("transient unhealthy replica must remain quorum-restoring");
        };
        assert_eq!(current.accepted, vec![2]);
        assert_eq!(current.outstanding, vec![3]);
        assert!(current.can_still_be_satisfied());
    }

    #[test]
    fn rejected_possible_best_replica_blocks_normal_election() {
        let mut input = input(&[1, 2, 3, 4, 5]);
        input.observations.get_mut(&5).unwrap().healthy = false;
        let ElectionDecision::WaitForBestCandidate {
            possible_candidates,
            current,
            ..
        } = evaluate(&input).unwrap()
        else {
            panic!("rejected possible-best replica must block promotion");
        };
        assert!(current.satisfied());
        assert_eq!(possible_candidates, vec![5]);
    }

    #[test]
    fn outstanding_member_waits_for_quorum() {
        let mut input = input(&[1, 2, 3, 4, 5]);
        input.observations.remove(&3);
        input.current.members[2].last_known = Some(StableReplicaElectionMetadata {
            current_lsn: 1,
            committed_lsn: 1,
            first_retained_lsn: 0,
            deactivation_epoch: Epoch::new(1, 4),
            deactivation_catch_up_lsn: 1,
        });
        input.observations.remove(&4);
        let ElectionDecision::WaitForQuorum { current, .. } = evaluate(&input).unwrap() else {
            panic!("outstanding member can still restore quorum");
        };
        assert_eq!(current.outstanding, vec![3, 4]);
    }

    #[test]
    fn unknown_or_better_outstanding_candidate_waits_before_data_loss() {
        let mut input = input(&[1, 2, 3, 4, 5]);
        input.observations.get_mut(&2).unwrap().healthy = false;
        input.observations.get_mut(&3).unwrap().healthy = false;
        input.observations.remove(&5);
        input.current.members[4].last_known = None;
        let ElectionDecision::WaitForBestCandidate {
            possible_candidates,
            ..
        } = evaluate(&input).unwrap()
        else {
            panic!("unknown candidate must block data loss");
        };
        assert_eq!(possible_candidates, vec![5]);
    }

    #[test]
    fn equal_lsn_with_better_catch_up_range_waits() {
        let mut input = input(&[1, 2, 3]);
        input.observations.remove(&2);
        input.current.members[1].last_known = Some(StableReplicaElectionMetadata {
            current_lsn: 30,
            committed_lsn: 30,
            first_retained_lsn: -1,
            deactivation_epoch: Epoch::new(1, 4),
            deactivation_catch_up_lsn: 30,
        });
        let ElectionDecision::WaitForBestCandidate {
            possible_candidates,
            ..
        } = evaluate(&input).unwrap()
        else {
            panic!("better catch-up coverage must block candidate");
        };
        assert_eq!(possible_candidates, vec![2]);
    }

    #[test]
    fn catch_up_incapable_high_lsn_does_not_outrank_capable_candidate() {
        let mut input = input(&[1, 2, 3]);
        input.required_catch_up_lsn = 5;
        input.observations.get_mut(&3).unwrap().first_retained_lsn = Some(6);
        input.observations.get_mut(&3).unwrap().current_lsn = 100;
        input.observations.get_mut(&3).unwrap().committed_lsn = 100;
        let ElectionDecision::Proceed { candidate, .. } = evaluate(&input).unwrap() else {
            panic!("capable candidate should proceed");
        };
        assert_eq!(candidate.id, 2);
    }

    #[test]
    fn target_is_deterministic_and_state_change_is_primary_only() {
        let input = input(&[1, 2, 3]);
        let decision = evaluate(&input).unwrap();
        let (primary, members, quorum) = build_target(&input, &decision, false).unwrap();
        assert_eq!(primary, 3);
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].role, Role::ActiveSecondary);
        assert_eq!(members[1].role, Role::Primary);
        assert_eq!(quorum, 2);
        let (_, members, quorum) = build_target(&input, &decision, true).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].id, 3);
        assert_eq!(quorum, 1);
    }

    #[test]
    fn deterministic_tie_uses_stable_replica_identity() {
        let mut input = input(&[1, 2, 3]);
        for id in [2, 3] {
            let observation = input.observations.get_mut(&id).unwrap();
            observation.current_lsn = 20;
            observation.committed_lsn = 20;
            observation.first_retained_lsn = Some(0);
        }
        let ElectionDecision::Proceed { candidate, .. } = evaluate(&input).unwrap() else {
            panic!("tie should be deterministic");
        };
        assert_eq!(candidate.id, 2);
    }

    #[test]
    fn dropped_and_malformed_observations_never_reduce_denominator() {
        let mut input = input(&[1, 2, 3, 4, 5]);
        input.current.members[1].dropped = true;
        input.observations.get_mut(&3).unwrap().instance_id = ReplicaInstanceId::new("replacement");
        input.observations.get_mut(&4).unwrap().epoch = Epoch::new(1, 5);
        input.observations.get_mut(&5).unwrap().configuration = None;

        assert!(matches!(
            evaluate(&input).unwrap(),
            ElectionDecision::WaitForBestCandidate { .. }
        ));

        let accepted = accepted_observations(&input);
        let assessment = assess_configuration(&input.current, &input, &accepted);
        assert_eq!(assessment.member_count, 5);
        assert_eq!(assessment.required, 3);
        assert!(assessment.accepted.is_empty());
        assert_eq!(assessment.excluded.len(), 5);
    }

    #[test]
    fn one_lost_joint_quorum_requires_data_loss() {
        let mut previous = config(&[1, 2, 3]);
        previous.members[0].dropped = true;
        let current = config(&[2, 3, 4]);
        let mut input = ElectionInput {
            expected_epoch: Epoch::new(1, 4),
            failed_primary_id: 1,
            required_catch_up_lsn: 0,
            previous: Some(previous.clone()),
            current: current.clone(),
            observations: BTreeMap::new(),
        };
        input
            .observations
            .insert(2, observation(2, &current, Some(&previous), 20));
        let mut excluded = observation(3, &current, Some(&previous), 30);
        excluded.role = Role::None;
        input.observations.insert(3, excluded);
        input
            .observations
            .insert(4, observation(4, &current, Some(&previous), 40));

        let ElectionDecision::DataLossRequired {
            candidate,
            previous,
            current,
        } = evaluate(&input).unwrap()
        else {
            panic!("lost previous quorum requires data loss");
        };
        assert_eq!(candidate.id, 4);
        assert!(!previous.unwrap().satisfied());
        assert!(current.satisfied());
    }

    #[test]
    fn no_live_candidate_waits_for_outstanding_candidate() {
        let current = config(&[1, 2, 3]);
        let input = ElectionInput {
            expected_epoch: Epoch::new(1, 4),
            failed_primary_id: 1,
            required_catch_up_lsn: 0,
            previous: None,
            current,
            observations: BTreeMap::new(),
        };
        let ElectionDecision::WaitForBestCandidate {
            possible_candidates,
            ..
        } = evaluate(&input).unwrap()
        else {
            panic!("outstanding replicas should wait");
        };
        assert_eq!(possible_candidates, vec![2, 3]);
    }

    #[test]
    fn waiting_decision_cannot_build_target() {
        let mut input = input(&[1, 2, 3]);
        input.observations.remove(&2);
        input.current.members[1].last_known = None;
        let decision = evaluate(&input).unwrap();
        assert!(build_target(&input, &decision, false).is_err());
    }

    #[test]
    fn satisfied_quorum_still_waits_for_unknown_unavailable_candidate() {
        let mut input = input(&[1, 2, 3, 4, 5]);
        input.observations.remove(&5);
        input.current.members[4].last_known = None;
        let ElectionDecision::WaitForBestCandidate {
            possible_candidates,
            current,
            ..
        } = evaluate(&input).unwrap()
        else {
            panic!("unknown unavailable candidate must block proceed");
        };
        assert!(current.satisfied());
        assert_eq!(possible_candidates, vec![5]);
    }

    #[test]
    fn equal_lsn_unavailable_candidate_uses_full_comparator() {
        let mut fresher = input(&[1, 2, 3, 4, 5]);
        fresher.observations.remove(&5);
        fresher.current.members[4].last_known = Some(StableReplicaElectionMetadata {
            current_lsn: 40,
            committed_lsn: 40,
            first_retained_lsn: 0,
            deactivation_epoch: Epoch::new(1, 5),
            deactivation_catch_up_lsn: 40,
        });
        assert!(matches!(
            evaluate(&fresher).unwrap(),
            ElectionDecision::WaitForBestCandidate { .. }
        ));

        let mut identity = input(&[1, 2, 3, 4, 5]);
        for id in [3, 4, 5] {
            let observation = identity.observations.get_mut(&id).unwrap();
            observation.current_lsn = 30;
            observation.committed_lsn = 30;
        }
        identity.observations.remove(&2);
        identity.current.members[1].last_known = Some(StableReplicaElectionMetadata {
            current_lsn: 30,
            committed_lsn: 30,
            first_retained_lsn: 0,
            deactivation_epoch: Epoch::new(1, 4),
            deactivation_catch_up_lsn: 30,
        });
        assert!(matches!(
            evaluate(&identity).unwrap(),
            ElectionDecision::WaitForBestCandidate { .. }
        ));
    }

    #[test]
    fn conclusively_lesser_unavailable_candidate_does_not_block_proceed() {
        let mut input = input(&[1, 2, 3, 4, 5]);
        input.observations.remove(&5);
        input.current.members[4].last_known = Some(StableReplicaElectionMetadata {
            current_lsn: 1,
            committed_lsn: 1,
            first_retained_lsn: 0,
            deactivation_epoch: Epoch::new(1, 3),
            deactivation_catch_up_lsn: 1,
        });
        assert!(matches!(
            evaluate(&input).unwrap(),
            ElectionDecision::Proceed { .. }
        ));
    }

    #[test]
    fn unknown_progress_observation_waits_without_reducing_denominator() {
        let mut input = input(&[1, 2, 3]);
        input.observations.get_mut(&2).unwrap().first_retained_lsn = None;
        let ElectionDecision::WaitForQuorum { current, .. } = evaluate(&input).unwrap() else {
            panic!("unknown progress must remain potentially quorum-restoring");
        };
        assert_eq!(current.member_count, 3);
        assert_eq!(current.required, 2);
        assert_eq!(current.accepted, vec![3]);
        assert!(
            current
                .excluded
                .contains(&(2, ObservationExclusion::UnknownProgress))
        );
    }

    #[test]
    fn unobserved_dropped_member_cannot_restore_quorum() {
        let mut input = input(&[1, 2, 3, 4, 5]);
        input.current.members[1].dropped = true;
        input.observations.remove(&2);
        input.observations.get_mut(&5).unwrap().role = Role::None;
        let ElectionDecision::DataLossRequired { current, .. } = evaluate(&input).unwrap() else {
            panic!("dropped member must not remain outstanding");
        };
        assert!(!current.can_still_be_satisfied());
        assert!(current.outstanding.is_empty());
        assert!(
            current
                .excluded
                .contains(&(2, ObservationExclusion::Dropped))
        );
    }

    #[test]
    fn all_incapable_survivors_still_select_data_loss_candidate() {
        let mut input = input(&[1, 2, 3, 4, 5]);
        input.required_catch_up_lsn = 5;
        for id in [2, 3] {
            input.observations.get_mut(&id).unwrap().first_retained_lsn = Some(6);
        }
        for id in [4, 5] {
            input.observations.get_mut(&id).unwrap().role = Role::None;
        }
        let decision = evaluate(&input).unwrap();
        let ElectionDecision::DataLossRequired { candidate, .. } = &decision else {
            panic!("all-incapable survivor set requires data loss");
        };
        assert_eq!(candidate.id, 3);
        let (_, members, quorum) = build_target(&input, &decision, false).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].id, 3);
        assert_eq!(members[0].role, Role::Primary);
        assert_eq!(quorum, 1);
    }
}
