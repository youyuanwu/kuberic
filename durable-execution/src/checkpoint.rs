use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ActivityName, ActivitySequence, AttemptId, ExactBytes, ExecutionId, LogicalActivityId,
};

pub const CHECKPOINT_FORMAT_VERSION: u32 = 1;

/// Version discriminator and opaque JSON payload bytes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckpointEnvelope {
    format_version: u32,
    payload: ExactBytes,
}

impl CheckpointEnvelope {
    pub fn new(format_version: u32, payload: ExactBytes) -> Self {
        Self {
            format_version,
            payload,
        }
    }

    pub fn encode(payload: &CheckpointPayload) -> Result<Self, CheckpointError> {
        serde_json::to_vec(payload)
            .map(ExactBytes::new)
            .map(|payload| Self::new(CHECKPOINT_FORMAT_VERSION, payload))
            .map_err(|error| CheckpointError::InvalidJson(error.to_string()))
    }

    pub const fn format_version(&self) -> u32 {
        self.format_version
    }

    pub fn payload(&self) -> &ExactBytes {
        &self.payload
    }

    pub fn decode_and_validate(
        &self,
        expected_execution_id: ExecutionId,
        expected_workflow_input: &ExactBytes,
    ) -> Result<CheckpointPayload, CheckpointError> {
        if self.format_version != CHECKPOINT_FORMAT_VERSION {
            return Err(CheckpointError::UnsupportedFormat {
                actual: self.format_version,
                supported: CHECKPOINT_FORMAT_VERSION,
            });
        }

        let payload: CheckpointPayload = serde_json::from_slice(self.payload.as_slice())
            .map_err(|error| CheckpointError::InvalidJson(error.to_string()))?;
        payload.validate(expected_execution_id, expected_workflow_input)?;
        Ok(payload)
    }
}

/// Decoded checkpoint state for one execution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckpointPayload {
    execution_id: ExecutionId,
    workflow_input: ExactBytes,
    activities: Vec<ActivityRecord>,
}

impl CheckpointPayload {
    pub fn new(
        execution_id: ExecutionId,
        workflow_input: ExactBytes,
        activities: Vec<ActivityRecord>,
    ) -> Self {
        Self {
            execution_id,
            workflow_input,
            activities,
        }
    }

    pub const fn execution_id(&self) -> ExecutionId {
        self.execution_id
    }

    pub fn workflow_input(&self) -> &ExactBytes {
        &self.workflow_input
    }

    pub fn activities(&self) -> &[ActivityRecord] {
        &self.activities
    }

    pub(crate) fn activities_mut(&mut self) -> &mut Vec<ActivityRecord> {
        &mut self.activities
    }

    pub fn validate(
        &self,
        expected_execution_id: ExecutionId,
        expected_workflow_input: &ExactBytes,
    ) -> Result<(), CheckpointError> {
        if self.execution_id != expected_execution_id {
            return Err(CheckpointError::ExecutionMismatch {
                expected: expected_execution_id,
                actual: self.execution_id,
            });
        }
        if self.workflow_input != *expected_workflow_input {
            return Err(CheckpointError::WorkflowInputMismatch {
                expected: expected_workflow_input.clone(),
                actual: self.workflow_input.clone(),
            });
        }

        for (index, record) in self.activities.iter().enumerate() {
            let expected = u64::try_from(index).map_err(|_| CheckpointError::HistoryTooLong)?;
            if record.sequence.get() != expected {
                return Err(CheckpointError::NonContiguousSequence {
                    expected: ActivitySequence::new(expected),
                    actual: record.sequence,
                });
            }
            if !matches!(record.state, ActivityState::Completed { .. })
                && index + 1 != self.activities.len()
            {
                return Err(CheckpointError::PendingActivityNotFinal {
                    sequence: record.sequence,
                });
            }
        }
        Ok(())
    }
}

/// One activity in contiguous zero-based workflow history.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActivityRecord {
    sequence: ActivitySequence,
    name: ActivityName,
    input: ExactBytes,
    state: ActivityState,
}

impl ActivityRecord {
    pub fn new(
        sequence: ActivitySequence,
        name: ActivityName,
        input: ExactBytes,
        state: ActivityState,
    ) -> Self {
        Self {
            sequence,
            name,
            input,
            state,
        }
    }

    pub fn scheduled(sequence: ActivitySequence, name: ActivityName, input: ExactBytes) -> Self {
        Self::new(sequence, name, input, ActivityState::Scheduled)
    }

    pub fn completed(
        sequence: ActivitySequence,
        name: ActivityName,
        input: ExactBytes,
        result: ExactBytes,
    ) -> Self {
        Self::new(sequence, name, input, ActivityState::Completed { result })
    }

    pub fn dispatch_exposed(
        sequence: ActivitySequence,
        name: ActivityName,
        input: ExactBytes,
        attempt_id: AttemptId,
    ) -> Self {
        Self::new(
            sequence,
            name,
            input,
            ActivityState::DispatchExposed { attempt_id },
        )
    }

    pub const fn sequence(&self) -> ActivitySequence {
        self.sequence
    }

    pub fn name(&self) -> &ActivityName {
        &self.name
    }

    pub fn input(&self) -> &ExactBytes {
        &self.input
    }

    pub fn state(&self) -> &ActivityState {
        &self.state
    }

    pub fn logical_id(&self, execution_id: ExecutionId) -> LogicalActivityId {
        LogicalActivityId::new(
            execution_id,
            self.sequence,
            self.name.clone(),
            self.input.clone(),
        )
    }
}

/// Persisted state of one logical activity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ActivityState {
    Scheduled,
    DispatchExposed { attempt_id: AttemptId },
    Completed { result: ExactBytes },
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CheckpointError {
    #[error("checkpoint format {actual} is unsupported; this build supports {supported}")]
    UnsupportedFormat { actual: u32, supported: u32 },
    #[error("checkpoint JSON is invalid: {0}")]
    InvalidJson(String),
    #[error("checkpoint execution identity differs from the requested execution")]
    ExecutionMismatch {
        expected: ExecutionId,
        actual: ExecutionId,
    },
    #[error("checkpoint workflow input differs from the exact requested input")]
    WorkflowInputMismatch {
        expected: ExactBytes,
        actual: ExactBytes,
    },
    #[error("activity history is too long to address with a u64 sequence")]
    HistoryTooLong,
    #[error("expected activity sequence {expected}, found {actual}")]
    NonContiguousSequence {
        expected: ActivitySequence,
        actual: ActivitySequence,
    },
    #[error("pending activity {sequence} must be the final history record")]
    PendingActivityNotFinal { sequence: ActivitySequence },
}
