# Linux platform compatibility and release boundaries

This document describes the Linux capabilities in the 1.9.1 source line and distinguishes host detection, build/package production, and validated installation/runtime support. These are separate guarantees: recognizing a distribution or producing a package does not prove that every runner workload, updater path or desktop feature is supported there.

## Host detection

The `gitrun-platform` crate provides shared, conservative Linux host detection:

- Distribution identity from `/etc/os-release`.
- CPU architecture and GNU/glibc versus musl libc ABI.
- Package-manager discovery with distribution-aware preference for APT, DNF, Zypper, Pacman, APK, XBPS and Portage.
- Active init-system detection for systemd, OpenRC and runit. The presence of an installed command alone is not treated as proof that the init system is active.
- Android runtime detection, including native Android, Termux and Android-based proot environments.

Run `gitrun platform` to inspect the detected operating system, distribution, architecture, ABI, Rust target identity, package manager and service manager. This is a diagnostic report; it does not certify that every GitRun feature or workload is supported on the detected host.

Linux target selection recognizes exactly these four architecture/ABI pairs:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`

Unknown libc ABIs and unsupported architectures fail closed. GNU and musl are distinct targets and must never be relabelled as one another. Cross-target checks validate the detection library, not a complete runtime on every target.

The Android guard runs before CLI internal dispatch and at the standalone scheduler, GSR executables/API, Recovery and Dashboard entry points. Android, Termux and Android-based proot are unsupported. An ordinary Linux proot environment is not rejected merely because a `PROOT_*` variable exists; Android SDK variables such as `ANDROID_HOME` alone are not treated as proof of an Android host.

## Server and Desktop architecture

The management backend extraction is implemented in this source line:

- `gitrun-management` owns management reads/writes, policy validation, persistence, typed action dispatch and the privileged Control Manager Unix-domain-socket protocol/service loop.
- Its default feature set is headless. The CLI and terminal management console use it without linking Tauri or GTK/WebKit.
- `gitrun-dashboard-tauri` is a thin Tauri adapter and enables the backend's optional `desktop` integration.
- `gitrun dashboard` launches the separately packaged Dashboard executable; the CLI itself does not link the Dashboard crate.
- Recovery's engine is headless by default; the graphical Recovery interface is behind its optional `desktop` feature.

The 1.9.1 release workflow builds Server and Desktop as separate components. The Desktop package does not bundle the Server executable.

## Release workflow outputs

The Server build matrix covers GNU/musl and x86_64/aarch64. The workflow packages canonical build outputs rather than compiling Rust again for every package format.

The Server package set includes:

- DEB and RPM for x86_64 and aarch64 GNU.
- Arch packages for x86_64 and aarch64 GNU.
- APK for x86_64 and aarch64 musl.
- XBPS for x86_64 and aarch64, GNU and musl.
- TAR.GZ updater archives and TAR.ZST portable archives for all four build targets.

The independent Desktop build matrix also covers GNU/musl and x86_64/aarch64. GNU targets produce AppImage, DEB, RPM and Arch packages. Musl targets produce standalone binaries and XBPS packages for both architectures; the APK is published for x86_64 only. These are workflow-defined outputs; check the signed manifest and the actual [GitHub Release assets](https://github.com/Vider06/GitRun/releases) for a particular published version. A source branch, successful workflow run or expected filename is not evidence that an asset has been published.

For 1.9.1 only, the release workflow also built a one-time Debian migration package for existing 1.8.23 x86_64 GNU/APT installations. It retained the legacy `gitrun` package identity so the older updater could install the new Server binary. It is not the fresh-install Server package and is not repeated in later releases. The new CLI prefers the `gitrun-server` package target and retains a compatibility fallback for older signed manifests.

## Native service-manager integration

The Server packages and setup flow now have distinct service definitions for the supported Linux init systems:

- **systemd:** GNU DEB/RPM/Arch packages carry the scheduler, GSR watchdog and Control Manager units. Setup installs, enables and starts the applicable units.
- **OpenRC:** Alpine APK packages carry the scheduler and GSR service definitions plus the shared service launcher. Both long-running processes use OpenRC's `supervise-daemon` with bounded respawn settings and file logging; the log directory is prepared before service startup. Setup installs the definitions, registers them in the default runlevel and starts them. APK removal stops and unregisters GitRun services; APK upgrade restarts only services that were already active.
- **runit:** Void XBPS packages carry the scheduler and GSR service trees, log-run scripts, shared service launcher, and XBPS `INSTALL`/`REMOVE` lifecycle hooks. Setup creates the `/var/service` links and starts the services. An XBPS upgrade restarts only services that were already running; package removal stops the services and removes only the GitRun supervision links created by setup.

Package installation alone does not enable or start GitRun. Run `gitrun setup` after the first Server package installation to configure the host services and runtime. Package lifecycle hooks preserve configuration and state; use the explicit `gitrun uninstall` operation when destructive GitRun cleanup is intended.

The privileged Control Manager service remains systemd-specific in this source line. OpenRC and runit provide the scheduler and GSR watchdog service lifecycle; they do not install a separate `gitrun-control.service` unit.

## What package production does not guarantee

The package and service-manager matrix is broader than the currently validated host lifecycle:

- The automatic `gitrun reinstall` package-replacement path currently supports only x86_64 GNU/Linux with APT. On other hosts, use the distribution's native package manager for package installation and replacement.
- The built-in signed archive update path and native package-manager update path are distinct operations. `gitrun update` does not replace the installed distro package.
- The current Docker runner profiles and images retain Debian/x86_64-specific assumptions. Building an ARM64 or musl Server binary does not make the runner image multi-architecture or musl-compatible.
- The Control Manager privileged service is currently systemd-specific; do not infer its availability from scheduler/GSR OpenRC or runit support.
- Package availability does not imply that every package has been installed and exercised on every distribution release. Validate dependency resolution, package ownership, upgrade/removal behavior, service startup, permissions and rollback on the target host.
- Android, Termux and Android-based proot remain explicitly unsupported.

Do not advertise a distribution/architecture as fully supported for every workload until its package installation, upgrade, removal, service lifecycle, updater, recovery and representative runner workload have been tested on that target.

## Follow-up work

- Validate the native service lifecycle on representative Alpine/OpenRC and Void/runit installations, including interrupted upgrades and repeated setup/uninstall operations.
- Generalize automatic package reinstall beyond the currently supported Linux x86_64 APT path.
- Remove Debian/x86_64 assumptions from runner profiles and build/publish compatible multi-architecture and musl runner images where intended.
- Extend the privileged Control Manager service beyond systemd if that capability is required on OpenRC or runit hosts.
- Add explicit GUI-only uninstall/reinstall commands if independent Desktop component lifecycle management is required; keep Server services, runner state and operator configuration untouched by those operations.
- Add further Desktop targets only after frontend/runtime dependencies and target packaging are validated.

Until these runtime and lifecycle checks are complete, report detected host capabilities and available release assets separately from fully validated deployment targets.
