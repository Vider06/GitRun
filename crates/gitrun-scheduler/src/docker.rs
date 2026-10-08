//! Thin wrapper around the `docker` CLI for the pieces the autoscaler needs:
//! listing/inspecting managed containers, creating runner containers, and the
//! shared cache volume. Mirrors `gitrun_manager.py`'s `docker()`/`managed_containers()`/
//! `container_status()`/`create_runner()`/`ensure_shared_cache_volume()`.
//!
//! Every function here does real I/O (spawns `docker`); nothing in this module
//! makes scheduling decisions — that's `reconcile.rs`, which is pure and takes
//! a `DockerRunner`-shaped snapshot as plain data instead of calling this module
//! directly, so it stays testable without Docker installed.

use serde_json::Value;
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DockerError {
    #[error("failed to invoke docker: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("docker command failed: {0}")]
    Command(String),
    #[error("unable to parse docker output: {0}")]
    Decode(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, DockerError>;

const SHARED_CACHE_VOLUME_DEFAULT: &str = "gitrun-runner-shared";
const DOCKER_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const GITRUN_API_SOCKET_GID: &str = "10000";

/// Where a Docker command actually runs. `Local` is the existing behavior
/// (talks to the host's own Docker socket via the CLI's default). `Remote`
/// targets a Docker daemon exposed over TCP — the shape Logic Containers
/// needs for a Windows-container host running inside a VirtualBox VM: same
/// `docker run`/`docker ps`/etc. commands, just pointed at a different
/// daemon via `DOCKER_HOST`, rather than a different code path per OS.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DockerHost {
    #[default]
    Local,
    /// `tcp://host:port`, e.g. `tcp://192.168.56.10:2376`. TLS is assumed
    /// configured on the daemon side (Docker's default posture for any
    /// TCP-exposed daemon that isn't purely for trusted-localhost use) —
    /// this struct doesn't currently carry cert paths; add them here before
    /// pointing at anything outside a trusted, isolated network.
    Remote(String),
    /// Remote Docker daemon using Docker client TLS/mTLS credentials.
    RemoteTls { endpoint: String, cert_dir: String },
}

fn run_on(host: &DockerHost, args: &[&str]) -> Result<Output> {
    let mut command = Command::new("docker");

    // A Docker context inherited from the scheduler environment must never
    // override the host selected explicitly by GitRun.
    command.env_remove("DOCKER_CONTEXT");

    match host {
        DockerHost::Local => {
            // Local means the host Docker socket, not whichever context the
            // service environment happened to inherit.
            command
                .env_remove("DOCKER_TLS_VERIFY")
                .env_remove("DOCKER_CERT_PATH")
                .env("DOCKER_HOST", "unix:///var/run/docker.sock");
        }
        DockerHost::Remote(addr) => {
            command.env("DOCKER_HOST", addr);
        }
        DockerHost::RemoteTls { endpoint, cert_dir } => {
            command
                .env("DOCKER_HOST", endpoint)
                .env("DOCKER_TLS_VERIFY", "1")
                .env("DOCKER_CERT_PATH", cert_dir);
        }
    }

    let command_name = args.first().copied().unwrap_or("command");
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args)
        .spawn()?;
    let mut stdout = child.stdout.take().ok_or_else(|| {
        DockerError::Command(format!("docker {command_name} did not provide stdout"))
    })?;
    let mut stderr = child.stderr.take().ok_or_else(|| {
        DockerError::Command(format!("docker {command_name} did not provide stderr"))
    })?;

    // Drain both pipes concurrently so a chatty Docker CLI can never block
    // waiting for the parent process to read a full pipe buffer.
    let stdout_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).map(|_| buf)
    });
    let stderr_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).map(|_| buf)
    });

    let deadline = Instant::now() + DOCKER_COMMAND_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            let stdout = stdout_thread
                .join()
                .map_err(|_| DockerError::Command("docker stdout reader panicked".into()))??;
            let stderr = stderr_thread
                .join()
                .map_err(|_| DockerError::Command("docker stderr reader panicked".into()))??;
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(DockerError::Command(format!(
                "docker {command_name} timed out after {}s",
                DOCKER_COMMAND_TIMEOUT.as_secs()
            )));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn run_checked_on(host: &DockerHost, args: &[&str]) -> Result<String> {
    let output = run_on(host, args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let command_name = args.first().copied().unwrap_or("command");
        return Err(DockerError::Command(if stderr.is_empty() {
            // Never include the complete argument vector here: docker run
            // arguments can contain runner registration tokens and vault secrets.
            format!("docker {command_name} failed with status {}", output.status)
        } else {
            stderr
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_checked(args: &[&str]) -> Result<String> {
    run_checked_on(&DockerHost::Local, args)
}

/// Sanitizes a string for use as part of a Docker container/volume name or
/// label value: keeps alphanumerics, `_`, `.`, `-`, replaces everything else
/// with the given placeholder character.
pub fn sanitize(value: &str, replacement: char) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                replacement
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct ManagedContainer {
    pub name: String,
    pub status: String,
    pub permanent: bool,
    pub dock_target: bool,
    pub workflow_job: Option<String>,
    pub workflow_run_id: Option<u64>,
}

fn cache_volume_name(base: &str, scope: &str, repo: &str, runner: &str) -> String {
    match scope {
        "global" => base.to_owned(),
        "repository" => format!("{}-repo-{}", base, cache_path_key(repo)),
        "runner" => format!("{}-runner-{}", base, sanitize(runner, '-')),
        _ => format!("{}-repo-{}", base, cache_path_key(repo)),
    }
}

pub fn shared_cache_volume(configured: Option<&str>) -> String {
    configured
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(SHARED_CACHE_VOLUME_DEFAULT)
        .to_owned()
}

pub fn ensure_shared_cache_volume(name: &str) -> Result<()> {
    // Docker treats create of an existing volume on the same driver as a
    // successful reuse, so this single operation avoids an inspect/create
    // time-of-check/time-of-use race between scheduler threads.
    run_checked(&["volume", "create", "--label", "gitrun.shared=true", name])?;
    Ok(())
}

/// Lists the full command line of every process currently running inside
/// `container_name`, via `docker top <name> -eo args`. This is Layer 2's
/// (external, host-side) window into what's executing - see
/// `gitrun_gsr::agent`'s module docs for why a second, independent
/// observation point exists at all (catching a job that bypassed or
/// removed the internal shell-wrapper agent). `-eo args` asks the
/// container's own `ps` for just the full argument list per process (no
/// PID/user/etc columns to parse out), one per output line, which is all
/// `gitrun_scheduler::gsr_poll` needs to re-evaluate against
/// `CommandPolicy`.
///
/// Returns an empty list (not an error) if the container has already
/// exited or been removed between the caller listing it and this call -
/// a normal race in a poll loop, not a failure worth surfacing.
pub fn container_command_lines_on(host: &DockerHost, container_name: &str) -> Result<Vec<String>> {
    match run_checked_on(host, &["top", container_name, "-eo", "args"]) {
        Ok(output) => Ok(parse_top_output(&output)),
        Err(DockerError::Command(message)) if is_missing_container_error(&message) => {
            Ok(Vec::new())
        }
        Err(error) => Err(error),
    }
}

/// Parsing half of `container_command_lines_on`, split out so it's
/// testable without a real Docker daemon: `docker top ... -eo args`
/// prints a `COMMAND` header line followed by one full command line per
/// process.
fn parse_top_output(output: &str) -> Vec<String> {
    let mut lines = output.lines();
    let first = lines.next();
    let lines = if first.map(str::trim) == Some("COMMAND") {
        lines
    } else {
        output.lines()
    };

    lines
        .map(str::to_owned)
        .filter(|line| !line.trim().is_empty())
        .collect()
}
/// Lists every GitRun-managed runner container regardless of repo — what
/// `gsr_poll`'s loop needs (it watches all runners at once, not one repo
/// at a time). Same `gitrun.runner=true` label filter as
/// `managed_containers_on`, just without the additional per-repo filter.
pub fn all_managed_container_names_on(host: &DockerHost) -> Result<Vec<String>> {
    let output = run_checked_on(
        host,
        &[
            "ps",
            "--filter",
            "label=gitrun.runner=true",
            "--format",
            "{{.Names}}",
        ],
    )?;
    Ok(output
        .lines()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Lists containers managed by GitRun for a given repo, with their raw
/// docker `Status` string and whether they carry the "permanent" label.
pub fn managed_containers(repo: &str) -> Result<Vec<ManagedContainer>> {
    managed_containers_on(&DockerHost::Local, repo)
}

pub fn managed_containers_on(host: &DockerHost, repo: &str) -> Result<Vec<ManagedContainer>> {
    let output = run_checked_on(
        host,
        &[
            "ps",
            "-a",
            "--filter",
            "label=gitrun.runner=true",
            "--filter",
            &format!("label=gitrun.repo={repo}"),
            "--format",
            "{{.Names}}",
        ],
    )?;

    let mut containers = Vec::new();
    for name in output.lines().map(str::trim).filter(|n| !n.is_empty()) {
        let Some(status) = container_status_string(host, name)? else {
            // The container disappeared between "docker ps" and "inspect" —
            // a normal reconciliation race. It must not turn into an empty
            // status that downstream code could interpret as a dead container.
            continue;
        };
        let permanent = container_is_permanent_on(host, name).unwrap_or(true); // fail-safe: assume permanent, matching gitrun_updater_utility.py's upgrade-safety default
        let dock_target = container_label_on(host, name, "gitrun.dock_target")
            .ok()
            .flatten()
            .is_some_and(|value| value.eq_ignore_ascii_case("true"));
        let workflow_job = container_label_on(host, name, "gitrun.workflow_job")
            .ok()
            .flatten();
        let workflow_run_id = container_label_on(host, name, "gitrun.workflow_run")
            .ok()
            .flatten()
            .and_then(|value| value.parse::<u64>().ok());
        containers.push(ManagedContainer {
            name: name.to_owned(),
            status,
            permanent,
            dock_target,
            workflow_job,
            workflow_run_id,
        });
    }
    Ok(containers)
}

/// Raw `.State.Status` string (e.g. "running", "exited"). Returns `None`
/// when the container disappears during a normal reconciliation race; real
/// Docker/JSON errors are returned to the caller.
/// Reads the `gitrun.repo` label off a running/existing container — the
/// same label `create_runner_on` sets at creation (see `RunnerSpec.repo`).
/// Used by `gsr_poll` to know which repo to (optionally) ban after a
/// policy violation, without needing to separately track container->repo
/// associations outside of what Docker itself already records.
pub fn container_repo_label_on(host: &DockerHost, container_name: &str) -> Result<Option<String>> {
    let output = run_on(
        host,
        &[
            "inspect",
            "-f",
            "{{index .Config.Labels \"gitrun.repo\"}}",
            container_name,
        ],
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if is_missing_container_error(&stderr) {
            return Ok(None);
        }
        return Err(DockerError::Command(if stderr.is_empty() {
            format!("docker inspect {container_name} failed")
        } else {
            stderr
        }));
    }
    let label = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(if label.is_empty() { None } else { Some(label) })
}

fn container_label_on(host: &DockerHost, name: &str, label: &str) -> Result<Option<String>> {
    let format = format!("{{index .Config.Labels \\\"{label}\\\"}}");
    let output = run_on(host, &["inspect", "-f", &format, name])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if is_missing_container_error(&stderr) {
            return Ok(None);
        }
        return Err(DockerError::Command(if stderr.is_empty() {
            format!("docker inspect {name} failed")
        } else {
            stderr
        }));
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!value.is_empty()).then_some(value))
}
fn container_status_string(host: &DockerHost, name: &str) -> Result<Option<String>> {
    let output = run_on(host, &["inspect", "-f", "{{json .State}}", name])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if is_missing_container_error(&stderr) {
            return Ok(None);
        }
        return Err(DockerError::Command(if stderr.is_empty() {
            format!("docker inspect {name} failed")
        } else {
            stderr
        }));
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(raw.trim())?;
    let status = value.get("Status").and_then(Value::as_str).ok_or_else(|| {
        DockerError::Command(format!(
            "docker inspect {name} returned no container status"
        ))
    })?;
    Ok(Some(status.to_owned()))
}

fn is_missing_container_error(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("no such container")
        || normalized.contains("no such object")
        || normalized.contains("is not running")
}
pub fn container_is_permanent(name: &str) -> Result<bool> {
    container_is_permanent_on(&DockerHost::Local, name)
}

pub fn container_is_permanent_on(host: &DockerHost, name: &str) -> Result<bool> {
    let output = run_on(
        host,
        &[
            "inspect",
            "-f",
            "{{index .Config.Labels \"gitrun.dynamic\"}}",
            name,
        ],
    )?;
    if !output.status.success() {
        return Ok(true);
    }
    let dynamic = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_ascii_lowercase();
    Ok(dynamic != "true")
}

pub fn restart_container(name: &str) -> Result<()> {
    restart_container_on(&DockerHost::Local, name)
}

pub fn restart_container_on(host: &DockerHost, name: &str) -> Result<()> {
    run_checked_on(host, &["restart", name])?;
    Ok(())
}

/// Produces a collision-free path component from a repository name.
/// Hex-encoding the original bytes is injective, unlike `sanitize`, which
/// intentionally replaces different punctuation with the same character.
fn cache_path_key(repo: &str) -> String {
    let mut key = String::with_capacity(repo.len() * 2);
    for byte in repo.bytes() {
        use std::fmt::Write;
        write!(&mut key, "{byte:02x}").expect("writing to String cannot fail");
    }
    key
}
/// Deterministic per-container volume name for `RunnerHomeBackend::Volume`,
/// so `remove_container_on` can clean it up without having to remember it
/// separately (the scheduler doesn't persist per-container metadata beyond
/// what Docker labels already carry).
fn home_volume_name(container_name: &str) -> String {
    format!("{container_name}-home")
}

pub fn remove_container(name: &str) -> Result<()> {
    remove_container_on(&DockerHost::Local, name)
}
pub fn pull_image(image: &str) -> Result<()> {
    run_checked(&["pull", image])?;
    Ok(())
}

pub fn image_id(image_or_container: &str) -> Result<String> {
    let output = run_checked(&["inspect", "-f", "{{.Id}}", image_or_container])?;
    let id = output.trim();
    if id.is_empty() {
        return Err(DockerError::Command(format!(
            "docker inspect returned an empty image ID for {image_or_container}"
        )));
    }
    Ok(id.to_owned())
}

/// Returns the image ID for an existing container. A container that
/// disappeared during reconciliation is reported as `None`; real Docker
/// failures still propagate.
pub fn container_image_id(name: &str) -> Result<Option<String>> {
    let output = run_on(&DockerHost::Local, &["inspect", "-f", "{{.Image}}", name])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if is_missing_container_error(&stderr) {
            return Ok(None);
        }
        return Err(DockerError::Command(if stderr.is_empty() {
            format!("docker inspect {name} failed")
        } else {
            stderr
        }));
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if id.is_empty() {
        return Err(DockerError::Command(format!(
            "docker inspect returned an empty image ID for container {name}"
        )));
    }
    Ok(Some(id))
}

pub fn rename_container(from: &str, to: &str) -> Result<()> {
    run_checked(&["rename", from, to])?;
    Ok(())
}

pub fn remove_container_on(host: &DockerHost, name_or_id: &str) -> Result<()> {
    // Resolve the deterministic home-volume name before removing the container.
    // Callers may intentionally pass an immutable container ID, so deriving
    // the volume name from that ID would leak the per-runner volume.
    let home_volume = match run_on(host, &["inspect", "-f", "{{.Name}}", name_or_id]) {
        Ok(output) if output.status.success() => {
            let resolved = String::from_utf8_lossy(&output.stdout)
                .trim()
                .trim_start_matches('/')
                .to_owned();
            (!resolved.is_empty()).then(|| home_volume_name(&resolved))
        }
        _ => None,
    };

    let output = run_on(host, &["rm", "-f", "-v", name_or_id])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if !is_missing_container_error(&stderr) {
            return Err(DockerError::Command(if stderr.is_empty() {
                format!("docker rm -f {name_or_id} failed")
            } else {
                stderr
            }));
        }
    }

    // Best-effort for disk-backed runner-home volumes. Harmless for tmpfs homes.
    if let Some(volume) = home_volume {
        let _ = run_on(host, &["volume", "rm", "-f", &volume]);
    }
    Ok(())
}
/// Parameters needed to create a runner container. Kept as a plain struct
/// (rather than a long argument list) so `reconcile.rs` can describe "create
/// this runner" as data without depending on this module's function signature.
pub struct RunnerSpec<'a> {
    pub name: &'a str,
    pub repo: &'a str,
    pub permanent: bool,
    pub registration_token: &'a str,
    pub image: &'a str,
    pub labels: &'a str,
    pub ephemeral: bool,
    pub disable_update: bool,
    pub cpus: &'a str,
    pub memory: &'a str,
    pub pids_limit: &'a str,
    pub shared_cache_volume: &'a str,
    pub cache_scope: &'a str,
    pub network: &'a str,
    pub seccomp_profile: &'a str,
    pub apparmor_profile: &'a str,
    pub docker_socket_gid: &'a str,
    /// Size string (e.g. "8g") for the runner's home directory, whether
    /// backed by tmpfs or a disk volume (see `home_backend`).
    pub runner_home_size: &'a str,
    /// Where the runner's home directory (`.runner`/`.credentials`,
    /// `_diag/`, `_work/`) lives. `Tmpfs` (default, matches the original
    /// behavior) trades host RAM for speed; `Volume` uses a per-container
    /// Docker-managed named volume on disk instead, removed alongside the
    /// container in `remove_container_on`. Configured via
    /// `Config::runner_home_backend` ("tmpfs" | "volume").
    pub home_backend: RunnerHomeBackend,
    /// Whether the Linux container root filesystem is mounted read-only.
    /// Writable state must come through the explicit runner-home/cache/runtime
    /// mounts below. This is the default sandbox posture.
    pub rootfs_read_only: bool,
    /// Decrypted GitVault secrets to inject as environment variables, as
    /// (name, value) pairs. Decryption happens just before this call and the
    /// plaintext lives only long enough to build the `docker run` argument
    /// list — see `main.rs::vault_env_for_repo`. Names are validated to be
    /// safe environment variable identifiers before reaching here.
    pub secret_env: &'a [(String, String)],
    /// Security-policy values passed to the container bootstrap. The
    /// bootstrap snapshots them into a root-owned file before the Actions
    /// runner starts; the GSR agent never trusts the workflow environment.
    pub gsr_policy_env: &'a [(String, String)],
    /// Workflow job name this runner is reserved for, when created from a
    /// queued job that GitDockRun statically marked as a target.
    pub workflow_job_name: Option<&'a str>,
    /// Workflow run ID paired with workflow_job_name.
    pub workflow_run_id: Option<u64>,
    /// Whether this container is a GitDockRun target that must survive the
    /// normal Dynamic-runner cleanup until the dock binding is released.
    pub dock_target: bool,
    /// True for a Windows container runner (Logic Containers). Changes which
    /// flags are valid: Windows containers don't support Linux `--read-only`,
    /// `--tmpfs`, `--pids-limit`, capability or Unix-style socket/group-add
    /// semantics. Windows isolation is enforced with its own container model.
    /// The Docker socket bind-mount is also
    /// skipped for Windows today: Docker-in-Docker via a mounted
    /// `//./pipe/docker_engine` named pipe is possible but not yet
    /// implemented here, so a Windows runner cannot itself run Docker builds
    /// until that's added.
    pub is_windows: bool,
    /// Whether this runner is explicitly allowed to expose the host Docker
    /// socket to workflow code. This is a repository policy decision and is
    /// false unless the repository settings explicitly enable the compatibility
    /// opt-out. GSR/GitDockRun do not depend on this flag.
    pub docker_socket_enabled: bool,
    /// GSR's "danger gate" (see `gitrun_core::Config::gsr_docker_socket_hardening`).
    /// When true (the default), extra Docker-level restrictions are applied
    /// to Linux runner containers to reduce what a process that escapes
    /// the job itself (not the container — see `apply_docker_socket_hardening`'s
    /// doc comment for exactly what this does and does not cover) can do
    /// with the mounted `/var/run/docker.sock`. Has no effect on Windows
    /// containers, which don't support these flags. Ignored (treated as
    /// hardening-on behavior regardless) unless `Config::validate` has
    /// already confirmed `gsr_allow_unsafe_runner` was explicitly set when
    /// this is false — this struct trusts its caller to have gone through
    /// that gate rather than re-checking it here.
    pub docker_socket_hardening: bool,
    /// Require Hyper-V isolation for Windows containers. Windows runners are
    /// only routed to configured VM-backed Docker daemons, so this adds a
    /// second isolation boundary inside the guest when supported.
    pub windows_hyperv_isolation: bool,
}

/// Where a runner container's home directory is backed. See
/// `RunnerSpec::home_backend`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerHomeBackend {
    Tmpfs,
    Volume,
}

impl RunnerHomeBackend {
    /// Parses `Config::runner_home_backend`. Falls back to `Tmpfs` (the
    /// pre-existing behavior) for anything unrecognized rather than
    /// failing the whole runner creation — `Config::validate` is the place
    /// that rejects a bad value up front, this is just a safe default if
    /// validation was ever skipped.
    pub fn from_config_str(value: &str) -> Self {
        match value {
            "volume" => Self::Volume,
            _ => Self::Tmpfs,
        }
    }
}

pub fn create_runner(spec: &RunnerSpec) -> Result<()> {
    create_runner_on(&DockerHost::Local, spec)
}

fn ensure_runner_network(host: &DockerHost, network: &str) -> Result<()> {
    let output = run_on(host, &["network", "inspect", network])?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr)
        .trim()
        .to_ascii_lowercase();
    if !(stderr.contains("no such network") || stderr.contains("network not found")) {
        return Err(DockerError::Command(if stderr.is_empty() {
            format!("docker network inspect {network} failed")
        } else {
            stderr
        }));
    }

    // A stable, GitRun-owned bridge network provides container-to-container
    // separation from Docker's default bridge and gives operators one fixed
    // network identity to attach host firewall/proxy policy to. It is not
    // itself an egress firewall: outbound control remains deployment-specific.
    run_checked_on(
        host,
        &[
            "network",
            "create",
            "--driver",
            "bridge",
            "--label",
            "gitrun.managed=true",
            network,
        ],
    )?;
    Ok(())
}

pub fn create_runner_on(host: &DockerHost, spec: &RunnerSpec) -> Result<()> {
    ensure_runner_network(host, spec.network)?;
    if spec.is_windows && spec.docker_socket_enabled {
        return Err(DockerError::Command(
            "Windows runner cannot enable the Linux Docker socket compatibility option".into(),
        ));
    }

    let cache_key = cache_path_key(spec.repo);
    let cache_volume = cache_volume_name(
        spec.shared_cache_volume,
        spec.cache_scope,
        spec.repo,
        spec.name,
    );
    let labels = ensure_label(spec.labels, "gitrun-ci");

    let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), spec.name.into()];
    args.extend(["--network".into(), spec.network.into()]);
    args.extend([
        "--label".into(),
        "gitrun.runner=true".into(),
        "--label".into(),
        format!("gitrun.repo={}", spec.repo),
        "--label".into(),
        "gitrun.managed=true".into(),
        "--label".into(),
        format!("gitrun.permanent={}", spec.permanent),
        "--label".into(),
        format!("gitrun.dynamic={}", !spec.permanent),
        "--label".into(),
        format!("gitrun.dock_target={}", spec.dock_target),
        "--cpus".into(),
        spec.cpus.into(),
        "--memory".into(),
        spec.memory.into(),
        "--restart".into(),
        if spec.ephemeral {
            "no".into()
        } else {
            "unless-stopped".into()
        },
    ]);

    if let Some(job_name) = spec.workflow_job_name {
        args.extend(["--label".into(), format!("gitrun.workflow_job={job_name}")]);
    }
    if let Some(run_id) = spec.workflow_run_id {
        args.extend(["--label".into(), format!("gitrun.workflow_run={run_id}")]);
    }

    if spec.is_windows {
        // Windows containers use the Windows container isolation primitive
        // instead of Linux namespaces/capabilities. GitRun routes these
        // runners only to configured VM-backed Windows Docker daemons; when
        // requested, Hyper-V container isolation adds another kernel boundary.
        if spec.windows_hyperv_isolation {
            args.extend(["--isolation".into(), "hyperv".into()]);
        }
        // Windows containers do not expose Linux filesystem/capability
        // primitives. Their VM is the primary guest boundary and Hyper-V
        // container isolation is requested by default, while the common
        // CPU/memory/network/cache policy remains enforced here.
        args.push("-e".into());
        args.push("GITRUN_SHARED_CACHE_DIR=C:\\gitrun\\shared".into());
    } else {
        args.extend(["--pids-limit".into(), spec.pids_limit.into()]);
        if spec.rootfs_read_only {
            args.push("--read-only".into());
        }
        match spec.home_backend {
            RunnerHomeBackend::Tmpfs => {
                args.extend([
                    "--tmpfs".into(),
                    format!(
                        "/home/runner/actions-runner:rw,nosuid,nodev,size={}",
                        spec.runner_home_size
                    ),
                ]);
            }
            RunnerHomeBackend::Volume => {
                // Docker creates this named volume automatically on `run`
                // if it doesn't exist yet — no separate `volume create`
                // step needed. One volume per runner container, named
                // after it, so cleanup in `remove_container_on` can find
                // it deterministically. `size=` isn't meaningful for a
                // disk-backed local-driver volume the way it is for
                // tmpfs, so `runner_home_size` is intentionally not
                // applied here — disk space is bounded by the host
                // filesystem, not this setting.
                args.extend([
                    "--mount".into(),
                    format!(
                        "type=volume,source={},target=/home/runner/actions-runner",
                        home_volume_name(spec.name)
                    ),
                ]);
            }
        }
        args.extend([
            "--tmpfs".into(),
            "/tmp:rw,nosuid,nodev,exec,size=256m".into(),
            "--tmpfs".into(),
            "/var/tmp:rw,nosuid,nodev,exec,size=64m".into(),
        ]);
        if spec.docker_socket_enabled && !spec.is_windows {
            args.extend([
                "--volume".into(),
                "/var/run/docker.sock:/var/run/docker.sock".into(),
                "--group-add".into(),
                spec.docker_socket_gid.into(),
            ]);
        }
        args.extend([
            "--group-add".into(),
            GITRUN_API_SOCKET_GID.into(),
            "--mount".into(),
            format!(
                "type=volume,source={},target=/var/lib/gitrun/shared",
                cache_volume
            ),
            // The API service lives on the host. The runner gets only this
            // Unix socket file, never the host API process or a workflow token.
            // Authentication happens from SO_PEERCRED + the container cgroup
            // on the host side before GSR authorizes the request.
            "--volume".into(),
            "/run/gitrun/api.sock:/run/gitrun/api.sock".into(),
            "-e".into(),
            "GITRUN_SHARED_CACHE_DIR=/var/lib/gitrun/shared".into(),
            "-e".into(),
            "CARGO_HOME=/var/lib/gitrun/shared/cargo".into(),
            "-e".into(),
            format!("CARGO_TARGET_DIR=/var/lib/gitrun/shared/cargo-target/{cache_key}"),
            "-e".into(),
            "PIP_CACHE_DIR=/var/lib/gitrun/shared/pip".into(),
            "-e".into(),
            "NPM_CONFIG_CACHE=/var/lib/gitrun/shared/npm".into(),
            "-e".into(),
            "HOME=/home/runner/actions-runner/home".into(),
            "-e".into(),
            "XDG_CONFIG_HOME=/home/runner/actions-runner/home/.config".into(),
            "-e".into(),
            "DOCKER_CONFIG=/tmp/docker-config".into(),
            "--tmpfs".into(),
            "/run/gitrun:rw,nosuid,nodev,noexec,size=16m,mode=0755".into(),
            "--tmpfs".into(),
            "/run/sudo:rw,nosuid,nodev,noexec,size=1m,mode=0755".into(),
        ]);
        if spec.docker_socket_hardening {
            args.extend(docker_socket_hardening_args());
        } else {
            // The kernel GSR supervisor still needs ptrace even when the
            // operator explicitly allows the broader unsafe-runner posture.
            // This does not claim the rest of Docker hardening is enabled.
            args.extend(gsr_supervisor_capability_args());
        }
    }

    if !spec.is_windows {
        args.extend([
            "--security-opt".into(),
            format!("seccomp={}", spec.seccomp_profile),
        ]);
        if !spec.apparmor_profile.trim().is_empty() {
            args.extend([
                "--security-opt".into(),
                format!("apparmor={}", spec.apparmor_profile),
            ]);
        }
    }

    for (name, value) in spec.gsr_policy_env {
        args.push("-e".into());
        args.push(format!("{name}={value}"));
    }

    args.extend([
        "-e".into(),
        format!("RUNNER_URL=https://github.com/{}", spec.repo),
        "-e".into(),
        format!("RUNNER_TOKEN={}", spec.registration_token),
        "-e".into(),
        format!("RUNNER_NAME={}", spec.name),
        "-e".into(),
        format!("RUNNER_LABELS={labels}"),
        "-e".into(),
        format!("RUNNER_EPHEMERAL={}", spec.ephemeral),
        "-e".into(),
        format!("RUNNER_DISABLE_UPDATE={}", spec.disable_update),
        "-e".into(),
        "GITRUN_API_SOCKET=/run/gitrun/api.sock".into(),
        spec.image.to_owned(),
    ]);
    // Secrets are inserted before the image argument (Docker requires -e
    // flags to precede the image name). Names were already validated as
    // safe env-var identifiers by the caller (see main.rs::vault_env_for_repo)
    // before reaching this point.
    let mut image_index = args.len() - 1;
    for (name, value) in spec.secret_env {
        args.insert(image_index, "-e".into());
        args.insert(image_index + 1, format!("{name}={value}"));
        image_index += 2;
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_checked_on(host, &arg_refs)?;
    Ok(())
}

/// Verifies that a Docker daemon is reachable and reports a minimal
/// server-version/API attestation before a VM becomes routable.
pub fn verify_docker_endpoint_health(host: &DockerHost) -> Result<String> {
    let output = run_checked_on(
        host,
        &[
            "version",
            "--format",
            "{{.Server.Version}}|{{.Server.APIVersion}}",
        ],
    )?;
    let value = output.trim();
    if value.is_empty() {
        return Err(DockerError::Command(
            "Docker daemon health check returned an empty version/API response".into(),
        ));
    }
    Ok(value.to_owned())
}

pub fn container_status(name: &str) -> Result<Option<String>> {
    container_status_string(&DockerHost::Local, name)
}

pub fn start_container(name: &str) -> Result<()> {
    run_checked(&["start", name])?;
    Ok(())
}

pub fn stop_container(name: &str) -> Result<()> {
    run_checked(&["stop", "--time", "10", name])?;
    Ok(())
}

pub fn container_id(name: &str) -> Result<Option<String>> {
    let output = run_on(&DockerHost::Local, &["inspect", "-f", "{{.Id}}", name])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if is_missing_container_error(&stderr) {
            return Ok(None);
        }
        return Err(DockerError::Command(if stderr.is_empty() {
            format!("docker inspect {name} failed")
        } else {
            stderr
        }));
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!value.is_empty()).then_some(value))
}

pub fn copy_to_container(
    container: &str,
    local_file: &std::path::Path,
    destination: &str,
) -> Result<()> {
    let source = local_file.to_string_lossy();
    run_checked(&["cp", source.as_ref(), &format!("{container}:{destination}")])?;
    Ok(())
}

pub fn remove_file_in_container(container: &str, path: &str) -> Result<()> {
    let output = exec_container(container, &["rm", "-f", "--", path])?;
    if !output.status.success() {
        return Err(DockerError::Command(if output.stderr.is_empty() {
            format!("unable to remove {path} from container {container}")
        } else {
            String::from_utf8_lossy(&output.stderr).trim().to_owned()
        }));
    }
    Ok(())
}

pub fn container_logs(name: &str) -> Result<String> {
    run_checked(&["logs", "--timestamps", name])
}

pub fn melt_filesystem(source: &str, target: &str) -> Result<()> {
    // Docker does not provide a literal "merge two container namespaces"
    // primitive. GitDockRun melt therefore merges the source container's
    // exported filesystem into the target container's writable filesystem.
    // Runtime/pseudo filesystems and the Actions runner installation are
    // deliberately excluded: the target remains the runner container.
    let mut export = Command::new("docker");
    export
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .env("DOCKER_HOST", "unix:///var/run/docker.sock")
        .args(["export", source])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut extract = Command::new("docker");
    extract
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .env("DOCKER_HOST", "unix:///var/run/docker.sock")
        .args([
            "exec",
            "-i",
            target,
            "tar",
            "-xpf",
            "-",
            "-C",
            "/",
            "--no-same-owner",
            "--exclude=./proc",
            "--exclude=./proc/*",
            "--exclude=./sys",
            "--exclude=./sys/*",
            "--exclude=./dev",
            "--exclude=./dev/*",
            "--exclude=./run",
            "--exclude=./run/*",
            "--exclude=./var/run",
            "--exclude=./var/run/*",
            "--exclude=./home/runner/actions-runner",
            "--exclude=./home/runner/actions-runner/*",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut export_child = export.spawn()?;
    let mut extract_child = extract.spawn()?;
    let mut export_stdout = export_child
        .stdout
        .take()
        .ok_or_else(|| DockerError::Command("docker export did not expose stdout".into()))?;
    let mut extract_stdin = extract_child
        .stdin
        .take()
        .ok_or_else(|| DockerError::Command("docker exec did not expose stdin".into()))?;

    let pipe = std::thread::spawn(move || std::io::copy(&mut export_stdout, &mut extract_stdin));

    let extract_output = extract_child.wait_with_output()?;
    let export_status = export_child.wait()?;
    let copied = pipe
        .join()
        .map_err(|_| DockerError::Command("melt pipe thread panicked".into()))?
        .map_err(DockerError::Spawn)?;

    if !export_status.success() {
        return Err(DockerError::Command(format!(
            "docker export {source} failed"
        )));
    }
    if !extract_output.status.success() {
        let stderr = String::from_utf8_lossy(&extract_output.stderr)
            .trim()
            .to_owned();
        return Err(DockerError::Command(if stderr.is_empty() {
            format!("filesystem melt into {target} failed")
        } else {
            stderr
        }));
    }

    if copied == 0 {
        return Err(DockerError::Command(
            "filesystem melt copied no data from source container".into(),
        ));
    }

    Ok(())
}

/// Identity discovered from a GitRun-managed runner container.
/// The API service derives this identity from the peer process cgroup,
/// rather than a workflow-visible token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRunnerIdentity {
    pub name: String,
    pub repository: String,
    /// Immutable Docker container ID resolved from the peer cgroup.
    pub container_id: String,
    /// Job/run labels are present on runner containers that were reserved
    /// for a specific workflow job. They are authoritative when present.
    pub workflow_job: Option<String>,
    pub workflow_run_id: Option<u64>,
}

/// Resolves the GitRun-managed runner container that owns a Unix-socket
/// peer process. On Linux, the connecting PID is taken from SO_PEERCRED and
/// its cgroup identifies the Docker container without exposing a secret to
/// the workflow environment.
#[cfg(target_os = "linux")]
pub fn runner_for_peer_pid(pid: i32) -> Result<Option<ApiRunnerIdentity>> {
    let cgroup = match std::fs::read_to_string(format!("/proc/{pid}/cgroup")) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(container_id) = container_id_from_cgroup(&cgroup) else {
        return Ok(None);
    };

    // Re-read the cgroup after the Docker lookup. If the peer process exited
    // and the PID was reused, the second lookup must not silently inherit the
    // old process's container identity.
    let cgroup_after = match std::fs::read_to_string(format!("/proc/{pid}/cgroup")) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if container_id_from_cgroup(&cgroup_after).as_deref() != Some(container_id.as_str()) {
        return Ok(None);
    }

    let output = run_on(
        &DockerHost::Local,
        &[
            "inspect",
            "-f",
            "{{.Id}}|{{.Name}}|{{index .Config.Labels \"gitrun.runner\"}}|{{index .Config.Labels \"gitrun.repo\"}}|{{index .Config.Labels \"gitrun.workflow_job\"}}|{{index .Config.Labels \"gitrun.workflow_run\"}}",
            &container_id,
        ],
    )?;

    if !output.status.success() {
        return Ok(None);
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let mut parts = raw.trim().splitn(6, '|');
    let resolved_id = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default().trim_start_matches('/');
    let runner_label = parts.next().unwrap_or_default();
    let repository = parts.next().unwrap_or_default();
    let workflow_job = parts.next().unwrap_or_default();
    let workflow_run_id = parts.next().and_then(|value| value.parse::<u64>().ok());

    if runner_label != "true" || resolved_id.is_empty() || name.is_empty() || repository.is_empty()
    {
        return Ok(None);
    }

    Ok(Some(ApiRunnerIdentity {
        name: name.to_owned(),
        repository: repository.to_owned(),
        container_id: resolved_id.to_owned(),
        workflow_job: (!workflow_job.is_empty()).then(|| workflow_job.to_owned()),
        workflow_run_id,
    }))
}

#[cfg(not(target_os = "linux"))]
pub fn runner_for_peer_pid(_pid: i32) -> Result<Option<ApiRunnerIdentity>> {
    Ok(None)
}

/// Extracts a Docker-style 64-hex container id from a Linux cgroup path.
#[cfg(target_os = "linux")]
fn container_id_from_cgroup(cgroup: &str) -> Option<String> {
    cgroup
        .split(|ch: char| !ch.is_ascii_hexdigit())
        .find(|part| part.len() == 64)
        .map(str::to_owned)
}

/// Executes a command in a managed container after the caller has already
/// passed the relevant GSR authorization gate.
pub fn exec_container(container: &str, args: &[&str]) -> Result<Output> {
    let mut command = Command::new("docker");
    command
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .env("DOCKER_HOST", "unix:///var/run/docker.sock")
        .arg("exec")
        .arg(container)
        .args(args);
    command.output().map_err(DockerError::Spawn)
}

/// Executes a command in a managed container and writes a bounded payload to
/// its stdin. Used by controlled GitWriteRun operations.
pub fn exec_container_with_stdin(container: &str, args: &[&str], input: &[u8]) -> Result<Output> {
    if input.len() > 16 * 1024 * 1024 {
        return Err(DockerError::Command(
            "container stdin payload exceeds the 16 MiB safety limit".into(),
        ));
    }

    let mut command = Command::new("docker");
    command
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .env("DOCKER_HOST", "unix:///var/run/docker.sock")
        .arg("exec")
        .arg("-i")
        .arg(container)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write as _;
        stdin.write_all(input)?;
    }
    child.wait_with_output().map_err(DockerError::Spawn)
}

/// Executes a command in one managed container while forwarding stdout and
/// stderr chunks to the caller as they arrive.
pub fn exec_container_stream<F>(container: &str, args: &[&str], mut on_output: F) -> Result<i32>
where
    F: FnMut(bool, &[u8]),
{
    let mut command = Command::new("docker");
    command
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .env("DOCKER_HOST", "unix:///var/run/docker.sock")
        .arg("exec")
        .arg(container)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| DockerError::Command("docker exec did not expose stdout".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| DockerError::Command("docker exec did not expose stderr".into()))?;

    let stdout_thread = thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        let mut chunks = Vec::new();
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => chunks.push(buffer[..count].to_vec()),
                Err(_) => break,
            }
        }
        chunks
    });
    let stderr_thread = thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        let mut chunks = Vec::new();
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => chunks.push(buffer[..count].to_vec()),
                Err(_) => break,
            }
        }
        chunks
    });

    for chunk in stdout_thread
        .join()
        .map_err(|_| DockerError::Command("stdout reader thread panicked".into()))?
    {
        on_output(false, &chunk);
    }
    for chunk in stderr_thread
        .join()
        .map_err(|_| DockerError::Command("stderr reader thread panicked".into()))?
    {
        on_output(true, &chunk);
    }

    Ok(child.wait()?.code().unwrap_or(1))
}

/// Extra `docker run` flags applied to a Linux runner container when
/// `RunnerSpec::docker_socket_hardening` is true (the default).
///
/// The runner container deliberately retains `CAP_SYS_PTRACE` because the
/// GSR PID-1 supervisor needs it to trace the unprivileged Actions runner and
/// inspect exec arguments after the kernel's seccomp TRACE stop. The
/// supervisor is the only long-lived root process in that container; it
/// permanently drops the runner child to the dedicated `runner` uid/gid
/// without retaining capabilities before the Actions workload starts.
///
/// The mounted `/var/run/docker.sock` remains a separate, intentional trust
/// boundary: anything that can successfully use the socket can control the
/// Docker daemon. These flags reduce the container's kernel attack surface
/// around that boundary without pretending the socket itself is a sandbox.
///
/// Dangerous capabilities stay removed: `SYS_ADMIN`, `NET_RAW`,
/// `SYS_MODULE`, and every capability other than the small bootstrap set
/// plus `SYS_PTRACE` are absent. `no-new-privileges` remains enabled.
fn docker_socket_hardening_args() -> Vec<String> {
    [
        "--cap-drop",
        "ALL",
        "--cap-add",
        "CHOWN",
        "--cap-add",
        "SETUID",
        "--cap-add",
        "SETGID",
        "--cap-add",
        "DAC_OVERRIDE",
        "--cap-add",
        "SYS_PTRACE",
        "--security-opt",
        "no-new-privileges",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn gsr_supervisor_capability_args() -> Vec<String> {
    ["--cap-add", "SYS_PTRACE"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// Appends `extra` to a comma-separated label list if not already present.
fn ensure_label(labels: &str, extra: &str) -> String {
    if labels.split(',').map(str::trim).any(|part| part == extra) {
        labels.to_owned()
    } else {
        format!("{labels},{extra}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_safe_characters() {
        assert_eq!(
            sanitize("owner/repo.name_v1-2", '_'),
            "owner_repo.name_v1-2"
        );
    }

    #[test]
    fn shared_cache_volume_falls_back_to_default_on_blank() {
        assert_eq!(
            shared_cache_volume(Some("   ")),
            SHARED_CACHE_VOLUME_DEFAULT
        );
        assert_eq!(shared_cache_volume(None), SHARED_CACHE_VOLUME_DEFAULT);
    }

    #[test]
    fn shared_cache_volume_respects_override() {
        assert_eq!(shared_cache_volume(Some("custom-vol")), "custom-vol");
    }

    #[test]
    fn ensure_label_does_not_duplicate() {
        assert_eq!(
            ensure_label("self-hosted,gitrun-ci", "gitrun-ci"),
            "self-hosted,gitrun-ci"
        );
    }

    #[test]
    fn ensure_label_appends_when_missing() {
        assert_eq!(
            ensure_label("self-hosted,Linux", "gitrun-ci"),
            "self-hosted,Linux,gitrun-ci"
        );
    }

    #[test]
    fn docker_socket_hardening_args_drop_all_then_add_back_only_safe_caps() {
        let args = docker_socket_hardening_args();
        assert!(args.windows(2).any(|w| w == ["--cap-drop", "ALL"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--security-opt", "no-new-privileges"]));
        // The GSR supervisor needs SYS_PTRACE; the unprivileged Actions
        // runner child drops its capability set before any job code runs.
        assert!(args.iter().any(|a| a == "SYS_PTRACE"));
        assert!(!args.iter().any(|a| a == "SYS_ADMIN" || a == "NET_RAW"));
    }

    #[test]
    fn gsr_supervisor_keeps_ptrace_when_optional_hardening_is_disabled() {
        let args = gsr_supervisor_capability_args();
        assert_eq!(args, vec!["--cap-add", "SYS_PTRACE"]);
    }

    #[test]
    fn parse_top_output_skips_header_and_blank_lines() {
        let raw = "COMMAND\ncargo build --release\nsh -c echo hi\n\n";
        assert_eq!(
            parse_top_output(raw),
            vec!["cargo build --release", "sh -c echo hi"]
        );
    }

    #[test]
    fn parse_top_output_on_header_only_is_empty() {
        assert_eq!(parse_top_output("COMMAND\n"), Vec::<String>::new());
    }

    #[test]
    fn parse_top_output_without_header_keeps_first_line() {
        let raw = "cargo build --release\nsh -c echo hi\n";
        assert_eq!(
            parse_top_output(raw),
            vec!["cargo build --release", "sh -c echo hi"]
        );
    }

    #[test]
    fn cache_path_key_is_collision_free_for_sanitization_collisions() {
        assert_ne!(cache_path_key("a_b/c"), cache_path_key("a/b_c"));
        assert_eq!(cache_path_key("owner/repo"), "6f776e65722f7265706f");
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn host_process_cgroup_without_container_id_is_not_a_runner_container() {
        assert_eq!(
            container_id_from_cgroup("0::/user.slice/user-1000.slice/session-42.scope"),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn docker_cgroup_parser_extracts_only_full_container_id() {
        let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let cgroup = format!("0::/system.slice/docker-{id}.scope");
        assert_eq!(container_id_from_cgroup(&cgroup).as_deref(), Some(id));
    }
}
