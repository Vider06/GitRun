# Linux musl Desktop AppImage experiment

## Status

The musl Desktop AppImage pipeline is experimental. Its workflow creates short-lived GitHub Actions artifacts for validation; these files are not uploaded to the official release, signed release manifest, or updater channel.

The supported GNU AppImages and the existing native musl Desktop binaries and packages remain unchanged. Do not describe these experimental files as generally supported or install them through the signed updater.

## Why a separate pipeline

An AppImage runtime and its application payload have independent compatibility requirements. A static runtime can remove the host glibc/FUSE dependency from the launcher, but it does not convert a glibc-linked application or its GTK/WebKit dependencies into musl binaries.

GitRun's Tauri Dashboard is therefore built inside the target musl distribution. The AppDir is assembled there from the target binary and the target distribution's runtime libraries. Only the final AppImage assembly is performed on the matching native GNU runner.

## Build matrix

The experiment builds all four combinations so that the library payload from one musl distribution is not assumed to be interchangeable with the other:

| Architecture | Build environment | AppImage artifact suffix |
| --- | --- | --- |
| x86_64 | Alpine Linux 3.24 / musl | x86_64-alpine-musl |
| aarch64 | Alpine Linux 3.24 / musl | aarch64-alpine-musl |
| x86_64 | Void Linux musl | x86_64-void-musl |
| aarch64 | Void Linux musl | aarch64-void-musl |

Arch Linux is a GNU/glibc target and continues to use the existing GNU AppImage. This experiment does not create an Arch-musl build.

## Packaging components

- Tauri and WebKitGTK are compiled natively in Alpine or Void, not cross-compiled against a mismatched sysroot.
- sharun v0.8.1 is fetched from immutable GitHub release asset URLs. Its x86_64 and aarch64 binaries are checked against published SHA-256 digests.
- lib4bin is fetched from the pinned VHSgunzo/sharun commit b1ef744989533d8b1b4327b647fb315a42765ab9. It collects the target ELF dependency closure and GTK/WebKit hooks. The packaging step deliberately does not patch PT_INTERP.
- The Rust appimagetool 0.3.2 crate assembles the AppDir using its pinned uruntime and DwarFS helpers. It is distinct from the official C++ AppImage appimagetool.
- The AppImage builder runs on the native GNU runner for the same architecture, so its mkdwarfs host executable is not run inside the musl container.
- GITHUB_REPOSITORY is unset during AppImage assembly to prevent automatic update metadata pointing at an artifact that has not been published as a release asset.

lib4bin --with-hooks stages WebKitGTK helper executables and additional runtime data recognized by sharun. That does not prove every dlopen dependency or optional plugin has been collected. The workflow statically checks every staged ELF for architecture and glibc interpreter/DT_NEEDED references.

## Runtime boundary

The AppImage contains application-side musl libraries, GTK/WebKit libraries and helper processes discovered by the packaging pass. Host integration is still required for facilities that cannot safely be made universal:

- the kernel and its syscall/namespace policy;
- the display server and its X11 or Wayland socket;
- the host graphics driver stack, EGL/GBM/DRM and GPU-specific modules;
- D-Bus services, desktop portals and PolicyKit;
- audio devices and optional PipeWire/GStreamer integration;
- fonts, input methods and other user-level desktop services where the host supplies them.

The pipeline must not copy host GPU kernel drivers or blindly package all of /usr/lib. The artifact must be tested on clean systems with the supported display and graphics paths.

The uruntime runtime can use FUSE, user namespaces, or an extraction fallback depending on host capabilities. This is not a security sandbox. Do not enable privileged/setuid workarounds merely to make a test pass. Failures in locked-down containers must be recorded with the exact runtime mode and host policy.

The uruntime-based AppImage is a Type 2-style AppImage but is not guaranteed to work with every AppImage integration tool. AppImageLauncher, Gear Lever, update tools, desktop integration and zsync behavior require explicit compatibility tests before promotion. Successful self-extraction alone does not prove AppImageLauncher integration.

## What the workflow proves

The workflow builds four native musl AppDirs, checks ELF architecture and libc linkage, assembles four experimental AppImages, extracts each image and revalidates the extracted AppDir. It does not run the GUI in a real Alpine or Void desktop session. In particular, WebKit helper execution, GPU acceleration, D-Bus and desktop portal integration still require manual testing on clean desktop hosts.

## Local and host validation

Run the dedicated workflow with workflow_dispatch or open a pull request touching the workflow, packager, validator or Dashboard sources. Each job uploads its own short-lived AppImage, SHA-256 file and build metadata.

On clean Alpine and Void musl desktops:

1. Verify the SHA-256 file before execution.
2. Make the AppImage executable and run it as an ordinary unprivileged user.
3. Test with FUSE available and unavailable.
4. Test with user namespaces allowed and blocked; record the selected fallback instead of relaxing host security.
5. Exercise Wayland and X11 where available, GTK file dialogs, tray integration, D-Bus/PolicyKit actions, font rendering, WebKit helper processes, GPU acceleration and suspend/resume.
6. Repeat on x86_64 and aarch64 and on both Alpine and Void. A build on one distribution is not evidence of runtime compatibility with the other.

Useful diagnostics include file, readelf -h -lW -dW, scanelf, strace -f, APPIMAGE_EXTRACT_AND_RUN=1 and the runtime's --appimage-extract option. Do not use ldd on untrusted downloaded binaries; the build-time ldd check is restricted to the Dashboard binary just built from this repository.

## Promotion gates

Do not move these artifacts into the official release matrix until all of the following are true:

- all four native build and packaging jobs pass repeatedly;
- clean Alpine and Void systems launch the AppImage on both architectures;
- FUSE, namespace and extraction fallback behavior is exercised without weakening host security;
- WebKitGTK helper processes, file pickers, tray, D-Bus, portals and graphics have been tested;
- AppImageLauncher/Gear Lever compatibility and the custom runtime impact are documented;
- release filenames, checksums, signatures and the signed updater manifest have an explicit musl-specific contract.

Until then, official releases continue publishing the existing standalone musl Dashboard binaries and native APK/XBPS packages.
