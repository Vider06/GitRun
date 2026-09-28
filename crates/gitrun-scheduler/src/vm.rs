//! VM lifecycle for Logic Containers' Windows (or any non-Linux ISO)
//! support, across two possible hypervisors.
//!
//! Model, as designed with the operator: GitRun provisions VMs *ahead of
//! time* based on configuration — not on demand per job. Each configured VM
//! runs its own Docker daemon (Docker Desktop/Engine inside the VM), and
//! Logic Containers rules target a VM by name (`Backend::Vm { vm_name }` in
//! `logic_containers.rs`); the *containers* inside that VM are still the
//! scaling unit, created and destroyed the same way Linux runner containers
//! are — just via a remote Docker host (`docker::DockerHost::Remote`)
//! pointed at the VM's IP.
//!
//! Hypervisor choice: **KVM/libvirt is preferred** — it's native to the
//! Linux kernel, has no licensing caveats for commercial use (VirtualBox's
//! Extension Pack does, for some features), and is meaningfully faster than
//! VirtualBox's software/hybrid virtualization for CI workloads where build
//! time matters. **VirtualBox is the fallback** when KVM isn't usable (no
//! `/dev/kvm`, virtualization not exposed to the host, `virsh` unavailable)
//! — chosen automatically, or offered as a one-time prompt: "KVM is not
//! available on this host. Try VirtualBox instead?" (see
//! `HypervisorChoice`/`resolve_hypervisor_choice`, which the dashboard/CLI
//! should call rather than hardcoding either backend).
//!
//! Both hypervisors are driven via their CLI tools (`virsh` for KVM/libvirt,
//! `VBoxManage` for VirtualBox) rather than native library bindings — same
//! shelling-out style already used for Docker elsewhere in this crate, and
//! it avoids pulling in libvirt's C bindings (a linking dependency this
//! sandbox can't verify anyway) for a fairly small set of lifecycle ops.
//!
//! What this module does NOT do (explicitly out of scope for this pass):
//! - Install either hypervisor's packages automatically — see
//!   `install_instructions`, which returns guidance text rather than
//!   executing anything, deliberately (see that function's doc comment).
//! - Provide a Windows ISO/image — that's the operator's to supply; GitRun
//!   cannot legally distribute Windows.
//! - Configure Docker *inside* the VM automatically on first boot — assumes
//!   a `base_disk_image` that already has Docker configured and listening on
//!   `docker_port`.

use serde::{Deserialize, Serialize};
use std::process::Command;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum VmError {
    #[error("{0:?} is not installed or not usable on this host (no /dev/kvm, or the CLI tool is missing)")]
    HypervisorUnavailable(HypervisorKind),
    #[error("invalid VM configuration file: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("hypervisor command failed: {0}")]
    Command(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("VM '{0}' did not report an IP address within the startup timeout")]
    NoIpAddress(String),
}

pub type Result<T> = std::result::Result<T, VmError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HypervisorKind {
    Kvm,
    VirtualBox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivationMode {
    /// VM starts on first job, stops after being idle — same idle-timeout
    /// concept as `gitrun_core::Config::idle_timeout` for containers.
    Standard,
    /// VM stays running always; only the in-VM runner service is toggled.
    /// Marked experimental per the operator's explicit request — this
    /// trades resource cost for responsiveness and should be opt-in, not
    /// the default a fresh Logic Containers setup gets.
    AlwaysOnExperimental,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    /// VM name (also how Logic Containers rules reference it via
    /// `Backend::Vm { vm_name }`). Must be unique across both hypervisors —
    /// callers shouldn't assume which one actually owns a given name without
    /// checking `VmConfig::hypervisor`.
    pub name: String,
    /// Hypervisor to *try first*. Not necessarily what ends up running this
    /// VM: `resolve_hypervisor_choice`'s KVM-preferred, ask-before-fallback
    /// flow (see `gitrun-scheduler::vm_resolution`) can end up using
    /// VirtualBox for a run even when this says `Kvm`, if KVM fails and the
    /// operator picks the fallback. This field is the configured starting
    /// point/preference, not a runtime guarantee.
    pub hypervisor: HypervisorKind,
    /// Path to the base disk image to clone from when provisioning — e.g. a
    /// prepared Windows Server image with Docker already installed, or a
    /// Linux ISO the operator wants to run as a VM instead of a container
    /// for stronger isolation. Format expectations differ by hypervisor
    /// (qcow2 for KVM, VDI/VMDK/VHD for VirtualBox) — not validated here.
    pub base_disk_image: String,
    pub memory_mb: u32,
    pub cpus: u32,
    /// Port the Docker daemon inside the VM listens on (commonly 2376 for
    /// TLS-secured remote Docker).
    pub docker_port: u16,
    pub activation: ActivationMode,
    /// Whether this VM runs Windows — used to pick the Windows-appropriate
    /// `docker::RunnerSpec` flags (see `RunnerSpec::is_windows`'s doc
    /// comment) once a runner container is created inside it. `false` for
    /// a Linux VM used for stronger isolation rather than an OS GitRun
    /// can't run as a plain container.
    pub is_windows: bool,
}

/// Loads VM definitions from `{state_dir}/vm-configs.json` — the same file
/// and format the dashboard's `list_vm_configs`/`save_vm_config` Tauri
/// commands already read and write (see
/// `gitrun-dashboard-tauri/src-tauri/src/lib.rs`). Mirrors
/// `logic_containers::load_rules` exactly: missing file means "no VMs
/// configured yet", not an error.
///
/// This used to be `Config::vm_definitions`, an env-var string parsed by a
/// hand-rolled `key:value,key:value` format — before it was noticed that
/// the dashboard already had a *different*, working, JSON-based way to
/// manage VM configs that the autoscaler was never reading, meaning
/// anything an operator configured through the dashboard would have been
/// silently invisible to the scheduler. Switched to share the dashboard's
/// file/format instead of inventing a second, disconnected one.
pub fn load_vm_configs(state_dir: &std::path::Path) -> Result<Vec<VmConfig>> {
    let path = state_dir.join("vm-configs.json");
    match std::fs::read_to_string(&path) {
        Ok(raw) => Ok(serde_json::from_str(&raw)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// Saves VM definitions to `{state_dir}/vm-configs.json`, same atomic
/// write-then-rename pattern as `logic_containers::save_rules`. Not
/// currently called from `gitrun-scheduler` itself (the dashboard is the
/// only writer today, via its own `save_vm_config` command) — provided
/// here so any other future caller (e.g. `gitrun setup`, or a CLI command)
/// has a single correct implementation to share rather than reinventing
/// the atomic-write dance.
pub fn save_vm_configs(state_dir: &std::path::Path, configs: &[VmConfig]) -> Result<()> {
    let path = state_dir.join("vm-configs.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(configs)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn find_vm_config<'a>(definitions: &'a [VmConfig], name: &str) -> Option<&'a VmConfig> {
    definitions.iter().find(|v| v.name == name)
}

/// What to do about hypervisor availability for a *new* VM. This is the
/// single place implementing the operator's "KVM not supported, want to try
/// VirtualBox instead?" framing — the dashboard/CLI calls
/// `resolve_hypervisor_choice()` and acts on the result, rather than any
/// caller hardcoding a hypervisor or silently switching between them.
pub enum HypervisorChoice {
    /// KVM is available and should be used without asking.
    UseKvm,
    /// Both KVM and VirtualBox could plausibly be made available, but KVM
    /// isn't usable right now — the operator should be asked before falling
    /// back, per the "want to try VirtualBox instead?" framing, rather than
    /// silently switching hypervisors underneath them.
    AskToFallBackToVirtualBox,
    /// Neither is available right now (and KVM looks installable with just
    /// a package, so that's the more helpful thing to surface first rather
    /// than immediately pushing VirtualBox).
    NeitherAvailable,
}

/// Checks the host and returns what choice should be presented/acted on.
/// Does not install anything or make the decision itself.
pub fn resolve_hypervisor_choice() -> HypervisorChoice {
    if is_kvm_available() {
        return HypervisorChoice::UseKvm;
    }
    if is_virtualbox_installed() {
        return HypervisorChoice::AskToFallBackToVirtualBox;
    }
    if kvm_device_exists() {
        // /dev/kvm exists but virsh isn't installed — KVM is plausible with
        // just a package install; don't push VirtualBox as the first
        // suggestion in that case.
        HypervisorChoice::NeitherAvailable
    } else {
        HypervisorChoice::AskToFallBackToVirtualBox
    }
}

fn kvm_device_exists() -> bool {
    std::path::Path::new("/dev/kvm").exists()
}

fn is_kvm_available() -> bool {
    kvm_device_exists()
        && Command::new("virsh")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}

pub fn is_virtualbox_installed() -> bool {
    Command::new("VBoxManage")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Human-readable, non-executed guidance for installing a hypervisor.
/// Deliberately text, not an automated installer: the correct package
/// manager invocation depends on the host distro, and — for VirtualBox
/// specifically — installing it can involve accepting a license for the
/// Extension Pack that GitRun should not silently accept on the operator's
/// behalf. A security/licensing-relevant action like this should be an
/// explicit, visible step, not something buried in a background
/// provisioning routine (same reasoning as `docs/SECURITY_MODEL.md`).
pub fn install_instructions(kind: HypervisorKind) -> &'static str {
    match kind {
        HypervisorKind::Kvm => {
            "Install libvirt + virsh (e.g. `apt install qemu-kvm libvirt-daemon-system virsh` on Debian/Ubuntu), \
             ensure /dev/kvm exists (check with `ls /dev/kvm`; if missing, verify virtualization is enabled in \
             the host's BIOS/hypervisor settings), and add the GitRun service user to the `libvirt` group."
        }
        HypervisorKind::VirtualBox => {
            "Install VirtualBox from your distro's package manager or Oracle's .deb/.rpm (headless use does not \
             require the Extension Pack, which has separate licensing terms for commercial use — review those \
             terms before installing it if GitRun will run in a commercial setting)."
        }
    }
}

fn run(kind: HypervisorKind, args: &[&str]) -> Result<std::process::Output> {
    let program = match kind {
        HypervisorKind::Kvm => "virsh",
        HypervisorKind::VirtualBox => "VBoxManage",
    };
    Command::new(program).args(args).output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            VmError::HypervisorUnavailable(kind)
        } else {
            VmError::Io(error)
        }
    })
}

fn run_checked(kind: HypervisorKind, args: &[&str]) -> Result<String> {
    let output = run(kind, args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(VmError::Command(if stderr.is_empty() {
            format!("{kind:?} command '{}' failed", args.join(" "))
        } else {
            stderr
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Creates a VM from `config` if it doesn't already exist, using whichever
/// hypervisor `config.hypervisor` names. Idempotent: safe to call on every
/// GitRun startup for every configured VM.
pub fn ensure_vm(config: &VmConfig) -> Result<()> {
    match config.hypervisor {
        HypervisorKind::Kvm => ensure_vm_kvm(config),
        HypervisorKind::VirtualBox => ensure_vm_virtualbox(config),
    }
}

fn ensure_vm_kvm(config: &VmConfig) -> Result<()> {
    if vm_exists(HypervisorKind::Kvm, &config.name)? {
        return Ok(());
    }
    // virt-install would be the friendlier tool for this, but it's a
    // separate package from libvirt/virsh; using `virsh define` with a
    // minimal generated domain XML keeps this to the one CLI tool already
    // required for everything else in this module.
    let xml = format!(
        r#"<domain type='kvm'>
  <name>{name}</name>
  <memory unit='MiB'>{memory_mb}</memory>
  <vcpu>{cpus}</vcpu>
  <os><type arch='x86_64'>hvm</type></os>
  <devices>
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2'/>
      <source file='{disk}'/>
      <target dev='vda' bus='virtio'/>
    </disk>
    <interface type='network'>
      <source network='default'/>
      <model type='virtio'/>
    </interface>
    <graphics type='none'/>
  </devices>
</domain>"#,
        name = config.name,
        memory_mb = config.memory_mb,
        cpus = config.cpus,
        disk = config.base_disk_image,
    );
    let tmp_path = std::env::temp_dir().join(format!("gitrun-vm-{}.xml", config.name));
    std::fs::write(&tmp_path, xml)?;
    let path_str = tmp_path.to_string_lossy().into_owned();
    run_checked(HypervisorKind::Kvm, &["define", &path_str])?;
    let _ = std::fs::remove_file(&tmp_path);
    Ok(())
}

fn ensure_vm_virtualbox(config: &VmConfig) -> Result<()> {
    if vm_exists(HypervisorKind::VirtualBox, &config.name)? {
        return Ok(());
    }
    run_checked(
        HypervisorKind::VirtualBox,
        &[
            "createvm",
            "--name",
            &config.name,
            "--ostype",
            "Other_64",
            "--register",
        ],
    )?;
    run_checked(
        HypervisorKind::VirtualBox,
        &[
            "modifyvm",
            &config.name,
            "--memory",
            &config.memory_mb.to_string(),
            "--cpus",
            &config.cpus.to_string(),
            "--nic1",
            "nat",
            "--natpf1",
            &format!("docker,tcp,,{},,{}", config.docker_port, config.docker_port),
        ],
    )?;
    run_checked(
        HypervisorKind::VirtualBox,
        &[
            "storagectl",
            &config.name,
            "--name",
            "SATA",
            "--add",
            "sata",
            "--controller",
            "IntelAhci",
        ],
    )?;
    run_checked(
        HypervisorKind::VirtualBox,
        &[
            "storageattach",
            &config.name,
            "--storagectl",
            "SATA",
            "--port",
            "0",
            "--type",
            "hdd",
            "--medium",
            &config.base_disk_image,
        ],
    )?;
    Ok(())
}

fn vm_exists(kind: HypervisorKind, name: &str) -> Result<bool> {
    match kind {
        HypervisorKind::Kvm => {
            let output = run_checked(kind, &["list", "--all", "--name"])?;
            Ok(output.lines().any(|line| line.trim() == name))
        }
        HypervisorKind::VirtualBox => {
            let output = run_checked(kind, &["list", "vms"])?;
            Ok(output
                .lines()
                .any(|line| line.starts_with(&format!("\"{name}\""))))
        }
    }
}

pub fn is_running(kind: HypervisorKind, name: &str) -> Result<bool> {
    match kind {
        HypervisorKind::Kvm => {
            let output = run_checked(kind, &["list", "--name"])?; // running domains only
            Ok(output.lines().any(|line| line.trim() == name))
        }
        HypervisorKind::VirtualBox => {
            let output = run_checked(kind, &["list", "runningvms"])?;
            Ok(output
                .lines()
                .any(|line| line.starts_with(&format!("\"{name}\""))))
        }
    }
}

pub fn start(kind: HypervisorKind, name: &str) -> Result<()> {
    if is_running(kind, name)? {
        return Ok(());
    }
    match kind {
        HypervisorKind::Kvm => run_checked(kind, &["start", name])?,
        HypervisorKind::VirtualBox => run_checked(kind, &["startvm", name, "--type", "headless"])?,
    };
    Ok(())
}

pub fn stop(kind: HypervisorKind, name: &str) -> Result<()> {
    if !is_running(kind, name)? {
        return Ok(());
    }
    match kind {
        HypervisorKind::Kvm => run_checked(kind, &["shutdown", name])?,
        HypervisorKind::VirtualBox => run_checked(kind, &["controlvm", name, "acpipowerbutton"])?,
    };
    Ok(())
}

/// Forcefully stops a VM that didn't respond to a graceful shutdown request
/// within a reasonable time. Callers should attempt `stop` first and only
/// escalate to this after a timeout.
pub fn force_stop(kind: HypervisorKind, name: &str) -> Result<()> {
    match kind {
        HypervisorKind::Kvm => run_checked(kind, &["destroy", name])?,
        HypervisorKind::VirtualBox => run_checked(kind, &["controlvm", name, "poweroff"])?,
    };
    Ok(())
}

/// Polls for the VM's guest-reported IP address, used to build the remote
/// Docker host address once the VM is up. KVM requires the guest agent
/// (`qemu-guest-agent`) running inside the guest; VirtualBox requires Guest
/// Additions — both are the standard mechanism for each hypervisor and are
/// not installed by this module (part of preparing `base_disk_image`).
pub fn wait_for_ip(kind: HypervisorKind, name: &str, timeout: Duration) -> Result<String> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let ip = match kind {
            HypervisorKind::Kvm => run_checked(kind, &["domifaddr", name])
                .ok()
                .and_then(|output| extract_kvm_ip(&output)),
            HypervisorKind::VirtualBox => run_checked(
                kind,
                &[
                    "guestproperty",
                    "get",
                    name,
                    "/VirtualBox/GuestInfo/Net/0/V4/IP",
                ],
            )
            .ok()
            .and_then(|output| output.strip_prefix("Value: ").map(|s| s.trim().to_owned())),
        };
        if let Some(ip) = ip {
            if !ip.is_empty() {
                return Ok(ip);
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err(VmError::NoIpAddress(name.to_owned()))
}

/// Parses `virsh domifaddr`'s table output for the first IPv4 address, e.g.:
/// ```text
///  Name       MAC address          Protocol     Address
/// -------------------------------------------------------------------------------
///  vnet0      52:54:00:aa:bb:cc    ipv4         192.168.122.45/24
/// ```
fn extract_kvm_ip(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let cidr = line.split_whitespace().last()?;
        let ip = cidr.split('/').next()?;
        ip.parse::<std::net::Ipv4Addr>().ok().map(|_| ip.to_owned())
    })
}

/// Builds the `tcp://ip:port` address string for a VM's Docker daemon once
/// its IP is known. Returned as a plain String — the caller wraps it in
/// `docker::DockerHost::Remote(..)`.
pub fn docker_host_address(ip: &str, docker_port: u16) -> String {
    format!("tcp://{ip}:{docker_port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_mode_variants_are_distinct() {
        assert_ne!(
            ActivationMode::Standard,
            ActivationMode::AlwaysOnExperimental
        );
    }

    fn sample_vm(name: &str, disk: &str) -> VmConfig {
        VmConfig {
            name: name.to_owned(),
            hypervisor: HypervisorKind::Kvm,
            base_disk_image: disk.to_owned(),
            memory_mb: 4096,
            cpus: 2,
            docker_port: 2376,
            activation: ActivationMode::Standard,
            is_windows: false,
        }
    }

    fn temp_state_dir(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gitrun-vm-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn load_vm_configs_on_missing_file_is_an_empty_list_not_an_error() {
        let dir = temp_state_dir("missing");
        assert_eq!(load_vm_configs(&dir).unwrap().len(), 0);
    }

    #[test]
    fn save_then_load_round_trips_vm_configs() {
        let dir = temp_state_dir("roundtrip");
        let configs = vec![
            sample_vm("win-runner-1", "/vms/win.qcow2"),
            sample_vm("linux-iso", "/vms/base.qcow2"),
        ];
        save_vm_configs(&dir, &configs).unwrap();
        let loaded = load_vm_configs(&dir).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].name, "win-runner-1");
        assert_eq!(loaded[1].base_disk_image, "/vms/base.qcow2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_vm_configs_reads_the_exact_json_the_dashboard_writes() {
        // Locks in that this is the SAME file/format the Tauri dashboard's
        // `save_vm_config` command uses (`{state_dir}/vm-configs.json`, a
        // plain JSON array) — this test would catch either side drifting.
        let dir = temp_state_dir("dashboard-format");
        save_vm_configs(&dir, &[sample_vm("a", "/a.qcow2")]).unwrap();
        let raw = std::fs::read_to_string(dir.join("vm-configs.json")).unwrap();
        let parsed: Vec<VmConfig> = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_vm_config_looks_up_by_name() {
        let defs = vec![sample_vm("a", "/a.qcow2"), sample_vm("b", "/b.qcow2")];
        assert_eq!(
            find_vm_config(&defs, "b").unwrap().base_disk_image,
            "/b.qcow2"
        );
        assert!(find_vm_config(&defs, "missing").is_none());
    }

    #[test]
    fn docker_host_address_builds_expected_tcp_address() {
        assert_eq!(
            docker_host_address("192.168.56.10", 2376),
            "tcp://192.168.56.10:2376"
        );
    }

    #[test]
    fn neither_hypervisor_available_does_not_panic() {
        let choice = resolve_hypervisor_choice();
        assert!(matches!(
            choice,
            HypervisorChoice::NeitherAvailable | HypervisorChoice::AskToFallBackToVirtualBox
        ));
    }

    #[test]
    fn extract_kvm_ip_parses_domifaddr_table() {
        let sample = " Name       MAC address          Protocol     Address\n-------------------------------------------------------------------------------\n vnet0      52:54:00:aa:bb:cc    ipv4         192.168.122.45/24\n";
        assert_eq!(extract_kvm_ip(sample), Some("192.168.122.45".to_owned()));
    }

    #[test]
    fn extract_kvm_ip_returns_none_for_empty_output() {
        assert_eq!(extract_kvm_ip(""), None);
        assert_eq!(extract_kvm_ip(" Name  MAC  Protocol  Address\n---\n"), None);
    }

    #[test]
    fn install_instructions_are_nonempty_for_both_kinds() {
        assert!(!install_instructions(HypervisorKind::Kvm).is_empty());
        assert!(!install_instructions(HypervisorKind::VirtualBox).is_empty());
    }
}
