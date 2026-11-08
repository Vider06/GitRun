#!/usr/bin/env python3
"""Fail-closed source inventory and dual-verifier result audit.

This preflight checks that each IMPLEMENTED scope is connected to production
code, then combines the per-source Kani and Verus execution records. A source is
PROVEN only when both tools have recorded a successful result for that source.
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
RUNNER_TEMP = Path(os.environ.get("RUNNER_TEMP", "/tmp"))
REPORT = RUNNER_TEMP / "gitrun-dual-verifier-coverage.tsv"
KANI_RESULTS = RUNNER_TEMP / "gitrun-kani-proof-results.tsv"
VERUS_RESULTS = RUNNER_TEMP / "gitrun-verus-proof-results.tsv"
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
VALID_RESULTS = {"PROVEN", "FAILED", "UNVERIFIED"}


def tracked_sources() -> set[str]:
    result = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs"],
        cwd=ROOT,
        check=True,
        stdout=subprocess.PIPE,
    )
    return {item.decode() for item in result.stdout.split(b"\0") if item}


def read_results(path: Path) -> dict[str, str]:
    results: dict[str, str] = {}
    try:
        with path.open(encoding="utf-8", newline="") as handle:
            for row in csv.reader(handle, delimiter="\t"):
                if len(row) != 2:
                    continue
                source, status = row
                if status in VALID_RESULTS:
                    results[source] = status
    except FileNotFoundError:
        return {}
    return results


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
    kani_results = read_results(KANI_RESULTS)
    verus_results = read_results(VERUS_RESULTS)
    counts = {
        "IMPLEMENTED": 0,
        "PLANNED": 0,
        "UNMAPPED": 0,
        "PROVEN": 0,
        "FAILED": 0,
        "UNVERIFIED": 0,
        "NOT_APPLICABLE": 0,
        "KANI_PROVEN": 0,
        "VERUS_PROVEN": 0,
    }
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
            if scope == "NOT_APPLICABLE":
                counts["NOT_APPLICABLE"] += 1
                writer.writerow(
                    [source, "NOT_APPLICABLE", "NOT_APPLICABLE", "NOT_APPLICABLE"]
                )
                continue
            if scope == "PLANNED":
                counts["PLANNED"] += 1
                writer.writerow([source, "NOT_IN_SCOPE", "NOT_IN_SCOPE", "PLANNED"])
                continue
            if scope != "IMPLEMENTED":
                counts["UNMAPPED"] += 1
                writer.writerow([source, "UNVERIFIED", "UNVERIFIED", "INVALID_STATUS"])
                continue

            counts["IMPLEMENTED"] += 1
            kani_status = kani_results.get(source, "UNVERIFIED")
            verus_status = verus_results.get(source, "UNVERIFIED")
            if kani_status == "PROVEN":
                counts["KANI_PROVEN"] += 1
            if verus_status == "PROVEN":
                counts["VERUS_PROVEN"] += 1

            if kani_status == "PROVEN" and verus_status == "PROVEN":
                scope_status = "PROVEN"
                counts["PROVEN"] += 1
            elif "FAILED" in (kani_status, verus_status):
                scope_status = "FAILED"
                counts["FAILED"] += 1
            else:
                scope_status = "UNVERIFIED"
                counts["UNVERIFIED"] += 1
            writer.writerow([source, kani_status, verus_status, scope_status])
    return counts


def write_summary(counts: dict[str, int], total: int, issues: list[str]) -> None:
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary:
        return
    gate = (
        "OPEN"
        if counts["PROVEN"] + counts["NOT_APPLICABLE"] == total
        and not counts["PLANNED"]
        and not counts["UNMAPPED"]
        and not counts["FAILED"]
        and not counts["UNVERIFIED"]
        and not issues
        else "CLOSED"
    )
    with Path(summary).open("a", encoding="utf-8") as handle:
        handle.write("### Dual-verifier source proof results\n\n")
        handle.write("| Metric | Result |\n|---|---:|\n")
        handle.write(f"| Tracked Rust source files | {total} |\n")
        handle.write(f"| Proof scopes declared IMPLEMENTED | {counts['IMPLEMENTED']} |\n")
        handle.write(f"| Kani successful source proofs | {counts['KANI_PROVEN']} |\n")
        handle.write(f"| Verus successful source proofs | {counts['VERUS_PROVEN']} |\n")
        handle.write(f"| Proven by both Kani and Verus | {counts['PROVEN']} |\n")
        handle.write(f"| Failed by at least one verifier | {counts['FAILED']} |\n")
        handle.write(f"| Awaiting one or both verifier results | {counts['UNVERIFIED']} |\n")
        handle.write(f"| Formally NOT_APPLICABLE with documented reason | {counts['NOT_APPLICABLE']} |\n")
        handle.write(f"| Proof scopes still PLANNED | {counts['PLANNED']} |\n")
        handle.write(f"| Sources without a valid mapping | {counts['UNMAPPED']} |\n")
        handle.write(f"| Manifest / ownership issues | {len(issues)} |\n")
        handle.write(f"| Formal coverage gate | **{gate}** |\n\n")
        handle.write(
            "PROVEN requires successful Kani and Verus results for the same "
            "tracked source. One verifier cannot substitute for the other. "
            "IMPLEMENTED is a declaration that proof entrypoints exist; it is "
            "not proof evidence. NOT_APPLICABLE entries are never counted as "
            "PROVEN and require a documented tool-scope limitation. PLANNED and "
            "missing results keep the gate closed.\n\n"
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
            if status not in {"PLANNED", "IMPLEMENTED", "NOT_APPLICABLE"}:
                issues.append(f"Manifest line {line}: invalid scope status {status!r}.")
            if status == "NOT_APPLICABLE":
                if kani or verus:
                    issues.append(
                        f"Manifest line {line}: NOT_APPLICABLE scopes must not declare verifier entrypoints."
                    )
                if (
                    "not applicable" not in kani_obligations.lower()
                    or "not applicable" not in verus_obligations.lower()
                ):
                    issues.append(
                        f"Manifest line {line}: NOT_APPLICABLE requires separate Kani and Verus limitation statements."
                    )
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
    if counts["PLANNED"]:
        issues.append(
            f"{counts['PLANNED']} tracked Rust source proof scopes remain PLANNED."
        )
    if counts["UNMAPPED"]:
        issues.append(f"{counts['UNMAPPED']} tracked Rust source files are unmapped.")
    if counts["FAILED"]:
        issues.append(f"{counts['FAILED']} source scopes failed at least one formal verifier.")
    if counts["UNVERIFIED"]:
        issues.append(
            f"{counts['UNVERIFIED']} implemented source scopes lack successful results "
            "from both Kani and Verus."
        )
    write_summary(counts, len(sources), issues)

    print(
        f"Audited {len(sources)} tracked Rust sources: "
        f"{counts['PROVEN']} proven by both tools, "
        f"{counts['FAILED']} failed, {counts['UNVERIFIED']} unverified, "
        f"{counts['NOT_APPLICABLE']} not applicable, {counts['PLANNED']} planned, "
        f"{counts['UNMAPPED']} unmapped."
    )
    if issues:
        for issue in issues[:200]:
            print(f"::error::{issue}", file=sys.stderr)
        return 1

    print(
        "Every applicable tracked Rust source is proven by both Kani and Verus; "
        "non-applicable files have explicit tool-scope rationales. "
        "No manifest declaration alone is treated as proof evidence."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
