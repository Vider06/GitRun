# `gitrun-setup`

First-run setup and host preflight. It validates configuration, required paths/dependencies and installation safety before the scheduler/dashboard is started.

## Legacy runtime migration

The setup library provides the guarded permission migration used by the recovery Repair path. On Linux it requires root, validates `/etc/gitrun` and the config file, rejects symlinks/hard links and unexpected owners, and limits automatic migration of a legacy desktop-user-owned install to the canonical `/var/lib/gitrun` state directory.

It may repair the state-directory entry and the validated environment/settings files only. It does not recursively `chown` descendants, loosen root-only setup/health/crash records, or delete state. Noncanonical paths and ownership ambiguity fail closed.

See [the consolidated release history](../../docs/RELEASE_HISTORY.md) for the historical migration behavior; current protected-settings dashboard behavior is documented in [consolidated release history](../../docs/RELEASE_HISTORY.md).
