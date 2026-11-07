import csv
import importlib.util
import os
import tempfile
import unittest
from pathlib import Path
from subprocess import CompletedProcess
from unittest.mock import patch


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
SCRIPT_PATH = REPOSITORY_ROOT / "scripts/ci/verify-rust-dual-coverage.py"
SPEC = importlib.util.spec_from_file_location("verify_rust_dual_coverage", SCRIPT_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"Unable to load dual-verifier gate: {SCRIPT_PATH}")
DUAL_COVERAGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DUAL_COVERAGE)


class DualVerifierCoverageTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / "src").mkdir()
        (self.root / "verification").mkdir()
        self.summary = self.root / "step-summary.md"
        self.report = self.root / "gitrun-dual-verifier-coverage.tsv"

        (self.root / "src/policy.rs").write_text(
            "pub fn accepted() -> bool { true }\n", encoding="utf-8"
        )
        (self.root / "kani.rs").write_text(
            '#[path = "src/policy.rs"] mod policy;\n'
            "#[kani::proof]\n"
            "fn policy_is_accepted() { assert!(policy::accepted()); }\n",
            encoding="utf-8",
        )
        (self.root / "verus.rs").write_text(
            '#[path = "src/policy.rs"] mod policy;\n'
            "verus! { fn policy_is_accepted() { assert(true); } }\n",
            encoding="utf-8",
        )

        self.old_root = DUAL_COVERAGE.ROOT
        self.old_manifest = DUAL_COVERAGE.MANIFEST
        self.old_report = DUAL_COVERAGE.REPORT
        DUAL_COVERAGE.ROOT = self.root
        DUAL_COVERAGE.MANIFEST = self.root / "verification/source-proof-map.tsv"
        DUAL_COVERAGE.REPORT = self.report
        self.addCleanup(setattr, DUAL_COVERAGE, "ROOT", self.old_root)
        self.addCleanup(setattr, DUAL_COVERAGE, "MANIFEST", self.old_manifest)
        self.addCleanup(setattr, DUAL_COVERAGE, "REPORT", self.old_report)

        self.sources_patch = patch.object(
            DUAL_COVERAGE, "tracked_sources", return_value={"src/policy.rs"}
        )
        self.sources_mock = self.sources_patch.start()
        self.addCleanup(self.sources_patch.stop)
        self.environment_patch = patch.dict(
            os.environ, {"GITHUB_STEP_SUMMARY": str(self.summary)}
        )
        self.environment_patch.start()
        self.addCleanup(self.environment_patch.stop)

        self.write_manifest(
            [("src/policy.rs", "kani.rs", "verus.rs", "pure policy", "Kani property", "finite Kani cases", "Verus invariant", "IMPLEMENTED")]
        )

    def write_manifest(self, rows: list[tuple[str, str, str, str, str, str, str, str]]) -> None:
        manifest = DUAL_COVERAGE.MANIFEST
        manifest.parent.mkdir(parents=True, exist_ok=True)
        with manifest.open("w", encoding="utf-8", newline="") as handle:
            writer = csv.writer(handle, delimiter="\t", lineterminator="\n")
            writer.writerow(DUAL_COVERAGE.EXPECTED_HEADER)
            writer.writerows(rows)

    def read_report(self) -> list[dict[str, str]]:
        with self.report.open(encoding="utf-8", newline="") as handle:
            return list(csv.DictReader(handle, delimiter="\t"))

    def test_manifest_schema_preserves_kani_cases_as_a_distinct_column(self) -> None:
        self.assertEqual(len(DUAL_COVERAGE.EXPECTED_HEADER), 8)
        self.assertEqual(DUAL_COVERAGE.EXPECTED_HEADER[5], "KANI_CASES")
        with DUAL_COVERAGE.MANIFEST.open(encoding="utf-8", newline="") as handle:
            rows = list(csv.reader(handle, delimiter="\t"))
        self.assertEqual(len(rows[0]), 8)
        self.assertEqual(len(rows[1]), 8)
        self.assertEqual(rows[1][5], "finite Kani cases")
        self.assertEqual(rows[1][7], "IMPLEMENTED")

    def test_reports_proven_only_after_both_verifiers_succeed(self) -> None:
        with patch.object(
            DUAL_COVERAGE.subprocess,
            "run",
            side_effect=[
                CompletedProcess(["kani"], 0),
                CompletedProcess(["verus"], 0),
            ],
        ) as run:
            result = DUAL_COVERAGE.main()

        self.assertEqual(result, 0)
        self.assertEqual(run.call_count, 2)
        self.assertEqual(
            self.read_report(),
            [{
                "SOURCE_FILE": "src/policy.rs",
                "KANI_STATUS": "PROVEN",
                "VERUS_STATUS": "PROVEN",
            }],
        )
        self.assertIn("Fully proven by Kani and Verus | 1", self.summary.read_text())

    def test_planned_source_proof_remains_unverified_and_is_not_executed(self) -> None:
        self.write_manifest(
            [("src/policy.rs", "", "", "pure policy", "Kani property", "finite Kani cases", "Verus invariant", "PLANNED")]
        )

        with patch.object(DUAL_COVERAGE.subprocess, "run") as run:
            result = DUAL_COVERAGE.main()

        self.assertEqual(result, 1)
        run.assert_not_called()
        row = self.read_report()[0]
        self.assertEqual(row["KANI_STATUS"], "UNVERIFIED")
        self.assertEqual(row["VERUS_STATUS"], "UNVERIFIED")

    def test_missing_source_mapping_does_not_block_validated_proofs(self) -> None:
        (self.root / "src/other.rs").write_text(
            "pub fn other() {}\n", encoding="utf-8"
        )
        self.sources_mock.return_value = {"src/policy.rs", "src/other.rs"}

        with patch.object(
            DUAL_COVERAGE.subprocess,
            "run",
            side_effect=[
                CompletedProcess(["kani"], 0),
                CompletedProcess(["verus"], 0),
            ],
        ) as run:
            result = DUAL_COVERAGE.main()

        self.assertEqual(result, 1)
        self.assertEqual(run.call_count, 2)
        rows = {row["SOURCE_FILE"]: row for row in self.read_report()}
        self.assertEqual(rows["src/policy.rs"]["KANI_STATUS"], "PROVEN")
        self.assertEqual(rows["src/policy.rs"]["VERUS_STATUS"], "PROVEN")
        self.assertEqual(rows["src/other.rs"]["KANI_STATUS"], "UNVERIFIED")
        self.assertEqual(rows["src/other.rs"]["VERUS_STATUS"], "UNVERIFIED")
        self.assertNotIn("READY", self.report.read_text())
        self.assertIn("execution complete with preflight issues", self.summary.read_text())

    def test_failed_verus_proof_does_not_report_dual_coverage(self) -> None:
        with patch.object(
            DUAL_COVERAGE.subprocess,
            "run",
            side_effect=[
                CompletedProcess(["kani"], 0),
                CompletedProcess(["verus"], 1),
            ],
        ) as run:
            result = DUAL_COVERAGE.main()

        self.assertEqual(result, 1)
        self.assertEqual(run.call_count, 2)
        row = self.read_report()[0]
        self.assertEqual(row["KANI_STATUS"], "PROVEN")
        self.assertEqual(row["VERUS_STATUS"], "FAILED")
        self.assertIn("Fully proven by Kani and Verus | 0", self.summary.read_text())


if __name__ == "__main__":
    unittest.main()
