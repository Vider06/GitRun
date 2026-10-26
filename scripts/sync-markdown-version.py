#!/usr/bin/env python3
"""Synchronize displayed current-source version numbers in GitHub Markdown.

Normal Markdown on GitHub does not expand environment variables. Live source
version numbers are therefore wrapped in invisible HTML-comment markers, and
this script replaces only the text inside those markers. Release history and
other historical version references are intentionally left unchanged.

Usage:
  python3 scripts/sync-markdown-version.py           # write version.txt into markers
  python3 scripts/sync-markdown-version.py --check   # fail if markers drift
  python3 scripts/sync-markdown-version.py --version 1.8.4
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

START = "<!-- gitrun-version:start -->"
END = "<!-- gitrun-version:end -->"
MARKER = re.compile(r"<!--\s*gitrun-version:start\s*-->(.*?)<!--\s*gitrun-version:end\s*-->", re.DOTALL)
VERSION_PATTERN = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
SKIP_DIRS = {".git", ".gitrun-ci", "node_modules", "target", "vendor"}

# These are the public documents that intentionally describe the *current*
# source line. Count checks stop a future edit from silently dropping a marker.
REQUIRED_MARKERS = {
    "README.md": 1,
    "crates/gitrun-dashboard-tauri/README.md": 1,
    "docs/ARCHITECTURE.md": 2,
    "docs/GITRUN_API.md": 1,
    "docs/IPC.md": 1,
    "docs/README.md": 1,
    "docs/RELEASE_HISTORY.md": 1,
    "docs/SECURITY_MODEL.md": 1,
    "docs/SMART_SCHEDULER.md": 3,
    "docs/VM.md": 1,
    "release/README.md": 1,
    "scripts/README.md": 1,
}


def repository_root() -> Path:
    return Path(__file__).resolve().parent.parent


def resolve_version(root: Path, argument: str | None) -> str:
    version = argument if argument is not None else (root / "version.txt").read_text(encoding="utf-8").strip()
    if not VERSION_PATTERN.fullmatch(version):
        raise ValueError(f"expected stable product version in MAJOR.MINOR.PATCH form, got {version!r}")
    return version


def markdown_files(root: Path) -> list[Path]:
    found: list[Path] = []
    for path in root.rglob("*.md"):
        if any(part in SKIP_DIRS for part in path.relative_to(root).parts):
            continue
        if path.is_file():
            found.append(path)
    return sorted(found)


def markers_in(text: str, path: Path) -> list[str]:
    matches = list(MARKER.finditer(text))
    starts = text.count(START)
    ends = text.count(END)
    if starts != len(matches) or ends != len(matches):
        raise ValueError(
            f"{path}: unpaired or malformed version markers "
            f"(start={starts}, end={ends}, complete={len(matches)})"
        )
    return [match.group(1) for match in matches]


def check_coverage(root: Path, by_path: dict[str, list[str]]) -> None:
    errors = []
    for relative, expected in REQUIRED_MARKERS.items():
        actual = len(by_path.get(relative, []))
        if actual != expected:
            errors.append(f"{relative}: expected {expected} current-version marker(s), found {actual}")
    if errors:
        raise ValueError("current-version marker coverage is incomplete:\n  " + "\n  ".join(errors))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", help="version to write (defaults to version.txt)")
    parser.add_argument("--check", action="store_true", help="check markers without modifying files")
    args = parser.parse_args()

    root = repository_root()
    try:
        version = resolve_version(root, args.version)
        files = markdown_files(root)
        by_path: dict[str, list[str]] = {}
        contents: dict[Path, str] = {}
        for path in files:
            text = path.read_text(encoding="utf-8")
            contents[path] = text
            by_path[path.relative_to(root).as_posix()] = markers_in(text, path.relative_to(root))
        check_coverage(root, by_path)

        drift = []
        changed = []
        for path, text in contents.items():
            values = by_path[path.relative_to(root).as_posix()]
            if any(value != version for value in values):
                drift.append((path, values))
            if args.check:
                continue
            updated, count = MARKER.subn(lambda match: START + version + END, text)
            if count and updated != text:
                path.write_text(updated, encoding="utf-8", newline="")
                changed.append(path.relative_to(root).as_posix())

        if args.check:
            if drift:
                for path, values in drift:
                    print(f"STALE {path.relative_to(root)}: {values!r}; expected {version}", file=sys.stderr)
                return 1
            print(f"Markdown source-version markers: PASS ({sum(len(v) for v in by_path.values())} markers, version {version})")
            return 0

        # Re-read after changes so the same validation also guards write mode.
        for path in files:
            if path.exists():
                markers_in(path.read_text(encoding="utf-8"), path.relative_to(root))
        print(f"Markdown source-version markers: {version} ({len(changed)} file(s) updated)")
        for relative in changed:
            print(f"  updated {relative}")
        return 0
    except (OSError, ValueError) as error:
        print(f"sync-markdown-version: ERROR: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
