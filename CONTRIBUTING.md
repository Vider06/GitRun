# Contributing

## Development

GitRun is primarily a Rust project with a self-contained runner manager, including the core, scheduler, dashboard, GitHub integration, security hardening, recovery, updater, setup, and packaging components. Python and Docker tooling remain part of the repository where applicable.

Before opening a pull request:

1. Keep changes focused.
2. Run the relevant Rust checks and tests for the components you changed.
3. Run `python3 scripts/test-gitrun.py` when changing the Python control-plane or its related tooling.
4. Run shell syntax checks for changed shell scripts.
5. Validate relevant Docker, packaging, or configuration changes when applicable.
6. Do not commit tokens, credentials, local configuration, logs, or generated state.
7. Update documentation when behavior or configuration changes.

## Pull requests

Explain what changed and why. Include any relevant compatibility or migration notes.

The main branch is kept stable; larger changes should be developed in a branch and reviewed before merging.
