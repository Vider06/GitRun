# `systemd`

Linux service units. `gitrun.service` runs the Rust scheduler; `gitrun-gsr.service` keeps the GSR watchdog separate so it can survive a scheduler crash.

The recovery Repair path validates the trusted `/etc/systemd/system` directory and restores the packaged unit contracts for both services, including hardening directives. It then reloads systemd and enables/restarts the scheduler and GSR unit. A unit with a plausible `ExecStart` but altered security directives is not considered equivalent to the packaged service.

If repair rejects a customized unit, compare it to the versioned files in this directory instead of weakening the contract or copying arbitrary settings. See [the GitRun 1.5.1 recovery notes](../docs/v1.5.1-release-notes.md).
