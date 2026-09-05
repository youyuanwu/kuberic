use crate::{
    ActivityRecord, ActivityState, AttemptId, CheckpointEnvelope, CheckpointError,
    CheckpointPayload, CheckpointStore, CompareAndSwap, Evaluation, ExactBytes, ExecutionId,
    HostEpoch, LogicalActivityId, Nondeterminism, StorageRevision, Workflow, evaluate,
};

/// The persistence boundary that must be reloaded after an uncertain CAS result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistenceBoundary {
    Schedule,
    Exposure,
    Observation,
}

/// Why the caller must reload instead of acting on a proposal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReloadReason {
    Conflict,
    RejectedBeforeApply,
    ResponseLostAfterApply,
}

/// Rejection of an authoritative result observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObservationRejection {
    CheckpointMissing,
    ActivityNotExposed,
    LogicalActivityMismatch {
        expected: LogicalActivityId,
        observed: LogicalActivityId,
    },
}

/// An authoritative result for one exact logical activity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityObservation {
    activity: LogicalActivityId,
    result: ExactBytes,
}

impl ActivityObservation {
    pub fn new(activity: LogicalActivityId, result: ExactBytes) -> Self {
        Self { activity, result }
    }

    pub const fn activity(&self) -> &LogicalActivityId {
        &self.activity
    }

    pub const fn result(&self) -> &ExactBytes {
        &self.result
    }
}

/// Unforgeable evidence that the dispatch-exposed checkpoint was accepted.
#[derive(Debug, Eq, PartialEq)]
pub struct DispatchPermit {
    activity: LogicalActivityId,
    attempt_id: AttemptId,
}

impl DispatchPermit {
    fn new(activity: LogicalActivityId, attempt_id: AttemptId) -> Self {
        Self {
            activity,
            attempt_id,
        }
    }

    pub const fn activity(&self) -> &LogicalActivityId {
        &self.activity
    }

    pub const fn attempt_id(&self) -> AttemptId {
        self.attempt_id
    }
}

macro_rules! define_host_outcomes {
    ($($variant:ident $body:tt),+ $(,)?) => {
        /// The complete public outcome set for a durable host turn or observation.
        #[derive(Debug, Eq, PartialEq)]
        pub enum HostOutcome {
            $($variant $body),+
        }

        /// Variant names generated from the same declaration as [`HostOutcome`].
        pub const HOST_OUTCOME_VARIANTS: &[&str] = &[$(stringify!($variant)),+];
    };
}

define_host_outcomes! {
    ScheduleAccepted {
        activity: LogicalActivityId,
        revision: StorageRevision,
    },
    DispatchPermitted {
        permit: DispatchPermit,
        revision: StorageRevision,
    },
    ObservationAccepted {
        activity: LogicalActivityId,
        revision: StorageRevision,
    },
    WorkflowCompleted {
        result: ExactBytes,
    },
    Quarantined {
        activity: LogicalActivityId,
        attempt_id: AttemptId,
    },
    Nondeterminism(Nondeterminism),
    CheckpointRejected(CheckpointError),
    ObservationRejected(ObservationRejection),
    ReloadRequired {
        boundary: PersistenceBoundary,
        reason: ReloadReason,
    },
}

/// Public in-process host for one-turn replay, persistence, and observation.
pub struct DurableHost<S> {
    store: S,
    host_epoch: HostEpoch,
    next_attempt_counter: u64,
}

impl<S: CheckpointStore> DurableHost<S> {
    pub fn new(store: S, host_epoch: HostEpoch) -> Self {
        Self {
            store,
            host_epoch,
            next_attempt_counter: 1,
        }
    }

    pub const fn store(&self) -> &S {
        &self.store
    }

    /// Evaluate and, when needed, commit exactly one schedule or exposure turn.
    pub fn turn<W: Workflow>(
        &mut self,
        workflow: &W,
        execution_id: ExecutionId,
        workflow_input: ExactBytes,
    ) -> HostOutcome {
        let loaded = self.store.load(execution_id);
        let expected_revision = loaded.as_ref().map(|stored| stored.revision());
        let evaluation = evaluate(
            workflow,
            execution_id,
            workflow_input.clone(),
            loaded.as_ref().map(|stored| stored.checkpoint()),
        );

        match evaluation {
            Evaluation::Scheduled {
                activity,
                checkpoint,
            } => self.commit_schedule(execution_id, expected_revision, activity, checkpoint),
            Evaluation::Pending {
                activity,
                state: ActivityState::Scheduled,
            } => {
                let stored = loaded.expect("pending evaluation requires a loaded checkpoint");
                match self.prepare_exposure(
                    execution_id,
                    workflow_input,
                    stored.revision(),
                    stored.checkpoint(),
                    activity,
                ) {
                    Ok(proposal) => self.commit_exposure(proposal),
                    Err(error) => HostOutcome::CheckpointRejected(error),
                }
            }
            Evaluation::Pending {
                activity,
                state: ActivityState::DispatchExposed { attempt_id },
            } => HostOutcome::Quarantined {
                activity,
                attempt_id,
            },
            Evaluation::Pending {
                state: ActivityState::Completed { .. },
                ..
            } => unreachable!("completed activities are replayed rather than pending"),
            Evaluation::Complete { result, .. } => HostOutcome::WorkflowCompleted { result },
            Evaluation::Nondeterminism(error) => HostOutcome::Nondeterminism(error),
            Evaluation::CheckpointRejected(error) => HostOutcome::CheckpointRejected(error),
            Evaluation::WorkflowStalled => {
                HostOutcome::Nondeterminism(Nondeterminism::UnsupportedSuspension)
            }
        }
    }

    /// Persist an authoritative result only for the currently exposed activity.
    pub fn observe(
        &self,
        execution_id: ExecutionId,
        workflow_input: &ExactBytes,
        observation: ActivityObservation,
    ) -> HostOutcome {
        let Some(stored) = self.store.load(execution_id) else {
            return HostOutcome::ObservationRejected(ObservationRejection::CheckpointMissing);
        };
        let mut payload = match stored
            .checkpoint()
            .decode_and_validate(execution_id, workflow_input)
        {
            Ok(payload) => payload,
            Err(error) => return HostOutcome::CheckpointRejected(error),
        };
        let Some(record) = payload.activities().last().cloned() else {
            return HostOutcome::ObservationRejected(ObservationRejection::ActivityNotExposed);
        };
        let expected_activity = record.logical_id(execution_id);
        if expected_activity != *observation.activity() {
            return HostOutcome::ObservationRejected(
                ObservationRejection::LogicalActivityMismatch {
                    expected: expected_activity,
                    observed: observation.activity,
                },
            );
        }
        if !matches!(record.state(), ActivityState::DispatchExposed { .. }) {
            return HostOutcome::ObservationRejected(ObservationRejection::ActivityNotExposed);
        }

        let activity = expected_activity;
        replace_final_record(
            &mut payload,
            ActivityRecord::completed(
                record.sequence(),
                record.name().clone(),
                record.input().clone(),
                observation.result,
            ),
        );
        let checkpoint = match CheckpointEnvelope::encode(&payload) {
            Ok(checkpoint) => checkpoint,
            Err(error) => return HostOutcome::CheckpointRejected(error),
        };
        match self
            .store
            .compare_and_swap(execution_id, Some(stored.revision()), checkpoint)
        {
            CompareAndSwap::Accepted(revision) => {
                HostOutcome::ObservationAccepted { activity, revision }
            }
            other => reload_outcome(PersistenceBoundary::Observation, other),
        }
    }

    fn commit_schedule(
        &self,
        execution_id: ExecutionId,
        expected_revision: Option<StorageRevision>,
        activity: LogicalActivityId,
        checkpoint: CheckpointEnvelope,
    ) -> HostOutcome {
        match self
            .store
            .compare_and_swap(execution_id, expected_revision, checkpoint)
        {
            CompareAndSwap::Accepted(revision) => {
                HostOutcome::ScheduleAccepted { activity, revision }
            }
            other => reload_outcome(PersistenceBoundary::Schedule, other),
        }
    }

    fn prepare_exposure(
        &mut self,
        execution_id: ExecutionId,
        workflow_input: ExactBytes,
        expected_revision: StorageRevision,
        checkpoint: &CheckpointEnvelope,
        activity: LogicalActivityId,
    ) -> Result<PreparedExposure, CheckpointError> {
        let mut payload = checkpoint.decode_and_validate(execution_id, &workflow_input)?;
        let record = payload
            .activities()
            .last()
            .cloned()
            .expect("scheduled evaluation requires an activity record");
        let attempt_id = self.next_attempt();
        replace_final_record(
            &mut payload,
            ActivityRecord::dispatch_exposed(
                record.sequence(),
                record.name().clone(),
                record.input().clone(),
                attempt_id,
            ),
        );
        Ok(PreparedExposure {
            execution_id,
            expected_revision,
            checkpoint: CheckpointEnvelope::encode(&payload)?,
            activity,
            attempt_id,
        })
    }

    fn commit_exposure(&self, proposal: PreparedExposure) -> HostOutcome {
        match self.store.compare_and_swap(
            proposal.execution_id,
            Some(proposal.expected_revision),
            proposal.checkpoint,
        ) {
            CompareAndSwap::Accepted(revision) => HostOutcome::DispatchPermitted {
                permit: DispatchPermit::new(proposal.activity, proposal.attempt_id),
                revision,
            },
            other => reload_outcome(PersistenceBoundary::Exposure, other),
        }
    }

    fn next_attempt(&mut self) -> AttemptId {
        let counter = self.next_attempt_counter;
        self.next_attempt_counter = counter
            .checked_add(1)
            .expect("host attempt counter exhausted");
        AttemptId::new(self.host_epoch, counter)
            .expect("host attempt counters start above the reserved zero value")
    }
}

struct PreparedExposure {
    execution_id: ExecutionId,
    expected_revision: StorageRevision,
    checkpoint: CheckpointEnvelope,
    activity: LogicalActivityId,
    attempt_id: AttemptId,
}

fn replace_final_record(payload: &mut CheckpointPayload, replacement: ActivityRecord) {
    *payload
        .activities_mut()
        .last_mut()
        .expect("replacement requires a final activity") = replacement;
}

fn reload_outcome(boundary: PersistenceBoundary, result: CompareAndSwap) -> HostOutcome {
    let reason = match result {
        CompareAndSwap::Conflict => ReloadReason::Conflict,
        CompareAndSwap::RejectedBeforeApply => ReloadReason::RejectedBeforeApply,
        CompareAndSwap::ResponseLostAfterApply => ReloadReason::ResponseLostAfterApply,
        CompareAndSwap::Accepted(_) => unreachable!("accepted CAS handled by caller"),
    };
    HostOutcome::ReloadRequired { boundary, reason }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::{ActivityName, InMemoryCheckpointStore, InMemoryFault, WorkflowContext};

    struct OneActivity;

    #[async_trait(?Send)]
    impl Workflow for OneActivity {
        async fn run(&self, context: &mut WorkflowContext<'_>, input: ExactBytes) -> ExactBytes {
            context
                .activity(ActivityName::new("unit", 1).unwrap(), input)
                .await
        }
    }

    #[test]
    fn only_an_accepted_consumed_exposure_proposal_constructs_a_permit() {
        let store = InMemoryCheckpointStore::new();
        let execution_id = ExecutionId::from_bytes([2; 16]);
        let input = ExactBytes::new(b"unit");
        let mut host = DurableHost::new(store.clone(), HostEpoch::from_bytes([1; 16]));
        assert!(matches!(
            host.turn(&OneActivity, execution_id, input.clone()),
            HostOutcome::ScheduleAccepted { .. }
        ));

        for fault in [
            InMemoryFault::RejectBeforeApply,
            InMemoryFault::LoseResponseAfterApply,
        ] {
            store.fail_next_compare_and_swap(fault);
            let outcome = host.turn(&OneActivity, execution_id, input.clone());
            assert!(!matches!(outcome, HostOutcome::DispatchPermitted { .. }));
            if fault == InMemoryFault::LoseResponseAfterApply {
                assert!(matches!(
                    host.turn(&OneActivity, execution_id, input.clone()),
                    HostOutcome::Quarantined { .. }
                ));
                return;
            }
        }
        panic!("post-apply response-loss case did not execute");
    }
}
