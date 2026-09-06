#!/usr/bin/env python3
"""Report reproducible lexical complexity for both switchover implementations."""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DECISIONS = re.compile(r"\b(?:if|match|for|while)\b|&&|\|\|")
BOUNDARIES = [
    ("explicit_switchover", "kuberic-operator/src/durable/switchover.rs"),
    ("pilot_module", "kuberic-operator/src/durable/pilot.rs"),
    ("pilot_workflow", "kuberic-operator/src/durable/pilot.rs"),
    ("pilot_store", "kuberic-operator/src/durable/pilot_store.rs"),
    ("pilot_effect_bridge", "kuberic-operator/src/reconciler.rs"),
    ("pilot_reconcile", "kuberic-operator/src/reconciler.rs"),
]


def bounded_lines(label: str, relative_path: str) -> list[str]:
    path = ROOT / relative_path
    lines = path.read_text(encoding="utf-8").splitlines()
    start_marker = f"// COMPLEXITY-BOUNDARY: {label.replace('_', '-')}:start"
    end_marker = f"// COMPLEXITY-BOUNDARY: {label.replace('_', '-')}:end"
    try:
        start = lines.index(start_marker) + 1
        end = lines.index(end_marker)
    except ValueError as error:
        raise SystemExit(f"{path}: missing complexity boundary for {label}") from error
    if start >= end:
        raise SystemExit(f"{path}: invalid complexity boundary for {label}")
    return lines[start:end]


def measure(label: str, relative_path: str) -> tuple[int, int]:
    lines = bounded_lines(label, relative_path)
    executable = [
        line
        for line in lines
        if line.strip() and not line.lstrip().startswith("//")
    ]
    return len(executable), sum(len(DECISIONS.findall(line)) for line in executable)


def main() -> None:
    print("implementation,executable_lines,decision_points")
    for label, relative_path in BOUNDARIES:
        executable_lines, decision_points = measure(label, relative_path)
        print(f"{label},{executable_lines},{decision_points}")


if __name__ == "__main__":
    main()
