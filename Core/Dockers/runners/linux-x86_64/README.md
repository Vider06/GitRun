# Linux x86_64 runner profiles

All official profiles are expected to include GitSecureRun (GSR), the protected GSR policy, and the GSR shell entrypoint. Do not treat a Dockerfile as an official selectable profile unless it is listed in `profiles.json` at the same immutable commit.

- `minimum/Dockerfile`: runner essentials and GSR; no Rust toolchain, desktop/GUI development libraries, PowerShell, or Docker CLI preinstalled.
- `workbench/Dockerfile`: the existing full runner toolset, including Rust, Docker CLI/Buildx/Compose, PowerShell, and GUI/build dependencies.

The build context is the GitRun application workspace root. Both profiles use `docker/runner/entrypoint.sh` and compile the GSR agent/API from the pinned workspace sources. The `Dockerfile` in this directory remains the compatibility path used by older setup clients.
