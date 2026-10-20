# Runner image

GitRun-managed GitHub Actions runner image. It contains the Actions runner environment and the tooling expected by GitRun workflows, including Docker compatibility where configured.

The PR #83 source-head CI passed the canonical runner image build and build-time entrypoint smoke checks. Upgrades from pre-existing or operator-customized images/control volumes are deployment-specific and need a migration test on the target installation. See the [status report](../../docs/v1.5.0-refactor-status.md).
