# GitRun architecture

GitRun is a Rust-native control plane. The workspace separates policy, execution, orchestration, security, secrets, setup and presentation so a workflow-facing API cannot silently become an arbitrary host command interface.

## Runtime components

| Component | Responsibility |
|---|---|
| `gitrun-core` | Shared configuration, policy, authentication, state and domain models |
| `gitrun-cli` | Operator CLI and terminal utilities |
| `gitrun-scheduler` | GitHub polling, reconciliation, autoscaling, runner lifecycle, Docker and VM backends |
| `gitrun-exe` | Internal authorized execution model and IPC/authentication primitives |
| `gitrun-gsr` | Security enforcement, watchdog, workflow API authorization and API launcher |
| `gitrun-vault` | AES-256-GCM encrypted secret storage |
| `gitrun-setup` | First-run preflight and installation preparation |
| `gitrun-updater` | Release update, verification and rollback |
| `gitrun-recovery` | Startup/repair diagnostics and recovery UI |
| `gitrun-dashboard-tauri` | Tauri operator dashboard |
| `gitrun-recovery` | Recovery diagnostics and UI |

## Workflow API flow

1. A workflow invokes one of the explicit `Git*Run` executables.
2. The launcher builds an `ApiInvocation` from the command grammar and GitHub Actions environment.
3. The launcher sends a newline-delimited JSON request through the local Unix socket.
4. The host-side GitRun/GSR API boundary verifies caller identity and evaluates the closed API contract.
5. Repository API policy, resource policy and GSR command policy are applied.
6. Only an authorized request becomes an `AuthorizedOperation`.
7. `gitrun-exe` dispatches the explicit operation to its handler/backend.
8. Results are returned as structured execution events.

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

The Tauri dashboard and scheduler intentionally share file formats for VM and Logic Containers configuration.

## Security boundaries

The major trust boundaries are:

1. GitHub workflow code -> runner container.
2. Runner API launcher -> host Unix socket.
3. API request -> GSR authorization.
4. Authorized operation -> internal executor.
5. Executor -> Docker host or VM Docker daemon.
6. GitVault ciphertext -> master key.

The Docker socket remains a host-level privilege boundary on Linux. See [SECURITY_MODEL.md](SECURITY_MODEL.md).
