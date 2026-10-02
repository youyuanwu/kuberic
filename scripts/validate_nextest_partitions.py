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


ORDINARY_FILTER = (
    "not package(=postgres-replicated) "
    "and not binary(=kubernetes_checkpoint_real)"
)
POSTGRES_FILTER = "package(=postgres-replicated)"
KIND_PACKAGE = "kuberic-level-tests"
KIND_PREFIX = "level_triggered_k8s::"
DEX_LIVE_BINARY = "kuberic-dex::kubernetes_checkpoint_real"
EXTERNAL_BINARY = "sqlserver-replicated::live_observation"
HELPER_BINARIES = {
    "kuberic-agent::crash_boundaries",
    "kuberic-protocol::model",
    "postgres-replicated::native_durable",
    "postgres-replicated::validation_oracles",
}
EXPECTED_KIND_LIVE_COUNT = 12


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
    if test.ignored:
        if test_id.package == KIND_PACKAGE and test_id.name.startswith(KIND_PREFIX):
            return "kind-live"
        if test_id.binary == EXTERNAL_BINARY:
            return "external-manual"
        if test_id.binary in HELPER_BINARIES:
            return "helper-parent"
        return None
    if test_id.binary == DEX_LIVE_BINARY:
        return "dex-live"
    if test_id.package == "postgres-replicated":
        return "postgres"
    return "ordinary"


def matching_ids(document: dict) -> set[TestId]:
    return {
        test.test_id
        for test in listed_tests(document)
        if test.matches and not test.ignored
    }


def all_matching_ids(document: dict) -> set[TestId]:
    return {
        test.test_id
        for test in listed_tests(document)
        if test.matches
    }


def validate_partitions(
    root: Path,
    extracted: Path | None,
    expected: set[TestId],
    filterset: str,
    mode: str,
    count: int,
) -> list[int]:
    if count < 1:
        raise ValueError(f"{mode} partition count must be positive")
    partitions: list[set[TestId]] = []
    for index in range(1, count + 1):
        document = nextest_json(
            root,
            extracted,
            "-E",
            filterset,
            "--partition",
            f"{mode}:{index}/{count}",
        )
        selected = matching_ids(document)
        if not selected:
            raise ValueError(f"{mode} partition {index}/{count} is empty")
        partitions.append(selected)

    union: set[TestId] = set()
    for index, partition in enumerate(partitions, start=1):
        overlap = union & partition
        if overlap:
            example = sorted(overlap)[0]
            raise ValueError(
                f"partition {index}/{count} overlaps a previous partition: {example}"
            )
        union.update(partition)

    missing = expected - union
    extra = union - expected
    if missing or extra:
        raise ValueError(
            f"partition union mismatch: missing={len(missing)} extra={len(extra)}"
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
            continue
        assignments.setdefault(tier, set()).add(test.test_id)

    if unassigned:
        rendered = "\n".join(f"  {item}" for item in sorted(unassigned))
        raise ValueError(f"unassigned tests in complete inventory:\n{rendered}")

    all_assigned = set().union(*assignments.values())
    if len(all_assigned) != len(inventory):
        raise ValueError("test inventory contains duplicate identifiers")

    kind_live = assignments.get("kind-live", set())
    if len(kind_live) != EXPECTED_KIND_LIVE_COUNT:
        raise ValueError(
            f"KinD live selector expected {EXPECTED_KIND_LIVE_COUNT} tests, "
            f"found {len(kind_live)}"
        )
    configured_kind_live = all_matching_ids(
        nextest_json(
            root,
            extracted,
            "--run-ignored",
            "all",
            "-E",
            "group(=kind-live)",
        )
    )
    if configured_kind_live != kind_live:
        raise ValueError(
            "configured kind-live group does not exactly match the live KinD inventory"
        )

    ordinary_counts = validate_partitions(
        root,
        extracted,
        assignments["ordinary"],
        ORDINARY_FILTER,
        "slice",
        args.ordinary_partitions,
    )
    postgres_counts = validate_partitions(
        root,
        extracted,
        assignments["postgres"],
        POSTGRES_FILTER,
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
