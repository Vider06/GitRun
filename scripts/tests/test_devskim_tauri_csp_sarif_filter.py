import unittest
from typing import Any

from scripts.filter_devskim_tauri_csp_sarif import (
    EXPECTED_FINDINGS,
    TARGET,
    filter_sarif,
    is_tauri_ipc_csp_false_positive,
)


def finding(rule_id: str, path: str = TARGET, line: int = 24, start: int = 131, end: int = 151) -> dict[str, Any]:
    return {
        "ruleId": rule_id,
        "message": {"text": EXPECTED_FINDINGS.get(rule_id, "different finding")},
        "locations": [{
            "physicalLocation": {
                "artifactLocation": {"uri": path},
                "region": {"startLine": line, "startColumn": start, "endColumn": end},
            }
        }],
    }


class DevSkimTauriCspFilterTests(unittest.TestCase):
    def test_filters_only_the_two_exact_tauri_ipc_rules(self) -> None:
        for rule_id in EXPECTED_FINDINGS:
            with self.subTest(rule_id=rule_id):
                self.assertTrue(is_tauri_ipc_csp_false_positive(finding(rule_id)))

    def test_preserves_same_rule_for_other_files_or_lines(self) -> None:
        self.assertFalse(
            is_tauri_ipc_csp_false_positive(finding("DS137138", path="src/network.rs"))
        )
        self.assertFalse(is_tauri_ipc_csp_false_positive(finding("DS137138", line=25)))

    def test_preserves_other_rules_and_nonmatching_messages(self) -> None:
        self.assertFalse(is_tauri_ipc_csp_false_positive(finding("DS999999")))
        bad_message = finding("DS137138")
        bad_message["message"]["text"] = "different finding"
        self.assertFalse(is_tauri_ipc_csp_false_positive(bad_message))

    def test_preserves_findings_outside_the_local_ipc_url_span(self) -> None:
        self.assertFalse(
            is_tauri_ipc_csp_false_positive(
                finding("DS137138", start=20, end=30)
            )
        )

    def test_filter_retains_unrelated_findings(self) -> None:
        expected = finding("DS137138")
        unrelated = finding("DS137138", path="src/network.rs")
        data = {"runs": [{"results": [expected, unrelated]}]}
        self.assertEqual(filter_sarif(data), 1)
        self.assertEqual(data["runs"][0]["results"], [unrelated])


if __name__ == "__main__":
    unittest.main()
