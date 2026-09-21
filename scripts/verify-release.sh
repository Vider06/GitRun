#!/usr/bin/env bash
set -Eeuo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo test --workspace --all-features
bash -n scripts/build-release.sh scripts/install-linux.sh scripts/install-macos.sh scripts/install-server.sh

python3 - <<'PY'
import json, pathlib
schema=json.loads(pathlib.Path("release/release-manifest.schema.json").read_text())
required={"name","version","git_commit","artifacts"}
if schema.get("title") != "GitRun Release Manifest":
    raise SystemExit("release manifest schema title mismatch")
if set(schema.get("required", [])) != required:
    raise SystemExit("release manifest schema required fields mismatch")
if not pathlib.Path(".github/workflows/release.yml").is_file():
    raise SystemExit("release workflow missing")
print("Release metadata and workflow checks passed")
PY

echo "GitRun release verification: PASS"
