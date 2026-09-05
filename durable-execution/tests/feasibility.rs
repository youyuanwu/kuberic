mod support;

use std::collections::BTreeSet;

use async_trait::async_trait;
use kuberic_durable_execution::{
    ActivityName, DurableHost, ExactBytes, ExecutionId, FeasibilityClassification,
    FeasibilityInputs, HOST_OUTCOME_VARIANTS, HostEpoch, HostOutcome, InMemoryCheckpointStore,
    Workflow, WorkflowContext, classify_feasibility,
};
use support::scenarios::{ScenarioId, run_conformance_matrix};

const EXPECTED_FR_013_SCENARIOS: usize = 20;

struct SelectedOrdinaryAsyncSurface;

// FR012_SELECTED_WORKFLOW_START
#[async_trait(?Send)]
impl Workflow for SelectedOrdinaryAsyncSurface {
    async fn run(&self, context: &mut WorkflowContext<'_>, input: ExactBytes) -> ExactBytes {
        context
            .activity(ActivityName::new("ordinary-async", 1).unwrap(), input)
            .await
    }
}
// FR012_SELECTED_WORKFLOW_END

#[derive(Clone, Copy)]
struct PredicateEvidence {
    name: &'static str,
    passed: bool,
}

#[test]
fn mechanically_assesses_the_selected_surface_and_full_denominator() {
    let scenarios = run_conformance_matrix();
    let registry_ids: Vec<_> = ScenarioId::ALL.iter().map(|id| id.stable_id()).collect();
    let unique_registry_ids: BTreeSet<_> = registry_ids.iter().copied().collect();

    assert_eq!(
        ScenarioId::ALL.len(),
        EXPECTED_FR_013_SCENARIOS,
        "the sole FR-013 registry must retain the reviewed denominator"
    );
    assert_eq!(
        scenarios.len(),
        EXPECTED_FR_013_SCENARIOS,
        "the runner must evaluate every registered FR-013 scenario"
    );
    assert_eq!(
        unique_registry_ids.len(),
        EXPECTED_FR_013_SCENARIOS,
        "FR-013 stable IDs must be unique"
    );
    for (index, id) in registry_ids.iter().enumerate() {
        assert_eq!(
            *id,
            format!("FR-013-{:02}", index + 1),
            "FR-013 stable IDs must be contiguous"
        );
    }

    let source = include_str!("feasibility.rs");
    let workflow_body = source
        .split_once("// FR012_SELECTED_WORKFLOW_START")
        .unwrap()
        .1
        .split_once("// FR012_SELECTED_WORKFLOW_END")
        .unwrap()
        .0;
    let framework_operation_count = workflow_body.matches(".activity(").count();
    let authored_poll_or_state_machine = workflow_body.contains(concat!("fn po", "ll("))
        || workflow_body.contains(concat!("impl Future", " for"))
        || workflow_body.contains(concat!("state_", "machine"));

    let library_exports = include_str!("../src/lib.rs");
    let ordinary_async_exported =
        library_exports.contains("pub use workflow::{Workflow, WorkflowContext};");
    let fallback_exported = library_exports.contains(concat!("mod po", "ll;"))
        || library_exports.contains("ReplayWorkflow")
        || library_exports.contains("ReplayContext");
    let public_authoring_surface_count =
        usize::from(ordinary_async_exported) + usize::from(fallback_exported);

    let store = InMemoryCheckpointStore::new();
    let mut host = DurableHost::new(store, HostEpoch::from_bytes([1; 16]));
    let first_turn = host.turn(
        &SelectedOrdinaryAsyncSurface,
        ExecutionId::from_bytes([1; 16]),
        ExactBytes::new(b"input"),
    );

    let fr_012 = [
        PredicateEvidence {
            name: "one-activity workflow is an ordinary async method",
            passed: matches!(first_turn, HostOutcome::ScheduleAccepted { .. })
                && workflow_body.matches("async fn run").count() == 1,
        },
        PredicateEvidence {
            name: "no author-written Future, poll, or state machine",
            passed: !authored_poll_or_state_machine,
        },
        PredicateEvidence {
            name: "workflow body uses no more than two framework-specific operations",
            passed: framework_operation_count <= 2,
        },
        PredicateEvidence {
            name: "every registered fixture runs through the integration-test public API",
            passed: scenarios.len() == ScenarioId::ALL.len()
                && scenarios
                    .iter()
                    .all(|scenario| !scenario.assertions.is_empty()),
        },
        PredicateEvidence {
            name: "exactly one public authoring surface remains",
            passed: ordinary_async_exported
                && !fallback_exported
                && public_authoring_surface_count == 1,
        },
    ];

    let assertion_count = scenarios
        .iter()
        .map(|scenario| scenario.assertions.len())
        .sum::<usize>();
    let passed_scenarios = scenarios
        .iter()
        .filter(|scenario| scenario.passed())
        .count();
    let passed_assertions = scenarios
        .iter()
        .flat_map(|scenario| &scenario.assertions)
        .filter(|assertion| assertion.passed)
        .count();
    println!("EVIDENCE selected_surface=\"ordinary async Workflow::run\"");
    println!(
        "COUNT taxonomy=public_authoring_surfaces value={public_authoring_surface_count} bound=1"
    );
    println!(
        "COUNT taxonomy=framework_specific_workflow_body_operations value={framework_operation_count} bound=2"
    );
    println!(
        "COUNT taxonomy=public_host_outcome_variants value={} bound=none",
        HOST_OUTCOME_VARIANTS.len()
    );
    println!(
        "COUNT taxonomy=fr_013_scenarios value={} denominator={EXPECTED_FR_013_SCENARIOS} passed={passed_scenarios}",
        scenarios.len()
    );
    println!("COUNT taxonomy=fr_013_assertions value={assertion_count} passed={passed_assertions}");

    for predicate in fr_012 {
        println!(
            "FR012 status={} predicate={:?}",
            status(predicate.passed),
            predicate.name
        );
    }
    for scenario in &scenarios {
        println!(
            "SCENARIO id={} status={} assertions={} setup={:?}",
            scenario.id.stable_id(),
            status(scenario.passed()),
            scenario.assertions.len(),
            scenario.setup
        );
        for (index, assertion) in scenario.assertions.iter().enumerate() {
            println!(
                "ASSERTION scenario={} index={} status={} text={:?}",
                scenario.id.stable_id(),
                index + 1,
                status(assertion.passed),
                assertion.assertion
            );
        }
    }

    let safety_and_determinism_pass = scenarios.iter().all(|scenario| scenario.passed());
    let all_conformance_pass = passed_scenarios == EXPECTED_FR_013_SCENARIOS
        && passed_assertions == assertion_count
        && scenarios
            .iter()
            .all(|scenario| !scenario.assertions.is_empty());
    let authoring_simplicity_pass = fr_012.iter().all(|predicate| predicate.passed);
    let has_in_scope_limitation = false;
    let classification = classify_feasibility(FeasibilityInputs {
        safety_and_determinism_pass,
        all_conformance_pass,
        authoring_simplicity_pass,
        has_in_scope_limitation,
    });

    println!(
        "INPUT safety_and_determinism_pass={safety_and_determinism_pass} all_conformance_pass={all_conformance_pass} authoring_simplicity_pass={authoring_simplicity_pass} has_in_scope_limitation={has_in_scope_limitation}"
    );
    println!(
        "CLASSIFICATION value={}",
        classification_name(classification)
    );
}

#[test]
fn fr_014_classifier_matches_all_truth_table_cases() {
    for safety_and_determinism_pass in [false, true] {
        for all_conformance_pass in [false, true] {
            for authoring_simplicity_pass in [false, true] {
                for has_in_scope_limitation in [false, true] {
                    let inputs = FeasibilityInputs {
                        safety_and_determinism_pass,
                        all_conformance_pass,
                        authoring_simplicity_pass,
                        has_in_scope_limitation,
                    };
                    let expected = if !safety_and_determinism_pass {
                        FeasibilityClassification::Infeasible
                    } else if !all_conformance_pass
                        || !authoring_simplicity_pass
                        || has_in_scope_limitation
                    {
                        FeasibilityClassification::ConditionallyFeasible
                    } else {
                        FeasibilityClassification::Feasible
                    };
                    assert_eq!(classify_feasibility(inputs), expected, "{inputs:?}");
                }
            }
        }
    }
}

const fn status(passed: bool) -> &'static str {
    if passed { "pass" } else { "fail" }
}

const fn classification_name(classification: FeasibilityClassification) -> &'static str {
    match classification {
        FeasibilityClassification::Feasible => "feasible",
        FeasibilityClassification::ConditionallyFeasible => "conditionally feasible",
        FeasibilityClassification::Infeasible => "infeasible",
    }
}
