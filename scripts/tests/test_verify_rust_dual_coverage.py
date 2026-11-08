import csv
import importlib.util
import os
import tempfile
import unittest
from pathlib import Path
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
        self.kani_results = self.root / "gitrun-kani-proof-results.tsv"
        self.verus_results = self.root / "gitrun-verus-proof-results.tsv"

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
            "verus! { proof fn policy_is_accepted() { assert(true); } }\n",
            encoding="utf-8",
        )

        constants = {
            "ROOT": self.root,
            "MANIFEST": self.root / "verification/source-proof-map.tsv",
            "REPORT": self.report,
            "KANI_RESULTS": self.kani_results,
            "VERUS_RESULTS": self.verus_results,
        }
        for name, value in constants.items():
            old_value = getattr(DUAL_COVERAGE, name)
            setattr(DUAL_COVERAGE, name, value)
            self.addCleanup(setattr, DUAL_COVERAGE, name, old_value)

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
            [
                (
                    "src/policy.rs",
                    "kani.rs",
                    "verus.rs",
                    "pure policy",
                    "Kani property",
                    "finite Kani cases",
                    "Verus invariant",
                    "IMPLEMENTED",
                )
            ]
        )

    def write_manifest(
        self, rows: list[tuple[str, str, str, str, str, str, str, str]]
    ) -> None:
        with DUAL_COVERAGE.MANIFEST.open("w", encoding="utf-8", newline="") as handle:
            writer = csv.writer(handle, delimiter="\t", lineterminator="\n")
            writer.writerow(DUAL_COVERAGE.EXPECTED_HEADER)
            writer.writerows(rows)

    @staticmethod
    def write_results(path: Path, rows: list[tuple[str, str]]) -> None:
        with path.open("w", encoding="utf-8", newline="") as handle:
            writer = csv.writer(handle, delimiter="\t", lineterminator="\n")
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

    def test_implemented_scope_stays_unverified_until_both_verifiers_run(self) -> None:
        with patch.object(DUAL_COVERAGE.subprocess, "run") as run:
            result = DUAL_COVERAGE.main()

        self.assertEqual(result, 1)
        run.assert_not_called()
        self.assertEqual(
            self.read_report(),
            [
                {
                    "SOURCE_FILE": "src/policy.rs",
                    "KANI_STATUS": "UNVERIFIED",
                    "VERUS_STATUS": "UNVERIFIED",
                    "SCOPE_STATUS": "UNVERIFIED",
                }
            ],
        )
        summary = self.summary.read_text(encoding="utf-8")
        self.assertIn("Formal coverage gate | **CLOSED**", summary)
        self.assertIn("Awaiting one or both verifier results | 1", summary)
        self.assertNotIn("PROVEN", self.report.read_text(encoding="utf-8"))

    def test_source_is_proven_only_after_both_verifiers_succeed(self) -> None:
        self.write_results(self.kani_results, [("src/policy.rs", "PROVEN")])
        self.write_results(self.verus_results, [("src/policy.rs", "PROVEN")])

        result = DUAL_COVERAGE.main()

        self.assertEqual(result, 0)
        self.assertEqual(
            self.read_report(),
            [
                {
                    "SOURCE_FILE": "src/policy.rs",
                    "KANI_STATUS": "PROVEN",
                    "VERUS_STATUS": "PROVEN",
                    "SCOPE_STATUS": "PROVEN",
                }
            ],
        )
        self.assertIn("Proven by both Kani and Verus | 1", self.summary.read_text())
        self.assertIn("Formal coverage gate | **OPEN**", self.summary.read_text())

    def test_one_verifier_failure_never_becomes_proven(self) -> None:
        self.write_results(self.kani_results, [("src/policy.rs", "PROVEN")])
        self.write_results(self.verus_results, [("src/policy.rs", "FAILED")])

        result = DUAL_COVERAGE.main()

        self.assertEqual(result, 1)
        row = self.read_report()[0]
        self.assertEqual(row["KANI_STATUS"], "PROVEN")
        self.assertEqual(row["VERUS_STATUS"], "FAILED")
        self.assertEqual(row["SCOPE_STATUS"], "FAILED")
        self.assertIn("Failed by at least one verifier | 1", self.summary.read_text())

    def test_one_verifier_success_without_the_other_stays_unverified(self) -> None:
        self.write_results(self.kani_results, [("src/policy.rs", "PROVEN")])

        result = DUAL_COVERAGE.main()

        self.assertEqual(result, 1)
        row = self.read_report()[0]
        self.assertEqual(row["KANI_STATUS"], "PROVEN")
        self.assertEqual(row["VERUS_STATUS"], "UNVERIFIED")
        self.assertEqual(row["SCOPE_STATUS"], "UNVERIFIED")

    def test_planned_source_keeps_gate_closed_and_is_not_reported_as_proven(self) -> None:
        self.write_manifest(
            [
                (
                    "src/policy.rs",
                    "",
                    "",
                    "pure policy",
                    "Kani property",
                    "finite Kani cases",
                    "Verus invariant",
                    "PLANNED",
                )
            ]
        )

        result = DUAL_COVERAGE.main()

        self.assertEqual(result, 1)
        row = self.read_report()[0]
        self.assertEqual(row["KANI_STATUS"], "NOT_IN_SCOPE")
        self.assertEqual(row["VERUS_STATUS"], "NOT_IN_SCOPE")
        self.assertEqual(row["SCOPE_STATUS"], "PLANNED")
        self.assertIn("Formal coverage gate | **CLOSED**", self.summary.read_text())

    def test_not_applicable_scope_is_documented_and_never_reported_as_proven(self) -> None:
        self.write_manifest(
            [
                (
                    "src/policy.rs",
                    "",
                    "",
                    "Build-time integration is NOT APPLICABLE to formal runtime proof",
                    "NOT APPLICABLE: Cargo owns the host environment and build-script execution",
                    "cargo build executes and validates the build script",
                    "NOT APPLICABLE: Verus does not model Cargo host orchestration",
                    "NOT_APPLICABLE",
                )
            ]
        )

        result = DUAL_COVERAGE.main()

        self.assertEqual(result, 0)
        row = self.read_report()[0]
        self.assertEqual(row["KANI_STATUS"], "NOT_APPLICABLE")
        self.assertEqual(row["VERUS_STATUS"], "NOT_APPLICABLE")
        self.assertEqual(row["SCOPE_STATUS"], "NOT_APPLICABLE")
        summary = self.summary.read_text(encoding="utf-8")
        self.assertIn("Proven by both Kani and Verus | 0", summary)
        self.assertIn("Formally NOT_APPLICABLE with documented reason | 1", summary)

    def test_not_applicable_requires_separate_kani_and_verus_limits(self) -> None:
        self.write_manifest(
            [
                (
                    "src/policy.rs",
                    "",
                    "",
                    "host build integration",
                    "NOT APPLICABLE: Kani cannot model the host build process",
                    "cargo build executes the build script",
                    "NOT APPLICABLE: Verus does not model Cargo build orchestration",
                    "NOT_APPLICABLE",
                )
            ]
        )

        result = DUAL_COVERAGE.main()

        self.assertEqual(result, 1)
        self.assertIn("Manifest / ownership issues | 1", self.summary.read_text())
        self.assertIn("Formal coverage gate | **CLOSED**", self.summary.read_text())

    def test_missing_source_mapping_keeps_gate_closed_and_is_reported(self) -> None:
        (self.root / "src/other.rs").write_text(
            "pub fn other() {}\n", encoding="utf-8"
        )
        self.sources_mock.return_value = {"src/policy.rs", "src/other.rs"}

        result = DUAL_COVERAGE.main()

        self.assertEqual(result, 1)
        rows = {row["SOURCE_FILE"]: row for row in self.read_report()}
        self.assertEqual(rows["src/policy.rs"]["KANI_STATUS"], "UNVERIFIED")
        self.assertEqual(rows["src/policy.rs"]["VERUS_STATUS"], "UNVERIFIED")
        self.assertEqual(rows["src/other.rs"]["KANI_STATUS"], "UNVERIFIED")
        self.assertEqual(rows["src/other.rs"]["VERUS_STATUS"], "UNVERIFIED")
        self.assertEqual(rows["src/other.rs"]["SCOPE_STATUS"], "UNMAPPED")
        self.assertIn("Sources without a valid mapping | 1", self.summary.read_text())
        self.assertIn("sources without a valid mapping", self.summary.read_text().lower())

    def test_implemented_scope_requires_existing_source_connected_entrypoints(self) -> None:
        self.write_manifest(
            [
                (
                    "src/policy.rs",
                    "missing-kani.rs",
                    "verus.rs",
                    "pure policy",
                    "Kani property",
                    "finite Kani cases",
                    "Verus invariant",
                    "IMPLEMENTED",
                )
            ]
        )

        result = DUAL_COVERAGE.main()

        self.assertEqual(result, 1)
        self.assertIn("Manifest / ownership issues | 2", self.summary.read_text())


if __name__ == "__main__":
    unittest.main()
