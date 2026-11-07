#!/usr/bin/env python3
"""Inventory, plan, report, and prune superseded GitHub Actions caches."""
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path

STARTED = time.monotonic()
ROOT = Path(os.environ.get("RUNNER_TEMP", "/tmp"))
STEPS = Path(os.environ.get("CI_STEPS", ROOT / "ci-steps.tsv"))
TOKEN = os.environ["GH_TOKEN"]
REPOSITORY = os.environ["GITHUB_REPOSITORY"]
BASE = "https://api.github.com/repos/" + REPOSITORY + "/actions"
EPOCH = os.environ["CACHE_BASE_EPOCH"]
RUST = os.environ["RUST_VERSION"]
DRY_RUN = os.environ.get("DRY_RUN", "false").lower() == "true"
HEADERS = {
    "Authorization": "Bearer " + TOKEN,
    "Accept": "application/vnd.github+json",
    "X-GitHub-Api-Version": "2022-11-28",
    "User-Agent": "GitRun-cache-garbage-collector",
}

def api(path, method="GET"):
    request = urllib.request.Request(BASE + path, headers=HEADERS, method=method)
    with urllib.request.urlopen(request, timeout=30) as response:
        payload = response.read()
    return json.loads(payload) if payload else {}

def parse_time(value):
    return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()

def cache_family(key):
    key = re.sub(r"-run[0-9]+-attempt[0-9]+$", "", str(key))
    if "-target-img" in key:
        return re.sub(r"-man[^-]+-[^-]+-prof", "-prof", key)
    return re.sub(r"-[^-]+$", "", key)

def make_plan(caches, now):
    annotated = []
    for cache in caches:
        key = str(cache.get("key", ""))
        age = (now - parse_time(cache["created_at"])) / 86400
        reason = None
        if not key.startswith("gitrun-"):
            reason = "legacy key format"
        elif re.match(r"^gitrun-e[0-9]+-", key) and re.match(r"^gitrun-(e[0-9]+)-", key).group(1) != EPOCH:
            reason = "old cache epoch"
        elif re.search(r"-rustup-[0-9]", key) and "-rustup-" + RUST + "-" not in key:
            reason = "toolchain of an old Rust version"
        elif re.search(r"-rustc[0-9]", key) and "-rustc" + RUST + "-" not in key:
            reason = "target built with an old Rust version"
        elif str(cache.get("ref", "")).startswith("refs/pull/") and age > 1:
            reason = "pull-request cache older than 1 day"
        elif re.match(r"^gitrun-e[0-9]+x", key) and age > 1:
            reason = "one-off fresh-cache run"
        annotated.append({**cache, "_family": cache_family(key), "_age": age, "_reason": reason})

    groups = defaultdict(list)
    for cache in annotated:
        if cache["_reason"] is None:
            groups[(cache.get("ref", ""), cache["_family"])].append(cache)
    superseded = []
    for group in groups.values():
        ordered = sorted(group, key=lambda item: item["created_at"], reverse=True)
        for cache in ordered[1:]:
            superseded.append({**cache, "_reason": "superseded by a newer cache of the same kind"})

    candidates = [
        {key: value for key, value in item.items() if not key.startswith("_")}
        for item in annotated if item["_reason"] is not None and item["_age"] > 0.007
    ]
    candidates.extend(
        {key: value for key, value in item.items() if not key.startswith("_")}
        for item in superseded if item["_age"] > 0.007
    )
    candidates.sort(key=lambda item: (item["reason"] if "reason" in item else "", item.get("key", "")))
    # The jq implementation sorts by reason then key after assigning the reason.
    for item in candidates:
        if "reason" not in item:
            source = next(x for x in annotated if x.get("id") == item.get("id"))
            item["reason"] = source["_reason"] or "superseded by a newer cache of the same kind"
    candidates.sort(key=lambda item: (item["reason"], item.get("key", "")))
    return candidates

def render_summary(caches, plan, used_before):
    freed = sum(int(item.get("size_in_bytes", 0)) for item in plan)
    mode = "dry run (nothing is deleted)" if DRY_RUN else "deleting"
    lines = [
        "### Cache garbage collection — " + mode, "",
        f"- {len(caches)} caches, {used_before // 1048576} MB used of the 10 GB repository limit",
        f"- {len(plan)} to remove, {freed // 1048576} MB to free", "",
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
    removed = {item.get("id") for item in plan}
    kept = defaultdict(list)
    for item in caches:
        if item.get("id") not in removed:
            kept[(cache_family(item.get("key", "")), str(item.get("ref", "")))].append(item)
    lines += ["<details><summary>Caches that are kept</summary>", "",
              "| Kind | Ref | Count | MB |", "|---|---|---:|---:|"]
    for (family, ref), items in sorted(kept.items()):
        size = sum(int(item.get("size_in_bytes", 0)) for item in items)
        lines.append(f"| {family} | {ref} | {len(items)} | {size // 1048576} |")
    lines += ["", "</details>", ""]
    return "\n".join(lines)

def main():
    caches = []
    for page in range(1, 31):
        payload = api("/caches?per_page=100&page=" + str(page))
        batch = payload.get("actions_caches", [])
        caches.extend(batch)
        if len(batch) < 100:
            break
    usage = api("/cache/usage")
    used_before = int(usage.get("active_caches_size_in_bytes") or 0)
    plan = make_plan(caches, time.time())

    (ROOT / "caches.json").write_text(json.dumps(caches, indent=2) + "\n", encoding="utf-8")
    (ROOT / "gc-plan.json").write_text(json.dumps(plan, indent=2) + "\n", encoding="utf-8")
    report = render_summary(caches, plan, used_before)
    print(report, end="")

    for item in plan:
        key = str(item.get("key", ""))
        size = int(item.get("size_in_bytes", 0))
        reason = str(item.get("reason", "unspecified"))
        if DRY_RUN:
            print(f"[dry run] would delete {key} ({size // 1048576} MB): {reason}")
            continue
        try:
            api("/caches/" + urllib.parse.quote(str(item["id"]), safe=""), method="DELETE")
            print(f"deleted {key} ({size // 1048576} MB): {reason}")
        except (OSError, urllib.error.URLError, urllib.error.HTTPError) as exc:
            print(f"warning: could not delete cache {key}: {exc}", file=sys.stderr)

    if not DRY_RUN:
        used_after = int((api("/cache/usage").get("active_caches_size_in_bytes") or 0))
        print(f"cache usage: {used_before // 1048576} MB -> {used_after // 1048576} MB")

if __name__ == "__main__":
    status = 0
    try:
        main()
    except Exception as exc:
        status = 1
        print(f"cache garbage collection failed: {exc}", file=sys.stderr)
    finally:
        STEPS.parent.mkdir(parents=True, exist_ok=True)
        with STEPS.open("a", encoding="utf-8") as stream:
            result = "ok" if status == 0 else "fail"
            stream.write(f"Cache inventory and garbage collection\t{result}\t{int(time.monotonic() - STARTED)}\n")
    raise SystemExit(status)
