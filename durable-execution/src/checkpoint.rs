use base64::encoded_len;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ActivityName, ActivitySequence, ActivitySpec, AttemptId, ExactBytes, ExecutionId,
    LogicalActivityId,
};

pub const CHECKPOINT_FORMAT_VERSION: u32 = 2;

/// Required limits for every loaded or proposed checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckpointLimits {
    max_activity_records: usize,
    max_encoded_bytes: usize,
}

impl CheckpointLimits {
    pub fn new(
        max_activity_records: usize,
        max_encoded_bytes: usize,
    ) -> Result<Self, CheckpointError> {
        if max_activity_records == 0 {
            return Err(CheckpointError::ZeroActivityRecordLimit);
        }
        if max_encoded_bytes == 0 {
            return Err(CheckpointError::ZeroEncodedCheckpointLimit);
        }
        Ok(Self {
            max_activity_records,
            max_encoded_bytes,
        })
    }

    pub const fn max_activity_records(self) -> usize {
        self.max_activity_records
    }

    pub const fn max_encoded_bytes(self) -> usize {
        self.max_encoded_bytes
    }
}

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
            .map_err(invalid_json)
    }

    pub fn encode_with_limits(
        payload: &CheckpointPayload,
        limits: CheckpointLimits,
    ) -> Result<Self, CheckpointError> {
        validate_activity_count(payload, limits)?;
        let checkpoint = Self::encode(payload)?;
        checkpoint.validate_encoded_size(limits)?;
        Ok(checkpoint)
    }

    pub const fn format_version(&self) -> u32 {
        self.format_version
    }

    pub fn payload(&self) -> &ExactBytes {
        &self.payload
    }

    /// Canonical JSON bytes required to persist this complete envelope.
    pub fn encoded_len(&self) -> Result<usize, CheckpointError> {
        serde_json::to_vec(self)
            .map(|encoded| encoded.len())
            .map_err(invalid_json)
    }

    pub fn decode_and_validate(
        &self,
        expected_execution_id: ExecutionId,
        expected_workflow_input: &ExactBytes,
        limits: CheckpointLimits,
    ) -> Result<CheckpointPayload, CheckpointError> {
        if self.format_version != CHECKPOINT_FORMAT_VERSION {
            return Err(CheckpointError::UnsupportedFormat {
                actual: self.format_version,
                supported: CHECKPOINT_FORMAT_VERSION,
            });
        }
        self.validate_encoded_size(limits)?;

        let payload: CheckpointPayload =
            serde_json::from_slice(self.payload.as_slice()).map_err(invalid_json)?;
        payload.validate(expected_execution_id, expected_workflow_input, limits)?;
        Ok(payload)
    }

    fn validate_encoded_size(&self, limits: CheckpointLimits) -> Result<(), CheckpointError> {
        let actual = self.encoded_len()?;
        if actual > limits.max_encoded_bytes {
            return Err(CheckpointError::EncodedCheckpointLimitExceeded {
                actual,
                maximum: limits.max_encoded_bytes,
            });
        }
        Ok(())
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
        limits: CheckpointLimits,
    ) -> Result<(), CheckpointError> {
        validate_activity_count(self, limits)?;
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
            if let ActivityState::Completed { result } = &record.state {
                let actual = u64::try_from(result.as_slice().len())
                    .map_err(|_| CheckpointError::ResultLengthUnrepresentable)?;
                if actual > record.spec.max_result_bytes() {
                    return Err(CheckpointError::CompletedResultExceedsDeclared {
                        sequence: record.sequence,
                        actual,
                        maximum: record.spec.max_result_bytes(),
                    });
                }
            } else if index + 1 != self.activities.len() {
                return Err(CheckpointError::PendingActivityNotFinal {
                    sequence: record.sequence,
                });
            }
        }
        Ok(())
    }

    /// Exact encoded envelope size for completing the final record with a
    /// result at its declared maximum, without allocating that result.
    pub(crate) fn maximum_completed_encoded_len(&self) -> Result<usize, CheckpointError> {
        let record = self
            .activities
            .last()
            .ok_or(CheckpointError::MissingPendingActivity)?;
        let result_len = usize::try_from(record.spec.max_result_bytes())
            .map_err(|_| CheckpointError::ResultLengthUnrepresentable)?;
        let result_base64_len =
            encoded_len(result_len, true).ok_or(CheckpointError::EncodedLengthOverflow)?;

        let mut empty_completion = self.clone();
        let final_record = empty_completion
            .activities
            .last_mut()
            .expect("final record was checked above");
        final_record.state = ActivityState::Completed {
            result: ExactBytes::default(),
        };
        let empty_inner_len = serde_json::to_vec(&empty_completion)
            .map_err(invalid_json)?
            .len();
        let projected_inner_len = empty_inner_len
            .checked_add(result_base64_len)
            .ok_or(CheckpointError::EncodedLengthOverflow)?;
        let projected_payload_base64_len =
            encoded_len(projected_inner_len, true).ok_or(CheckpointError::EncodedLengthOverflow)?;
        let empty_envelope_len =
            CheckpointEnvelope::new(CHECKPOINT_FORMAT_VERSION, ExactBytes::default())
                .encoded_len()?;
        empty_envelope_len
            .checked_add(projected_payload_base64_len)
            .ok_or(CheckpointError::EncodedLengthOverflow)
    }
}

/// One activity in contiguous zero-based workflow history.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActivityRecord {
    sequence: ActivitySequence,
    spec: ActivitySpec,
    state: ActivityState,
}

impl ActivityRecord {
    pub const fn new(sequence: ActivitySequence, spec: ActivitySpec, state: ActivityState) -> Self {
        Self {
            sequence,
            spec,
            state,
        }
    }

    pub const fn scheduled(sequence: ActivitySequence, spec: ActivitySpec) -> Self {
        Self::new(sequence, spec, ActivityState::Scheduled)
    }

    pub const fn completed(
        sequence: ActivitySequence,
        spec: ActivitySpec,
        result: ExactBytes,
    ) -> Self {
        Self::new(sequence, spec, ActivityState::Completed { result })
    }

    pub const fn dispatch_exposed(
        sequence: ActivitySequence,
        spec: ActivitySpec,
        attempt_id: AttemptId,
    ) -> Self {
        Self::new(
            sequence,
            spec,
            ActivityState::DispatchExposed { attempt_id },
        )
    }

    pub const fn sequence(&self) -> ActivitySequence {
        self.sequence
    }

    pub const fn spec(&self) -> &ActivitySpec {
        &self.spec
    }

    pub const fn name(&self) -> &ActivityName {
        self.spec.name()
    }

    pub const fn input(&self) -> &ExactBytes {
        self.spec.input()
    }

    pub const fn max_result_bytes(&self) -> u64 {
        self.spec.max_result_bytes()
    }

    pub const fn state(&self) -> &ActivityState {
        &self.state
    }

    pub fn logical_id(&self, execution_id: ExecutionId) -> LogicalActivityId {
        LogicalActivityId::new(execution_id, self.sequence, self.spec.clone())
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
    #[error("maximum activity records must be greater than zero")]
    ZeroActivityRecordLimit,
    #[error("maximum encoded checkpoint bytes must be greater than zero")]
    ZeroEncodedCheckpointLimit,
    #[error("checkpoint has {actual} activity records; configured maximum is {maximum}")]
    ActivityRecordLimitExceeded { actual: usize, maximum: usize },
    #[error("encoded checkpoint uses {actual} bytes; configured maximum is {maximum}")]
    EncodedCheckpointLimitExceeded { actual: usize, maximum: usize },
    #[error("activity result length cannot be represented on this platform")]
    ResultLengthUnrepresentable,
    #[error("encoded checkpoint length overflow")]
    EncodedLengthOverflow,
    #[error("capacity reservation requires a final pending activity")]
    MissingPendingActivity,
    #[error("activity {sequence} result uses {actual} bytes; declared maximum is {maximum}")]
    CompletedResultExceedsDeclared {
        sequence: ActivitySequence,
        actual: u64,
        maximum: u64,
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

fn validate_activity_count(
    payload: &CheckpointPayload,
    limits: CheckpointLimits,
) -> Result<(), CheckpointError> {
    let actual = payload.activities.len();
    if actual > limits.max_activity_records {
        return Err(CheckpointError::ActivityRecordLimitExceeded {
            actual,
            maximum: limits.max_activity_records,
        });
    }
    Ok(())
}

fn invalid_json(error: serde_json::Error) -> CheckpointError {
    CheckpointError::InvalidJson(error.to_string())
}
