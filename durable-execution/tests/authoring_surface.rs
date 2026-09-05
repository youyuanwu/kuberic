mod support;

use async_trait::async_trait;
use futures::executor::block_on;
use kuberic_durable_execution::{
    ActivityName, DurableHost, ExactBytes, ExecutionId, HOST_OUTCOME_VARIANTS, HostEpoch,
    HostOutcome, InMemoryCheckpointStore, Workflow, WorkflowContext,
};
use support::scenarios::{ScenarioId, run_conformance_matrix};

struct OrdinaryAsyncWorkflow;

// FR012_WORKFLOW_START
#[async_trait(?Send)]
impl Workflow for OrdinaryAsyncWorkflow {
    async fn run(&self, context: &mut WorkflowContext<'_>, input: ExactBytes) -> ExactBytes {
        context
            .activity(ActivityName::new("ordinary-async", 1).unwrap(), input)
            .await
    }
}
// FR012_WORKFLOW_END

#[test]
fn ordinary_async_mechanically_passes_fr_012_and_is_the_sole_surface() {
    let source = include_str!("authoring_surface.rs");
    let body = source
        .split_once("// FR012_WORKFLOW_START")
        .unwrap()
        .1
        .split_once("// FR012_WORKFLOW_END")
        .unwrap()
        .0;
    let framework_operation_count = body.matches(".activity(").count();
    let authored_poll = body.contains(concat!("fn po", "ll("))
        || body.contains(concat!("impl Future", " for"))
        || body.contains(concat!("state_", "machine"));

    let store = InMemoryCheckpointStore::new();
    let mut host = DurableHost::new(store, HostEpoch::from_bytes([1; 16]));
    let first_turn = block_on(host.turn(
        &OrdinaryAsyncWorkflow,
        ExecutionId::from_bytes([1; 16]),
        ExactBytes::new(b"input"),
    ));
    let all_scenarios = block_on(run_conformance_matrix());
    for scenario in &all_scenarios {
        println!(
            "{} public-API fixture: {}",
            scenario.id.stable_id(),
            if scenario.passed() { "PASS" } else { "FAIL" }
        );
    }
    let library_exports = include_str!("../src/lib.rs");
    let async_surface_exported =
        library_exports.contains("pub use workflow::{Workflow, WorkflowContext};");
    let fallback_surface_exported = library_exports.contains(concat!("mod po", "ll;"))
        || library_exports.contains("ReplayWorkflow")
        || library_exports.contains("ReplayContext");

    let predicates = [
        (
            "one-activity workflow is an ordinary async method",
            matches!(first_turn, HostOutcome::ScheduleAccepted { .. }),
        ),
        ("no author-written poll or state machine", !authored_poll),
        (
            "workflow body uses no more than two framework operations",
            framework_operation_count <= 2,
        ),
        (
            "every FR-013 fixture uses and passes through public APIs",
            all_scenarios.len() == ScenarioId::ALL.len()
                && all_scenarios.iter().all(|scenario| scenario.passed()),
        ),
        (
            "exactly one public authoring surface remains",
            async_surface_exported && !fallback_surface_exported,
        ),
    ];

    println!("selected surface: ordinary async Workflow::run");
    println!("workflow-body framework operations: {framework_operation_count}");
    let outcome_count = HOST_OUTCOME_VARIANTS.len();
    println!("public HostOutcome variants: {outcome_count}");
    assert_eq!(outcome_count, 10);
    for (predicate, passed) in predicates {
        println!("[{}] {predicate}", if passed { "PASS" } else { "FAIL" });
        assert!(passed, "FR-012 predicate failed: {predicate}");
    }
}
