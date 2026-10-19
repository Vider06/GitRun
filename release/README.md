# `release`

Release metadata for GitRun releases, including the **1.5.0** refactor target.

This directory contains machine-readable release manifests and schemas. Release tooling uses these files to describe artifacts and validate manifest structure.

Official release manifests are signed during the release workflow. Verification is mandatory for official releases; unsigned custom manifests require an explicit compatibility/unsafe opt-out.

The v1.5.0 refactor is tracked by [PR #83](https://github.com/Vider06/GitRun/pull/83). A branch version target is not itself a published release. Only treat a release as ready after the current-head verification and residual-risk disposition in [the status tracker](../docs/v1.5.0-refactor-status.md) are complete.
