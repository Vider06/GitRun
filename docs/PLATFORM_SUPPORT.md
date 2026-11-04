# Linux platform compatibility and release boundaries

This document describes the Linux capabilities present in the 1.9.0 source tree and distinguishes host detection, build/package production, and validated installation/runtime support. These are separate guarantees: recognizing a distribution or producing a package for it does not prove that every installation, service lifecycle, updater or runner workload is supported there.

## Host detection

The `gitrun-platform` crate provides shared, conservative Linux host detection:

- Distribution identity from `/etc/os-release`.
- CPU architecture and GNU/glibc versus musl libc ABI.
- Package-manager discovery with distribution-aware preference for APT, DNF, Zypper, Pacman, APK, XBPS and Portage.
- Active init-system detection for systemd, OpenRC and runit. The presence of an installed command alone is not treated as proof that the init system is active.
- Android runtime detection, including native Android, Termux and Android-based proot environments.

Run `gitrun platform` to inspect the detected operating system, distribution, architecture, ABI, Rust target identity, package manager and service manager. This is a diagnostic report; it does not certify that GitRun can install or manage services on the detected host.

Linux target selection recognizes exactly these four architecture/ABI pairs:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`

Unknown libc ABIs and unsupported architectures fail closed. GNU and musl are distinct targets and must never be relabelled as one another. The platform crate's cross-target checks validate the detection library, not every GitRun executable or a complete runtime on those targets.

The Android guard runs before CLI internal dispatch and at the standalone scheduler, GSR executables/API, Recovery and Dashboard entry points. Android, Termux and Android-based proot are unsupported. An ordinary Linux proot environment is not rejected merely because a `PROOT_*` variable exists; Android SDK variables such as `ANDROID_HOME` alone are not treated as proof of an Android host.

## Server and Desktop architecture

The management backend extraction is implemented in this source line:

- `gitrun-management` owns management reads/writes, policy validation, persistence, typed action dispatch and the privileged Control Manager Unix-domain-socket protocol/service loop.
- Its default feature set is headless. The CLI, terminal management console and system Control Manager use it without linking Tauri or GTK/WebKit.
- `gitrun-dashboard-tauri` is a thin Tauri adapter and enables the backend's optional `desktop` integration. It no longer owns a duplicate management backend or Control Manager implementation.
- `gitrun dashboard` launches the separately packaged Dashboard executable; the CLI itself does not link the Dashboard crate.
- Recovery's engine is headless by default; the graphical Recovery interface is behind its optional `desktop` feature.

The 1.9.0 release workflow builds Server and Desktop as separate components. The Desktop package does not bundle the Server executable. This packaging boundary does not yet imply that GUI-only uninstall/reinstall lifecycle commands exist: the CLI's current uninstall/reinstall commands operate on the supported system installation, not on a separately selected GUI component.

## Release workflow outputs

The 1.9.0 release workflow defines a four-target Server build matrix for the GNU/musl and x86_64/aarch64 combinations listed above. It packages the canonical build outputs rather than compiling Rust again for every package format.

The workflow's Server package set includes:

- DEB for x86_64 and aarch64 GNU.
- RPM for x86_64 and aarch64 GNU.
- APK for x86_64 and aarch64 musl.
- Arch package for x86_64 GNU.
- XBPS packages for x86_64 and aarch64, GNU and musl.
- TAR.GZ and TAR.ZST Server archives for all four build targets.

The independent Desktop build is x86_64 Linux and produces an AppImage plus a Debian package. These are workflow-defined outputs; check the signed manifest and the actual [GitHub Release assets](https://github.com/Vider06/GitRun/releases) for a particular published version. A source branch, successful workflow run or expected filename is not evidence that an asset has been published.

For 1.9.0 only, the release workflow also builds a one-time Debian migration package for existing 1.8.23 installations. It retains the legacy `gitrun` package identity so the older updater can install the new Server binary. It is not the fresh-install Server package. After migration, the new Server package identity is used by subsequent installation/reinstall flows.

## What package production does not guarantee

The release workflow's target/package matrix is broader than the currently validated host lifecycle:

- The platform crate detects systemd, OpenRC and runit, but detection is not an OpenRC/runit service backend. The installed service lifecycle remains systemd-oriented.
- Setup, full uninstall/reinstall and update flows must not be described as universally supported merely because a package can be assembled. The current automatic release reinstall path explicitly targets Linux x86_64; other host/architecture combinations need their own implementation and target validation.
- The current Docker runner profiles and images retain Debian/x86_64-specific assumptions. Building an ARM64 or musl Server binary does not make the runner image multi-architecture or musl-compatible.
- Package dependency metadata, lifecycle hooks, upgrade/removal behavior, permissions, service startup and rollback need validation on the corresponding distribution.
- The Desktop build/package is currently x86_64 Linux; the Server target matrix does not imply ARM64 or musl Desktop support.
- Android, Termux and Android-based proot remain explicitly unsupported.

Do not advertise a distribution/architecture as fully supported until its package installation, upgrade, removal, service lifecycle, updater, recovery and representative runner workload have been tested on that target.

## Follow-up work

- Implement and validate native OpenRC and runit service backends before claiming those init systems are supported for managed installations.
- Generalize setup, updater and lifecycle operations beyond the currently supported Linux x86_64 system path.
- Validate all Server package formats and target architectures on their respective distributions, including dependency names and package ownership transitions.
- Remove Debian/x86_64 assumptions from runner profiles and build/publish compatible multi-architecture and musl runner images where intended.
- Add explicit GUI-only uninstall/reinstall commands if independent Desktop component lifecycle management is required; keep Server services, runner state and operator configuration untouched by those operations.
- Add Desktop builds for further architectures only after the frontend/runtime dependencies and target packaging are validated.

Until these runtime and lifecycle checks are complete, report detected host capabilities and available release assets separately from fully supported deployment targets.
