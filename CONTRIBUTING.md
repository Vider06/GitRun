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
