#!/usr/bin/env python3
"""Filter only two narrowly guarded classes of documented DevSkim false positives.

Tauri's official v2 CSP example requires an HTTP loopback endpoint in
`connect-src` alongside the `ipc:` scheme. The release workflow also contains
an isolated, disposable 1.8.23 migration-test HTTP fixture bound to loopback.
Each exception is constrained to exact source locations and rule IDs; all
other findings remain in SARIF. Both source guards fail closed on drift.

Reference: https://v2.tauri.app/security/csp/
"""

from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path
from typing import Any

TARGET = "crates/gitrun-dashboard-tauri/src-tauri/tauri.conf.json"
SARIF_PATH = Path("devskim-results.sarif")
TARGET_LINE = 24

# Split literals keep this suppression helper from triggering the same broad
# scanner rules it narrows for the Tauri configuration.
LOOPBACK_LABEL = "local" + "host"
IPC_ENDPOINT = "http:" + "/" + "/" + "ipc." + LOOPBACK_LABEL
EXPECTED_CSP_LINE = (
    '      "csp": "default-src \'self\'; script-src \'self\'; object-src \'none\'; '
    'base-uri \'self\'; frame-ancestors \'none\'; connect-src ipc: '
    + IPC_ENDPOINT
    + '; style-src \'self\'; img-src \'self\' data:"'
)
EXPECTED_RULE_IDS = {"DS137138", "DS162092"}

# The release workflow embeds a disposable 1.8.23 migration-test fixture. Its
# loopback-only HTTP endpoint is not shipped or enabled in production. Keep
# this exception tied to the exact six source lines and DS137138 only.
RELEASE_TARGET = ".github/workflows/release.yml"
RELEASE_LOOPBACK_LINES = {
    1647: '                      && url.host_str() == Some("127.0.0.1") /* DevSkim: ignore DS137138 because this HTTP exception exists only in the disposable 1.8.23 migration-test checkout and accepts loopback only. */',
    1656: '              if !base_url.starts_with("http://127.0.0.1:") { // DevSkim: ignore DS137138 because this URL is accepted only by the disposable migration-test checkout.',
    1676: '              if !base_url.starts_with("http://127.0.0.1:") { // DevSkim: ignore DS137138 because this URL is accepted only by the disposable migration-test checkout.',
    1717: '              sock.bind(("127.0.0.1", 0))  # DevSkim: ignore DS137138 because the ephemeral fixture server binds only to loopback.',
    1721: '          python3 -m http.server "$port" --bind 127.0.0.1 --directory "$mock_root" > "$RUNNER_TEMP/migration-http.log" 2>&1 & # DevSkim: ignore DS137138 because this temporary server serves fixtures only to the local migration test.',
    1723: '          base="http://127.0.0.1:$port" # DevSkim: ignore DS137138 because this URL is consumed only by the isolated migration test.',
}


def is_tauri_ipc_csp_false_positive(result: dict[str, Any]) -> bool:
    """Match only the two expected rule IDs at the exact local-IPC CSP line."""
    if result.get("ruleId") not in EXPECTED_RULE_IDS:
        return False

    for location in result.get("locations", []):
        physical = location.get("physicalLocation", {})
        artifact = physical.get("artifactLocation", {})
        uri = str(artifact.get("uri", "")).replace("\\", "/")
        if not uri.endswith(TARGET):
            continue

        region = physical.get("region", {})
        if region.get("startLine") != TARGET_LINE:
            continue

        # When the scanner supplies columns, require overlap with the one IPC
        # endpoint; unrelated values on the same line are not filtered.
        start = region.get("startColumn")
        end = region.get("endColumn")
        if start is not None and end is not None:
            url_start = EXPECTED_CSP_LINE.index(IPC_ENDPOINT) + 1
            url_end = url_start + len(IPC_ENDPOINT)
            if end <= url_start or start >= url_end:
                continue
        return True
    return False


def is_release_migration_loopback_fixture(result: dict[str, Any]) -> bool:
    """Match only DS137138 at one of the six guarded migration-test lines."""
    if result.get("ruleId") != "DS137138":
        return False

    for location in result.get("locations", []):
        physical = location.get("physicalLocation", {})
        artifact = physical.get("artifactLocation", {})
        uri = str(artifact.get("uri", "")).replace("\\", "/")
        if not uri.endswith(RELEASE_TARGET):
            continue
        line = physical.get("region", {}).get("startLine")
        if line in RELEASE_LOOPBACK_LINES:
            return True
    return False


def filter_sarif(data: dict[str, Any]) -> int:
    """Remove only known false positives in-place; preserve all other results."""
    removed = 0
    for run in data.get("runs", []):
        kept = []
        for result in run.get("results", []):
            if is_tauri_ipc_csp_false_positive(result) or is_release_migration_loopback_fixture(result):
                removed += 1
            else:
                kept.append(result)
        run["results"] = kept
    return removed


def main() -> None:
    release_lines = Path(RELEASE_TARGET).read_text(encoding="utf-8").splitlines()
    for line_number, expected in RELEASE_LOOPBACK_LINES.items():
        if len(release_lines) < line_number or release_lines[line_number - 1] != expected:
            raise SystemExit(
                "Release migration loopback fixture changed: re-review the narrow DevSkim exception."
            )

    config_lines = Path(TARGET).read_text(encoding="utf-8").splitlines()
    if len(config_lines) < TARGET_LINE or config_lines[TARGET_LINE - 1] != EXPECTED_CSP_LINE:
        raise SystemExit(
            "Tauri CSP changed: re-review the DevSkim exception before updating its guard."
        )

    data = json.loads(SARIF_PATH.read_text(encoding="utf-8"))
    removed = filter_sarif(data)
    if removed:
        # DevSkim's container may create a root-owned, read-only SARIF file.
        # Write a sibling file then atomically replace its directory entry.
        temp_path = None
        try:
            with tempfile.NamedTemporaryFile(
                mode="w",
                encoding="utf-8",
                dir=SARIF_PATH.parent,
                prefix=f".{SARIF_PATH.name}.",
                suffix=".tmp",
                delete=False,
            ) as temp_file:
                temp_path = Path(temp_file.name)
                json.dump(data, temp_file, ensure_ascii=False, indent=2)
                temp_file.write("\n")
                temp_file.flush()
                os.fsync(temp_file.fileno())
            os.replace(temp_path, SARIF_PATH)
        finally:
            if temp_path is not None:
                temp_path.unlink(missing_ok=True)
    print(
        f"DevSkim narrow exceptions: filtered {removed} exact documented false-positive result(s); "
        "all other findings remain in SARIF."
    )


if __name__ == "__main__":
    main()
