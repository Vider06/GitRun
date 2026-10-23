# `gitrun-cli`

The primary operator command-line interface for setup, repository connection, configuration, diagnostics, updates, dashboard launch, rollback and terminal utilities. `gitrun manage` adds a styled, keyboard-driven management console for interactive hosts and SSH sessions.

## Common commands

```text
gitrun setup
gitrun setup --terminal
gitrun connect OWNER/REPOSITORY
gitrun config
gitrun doctor
gitrun update --check
gitrun manage
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

## Terminal management console

\`\`\`text
gitrun manage
\`\`\`

The console is intended for Debian/Ubuntu Server and other hosts administered through a real terminal or SSH TTY. It does not open a browser or require a desktop session. It provides navigation for live overview, repository details, runners and scheduler, Kitty and host resources, privacy/security, GitVault, GitSecureRun, logs, recovery, global configuration, VM definitions, Logic Containers and service controls.

Keyboard controls: use the arrow keys or \`j\`/\`k\` to move, \`Tab\` to change focus, \`Enter\` to run the selected operation, \`r\` to refresh, \`Esc\` to go back, \`?\` for help and \`q\` or \`Ctrl+C\` to exit. Number shortcuts jump to sections.

Complex configuration documents can be opened in \`$VISUAL\` or \`$EDITOR\` and are saved only through the same validated backend handlers used by the dashboard. Sensitive fields are redacted in the editor and restored from the original in-memory configuration when left redacted. GitVault values are never read back; newly entered values are accepted only when terminal echo can be disabled. Operations that can interrupt workloads or remove configuration require confirmation.

The console requires an interactive terminal. For scripts, pipes and automation, continue to use the direct CLI commands and their explicit flags instead of driving the TUI.
