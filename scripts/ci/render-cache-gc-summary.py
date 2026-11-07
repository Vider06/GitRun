#!/usr/bin/env python3
"""Render the cache-GC Markdown summary from the API inventory and deletion plan."""
import json
import os
import re
import sys
from collections import defaultdict
from pathlib import Path

root = Path(os.environ.get("RUNNER_TEMP", "/tmp"))
caches = json.loads((root / "caches.json").read_text(encoding="utf-8"))
plan = json.loads((root / "gc-plan.json").read_text(encoding="utf-8"))
used_before = int(os.environ.get("USED_BEFORE", "0"))
dry_run = os.environ.get("DRY_RUN", "false").lower() == "true"
mode = "dry run (nothing is deleted)" if dry_run else "deleting"
freed = sum(int(item.get("size_in_bytes", 0)) for item in plan)
lines = [
    "### Cache garbage collection — " + mode,
    "",
    f"- {len(caches)} caches, {used_before // 1048576} MB used of the 10 GB repository limit",
    f"- {len(plan)} to remove, {freed // 1048576} MB to free",
    "",
]
if plan:
    reasons = defaultdict(list)
    for item in plan:
        reasons[str(item.get("reason", "unspecified"))].append(item)
    lines += ["| Reason | Caches | MB |", "|---|---:|---:|"]
    for reason, items in sorted(reasons.items()):
        size = sum(int(item.get("size_in_bytes", 0)) for item in items)
        lines.append(f"| {reason} | {len(items)} | {size // 1048576} |")
    lines.append("")
lines += ["<details><summary>Caches that are kept</summary>", "",
          "| Kind | Ref | Count | MB |", "|---|---|---:|---:|"]
removed = {item.get("id") for item in plan}
kept = defaultdict(list)
for item in caches:
    if item.get("id") in removed:
        continue
    key = re.sub(r"-run[0-9]+-attempt[0-9]+$", "", str(item.get("key", "")))
    if "-target-img" in key:
        family = re.sub(r"-man[^-]+-[^-]+-prof", "-prof", key)
    else:
        family = re.sub(r"-[^-]+$", "", key)
    kept[(family, str(item.get("ref", "")))].append(item)
for (family, ref), items in sorted(kept.items()):
    size = sum(int(item.get("size_in_bytes", 0)) for item in items)
    lines.append(f"| {family} | {ref} | {len(items)} | {size // 1048576} |")
lines += ["", "</details>", ""]
sys.stdout.write("\n".join(lines))
