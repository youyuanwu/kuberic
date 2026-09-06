use std::task::Poll;

use async_trait::async_trait;
use futures::future::poll_fn;
use serde::{Deserialize, Serialize};

use crate::{
    ActivityRecord, ActivitySequence, ActivitySpec, ActivityState, ExactBytes, ExecutionId,
    LogicalActivityId, Nondeterminism,
};

/// Exact terminal result of one workflow execution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TerminalOutcome {
    Succeeded(ExactBytes),
    Failed(ExactBytes),
}

impl TerminalOutcome {
    pub fn succeeded(payload: impl Into<ExactBytes>) -> Self {
        Self::Succeeded(payload.into())
    }

    pub fn failed(payload: impl Into<ExactBytes>) -> Self {
        Self::Failed(payload.into())
    }

    pub const fn payload(&self) -> &ExactBytes {
        match self {
            Self::Succeeded(payload) | Self::Failed(payload) => payload,
        }
    }
}

/// Provisional ordinary-async workflow authoring surface.
#[async_trait]
pub trait Workflow: Sync {
    async fn run(&self, context: &mut WorkflowContext<'_>, input: ExactBytes) -> TerminalOutcome;
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

    pub async fn activity(&mut self, spec: ActivitySpec) -> ExactBytes {
        poll_fn(|_| self.poll_activity(&spec)).await
    }

    pub(crate) const fn cursor(&self) -> usize {
        self.cursor
    }

    fn poll_activity(&mut self, spec: &ActivitySpec) -> Poll<ExactBytes> {
        if self.decision.is_some() {
            return Poll::Pending;
        }

        let sequence = ActivitySequence::new(
            u64::try_from(self.cursor).expect("validated history length fits in u64"),
        );
        let requested_id = LogicalActivityId::new(self.execution_id, sequence, spec.clone());

        let Some(record) = self.history.get(self.cursor) else {
            self.decision = Some(ContextDecision::Schedule {
                sequence,
                spec: spec.clone(),
                logical_id: requested_id,
            });
            return Poll::Pending;
        };

        if record.spec() != spec {
            self.decision = Some(ContextDecision::Nondeterminism(
                Nondeterminism::ActivityMismatch {
                    sequence,
                    recorded: record.spec().clone(),
                    requested: spec.clone(),
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
        spec: ActivitySpec,
        logical_id: LogicalActivityId,
    },
    ExistingPending {
        logical_id: LogicalActivityId,
        state: ActivityState,
    },
    Nondeterminism(Nondeterminism),
}
