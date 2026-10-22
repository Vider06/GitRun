# Contributing

## Development

GitRun is primarily a Rust project with a self-contained runner manager, including the core, scheduler, dashboard, GitHub integration, security hardening, recovery, updater, setup, and packaging components. Python and Docker tooling remain part of the repository where applicable.

Before opening a pull request:

1. Keep changes focused.
2. Run the relevant Rust checks and tests for the components you changed.
3. Run the relevant repository, Docker, shell, packaging, and configuration checks when changing those areas.
4. Run shell syntax checks for changed shell scripts.
5. Validate relevant Docker, packaging, or configuration changes when applicable.
6. Do not commit tokens, credentials, local configuration, logs, or generated state.
7. Update documentation when behavior or configuration changes.

## Pull requests

Explain what changed and why. Include any relevant compatibility or migration notes.

The main branch is kept stable; larger changes should be developed in a branch and reviewed before merging.

Do not merge known-broken CI into main.

## Public repository hygiene

Before adding a new file, check that it contains no credentials, personal addresses, private hostnames, access tokens, or generated state.

Runtime configuration should use environment variables or secret-management systems rather than hardcoded values.


## Documentation structure

Architecture and operator behavior are documented under `docs/`. Major source/configuration directories should have a local `README.md` describing their responsibility. Keep existing directory READMEs aligned when adding, removing or repurposing a directory, and add one when a new directory introduces a distinct component or operational responsibility. Changes to public behavior must update the relevant top-level documentation and detailed architecture/API/security documentation.


## Predictive scheduling changes

Kitty forecasting, queueing, Future Mapping and Think changes must include bounded deterministic tests for stale/missing data, configured maxima and resource-pressure fallback. Offline queue replay must not leak future job durations into an earlier decision. Preserve shadow-mode behavior and the exact-revision GSR authorization path, and update the top-level README, scheduler guide, security model and affected crate README when configuration or behavior changes.
