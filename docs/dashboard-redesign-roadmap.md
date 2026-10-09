# Dashboard redesign roadmap

Status: implementation in progress on \`feat/dashboard-redesign\`.

## Goals

Rebuild the GitRun desktop experience as an operational control plane rather than restyling the existing monolithic page. Keep privileged behavior in the Rust backend, reuse authoritative scheduler/runtime data, and never present configured policy as proof of active enforcement.

## Information architecture

1. **Overview** — compact host-wide status, one card per configured repository, runner inventory, recent workflow-run outcomes, PR counts, stars, host pressure, critical security events, source/freshness and partial-data warnings.
2. **Repository detail** — repository metrics and runner inventory, recent workflow-run summary, Logic Containers rules, GitDockRun requirements, GitVault scope membership and repository-specific execution/API/mount policies. Permanent runners may expose their own settings entry point; dynamic runners must not show permanent-runner controls.
3. **Privacy & Security** — global API capability policy, runner isolation, network/seccomp/AppArmor/rootfs controls, cache scope, resource-pressure thresholds, GSR command/workflow policy, socket exceptions and an evidence-labelled risk map. High-risk changes remain behind explicit confirmation and backend validation.
4. **GitVault** — metadata-only secret listing and explicit global/repository/group scopes. The dashboard must never return plaintext secret values to the webview.
5. **GitSecureRun** — watchdog/process observation, event timeline, optional workflow analyzer status and an honest distinction between process presence and verified runtime enforcement.
6. **Recovery** — diagnostics first, explicit host-repair actions, update outcomes and a terminal-style activity log. A successful command must not hide warnings or partial update failures.
7. **General Settings** — global runner pool limits, scheduler lifecycle, container update/recovery behavior, VM definitions and pending hypervisor decisions, health checks and non-installing GitRun update check.
8. **App Settings** — theme, density, cat visibility/expressions, guided tutorial, support links and confirmed reinstall/uninstall actions.

## Data and refresh contract

| Data | Source | Default reuse window | UI semantics |
|---|---|---:|---|
| Registered GitHub runners and online/busy flags | GitHub Actions runner API | 60 s | Repository-by-repository completeness; unknown is not zero |
| Permanent/dynamic classification and container state | GitRun labels and Docker inventory | Shared runner snapshot, 60 s | Distinguish a GitHub runner with no matching local container from unknown Docker state |
| Latest workflow-run outcome counts | GitHub Actions workflow-runs API, at most 100 runs per configured repository | 60 s | Count workflow runs, not individual jobs |
| Stars, forks, open PRs and merged PRs | GitHub repository metadata and Search API | 300 s | Partial/stale data is disclosed; Search API totals can be capped |
| CPU, memory and critical-filesystem utilization | Existing local scheduler resource-pressure sampler | 15 s | Host-wide only; never label it per-job resource usage |
| Critical GSR events | Local event queue | Read on overview refresh | Empty event queue is not proof that enforcement is healthy |
| Job duration, per-job CPU/memory, queue item identity and cost | Not yet supplied by a complete dashboard contract | Not applicable | Do not fabricate values; add only after an authoritative event/history source exists |

Refresh windows are defaults, not a reason to poll on every component render. Views should reuse snapshots within a process, show sample time and stale state, and avoid duplicating equivalent calls when navigation changes.

## Implementation phases

### Phase A — shell and shared UI primitives
- Replace the previous navigation structure with clear operational, repository, security, recovery and preference areas.
- Extract shared escaping, metric, notice, table, confirmation and toast primitives.
- Preserve keyboard focus, visible loading/error states, responsive layouts and reduced-motion support.
- Keep the cat as a coherent product element with calm, alert and serious states driven only by observed signals.

### Phase B — data contracts and backend aggregation
- Introduce small serializable DTOs for runner/workflow, repository activity and host-resource snapshots.
- Cache and serve snapshots from the Rust side so multiple views share one result.
- Preserve partial failures and timestamps; use stale cache on temporary refresh failure.
- Add explicit non-installing update-check and local health-check commands.
- Validate config/settings writes and restore the previous files if a multi-file save fails.

### Phase C — Overview and repository experience
- Add one operational card per configured repository.
- Show registered runner status and permanent/dynamic/container classification.
- Show recent workflow-run outcomes, stars, PR counts and host resource pressure.
- Separate saved repository policy from live state.
- Expose repository-specific API operations with an explicit Inherit-versus-Custom mode. An empty custom allowlist must remain an explicit deny-all list.
- Add a settings entry point only for permanent runners; per-runner overrides remain unavailable until the backend has a validated persistence contract.

### Phase D — privacy, security and secret scopes
- Consolidate security controls by their actual scope (global versus repository).
- Base risk-map labels on stored settings and explicitly mark runtime verification as pending where no inspector exists.
- Preserve the backend unsafe-runner gate and explicit socket-opt-out confirmation.
- Keep Vault secrets metadata-only, and use the backend's tagged scope representation.

### Phase E — general settings and recovery
- Keep host/global runner configuration separate from visual app preferences.
- Manage VM definitions and respond to pending hypervisor decisions through existing backend commands.
- Put reinstall and uninstall under App Settings, with explicit high-impact confirmations.
- Report GTUU partial failures instead of reducing them to a generic success.
- Preserve a dedicated recovery console with a terminal log and targeted repair actions.

### Phase F — guided onboarding and polish
- Offer the tutorial after initial setup and make it restartable.
- Persist theme, density and mascot preferences locally.
- Ensure mascot status is derived from observed signals; it must not imply that a configuration has been verified.
- Check narrow windows, large repository counts, error and stale-data states, keyboard access and reduced-motion mode.

### Phase G — verification and release readiness
- Run JavaScript syntax/lint checks, Rust formatting, dashboard crate tests, setup/recovery tests and the relevant workspace checks.
- Build the Tauri application and standalone Recovery target.
- Exercise first-run and reinstall flows, GitHub API errors/rate limits, Docker-unavailable states, Vault metadata/scopes, empty and partial runner inventories, stale caches, denied policy operations and rollback failures.
- Inspect the final diff for accidental changes to unrelated files or generated artifacts.
- Keep the pull request draft until validation failures found by CI are resolved. Never merge to \`main\` as part of this task.

## Acceptance criteria

- All navigation destinations render without syntax/runtime errors.
- No empty or failed API query is silently represented as a trustworthy zero.
- Runner and workflow summaries carry timestamps, completeness and stale-state information.
- Security UI does not label configuration as verified runtime enforcement.
- Repository operation overrides make inheritance and explicit-empty policy unambiguous.
- No plaintext secret value is passed back to the UI.
- Global and repository writes are validated by Rust; failed multi-file persistence attempts restore prior content or report rollback failure.
- Reinstall/uninstall and unsafe runner options require explicit confirmation.
- CI is green for the changed dashboard, scheduler API, and recovery code before the PR is marked ready for review.


## Latest implementation notes

- \`gitrun update --check\` now has a read-only CLI path: it verifies the signed release manifest, reports whether a GitRun release is newer, checks the runner image digest, and does not download or install anything.
- A separate, confirmed runner-only action invokes the trusted privileged CLI. It verifies the release's runner image digest, pins the image in configuration, and reconciles permanent runners without replacing GitRun itself. GTUU skips busy runners and lets a later run reconcile them.
- The scheduler reloads local GTUU schedule settings every tick, so changing automatic-update enablement, time or timezone in the dashboard does not require a service restart.
- The Tauri backend has explicit service start/stop/restart commands, a bounded read-only host health report, coordinated config/settings persistence with rollback, and cached runner/workflow/repository/resource snapshots.

These implementation notes describe code that has been authored on this branch; CI and runtime testing are still required before the PR can be considered ready.
