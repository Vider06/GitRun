# `gitrun-cli`

The primary operator command-line interface for setup, repository connection, configuration, diagnostics, updates, dashboard launch, rollback and terminal utilities. `gitrun manage` adds a styled, keyboard-driven management console for interactive hosts and SSH sessions. The mascot is shown for ordinary CLI invocations by default; pass `--no-cat` to suppress it explicitly. Internal child-process launches use `--no-cat` so the mascot does not corrupt a parent's TUI or command output.

## Common commands

```text
gitrun setup
gitrun setup --terminal
gitrun connect OWNER/REPOSITORY
gitrun config
gitrun doctor
gitrun update --check
gitrun update --force
gitrun manage
gitrun dashboard
gitrun rollback BACKUP_PATH
gitrun cat
```

## Update safeguards

```text
gitrun update --force [MANIFEST_URL]
```

The explicit `--force` option skips only the post-update `doctor` gate when that check is a known false negative. Signed-manifest verification, artifact SHA-256 verification, executable validation, systemd reconciliation and scheduler restart checks still run. If those other steps fail, the update can still fail or roll back. The option conflicts with `--check` and `--only-containers` and does not force a downgrade.

## Fast Stop

```text
gitrun fast-stop <owner/repo> <workflow-run-id> --yes
```

This requests GitHub's force-cancellation endpoint for an entire workflow run, not an individual job. The repository must already be configured, and `--yes` is required to prevent accidental cancellation. A successful response means GitHub accepted the request; it does not prove every runner process has exited. The command does not kill or remove Docker containers locally; normal scheduler reconciliation owns runner cleanup.

The CLI also contains the fixed-purpose privileged helper used by the dashboard to read protected GitRun settings after a real permission-denied result. That helper is not a generic file-reading interface. See the [v1.6.2 fixes](../../docs/v1.6.2-fixes.md) and [security model](../../docs/SECURITY_MODEL.md).

For feature usage, start with the [main README](../../README.md); for workflow-facing APIs, see the [Git*Run API reference](../../docs/GITRUN_API.md).

## Terminal management console

```text
gitrun manage
```

The console is intended for Debian/Ubuntu Server and other hosts administered through a real terminal or SSH TTY. It does not open a browser or require a desktop session. It provides navigation for live overview, repository details, runners and scheduler, Kitty and host resources, privacy/security, GitVault, GitSecureRun, logs, recovery, global configuration, VM definitions, Logic Containers and service controls.

Keyboard controls: use the arrow keys or `j`/`k` to move, `Tab` to change focus, `Enter` to run the selected operation, `r` to refresh, `Esc` to go back, `?` for help and `q` or `Ctrl+C` to exit. Number shortcuts jump to sections.

Complex configuration documents can be opened in `$VISUAL` or `$EDITOR` and are saved only through the same validated backend handlers used by the dashboard. Sensitive fields are redacted in the editor and restored from the original in-memory configuration when left redacted. GitVault values are never read back; newly entered values are accepted only when terminal echo can be disabled. Operations that can interrupt workloads or remove configuration require confirmation.

The App Settings section also exposes the dashboard's nine security-baseline checks. They report saved configuration and one process-name observation; a passing result is not end-to-end proof of runtime enforcement.

The console requires an interactive terminal. For scripts, pipes and automation, continue to use the direct CLI commands and their explicit flags instead of driving the TUI.

### Visual feedback and mascot

The console uses a low-contrast dark surface, rounded panel borders, explicit foreground colors and a restrained background for the selected row. The Settings section uses a text-oriented `≡` symbol rather than a gear whose emoji/text presentation can vary by terminal and font.

On terminals at least 110 columns wide and 24 rows tall, a small **GitRun** mascot panel appears below the section list. It reuses validated ASCII sprites from the shared `src/cat/cat_states.json` catalogue. Errors map to `failed`, active work to `working`, and successful operations to `success`. When no recent operation status takes precedence, the mascot uses the selected area's existing state sprite. The mascot is status feedback, not an independent health check or a source of operational data. On narrower or shorter terminals it is hidden so navigation remains readable.

In the **Logs** section, press `c` to copy the complete loaded Logs output—including lines outside the currently visible scrolled viewport—through the terminal's OSC 52 clipboard sequence. The footer shows `c Copy logs` only in that section, and the same shortcut is listed in the `?` help screen. Clipboard support and permission depend on the local terminal and any SSH/tmux configuration; GitRun sends a clipboard-write request but cannot confirm that the client accepted it. To keep the request bounded across different terminals, output larger than 64 KiB copies only the newest 64 KiB. This writes the current visible log data to the system clipboard; it does not read the clipboard.


## JSON integrity and recovery

`gitrun doctor` keeps its diagnostic report in normal terminal scrollback and inspects JSON files individually in the discovered configuration, state and Main Library directories. Invalid JSON, inaccessible paths and unsafe file types are reported with the affected path instead of being silently treated as healthy.

The recovery tools in `gitrun manage` and the Recovery UI expose three separate operations:

1. **Try to repair JSON files** — removes a byte-order mark or trailing commas and normalizes indentation when the file can be parsed safely. If parsing remains ambiguous or the top-level shape disagrees with the sibling `.json.Back` reference, the file is left unchanged.
2. **Restore JSON from .Back** — explicitly overwrites active JSON files from available sibling backup copies. Deleted files can be recreated when a valid backup is present; this action requires confirmation because newer settings may be lost.
3. **Restore Main Library** — downloads the library archive described by the signed release manifest, verifies the artifact checksum and restores the reference files inside the dedicated library directory. It does not replace active configuration files.

Setup and updater create missing local `.json.Back` snapshots without replacing existing copies. Release builds publish the official `library.zip` with reference files only; normal setup/update extraction fills missing Main Library entries without overwriting existing ones.

## CPU history

The Overview now draws a host CPU utilization trend from real Linux resource samples, refreshed periodically and bounded to the latest 60 samples in memory. The Management TUI can display a CPU sparkline when the backend returns that history. The graph is host-wide utilization, not per-job CPU usage; the history is currently process-local and resets when GitRun restarts.
