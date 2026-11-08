#!/usr/bin/env python3
"""Audit source ownership and the declared dual-verifier proof scope.

This is a fail-closed preflight, not a verifier. Kani and Verus are executed by
their dedicated CI scripts. This audit confirms that every IMPLEMENTED row
points to real, source-connected proof entrypoints and keeps the overall gate
closed while any tracked source remains PLANNED or unmapped.
"""

from __future__ import annotations

import csv
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "verification/source-proof-map.tsv"
REPORT = Path(os.environ.get("RUNNER_TEMP", "/tmp")) / "gitrun-dual-verifier-coverage.tsv"
EXPECTED_HEADER = [
    "SOURCE_FILE",
    "KANI_ENTRYPOINT",
    "VERUS_ENTRYPOINT",
    "PROOF_SCOPE",
    "KANI_OBLIGATIONS",
    "KANI_CASES",
    "VERUS_OBLIGATIONS",
    "STATUS",
]
PATH_ATTRIBUTE = re.compile(r'#\s*\[\s*path\s*=\s*["\']([^"\']+)["\']\s*\]')


def tracked_sources() -> set[str]:
    result = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs"],
        cwd=ROOT,
        check=True,
        stdout=subprocess.PIPE,
    )
    return {item.decode() for item in result.stdout.split(b"\0") if item}


def references_source(entrypoint: str, source: str) -> bool:
    entrypoint_path = ROOT / entrypoint
    source_path = (ROOT / source).resolve()
    if entrypoint_path.resolve() == source_path:
        return True
    try:
        content = entrypoint_path.read_text(encoding="utf-8")
    except OSError:
        return False
    for relative_path in PATH_ATTRIBUTE.findall(content):
        if (entrypoint_path.parent / relative_path).resolve() == source_path:
            return True
    return False


def has_proof_construct(entrypoint: str, tool: str) -> bool:
    try:
        content = (ROOT / entrypoint).read_text(encoding="utf-8")
    except OSError:
        return False
    if tool == "KANI":
        return "#[kani::proof]" in content
    return "verus!" in content and (
        "proof fn " in content or "verus_verify" in content
    )


def write_report(
    sources: set[str], rows: dict[str, tuple[str, ...]]
) -> dict[str, int]:
    counts = {"IMPLEMENTED": 0, "PLANNED": 0, "UNMAPPED": 0}
    REPORT.parent.mkdir(parents=True, exist_ok=True)
    with REPORT.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle, delimiter="\t", lineterminator="\n")
        writer.writerow(
            ["SOURCE_FILE", "KANI_STATUS", "VERUS_STATUS", "SCOPE_STATUS"]
        )
        for source in sorted(sources):
            mapping = rows.get(source)
            if mapping is None:
                counts["UNMAPPED"] += 1
                writer.writerow([source, "UNVERIFIED", "UNVERIFIED", "UNMAPPED"])
                continue
            scope = mapping[7]
            if scope in counts:
                counts[scope] += 1
            # Inventory and source-closure checks are not verifier evidence.
            # Even IMPLEMENTED scopes remain UNVERIFIED until Kani and Verus run.
            status = "UNVERIFIED" if scope == "IMPLEMENTED" else "NOT_IN_SCOPE"
            writer.writerow([source, status, status, scope])
    return counts


def write_summary(counts: dict[str, int], total: int, issues: list[str]) -> None:
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary:
        return
    gate = "OPEN" if not counts["PLANNED"] and not counts["UNMAPPED"] and not issues else "CLOSED"
    with Path(summary).open("a", encoding="utf-8") as handle:
        handle.write("### Dual-verifier source-scope audit\n\n")
        handle.write("| Metric | Result |\n|---|---:|\n")
        handle.write(f"| Tracked Rust source files | {total} |\n")
        handle.write(f"| Proof scopes declared IMPLEMENTED | {counts['IMPLEMENTED']} |\n")
        handle.write(f"| Proof scopes still PLANNED | {counts['PLANNED']} |\n")
        handle.write(f"| Sources without a manifest mapping | {counts['UNMAPPED']} |\n")
        handle.write(f"| Manifest / ownership issues | {len(issues)} |\n")
        handle.write(f"| Formal coverage gate | **{gate}** |\n\n")
        handle.write(
            "This preflight checks source ownership and entrypoint linkage only. "
            "It never reports PROVEN: IMPLEMENTED entries remain UNVERIFIED here "
            "until their dedicated Kani and Verus executions succeed. PLANNED "
            "entries remain NOT_IN_SCOPE. The gate stays closed while any source "
            "is planned or unmapped.\n\n"
        )


def main() -> int:
    sources = tracked_sources()
    issues: list[str] = []
    rows: dict[str, tuple[str, ...]] = {}

    if not sources:
        print("No tracked Rust sources found; refusing an empty audit.", file=sys.stderr)
        return 1
    if not MANIFEST.is_file():
        print(f"Required proof-scope manifest is missing: {MANIFEST}", file=sys.stderr)
        return 1

    with MANIFEST.open(encoding="utf-8", newline="") as handle:
        reader = csv.reader(handle, delimiter="\t")
        if next(reader, []) != EXPECTED_HEADER:
            issues.append("Manifest header does not match the required eight-column schema.")
        for line, row in enumerate(reader, start=2):
            if not row or (len(row) == 1 and not row[0].strip()):
                continue
            if len(row) != len(EXPECTED_HEADER):
                issues.append(
                    f"Manifest line {line}: expected {len(EXPECTED_HEADER)} TSV columns."
                )
                continue
            values = tuple(value.strip() for value in row)
            (
                source,
                kani,
                verus,
                scope,
                kani_obligations,
                kani_cases,
                verus_obligations,
                status,
            ) = values
            if source in rows:
                issues.append(f"Duplicate source mapping: {source}")
                continue
            if not all((source, scope, kani_obligations, kani_cases, verus_obligations)):
                issues.append(
                    f"Manifest line {line}: source and all proof-scope descriptions are required."
                )
            if status not in {"PLANNED", "IMPLEMENTED"}:
                issues.append(f"Manifest line {line}: invalid scope status {status!r}.")
            if status == "IMPLEMENTED":
                for tool, entrypoint in (("KANI", kani), ("VERUS", verus)):
                    if not entrypoint:
                        issues.append(f"{tool} entrypoint missing for implemented scope {source}.")
                        continue
                    entrypoint_path = ROOT / entrypoint
                    if not entrypoint_path.is_file():
                        issues.append(
                            f"{tool} entrypoint does not exist for {source}: {entrypoint}"
                        )
                        continue
                    if not references_source(entrypoint, source):
                        issues.append(
                            f"{tool} entrypoint is not connected to mapped production source "
                            f"{source}: {entrypoint}"
                        )
                    if not has_proof_construct(entrypoint, tool):
                        issues.append(
                            f"{tool} proof construct is missing from entrypoint for "
                            f"{source}: {entrypoint}"
                        )
            rows[source] = values

    for stale in sorted(set(rows) - sources):
        issues.append(f"Manifest contains a non-tracked or stale Rust source: {stale}")
    for missing in sorted(sources - set(rows)):
        issues.append(f"Tracked Rust source has no proof-scope mapping: {missing}")

    counts = write_report(sources, rows)
    write_summary(counts, len(sources), issues)

    print(
        f"Audited {len(sources)} tracked Rust sources: "
        f"{counts['IMPLEMENTED']} declared proof scopes implemented, "
        f"{counts['PLANNED']} planned, {counts['UNMAPPED']} unmapped."
    )
    if counts["PLANNED"]:
        issues.append(
            f"{counts['PLANNED']} tracked Rust source proof scopes remain PLANNED."
        )
    if counts["UNMAPPED"]:
        issues.append(f"{counts['UNMAPPED']} tracked Rust source files are unmapped.")
    if issues:
        for issue in issues[:200]:
            print(f"::error::{issue}", file=sys.stderr)
        return 1

    print(
        "Every tracked Rust source has an implemented and connected proof scope. "
        "Verifier results are reported only by the Kani and Verus execution jobs."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
