#!/usr/bin/env python3
"""Static contracts for musl Desktop AppImages and APK upgrade script wiring."""
from __future__ import annotations

import re
import subprocess
import sys
import textwrap
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def main() -> int:
    errors: list[str] = []
    release = read(".github/workflows/release.yml")
    ci = read(".github/workflows/ci.yml")
    workflow = read(".github/workflows/dashboard-musl-appimage.yml")
    packager = read("scripts/ci/prepare-musl-dashboard-appdir.sh")
    validator = read("scripts/ci/check-musl-dashboard-appdir.py")
    docs = read("docs/DESKTOP_MUSL_APPIMAGE.md")

    if 'cat > "$RUNNER_TEMP/nfpm.yaml" <<NFPM' not in release:
        errors.append("release workflow server nFPM configuration was not found")
    else:
        source = release.split('cat > "$RUNNER_TEMP/nfpm.yaml" <<NFPM', 1)[1].split(
            "\n          NFPM", 1
        )[0]
        config = textwrap.dedent(source)
        if not re.search(r"(?m)^apk:\n  scripts:\n    postupgrade: packaging/apk-post-upgrade$", config):
            errors.append("APK postupgrade must be nested under the APK-specific nFPM section")
        override = re.search(r"(?ms)^overrides:\n(.*)", config)
        if not override or not re.search(r"(?m)^  apk:\n    depends: \[musl\]$", override.group(1)):
            errors.append("APK musl dependency must remain under overrides.apk")
        if override and re.search(r"(?m)^  apk:\n    depends: \[musl\]\n    scripts:", override.group(1)):
            errors.append("APK postupgrade must not be nested under overrides.apk.scripts")

    if "python3 scripts/ci/check-desktop-appimage-contracts.py" not in ci:
        errors.append("CI release-contracts job does not run the Desktop AppImage contract")

    workflow_markers = (
        "pull_request:",
        "workflow_dispatch:",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "alpine",
        "void",
        'APPIMAGETOOL_VERSION: "0.3.2"',
        'cargo install --locked --version "$APPIMAGETOOL_VERSION" appimagetool',
        "env -u GITHUB_REPOSITORY",
        "actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9",
    )
    for marker in workflow_markers:
        if marker not in workflow:
            errors.append(f"musl AppImage workflow is missing {marker!r}")

    packager_markers = (
        'SHARUN_VERSION="v0.8.1"',
        'SHARUN_COMMIT="b1ef744989533d8b1b4327b647fb315a42765ab9"',
        "18d970f56eca2c527ffd3993b161b6bc340055129db14b394a77cb67d8bbfff9",
        "36a0d16e9d6085e0f11a3e3f76727a0a14aee44ef8e626a76a99555f4da541dc",
        "--with-sharun",
        "--with-hooks",
        "shared/lib/lib.path",
        'export SHARUN_DIR="$HERE"',
    )
    for marker in packager_markers:
        if marker not in packager:
            errors.append(f"AppDir packager is missing {marker!r}")
    for marker in ("libc.so.6", "ld-linux", "ld-musl-", "readelf", "GitRun.desktop"):
        if marker not in validator:
            errors.append(f"AppDir validator is missing {marker!r}")

    for marker in (
        "experimental",
        "not uploaded to the official release",
        "AppImageLauncher",
        "signed release manifest",
        "FUSE",
        "user namespace",
        "WebKitGTK",
    ):
        if marker.lower() not in docs.lower():
            errors.append(f"Desktop musl AppImage docs are missing {marker!r}")

    if "dashboard-musl-appimage" in release:
        errors.append("experimental musl AppImages must not enter official release assets")

    shell = subprocess.run(
        ["bash", "-n", "scripts/ci/prepare-musl-dashboard-appdir.sh"],
        cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
    )
    if shell.returncode:
        errors.append(f"AppDir packager shell syntax failed: {shell.stderr.strip()}")

    if errors:
        print("Desktop AppImage and APK packaging contracts: FAIL", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print("Desktop AppImage and APK packaging contracts: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
