# GitRun architecture — GitRun <!-- gitrun-version:start -->1.8.6<!-- gitrun-version:end -->

> This is the high-level architecture overview for the <!-- gitrun-version:start -->1.8.6<!-- gitrun-version:end --> source line. The [release history](RELEASE_HISTORY.md) records the post-1.5.0 recovery and historical verification boundaries; current trust boundaries and deployment limits are documented in this file and the [security model](SECURITY_MODEL.md).

GitRun is a Rust-native control plane. The workspace separates policy, execution, orchestration, security, secrets, setup and presentation so a workflow-facing API cannot silently become an arbitrary host command interface.

## Runtime components

The installed Linux service runs the Rust scheduler through the `gitrun` executable. The retired Python autoscaler and legacy egui dashboard are not part of the current runtime; the operator dashboard is the Tauri 2 application. The Rust service does not require a separate Python manager to remain synchronized.

| Component | Responsibility |
|---|---|
| `gitrun-core` | Shared configuration, policy, authentication, state and domain models |
| `gitrun-cli` | Operator CLI, keyboard-driven headless management console, and shared terminal mascot/presentation layer |
| `gitrun-scheduler` | GitHub polling, deterministic reconciliation, autoscaling, runner lifecycle, Docker and VM backends |
| `gitrun-exe` | Internal authorized execution model and IPC/authentication primitives |
| `gitrun-gsr` | Security enforcement, watchdog, workflow API authorization and API launcher |
| `gitrun-smart-scheduler` | Optional bounded statistical workload model, Erlang-C queue-wait estimates, offline FCFS replay, Future Mapping and Think. New capabilities are disabled by default; the model has no execution authority. |
| `gitrun-vault` | AES-256-GCM encrypted secret storage |
| `gitrun-setup` | First-run preflight and installation preparation |
| `gitrun-updater` | Release update, verification and rollback |
| `gitrun-recovery` | Startup/repair diagnostics and recovery UI |
| `gitrun-dashboard-tauri` | Tauri operator dashboard |

## Workflow API flow

1. A workflow invokes one of the explicit `Git*Run` executables.
2. The launcher builds an `ApiInvocation` from the command grammar and GitHub Actions environment.
3. The launcher sends a newline-delimited JSON request through the local Unix socket.
4. The host-side GitRun/GSR API boundary verifies the peer/container identity and resolves the authoritative in-progress GitHub workflow/job.
5. Repository API policy, resource policy and GSR command policy are applied.
6. GitDockRun operations additionally require the exact persisted repository/run/job/runner/container binding and immutable Docker container ID.
7. Only an authorized request becomes an `AuthorizedOperation`.
8. `gitrun-exe` dispatches the explicit operation to its handler/backend.
9. Results are returned as structured execution events.

The public API deliberately has no arbitrary shell-operation variant.

## Runner backends

The scheduler can resolve a dynamic runner to:

- the local Linux Docker daemon; or
- a named VM Docker daemon.

VM routing is driven by Logic Containers rules matching GitHub job labels. VM resolution runs asynchronously so a KVM/VirtualBox decision never stalls reconciliation for unrelated repositories.

## Persistent state

Important shared state lives under `GITRUN_STATE_DIR`:

- scheduler health/crash state;
- VM definitions in `vm-configs.json`;
- Logic Containers state;
- GitDockRun bindings;
- GSR security events and queues;
- updater/recovery state.

Some setup, health and crash records are intentionally root-owned and mode `0600`. An unprivileged recovery UI can receive `PermissionDenied` when reading them; that alone is not evidence of corruption or incomplete setup. The repair path treats this as protected state and only migrates the validated legacy files/directories described in the [release history](RELEASE_HISTORY.md).

The Tauri dashboard and scheduler intentionally share file formats for VM and Logic Containers configuration.

## Security boundaries

The major trust boundaries are:

1. GitHub workflow code -> runner container.
2. Runner API launcher -> host Unix socket.
3. API request -> authenticated peer/container identity.
4. Identity -> authoritative GitHub workflow/run/job lookup.
5. API request -> GSR authorization and repository/resource policy.
6. Authorized operation -> internal executor.
7. Executor -> Docker host or VM Docker daemon.
8. GitVault ciphertext -> master/root key.

Runner caches default to per-runner isolation. Repository/global scopes are explicit broader trust decisions. Managed Linux runners use a dedicated GitRun Docker network; the network itself is a segregation point for deployment firewall/proxy policy, not an egress firewall.

The Docker socket remains a host-level privilege boundary on Linux. See [SECURITY_MODEL.md](SECURITY_MODEL.md).


## Verification and deployment boundaries

The PR #83 source-head checks are historical evidence for the 1.5.0 refactor commit, not certification of every later change. PR [#103](https://github.com/Vider06/GitRun/pull/103) added protected-state reporting and guarded legacy runtime/systemd repair; see the [release history](RELEASE_HISTORY.md) and check the exact CI results for the commit being deployed. PR [#109](https://github.com/Vider06/GitRun/pull/109) adds a fixed-purpose privileged read for protected GitRun security settings when the desktop user receives a real permission-denied error; it does not weaken state-file permissions. Live desktop `pkexec` transitions, repair on the actual legacy host, real KVM/VirtualBox provisioning/recovery, the full authorized disconnect API path, and customized/pre-existing runner-image migration still require environment-specific validation.


## Future Mapping decision boundary

Future Mapping projects the next hour into twelve five-minute buckets, with an operator-bounded resource envelope and a monotonic map revision. Think compares distinct lookback windows and blocks predictive growth when coverage is incomplete or demand estimates disagree. These features are advisory: the scheduler re-checks the exact current revision before accepting GSR authorization, and deterministic reconciliation, runner maxima, host budgets and resource-pressure backpressure still control the actual operation.
