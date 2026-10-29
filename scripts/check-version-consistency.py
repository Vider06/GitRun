#!/usr/bin/env python3
"""Validate GitRun's product-version metadata and independent Cargo crate versions."""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STABLE_VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")


def fail(message: str) -> None:
    raise ValueError(message)


def read_toml(path: Path) -> dict:
    with path.open("rb") as stream:
        return tomllib.load(stream)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--cargo-only",
        action="store_true",
        help="validate Cargo manifests and Cargo.lock without checking release-generated frontend metadata",
    )
    args = parser.parse_args()

    try:
        product_version = (ROOT / "version.txt").read_text(encoding="utf-8").strip()
        if not STABLE_VERSION.fullmatch(product_version):
            fail(f"version.txt must contain one stable MAJOR.MINOR.PATCH version, got {product_version!r}")

        workspace = read_toml(ROOT / "Cargo.toml")
        workspace_version = workspace.get("workspace", {}).get("package", {}).get("version")
        lock = read_toml(ROOT / "Cargo.lock")
        locked = {}
        for package in lock.get("package", []):
            locked.setdefault(package["name"], set()).add(package["version"])

        checked = 0
        errors = []
        for manifest_path in sorted((ROOT / "crates").rglob("Cargo.toml")):
            manifest = read_toml(manifest_path)
            package = manifest.get("package")
            if not isinstance(package, dict):
                continue
            name = package.get("name")
            version = package.get("version")
            if version == {"workspace": True}:
                version = workspace_version
            if not isinstance(name, str) or not isinstance(version, str):
                errors.append(f"{manifest_path.relative_to(ROOT)}: package name/version is not an explicit string or workspace version")
                continue
            checked += 1
            if version not in locked.get(name, set()):
                errors.append(
                    f"{manifest_path.relative_to(ROOT)}: {name} {version} is absent from Cargo.lock"
                )

        if errors:
            fail("Cargo manifest/lock version mismatches:\n  " + "\n  ".join(errors))

        if not args.cargo_only:
            dashboard = ROOT / "crates/gitrun-dashboard-tauri"
            package = json.loads((dashboard / "package.json").read_text(encoding="utf-8"))
            lock_json = json.loads((dashboard / "package-lock.json").read_text(encoding="utf-8"))
            tauri = json.loads((dashboard / "src-tauri/tauri.conf.json").read_text(encoding="utf-8"))
            values = {
                "package.json": package.get("version"),
                "package-lock.json": lock_json.get("version"),
                "package-lock.json root package": lock_json.get("packages", {}).get("", {}).get("version"),
                "tauri.conf.json": tauri.get("version"),
            }
            mismatches = [f"{name}={value!r} (expected {product_version})"
                          for name, value in values.items() if value != product_version]
            if mismatches:
                fail("Release product-version metadata is inconsistent:\n  " + "\n  ".join(mismatches))

        print(
            f"Version consistency: PASS ({checked} Cargo packages; "
            f"{'Cargo-only' if args.cargo_only else 'product metadata and Cargo'}; product {product_version})"
        )
        return 0
    except (OSError, ValueError, KeyError, TypeError, json.JSONDecodeError, tomllib.TOMLDecodeError) as error:
        print(f"check-version-consistency: ERROR: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
