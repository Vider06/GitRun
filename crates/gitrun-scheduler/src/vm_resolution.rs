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
//! 1. Try KVM. If it comes up, done — the thread records the result and
//!    exits. This is the common case and doesn't involve the operator at
//!    all.
//! 2. If KVM fails, write a pending decision (`gitrun_core::hypervisor_decision`)
//!    and block *this thread only*, polling for up to 5 minutes for the
//!    dashboard to answer.
//! 3. Answered → act on the choice (retry KVM, or set up VirtualBox
//!    instead) and record the result.
//! 4. Not answered within 5 minutes → give up *for this attempt*: clear the
//!    pending decision and record nothing, so the next time reconcile asks
//!    for this VM (next poll cycle), resolution starts fresh from step 1 —
//!    self-healing, no separate cooldown timer needed.
//!
//! While a thread is in flight (or hasn't been spawned yet), reconcile's
//! caller (`main.rs::resolve_backend_and_image`) gets `None` back
//! immediately and falls back to the local Linux host for that cycle only,
//! exactly like the pre-existing "VM lookup not wired up" fallback — the
//! only difference is this one resolves itself once the VM is ready.

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

#[derive(Debug, Clone)]
enum VmResolution {
    /// A background thread is already working on this VM; don't spawn a
    /// second one.
    Resolving,
    /// Resolved for the lifetime of this `gitrun-autoscaler` process —
    /// re-checked from scratch on every restart, not just once ever, since
    /// a VM that was up when this was cached could have been stopped
    /// externally in the meantime. See `resolve_or_spawn`'s doc comment on
    /// why that's an accepted trade-off rather than a health-check loop.
    Resolved(DockerHost),
}

/// Shared across every reconcile cycle (and the background threads it
/// spawns) for the lifetime of the `gitrun-autoscaler` process. A plain
/// `Mutex<HashMap>` rather than anything fancier: entries are written
/// rarely (once per VM, occasionally re-written after a give-up), and read
/// once per reconcile cycle per VM-targeting Logic Containers rule — not a
/// hot path that needs lock-free structures.
pub type VmResolutionRegistry = Arc<Mutex<HashMap<String, VmResolution>>>;

pub fn new_registry() -> VmResolutionRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Called from `resolve_backend_and_image` on every reconcile cycle that
/// needs a VM-backed runner. Never blocks:
/// - Already resolved this run → returns the cached `DockerHost`
///   immediately, no thread involved.
/// - A resolution thread is already in flight → returns `None` (caller
///   falls back to local host for this cycle) without spawning another.
/// - Neither → spawns the background thread described in this module's
///   doc comment and returns `None` for *this* cycle; a later cycle will
///   see `Resolved` once the thread finishes.
pub fn resolve_or_spawn(registry: &VmResolutionRegistry, state_dir: &Path, vm_config: &VmConfig) -> Option<DockerHost> {
    let mut guard = registry.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match guard.get(&vm_config.name) {
        Some(VmResolution::Resolved(host)) => return Some(host.clone()),
        Some(VmResolution::Resolving) => return None,
        None => {
            guard.insert(vm_config.name.clone(), VmResolution::Resolving);
        }
    }
    drop(guard); // release before spawning; the thread takes its own lock when it finishes

    let registry = Arc::clone(registry);
    let state_dir = state_dir.to_path_buf();
    let vm_config = vm_config.clone();
    let name_for_thread = vm_config.name.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("gitrun-vm-resolve-{}", vm_config.name))
        .spawn(move || {
            let outcome = resolve_blocking(&state_dir, &vm_config);
            let mut guard = registry.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match outcome {
                Some(host) => {
                    guard.insert(vm_config.name.clone(), VmResolution::Resolved(host));
                }
                // Gave up (operator didn't answer in time, or the chosen
                // hypervisor itself failed) — remove rather than leave
                // `Resolving` stuck forever, so the next reconcile cycle
                // that needs this VM spawns a fresh attempt instead of
                // waiting on a thread that no longer exists.
                None => {
                    guard.remove(&vm_config.name);
                }
            }
        });
    if let Err(error) = spawned {
        eprintln!("gitrun-autoscaler: failed to spawn VM resolution thread for '{name_for_thread}': {error}");
        // Roll back the `Resolving` marker we just inserted — otherwise a
        // failed spawn would wedge this VM as "in progress" forever with
        // no thread actually working on it.
        if let Ok(mut guard) = registry.lock() {
            guard.remove(&name_for_thread);
        }
    }
    None
}

/// The actual blocking sequence, run only inside the background thread —
/// `resolve_or_spawn` never calls this on the reconcile loop's own thread.
fn resolve_blocking(state_dir: &Path, vm_config: &VmConfig) -> Option<DockerHost> {
    match provision_and_wait(HypervisorKind::Kvm, vm_config) {
        Ok(host) => return Some(host),
        Err(error) => {
            println!(
                "gitrun-autoscaler: KVM setup failed for VM '{}' ({error}); asking the operator via the dashboard (retry KVM, or use VirtualBox instead), waiting up to {}s",
                vm_config.name,
                DECISION_TIMEOUT.as_secs()
            );
            if let Err(write_error) = hypervisor_decision::request(state_dir, &vm_config.name, &error.to_string()) {
                eprintln!(
                    "gitrun-autoscaler: could not write hypervisor decision request for '{}': {write_error} — giving up for this attempt",
                    vm_config.name
                );
                return None;
            }
        }
    }

    let deadline = Instant::now() + DECISION_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            println!(
                "gitrun-autoscaler: no operator decision for VM '{}' within {}s, giving up for this cycle — will try KVM again next time this VM is needed",
                vm_config.name,
                DECISION_TIMEOUT.as_secs()
            );
            let _ = hypervisor_decision::clear(state_dir, &vm_config.name);
            return None;
        }
        match hypervisor_decision::poll(state_dir, &vm_config.name) {
            Ok(Some(record)) if record.choice.is_some() => {
                let choice = record.choice.expect("just checked is_some");
                let _ = hypervisor_decision::clear(state_dir, &vm_config.name);
                let chosen_kind = match choice {
                    DecisionChoice::RetryKvm => HypervisorKind::Kvm,
                    DecisionChoice::UseVirtualBox => HypervisorKind::VirtualBox,
                };
                return match provision_and_wait(chosen_kind, vm_config) {
                    Ok(host) => Some(host),
                    Err(error) => {
                        eprintln!(
                            "gitrun-autoscaler: {chosen_kind:?} setup for VM '{}' failed after operator decision: {error} — giving up for this cycle",
                            vm_config.name
                        );
                        None
                    }
                };
            }
            Ok(_) => {
                // Either no record (shouldn't normally happen right after
                // we just wrote one, but tolerate it — e.g. something else
                // cleared it) or still pending. Either way, keep waiting.
            }
            Err(error) => {
                eprintln!("gitrun-autoscaler: error polling hypervisor decision for VM '{}': {error}", vm_config.name);
            }
        }
        std::thread::sleep(DECISION_POLL_INTERVAL);
    }
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
    Ok(DockerHost::Remote(vm::docker_host_address(&ip, config.docker_port)))
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
    fn resolve_or_spawn_returns_none_immediately_and_does_not_block() {
        // This is the core guarantee: even though the spawned thread will
        // fail fast (virsh/VBoxManage aren't installed in a test
        // environment) and go on to wait on a decision file that will
        // never be answered, the *caller* must get control back right
        // away rather than waiting on any of that. The thread itself keeps
        // running in the background until its own 5-minute give-up (or
        // process exit, whichever comes first, and `cargo test` doesn't
        // wait on non-test threads to exit) — harmless for the test, not
        // cleaned up here beyond best-effort temp dir removal.
        let registry = new_registry();
        let dir = std::env::temp_dir().join(format!("gitrun-vm-resolution-test-{}", std::process::id()));
        let started = Instant::now();
        let result = resolve_or_spawn(&registry, &dir, &sample_vm("never-answered"));
        assert!(result.is_none());
        assert!(started.elapsed() < Duration::from_secs(2), "resolve_or_spawn must not block the caller");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_or_spawn_does_not_spawn_a_second_thread_while_resolving() {
        let registry = new_registry();
        let dir = std::env::temp_dir().join(format!("gitrun-vm-resolution-test-{}", std::process::id()));
        let vm = sample_vm("dup-check");
        let _ = resolve_or_spawn(&registry, &dir, &vm);
        // Immediately call again: the entry should already be `Resolving`
        // (inserted synchronously before the thread was spawned), so this
        // call must also return None without inserting a duplicate state.
        let second = resolve_or_spawn(&registry, &dir, &vm);
        assert!(second.is_none());
        let guard = registry.lock().unwrap();
        assert_eq!(guard.len(), 1, "only one registry entry should exist for one VM name");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
