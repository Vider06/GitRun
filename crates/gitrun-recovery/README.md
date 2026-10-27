# `gitrun-recovery`

Recovery and health-diagnostic component for detecting broken GitRun installations and preparing safe repair/startup actions. It also contains the recovery Tauri UI.

## Protected state and repair

The unprivileged recovery UI distinguishes inaccessible root-only state from broken state. A `PermissionDenied` result when reading the root-owned, mode-`0600` setup marker, health record or last-crash record is not treated as proof of incomplete setup or corruption.

The privileged repair path validates the trusted configuration and systemd directories, then invokes the guarded legacy-runtime migration in `gitrun-setup`. That migration is limited to known files and the canonical `/var/lib/gitrun` state directory; it does not recursively change ownership or delete health, crash, Vault, backup, update or arbitrary registry data. Repair restores the packaged `gitrun.service` and `gitrun-gsr.service` contracts, including their hardening settings.

## JSON integrity and restoration

The Recovery UI separates three operations so routine repair does not silently replace configuration:

- **Try to repair JSON files** applies conservative formatting/normalization repairs, including a leading byte-order mark and trailing commas, only when the result is unambiguous. It leaves uncertain files unchanged rather than guessing missing values or structure.
- **Restore JSON from .Back** explicitly restores active JSON files from available sibling `.json.Back` copies. It can recreate deleted files when a valid backup exists and requires confirmation because newer configuration may be lost.
- **Restore Main Library** downloads the reference archive described by the signed release manifest and checks its artifact checksum, member paths, JSON structure and extraction limits before restoring reference files in the dedicated library directory. It does not replace active operational configuration.

Routine setup and update flows create only missing local `.json.Back` backups and fill missing Main Library entries; they do not replace existing backups or reference files. The official `library.zip` contains the selected reference JSON files as `.json.Back` entries, not live secrets or an export of the operator's active configuration.

Doctor checks discovered JSON files individually and reports path-specific malformed content, inaccessible files and safety-limit failures. A permission-denied read of protected root-owned state is not treated as corruption. Protected system settings may be read through the existing fixed-purpose Control Manager path after a real permission-denied result; this does not weaken file permissions.

For operator usage and CPU-history caveats, see the [CLI recovery guide](../gitrun-cli/README.md#json-integrity-and-recovery) and [dashboard guide](../gitrun-dashboard-tauri/README.md#host-cpu-history).

See [the 1.6.2 protected-settings fix](../../docs/v1.6.2-fixes.md) for the current dashboard read path and [the consolidated release history](../../docs/RELEASE_HISTORY.md) for the earlier legacy runtime migration. The exact CI status is commit-specific, and repair should also be verified on the target host after deployment.
