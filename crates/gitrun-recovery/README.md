# `gitrun-recovery`

Recovery and health-diagnostic component for detecting broken GitRun installations and preparing safe repair/startup actions. It also contains the recovery Tauri UI.

## Protected state and repair

The unprivileged recovery UI distinguishes inaccessible root-only state from broken state. A `PermissionDenied` result when reading the root-owned, mode-`0600` setup marker, health record or last-crash record is not treated as proof of incomplete setup or corruption.

The privileged repair path validates the trusted configuration and systemd directories, then invokes the guarded legacy-runtime migration in `gitrun-setup`. That migration is limited to known files and the canonical `/var/lib/gitrun` state directory; it does not recursively change ownership or delete health, crash, Vault, backup, update or arbitrary registry data. Repair restores the packaged `gitrun.service` and `gitrun-gsr.service` contracts, including their hardening settings.

See [the 1.6.2 protected-settings fix](../../docs/v1.6.2-fixes.md) for the current dashboard read path and [the GitRun 1.5.1 recovery notes](../../docs/v1.5.1-release-notes.md) for the earlier legacy runtime migration. The exact CI status is commit-specific, and repair should also be verified on the target host after deployment.
