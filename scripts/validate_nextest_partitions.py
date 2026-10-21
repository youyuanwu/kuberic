#!/usr/bin/env python3
"""Validate repository-wide nextest tier and partition coverage."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tempfile
import tomllib
from copy import deepcopy
from dataclasses import dataclass
from pathlib import Path


KIND_PACKAGE = "kuberic-level-tests"
KIND_PREFIX = "level_triggered_k8s::"
EXPECTED_KIND_LIVE_COUNT = 3
POSTGRES_PACKAGE = "postgres-replicated"
POSTGRES_LIBRARY_BINARY = "postgres-replicated"
POSTGRES_MAIN_BINARY = "postgres-replicated::bin/postgres-replicated"


@dataclass(frozen=True, order=True)
class TestId:
    package: str
    binary: str
    name: str


@dataclass(frozen=True)
class ListedTest:
    test_id: TestId
    ignored: bool
    matches: bool


# These source-only cases change validation granularity for actual native/service
# journeys; they retain the same bounds and every registered behavioral selector.
CONFORMANCE_EXECUTION_TESTS = {
    TestId("kuberic-runtime", "kuberic-runtime::public_v1_conformance", name)
    for name in [
        "public_v1_conformance_executes_built_in_replacement_scenarios",
        "public_v1_conformance_executes_service_failover_after_durable_promotion",
        "public_v1_conformance_executes_service_failover_at_persisted_promotion",
        "public_v1_conformance_executes_service_ambiguous_commit_abort",
        "public_v1_conformance_executes_service_switchover_after_catch_up",
        "public_v1_conformance_executes_service_switchover_after_durable_handoff",
    ]
}

# Deadline/pipe ownership is source infrastructure, not ordinary native behavior.
CONFORMANCE_HELPER_TESTS = {
    TestId(
        "kuberic-runtime",
        "kuberic-runtime",
        f"testing::public_v1_conformance::bounded_process_tests::{name}",
    )
    for name in [
        "nested_commands_reuse_workspace_features_or_the_exact_archive",
        "partition_budget_reserves_cleanup_and_outer_owner_margin",
        "exhausted_shared_budget_never_spawns_a_later_command",
        "direct_child_exit_with_open_descendant_pipes_is_deadline_bounded",
        "undrained_pipe_is_an_explicit_bounded_failure",
        "shared_deadline_does_not_reset_and_reaps_nested_process_tree",
    ]
}


SOURCE_TESTS = {
    TestId(
        "kuberic-runtime",
        "kuberic-runtime::public_api_inventory",
        name,
    )
    for name in [
        "application_replication_api_has_no_parallel_driver_or_admission_surface",
        "source_public_api_matches_reviewed_inventory",
        "host_capabilities_are_not_source_public_even_when_doc_hidden",
        "control_inventory_tracks_ownership_not_referenced_types",
        "retired_replication_data_protocol_is_not_source_public",
    ]
} | {
    TestId(
        "kuberic-runtime",
        "kuberic-runtime::lifecycle_capability_boundaries",
        name,
    )
    for name in [
        "protected_public_trait_method_sets_are_unchanged",
        "replacement_runtime_has_no_managed_default_or_agent_replication_owner",
        "public_operation_owners_are_ordinary_modules",
        "native_production_owners_remain_private",
    ]
} | {
    TestId(
        "kuberic-runtime",
        "kuberic-runtime::public_v1_conformance",
        name,
    )
    for name in [
        "public_v1_conformance_oracle_has_exactly_ksf_01_through_ksf_16",
        "public_v1_conformance_partitions_cover_every_registered_observation_once",
        "built_in_replacement_ksf_mappings_require_each_authorized_observation",
    ]
} | {
    TestId(
        "kuberic-controller",
        "kuberic-controller::public_api",
        name,
    )
    for name in [
        "crdgen_preserves_public_v1_only_typed_status",
        "deployment_sources_installation_namespace_from_metadata_namespace_only",
        "deployment_prevents_concurrent_controller_reconciliation",
        "production_inventory_contains_only_public_v1_controller_surfaces",
    ]
} | {
    TestId(
        "kvstore2",
        "kvstore2::bin/kvstore2",
        name,
    )
    for name in [
        "tests::application_main_uses_the_agent_host_instead_of_protocol_internals",
        "tests::default_manifest_neither_enables_nor_routes_live_test_gate",
    ]
} | {
    TestId(
        "kuberic-runtime",
        "kuberic-runtime::public_api_privacy",
        "external_applications_can_use_contracts_but_cannot_forge_host_capabilities",
    ),
    TestId(
        "kuberic-runtime",
        "kuberic-runtime::public_v1_evidence",
        "public_v1_source_uses_no_native_evidence",
    ),
    TestId(
        "kuberic-runtime",
        "kuberic-runtime::public_v1_recovery",
        "production_config_has_no_protocol_selector_or_application_commit_oracle",
    ),
    TestId(
        "kuberic-controller",
        "kuberic-controller",
        "cluster_api::tests::production_kubernetes_api_requires_one_exact_namespace",
    ),
    TestId(
        "sqlite-replicated",
        "sqlite-replicated::bin/sqlite-replicated",
        "tests::startup_uses_replica_host_and_has_no_demo_surface",
    ),
    TestId(
        "kuberic-level-tests",
        "kuberic-level-tests::source_checks",
        "packaged_controller_test_util_boundary",
    ),
} | CONFORMANCE_EXECUTION_TESTS | CONFORMANCE_HELPER_TESTS

IGNORED_SOURCE_TESTS = {
    TestId(
        "kuberic-runtime",
        "kuberic-runtime::public_api_privacy",
        "external_applications_can_use_contracts_but_cannot_forge_host_capabilities",
    ),
    TestId(
        "kuberic-level-tests",
        "kuberic-level-tests::source_checks",
        "packaged_controller_test_util_boundary",
    ),
} | CONFORMANCE_EXECUTION_TESTS


def postgres_test(name: str, binary: str = POSTGRES_LIBRARY_BINARY) -> TestId:
    return TestId(POSTGRES_PACKAGE, binary, name)


POSTGRES_CATALOG = {
    "PG-R01": postgres_test("tests::public_v1_postgres_primary_lifecycle_and_joint_configuration"),
    "PG-R02": postgres_test("tests::public_v1_postgres_build_cancellation_and_retirement"),
    "PG-R03": postgres_test("tests::public_v1_postgres_data_loss_history_and_replay"),
    "PG-R04": postgres_test("tests::public_v1_postgres_contained_close"),
    "PG-R05": postgres_test("tests::public_v1_postgres_selected_resources"),
    "PG-R06": postgres_test("tests::public_v1_postgres_retained_abort"),
    "PG-R07": postgres_test("tests::public_v1_postgres_partition_authority"),
    "PG-R08": postgres_test("tests::public_v1_postgres_orderly_progress"),
    "PG-R09": postgres_test("tests::public_v1_postgres_marked_catchup"),
    "PG-R10": postgres_test("tests::public_v1_postgres_secondary_readiness"),
    "PG-N01": postgres_test("tests::public_v1_postgres_basebackup_cancellation_and_retry"),
    "PG-N02": postgres_test("tests::public_v1_postgres_stale_completion_cannot_mutate_successor"),
    "PG-N03": postgres_test("tests::public_v1_postgres_production_factory_and_fault_boundary"),
    "PG-N04": postgres_test("tests::public_v1_postgres_failover_switchover_and_replacement"),
    "PG-L01": postgres_test("owned_process::tests::cancellation_racing_start_still_reaps_the_launched_tree"),
    "PG-L02": postgres_test("owned_process::tests::successful_supervisor_exit_cannot_hide_an_unreaped_descendant"),
    "PG-L03": postgres_test("owned_process::tests::exited_pidfd_never_signals_a_replacement_numeric_pid"),
    "PG-L04": postgres_test("owned_process::tests::unresponsive_owned_child_is_killed_and_reaped_without_pid_file"),
    "PG-L05": postgres_test("access::tests::cancelled_hba_grant_cannot_overwrite_successor_closed_generation"),
    "PG-L06": postgres_test("access::tests::cancelled_grant_remains_fence_required"),
    "PG-L07": postgres_test("adapter::build_deletion_selected_tests::pgdata_deletion_supersession_reaps_before_successor_configuration"),
    "PG-L08": postgres_test("adapter::build_deletion_selected_tests::pgdata_deletion_timeout_reaps_before_same_build_retry"),
    "PG-L09": postgres_test("adapter::build_deletion_selected_tests::pgdata_deletion_io_failure_is_reported_and_partial_copy_retried"),
    "PG-L10": postgres_test("adapter::build_deletion_selected_tests::pgdata_deletion_release_executes_removals_without_following_symlinks"),
    "PG-L11": postgres_test("generation::tests::process_clock_never_reuses_ownership_after_reopen_or_exhaustion"),
    "PG-L12": postgres_test("generation::tests::initialized_process_clock_is_not_recreated_after_loss_or_corruption"),
    "PG-L13": postgres_test("durable::tests::exhausted_policy_and_metadata_generations_remain_fenced_after_reopen"),
    "PG-L14": postgres_test("durable::tests::durable_validation_rejects_version_progress_and_fence_mismatches"),
    "PG-L15": postgres_test("adapter::public_v1::tests::production_factory_returns_no_parallel_state_replicator"),
    "PG-L16": postgres_test("instance::tests::granted_primary_role_mismatch_reports_permanent_before_returning_error"),
    "PG-L17": postgres_test("instance::tests::retired_role_mismatch_cannot_fault_successor"),
}

POSTGRES_AUXILIARY_TESTS = {
    postgres_test("adapter::public_transport::tests::peer_gateway_accepts_only_the_authorized_physical_slot"),
    postgres_test("adapter::public_v1::tests::public_v1_process_fence_rejects_same_epoch_session_and_address_substitution"),
    postgres_test("adapter::public_v1::tests::public_v1_secondary_admits_joint_then_current_only_authority"),
    postgres_test("adapter::recovery::connection_validation_tests::malformed_streaming_connection_is_rejected_with_or_without_follow_receipt"),
    postgres_test("adapter::recovery::tests::exact_policy_quorum_requires_strict_intersection_and_valid_configuration_generation"),
    postgres_test("adapter::recovery::tests::final_round_rejects_stale_sessions_policy_lineage_and_receiver_state"),
    postgres_test("adapter::recovery::tests::legacy_running_receipts_keep_their_serialized_shape_and_stopped_acks_are_explicit"),
    postgres_test("monitor::tests::parses_native_lsn_without_truncation"),
    postgres_test("native::tests::application_names_are_bounded_and_session_unique"),
    postgres_test("native::tests::zero_secondary_policy_is_identity_and_joint_policy_is_conservative"),
    postgres_test("owned_command::tests::helper_identity_and_cleanup_epoch_exhaustion_remain_closed"),
    postgres_test("owned_command::tests::helper_output_preserves_streams_status_and_environment"),
    postgres_test("owned_command::tests::helper_pipes_are_drained_concurrently"),
    postgres_test("owned_command::tests::helper_signal_is_not_a_supervisor_cleanup_failure"),
    postgres_test("tests::shutdown_never_converts_acknowledgement_or_cleanup_failure_to_success", POSTGRES_MAIN_BINARY),
    postgres_test("tests::shutdown_preserves_acknowledgement_failure_with_cleanup_context", POSTGRES_MAIN_BINARY),
}


def postgres_smoke_tests() -> set[TestId]:
    return {
        test_id
        for catalog_id, test_id in POSTGRES_CATALOG.items()
        if catalog_id.startswith(("PG-R", "PG-N"))
    }


def postgres_smoke_filter() -> str:
    return " or ".join(
        f"test(={test_id.name})" for test_id in sorted(postgres_smoke_tests())
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--archive-file", type=Path)
    parser.add_argument("--ordinary-partitions", type=int, default=4)
    parser.add_argument("--postgres-partitions", type=int, default=4)
    parser.add_argument("--postgres-smoke-filter", action="store_true")
    return parser.parse_args()


def nextest_json(root: Path, extracted: Path | None, *extra: str) -> dict:
    command = ["cargo", "nextest", "list"]
    if extracted is None:
        command.extend(["--workspace", "--all-features"])
    else:
        command.extend(
            [
                "--binaries-metadata",
                str(extracted / "target/nextest/binaries-metadata.json"),
                "--cargo-metadata",
                str(extracted / "target/nextest/cargo-metadata.json"),
                "--target-dir-remap",
                str(extracted / "target"),
                "--workspace-remap",
                str(root),
            ]
        )
    command.extend(extra)
    command.extend(["--message-format", "json"])
    result = subprocess.run(
        command,
        cwd=root,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    return json.loads(result.stdout)


def listed_tests(document: dict) -> list[ListedTest]:
    tests: list[ListedTest] = []
    for suite in document["rust-suites"].values():
        package = suite["package-name"]
        binary = suite["binary-id"]
        for name, testcase in suite["testcases"].items():
            tests.append(
                ListedTest(
                    TestId(package, binary, name),
                    bool(testcase["ignored"]),
                    testcase["filter-match"]["status"] == "matches",
                )
            )
    return tests


def classify(test: ListedTest) -> str | None:
    test_id = test.test_id
    if test_id in SOURCE_TESTS:
        return "source" if test.ignored == (test_id in IGNORED_SOURCE_TESTS) else None
    if test.ignored:
        if test_id.package == KIND_PACKAGE and test_id.name.startswith(KIND_PREFIX):
            return "kind-live"
        return None
    if test_id.package == "postgres-replicated":
        return "postgres"
    return "ordinary"


def matching_ids(document: dict, *, include_ignored: bool = False) -> set[TestId]:
    return {
        test.test_id
        for test in listed_tests(document)
        if test.matches and (include_ignored or not test.ignored)
    }


def validate_partitions(
    root: Path,
    extracted: Path | None,
    expected: set[TestId],
    profile: str,
    mode: str,
    count: int,
) -> list[int]:
    if count < 1:
        raise ValueError(f"{mode} partition count must be positive")
    partitions: list[set[TestId]] = []
    for index in range(1, count + 1):
        selected = matching_ids(
            nextest_json(
                root,
                extracted,
                "--profile",
                profile,
                "--partition",
                f"{mode}:{index}/{count}",
            )
        )
        if not selected:
            raise ValueError(f"{mode} partition {index}/{count} is empty")
        partitions.append(selected)

    union: set[TestId] = set()
    for index, partition in enumerate(partitions, start=1):
        overlap = union & partition
        if overlap:
            raise ValueError(
                f"partition {index}/{count} overlaps a previous partition: "
                f"{sorted(overlap)[0]}"
            )
        union.update(partition)
    if union != expected:
        raise ValueError(
            f"partition union mismatch: missing={len(expected - union)} "
            f"extra={len(union - expected)}"
        )
    return [len(partition) for partition in partitions]


def validate(args: argparse.Namespace, root: Path, extracted: Path | None) -> None:
    validate_kind_contract(root)
    validate_conformance_execution_contract(root)
    inventory = listed_tests(nextest_json(root, extracted))
    assignments: dict[str, set[TestId]] = {}
    unassigned: list[TestId] = []
    for test in inventory:
        tier = classify(test)
        if tier is None:
            unassigned.append(test.test_id)
        else:
            assignments.setdefault(tier, set()).add(test.test_id)

    if unassigned:
        rendered = "\n".join(f"  {item}" for item in sorted(unassigned))
        raise ValueError(f"unassigned tests in complete inventory:\n{rendered}")
    if len(set().union(*assignments.values())) != len(inventory):
        raise ValueError("test inventory contains duplicate identifiers")
    if assignments.get("source", set()) != SOURCE_TESTS:
        missing = SOURCE_TESTS - assignments.get("source", set())
        extra = assignments.get("source", set()) - SOURCE_TESTS
        raise ValueError(
            f"source catalog mismatch: missing={sorted(missing)} extra={sorted(extra)}"
        )
    if len(assignments.get("kind-live", set())) != EXPECTED_KIND_LIVE_COUNT:
        raise ValueError(
            f"KinD live selector expected {EXPECTED_KIND_LIVE_COUNT} tests, "
            f"found {len(assignments.get('kind-live', set()))}"
        )
    expected_postgres = set(POSTGRES_CATALOG.values()) | POSTGRES_AUXILIARY_TESTS
    if assignments.get("postgres", set()) != expected_postgres:
        missing = expected_postgres - assignments.get("postgres", set())
        extra = assignments.get("postgres", set()) - expected_postgres
        raise ValueError(
            f"PostgreSQL catalog mismatch: missing={sorted(missing)} extra={sorted(extra)}"
        )

    configured = {
        "source": matching_ids(
            nextest_json(
                root, extracted, "--profile", "source", "--run-ignored", "all"
            ),
            include_ignored=True,
        ),
        "ordinary": matching_ids(nextest_json(root, extracted, "--profile", "ordinary")),
        "postgres": matching_ids(nextest_json(root, extracted, "--profile", "postgres")),
        "kind-live": matching_ids(
            nextest_json(root, extracted, "--profile", "kind", "--run-ignored", "only"),
            include_ignored=True,
        ),
    }
    for tier, selected in configured.items():
        if selected != assignments.get(tier, set()):
            missing = assignments.get(tier, set()) - selected
            extra = selected - assignments.get(tier, set())
            raise ValueError(
                f"configured {tier} profile mismatch: "
                f"missing={sorted(missing)} extra={sorted(extra)}"
            )
    selected_once: set[TestId] = set()
    for tier, selected in configured.items():
        overlap = selected_once & selected
        if overlap:
            raise ValueError(f"configured {tier} profile overlaps: {sorted(overlap)}")
        selected_once.update(selected)
    if selected_once != {test.test_id for test in inventory}:
        raise ValueError("configured tiers do not cover the complete archive inventory")
    compatibility = matching_ids(
        nextest_json(
            root, extracted, "--profile", "build-only", "--run-ignored", "all"
        ),
        include_ignored=True,
    )
    if compatibility != SOURCE_TESTS:
        raise ValueError("build-only compatibility alias does not select all source checks")
    selected_smoke = matching_ids(
        nextest_json(
            root,
            extracted,
            "--profile",
            "postgres",
            "-E",
            postgres_smoke_filter(),
        )
    )
    if selected_smoke != postgres_smoke_tests():
        raise ValueError("PostgreSQL smoke filter does not exactly match PG-R plus PG-N")
    if selected_smoke & {
        test_id
        for catalog_id, test_id in POSTGRES_CATALOG.items()
        if catalog_id.startswith("PG-L")
    }:
        raise ValueError("PostgreSQL smoke filter includes a PG-L low-level test")

    ordinary_counts = validate_partitions(
        root,
        extracted,
        assignments["ordinary"],
        "ordinary",
        "slice",
        args.ordinary_partitions,
    )
    postgres_counts = validate_partitions(
        root,
        extracted,
        assignments["postgres"],
        "postgres",
        "hash",
        args.postgres_partitions,
    )

    print(f"inventory: {len(inventory)} tests")
    for tier in sorted(assignments):
        print(f"{tier}: {len(assignments[tier])}")
    print(f"ordinary slice counts: {ordinary_counts}")
    print(f"postgres hash counts: {postgres_counts}")
    print("exact-once tier and partition validation passed")


def validate_conformance_execution_cases(helper: str, tests: str) -> None:
    service_manifest = re.search(
        r"const SERVICE_PARTITIONS:[^=]+=\s*&\[(.*?)^\];", helper, re.S | re.M
    )
    if service_manifest is None:
        raise ValueError("conformance service partition manifest is missing")
    services = re.findall(
        r'\(\s*"([^"]+)"\s*,\s*"([^"]+)"\s*,?\s*\)', service_manifest.group(1)
    )
    ids = [partition for partition, _ in services]
    observations = [scenario for _, scenario in services]
    if not ids or len(set(ids)) != len(ids) or len(set(observations)) != len(observations):
        raise ValueError("conformance service partitions are empty or overlap")

    cases = re.findall(
        r"fn\s+(public_v1_conformance_executes_\w+)\s*\(\)\s*\{(.*?)^\}",
        tests,
        re.S | re.M,
    )
    expected = {test.name for test in CONFORMANCE_EXECUTION_TESTS}
    if {name for name, _ in cases} != expected or len(cases) != len(expected):
        raise ValueError("conformance execution cases differ from the source inventory")
    dispatched = []
    for name, body in cases:
        if name == "public_v1_conformance_executes_built_in_replacement_scenarios":
            if not re.fullmatch(
                r'\s*kuberic_runtime::testing::execute_public_v1_conformance_subject'
                r'\(\s*"built-in-replacement"\s*,?\s*\)\s*;\s*',
                body,
            ):
                raise ValueError("native/controller conformance dispatch changed")
            continue
        dispatch = re.fullmatch(
            r"\s*kuberic_runtime::testing::execute_public_v1_conformance_partition"
            r'\(\s*"built-in-replacement"\s*,\s*"([^"]+)"\s*,?\s*\)\s*;\s*',
            body,
        )
        if dispatch is None:
            raise ValueError(f"service conformance case has no exact dispatch: {name}")
        dispatched.append(dispatch.group(1))
    if set(dispatched) != set(ids) or len(dispatched) != len(ids):
        raise ValueError("source execution cases omit or repeat a service partition")


def validate_conformance_execution_contract(root: Path) -> None:
    helper = (root / "kuberic-runtime/src/testing/public_v1_conformance.rs").read_text()
    validate_conformance_execution_cases(
        helper,
        (root / "kuberic-runtime/tests/public_v1_conformance.rs").read_text(),
    )
    budgets = {
        "CONFORMANCE_OUTER_TIMEOUT": 270,
        "CONFORMANCE_TIMEOUT": 240,
        "CONFORMANCE_CLEANUP_TIMEOUT": 10,
        "CONFORMANCE_OWNER_MARGIN": 20,
    }
    for name, seconds in budgets.items():
        configured = re.search(
            rf"const {name}:\s*Duration\s*=\s*Duration::from_secs\((\d+)\);",
            helper,
        )
        if configured is None or int(configured.group(1)) != seconds:
            raise ValueError(f"conformance budget must retain {name}={seconds}s")
    if not re.search(
        r"pub fn execute_public_v1_conformance_partition\([^{}]*\)\s*\{"
        r"\s*let deadline = partition_deadline\(Instant::now\(\)\);",
        helper,
        re.S,
    ):
        raise ValueError("conformance budget must start before eligibility or partition checks")
    config = tomllib.loads((root / ".config/nextest.toml").read_text())
    if (
        config["profile"]["source"]["test-threads"] != 1
        or config["test-groups"]["process-boundaries"]["max-threads"] != 1
    ):
        raise ValueError("conformance source partitions must retain serial isolation")
    execution_filter = (
        "binary_id(=kuberic-runtime::public_v1_conformance) "
        "and test(/^public_v1_conformance_executes_/)"
    )
    helper_filter = (
        "binary_id(=kuberic-runtime) "
        "and test(/^testing::public_v1_conformance::bounded_process_tests::/)"
    )
    for required_filter in [execution_filter, helper_filter]:
        overrides = [
            item
            for item in config["profile"]["default"]["overrides"]
            if item["filter"] == required_filter
        ]
        if len(overrides) != 1 or overrides[0] != {
            "filter": required_filter,
            "slow-timeout": {"period": "270s", "terminate-after": 1},
            "test-group": "process-boundaries",
        }:
            raise ValueError("conformance partitions/helpers changed their process group or timeout")


def validate_kind_contract(root: Path) -> None:
    justfile = (root / "justfile").read_text()
    workflow = (root / ".github/workflows/CI.yml").read_text()
    scenarios = set(
        re.findall(r"^\s*(bootstrap|replacement|failover)\)\s+test_names=", justfile, re.M)
    )
    if scenarios != {"bootstrap", "replacement", "failover"}:
        raise ValueError(f"unexpected KinD scenario mapping: {sorted(scenarios)}")
    all_match = re.search(r"expanded\+=\(([^)]*)\)", justfile)
    if all_match is None or all_match.group(1).split() != [
        "bootstrap",
        "replacement",
        "failover",
    ]:
        raise ValueError("KinD all expansion is not bootstrap replacement failover")
    for arguments in re.findall(r"just level-triggered-kind-test ([^\n]+)", workflow):
        for scenario in arguments.split():
            if scenario not in scenarios | {"all"}:
                raise ValueError(f"CI invokes unsupported KinD scenario: {scenario}")
    config = tomllib.loads((root / ".config/nextest.toml").read_text())
    validate_timeout_profiles(config)
    mutated = deepcopy(config)
    mutated["profile"]["default"]["overrides"][0]["slow-timeout"] = {
        "period": "300s",
        "terminate-after": 1,
    }
    try:
        validate_timeout_profiles(mutated)
    except ValueError:
        pass
    else:
        raise ValueError("timeout validator accepted a 300-second override mutation")


def validate_timeout_profiles(config: dict) -> None:
    profiles = config.get("profile", {})

    def validate_timeout(label: str, timeout: dict, maximum: int = 270) -> None:
        period = timeout.get("period", "")
        match = re.fullmatch(r"(\d+)(s|m)", period)
        if match is None:
            raise ValueError(f"{label} has an unsupported timeout: {period}")
        seconds = int(match.group(1)) * (60 if match.group(2) == "m" else 1)
        if seconds <= 0 or seconds > maximum or timeout.get("terminate-after") != 1:
            raise ValueError(f"{label} must terminate once within {maximum} seconds")

    tooling_filters = {
        "binary_id(=kuberic-level-tests::source_checks) and test(=packaged_controller_test_util_boundary)",
        "binary_id(=kuberic-runtime::public_api_privacy)",
    }
    for name in ["default", "ci", "ordinary", "source", "build-only", "postgres", "kind"]:
        current = profiles.get(name, {})
        seen: set[str] = set()
        while "slow-timeout" not in current:
            inherited = current.get("inherits")
            if not inherited or inherited in seen:
                raise ValueError(f"profile {name} has no terminating timeout")
            seen.add(inherited)
            current = profiles.get(inherited, {})
        validate_timeout(f"profile {name} timeout", current["slow-timeout"])
        for index, override in enumerate(profiles.get(name, {}).get("overrides", [])):
            if "slow-timeout" in override:
                validate_timeout(
                    f"profile {name} override {index} timeout",
                    override["slow-timeout"],
                    1200 if name == "source" and override.get("filter") in tooling_filters else 270,
                )


def main() -> int:
    args = parse_args()
    if args.postgres_smoke_filter:
        print(postgres_smoke_filter())
        return 0
    root = Path(__file__).resolve().parents[1]
    archive = args.archive_file.resolve() if args.archive_file else None
    if archive is None:
        validate(args, root, None)
        return 0
    if not archive.is_file():
        raise ValueError(f"archive does not exist: {archive}")

    extract_parent = root / "target/nextest"
    extract_parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix="validation-", dir=extract_parent
    ) as extract_dir:
        extracted = Path(extract_dir)
        subprocess.run(
            ["tar", "xf", str(archive), "-C", str(extracted)],
            check=True,
            cwd=root,
        )
        validate(args, root, extracted)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, subprocess.CalledProcessError, ValueError, json.JSONDecodeError) as error:
        print(f"nextest coverage validation failed: {error}", file=sys.stderr)
        raise SystemExit(1)
