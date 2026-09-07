#!/usr/bin/env python3
"""Report attributable durable-workflow complexity without overlapping charges."""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DECISIONS = re.compile(r"\b(?:if|match|for|while)\b|&&|\|\|")
BASELINE_REVISION = "8d773ef2b32fd3073e11849a131fe2c2f5e6b97b"
SHARED_BEFORE = (1208, 110)
SHARED_LABELS = (
    "shared_operator_effect_adapters",
    "shared_operator_checkpoint_support",
    "shared_operator_workflow_host",
    "shared_kernel_typed",
    "shared_kernel_fused",
)


@dataclass(frozen=True)
class Segment:
    path: str
    boundary: str | None = None


MEASUREMENTS = [
    ("explicit_switchover", [Segment("kuberic-operator/src/durable/switchover.rs", "explicit-switchover")]),
    (
        "legacy_remove",
        [Segment("kuberic-operator/src/durable/remove_replica.rs", "explicit-remove")],
    ),
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
    (
        "shared_operator_checkpoint_support",
        [
            Segment(
                "kuberic-operator/src/durable/pilot_store.rs",
                "shared-operator-checkpoint-support",
            )
        ],
    ),
    (
        "shared_operator_workflow_host",
        [
            Segment(
                "kuberic-operator/src/durable/workflow_host.rs",
                "shared-operator-workflow-host",
            )
        ],
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


def extract_segment_lines(lines: list[str], segment: Segment) -> list[str]:
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
            f"{segment.path}: missing complexity boundary for {segment.boundary}"
        ) from error
    if start >= end:
        raise SystemExit(
            f"{segment.path}: invalid complexity boundary for {segment.boundary}"
        )
    return lines[start:end]


def segment_lines(segment: Segment) -> list[str]:
    path = ROOT / segment.path
    lines = path.read_text(encoding="utf-8").splitlines()
    return extract_segment_lines(lines, segment)


def segment_locations_from_lines(
    lines: list[str], segment: Segment
) -> set[tuple[str, int]]:
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
            f"{segment.path}: missing complexity boundary for {segment.boundary}"
        ) from error
    if start > end:
        raise SystemExit(
            f"{segment.path}: invalid complexity boundary for {segment.boundary}"
        )
    return {(segment.path, line) for line in range(start, end)}


def segment_locations(segment: Segment) -> set[tuple[str, int]]:
    path = ROOT / segment.path
    return segment_locations_from_lines(
        path.read_text(encoding="utf-8").splitlines(), segment
    )


def validate_measurement_registry(
    measurements: list[tuple[str, list[Segment]]],
) -> None:
    labels: set[str] = set()
    segments: set[Segment] = set()
    whole_files: set[str] = set()
    bounded_files: set[str] = set()
    for label, entries in measurements:
        if label in labels:
            raise SystemExit(f"duplicate complexity label: {label}")
        labels.add(label)
        for segment in entries:
            if segment in segments:
                raise SystemExit(
                    "duplicate complexity segment: "
                    f"{segment.path}:{segment.boundary or '<whole-file>'}"
                )
            segments.add(segment)
            if segment.boundary is None:
                if segment.path in whole_files or segment.path in bounded_files:
                    raise SystemExit(
                        f"duplicate whole-file complexity source: {segment.path}"
                    )
                whole_files.add(segment.path)
            else:
                if segment.path in whole_files:
                    raise SystemExit(
                        f"duplicate whole-file complexity source: {segment.path}"
                    )
                bounded_files.add(segment.path)


def validate_nonoverlapping(
    labels: list[str],
    measurements: list[tuple[str, list[Segment]]] = MEASUREMENTS,
    location_reader=segment_locations,
) -> None:
    occupied: dict[tuple[str, int], str] = {}
    measurement_map = dict(measurements)
    for label in labels:
        for segment in measurement_map[label]:
            for location in location_reader(segment):
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


def subtract(left: tuple[int, int], right: tuple[int, int]) -> tuple[int, int]:
    return left[0] - right[0], left[1] - right[1]


def ratio(numerator: tuple[int, int], denominator: tuple[int, int]) -> tuple[float, float]:
    return numerator[0] / denominator[0], numerator[1] / denominator[1]


def classify_amortization(
    remove_marginal: tuple[int, int],
    legacy_remove: tuple[int, int],
    shared_growth: tuple[int, int],
    shared_before: tuple[int, int] = SHARED_BEFORE,
) -> str:
    marginal_ratio = ratio(remove_marginal, legacy_remove)
    shared_growth_ratio = ratio(shared_growth, shared_before)
    if (
        marginal_ratio[0] < 1.0
        and marginal_ratio[1] < 1.0
        and shared_growth_ratio[0] <= 0.25
        and shared_growth_ratio[1] <= 0.25
    ):
        return "positive"
    if (
        marginal_ratio[0] >= 1.0
        or marginal_ratio[1] >= 1.0
        or shared_growth_ratio[0] > 0.50
        or shared_growth_ratio[1] > 0.50
    ):
        return "negative"
    return "inconclusive"


def main() -> None:
    validate_measurement_registry(MEASUREMENTS)
    validate_nonoverlapping(
        [
            "pilot_module",
            "shared_operator_effect_adapters",
            "shared_operator_checkpoint_support",
            "shared_operator_workflow_host",
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
        *(measured[label] for label in SHARED_LABELS),
    )
    shared_growth = subtract(shared, SHARED_BEFORE)
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
    print(f"shared_before,{SHARED_BEFORE[0]},{SHARED_BEFORE[1]}")
    print(f"shared_before_revision,{BASELINE_REVISION},not_applicable")
    print(f"shared_after,{shared[0]},{shared[1]}")
    print(f"shared_growth,{shared_growth[0]},{shared_growth[1]}")
    print(
        "legacy_remove,"
        f"{measured['legacy_remove'][0]},{measured['legacy_remove'][1]}"
    )
    print("remove_body,not_available,not_available")
    print("remove_integration,not_available,not_available")
    print("remove_marginal,not_available,not_available")
    print("remove_marginal_ratio,not_available,not_available")
    print("shared_growth_ratio,not_available,not_available")
    print("remove_amortization_classification,not_available,not_available")
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
        "totals are emitted; shared_before is frozen at revision "
        f"{BASELINE_REVISION} and second-workflow values remain unavailable until "
        "their attributable scopes exist"
    )


if __name__ == "__main__":
    main()
