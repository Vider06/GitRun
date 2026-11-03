# Linux platform compatibility and distribution roadmap

This document separates what GitRun can identify from what the current source can install, run and update. A recognized distribution or package manager is not, by itself, a supported deployment target.

## Runtime detection in 1.9.0

The `gitrun-platform` crate provides shared, conservative host detection:

- Distribution identity from `/etc/os-release`.
- CPU architecture and libc ABI (GNU/glibc or musl).
- Package-manager discovery and distro-aware preference for APT, DNF, Zypper, Pacman, APK, XBPS and Portage.
- Active init-system detection for systemd, OpenRC and runit. Installed commands alone are not treated as proof that an init system is running.
- Android runtime detection for native Android, Termux and Android-based proot environments.

Run `gitrun platform` to inspect the detected host. The report is diagnostic only: it does not certify package availability, dependency compatibility, service installation, Docker runner availability or update support.

Unknown libc ABIs and unsupported CPU architectures fail closed when GitRun constructs a Linux update target. GNU and musl are distinct Rust targets; they must never be represented as aliases of one another.

The Android guard runs before the CLI's internal watchdog and Control Manager dispatch, and at the standalone scheduler, GSR, workflow API, recovery and dashboard entry points. It returns exit code 2 with:

```text
GitRun cannot run on Android. Android, Termux and Android-based proot environments are not supported.
```

The guard does not reject an ordinary Linux proot solely because a `PROOT_*` variable is present, and it does not use Android SDK variables such as `ANDROID_HOME` alone as an Android-host signal.

## Current deployment boundaries

The 1.9.0 source foundation does **not** claim that all detected Linux distributions are installable.

| Platform area | Current boundary |
|---|---|
| Debian / Ubuntu | Existing official Linux release path is Debian-package centric and x86_64 GNU. Inspect the current release manifest for published assets. |
| Other GNU distributions | Host identification can recognize common package-manager families, but native package metadata, dependency names, service installation and update paths are not yet generalized. |
| Alpine | An important planned target. APK packaging, musl builds, OpenRC services, dependency handling and runner-image support are not completed by host detection. |
| Void Linux | XBPS and runit are recognized for reporting; native package creation and service integration remain planned. |
| Arch, Fedora, openSUSE, Gentoo | Package-manager families can be detected; package-specific integration and installation/update lifecycle still need implementation and target testing. |
| GNU ARM64 / musl x86_64 / musl ARM64 | The platform layer can identify the corresponding target triples, but this does not mean official artifacts, cross-compilation, runner images or full runtime support have been published. |
| ARMv7 / ARMv6 / i686 / RISC-V / ppc64le / s390x | Not declared supported by this release foundation. |
| Android / Termux / Android proot | Explicitly unsupported. |
| NixOS | Deferred; its declarative package and service model needs a separate integration design. |

The platform-detection crate is checked as a standalone Rust library for all four target triples above. This is a compile-compatibility check for the detection layer only; it is not a cross-build of the GitRun CLI, scheduler, desktop dashboard, runner image or distributable package.

The current Docker runner profiles are Debian-based and x86_64-specific. A host ABI detector cannot make those images multi-architecture or make an image built for glibc work on musl.

## Release and package architecture

The intended release design is one orchestrated release workflow, not multiple independent workflows publishing the same version:

1. Resolve and validate the version and target matrix.
2. Build one canonical binary/component set per supported ABI and architecture target.
3. Collect and verify the canonical build outputs and their manifests.
4. Create native package formats from those outputs without running Cargo again.
5. Validate package contents, dependency metadata, service integration and installation/removal behavior on target environments.
6. Generate checksums and signatures, then publish the complete asset set once.

A package filename must communicate both target identity and package format. For example, `GitRun-1.9.0-gnu-x86_64.deb` and `GitRun-1.9.0-musl-aarch64.apk` distinguish the target ABI/architecture from the package-manager format. These are naming examples, not claims that those assets exist.

The four initial Linux compilation targets under consideration are:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`

Each target must have its own successful build and runtime/package validation before it is included in an official release. A GNU binary cannot be relabelled as musl, nor can ARM64 support be inferred from upstream runner-tool availability.

All GitRun-owned executables, libraries, scripts and service definitions must be included in official GitHub Release artifacts. Declared OS-library dependencies may be supplied by the distribution's repositories; an installer must not fetch GitRun components from third-party hosting or use a separate bootstrap downloader.

Expected native package formats include DEB, RPM, APK, Arch `.pkg.tar.zst` and XBPS packages, with a versioned archive for compatible systems without a native package. Each package needs native dependency metadata and lifecycle hooks. Package creation is not complete until install, upgrade, removal and preservation of operator state have been tested on the corresponding distribution.

## Server and Desktop package boundary

The current CLI uses shared management handlers exported by the Tauri dashboard crate. The dashboard crate also brings Tauri and desktop build dependencies into the dependency graph. Simply omitting the GUI executable from a package would therefore not create a clean Server build.

The required architectural step is to extract the shared management operations, authorization checks and Control Manager protocol/client into a non-Tauri crate (proposed name: `gitrun-management`). The CLI/TUI and Tauri dashboard must consume that shared backend so both interfaces retain identical validation and authorization behavior.

After that extraction, publish separate Server and Desktop package components. This separation is also required for the GUI lifecycle commands below; package managers must not believe that a file still owned by an installed monolithic package has been independently removed.

## GUI lifecycle contract

These commands are a required follow-up to the package split. They are **not implemented by the 1.9.0 platform foundation**.

### `gitrun uninstall --gui`

- Remove only the installed GUI component and its desktop integration: dashboard executable, desktop entries, icons, shortcuts and GUI-only files.
- Preserve the CLI, scheduler, GSR, Control Manager, runners, service configuration, repository registrations, user configuration and persistent state.
- Be idempotent. Repeating the command must be safe.
- Update package-manager ownership consistently; it must not manually delete files still owned by a monolithic package record.

### `gitrun reinstall --gui`

- Determine the installed CLI version and select only a GUI artifact compatible with that exact CLI release.
- Obtain the GUI package from official GitHub Release assets or the official release mechanism.
- Verify the signed release manifest, artifact digest and signature before installation.
- Restore only the GUI component and its desktop integration. Do not reinstall or reset server services, scheduler, runners, configuration, registrations or state.
- Reject a missing or incompatible GUI/CLI version rather than installing an arbitrary latest dashboard.
- Verify the installed GUI component after restoration.

The GUI lifecycle must be exercised on install, repeated uninstall, reinstall, package upgrade and rollback paths while confirming that server state is unchanged.

## Known work still required

The earlier packaging/runtime audit identified these concrete follow-ups:

- Generalize APT/dpkg assumptions in setup, updater and CLI package installation; ensure every declared package manager uses its native metadata and safe invocation. The official Debian release control file now declares its baseline runtime dependencies, while other package formats remain future work.
- Add correct `Depends`/runtime dependency metadata to the Debian package build paths and ensure the release workflow and local package script do not diverge.
- Replace hard-coded systemd-only setup, recovery, doctor, dashboard controls and lifecycle hooks with explicit systemd/OpenRC/runit backends. Ship and validate native service definitions for each supported init system.
- Split the Debian x86_64 runner profiles and Docker build from architecture-specific assumptions; validate Actions runner and PowerShell toolchains per architecture and ABI.
- Complete the shared `gitrun-management` extraction before removing Tauri from Server builds and implementing the GUI lifecycle commands.
- Turn the one-target release workflow into the single orchestrated build/package/validate/sign/publish pipeline described above. Only artifacts with successful target-specific validation may enter a release manifest.

Until these items are implemented and tested, GitRun must report detected platforms without advertising them as fully supported installation targets.
