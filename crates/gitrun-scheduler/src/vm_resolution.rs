//! Wires `vm.rs` (which is fully implemented but was previously never
//! called from anywhere — see the session notes this connects) into the
//! rest of the scheduler, without ever blocking the main reconcile loop.
//!
//! `gitrun-autoscaler`'s poll loop is single-threaded and sequential across
//! repositories (see `main.rs`'s `while` loop): if resolving a VM's
//! hypervisor blocked *there*, one repo waiting on an operator's KVM/
//! VirtualBox decision would stall every other repo's reconciliation for
//! up to five minutes. Instead, resolution happens in its own short-lived
//! background thread, spawned on demand (only one per VM name at a time —
//! see `VmResolutionRegistry`) and left to run its course while the main
//! loop keeps polling everything else normally.
//!
//! Flow, per VM name, the first time it's needed:
//! 1. Try KVM. If it comes up, record the endpoint and selected hypervisor.
//! 2. If KVM fails, write a pending decision (`gitrun_core::hypervisor_decision`)
//!    and block *this thread only*, polling for up to 5 minutes for the
//!    dashboard to answer.
//! 3. Answered → act on the choice (retry KVM, or set up VirtualBox
//!    instead) and record the result.
//! 4. Not answered within 5 minutes → clear the pending decision and record
//!    a failed attempt. The next reconcile cycle retries the VM resolution.
//!
//! While a thread is in flight, reconcile's caller receives a non-ready
//! result immediately and defers creation of the VM-targeted runner for
//! that cycle. It never redirects the job to the local Docker host.

use crate::docker::DockerHost;
use crate::vm::{self, HypervisorKind, VmConfig};
use gitrun_core::hypervisor_decision::{self, DecisionChoice};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DECISION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DECISION_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How long to wait for the VM to report an IP address after starting it,
/// separate from `DECISION_TIMEOUT` (that's for the *operator*, this is
/// for the *VM's boot process* once a hypervisor has been chosen).
const VM_BOOT_TIMEOUT: Duration = Duration::from_secs(120);
const VM_HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub enum VmResolution {
    Resolving {
        config_fingerprint: u64,
    },
    Resolved {
        host: DockerHost,
        hypervisor: HypervisorKind,
        config_fingerprint: u64,
        checked_at: Instant,
    },
    Failed {
        config_fingerprint: u64,
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmResolutionResult {
    Ready(DockerHost),
    Resolving,
    Failed(String),
}

/// Shared across every reconcile cycle (and the background threads it
/// spawns) for the lifetime of the `gitrun-autoscaler` process. A plain
/// `Mutex<HashMap>` rather than anything fancier: entries are written
/// rarely (once per VM, occasionally re-written after a give-up), and read
/// once per reconcile cycle per VM-targeting Logic Containers rule — not a
/// hot path that needs lock-free structures.
pub type VmResolutionRegistry = Arc<Mutex<HashMap<String, VmResolution>>>;

fn lock_registry(
    registry: &VmResolutionRegistry,
) -> std::sync::MutexGuard<'_, HashMap<String, VmResolution>> {
    registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn new_registry() -> VmResolutionRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

fn config_fingerprint(config: &VmConfig) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    let feed = |hash: &mut u64, bytes: &[u8]| {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(0x100000001b3);
        }
        *hash = hash.wrapping_mul(0x100000001b3);
    };

    feed(&mut hash, config.name.as_bytes());
    feed(
        &mut hash,
        &[match config.hypervisor {
            HypervisorKind::Kvm => 1,
            HypervisorKind::VirtualBox => 2,
        }],
    );
    feed(&mut hash, config.base_disk_image.as_bytes());
    feed(&mut hash, &config.memory_mb.to_le_bytes());
    feed(&mut hash, &config.cpus.to_le_bytes());
    feed(&mut hash, &config.docker_port.to_le_bytes());
    feed(
        &mut hash,
        &[match config.activation {
            vm::ActivationMode::Standard => 1,
            vm::ActivationMode::AlwaysOnExperimental => 2,
        }],
    );
    feed(&mut hash, &[u8::from(config.is_windows)]);
    hash
}

/// Called from resolve_backend_and_image on every reconcile cycle that
/// needs a VM-backed runner. Never blocks the caller.
pub fn resolve_or_spawn(
    registry: &VmResolutionRegistry,
    state_dir: &Path,
    vm_config: &VmConfig,
) -> VmResolutionResult {
    if let Err(error) = vm::validate_vm_config(vm_config) {
        return VmResolutionResult::Failed(error.to_string());
    }

    let fingerprint = config_fingerprint(vm_config);
    let work = {
        let mut guard = lock_registry(registry);

        match guard.get(&vm_config.name) {
            Some(VmResolution::Resolved {
                host,
                config_fingerprint: current,
                checked_at,
                ..
            }) if *current == fingerprint && checked_at.elapsed() < VM_HEALTH_CHECK_INTERVAL => {
                return VmResolutionResult::Ready(host.clone());
            }
            Some(VmResolution::Resolved {
                config_fingerprint: current,
                hypervisor,
                ..
            }) if *current == fingerprint => {
                let hypervisor = *hypervisor;
                guard.insert(
                    vm_config.name.clone(),
                    VmResolution::Resolving {
                        config_fingerprint: fingerprint,
                    },
                );
                ResolutionWork::Verify(hypervisor)
            }
            Some(VmResolution::Resolving { .. }) => {
                return VmResolutionResult::Resolving;
            }
            Some(VmResolution::Failed {
                config_fingerprint: current,
                ..
            }) if *current == fingerprint => {
                guard.insert(
                    vm_config.name.clone(),
                    VmResolution::Resolving {
                        config_fingerprint: fingerprint,
                    },
                );
                ResolutionWork::Full
            }
            _ => {
                guard.insert(
                    vm_config.name.clone(),
                    VmResolution::Resolving {
                        config_fingerprint: fingerprint,
                    },
                );
                ResolutionWork::Full
            }
        }
    };

    spawn_resolution_worker(registry, state_dir, vm_config, fingerprint, work)
}

enum ResolutionWork {
    Full,
    Verify(HypervisorKind),
}

fn spawn_resolution_worker(
    registry: &VmResolutionRegistry,
    state_dir: &Path,
    vm_config: &VmConfig,
    fingerprint: u64,
    work: ResolutionWork,
) -> VmResolutionResult {
    let registry_for_thread = Arc::clone(registry);
    let state_dir = state_dir.to_path_buf();
    let vm_config = vm_config.clone();
    let name = vm_config.name.clone();

    let spawned = std::thread::Builder::new()
        .name(format!("gitrun-vm-resolve-{name}"))
        .spawn(move || {
            let outcome = match work {
                ResolutionWork::Full => resolve_blocking(&state_dir, &vm_config),
                ResolutionWork::Verify(kind) => refresh_resolved(&vm_config, kind),
            };
            finish_resolution(&registry_for_thread, &vm_config.name, fingerprint, outcome);
        });

    match spawned {
        Ok(_) => VmResolutionResult::Resolving,
        Err(error) => {
            let message = error.to_string();
            eprintln!(
                "gitrun-autoscaler: failed to spawn VM resolution thread for '{name}': {message}"
            );
            let mut guard = lock_registry(registry);
            if matches!(
                guard.get(&name),
                Some(VmResolution::Resolving { config_fingerprint })
                    if *config_fingerprint == fingerprint
            ) {
                guard.insert(
                    name,
                    VmResolution::Failed {
                        config_fingerprint: fingerprint,
                        error: message.clone(),
                    },
                );
            }
            VmResolutionResult::Failed(message)
        }
    }
}

fn finish_resolution(
    registry: &VmResolutionRegistry,
    name: &str,
    fingerprint: u64,
    outcome: Result<(DockerHost, HypervisorKind), String>,
) {
    let mut guard = lock_registry(registry);
    let owns_slot = matches!(
        guard.get(name),
        Some(VmResolution::Resolving { config_fingerprint })
            if *config_fingerprint == fingerprint
    );
    if !owns_slot {
        return;
    }

    match outcome {
        Ok((host, hypervisor)) => {
            guard.insert(
                name.to_owned(),
                VmResolution::Resolved {
                    host,
                    hypervisor,
                    config_fingerprint: fingerprint,
                    checked_at: Instant::now(),
                },
            );
        }
        Err(error) => {
            eprintln!("gitrun-autoscaler: VM resolution for '{name}' failed: {error}");
            guard.insert(
                name.to_owned(),
                VmResolution::Failed {
                    config_fingerprint: fingerprint,
                    error,
                },
            );
        }
    }
}
/// The actual blocking sequence, run only inside a background thread.
fn resolve_blocking(
    state_dir: &Path,
    vm_config: &VmConfig,
) -> Result<(DockerHost, HypervisorKind), String> {
    match provision_and_wait(HypervisorKind::Kvm, vm_config) {
        Ok(host) => return Ok((host, HypervisorKind::Kvm)),
        Err(error) => {
            println!(
                "gitrun-autoscaler: KVM setup failed for VM '{}' ({error}); asking the operator via the dashboard (retry KVM, or use VirtualBox instead), waiting up to {}s",
                vm_config.name,
                DECISION_TIMEOUT.as_secs()
            );
            hypervisor_decision::request(state_dir, &vm_config.name, &error.to_string()).map_err(
                |write_error| {
                    format!(
                        "could not write hypervisor decision request for '{}': {write_error}",
                        vm_config.name
                    )
                },
            )?;
        }
    }

    let deadline = Instant::now()
        .checked_add(DECISION_TIMEOUT)
        .unwrap_or_else(Instant::now);

    loop {
        if Instant::now() >= deadline {
            hypervisor_decision::clear(state_dir, &vm_config.name).map_err(|error| {
                format!(
                    "operator decision timed out for VM '{}' and clearing the pending decision failed: {error}",
                    vm_config.name
                )
            })?;
            return Err(format!(
                "no operator decision for VM '{}' within {}s",
                vm_config.name,
                DECISION_TIMEOUT.as_secs()
            ));
        }

        match hypervisor_decision::poll(state_dir, &vm_config.name) {
            Ok(Some(record)) => {
                if let Some(choice) = record.choice {
                    hypervisor_decision::clear(state_dir, &vm_config.name).map_err(|error| {
                        format!(
                            "VM '{}' decision was received but could not be cleared: {error}",
                            vm_config.name
                        )
                    })?;
                    let chosen_kind = match choice {
                        DecisionChoice::RetryKvm => HypervisorKind::Kvm,
                        DecisionChoice::UseVirtualBox => HypervisorKind::VirtualBox,
                    };
                    let host = provision_and_wait(chosen_kind, vm_config).map_err(|error| {
                        format!(
                            "{chosen_kind:?} setup for VM '{}' failed after operator decision: {error}",
                            vm_config.name
                        )
                    })?;
                    return Ok((host, chosen_kind));
                }
            }
            Ok(None) => {}
            Err(error) => {
                return Err(format!(
                    "error polling hypervisor decision for VM '{}': {error}",
                    vm_config.name
                ))
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            continue;
        }
        std::thread::sleep(remaining.min(DECISION_POLL_INTERVAL));
    }
}

/// Refreshes a previously resolved VM using the hypervisor already
/// selected for it. This can recover a VM that was stopped or removed
/// externally, while keeping the refresh asynchronous.
fn refresh_resolved(
    vm_config: &VmConfig,
    kind: HypervisorKind,
) -> Result<(DockerHost, HypervisorKind), String> {
    let mut config = vm_config.clone();
    config.hypervisor = kind;
    provision_and_wait(kind, &config)
        .map(|host| (host, kind))
        .map_err(|error| error.to_string())
}
/// Ensures the VM exists and is running under `kind`, waits for it to
/// report an IP, and returns the `DockerHost::Remote` pointed at its
/// Docker daemon.
fn provision_and_wait(kind: HypervisorKind, vm_config: &VmConfig) -> vm::Result<DockerHost> {
    let mut config = vm_config.clone();
    config.hypervisor = kind;
    vm::ensure_vm(&config)?;
    if !vm::is_running(kind, &config.name)? {
        vm::start(kind, &config.name)?;
    }
    let ip = vm::wait_for_ip(kind, &config.name, VM_BOOT_TIMEOUT)?;
    Ok(DockerHost::Remote(vm::docker_host_address(
        kind,
        &ip,
        config.docker_port,
    )))
}

/// Loads VM definitions from `{state_dir}/vm-configs.json` at startup —
/// see `vm::load_vm_configs` for why this file/format instead of
/// something in `Config` (short version: it's the same file the dashboard
/// already reads and writes, so this is what keeps the two in sync). A
/// malformed file is treated the same as any other invalid configuration
/// by `main.rs`'s caller (fail fast at boot) rather than silently starting
/// with zero VMs.
pub fn load_vm_definitions(state_dir: &Path) -> vm::Result<Vec<VmConfig>> {
    vm::load_vm_configs(state_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_vm(name: &str) -> VmConfig {
        VmConfig {
            name: name.to_owned(),
            hypervisor: HypervisorKind::Kvm,
            base_disk_image: "/tmp/does-not-exist.qcow2".into(),
            memory_mb: 2048,
            cpus: 2,
            docker_port: 2376,
            activation: vm::ActivationMode::Standard,
            is_windows: false,
        }
    }

    #[test]
    fn config_fingerprint_changes_when_vm_config_changes() {
        let mut vm = sample_vm("fingerprint");
        let first = config_fingerprint(&vm);
        vm.docker_port += 1;
        assert_ne!(first, config_fingerprint(&vm));
    }

    #[test]
    fn invalid_vm_config_fails_before_spawning() {
        let registry = new_registry();
        let dir = std::env::temp_dir().join(format!(
            "gitrun-vm-resolution-invalid-{}",
            std::process::id()
        ));
        let mut vm = sample_vm("invalid");
        vm.name = "bad/name".into();

        assert!(matches!(
            resolve_or_spawn(&registry, &dir, &vm),
            VmResolutionResult::Failed(_)
        ));
        assert!(registry.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn resolve_or_spawn_returns_resolving_immediately_and_does_not_block() {
        // Hypervisor setup and operator interaction never block the
        // reconcile caller; the worker owns the long-running wait.
        let registry = new_registry();
        let dir =
            std::env::temp_dir().join(format!("gitrun-vm-resolution-test-{}", std::process::id()));
        let started = Instant::now();
        let result = resolve_or_spawn(&registry, &dir, &sample_vm("never-answered"));
        assert!(matches!(result, VmResolutionResult::Resolving));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "resolve_or_spawn must not block the caller"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_or_spawn_does_not_spawn_a_second_thread_while_resolving() {
        let registry = new_registry();
        let dir =
            std::env::temp_dir().join(format!("gitrun-vm-resolution-test-{}", std::process::id()));
        let vm = sample_vm("dup-check");
        let _ = resolve_or_spawn(&registry, &dir, &vm);
        // Immediately call again: the entry is already Resolving (inserted
        // synchronously before the worker was spawned), so this call must
        // not create a second worker.
        let second = resolve_or_spawn(&registry, &dir, &vm);
        assert!(matches!(second, VmResolutionResult::Resolving));
        let guard = registry.lock().unwrap();
        assert_eq!(
            guard.len(),
            1,
            "only one registry entry should exist for one VM name"
        );
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
