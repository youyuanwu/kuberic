#!/usr/bin/env python3
"""Report workflow-specific, shared, integration, and total switchover complexity."""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DECISIONS = re.compile(r"\b(?:if|match|for|while)\b|&&|\|\|")


@dataclass(frozen=True)
class Segment:
    path: str
    boundary: str | None = None


MEASUREMENTS = [
    ("explicit_switchover", [Segment("kuberic-operator/src/durable/switchover.rs", "explicit-switchover")]),
    ("pilot_module", [Segment("kuberic-operator/src/durable/pilot.rs", "pilot-module")]),
    ("pilot_workflow_subset", [Segment("kuberic-operator/src/durable/pilot.rs", "pilot-workflow")]),
    (
        "pilot_workflow_body",
        [Segment("kuberic-operator/src/durable/pilot.rs", "pilot-workflow-body")],
    ),
    (
        "shared_operator_effect_adapters",
        [Segment("kuberic-operator/src/durable/effects.rs", "shared-operator-effect-adapters")],
    ),
    ("pilot_store_integration", [Segment("kuberic-operator/src/durable/pilot_store.rs", "pilot-store")]),
    (
        "pilot_effect_bridge_integration",
        [Segment("kuberic-operator/src/reconciler.rs", "pilot-effect-bridge")],
    ),
    (
        "pilot_reconcile_integration",
        [Segment("kuberic-operator/src/reconciler.rs", "pilot-reconcile")],
    ),
    ("shared_kernel_typed", [Segment("durable-execution/src/typed.rs")]),
    (
        "shared_kernel_fused",
        [
            Segment("durable-execution/src/host.rs", "shared-kernel-fused-turn"),
            Segment("durable-execution/src/host.rs", "shared-kernel-fused-observe"),
        ],
    ),
]


def segment_lines(segment: Segment) -> list[str]:
    path = ROOT / segment.path
    lines = path.read_text(encoding="utf-8").splitlines()
    if segment.boundary is None:
        return lines
    start_marker = f"// COMPLEXITY-BOUNDARY: {segment.boundary}:start"
    end_marker = f"// COMPLEXITY-BOUNDARY: {segment.boundary}:end"
    stripped = [line.strip() for line in lines]
    try:
        start = stripped.index(start_marker) + 1
        end = stripped.index(end_marker)
    except ValueError as error:
        raise SystemExit(
            f"{path}: missing complexity boundary for {segment.boundary}"
        ) from error
    if start >= end:
        raise SystemExit(f"{path}: invalid complexity boundary for {segment.boundary}")
    return lines[start:end]


def segment_locations(segment: Segment) -> set[tuple[str, int]]:
    path = ROOT / segment.path
    lines = path.read_text(encoding="utf-8").splitlines()
    if segment.boundary is None:
        return {(segment.path, line) for line in range(1, len(lines) + 1)}
    start_marker = f"// COMPLEXITY-BOUNDARY: {segment.boundary}:start"
    end_marker = f"// COMPLEXITY-BOUNDARY: {segment.boundary}:end"
    stripped = [line.strip() for line in lines]
    try:
        start = stripped.index(start_marker) + 2
        end = stripped.index(end_marker) + 1
    except ValueError as error:
        raise SystemExit(
            f"{path}: missing complexity boundary for {segment.boundary}"
        ) from error
    return {(segment.path, line) for line in range(start, end)}


def validate_nonoverlapping(labels: list[str]) -> None:
    occupied: dict[tuple[str, int], str] = {}
    measurements = dict(MEASUREMENTS)
    for label in labels:
        for segment in measurements[label]:
            for location in segment_locations(segment):
                previous = occupied.get(location)
                if previous is not None:
                    raise SystemExit(
                        f"complexity scopes overlap: {previous} and {label} at "
                        f"{location[0]}:{location[1]}"
                    )
                occupied[location] = label


def measure(segments: list[Segment]) -> tuple[int, int]:
    lines = [line for segment in segments for line in segment_lines(segment)]
    executable = [
        line
        for line in lines
        if line.strip() and not line.lstrip().startswith("//")
    ]
    return len(executable), sum(len(DECISIONS.findall(line)) for line in executable)


def add(*values: tuple[int, int]) -> tuple[int, int]:
    return sum(value[0] for value in values), sum(value[1] for value in values)


def main() -> None:
    validate_nonoverlapping(
        [
            "pilot_module",
            "shared_operator_effect_adapters",
            "pilot_store_integration",
            "pilot_effect_bridge_integration",
            "pilot_reconcile_integration",
            "shared_kernel_typed",
            "shared_kernel_fused",
        ]
    )
    measured = {label: measure(segments) for label, segments in MEASUREMENTS}
    print("implementation,executable_lines,decision_points")
    for label, _ in MEASUREMENTS:
        lines, decisions = measured[label]
        print(f"{label},{lines},{decisions}")

    shared = add(
        measured["shared_operator_effect_adapters"],
        measured["shared_kernel_typed"],
        measured["shared_kernel_fused"],
    )
    integration = add(
        measured["pilot_store_integration"],
        measured["pilot_effect_bridge_integration"],
        measured["pilot_reconcile_integration"],
    )
    # pilot_workflow_subset is nested inside pilot_module and is intentionally
    # not added again.
    total = add(measured["pilot_module"], shared, integration)
    combined = add(total, measured["explicit_switchover"])
    print()
    print("summary,executable_lines,decision_points")
    print(
        "workflow_body_only,"
        f"{measured['pilot_workflow_body'][0]},{measured['pilot_workflow_body'][1]}"
    )
    print(
        "workflow_comparable_legacy_scope,"
        f"{measured['pilot_workflow_subset'][0]},{measured['pilot_workflow_subset'][1]}"
    )
    print(f"shared_reusable_infrastructure,{shared[0]},{shared[1]}")
    print(f"operator_integration,{integration[0]},{integration[1]}")
    print(f"pilot_nonoverlapping_total,{total[0]},{total[1]}")
    print(f"combined_explicit_shared_and_pilot_total,{combined[0]},{combined[1]}")
    print("baseline_explicit,1449,172")
    print("baseline_pilot_workflow_subset,820,99")
    print("baseline_pilot_nonoverlapping_total,3709,295")
    print("baseline_combined_explicit_and_pilot_total,5158,467")
    print()
    print(
        "note: pilot_workflow_body is the new workflow-only scope; "
        "pilot_workflow_subset preserves the merged-pilot marker for honest baseline "
        "comparison; both are nested in pilot_module and are not added twice; shared "
        "reusable infrastructure is charged in pilot total but can amortize across "
        "workflows; the combined total also charges shared protocol changes retained "
        "inside explicit_switchover; charged scopes are checked for line overlap before "
        "totals are emitted"
    )


if __name__ == "__main__":
    main()
