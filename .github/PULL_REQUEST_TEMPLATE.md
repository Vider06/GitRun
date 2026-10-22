## Summary

<!-- What does this change do? -->

## Why

<!-- Why is this change needed? -->

## Validation

- [ ] I ran the relevant local checks.
- [ ] I updated documentation when behavior or configuration changed.
- [ ] I did not add credentials, tokens, private keys, local config, logs, dumps, or generated state.

## Security impact

<!-- Mention Docker, runner privileges, credentials, filesystem mounts, network exposure, or other security-boundary changes. Write "None" when not applicable. -->

## Notes

<!-- Migration, compatibility, rollout, or operational notes. -->

- [ ] Predictive scheduling changes include tests for missing/stale history, bounds, shadow mode and the GSR authorization boundary.
- [ ] Queue replays do not use future observations or durations to affect earlier decisions.
- [ ] Product version, affected crate SemVer and Cargo.lock are aligned.
