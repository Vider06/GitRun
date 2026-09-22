# Contributing

Thanks for contributing to GitRun.

## Before you start

Please read SECURITY.md and docs/SECURITY_MODEL.md for the security boundary around self-hosted runners and Docker, and docs/RUST_MIGRATION.md for the current migration contract.

Do not include tokens, passwords, private keys, runtime .env files, logs containing secrets, crash dumps, or private infrastructure details in commits or issues.

## Development

GitRun currently contains both Python/Docker and Rust components. The Python/Docker manager remains the deployment-compatible control plane during the Rust migration.

Before opening a pull request, the preferred full validation is:

~~~bash
python3 scripts/test-gitrun.py
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
~~~

For shell changes also run bash -n and shellcheck on the changed scripts.

For Docker changes, build the affected image locally when practical.

## Pull requests

Keep PRs focused and explain:

1. what changed;
2. why it changed;
3. how it was validated;
4. whether the change affects security, runner privileges, credentials, filesystem mounts, networking, or compatibility.

Update documentation when user-visible behavior or configuration changes.

Do not merge known-broken CI into main.

## Commits

Use clear imperative commit messages and avoid credentials or personal infrastructure details in commit messages.

Prefer small, reviewable commits over unrelated cleanups.

## Public repository hygiene

Before adding a new file, check that it contains no credentials, personal addresses, private hostnames, access tokens, or generated state.

Runtime configuration should use environment variables or secret-management systems rather than hardcoded values.
