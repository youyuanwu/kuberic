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

    def test_frozen_shared_baseline(self) -> None:
        measured = {
            label: measure_complexity.measure(dict(measure_complexity.MEASUREMENTS)[label])
            for label in measure_complexity.SHARED_LABELS
        }
        self.assertEqual(
            measure_complexity.add(
                *(measured[label] for label in measure_complexity.SHARED_LABELS)
            ),
            measure_complexity.SHARED_BEFORE,
        )

    def test_amortization_classifications(self) -> None:
        self.assertEqual(
            measure_complexity.classify_amortization(
                (700, 70), (1000, 100), (100, 10), (1000, 100)
            ),
            "positive",
        )
        self.assertEqual(
            measure_complexity.classify_amortization(
                (1000, 70), (1000, 100), (100, 10), (1000, 100)
            ),
            "negative",
        )
        self.assertEqual(
            measure_complexity.classify_amortization(
                (700, 70), (1000, 100), (300, 30), (1000, 100)
            ),
            "inconclusive",
        )


if __name__ == "__main__":
    unittest.main()
