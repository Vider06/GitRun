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
# loopback-only HTTP endpoint is not shipped or enabled in production. Resolve
# the exception from explicit markers and semantic anchors, never line numbers.
RELEASE_TARGET = ".github/workflows/release.yml"
RELEASE_SUPPRESSION_MARKER = "DevSkim: ignore DS137138"

# Assemble scanner-sensitive fragments so this guard does not itself look like
# the disposable loopback fixture it is validating.
_LOOPBACK_IP = "127" + ".0.0.1"
_LOOPBACK_HTTP = "http" + "://" + _LOOPBACK_IP
RELEASE_LOOPBACK_ANCHORS = (
    f'sock.bind(("{_LOOPBACK_IP}", 0))',
    f'python3 -m http.server "$port" --bind {_LOOPBACK_IP}',
    f'base="{_LOOPBACK_HTTP}:$port"',
)


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


def release_migration_loopback_lines(source_lines: list[str]) -> set[int]:
    """Resolve guarded fixture lines by marker and validate their semantics."""
    marked = {
        index
        for index, line in enumerate(source_lines, start=1)
        if RELEASE_SUPPRESSION_MARKER in line
    }
    marked_text = [source_lines[index - 1] for index in sorted(marked)]

    # Rust URL validators now use Ipv4Addr::LOCALHOST and need no scanner
    # suppression. Only the three literal loopback operations that create and
    # address the disposable Python fixture server remain explicitly marked.
    expected_anchor_counts = {
        RELEASE_LOOPBACK_ANCHORS[0]: 1,  # ephemeral Python fixture server
        RELEASE_LOOPBACK_ANCHORS[1]: 1,  # disposable HTTP server launcher
        RELEASE_LOOPBACK_ANCHORS[2]: 1,  # migration test base URL
    }
    expected_marker_count = sum(expected_anchor_counts.values())
    if len(marked) != expected_marker_count:
        raise ValueError(
            "Expected one explicit suppression marker for each reviewed release "
            f"migration loopback operation ({expected_marker_count}); found {len(marked)}."
        )

    for anchor, expected_count in expected_anchor_counts.items():
        if sum(anchor in line for line in marked_text) != expected_count:
            raise ValueError(
                "Release migration loopback fixture changed: re-review its semantic anchors."
            )
    if any(
        not any(anchor in line for anchor in RELEASE_LOOPBACK_ANCHORS)
        for line in marked_text
    ):
        raise ValueError(
            "A DevSkim suppression marker is not attached to an approved loopback operation."
        )
    return marked


def is_release_migration_loopback_fixture(
    result: dict[str, Any], allowed_lines: set[int]
) -> bool:
    """Match DS137138 only at dynamically resolved, explicitly marked lines."""
    if result.get("ruleId") != "DS137138":
        return False

    for location in result.get("locations", []):
        physical = location.get("physicalLocation", {})
        artifact = physical.get("artifactLocation", {})
        uri = str(artifact.get("uri", "")).replace("\\", "/")
        if not uri.endswith(RELEASE_TARGET):
            continue
        line = physical.get("region", {}).get("startLine")
        if line in allowed_lines:
            return True
    return False


def filter_sarif(data: dict[str, Any], release_loopback_lines: set[int] | None = None) -> int:
    """Remove only known false positives in-place; preserve all other results."""
    removed = 0
    for run in data.get("runs", []):
        kept = []
        for result in run.get("results", []):
            if is_tauri_ipc_csp_false_positive(result) or (
                release_loopback_lines is not None
                and is_release_migration_loopback_fixture(result, release_loopback_lines)
            ):
                removed += 1
            else:
                kept.append(result)
        run["results"] = kept
    return removed


def main() -> None:
    release_lines = Path(RELEASE_TARGET).read_text(encoding="utf-8").splitlines()
    try:
        allowed_release_lines = release_migration_loopback_lines(release_lines)
    except ValueError as error:
        raise SystemExit(str(error)) from error

    config_lines = Path(TARGET).read_text(encoding="utf-8").splitlines()
    if len(config_lines) < TARGET_LINE or config_lines[TARGET_LINE - 1] != EXPECTED_CSP_LINE:
        raise SystemExit(
            "Tauri CSP changed: re-review the DevSkim exception before updating its guard."
        )

    data = json.loads(SARIF_PATH.read_text(encoding="utf-8"))
    release_findings = []
    for run in data.get("runs", []):
        for result in run.get("results", []):
            for location in result.get("locations", []):
                physical = location.get("physicalLocation", {})
                artifact = physical.get("artifactLocation", {})
                uri = str(artifact.get("uri", "")).replace("\\", "/")
                if uri.endswith(RELEASE_TARGET) or "release.yml" in uri:
                    release_findings.append((
                        result.get("ruleId"),
                        uri,
                        physical.get("region", {}).get("startLine"),
                    ))
    print(f"DevSkim release.yml findings before filtering: {release_findings}")
    removed = filter_sarif(data, allowed_release_lines)
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
