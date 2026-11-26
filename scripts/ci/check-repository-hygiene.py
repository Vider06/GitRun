#!/usr/bin/env python3
"""Run tracked-file repository hygiene and syntax checks for GitRun CI."""
from __future__ import annotations

import json
import os
import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ANNOTATION_FILE = Path(os.environ.get("CI_ANNOT", "/tmp/gitrun-ci-annotations.log"))
EVENT = os.environ.get("EVENT", "")
MAX_FILE_MB = int(os.environ.get("MAX_FILE_MB", "5"))


def git(*args: str, check: bool = True) -> str:
    result = subprocess.run(
        ("git", *args),
        cwd=ROOT,
        check=check,
        capture_output=True,
        text=True,
        errors="replace",
    )
    return result.stdout


def tracked_files(*patterns: str) -> list[str]:
    return [item for item in git("ls-files", "-z", *patterns).split("\0") if item]


def annotate(level: str, message: str, path: str = "", line: int = 0, title: str = "") -> None:
    fields = []
    if path:
        fields.append(f"file={path.replace('%', '%25').replace(',', '%2C')}")
    if line:
        fields.append(f"line={line}")
    if title:
        fields.append(f"title={title.replace('%', '%25').replace(',', '%2C')}")
    escaped = message.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
    print(f"::{level} {','.join(fields)}::{escaped}")
    if level == "error":
        ANNOTATION_FILE.parent.mkdir(parents=True, exist_ok=True)
        with ANNOTATION_FILE.open("a", encoding="utf-8") as stream:
            location = f"{path}:{line}" if path and line else path
            stream.write(f"• {location} [{title}] {message}\n")


def run_checks() -> int:
    failures = 0

    # Credential-like files must never be tracked. Explicit example/template files
    # and the documented example environment file are safe to keep in the repo.
    secret_re = re.compile(
        r"(^|/)\.env(\.[^/]+)?$|\.(pem|key|p12|pfx|jks|keystore)$|"
        r"(^|/)id_(rsa|dsa|ecdsa|ed25519)$"
    )
    for path in tracked_files():
        if secret_re.search(path) and not re.search(
            r"\.(example|sample|template|dist)$|(^|/)config/config\.example\.env$|\.example\.env$",
            path,
        ):
            annotate("error", "looks like a credential or key file: remove it from git and rotate the secret", path, 1, "tracked secret/key file")
            failures += 1

    conflict = git("grep", "-nIE", "^(<<<<<<<|>>>>>>>) ", "--", ".", ":!*.md", check=False)
    if conflict:
        for item in conflict.splitlines():
            path, _, remainder = item.partition(":")
            line_text, _, _ = remainder.partition(":")
            line = int(line_text) if line_text.isdigit() else 1
            annotate("error", "unresolved merge conflict marker", path, line, "merge conflict marker")
            failures += 1

    eol = git("ls-files", "--eol")
    for item in eol.splitlines():
        if "\t" not in item:
            continue
        metadata, path = item.split("\t", 1)
        if not metadata.startswith("i/crlf"):
            continue
        if re.search(r"\.(sh|ya?ml|env)$|(^|/)Dockerfile[^/]*$|(^|/)Makefile$", path):
            annotate("error", "file is committed with CRLF line endings: convert to LF", path, 1, "CRLF line endings")
            failures += 1

    lowered: dict[str, list[str]] = {}
    for path in tracked_files():
        lowered.setdefault(path.lower(), []).append(path)
    for paths in lowered.values():
        if len(paths) > 1:
            annotate("error", "two tracked paths differ only by case: " + ", ".join(paths), title="case-insensitive name collision")
            failures += 1

    excluded_json = re.compile(r"(^|/)(tsconfig|jsconfig)[^/]*\.json$|(^|/)\.vscode/|devcontainer\.json$")
    for path in tracked_files("*.json"):
        if excluded_json.search(path):
            continue
        try:
            json.loads((ROOT / path).read_text(encoding="utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError) as error:
            line = getattr(error, "lineno", 1)
            annotate("error", str(error).splitlines()[0], path, line, "JSON syntax")
            failures += 1

    try:
        import yaml
    except ImportError:
        annotate("error", "PyYAML is required for YAML syntax validation", title="YAML syntax")
        failures += 1
    else:
        for path in tracked_files("*.yml", "*.yaml"):
            try:
                with (ROOT / path).open(encoding="utf-8") as stream:
                    list(yaml.safe_load_all(stream))
            except yaml.YAMLError as error:
                mark = getattr(error, "problem_mark", None)
                line = mark.line + 1 if mark else 1
                column = mark.column + 1 if mark else 1
                message = (getattr(error, "problem", None) or str(error)).replace("\n", " ")
                annotate("error", message, path, line, f"YAML syntax (column {column})")
                failures += 1

    try:
        import tomllib
    except ImportError:
        print("Python < 3.11: TOML validation skipped")
    else:
        for path in tracked_files("*.toml"):
            try:
                tomllib.loads((ROOT / path).read_text(encoding="utf-8"))
            except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
                line_match = re.search(r"line (\d+)", str(error))
                line = int(line_match.group(1)) if line_match else 1
                annotate("error", str(error).replace("\n", " "), path, line, "TOML syntax")
                failures += 1

    # Diff whitespace is advisory, as are size, executable-bit, and Markdown-link
    # hygiene; preserve those severities from the previous workflow implementation.
    if EVENT == "pull_request":
        diff = git("diff", "--check", "HEAD^1", "HEAD", "--", ".", ":!*.md", check=False)
    elif EVENT == "push" and subprocess.run(
        ("git", "rev-parse", "-q", "--verify", "HEAD^"),
        cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False,
    ).returncode == 0:
        diff = git("diff", "--check", "HEAD^", "HEAD", "--", ".", ":!*.md", check=False)
    else:
        diff = ""
        if EVENT not in ("pull_request", "push"):
            print(f"whitespace check skipped for {EVENT} events")
    for item in diff.splitlines():
        match = re.match(r"(.+?):(\d+):\s*(.*)", item)
        if match:
            annotate("warning", match.group(3), match.group(1), int(match.group(2)), "whitespace")

    max_bytes = MAX_FILE_MB * 1024 * 1024
    for path in tracked_files():
        if re.search(r"(Cargo|package-lock)\.(lock|json)$", path):
            continue
        try:
            size = (ROOT / path).stat().st_size
        except OSError:
            continue
        if size > max_bytes:
            mb = size / (1024 * 1024)
            annotate("warning", f"{mb:.1f} MB tracked in git (limit {MAX_FILE_MB} MB): consider Git LFS or a release asset", path, 1, "large file")

    for item in git("ls-files", "-s", "--", "*.sh").splitlines():
        metadata, _, path = item.partition("\t")
        mode = metadata.split()[0] if metadata else ""
        try:
            first_two = (ROOT / path).read_bytes()[:2]
        except OSError:
            first_two = b""
        if mode != "100755":
            annotate("warning", "shell script is not executable in git (git update-index --chmod=+x)", path, 1, "script not executable")
        if first_two != b"#!":
            annotate("warning", "shell script has no #! line", path, 1, "missing shebang")

    link_re = re.compile(r"\]\(([^)\s]+)\)")
    for markdown in tracked_files("*.md"):
        text = (ROOT / markdown).read_text(encoding="utf-8", errors="replace")
        for match in link_re.finditer(text):
            link = match.group(1)
            target = link.split("#", 1)[0].split("?", 1)[0]
            if not target or target.startswith(("http", "mailto:", "tel:", "/")):
                continue
            if not (ROOT / Path(markdown).parent / target).exists():
                annotate("warning", f"relative link target not found: {link}", markdown, 1, "broken link")

    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(run_checks())
