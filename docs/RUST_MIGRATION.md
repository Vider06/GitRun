# Rust migration contract

The Rust workspace is additive. The existing Python autoscaler remains authoritative until each responsibility has a tested replacement.

Migration order:
1. configuration and state;
2. runner lifecycle;
3. updater and recovery (release artifacts, checksum verification, dependency compatibility, version-pinned runner images, rollback);
4. CLI delegation;
5. manager API;
6. dashboard integration.

The repository can still be deployed through the existing Python/Docker path while the Rust core is introduced incrementally.

## Status

- `gitrun-scheduler` (crate, binary `gitrun-autoscaler`): a from-scratch Rust
  reimplementation of `autoscaler/gitrun_manager.py`'s reconcile loop and
  `autoscaler/gitrun_updater_utility.py`'s GTUU rotation. Not "1:1", built as
  fix-and-improve: pure/testable reconcile planning (`reconcile.rs`, 14 unit
  tests, zero I/O), GitHub API client with real rate-limit backoff and full
  pagination (`github.rs`), GTUU with a real bug fix (the Python `token()`
  helper never returned its value), and a lock that releases on `Drop` so a
  hard crash mid-run can't leave a stale lock behind.
- **Not yet wired into `docker-compose.yml` or systemd.** `gitrun-manager`
  (the Python path) remains the one actually running in the compose file.
  `gitrun-autoscaler` builds and passes its test suite but has not been run
  against a live GitHub/Docker environment yet — validate it standalone
  (`GITHUB_TOKEN=... GITRUN_CONFIG_FILE=... ./gitrun-autoscaler`) before
  pointing `docker-compose.yml` at it.
- Known behavioral difference to be aware of before cutting over: GTUU's
  daily update-time check in the Rust version compares against UTC (computed
  manually to avoid adding a `chrono` dependency), while the Python version
  used the host's local time. If the host isn't UTC, adjust
  `GITRUN_CONTAINER_UPDATE_TIME` accordingly or flag this for a follow-up fix.
