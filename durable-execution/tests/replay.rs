use std::cell::Cell;

use async_trait::async_trait;
use kuberic_durable_execution::{
    ActivityName, ActivityRecord, ActivitySequence, ActivityState, AttemptId, CheckpointEnvelope,
    CheckpointError, CheckpointPayload, Evaluation, ExactBytes, ExecutionId, HostEpoch,
    IdentityError, LogicalActivityId, Nondeterminism, Workflow, WorkflowContext, evaluate,
};

fn execution(byte: u8) -> ExecutionId {
    ExecutionId::from_bytes([byte; 16])
}

fn name(value: &str, version: u32) -> ActivityName {
    ActivityName::new(value, version).unwrap()
}

fn bytes(value: &[u8]) -> ExactBytes {
    ExactBytes::new(value)
}

#[derive(Clone)]
struct LinearWorkflow {
    activities: Vec<(ActivityName, ExactBytes)>,
}

#[async_trait(?Send)]
impl Workflow for LinearWorkflow {
    async fn run(&self, context: &mut WorkflowContext<'_>, _input: ExactBytes) -> ExactBytes {
        let mut results = Vec::new();
        for (name, input) in &self.activities {
            results.extend(
                context
                    .activity(name.clone(), input.clone())
                    .await
                    .as_slice(),
            );
        }
        ExactBytes::new(results)
    }
}

fn envelope(
    execution_id: ExecutionId,
    workflow_input: ExactBytes,
    activities: Vec<ActivityRecord>,
) -> CheckpointEnvelope {
    CheckpointEnvelope::encode(&CheckpointPayload::new(
        execution_id,
        workflow_input,
        activities,
    ))
    .unwrap()
}

#[test]
fn first_turn_schedules_exactly_one_activity() {
    let workflow = LinearWorkflow {
        activities: vec![(name("greeting", 1), bytes(b"A"))],
    };

    let Evaluation::Scheduled {
        activity,
        checkpoint,
    } = evaluate(&workflow, execution(1), bytes(b"workflow"), None)
    else {
        panic!("first turn did not schedule");
    };

    assert_eq!(activity.execution_id(), execution(1));
    assert_eq!(activity.sequence(), ActivitySequence::new(0));
    assert_eq!(activity.name(), &name("greeting", 1));
    assert_eq!(activity.input(), &bytes(b"A"));
    let payload = checkpoint
        .decode_and_validate(execution(1), &bytes(b"workflow"))
        .unwrap();
    assert_eq!(payload.activities().len(), 1);
    assert_eq!(payload.activities()[0].state(), &ActivityState::Scheduled);
}

#[test]
fn completed_result_replays_without_rescheduling() {
    let workflow = LinearWorkflow {
        activities: vec![(name("greeting", 1), bytes(b"A"))],
    };
    let checkpoint = envelope(
        execution(2),
        bytes(b"workflow"),
        vec![ActivityRecord::completed(
            ActivitySequence::new(0),
            name("greeting", 1),
            bytes(b"A"),
            bytes(b"recorded-result"),
        )],
    );

    let first = evaluate(
        &workflow,
        execution(2),
        bytes(b"workflow"),
        Some(&checkpoint),
    );
    let second = evaluate(
        &workflow,
        execution(2),
        bytes(b"workflow"),
        Some(&checkpoint),
    );

    assert_eq!(first, second);
    let Evaluation::Complete {
        result,
        checkpoint: replayed,
    } = first
    else {
        panic!("completed history did not complete");
    };
    assert_eq!(result, bytes(b"recorded-result"));
    assert_eq!(
        replayed
            .decode_and_validate(execution(2), &bytes(b"workflow"))
            .unwrap(),
        checkpoint
            .decode_and_validate(execution(2), &bytes(b"workflow"))
            .unwrap()
    );
}

#[test]
fn completed_prefix_can_schedule_one_next_activity() {
    let workflow = LinearWorkflow {
        activities: vec![
            (name("first", 1), bytes(b"A")),
            (name("second", 2), bytes(b"B")),
        ],
    };
    let checkpoint = envelope(
        execution(3),
        bytes(b"workflow"),
        vec![ActivityRecord::completed(
            ActivitySequence::new(0),
            name("first", 1),
            bytes(b"A"),
            bytes(b"done"),
        )],
    );

    let Evaluation::Scheduled {
        activity,
        checkpoint,
    } = evaluate(
        &workflow,
        execution(3),
        bytes(b"workflow"),
        Some(&checkpoint),
    )
    else {
        panic!("next activity was not scheduled");
    };

    assert_eq!(activity.sequence(), ActivitySequence::new(1));
    let decoded = checkpoint
        .decode_and_validate(execution(3), &bytes(b"workflow"))
        .unwrap();
    assert_eq!(decoded.activities().len(), 2);
    assert_eq!(decoded.activities()[1].state(), &ActivityState::Scheduled);
}

#[test]
fn changed_order_name_or_exact_input_is_nondeterminism() {
    let cases = [
        (name("second", 1), bytes(b"A")),
        (name("first", 2), bytes(b"A")),
        (name("first", 1), bytes(b"a")),
    ];
    let checkpoint = envelope(
        execution(4),
        bytes(b"workflow"),
        vec![ActivityRecord::completed(
            ActivitySequence::new(0),
            name("first", 1),
            bytes(b"A"),
            bytes(b"done"),
        )],
    );

    for (requested_name, requested_input) in cases {
        let workflow = LinearWorkflow {
            activities: vec![(requested_name, requested_input)],
        };
        assert!(matches!(
            evaluate(
                &workflow,
                execution(4),
                bytes(b"workflow"),
                Some(&checkpoint)
            ),
            Evaluation::Nondeterminism(Nondeterminism::ActivityMismatch { .. })
        ));
    }
}

#[test]
fn unused_otherwise_valid_history_is_nondeterminism() {
    let workflow = LinearWorkflow { activities: vec![] };
    let checkpoint = envelope(
        execution(5),
        bytes(b"workflow"),
        vec![ActivityRecord::completed(
            ActivitySequence::new(0),
            name("old", 1),
            bytes(b"A"),
            bytes(b"done"),
        )],
    );

    assert_eq!(
        evaluate(
            &workflow,
            execution(5),
            bytes(b"workflow"),
            Some(&checkpoint)
        ),
        Evaluation::Nondeterminism(Nondeterminism::UnusedHistory {
            consumed: 0,
            remaining: 1,
        })
    );
}

#[test]
fn logical_identity_equality_and_rendering_cover_the_full_semantic_tuple() {
    let base = LogicalActivityId::new(
        execution(6),
        ActivitySequence::new(7),
        name("write", 3),
        bytes(&[0, 1, 255]),
    );
    let same = LogicalActivityId::new(
        execution(6),
        ActivitySequence::new(7),
        name("write", 3),
        bytes(&[0, 1, 255]),
    );
    assert_eq!(base, same);
    assert_eq!(base.to_external_id(), same.to_external_id());

    let variants = [
        LogicalActivityId::new(
            execution(7),
            ActivitySequence::new(7),
            name("write", 3),
            bytes(&[0, 1, 255]),
        ),
        LogicalActivityId::new(
            execution(6),
            ActivitySequence::new(8),
            name("write", 3),
            bytes(&[0, 1, 255]),
        ),
        LogicalActivityId::new(
            execution(6),
            ActivitySequence::new(7),
            name("write-other", 3),
            bytes(&[0, 1, 255]),
        ),
        LogicalActivityId::new(
            execution(6),
            ActivitySequence::new(7),
            name("write", 4),
            bytes(&[0, 1, 255]),
        ),
        LogicalActivityId::new(
            execution(6),
            ActivitySequence::new(7),
            name("write", 3),
            bytes(&[0, 1, 254]),
        ),
    ];
    for variant in variants {
        assert_ne!(base, variant);
        assert_ne!(base.to_external_id(), variant.to_external_id());
    }
    assert!(base.to_external_id().ends_with(":3:0001ff"));
}

#[test]
fn changed_name_and_input_cannot_alias_a_logical_external_id() {
    let original = LogicalActivityId::new(
        execution(8),
        ActivitySequence::new(0),
        name("effect", 1),
        bytes(b"input"),
    );
    let renamed = LogicalActivityId::new(
        execution(8),
        ActivitySequence::new(0),
        name("effect", 2),
        bytes(b"input"),
    );
    let changed_input = LogicalActivityId::new(
        execution(8),
        ActivitySequence::new(0),
        name("effect", 1),
        bytes(b"Input"),
    );

    assert_ne!(original.to_external_id(), renamed.to_external_id());
    assert_ne!(original.to_external_id(), changed_input.to_external_id());
}

#[test]
fn name_and_attempt_identifiers_reject_unversioned_or_reserved_values() {
    assert_eq!(
        ActivityName::new("", 1),
        Err(IdentityError::EmptyActivityName)
    );
    assert_eq!(
        ActivityName::new("effect", 0),
        Err(IdentityError::ZeroActivityVersion)
    );
    assert_eq!(
        AttemptId::new(HostEpoch::from_bytes([1; 16]), 0),
        Err(IdentityError::ZeroAttemptCounter)
    );
}

#[test]
fn rejects_invalid_sequence_and_pending_shapes() {
    let workflow = LinearWorkflow {
        activities: vec![(name("first", 1), bytes(b"A"))],
    };
    let invalid_sequence = envelope(
        execution(9),
        bytes(b"workflow"),
        vec![ActivityRecord::completed(
            ActivitySequence::new(1),
            name("first", 1),
            bytes(b"A"),
            bytes(b"done"),
        )],
    );
    assert!(matches!(
        evaluate(
            &workflow,
            execution(9),
            bytes(b"workflow"),
            Some(&invalid_sequence)
        ),
        Evaluation::CheckpointRejected(CheckpointError::NonContiguousSequence { .. })
    ));

    let invalid_pending = envelope(
        execution(9),
        bytes(b"workflow"),
        vec![
            ActivityRecord::scheduled(ActivitySequence::new(0), name("first", 1), bytes(b"A")),
            ActivityRecord::completed(
                ActivitySequence::new(1),
                name("second", 1),
                bytes(b"B"),
                bytes(b"done"),
            ),
        ],
    );
    assert!(matches!(
        evaluate(
            &workflow,
            execution(9),
            bytes(b"workflow"),
            Some(&invalid_pending)
        ),
        Evaluation::CheckpointRejected(CheckpointError::PendingActivityNotFinal { .. })
    ));
}

struct PollCountingWorkflow {
    polls: Cell<u32>,
}

#[async_trait(?Send)]
impl Workflow for PollCountingWorkflow {
    async fn run(&self, _context: &mut WorkflowContext<'_>, _input: ExactBytes) -> ExactBytes {
        self.polls.set(self.polls.get() + 1);
        bytes(b"complete")
    }
}

#[test]
fn identity_input_and_format_validation_happen_before_workflow_polling() {
    let workflow = PollCountingWorkflow {
        polls: Cell::new(0),
    };
    let valid_payload = CheckpointPayload::new(execution(10), bytes(b"expected"), vec![]);
    let unsupported = CheckpointEnvelope::new(
        2,
        CheckpointEnvelope::encode(&valid_payload)
            .unwrap()
            .payload()
            .clone(),
    );

    assert!(matches!(
        evaluate(
            &workflow,
            execution(10),
            bytes(b"expected"),
            Some(&unsupported)
        ),
        Evaluation::CheckpointRejected(CheckpointError::UnsupportedFormat { .. })
    ));
    assert_eq!(workflow.polls.get(), 0);

    let wrong_execution = envelope(execution(11), bytes(b"expected"), vec![]);
    assert!(matches!(
        evaluate(
            &workflow,
            execution(10),
            bytes(b"expected"),
            Some(&wrong_execution)
        ),
        Evaluation::CheckpointRejected(CheckpointError::ExecutionMismatch { .. })
    ));
    assert_eq!(workflow.polls.get(), 0);

    let wrong_input = envelope(execution(10), bytes(b"different"), vec![]);
    assert!(matches!(
        evaluate(
            &workflow,
            execution(10),
            bytes(b"expected"),
            Some(&wrong_input)
        ),
        Evaluation::CheckpointRejected(CheckpointError::WorkflowInputMismatch { .. })
    ));
    assert_eq!(workflow.polls.get(), 0);
}
