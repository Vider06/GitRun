# `gitrun-scheduler`

GitRun's runtime control plane. It reconciles GitHub jobs and runner pools, creates/removes/restarts runner containers, performs recovery, applies Logic Containers routing, resolves VM backends and runs GitHub/update services.

The v1.5.0 CI suite includes runtime-contract, reconciliation, VM-resolution and privileged GitDockRun namespace checks. Do not infer that this covers every real hypervisor lifecycle, arbitrary custom runner-image migration, or the complete authorized API disconnect flow; current evidence and limits are recorded in the [status tracker](../../docs/v1.5.0-refactor-status.md).
