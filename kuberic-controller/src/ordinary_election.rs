use super::*;

pub(super) fn inspection_matches(goal: &ReplicaGoal, status: &NativeStatusObservation) -> bool {
    matches!(goal.desired, ReplicaGoalKind::Discover)
        && status.input.operation_id == goal.operation_id
        && status.input.revision == goal.revision
        && status.input.scope == goal.scope
        && match status.input.state {
            NativeReplicaState::StandBy => {
                status.epoch == NativeInspectionEpoch::StandBy
                    && status.input.native_role == ReplicaRole::None
                    && status.capability == ProgressQueryResult::Value(0)
            }
            NativeReplicaState::InBuild | NativeReplicaState::Ready => {
                status.input.native_role != ReplicaRole::None
                    && (status.epoch == NativeInspectionEpoch::Applied
                        || status.epoch == NativeInspectionEpoch::AlreadyApplied
                            && status.input.applied_primary_fence == Some(goal.scope.epoch))
            }
        }
}

pub(super) struct Reply<'a> {
    pub identity: &'a ReplicaIdentity,
    pub status: &'a NativeStatusObservation,
    pub up: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Waiting,
    MissingHistory,
    HighestDown,
    Primary(ReplicaId),
}

/// Read-quorum inspection is not write-quorum admission or a copy/ACK receipt.
pub(super) fn select(
    previous: &ConfigurationDescriptor,
    current: &ConfigurationDescriptor,
    local_nominee: ReplicaId,
    replies: &[Reply<'_>],
) -> Decision {
    let completed = replies
        .iter()
        .filter(|reply| {
            matches!(reply.status.progress, ProgressQueryResult::Value(value) if value >= 0)
                && matches!(reply.status.capability, ProgressQueryResult::Value(value) if value >= 0)
                && !matches!(reply.status.epoch, NativeInspectionEpoch::Failed(_))
        })
        .collect::<Vec<_>>();
    for configuration in [previous, current] {
        let count = configuration
            .members
            .iter()
            .filter(|member| {
                completed
                    .iter()
                    .any(|reply| reply.identity == &member.identity)
            })
            .count();
        if count < configuration.members.len().div_ceil(2) {
            return Decision::Waiting;
        }
    }
    if completed
        .iter()
        .any(|reply| reply.status.input.deactivation.is_none())
    {
        // Absence is not native old-version support, nor declared data loss.
        return Decision::MissingHistory;
    }
    let Some(epoch) = completed
        .iter()
        .filter_map(|reply| {
            let history = reply.status.input.deactivation.as_ref()?;
            (reply.status.progress.value()? >= history.catch_up_lsn)
                .then_some(history.primary_fence)
        })
        .max()
    else {
        return Decision::Waiting;
    };
    let eligible = completed
        .into_iter()
        .filter(|reply| {
            reply
                .status
                .input
                .deactivation
                .as_ref()
                .unwrap()
                .primary_fence
                >= epoch
        })
        .collect::<Vec<_>>();
    let highest = eligible
        .iter()
        .filter_map(|reply| reply.status.progress.value())
        .max()
        .unwrap();
    let mut best = eligible
        .iter()
        .copied()
        .filter(|reply| reply.up && reply.status.progress.value() == Some(highest))
        .collect::<Vec<_>>();
    if best.is_empty() {
        return Decision::HighestDown;
    }
    let mut selected = best
        .iter()
        .copied()
        .find(|reply| reply.identity.replica_id == local_nominee)
        .unwrap_or(best[0]);
    if selected.identity.replica_id == local_nominee {
        best.sort_by_key(|reply| match reply.status.capability.value().unwrap() {
            0 => i64::MAX,
            positive => positive,
        });
        if let Some(lagger) = eligible
            .iter()
            .filter_map(|reply| reply.status.progress.value())
            .min()
            .filter(|last| *last < highest)
        {
            let first = selected.status.capability.value().unwrap();
            if first == 0 || first > lagger.saturating_add(1) {
                let earlier = best[0];
                let earlier_first = earlier.status.capability.value().unwrap();
                if earlier_first > 0 && (first == 0 || earlier_first < first) {
                    selected = earlier;
                }
            }
        }
    }
    Decision::Primary(selected.identity.replica_id)
}

#[cfg(test)]
#[path = "ordinary_election_tests.rs"]
mod tests;
