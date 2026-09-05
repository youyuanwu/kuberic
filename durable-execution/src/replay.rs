use std::task::{Context, Poll};

use futures::task::noop_waker_ref;
use thiserror::Error;

use crate::{
    ActivityRecord, ActivitySequence, ActivitySpec, ActivityState, CheckpointEnvelope,
    CheckpointError, CheckpointLimits, CheckpointPayload, ExactBytes, ExecutionId,
    LogicalActivityId, Workflow, WorkflowContext, workflow::ContextDecision,
};

/// Result of deterministically polling one workflow turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Evaluation {
    Scheduled {
        activity: LogicalActivityId,
        checkpoint: CheckpointEnvelope,
    },
    Pending {
        activity: LogicalActivityId,
        state: ActivityState,
    },
    Complete {
        result: ExactBytes,
        checkpoint: CheckpointEnvelope,
    },
    Nondeterminism(Nondeterminism),
    CheckpointRejected(CheckpointError),
    WorkflowStalled,
}

/// A valid checkpoint that cannot be replayed by the supplied workflow.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum Nondeterminism {
    #[error("activity {sequence} differs from its recorded immutable specification")]
    ActivityMismatch {
        sequence: ActivitySequence,
        recorded: ActivitySpec,
        requested: ActivitySpec,
    },
    #[error(
        "workflow completed after consuming {consumed} activities with {remaining} history records unused"
    )]
    UnusedHistory { consumed: u64, remaining: u64 },
    #[error("workflow suspended without requesting its next durable activity")]
    UnsupportedSuspension,
}

/// Validate a checkpoint, then poll the workflow exactly once with a no-op waker.
pub fn evaluate<W: Workflow>(
    workflow: &W,
    execution_id: ExecutionId,
    workflow_input: ExactBytes,
    checkpoint: Option<&CheckpointEnvelope>,
    limits: CheckpointLimits,
) -> Evaluation {
    let mut payload = match checkpoint {
        Some(envelope) => match envelope.decode_and_validate(execution_id, &workflow_input, limits)
        {
            Ok(payload) => payload,
            Err(error) => return Evaluation::CheckpointRejected(error),
        },
        None => CheckpointPayload::new(execution_id, workflow_input.clone(), Vec::new()),
    };

    let (poll, cursor, decision) = {
        let mut context = WorkflowContext::new(execution_id, payload.activities());
        let poll = {
            let mut future = workflow.run(&mut context, workflow_input);
            let mut task_context = Context::from_waker(noop_waker_ref());
            future.as_mut().poll(&mut task_context)
        };
        (poll, context.cursor(), context.decision)
    };

    match decision {
        Some(ContextDecision::Schedule {
            sequence,
            spec,
            logical_id,
        }) => {
            payload
                .activities_mut()
                .push(ActivityRecord::scheduled(sequence, spec));
            match CheckpointEnvelope::encode_with_limits(&payload, limits) {
                Ok(checkpoint) => Evaluation::Scheduled {
                    activity: logical_id,
                    checkpoint,
                },
                Err(error) => Evaluation::CheckpointRejected(error),
            }
        }
        Some(ContextDecision::ExistingPending { logical_id, state }) => Evaluation::Pending {
            activity: logical_id,
            state,
        },
        Some(ContextDecision::Nondeterminism(error)) => Evaluation::Nondeterminism(error),
        None => match poll {
            Poll::Ready(result) => {
                if cursor != payload.activities().len() {
                    let remaining = payload.activities().len() - cursor;
                    return Evaluation::Nondeterminism(Nondeterminism::UnusedHistory {
                        consumed: u64::try_from(cursor)
                            .expect("validated history length fits in u64"),
                        remaining: u64::try_from(remaining)
                            .expect("validated history length fits in u64"),
                    });
                }
                match CheckpointEnvelope::encode_with_limits(&payload, limits) {
                    Ok(checkpoint) => Evaluation::Complete { result, checkpoint },
                    Err(error) => Evaluation::CheckpointRejected(error),
                }
            }
            Poll::Pending => Evaluation::WorkflowStalled,
        },
    }
}
