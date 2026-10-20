# `gitrun-cli/src`

CLI command dispatch and implementation. `main.rs` defines the command surface; `cat.rs` implements the terminal animation; `cat/` contains its single JSON frame source.

## Fast Stop

`gitrun fast-stop <owner/repo> <workflow-run-id> --yes` requests GitHub's force-cancellation endpoint for a workflow run. The repository must already be configured in GitRun. `--yes` is required because the operation cancels **every job in the workflow run**, not an individual job. A successful response means GitHub accepted the request; it does not prove all runner processes have exited. The command does not kill or remove Docker containers locally. The 1.5.1 source/recovery and version metadata changes are summarized in [the release notes](../../../docs/v1.5.1-release-notes.md); check CI against the exact commit before treating a source-tree build as release-verified.
