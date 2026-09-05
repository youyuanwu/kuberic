use std::task::Poll;

use async_trait::async_trait;
use futures::future::poll_fn;

use crate::{
    ActivityName, ActivityRecord, ActivitySequence, ActivityState, ExactBytes, ExecutionId,
    LogicalActivityId, Nondeterminism,
};

/// Provisional ordinary-async workflow authoring surface.
#[async_trait(?Send)]
pub trait Workflow {
    async fn run(&self, context: &mut WorkflowContext<'_>, input: ExactBytes) -> ExactBytes;
}

/// Linear replay context. `activity` is its only workflow-body operation.
pub struct WorkflowContext<'history> {
    execution_id: ExecutionId,
    history: &'history [ActivityRecord],
    cursor: usize,
    pub(crate) decision: Option<ContextDecision>,
}

impl<'history> WorkflowContext<'history> {
    pub(crate) fn new(execution_id: ExecutionId, history: &'history [ActivityRecord]) -> Self {
        Self {
            execution_id,
            history,
            cursor: 0,
            decision: None,
        }
    }

    pub async fn activity(&mut self, name: ActivityName, input: ExactBytes) -> ExactBytes {
        poll_fn(|_| self.poll_activity(&name, &input)).await
    }

    pub(crate) const fn cursor(&self) -> usize {
        self.cursor
    }

    fn poll_activity(&mut self, name: &ActivityName, input: &ExactBytes) -> Poll<ExactBytes> {
        if self.decision.is_some() {
            return Poll::Pending;
        }

        let sequence = ActivitySequence::new(
            u64::try_from(self.cursor).expect("validated history length fits in u64"),
        );
        let requested_id =
            LogicalActivityId::new(self.execution_id, sequence, name.clone(), input.clone());

        let Some(record) = self.history.get(self.cursor) else {
            self.decision = Some(ContextDecision::Schedule {
                sequence,
                name: name.clone(),
                input: input.clone(),
                logical_id: requested_id,
            });
            return Poll::Pending;
        };

        if record.name() != name || record.input() != input {
            self.decision = Some(ContextDecision::Nondeterminism(
                Nondeterminism::ActivityMismatch {
                    sequence,
                    recorded_name: record.name().clone(),
                    requested_name: name.clone(),
                    recorded_input: record.input().clone(),
                    requested_input: input.clone(),
                },
            ));
            return Poll::Pending;
        }

        match record.state() {
            ActivityState::Completed { result } => {
                self.cursor += 1;
                Poll::Ready(result.clone())
            }
            state @ (ActivityState::Scheduled | ActivityState::DispatchExposed { .. }) => {
                self.decision = Some(ContextDecision::ExistingPending {
                    logical_id: requested_id,
                    state: state.clone(),
                });
                Poll::Pending
            }
        }
    }
}

pub(crate) enum ContextDecision {
    Schedule {
        sequence: ActivitySequence,
        name: ActivityName,
        input: ExactBytes,
        logical_id: LogicalActivityId,
    },
    ExistingPending {
        logical_id: LogicalActivityId,
        state: ActivityState,
    },
    Nondeterminism(Nondeterminism),
}
