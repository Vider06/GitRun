#!/usr/bin/env python3
"""Remove only the two documented DevSkim false positives for Tauri's local IPC CSP.

Tauri's official v2 CSP example explicitly requires `http://ipc.localhost` in
`connect-src` alongside `ipc:`. DevSkim's generic HTTP and localhost rules
misclassify that local IPC endpoint as insecure transport/debug code.

The suppression is intentionally bound to one file, one line, two rule IDs,
and the exact documented CSP. If the CSP line changes, this script fails closed
until the exception is re-reviewed. All other DevSkim findings are preserved.
Reference: https://v2.tauri.app/security/csp/
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

TARGET = "crates/gitrun-dashboard-tauri/src-tauri/tauri.conf.json"
SARIF_PATH = Path("devskim-results.sarif")
TARGET_LINE = 24
EXPECTED_CSP_LINE = (
    '      "csp": "default-src \'self\'; script-src \'self\'; object-src \'none\'; '
    'base-uri \'self\'; frame-ancestors \'none\'; connect-src ipc: http://ipc.localhost; '
    'style-src \'self\'; img-src \'self\' data:"'
)
EXPECTED_FINDINGS = {
    "DS137138": "An HTTP-based URL without TLS was detected.",
    "DS162092": "Accessing localhost could indicate debug code, or could hinder scaling.",
}


def is_tauri_ipc_csp_false_positive(result: dict[str, Any]) -> bool:
    """Match only the exact Tauri IPC warnings; never ignore a rule globally."""
    rule_id = result.get("ruleId")
    if rule_id not in EXPECTED_FINDINGS:
        return False
    if result.get("message", {}).get("text") != EXPECTED_FINDINGS[rule_id]:
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

        # When the scanner provides columns, require the finding to overlap the
        # actual local IPC URL. If it reports a whole-line region, the exact CSP
        # line is validated separately before filtering.
        start = region.get("startColumn")
        end = region.get("endColumn")
        if start is not None and end is not None:
            url_start = EXPECTED_CSP_LINE.index("http://ipc.localhost") + 1
            url_end = url_start + len("http://ipc.localhost")
            if end <= url_start or start >= url_end:
                continue
        return True
    return False


def filter_sarif(data: dict[str, Any]) -> int:
    """Remove the narrow known-false-positive matches in-place; retain all else."""
    removed = 0
    for run in data.get("runs", []):
        results = run.get("results", [])
        kept = []
        for result in results:
            if is_tauri_ipc_csp_false_positive(result):
                removed += 1
            else:
                kept.append(result)
        run["results"] = kept
    return removed


def main() -> None:
    config_lines = Path(TARGET).read_text(encoding="utf-8").splitlines()
    if len(config_lines) < TARGET_LINE or config_lines[TARGET_LINE - 1] != EXPECTED_CSP_LINE:
        raise SystemExit(
            "Tauri CSP changed: re-review the DevSkim exception before updating its guard."
        )

    data = json.loads(SARIF_PATH.read_text(encoding="utf-8"))
    removed = filter_sarif(data)
    if removed:
        SARIF_PATH.write_text(
            json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
    print(
        f"DevSkim Tauri IPC exception: filtered {removed} exact false-positive result(s); "
        "all other findings remain in SARIF."
    )


if __name__ == "__main__":
    main()
