# Runner image

GitRun-managed GitHub Actions runner image. It contains the Actions runner environment and the tooling expected by GitRun workflows, including Docker compatibility where configured.

The PR #83 source-head CI passed the canonical runner image build and build-time entrypoint smoke checks; these results apply to the 1.5.0 source HEAD. Upgrades from pre-existing or operator-customized images/control volumes remain deployment-specific and need migration tests on the target installation. The guarded host runtime migration added by PR #103 does not recursively adopt runner-image or control-volume contents. See the [1.5.1 release notes](../../docs/v1.5.1-release-notes.md) and [historical status report](../../docs/v1.5.0-refactor-status.md).
