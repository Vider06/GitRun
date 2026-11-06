import unittest
from typing import Any

from scripts.filter_devskim_tauri_csp_sarif import (
    EXPECTED_RULE_IDS,
    RELEASE_LOOPBACK_ANCHORS,
    RELEASE_SUPPRESSION_MARKER,
    release_migration_loopback_lines,
    RELEASE_TARGET,
    TARGET,
    filter_sarif,
    is_release_migration_loopback_fixture,
    is_tauri_ipc_csp_false_positive,
)


def finding(
    rule_id: str,
    path: str = TARGET,
    line: int = 24,
    start: int = 131,
    end: int = 151,
) -> dict[str, Any]:
    return {
        "ruleId": rule_id,
        "message": {"text": "scanner finding"},
        "locations": [{
            "physicalLocation": {
                "artifactLocation": {"uri": path},
                "region": {"startLine": line, "startColumn": start, "endColumn": end},
            }
        }],
    }


class DevSkimTauriCspFilterTests(unittest.TestCase):
    def test_resolves_release_suppressions_after_arbitrary_line_shifts(self) -> None:
        anchors = RELEASE_LOOPBACK_ANCHORS
        fixture = [
            "ordinary workflow line",
            *[f"inserted line {index}" for index in range(40)],
            *[f"{anchor} # {RELEASE_SUPPRESSION_MARKER}" for anchor in anchors],
        ]
        allowed = release_migration_loopback_lines(fixture)
        self.assertEqual(allowed, set(range(42, 45)))
        self.assertTrue(
            is_release_migration_loopback_fixture(
                finding("DS137138", path=RELEASE_TARGET, line=42),
                allowed,
            )
        )

    def test_rejects_missing_or_misplaced_release_suppression_markers(self) -> None:
        fixture = [
            f'{anchor} # {RELEASE_SUPPRESSION_MARKER}'
            for anchor in RELEASE_LOOPBACK_ANCHORS
        ]
        with self.assertRaises(ValueError):
            release_migration_loopback_lines(fixture[:-1])
        fixture[0] = f'ordinary HTTP operation # {RELEASE_SUPPRESSION_MARKER}'
        with self.assertRaises(ValueError):
            release_migration_loopback_lines(fixture)

    def test_preserves_other_release_findings(self) -> None:
        allowed = {1661}
        self.assertFalse(
            is_release_migration_loopback_fixture(
                finding("DS137138", path=RELEASE_TARGET, line=1662), allowed
            )
        )
        self.assertFalse(
            is_release_migration_loopback_fixture(
                finding("DS999999", path=RELEASE_TARGET, line=1661), allowed
            )
        )
        self.assertFalse(
            is_release_migration_loopback_fixture(
                finding("DS137138", path="src/network.rs", line=1647), allowed
            )
        )

    def test_filters_only_the_two_expected_tauri_ipc_rules(self) -> None:
        self.assertEqual(EXPECTED_RULE_IDS, {"DS137138", "DS162092"})
        for rule_id in EXPECTED_RULE_IDS:
            with self.subTest(rule_id=rule_id):
                self.assertTrue(is_tauri_ipc_csp_false_positive(finding(rule_id)))

    def test_preserves_same_rule_for_other_files_or_lines(self) -> None:
        self.assertFalse(
            is_tauri_ipc_csp_false_positive(finding("DS137138", path="src/network.rs"))
        )
        self.assertFalse(is_tauri_ipc_csp_false_positive(finding("DS137138", line=25)))

    def test_preserves_other_rules(self) -> None:
        self.assertFalse(is_tauri_ipc_csp_false_positive(finding("DS999999")))

    def test_preserves_findings_outside_the_local_ipc_endpoint_span(self) -> None:
        self.assertFalse(
            is_tauri_ipc_csp_false_positive(finding("DS137138", start=20, end=30))
        )

    def test_filter_retains_unrelated_findings(self) -> None:
        expected = finding("DS137138")
        unrelated = finding("DS137138", path="src/network.rs")
        data = {"runs": [{"results": [expected, unrelated]}]}
        self.assertEqual(filter_sarif(data), 1)
        self.assertEqual(data["runs"][0]["results"], [unrelated])


if __name__ == "__main__":
    unittest.main()
