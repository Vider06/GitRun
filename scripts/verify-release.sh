#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo test --workspace --all-features
git diff --check
bash -n \
  scripts/build-release.sh \
  scripts/install-macos.sh \
  scripts/install-server.sh \
  scripts/verify-release.sh \
  scripts/build-deb.sh

python3 - <<'PY'
import hashlib
import json
import pathlib
import re
import sys
from urllib.parse import urlparse

schema_path = pathlib.Path("release/release-manifest.schema.json")
workflow_path = pathlib.Path(".github/workflows/release.yml")

schema = json.loads(schema_path.read_text(encoding="utf-8"))
required = {"name", "version", "git_commit", "artifacts"}
if schema.get("title") != "GitRun Release Manifest":
    raise SystemExit("release manifest schema title mismatch")
if set(schema.get("required", [])) != required:
    raise SystemExit("release manifest schema required fields mismatch")
if not workflow_path.is_file():
    raise SystemExit("release workflow missing")

def fail(message):
    raise SystemExit(f"release manifest validation failed: {message}")

def validate(value, schema, path="$"):
    if "const" in schema and value != schema["const"]:
        fail(f"{path} must equal {schema['const']!r}")

    expected_type = schema.get("type")
    type_ok = {
        "object": isinstance(value, dict),
        "array": isinstance(value, list),
        "string": isinstance(value, str),
        "integer": isinstance(value, int) and not isinstance(value, bool),
        "number": isinstance(value, (int, float)) and not isinstance(value, bool),
        "boolean": isinstance(value, bool),
    }
    if expected_type in type_ok and not type_ok[expected_type]:
        fail(f"{path} must be a {expected_type}")

    if "required" in schema:
        for key in schema["required"]:
            if key not in value:
                fail(f"{path}.{key} is required")

    if isinstance(value, dict):
        properties = schema.get("properties", {})
        if schema.get("additionalProperties") is False:
            extra = sorted(set(value) - set(properties))
            if extra:
                fail(f"{path} contains unsupported properties: {', '.join(extra)}")
        for key, subschema in properties.items():
            if key in value:
                validate(value[key], subschema, f"{path}.{key}")

    if isinstance(value, list):
        minimum = schema.get("minItems")
        if minimum is not None and len(value) < minimum:
            fail(f"{path} must contain at least {minimum} item(s)")
        item_schema = schema.get("items")
        if item_schema:
            for index, item in enumerate(value):
                validate(item, item_schema, f"{path}[{index}]")

    if isinstance(value, str):
        minimum = schema.get("minLength")
        if minimum is not None and len(value) < minimum:
            fail(f"{path} must contain at least {minimum} character(s)")
        pattern = schema.get("pattern")
        if pattern and re.fullmatch(pattern, value) is None:
            fail(f"{path} does not match {pattern!r}")
        if schema.get("format") == "uri":
            parsed = urlparse(value)
            if not parsed.scheme:
                fail(f"{path} must be a URI")

    for subschema in schema.get("allOf", []):
        validate(value, subschema, path)

    any_of = schema.get("anyOf")
    if any_of and not any(
        try_validate(value, subschema, path) for subschema in any_of
    ):
        fail(f"{path} does not satisfy any allowed schema branch")

def try_validate(value, schema, path):
    try:
        validate(value, schema, path)
        return True
    except SystemExit:
        return False

manifest_candidates = []
if len(sys.argv) > 1:
    manifest_candidates.append(pathlib.Path(sys.argv[1]))
else:
    for candidate in (
        pathlib.Path("dist/release/release-manifest.json"),
        pathlib.Path("release/release-manifest.json"),
    ):
        if candidate.is_file():
            manifest_candidates.append(candidate)
            break

if manifest_candidates:
    manifest_path = manifest_candidates[0]
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    validate(manifest, schema)

    manifest_dir = manifest_path.parent
    for artifact in manifest["artifacts"]:
        file_name = artifact["file"]
        local_artifact = manifest_dir / file_name
        if local_artifact.is_file():
            digest = hashlib.sha256(local_artifact.read_bytes()).hexdigest()
            if digest != artifact["sha256"]:
                fail(
                    f"checksum mismatch for {file_name}: "
                    f"manifest={artifact['sha256']} actual={digest}"
                )
        elif "download_url" not in artifact:
            fail(f"artifact file is missing and has no download_url: {file_name}")

    print(f"Release manifest validation passed: {manifest_path}")
else:
    print("No concrete release manifest found; schema and workflow metadata checks passed.")

print("Release metadata and workflow checks passed")
PY

echo "GitRun release verification: PASS"
