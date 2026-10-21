# `gitrun-cli`

The primary operator command-line interface for setup, repository connection, configuration, diagnostics, updates, dashboard launch, rollback and terminal utilities.

## Common commands

```text
gitrun setup
gitrun setup --terminal
gitrun connect OWNER/REPOSITORY
gitrun config
gitrun doctor
gitrun update --check
gitrun dashboard
gitrun rollback BACKUP_PATH
gitrun cat
```

## Fast Stop

```text
gitrun fast-stop <owner/repo> <workflow-run-id> --yes
```

This requests GitHub's force-cancellation endpoint for an entire workflow run, not an individual job. The repository must already be configured, and `--yes` is required to prevent accidental cancellation. A successful response means GitHub accepted the request; it does not prove every runner process has exited. The command does not kill or remove Docker containers locally; normal scheduler reconciliation owns runner cleanup.

The CLI also contains the fixed-purpose privileged helper used by the dashboard to read protected GitRun settings after a real permission-denied result. That helper is not a generic file-reading interface. See the [v1.6.2 fixes](../../docs/v1.6.2-fixes.md) and [security model](../../docs/SECURITY_MODEL.md).

For feature usage, start with the [main README](../../README.md); for workflow-facing APIs, see the [Git*Run API reference](../../docs/GITRUN_API.md).
