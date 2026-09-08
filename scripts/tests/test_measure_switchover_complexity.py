from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "measure-switchover-complexity.py"
SPEC = importlib.util.spec_from_file_location("measure_complexity", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
measure_complexity = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = measure_complexity
SPEC.loader.exec_module(measure_complexity)

Segment = measure_complexity.Segment


class ComplexityMeasurementTests(unittest.TestCase):
    def test_extract_rejects_missing_boundary(self) -> None:
        with self.assertRaisesRegex(SystemExit, "missing complexity boundary"):
            measure_complexity.extract_segment_lines(
                ["fn main() {}"], Segment("fixture.rs", "missing")
            )

    def test_extract_rejects_reversed_boundary(self) -> None:
        lines = [
            "// COMPLEXITY-BOUNDARY: fixture:end",
            "fn body() {}",
            "// COMPLEXITY-BOUNDARY: fixture:start",
        ]
        with self.assertRaisesRegex(SystemExit, "invalid complexity boundary"):
            measure_complexity.extract_segment_lines(
                lines, Segment("fixture.rs", "fixture")
            )

    def test_registry_rejects_duplicate_label(self) -> None:
        measurements = [
            ("same", [Segment("a.rs", "one")]),
            ("same", [Segment("b.rs", "two")]),
        ]
        with self.assertRaisesRegex(SystemExit, "duplicate complexity label"):
            measure_complexity.validate_measurement_registry(measurements)

    def test_registry_rejects_duplicate_segment(self) -> None:
        segment = Segment("a.rs", "one")
        measurements = [("one", [segment]), ("two", [segment])]
        with self.assertRaisesRegex(SystemExit, "duplicate complexity segment"):
            measure_complexity.validate_measurement_registry(measurements)

    def test_registry_rejects_whole_file_with_bounded_duplicate(self) -> None:
        measurements = [
            ("whole", [Segment("a.rs")]),
            ("bounded", [Segment("a.rs", "one")]),
        ]
        with self.assertRaisesRegex(
            SystemExit, "duplicate whole-file complexity source"
        ):
            measure_complexity.validate_measurement_registry(measurements)

    def test_overlap_rejected(self) -> None:
        measurements = [
            ("one", [Segment("a.rs", "one")]),
            ("two", [Segment("a.rs", "two")]),
        ]

        def same_locations(_segment: Segment) -> set[tuple[str, int]]:
            return {("a.rs", 7)}

        with self.assertRaisesRegex(SystemExit, "complexity scopes overlap"):
            measure_complexity.validate_nonoverlapping(
                ["one", "two"], measurements, same_locations
            )

    def test_frozen_baseline_revision(self) -> None:
        self.assertEqual(
            measure_complexity.BASELINE_REVISION,
            "8d773ef2b32fd3073e11849a131fe2c2f5e6b97b",
        )

    def test_remove_comparison_scopes_are_retired(self) -> None:
        labels = {label for label, _segments in measure_complexity.MEASUREMENTS}
        self.assertFalse(any(label.startswith("remove_") for label in labels))


if __name__ == "__main__":
    unittest.main()
