# Contributing to GitRun

## Before opening a pull request

1. Keep the change focused and explain the user or operator problem it solves.
2. Run the checks relevant to the files and components changed.
3. Run shell syntax checks for changed shell scripts.
4. Validate relevant Docker, packaging, configuration and platform-specific behavior when practical.
5. Update user-facing and technical documentation when behavior, configuration or security boundaries change.
6. Never commit tokens, credentials, private keys, local configuration, logs, dumps or generated runtime state.

GitRun is primarily a Rust workspace, with Docker, shell, PowerShell, Tauri, packaging and configuration components where applicable. Choose validation for the actual change rather than relying on an unrelated broad check.

## Pull requests

Describe the problem, the approach, important behavior changes, compatibility or migration notes, and any operational follow-up. Include security impact when a change affects credentials, runner privileges, Docker access, filesystem mounts, network exposure, update or recovery paths.

Use a branch and request review before merging into main. Do not claim that a check, manual validation or target-host test passed unless it was actually performed.

## Repository hygiene

Before adding files, check that they contain no credentials, personal addresses, private hostnames, access tokens or generated state. Prefer environment variables and supported secret-management systems over hardcoded secrets.

Keep documentation close to the relevant subsystem. A distinct component or operational directory should have a concise local README when that improves discoverability. Update cross-links when files move, and avoid creating a second document that duplicates an existing maintained reference.

## Predictive scheduling

Kitty forecasting, queueing, Future Mapping and Think changes need bounded deterministic tests for missing or stale data, configured limits and resource-pressure fallback. Offline queue replay must not use future observations or job durations to influence earlier decisions. Preserve shadow-mode behavior and the exact-revision GSR authorization boundary, and update the scheduler guide and affected component documentation when behavior changes.
