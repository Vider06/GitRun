import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).parents[1] / "ci" / "ci-job-report.py"


class JobReportTests(unittest.TestCase):
    def test_write_emits_common_schema_and_step_data(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            ci = root / "gitrun-ci"
            ci.mkdir()
            (ci / "steps.tsv").write_text("compile\tok\t12\n", encoding="utf-8")
            (ci / "checks.tsv").write_text("error\tfail\ttest\tassertion failed\n", encoding="utf-8")
            (ci / "started_at").write_text("2026-11-23T10:00:00Z", encoding="utf-8")
            env = {**os.environ, "RUNNER_TEMP": temp, "GITHUB_JOB": "unit-test", "GITRUN_JOB_STATUS": "failure", "GITHUB_RUN_ID": "42"}
            subprocess.run(["python3", str(SCRIPT), "write"], env=env, check=True, capture_output=True, text=True)
            result = json.loads((root / "gitrun-job-result.json").read_text(encoding="utf-8"))
            self.assertEqual(result["schema_version"], 1)
            self.assertEqual(result["job"], "unit-test")
            self.assertEqual(result["status"], "failure")
            self.assertEqual(result["steps"][0]["duration_seconds"], 12)
            self.assertEqual(result["checks"][0]["name"], "test")

    def test_diagnostic_keeps_failure_context_and_redacts_tokens(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            log = root / "kani.log"
            log.write_text("proof failed: error[E0001]\ngithub_pat_abcdefghijklmnopqrstuvwxyz123456\n", encoding="utf-8")
            env = {**os.environ, "RUNNER_TEMP": temp}
            subprocess.run(["python3", str(SCRIPT), "diagnostic", "--step", "kani", "--command", "cargo kani", "--exit-code", "1", "--log-file", str(log)], env=env, check=True, capture_output=True, text=True)
            record = json.loads((root / "gitrun-ci" / "diagnostics.jsonl").read_text(encoding="utf-8"))
            self.assertEqual(record["exit_code"], 1)
            self.assertIn("error[E0001]", record["diagnostic_excerpt"])
            self.assertNotIn("github_pat_", record["diagnostic_excerpt"])


if __name__ == "__main__":
    unittest.main()