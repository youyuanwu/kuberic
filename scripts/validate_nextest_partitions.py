#!/usr/bin/env python3
"""Validate repository-wide nextest tier and partition coverage."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path


KIND_PACKAGE = "kuberic-level-tests"
KIND_PREFIX = "level_triggered_k8s::"
DEX_LIVE_BINARY = "kuberic-dex::kubernetes_checkpoint_real"
EXPECTED_KIND_LIVE_COUNT = 19


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


BUILD_ONLY_TESTS = {
    TestId(
        "kuberic-runtime",
        "kuberic-runtime::public_api_privacy",
        "external_applications_can_use_contracts_but_cannot_forge_host_capabilities",
    ),
    TestId(
        "kuberic-runtime",
        "kuberic-runtime::public_v1_conformance",
        "public_v1_conformance_executes_built_in_replacement_scenarios",
    ),
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--archive-file", type=Path)
    parser.add_argument("--ordinary-partitions", type=int, default=4)
    parser.add_argument("--postgres-partitions", type=int, default=4)
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
    if test_id in BUILD_ONLY_TESTS:
        return "build-only" if test.ignored else None
    if test.ignored:
        if test_id.package == KIND_PACKAGE and test_id.name.startswith(KIND_PREFIX):
            return "kind-live"
        return None
    if test_id.binary == DEX_LIVE_BINARY:
        return "dex-live"
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
    if assignments.get("build-only", set()) != BUILD_ONLY_TESTS:
        raise ValueError("build-only inventory does not exactly match its guards")
    if len(assignments.get("kind-live", set())) != EXPECTED_KIND_LIVE_COUNT:
        raise ValueError(
            f"KinD live selector expected {EXPECTED_KIND_LIVE_COUNT} tests, "
            f"found {len(assignments.get('kind-live', set()))}"
        )

    configured = {
        "build-only": matching_ids(
            nextest_json(
                root, extracted, "--profile", "build-only", "--run-ignored", "only"
            ),
            include_ignored=True,
        ),
        "ordinary": matching_ids(nextest_json(root, extracted, "--profile", "ordinary")),
        "postgres": matching_ids(nextest_json(root, extracted, "--profile", "postgres")),
        "kind-live": matching_ids(
            nextest_json(root, extracted, "--profile", "kind", "--run-ignored", "only"),
            include_ignored=True,
        ),
        "dex-live": matching_ids(nextest_json(root, extracted, "--profile", "dex-live")),
    }
    for tier, selected in configured.items():
        if selected != assignments.get(tier, set()):
            raise ValueError(f"configured {tier} profile does not exactly match inventory")

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


def main() -> int:
    args = parse_args()
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
