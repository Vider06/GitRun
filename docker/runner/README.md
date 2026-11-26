# Runner image

GitRun-managed GitHub Actions runner image. It contains the Actions runner environment and the tooling expected by GitRun workflows, including Docker compatibility where configured.

The PR #83 source-head CI passed the canonical runner image build and build-time entrypoint smoke checks; these results apply to the 1.5.0 source HEAD. Upgrades from pre-existing or operator-customized images/control volumes remain deployment-specific and need migration tests on the target installation. The guarded host runtime migration added by PR #103 does not recursively adopt runner-image or control-volume contents. See the [consolidated release history](../../docs/RELEASE_HISTORY.md).

## Build toolchains

The runner image uses digest-pinned official Debian Bookworm images: Rust `1.100.0` for the builder stage that compiles the GSR agent and API, and Node.js `26.13.1` for the runtime stage. Keep each image tag and its SHA-256 digest synchronized in `Dockerfile` when updating either toolchain.

<!-- Documentation refresh marker: 2026-11-26 -->
