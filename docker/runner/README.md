# Runner image

GitRun-managed GitHub Actions runner image. It contains the Actions runner environment and the tooling expected by GitRun workflows, including Docker compatibility where configured.

The v1.5.0 CI validates the canonical runner image build and its build-time entrypoint smoke checks. That does not certify upgraded, pre-existing or operator-customized images/control volumes. Validate those migration paths on the target installation and consult the [refactor status tracker](../../docs/v1.5.0-refactor-status.md).
