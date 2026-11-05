#!/usr/bin/env python3
"""Verify Tauri v2 native build dependencies on representative Linux families.

These are isolated dependency probes, not claims that package installation or
service lifecycle is supported on every distribution. The probes use the
package names documented by Tauri for Debian, Fedora, Arch, openSUSE and Alpine,
plus the Void musl packages used for the ARM64 Desktop build.
"""
from __future__ import annotations

import subprocess
import sys

PROBES = (
    (
        "Debian/Ubuntu (glibc)",
        "ubuntu:24.04",
        "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "
        "pkg-config build-essential curl wget file libxdo-dev libssl-dev "
        "libayatana-appindicator3-dev librsvg2-dev libwebkit2gtk-4.1-dev",
    ),
    (
        "Fedora (glibc)",
        "fedora:latest",
        "dnf install -y pkgconf-pkg-config gcc gcc-c++ make curl wget file openssl-devel "
        "libappindicator-gtk3-devel librsvg2-devel libxdo-devel webkit2gtk4.1-devel",
    ),
    (
        "Arch Linux (glibc)",
        "archlinux:latest",
        "pacman -Syu --noconfirm --needed pkgconf base-devel curl wget file openssl "
        "appmenu-gtk-module libappindicator-gtk3 librsvg xdotool webkit2gtk-4.1",
    ),
    (
        "openSUSE (glibc)",
        "opensuse/leap:latest",
        "zypper --non-interactive refresh && zypper --non-interactive install --no-recommends "
        "pkg-config gcc gcc-c++ make curl wget file libopenssl-devel "
        "libappindicator3-1 librsvg-devel webkit2gtk3-devel",
    ),
    (
        "Alpine Linux (musl, x86_64 APK)",
        "alpine:3.24",
        "apk add --no-cache pkgconf build-base curl wget file openssl "
        "libayatana-appindicator-dev librsvg font-dejavu webkit2gtk-4.1-dev "
        "openssl-libs-static cairo-static harfbuzz-static glib-static wayland-static zlib-static",
    ),
    (
        "Void Linux (musl, XBPS)",
        "ghcr.io/void-linux/void-musl-full:20260801r1",
        "xbps-install -Suy && xbps-install -y pkg-config curl ca-certificates "
        "libwebkit2gtk41-devel libayatana-appindicator-devel librsvg-devel gtk+3-devel openssl-devel",
    ),
)


def main() -> int:
    try:
        subprocess.run(["docker", "info"], check=True, stdout=subprocess.DEVNULL)
    except (FileNotFoundError, subprocess.CalledProcessError):
        print("Dashboard distro dependency probes require a working Docker daemon.", file=sys.stderr)
        return 2

    failed: list[str] = []
    for label, image, install in PROBES:
        print(f"::group::Dashboard native dependencies: {label}", flush=True)
        command = (
            "set -eu; "
            + install
            + "; "
            + "pkg-config --exists webkit2gtk-4.1; "
            + "pkg-config --exists gtk+-3.0; "
            + "pkg-config --exists openssl; "
            + "printf 'WebKitGTK %s\\n' \"$(pkg-config --modversion webkit2gtk-4.1)\""
        )
        result = subprocess.run(
            ["docker", "run", "--rm", "--pull=always", image, "sh", "-ec", command],
            check=False,
        )
        print("::endgroup::", flush=True)
        if result.returncode:
            failed.append(label)
            print(f"FAIL: {label} (exit {result.returncode})", file=sys.stderr)
        else:
            print(f"PASS: {label}")

    if failed:
        print("Dashboard dependency probes failed: " + ", ".join(failed), file=sys.stderr)
        return 1
    print("Dashboard native dependency probes: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
