# Contributing

## Development

GitRun is currently a small Python/Docker control plane. The Rust rewrite is planned after the current release is stabilized.

Before opening a pull request:

1. Keep changes focused.
2. Run `python3 scripts/test-gitrun.py`.
3. Run shell syntax checks for changed shell scripts.
4. Do not commit tokens, credentials, local configuration, logs, or generated state.
5. Update documentation when behavior or configuration changes.

## Pull requests

Explain what changed and why. Include any relevant compatibility or migration notes.

The main branch is kept stable; larger changes should be developed in a branch and reviewed before merging.
