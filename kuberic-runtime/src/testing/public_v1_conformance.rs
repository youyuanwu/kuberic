use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FutureObligation {
    None,
    Phase6Engine,
    Phase7Cutover,
}

#[derive(Clone, Copy)]
struct Gate {
    id: &'static str,
    name: &'static str,
    owner: &'static str,
    scenarios: &'static [&'static str],
    postgres: bool,
    expected: &'static str,
    eligibility: fn(),
    future: FutureObligation,
}

const PUBLIC_LIFECYCLE: &str = include_str!("../host/public_lifecycle.rs");
const REPLICATOR: &str = include_str!("../replicator/mod.rs");
const AUTHORITY: &str = include_str!("../authority.rs");
const HOST_SERVICE: &str = include_str!("../host/service.rs");

fn contract_gate() {}

fn ksf_09() {
    assert!(REPLICATOR.contains("struct ManagedReplicatorCapabilities"));
    assert!(!PUBLIC_LIFECYCLE.contains("ManagedReplicatorCapabilities"));
}

fn ksf_10() {
    assert!(HOST_SERVICE.contains("replication_listener"));
}

fn ksf_16() {
    for owner in [
        "trait ReplicationProgressStore",
        "trait BuildAuthorityStore",
        "trait BuildProgressStore",
    ] {
        assert!(
            AUTHORITY.contains(owner),
            "legacy continuation owner unexpectedly absent: {owner}"
        );
    }
}

const GATES: [Gate; 16] = [
    Gate {
        id: "KSF-01",
        name: "primary_configuration_follows_primary_role",
        owner: "lifecycle",
        scenarios: &["strict_trace_fixture_covers_the_public_callback_inventory_and_primary_guard"],
        postgres: true,
        expected: "primary role completes before primary configuration",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-02",
        name: "authorized_data_loss_false_true_error_ambiguity",
        owner: "recovery",
        scenarios: &[
            "public_v1_history_admission_gates_all_data_loss_outcomes",
            "public_v1_data_loss_replay_uses_attached_capability",
        ],
        postgres: true,
        expected: "logical authorization distinguishes every callback outcome",
        eligibility: contract_gate,
        future: FutureObligation::Phase6Engine,
    },
    Gate {
        id: "KSF-03",
        name: "same_role_secondary_receives_new_epoch",
        owner: "lifecycle",
        scenarios: &["public_v1_session_renewal_requires_higher_epoch"],
        postgres: true,
        expected: "same-role epoch renewal fences predecessor traffic",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-04",
        name: "configuration_contains_only_remote_active_members",
        owner: "configuration",
        scenarios: &[
            "public_v1_values_distinguishes_invalid_lsn",
            "public_v1_swap_captures_specific_quorum_capability",
            "public_v1_role_and_epoch_transitions_persist_current_authority",
        ],
        postgres: true,
        expected: "configuration contains only exact ready remote peers",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-05",
        name: "build_uses_invalid_target_progress_and_exact_attempt",
        owner: "build",
        scenarios: &[
            "public_v1_values_distinguishes_invalid_lsn",
            "build_rejects_targets_present_in_lifecycle_or_swap_topology",
        ],
        postgres: true,
        expected: "build target is idle, outside PC/CC, and has invalid progress",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-06",
        name: "build_is_cancelled_and_drained_before_remove",
        owner: "build",
        scenarios: &["exact_build_is_cancelled_and_drained_before_removal_and_successor"],
        postgres: true,
        expected: "build root and descendants drain before removal",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-07",
        name: "swap_uses_captured_specific_quorum_capability",
        owner: "swap",
        scenarios: &[
            "public_v1_swap_captures_specific_quorum_capability",
            "public_operation_replay_swap_install_cuts_never_wait_on_stale_topology",
        ],
        postgres: false,
        expected: "specific quorum and fallback are captured replay inputs",
        eligibility: contract_gate,
        future: FutureObligation::Phase6Engine,
    },
    Gate {
        id: "KSF-08",
        name: "progress_is_role_correct_and_history_scoped",
        owner: "progress",
        scenarios: &[
            "public_v1_history_partition_seed_is_shared",
            "public_v1_history_admission_gates_all_data_loss_outcomes",
            "public_v1_schema11_build_ambiguity_requires_retirement",
        ],
        postgres: true,
        expected: "numeric progress is comparable only inside one history",
        eligibility: contract_gate,
        future: FutureObligation::Phase6Engine,
    },
    Gate {
        id: "KSF-09",
        name: "replacement_bundle_has_no_managed_capability",
        owner: "construction",
        scenarios: &["public_v1_source_uses_no_native_evidence"],
        postgres: false,
        expected: "current legacy-bearing artifact is production ineligible",
        eligibility: ksf_09,
        future: FutureObligation::Phase7Cutover,
    },
    Gate {
        id: "KSF-10",
        name: "replicator_open_owns_endpoint_lifetime",
        owner: "transport",
        scenarios: &["bound_agent_listeners_remain_owned_until_shutdown"],
        postgres: true,
        expected: "public endpoint lifetime follows Replicator lifecycle",
        eligibility: ksf_10,
        future: FutureObligation::Phase7Cutover,
    },
    Gate {
        id: "KSF-11",
        name: "role_address_is_published_and_cleared",
        owner: "reporting",
        scenarios: &["public_operation_role_address_runtime_to_controller_publish_replace_clear"],
        postgres: true,
        expected: "exact role completion publishes and clears address",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-12",
        name: "supersession_cancels_and_drains_public_callbacks",
        owner: "operation",
        scenarios: &[
            "every_non_build_public_callback_is_independently_cancellable",
            "callback_cancellation_cross_product_has_exact_root_end_and_descendant_drain",
        ],
        postgres: true,
        expected: "supersession contains callback roots and descendants",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-13",
        name: "transient_fault_restarts_or_drops_exact_incarnation",
        owner: "fault",
        scenarios: &["public_fault_reporting_closes_access_and_location_before_returning"],
        postgres: false,
        expected: "fault closes access and targets one exact incarnation",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-14",
        name: "contained_close_failure_completes_outer_close",
        owner: "close",
        scenarios: &["contained_close_failures_preserve_typed_diagnostics_and_order"],
        postgres: true,
        expected: "child diagnostics do not change normalized outer close",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-15",
        name: "callback_replay_follows_declared_disposition",
        owner: "replay",
        scenarios: &[
            "public_operation_replay_matrix_converges_each_repeatable_program_family",
            "public_v1_replay_preserves_declared_disposition",
        ],
        postgres: true,
        expected: "journal replay preserves ambiguity and capability policy",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-16",
        name: "cutover_artifact_contains_no_legacy_continuation_owner",
        owner: "cutover",
        scenarios: &[],
        postgres: false,
        expected: "current artifact inventories and rejects legacy continuation owners",
        eligibility: ksf_16,
        future: FutureObligation::Phase7Cutover,
    },
];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("runtime crate is inside the workspace")
        .to_path_buf()
}

fn scenario_filter() -> String {
    GATES
        .iter()
        .filter(|gate| gate.id != "KSF-16")
        .flat_map(|gate| gate.scenarios.iter())
        .map(|scenario| format!("test({scenario})"))
        .collect::<Vec<_>>()
        .join(" or ")
}

fn run_strict_scenarios() {
    let root = workspace_root();
    let filter = scenario_filter();
    let list = Command::new("cargo")
        .current_dir(&root)
        .args([
            "nextest",
            "list",
            "-p",
            "kuberic-runtime",
            "--all-features",
            "--profile",
            "ordinary",
            "-E",
            &filter,
        ])
        .output()
        .expect("list strict Public V1 conformance scenarios");
    assert!(
        list.status.success(),
        "strict Public V1 conformance inventory failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let inventory = String::from_utf8(list.stdout).expect("nextest inventory is UTF-8");
    for gate in GATES.iter().filter(|gate| gate.id != "KSF-16") {
        for scenario in gate.scenarios {
            assert!(
                inventory.lines().any(|line| line.ends_with(scenario)),
                "{} scenario is not selected: {}",
                gate.id,
                scenario
            );
        }
    }

    let run = Command::new("cargo")
        .current_dir(root)
        .args([
            "nextest",
            "run",
            "-j",
            "1",
            "-p",
            "kuberic-runtime",
            "--all-features",
            "--profile",
            "ordinary",
            "-E",
            &filter,
        ])
        .output()
        .expect("run strict Public V1 conformance scenarios");
    assert!(
        run.status.success(),
        "strict Public V1 conformance scenarios failed:\n{}\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}

pub fn assert_public_v1_conformance_subject(subject: &str) {
    assert!(
        matches!(subject, "strict-custom" | "postgresql"),
        "unknown Public V1 conformance subject: {subject}"
    );
    if subject == "strict-custom" {
        run_strict_scenarios();
    }

    for (index, gate) in GATES.iter().enumerate() {
        assert_eq!(gate.id, format!("KSF-{:02}", index + 1));
        assert!(!gate.name.is_empty(), "{} has no stable gate name", gate.id);
        assert!(!gate.owner.is_empty(), "{} has no component owner", gate.id);
        assert!(
            !gate.expected.is_empty(),
            "{} has no expected public result",
            gate.id
        );
        (gate.eligibility)();
        if subject == "postgresql" && gate.postgres {
            assert!(
                gate.future != FutureObligation::Phase7Cutover || gate.id == "KSF-10",
                "{} has invalid PostgreSQL applicability",
                gate.id
            );
        }
    }

    for id in ["KSF-09", "KSF-10", "KSF-16"] {
        let gate = GATES.iter().find(|gate| gate.id == id).unwrap();
        assert_eq!(
            gate.future,
            FutureObligation::Phase7Cutover,
            "{id} must remain an executable negative cutover guard"
        );
    }
    assert!(
        GATES
            .iter()
            .any(|gate| gate.future == FutureObligation::Phase6Engine),
        "the oracle must retain Phase 6 engine obligations"
    );
}
