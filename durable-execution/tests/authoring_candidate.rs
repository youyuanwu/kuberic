use async_trait::async_trait;
use kuberic_durable_execution::{
    ActivityName, ActivitySpec, CheckpointLimits, Evaluation, ExactBytes, ExecutionId, Workflow,
    WorkflowContext, evaluate,
};

struct OneActivityWorkflow;

#[async_trait(?Send)]
impl Workflow for OneActivityWorkflow {
    async fn run(&self, context: &mut WorkflowContext<'_>, input: ExactBytes) -> ExactBytes {
        context
            .activity(ActivitySpec::new(
                ActivityName::new("one-activity", 1).unwrap(),
                input,
                1024,
            ))
            .await
    }
}

#[test]
fn one_activity_is_an_ordinary_async_method_with_one_framework_call() {
    let outcome = evaluate(
        &OneActivityWorkflow,
        ExecutionId::from_bytes([1; 16]),
        ExactBytes::new(b"exact input"),
        None,
        CheckpointLimits::new(16, 100_000).unwrap(),
    );

    assert!(matches!(outcome, Evaluation::Scheduled { .. }));
}
