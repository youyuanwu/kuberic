use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const CONFORMANCE_TIMEOUT: Duration = Duration::from_secs(270);

#[derive(Debug)]
struct BoundedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn bounded_output(
    command: &mut Command,
    deadline: Instant,
    description: &str,
) -> Result<BoundedOutput, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("{description} failed to start: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{description} did not capture stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("{description} did not capture stderr"))?;
    let stdout = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stdout = stdout;
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stderr = stderr;
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });

    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("{description} status failed: {error}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            terminate_process_tree(&mut child);
            let status = child
                .wait()
                .map_err(|error| format!("{description} reap failed: {error}"))?;
            let stdout = stdout
                .join()
                .map_err(|_| format!("{description} stdout reader panicked"))?
                .map_err(|error| format!("{description} stdout read failed: {error}"))?;
            let stderr = stderr
                .join()
                .map_err(|_| format!("{description} stderr reader panicked"))?
                .map_err(|error| format!("{description} stderr read failed: {error}"))?;
            return Err(format!(
                "{description} exceeded the shared parent deadline \
                 (terminated with {status}):\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout
        .join()
        .map_err(|_| format!("{description} stdout reader panicked"))?
        .map_err(|error| format!("{description} stdout read failed: {error}"))?;
    let stderr = stderr
        .join()
        .map_err(|_| format!("{description} stderr reader panicked"))?
        .map_err(|error| format!("{description} stderr read failed: {error}"))?;
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
    })
}

fn terminate_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        const SIGTERM: i32 = 15;
        const SIGKILL: i32 = 9;
        let process_group = -(child.id() as i32);
        unsafe {
            kill(process_group, SIGTERM);
        }
        let grace = Instant::now() + Duration::from_millis(100);
        while Instant::now() < grace {
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        unsafe {
            kill(process_group, SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

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
    strict_scenarios: &'static [&'static str],
    postgres_scenarios: &'static [&'static str],
    preview_scenarios: &'static [&'static str],
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
        strict_scenarios: &[
            "strict_trace_fixture_covers_the_public_callback_inventory_and_primary_guard",
        ],
        postgres_scenarios: &["public_v1_postgres_primary_lifecycle_and_joint_configuration"],
        preview_scenarios: &["static_preview_is_store_free_access_gated_and_durable_before_ack"],
        expected: "primary role completes before primary configuration",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-02",
        name: "authorized_data_loss_false_true_error_ambiguity",
        owner: "recovery",
        strict_scenarios: &[
            "public_v1_history_admission_gates_all_data_loss_outcomes",
            "public_v1_data_loss_replay_uses_attached_capability",
        ],
        postgres_scenarios: &[],
        preview_scenarios: &["hosted_preview_authorized_data_loss_maps_only_the_effective_outcome"],
        expected: "logical authorization distinguishes every callback outcome",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-03",
        name: "same_role_secondary_receives_new_epoch",
        owner: "lifecycle",
        strict_scenarios: &["public_v1_session_renewal_requires_higher_epoch"],
        postgres_scenarios: &["public_v1_postgres_primary_lifecycle_and_joint_configuration"],
        preview_scenarios: &[
            "hosted_preview_switchover_dispatches_exact_intent_and_accepts_matching_completion",
        ],
        expected: "same-role epoch renewal fences predecessor traffic",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-04",
        name: "configuration_contains_only_remote_active_members",
        owner: "configuration",
        strict_scenarios: &[
            "public_v1_values_distinguishes_invalid_lsn",
            "public_v1_swap_captures_specific_quorum_capability",
            "public_v1_role_and_epoch_transitions_persist_current_authority",
        ],
        postgres_scenarios: &["public_v1_postgres_primary_lifecycle_and_joint_configuration"],
        preview_scenarios: &[
            "hosted_preview_switchover_dispatches_exact_intent_and_accepts_matching_completion",
        ],
        expected: "configuration contains only exact ready remote peers",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-05",
        name: "build_uses_invalid_target_progress_and_exact_attempt",
        owner: "build",
        strict_scenarios: &[
            "public_v1_values_distinguishes_invalid_lsn",
            "build_rejects_targets_present_in_lifecycle_or_swap_topology",
        ],
        postgres_scenarios: &["public_v1_postgres_build_cancellation_and_retirement"],
        preview_scenarios: &[
            "hosted_preview_build_dispatches_exact_attempt_and_waits_for_matching_completion",
        ],
        expected: "build target is idle, outside PC/CC, and has invalid progress",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-06",
        name: "build_is_cancelled_and_drained_before_remove",
        owner: "build",
        strict_scenarios: &["exact_build_is_cancelled_and_drained_before_removal_and_successor"],
        postgres_scenarios: &["public_v1_postgres_build_cancellation_and_retirement"],
        preview_scenarios: &[
            "exact_build_is_cancelled_and_drained_before_removal_and_successor",
            "hosted_preview_build_dispatches_exact_attempt_and_waits_for_matching_completion",
            "hosted_preview_removal_drains_build_excludes_peer_and_rejects_stale_session",
        ],
        expected: "build root and descendants drain before removal",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-07",
        name: "swap_uses_captured_specific_quorum_capability",
        owner: "swap",
        strict_scenarios: &[
            "public_v1_swap_captures_specific_quorum_capability",
            "public_operation_replay_swap_install_cuts_never_wait_on_stale_topology",
        ],
        postgres_scenarios: &[],
        preview_scenarios: &[
            "hosted_preview_switchover_dispatches_exact_intent_and_accepts_matching_completion",
        ],
        expected: "specific quorum and fallback are captured replay inputs",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-08",
        name: "progress_is_role_correct_and_history_scoped",
        owner: "progress",
        strict_scenarios: &[
            "public_v1_history_partition_seed_is_shared",
            "public_v1_history_admission_gates_all_data_loss_outcomes",
            "public_v1_schema11_build_ambiguity_requires_retirement",
        ],
        postgres_scenarios: &["public_v1_postgres_data_loss_history_and_replay"],
        preview_scenarios: &[
            "public_v1_history_partition_seed_is_shared",
            "ambiguous_suffix_exactly_replays_quorum_peers_despite_higher_advertised_progress",
            "hosted_preview_switchover_dispatches_exact_intent_and_accepts_matching_completion",
        ],
        expected: "numeric progress is comparable only inside one history",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-09",
        name: "replacement_bundle_has_no_managed_capability",
        owner: "construction",
        strict_scenarios: &["public_v1_source_uses_no_native_evidence"],
        postgres_scenarios: &[],
        preview_scenarios: &[
            "production_config_has_no_preview_selector_and_stateless_source_has_no_legacy_store_or_auth",
        ],
        expected: "current legacy-bearing artifact is production ineligible",
        eligibility: ksf_09,
        future: FutureObligation::Phase7Cutover,
    },
    Gate {
        id: "KSF-10",
        name: "replicator_open_owns_endpoint_lifetime",
        owner: "transport",
        strict_scenarios: &["public_lifecycle_owns_replicator_endpoint_until_close_and_abort"],
        postgres_scenarios: &[],
        preview_scenarios: &["preview_open_owns_registration_listener_and_writer_exactly_once"],
        expected: "public endpoint lifetime follows Replicator lifecycle",
        eligibility: ksf_10,
        future: FutureObligation::Phase7Cutover,
    },
    Gate {
        id: "KSF-11",
        name: "role_address_is_published_and_cleared",
        owner: "reporting",
        strict_scenarios: &["public_v1_role_address_runtime_to_controller_publish_replace_clear"],
        postgres_scenarios: &[],
        preview_scenarios: &["public_v1_role_address_runtime_to_controller_publish_replace_clear"],
        expected: "exact role completion publishes and clears address",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-12",
        name: "supersession_cancels_and_drains_public_callbacks",
        owner: "operation",
        strict_scenarios: &[
            "every_non_build_public_callback_is_independently_cancellable",
            "callback_cancellation_cross_product_has_exact_root_end_and_descendant_drain",
        ],
        postgres_scenarios: &["public_v1_postgres_build_cancellation_and_retirement"],
        preview_scenarios: &[
            "hosted_preview_supersession_cancels_build_root_and_drains_descendant_before_close",
        ],
        expected: "supersession contains callback roots and descendants",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-13",
        name: "transient_fault_restarts_or_drops_exact_incarnation",
        owner: "fault",
        strict_scenarios: &["public_fault_reporting_closes_access_and_location_before_returning"],
        postgres_scenarios: &[],
        preview_scenarios: &[
            "public_fault_reporting_closes_access_and_location_before_returning",
            "hosted_preview_unreachable_survivor_blocks_access_until_return_or_retirement",
            "hosted_preview_rebuild_retires_and_restarts_fresh_replacement",
            "hosted_preview_switchover_dispatches_exact_intent_and_accepts_matching_completion",
        ],
        expected: "fault closes access and targets one exact incarnation",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-14",
        name: "contained_close_failure_completes_outer_close",
        owner: "close",
        strict_scenarios: &["contained_close_failures_preserve_typed_diagnostics_and_order"],
        postgres_scenarios: &["public_v1_postgres_contained_close"],
        preview_scenarios: &["hosted_preview_contained_close_retains_typed_child_diagnostic"],
        expected: "child diagnostics do not change normalized outer close",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-15",
        name: "callback_replay_follows_declared_disposition",
        owner: "replay",
        strict_scenarios: &[
            "public_operation_replay_matrix_converges_each_repeatable_program_family",
            "public_v1_replay_preserves_declared_disposition",
        ],
        postgres_scenarios: &[],
        preview_scenarios: &[
            "durable_build_application_never_reinvokes_the_build_callback",
            "public_operation_replay_matrix_converges_each_repeatable_program_family",
            "public_v1_history_admission_gates_all_data_loss_outcomes",
            "public_v1_replay_preserves_declared_disposition",
            "public_v1_schema11_build_ambiguity_requires_retirement",
        ],
        expected: "journal replay preserves ambiguity and capability policy",
        eligibility: contract_gate,
        future: FutureObligation::None,
    },
    Gate {
        id: "KSF-16",
        name: "cutover_artifact_contains_no_legacy_continuation_owner",
        owner: "cutover",
        strict_scenarios: &[],
        postgres_scenarios: &[],
        preview_scenarios: &[],
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

fn scenario_filter(scenarios: fn(&Gate) -> &'static [&'static str]) -> String {
    GATES
        .iter()
        .filter(|gate| gate.id != "KSF-16")
        .flat_map(|gate| scenarios(gate).iter())
        .map(|scenario| format!("test({scenario})"))
        .collect::<Vec<_>>()
        .join(" or ")
}

fn run_scenarios(
    subject: &str,
    packages: &[&str],
    scenarios: fn(&Gate) -> &'static [&'static str],
    deadline: Instant,
) {
    let root = workspace_root();
    let filter = scenario_filter(scenarios);
    let archive = std::env::var_os("NEXTEST_ARCHIVE")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        })
        .filter(|path| path.is_file());
    let mut list = Command::new("cargo");
    list.current_dir(&root).args(["nextest", "list"]);
    if let Some(archive) = &archive {
        list.arg("--archive-file").arg(archive);
    } else {
        for package in packages {
            list.args(["-p", package]);
        }
        list.arg("--all-features");
    }
    list.args(["--profile", "ordinary", "-E", &filter]);
    let list = bounded_output(&mut list, deadline, "list Public V1 conformance scenarios")
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        list.status.success(),
        "strict Public V1 conformance inventory failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let inventory = String::from_utf8(list.stdout).expect("nextest inventory is UTF-8");
    for gate in GATES.iter().filter(|gate| gate.id != "KSF-16") {
        for scenario in scenarios(gate) {
            assert!(
                inventory.contains(scenario),
                "{} scenario is not selected: {}",
                gate.id,
                scenario
            );
        }
    }

    let mut run = Command::new("cargo");
    run.current_dir(&root).args(["nextest", "run", "-j", "1"]);
    let extract = archive.as_ref().map(|archive| {
        let path = root.join("target").join(format!(
            "public-v1-conformance-extract-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create Public V1 conformance extraction directory");
        run.arg("--archive-file")
            .arg(archive)
            .arg("--extract-to")
            .arg(&path)
            .arg("--workspace-remap")
            .arg(&root);
        path
    });
    if archive.is_none() {
        for package in packages {
            run.args(["-p", package]);
        }
        run.arg("--all-features");
    }
    run.args(["--profile", "ordinary", "-E", &filter]);
    let result = bounded_output(
        &mut run,
        deadline,
        &format!("run {subject} Public V1 conformance scenarios"),
    );
    if let Some(extract) = extract {
        let _ = std::fs::remove_dir_all(extract);
    }
    let run = result.unwrap_or_else(|error| panic!("{error}"));
    assert!(
        run.status.success(),
        "{subject} Public V1 conformance scenarios failed:\n{}\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}

#[cfg(all(test, unix))]
mod bounded_process_tests {
    use super::*;

    fn process_exists(pid: i32) -> bool {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        unsafe { kill(pid, 0) == 0 }
    }

    #[test]
    fn shared_deadline_does_not_reset_and_reaps_nested_process_tree() {
        let root = workspace_root()
            .join("target")
            .join(format!("conformance-timeout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let pid_file = root.join("descendant.pid");
        let deadline = Instant::now() + Duration::from_millis(400);
        let started = Instant::now();
        let mut first = Command::new("sh");
        first.args(["-c", "sleep 0.25"]);
        let first = bounded_output(&mut first, deadline, "first shared-deadline subject").unwrap();
        assert!(first.status.success());

        let mut command = Command::new("sh");
        command.arg("-c").arg(format!(
            "sleep 30 & child=$!; printf '%s' \"$child\" > '{}'; wait",
            pid_file.display()
        ));
        let error =
            bounded_output(&mut command, deadline, "second shared-deadline subject").unwrap_err();
        assert!(error.contains("exceeded"));
        assert!(
            started.elapsed() < Duration::from_millis(550),
            "the second subject reset the shared deadline"
        );
        let descendant: i32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        let reaped = (0..100).any(|_| {
            if !process_exists(descendant) {
                true
            } else {
                std::thread::sleep(Duration::from_millis(10));
                false
            }
        });
        assert!(reaped, "descendant process {descendant} survived timeout");
        std::fs::remove_dir_all(root).unwrap();
    }
}

pub fn assert_public_v1_conformance_subject(subject: &str) {
    assert!(
        matches!(
            subject,
            "strict-custom" | "postgresql" | "built-in-preview" | "strict-custom+built-in-preview"
        ),
        "unknown Public V1 conformance subject: {subject}"
    );
    let deadline = Instant::now() + CONFORMANCE_TIMEOUT;
    let subjects = if subject == "strict-custom+built-in-preview" {
        &["strict-custom", "built-in-preview"][..]
    } else {
        std::slice::from_ref(&subject)
    };
    for selected in subjects {
        if *selected == "strict-custom" {
            run_scenarios(
                "strict-custom",
                &["kuberic-runtime"],
                |gate| gate.strict_scenarios,
                deadline,
            );
        } else if *selected == "built-in-preview" {
            run_scenarios(
                "built-in-preview",
                &["kuberic-runtime", "kuberic-controller"],
                |gate| gate.preview_scenarios,
                deadline,
            );
        }
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
        if subject == "postgresql" && !gate.postgres_scenarios.is_empty() {
            assert!(
                gate.future != FutureObligation::Phase7Cutover,
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
        !GATES
            .iter()
            .any(|gate| gate.future == FutureObligation::Phase6Engine)
    );
}

pub fn assert_public_v1_conformance_gate(subject: &str, id: &str, scenario: &str) {
    assert!(
        matches!(subject, "strict-custom" | "postgresql" | "built-in-preview"),
        "unknown Public V1 conformance subject: {subject}"
    );
    let gate = GATES
        .iter()
        .find(|gate| gate.id == id)
        .unwrap_or_else(|| panic!("unknown Public V1 conformance gate: {id}"));
    let scenarios = match subject {
        "postgresql" => gate.postgres_scenarios,
        "built-in-preview" => gate.preview_scenarios,
        _ => gate.strict_scenarios,
    };
    assert!(
        scenarios.contains(&scenario),
        "{subject} scenario {scenario} is not registered for {id}"
    );
    (gate.eligibility)();
}

pub fn assert_public_v1_conformance_not_applicable(subject: &str, id: &str) {
    assert!(
        matches!(subject, "strict-custom" | "postgresql" | "built-in-preview"),
        "unknown Public V1 conformance subject: {subject}"
    );
    let gate = GATES
        .iter()
        .find(|gate| gate.id == id)
        .unwrap_or_else(|| panic!("unknown Public V1 conformance gate: {id}"));
    let scenarios = match subject {
        "postgresql" => gate.postgres_scenarios,
        "built-in-preview" => gate.preview_scenarios,
        _ => gate.strict_scenarios,
    };
    assert!(
        scenarios.is_empty(),
        "{subject} unexpectedly claims executable applicability for {id}"
    );
    (gate.eligibility)();
}

pub fn public_v1_postgres_conformance_scenarios() -> Vec<&'static str> {
    let mut scenarios = GATES
        .iter()
        .flat_map(|gate| gate.postgres_scenarios.iter().copied())
        .collect::<Vec<_>>();
    scenarios.sort_unstable();
    scenarios.dedup();
    scenarios
}
