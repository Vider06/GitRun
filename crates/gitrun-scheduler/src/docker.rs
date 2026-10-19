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
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
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
const MAX_DOCKER_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const DOCKER_OUTPUT_TRUNCATION_MARKER: &[u8] = b"\n[Docker output truncated at 8 MiB]";

// Docker export archive members are normally root-relative (for example
// `proc/mounts`), but tar implementations and archive producers can also
// emit a leading `./`. Keep both spellings excluded: using only `./proc`
// silently fails to match root-relative members and can merge runtime/runner
// state into the destination container.
// Writable Docker mount destinations must be covered by these archived-path
// exclusions before MELT is allowed to extract a source image into the target.
// Otherwise tar could write through a mount into a host bind or persistent volume.
const MELT_TAR_EXCLUDE_ROOTS: &[&str] = &[
    "proc",
    "sys",
    "dev",
    "run",
    "var/run",
    "etc/hosts",
    "etc/hostname",
    "etc/resolv.conf",
    "tmp",
    "var/tmp",
    "var/lib/gitrun/shared",
    "home/runner/actions-runner",
];

const MELT_TAR_EXCLUDE_ARGS: &[&str] = &[
    "--exclude=./proc",
    "--exclude=./proc/*",
    "--exclude=proc",
    "--exclude=proc/*",
    "--exclude=./sys",
    "--exclude=./sys/*",
    "--exclude=sys",
    "--exclude=sys/*",
    "--exclude=./dev",
    "--exclude=./dev/*",
    "--exclude=dev",
    "--exclude=dev/*",
    "--exclude=./run",
    "--exclude=./run/*",
    "--exclude=run",
    "--exclude=run/*",
    "--exclude=./var/run",
    "--exclude=./var/run/*",
    "--exclude=var/run",
    "--exclude=var/run/*",
    "--exclude=./etc/hosts",
    "--exclude=./etc/hostname",
    "--exclude=./etc/resolv.conf",
    "--exclude=etc/hosts",
    "--exclude=etc/hostname",
    "--exclude=etc/resolv.conf",
    "--exclude=./tmp",
    "--exclude=./tmp/*",
    "--exclude=tmp",
    "--exclude=tmp/*",
    "--exclude=./var/tmp",
    "--exclude=./var/tmp/*",
    "--exclude=var/tmp",
    "--exclude=var/tmp/*",
    "--exclude=./var/lib/gitrun/shared",
    "--exclude=./var/lib/gitrun/shared/*",
    "--exclude=var/lib/gitrun/shared",
    "--exclude=var/lib/gitrun/shared/*",
    "--exclude=./home/runner/actions-runner",
    "--exclude=./home/runner/actions-runner/*",
    "--exclude=home/runner/actions-runner",
    "--exclude=home/runner/actions-runner/*",
];
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

fn read_bounded_output(mut reader: impl Read) -> std::io::Result<Vec<u8>> {
    let mut retained = Vec::with_capacity(MAX_DOCKER_OUTPUT_BYTES.min(64 * 1024));
    let mut chunk = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        let remaining = MAX_DOCKER_OUTPUT_BYTES.saturating_sub(retained.len());
        let keep = remaining.min(count);
        retained.extend_from_slice(&chunk[..keep]);
        truncated |= keep < count;
        // Continue draining after the cap so the child cannot deadlock on a
        // full pipe, while retained memory stays bounded.
    }
    if truncated {
        retained.extend_from_slice(DOCKER_OUTPUT_TRUNCATION_MARKER);
    }
    Ok(retained)
}

fn output_was_truncated(output: &[u8]) -> bool {
    output.ends_with(DOCKER_OUTPUT_TRUNCATION_MARKER)
}

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

#[cfg(unix)]
fn kill_command_group(pid: u32) {
    // run_on places each CLI in its own process group, so kill descendants
    // that inherited stdout/stderr as well as the direct child.
    let _ = unsafe { kill(-(pid as i32), 9) };
}

#[cfg(not(unix))]
fn kill_command_group(_pid: u32) {}

fn terminate_child(child: &mut Child) {
    kill_command_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

fn run_on(host: &DockerHost, args: &[&str]) -> Result<Output> {
    run_on_with_env(host, args, &[])
}

fn run_on_with_env(
    host: &DockerHost,
    args: &[&str],
    env_overrides: &[(String, String)],
) -> Result<Output> {
    let mut command = Command::new("/usr/bin/docker");

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
            // A plain remote endpoint must not inherit TLS settings from the
            // scheduler's environment. TLS is enabled only by RemoteTls,
            // whose endpoint and certificate directory are explicit.
            command
                .env_remove("DOCKER_TLS_VERIFY")
                .env_remove("DOCKER_CERT_PATH")
                .env("DOCKER_HOST", addr);
        }
        DockerHost::RemoteTls { endpoint, cert_dir } => {
            command
                .env("DOCKER_HOST", endpoint)
                .env("DOCKER_TLS_VERIFY", "1")
                .env("DOCKER_CERT_PATH", cert_dir);
        }
    }

    // Secret values travel in the Docker CLI process environment, not its
    // argv. Linux exposes argv through /proc/<pid>/cmdline to local observers
    // authorized to inspect the process; "-e NAME" forwards by name only.
    for (name, value) in env_overrides {
        command.env(name, value);
    }

    let command_name = args.first().copied().unwrap_or("command");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args)
        .spawn()?;
    let Some(stdout) = child.stdout.take() else {
        terminate_child(&mut child);
        return Err(DockerError::Command(format!(
            "docker {command_name} did not provide stdout"
        )));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_child(&mut child);
        return Err(DockerError::Command(format!(
            "docker {command_name} did not provide stderr"
        )));
    };

    // Drain both pipes concurrently with bounded retained output. A child
    // process must never be able to exhaust scheduler memory by flooding logs.
    let stdout_thread = thread::spawn(move || read_bounded_output(stdout));
    let stderr_thread = thread::spawn(move || read_bounded_output(stderr));

    let deadline = Instant::now() + DOCKER_COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // A CLI that exits while leaving a descendant holding a pipe
                // open would otherwise make the reader joins hang forever.
                kill_command_group(child.id());
                let stdout = stdout_thread
                    .join()
                    .map_err(|_| DockerError::Command("docker stdout reader panicked".into()))??;
                let stderr = stderr_thread
                    .join()
                    .map_err(|_| DockerError::Command("docker stderr reader panicked".into()))??;
                if output_was_truncated(&stdout) || output_was_truncated(&stderr) {
                    return Err(DockerError::Command(format!(
                        "docker {command_name} output exceeded the 8 MiB safety limit"
                    )));
                }
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) if Instant::now() >= deadline => {
                terminate_child(&mut child);
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(DockerError::Command(format!(
                    "docker {command_name} timed out after {}s",
                    DOCKER_COMMAND_TIMEOUT.as_secs()
                )));
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                terminate_child(&mut child);
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(DockerError::Spawn(error));
            }
        }
    }
}

fn run_checked_on(host: &DockerHost, args: &[&str]) -> Result<String> {
    run_checked_on_with_env(host, args, &[])
}

fn run_checked_on_with_env(
    host: &DockerHost,
    args: &[&str],
    env_overrides: &[(String, String)],
) -> Result<String> {
    let output = run_on_with_env(host, args, env_overrides)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let command_name = args.first().copied().unwrap_or("command");
        return Err(DockerError::Command(if stderr.is_empty() {
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

/// Collision-resistant, URL-safe repository prefix for runner container names.
/// The short readable prefix is cosmetic; the full unpadded Base64URL encoding
/// makes distinct repository identifiers map to distinct prefixes.
fn valid_prewarm_workload_id(code: &str) -> bool {
    let Some(digest) = code.strip_prefix("wk-") else {
        return false;
    };
    digest.len() == 16
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn runner_name_prefix(repo: &str) -> String {
    use base64::Engine as _;
    let readable: String = sanitize(repo, '-').chars().take(16).collect();
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(repo.as_bytes());
    format!("gitrun-{readable}-{encoded}-")
}

#[derive(Debug, Clone)]
pub struct ManagedContainer {
    pub name: String,
    /// Full immutable Docker identity captured with the inventory snapshot.
    /// Destructive reconciliation actions must use this ID, never re-resolve
    /// a mutable name after the plan has been computed.
    pub id: String,
    pub status: String,
    pub permanent: bool,
    pub dock_target: bool,
    pub workflow_job: Option<String>,
    pub workflow_run_id: Option<u64>,
    pub workflow_job_id: Option<u64>,
    /// Predicted recurring workload attached to a time-bounded warmup, if any.
    pub prewarm_workload_id: Option<String>,
    pub prewarm_target_epoch: Option<u64>,
    pub prewarm_hold_until_epoch: Option<u64>,
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
        let Some(id) = container_id_on(host, name)? else {
            // The container disappeared between "docker ps" and "inspect".
            continue;
        };
        let Some(status) = container_status_string(host, &id)? else {
            // The container disappeared between "docker ps" and "inspect" —
            // a normal reconciliation race. It must not turn into an empty
            // status that downstream code could interpret as a dead container.
            continue;
        };
        let permanent = container_is_permanent_on(host, &id).unwrap_or(true); // fail-safe: assume permanent, matching gitrun_updater_utility.py's upgrade-safety default
                                                                              // These labels affect security-sensitive scheduling and preservation.
                                                                              // Do not turn Docker inspect failures into "label absent": that could
                                                                              // make a GitDockRun target eligible for ordinary cleanup or lose its
                                                                              // job binding. A missing container is represented as None by the
                                                                              // helper; other Docker errors abort the snapshot and trigger a retry.
        let dock_target = container_label_on(host, &id, "gitrun.dock_target")?
            .is_some_and(|value| value.eq_ignore_ascii_case("true"));
        let workflow_job = container_label_on(host, &id, "gitrun.workflow_job")?;
        let workflow_run_id = container_label_on(host, &id, "gitrun.workflow_run")?
            .and_then(|value| value.parse::<u64>().ok());
        let workflow_job_id = match container_label_on(host, &id, "gitrun.workflow_job_id")? {
            None => None,
            Some(value) => Some(value.parse::<u64>().ok().filter(|id| *id > 0).ok_or_else(
                || {
                    DockerError::Command(format!(
                        "managed runner {name} has an invalid authoritative workflow job ID label"
                    ))
                },
            )?),
        };
        if workflow_job_id.is_some() && (workflow_run_id.is_none() || workflow_job.is_none()) {
            return Err(DockerError::Command(format!(
                "managed runner {name} has incomplete authoritative workflow job binding labels"
            )));
        }
        let raw_prewarm_code = container_label_on(host, &id, "gitrun.prewarm_workload_code")?;
        let raw_prewarm_target = container_label_on(host, &id, "gitrun.prewarm_target_epoch")?;
        let raw_prewarm_hold = container_label_on(host, &id, "gitrun.prewarm_hold_until_epoch")?;
        let (prewarm_workload_id, prewarm_target_epoch, prewarm_hold_until_epoch) =
            match (raw_prewarm_code, raw_prewarm_target, raw_prewarm_hold) {
                (None, None, None) => (None, None, None),
                (Some(code), Some(target), Some(hold)) => {
                    let target = target.parse::<u64>().map_err(|_| {
                        DockerError::Command(format!(
                            "managed runner {name} has an invalid Kitty prewarm target label"
                        ))
                    })?;
                    let hold = hold.parse::<u64>().map_err(|_| {
                        DockerError::Command(format!(
                            "managed runner {name} has an invalid Kitty prewarm hold label"
                        ))
                    })?;
                    if !valid_prewarm_workload_id(&code)
                        || target == 0
                        || !target.is_multiple_of(3_600)
                        || hold < target
                    {
                        return Err(DockerError::Command(format!(
                            "managed runner {name} has inconsistent Kitty prewarm labels"
                        )));
                    }
                    (Some(code), Some(target), Some(hold))
                }
                _ => {
                    return Err(DockerError::Command(format!(
                        "managed runner {name} has incomplete Kitty prewarm metadata"
                    )));
                }
            };
        // Labels/status above are looked up by name. Refuse to publish a
        // mixed snapshot if that name was rebound to a different container
        // while inventory metadata was being read.
        if container_id_on(host, name)?.as_deref() != Some(id.as_str()) {
            return Err(DockerError::Command(format!(
                "managed container {name} changed identity during inventory; retry reconciliation"
            )));
        }
        containers.push(ManagedContainer {
            name: name.to_owned(),
            id,
            status,
            permanent,
            dock_target,
            workflow_job,
            workflow_run_id,
            workflow_job_id,
            prewarm_workload_id,
            prewarm_target_epoch,
            prewarm_hold_until_epoch,
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

fn container_label_format(label: &str) -> String {
    // format! requires doubled braces to emit Docker's double-brace Go template.
    format!("{{{{index .Config.Labels \"{label}\"}}}}")
}

fn container_label_on(host: &DockerHost, name: &str, label: &str) -> Result<Option<String>> {
    let format = container_label_format(label);
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

/// Persistent, root-only lifecycle metadata. Unlike the runner-home volume,
/// workflow code must not be able to forge or remove the dock-only marker.
fn control_volume_name(container_name: &str) -> String {
    format!("{container_name}-control")
}

fn control_volume_mount_argument(container_name: &str) -> String {
    format!(
        "type=volume,source={},target=/var/lib/gitrun/control,readonly",
        control_volume_name(container_name)
    )
}

/// Set or clear the dock-only marker from the host side. The control volume is
/// mounted read-only inside the runner container because even a root process
/// created by an explicitly enabled install API must not be able to forge
/// lifecycle state. Only the privileged scheduler writes the volume contents.
#[cfg(target_os = "linux")]
pub fn set_dock_only_marker(container: &str, enabled: bool) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let resolved_name = run_checked(&["inspect", "--format", "{{.Name}}", container])?;
    let name = resolved_name.trim().trim_start_matches('/');
    if name.is_empty()
        || name.contains('/')
        || name.chars().any(char::is_control)
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(DockerError::Command(
            "Docker returned an unsafe name for the dock-only control volume".into(),
        ));
    }

    let volume = control_volume_name(name);
    let mount_inventory = run_checked(&[
        "inspect",
        "--format",
        "{{range .Mounts}}{{if eq .Destination \"/var/lib/gitrun/control\"}}{{.Name}}|{{.RW}}{{end}}{{end}}",
        container,
    ])?;
    validate_control_volume_attachment(&volume, &mount_inventory).map_err(DockerError::Command)?;

    let raw_mountpoint =
        run_checked(&["volume", "inspect", "--format", "{{.Mountpoint}}", &volume])?;
    let mountpoint = PathBuf::from(raw_mountpoint.trim());
    if !mountpoint.is_absolute()
        || mountpoint.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        return Err(DockerError::Command(
            "Docker returned an unsafe control-volume mountpoint".into(),
        ));
    }

    // Validate the entire host path chain before touching the Docker volume.
    // A malicious symlink or non-root-writable ancestor must not redirect a
    // privileged host write into another location.
    let mut prefix = PathBuf::from("/");
    for component in mountpoint.components() {
        match component {
            Component::RootDir => continue,
            Component::Normal(part) => prefix.push(part),
            _ => {
                return Err(DockerError::Command(
                    "control-volume mountpoint contains unsupported path components".into(),
                ))
            }
        }
        let metadata = std::fs::symlink_metadata(&prefix)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != 0
            || metadata.permissions().mode() & 0o022 != 0
        {
            return Err(DockerError::Command(format!(
                "control-volume path component {} is not a trusted root-owned directory",
                prefix.display()
            )));
        }
    }

    set_dock_only_marker_in_volume(&mountpoint, enabled, 0)
}

fn validate_control_volume_attachment(
    expected_volume: &str,
    mount_inventory: &str,
) -> std::result::Result<(), String> {
    let Some((actual_volume, read_write)) = mount_inventory.trim().split_once('|') else {
        return Err(
            "container is missing the dedicated GitRun dock-only control-volume mount".into(),
        );
    };
    if actual_volume != expected_volume {
        return Err(
            "container control-volume mount does not match its deterministic runner identity"
                .into(),
        );
    }
    if read_write != "false" {
        return Err(
            "container control-volume mount must be read-only so container-root code cannot forge lifecycle state".into(),
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn set_dock_only_marker(_container: &str, _enabled: bool) -> Result<()> {
    Err(DockerError::Command(
        "host-managed dock-only markers are supported only for local Linux Docker containers"
            .into(),
    ))
}

#[cfg(unix)]
fn set_dock_only_marker_in_volume(
    control_dir: &Path,
    enabled: bool,
    expected_uid: u32,
) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let directory = std::fs::symlink_metadata(control_dir)?;
    if directory.file_type().is_symlink()
        || !directory.is_dir()
        || directory.uid() != expected_uid
        || directory.permissions().mode() & 0o077 != 0
    {
        return Err(DockerError::Command(
            "dock-only control directory must be a private, real directory owned by the expected identity"
                .into(),
        ));
    }

    let marker = control_dir.join("dock-only");
    let existing = match std::fs::symlink_metadata(&marker) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.nlink() != 1
                || metadata.uid() != expected_uid
            {
                return Err(DockerError::Command(
                    "existing dock-only marker is not a trusted regular file".into(),
                ));
            }
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };

    if !enabled {
        if existing.is_some() {
            std::fs::remove_file(&marker)?;
            std::fs::File::open(control_dir)?.sync_all()?;
        }
        return Ok(());
    }

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| DockerError::Command(error.to_string()))?
        .as_nanos();
    let temporary = control_dir.join(format!(".dock-only.tmp.{}.{}", std::process::id(), nonce));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(b"GitRun dock-only v1\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, &marker)?;
        std::fs::File::open(control_dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
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
    // Resolve deterministic home/control volume names before removing the container.
    // Callers may pass an immutable container ID, so deriving volume names
    // from that ID would leak per-runner state.
    let (home_volume, control_volume, home_volume_resolution_error) =
        match run_on(host, &["inspect", "-f", "{{.Name}}", name_or_id]) {
            Ok(output) if output.status.success() => {
                let resolved = String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .trim_start_matches('/')
                    .to_owned();
                if resolved.is_empty() {
                    (
                        None,
                        None,
                        Some(format!(
                            "docker inspect returned an empty name for {name_or_id}"
                        )),
                    )
                } else {
                    // Resolve volume names while the container still exists;
                    // cleanup callers may have supplied an immutable container ID.
                    (
                        Some(home_volume_name(&resolved)),
                        Some(control_volume_name(&resolved)),
                        None,
                    )
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                if is_missing_container_error(&stderr) {
                    // Another cleanup worker may already have removed it.
                    (None, None, None)
                } else {
                    (
                        None,
                        None,
                        Some(if stderr.is_empty() {
                            format!("unable to resolve runner/control volumes for {name_or_id}")
                        } else {
                            format!(
                            "unable to resolve runner/control volumes for {name_or_id}: {stderr}"
                        )
                        }),
                    )
                }
            }
            Err(error) => (
                None,
                None,
                Some(format!(
                    "unable to resolve runner/control volumes for {name_or_id}: {error}"
                )),
            ),
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
        let output = run_on(host, &["volume", "rm", "-f", &volume])?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let normalized = stderr.to_ascii_lowercase();
            // Cleanup is idempotent: a volume already removed by another
            // reconciliation worker is not a failure, but any other error
            // must be surfaced because the volume may retain runner credentials.
            if !(normalized.contains("no such volume") || normalized.contains("volume not found")) {
                return Err(DockerError::Command(if stderr.is_empty() {
                    format!("container removed but runner home volume cleanup failed for {volume}")
                } else {
                    format!("container removed but runner home volume cleanup failed for {volume}: {stderr}")
                }));
            }
        }
    }

    // The dock-only marker lives on its own root-only persistent volume, not
    // in the read-only container rootfs or the runner-owned home volume.
    // Remove it with the container's home volume so marker state can't leak
    // into a later container that reuses the deterministic name.
    if let Some(volume) = control_volume {
        let output = run_on(host, &["volume", "rm", "-f", &volume])?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let normalized = stderr.to_ascii_lowercase();
            if !(normalized.contains("no such volume") || normalized.contains("volume not found")) {
                return Err(DockerError::Command(if stderr.is_empty() {
                    format!("container removed but control volume cleanup failed for {volume}")
                } else {
                    format!("container removed but control volume cleanup failed for {volume}: {stderr}")
                }));
            }
        }
    }
    if let Some(error) = home_volume_resolution_error {
        return Err(DockerError::Command(format!(
            "container removal completed but runner home volume cleanup could not be verified: {error}"
        )));
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
    /// Remove GitHub's default self-hosted/OS labels for exclusive workload
    /// family routing. Only GitRun job-family labels may remain on such runners.
    pub no_default_labels: bool,
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
    /// GitHub's unique workflow job ID paired with the run ID.
    pub workflow_job_id: Option<u64>,
    /// Metadata for one recurring-workload prewarm. These labels are for
    /// deduplication/retention only; GitHub dispatch still uses runner labels.
    pub prewarm_workload_id: Option<&'a str>,
    pub prewarm_target_epoch: Option<u64>,
    pub prewarm_hold_until_epoch: Option<u64>,
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

/// Insert only environment variable names into Docker argv and keep values
/// for the spawned CLI's private environment. This avoids putting runner
/// registration tokens or GitVault plaintext in /proc/<pid>/cmdline.
fn append_env_passthrough_args(
    args: &mut Vec<String>,
    env_values: &[(String, String)],
    forwarded_env: &mut Vec<(String, String)>,
) {
    let mut image_index = args.len().saturating_sub(1);
    for (name, value) in env_values {
        args.insert(image_index, "-e".into());
        args.insert(image_index + 1, name.clone());
        forwarded_env.push((name.clone(), value.clone()));
        image_index += 2;
    }
}

fn validate_secret_env(secret_env: &[(String, String)]) -> Result<()> {
    const RESERVED_EXACT: &[&str] = &[
        "HOME",
        "PATH",
        "DOCKER_CONFIG",
        "XDG_CONFIG_HOME",
        "NPM_CONFIG_CACHE",
        "BASH_ENV",
        "ENV",
        "IFS",
        "PYTHONPATH",
        "PYTHONHOME",
        "PYTHONSTARTUP",
        "PYTHONUSERBASE",
        "NODE_OPTIONS",
        "NODE_PATH",
        "RUBYOPT",
        "RUBYLIB",
        "PERL5OPT",
        "PERL5LIB",
        "PHPRC",
        "PHP_INI_SCAN_DIR",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "SHELLOPTS",
        "BASHOPTS",
        "GLIBC_TUNABLES",
        "GCONV_PATH",
        "LOCPATH",
        "NLSPATH",
        "MALLOC_TRACE",
    ];
    for (name, value) in secret_env {
        let mut chars = name.chars();
        let valid_name = matches!(chars.next(), Some(c) if c.is_ascii_uppercase() || c == '_')
            && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !valid_name {
            return Err(DockerError::Command(format!(
                "refusing GitVault secret with invalid environment variable name: {name:?}"
            )));
        }
        if name.starts_with("RUNNER_")
            // Treat GitRun/GitHub/ACTIONS and Git config namespaces as runtime
            // control variables, not user-defined secrets. The Docker CLI
            // injects Vault values before the entrypoint's privilege drop, and
            // later runner startup consumes these namespaces for identity,
            // API routing, hooks, configuration and action runtime credentials.
            || name.starts_with("GITRUN_")
            || name.starts_with("GITHUB_")
            || name.starts_with("ACTIONS_")
            || name.starts_with("GIT_CONFIG_")
            || name.starts_with("GITRUN_GSR_")
            // GitVault values are passed to Docker before the entrypoint starts.
            // The dynamic loader processes LD_* variables before the root
            // entrypoint can unset them; LD_AUDIT can load a shared object into
            // root-run startup commands before the privilege drop.
            || name.starts_with("LD_")
            || RESERVED_EXACT.contains(&name.as_str())
        {
            return Err(DockerError::Command(format!(
                "refusing GitVault secret that overrides a protected runner variable: {name}"
            )));
        }
        if value.contains('\0') {
            return Err(DockerError::Command(format!(
                "refusing GitVault secret {name}: environment values cannot contain NUL"
            )));
        }
    }
    Ok(())
}

fn validate_runner_network_metadata(
    network: &str,
    metadata: &str,
    require_inter_container_isolation: bool,
) -> std::result::Result<(), String> {
    let mut fields = metadata.trim().split('|');
    let driver = fields.next().unwrap_or("");
    let managed = fields.next().unwrap_or("");
    let icc = fields.next();

    if driver != "bridge" || managed != "true" {
        return Err(format!(
            "refusing runner network {network}: expected a GitRun-managed bridge (driver={driver}, gitrun.managed={managed})"
        ));
    }
    if require_inter_container_isolation && icc != Some("false") {
        return Err(format!(
            "refusing runner network {network}: Linux runner bridges must set com.docker.network.bridge.enable_icc=false; drain connected runners and recreate the network before retrying"
        ));
    }
    Ok(())
}

fn ensure_runner_network(
    host: &DockerHost,
    network: &str,
    require_inter_container_isolation: bool,
) -> Result<()> {
    let inspect_format = if require_inter_container_isolation {
        "{{.Driver}}|{{index .Labels \"gitrun.managed\"}}|{{index .Options \"com.docker.network.bridge.enable_icc\"}}"
    } else {
        "{{.Driver}}|{{index .Labels \"gitrun.managed\"}}"
    };
    let output = run_on(
        host,
        &["network", "inspect", "--format", inspect_format, network],
    )?;
    if output.status.success() {
        let metadata = String::from_utf8_lossy(&output.stdout);
        validate_runner_network_metadata(network, &metadata, require_inter_container_isolation)
            .map_err(DockerError::Command)?;
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

    // Linux runner containers share this stable, GitRun-owned bridge so
    // operators can attach one host firewall/proxy policy. Disable bridge ICC:
    // a custom bridge alone still allows every attached runner to reach every
    // other runner's listening ports and Docker DNS names. Existing bridges
    // without ICC=false are rejected above rather than silently trusted.
    let mut create_args = vec!["network", "create", "--driver", "bridge"];
    if require_inter_container_isolation {
        create_args.extend(["--opt", "com.docker.network.bridge.enable_icc=false"]);
    }
    create_args.extend(["--label", "gitrun.managed=true", network]);
    run_checked_on(host, &create_args)?;
    Ok(())
}

fn remote_linux_runner_api_unsupported(host: &DockerHost, is_windows: bool) -> bool {
    !is_windows && !matches!(host, DockerHost::Local)
}

pub fn create_runner_on(host: &DockerHost, spec: &RunnerSpec) -> Result<()> {
    // A Docker bind mount is resolved by the daemon, not by this scheduler.
    // Linux runner containers require the host GitRun API socket; until the
    // socket is securely forwarded into remote guests, refuse to create a
    // runner that would silently lack the mandatory API channel.
    if remote_linux_runner_api_unsupported(host, spec.is_windows) {
        return Err(DockerError::Command(
            "Linux runners on remote Docker hosts are unsupported until the GitRun API socket is securely forwarded".into(),
        ));
    }

    match (
        spec.prewarm_workload_id,
        spec.prewarm_target_epoch,
        spec.prewarm_hold_until_epoch,
    ) {
        (None, None, None) => {}
        (Some(code), Some(target), Some(hold))
            if valid_prewarm_workload_id(code)
                && target > 0
                && target.is_multiple_of(3_600)
                && hold >= target => {}
        _ => {
            return Err(DockerError::Command(
                "Kitty prewarm runner metadata is incomplete or outside supported bounds".into(),
            ));
        }
    }

    if spec.no_default_labels {
        let route_labels = vec![spec.labels.to_owned()];
        if spec.is_windows {
            return Err(DockerError::Command(
                "exclusive workload-family labels require the managed Linux runner bootstrap; Windows runner bootstrap has no verified no-default-labels contract"
                    .into(),
            ));
        }
        if spec.permanent
            || spec.dock_target
            || gitrun_smart_scheduler::workload_id_from_routing_labels(&route_labels).is_none()
        {
            return Err(DockerError::Command(
                "no-default-label routing is allowed only for one valid GitRun workload-family label on an ordinary dynamic runner"
                    .into(),
            ));
        }
    }

    validate_secret_env(spec.secret_env)?;
    ensure_runner_network(host, spec.network, !spec.is_windows)?;
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
    let labels = if spec.no_default_labels {
        spec.labels.trim().to_owned()
    } else {
        ensure_label(spec.labels, "gitrun-ci")
    };

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
    if let Some(job_id) = spec.workflow_job_id {
        args.extend(["--label".into(), format!("gitrun.workflow_job_id={job_id}")]);
    }
    if let (Some(code), Some(target), Some(hold)) = (
        spec.prewarm_workload_id,
        spec.prewarm_target_epoch,
        spec.prewarm_hold_until_epoch,
    ) {
        args.extend([
            "--label".into(),
            format!("gitrun.prewarm_workload_code={code}"),
            "--label".into(),
            format!("gitrun.prewarm_target_epoch={target}"),
            "--label".into(),
            format!("gitrun.prewarm_hold_until_epoch={hold}"),
        ]);
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
            // A dedicated volume persists root-owned lifecycle markers across
            // stop/start while the container rootfs remains read-only. The
            // runner cannot access its 0700 directory or forge dock-only state.
            "--mount".into(),
            control_volume_mount_argument(spec.name),
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

    // Forward policy values through the child environment too; argv carries
    // only names. Although these are configuration rather than Vault secrets,
    // they may be lengthy or sensitive operator-controlled policy strings.
    let mut forwarded_env: Vec<(String, String)> = spec
        .gsr_policy_env
        .iter()
        .map(|(name, value)| ((*name).clone(), value.clone()))
        .collect();
    for (name, _) in spec.gsr_policy_env {
        args.push("-e".into());
        args.push((*name).clone());
    }

    forwarded_env.push((
        "RUNNER_TOKEN".to_owned(),
        spec.registration_token.to_owned(),
    ));
    args.extend([
        "-e".into(),
        format!("RUNNER_URL=https://github.com/{}", spec.repo),
        "-e".into(),
        "RUNNER_TOKEN".into(),
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
    if spec.no_default_labels {
        args.extend(["-e".into(), "GITRUN_RUNNER_NO_DEFAULT_LABELS=true".into()]);
    }

    // Docker accepts "-e NAME" and reads NAME from the CLI process
    // environment. Keep registration tokens and Vault plaintext out of argv;
    // only variable names are visible in the process listing.
    append_env_passthrough_args(&mut args, spec.secret_env, &mut forwarded_env);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_checked_on_with_env(host, &arg_refs, &forwarded_env)?;
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
    container_id_on(&DockerHost::Local, name)
}

pub fn container_id_on(host: &DockerHost, name: &str) -> Result<Option<String>> {
    let output = run_on(host, &["inspect", "-f", "{{.Id}}", name])?;
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

fn melt_excludes_mount_destination(destination: &str) -> bool {
    let path = std::path::Path::new(destination);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return false;
    }
    let Ok(relative) = path.strip_prefix("/") else {
        return false;
    };
    MELT_TAR_EXCLUDE_ROOTS.iter().any(|root| {
        let root = std::path::Path::new(root);
        relative == root || relative.starts_with(root)
    })
}

/// Refuse MELT if Docker reports a writable target mount that the archive
/// exclusion list does not cover. GitDockRun lease checks alone cannot detect
/// ordinary Docker volumes/binds created outside the GitDockRun mount API.
pub fn validate_melt_target_mounts_on(host: &DockerHost, target_id: &str) -> Result<()> {
    let output = run_on(
        host,
        &["inspect", "--format", "{{json .Mounts}}", target_id],
    )?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(DockerError::Command(if detail.is_empty() {
            format!("cannot inspect target mounts for MELT target {target_id}")
        } else {
            format!("cannot inspect target mounts for MELT target {target_id}: {detail}")
        }));
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let mounts: Value = serde_json::from_str(raw.trim())?;
    validate_melt_target_mount_inventory(target_id, &mounts)
}

fn validate_melt_target_mount_inventory(target_id: &str, mounts: &Value) -> Result<()> {
    let mounts = mounts.as_array().ok_or_else(|| {
        DockerError::Command(format!(
            "Docker returned a non-array mount inventory for MELT target {target_id}"
        ))
    })?;
    for mount in mounts {
        let destination = mount
            .get("Destination")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                DockerError::Command(format!(
                    "Docker returned a mount without a destination for MELT target {target_id}"
                ))
            })?;
        let read_write = mount.get("RW").and_then(Value::as_bool).ok_or_else(|| {
            DockerError::Command(format!(
                "Docker returned a mount without an RW flag for MELT target {target_id}"
            ))
        })?;
        if read_write && !melt_excludes_mount_destination(destination) {
            return Err(DockerError::Command(format!(
                "GitDockRun MELT refuses target {target_id}: writable Docker mount {destination:?} is not protected by the archive exclusions"
            )));
        }
    }
    Ok(())
}

pub fn melt_filesystem(source: &str, target: &str) -> Result<()> {
    // Docker does not provide a literal "merge two container namespaces"
    // primitive. GitDockRun melt merges an exported source filesystem into
    // the target writable filesystem, excluding runtime/runner state.
    let mut export = Command::new("/usr/bin/docker");
    export
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .env("DOCKER_HOST", "unix:///var/run/docker.sock")
        .args(["export", source])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut extract = Command::new("/usr/bin/docker");
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
        ])
        .args(MELT_TAR_EXCLUDE_ARGS)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        export.process_group(0);
        extract.process_group(0);
    }

    let mut export_child = export.spawn()?;
    let mut extract_child = match extract.spawn() {
        Ok(child) => child,
        Err(error) => {
            terminate_child(&mut export_child);
            return Err(DockerError::Spawn(error));
        }
    };

    let export_stdout = match export_child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_child(&mut export_child);
            terminate_child(&mut extract_child);
            return Err(DockerError::Command(
                "docker export did not expose stdout".into(),
            ));
        }
    };
    let extract_stdin = match extract_child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            terminate_child(&mut export_child);
            terminate_child(&mut extract_child);
            return Err(DockerError::Command(
                "docker exec did not expose stdin".into(),
            ));
        }
    };
    let export_stderr = match export_child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_child(&mut export_child);
            terminate_child(&mut extract_child);
            return Err(DockerError::Command(
                "docker export did not expose stderr".into(),
            ));
        }
    };
    let extract_stdout = match extract_child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_child(&mut export_child);
            terminate_child(&mut extract_child);
            return Err(DockerError::Command(
                "docker exec did not expose stdout".into(),
            ));
        }
    };
    let extract_stderr = match extract_child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_child(&mut export_child);
            terminate_child(&mut extract_child);
            return Err(DockerError::Command(
                "docker exec did not expose stderr".into(),
            ));
        }
    };

    // Drain both stderr pipes and extract stdout while streaming the tar bytes.
    // Otherwise a full stderr pipe can deadlock the export->extract pipeline.
    let export_stderr_thread = thread::spawn(move || read_bounded_output(export_stderr));
    let extract_stdout_thread = thread::spawn(move || read_bounded_output(extract_stdout));
    let extract_stderr_thread = thread::spawn(move || read_bounded_output(extract_stderr));
    let pipe_thread = thread::spawn(move || {
        let mut export_stdout = export_stdout;
        let mut extract_stdin = extract_stdin;
        std::io::copy(&mut export_stdout, &mut extract_stdin)
    });

    let deadline = Instant::now() + DOCKER_COMMAND_TIMEOUT;
    let mut export_status = None;
    let mut extract_status = None;
    while export_status.is_none() || extract_status.is_none() {
        if export_status.is_none() {
            match export_child.try_wait() {
                Ok(status) => {
                    if status.is_some() {
                        kill_command_group(export_child.id());
                    }
                    export_status = status;
                }
                Err(error) => {
                    terminate_child(&mut export_child);
                    terminate_child(&mut extract_child);
                    let _ = pipe_thread.join();
                    let _ = export_stderr_thread.join();
                    let _ = extract_stdout_thread.join();
                    let _ = extract_stderr_thread.join();
                    return Err(DockerError::Spawn(error));
                }
            }
        }
        if extract_status.is_none() {
            match extract_child.try_wait() {
                Ok(status) => {
                    if status.is_some() {
                        kill_command_group(extract_child.id());
                    }
                    extract_status = status;
                }
                Err(error) => {
                    terminate_child(&mut export_child);
                    terminate_child(&mut extract_child);
                    let _ = pipe_thread.join();
                    let _ = export_stderr_thread.join();
                    let _ = extract_stdout_thread.join();
                    let _ = extract_stderr_thread.join();
                    return Err(DockerError::Spawn(error));
                }
            }
        }
        if export_status.is_some() && extract_status.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            terminate_child(&mut export_child);
            terminate_child(&mut extract_child);
            let _ = pipe_thread.join();
            let _ = export_stderr_thread.join();
            let _ = extract_stdout_thread.join();
            let _ = extract_stderr_thread.join();
            return Err(DockerError::Command(format!(
                "filesystem melt timed out after {}s",
                DOCKER_COMMAND_TIMEOUT.as_secs()
            )));
        }
        thread::sleep(Duration::from_millis(50));
    }

    let copied = pipe_thread
        .join()
        .map_err(|_| DockerError::Command("melt pipe thread panicked".into()))?
        .map_err(DockerError::Spawn)?;
    let export_stderr = export_stderr_thread
        .join()
        .map_err(|_| DockerError::Command("docker export stderr reader panicked".into()))??;
    let extract_stdout = extract_stdout_thread
        .join()
        .map_err(|_| DockerError::Command("docker exec stdout reader panicked".into()))??;
    let extract_stderr = extract_stderr_thread
        .join()
        .map_err(|_| DockerError::Command("docker exec stderr reader panicked".into()))??;
    if output_was_truncated(&export_stderr)
        || output_was_truncated(&extract_stdout)
        || output_was_truncated(&extract_stderr)
    {
        return Err(DockerError::Command(
            "filesystem melt diagnostics exceeded the 8 MiB safety limit".into(),
        ));
    }

    let export_status = export_status.expect("loop exits only when export status is available");
    let extract_status = extract_status.expect("loop exits only when extract status is available");
    if !export_status.success() {
        let detail = String::from_utf8_lossy(&export_stderr).trim().to_owned();
        return Err(DockerError::Command(if detail.is_empty() {
            format!("docker export {source} failed")
        } else {
            format!("docker export {source} failed: {detail}")
        }));
    }
    if !extract_status.success() {
        let detail = String::from_utf8_lossy(&extract_stderr).trim().to_owned();
        return Err(DockerError::Command(if detail.is_empty() {
            format!("filesystem melt into {target} failed")
        } else {
            format!("filesystem melt into {target} failed: {detail}")
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
    pub workflow_job_id: Option<u64>,
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
            "{{.Id}}|{{.Name}}|{{index .Config.Labels \"gitrun.runner\"}}|{{index .Config.Labels \"gitrun.repo\"}}|{{index .Config.Labels \"gitrun.workflow_job\"}}|{{index .Config.Labels \"gitrun.workflow_run\"}}|{{index .Config.Labels \"gitrun.workflow_job_id\"}}",
            &container_id,
        ],
    )?;

    if !output.status.success() {
        return Ok(None);
    }

    // A peer may exit and its PID may be reused while Docker is being
    // queried. Re-check the cgroup after the lookup before trusting its labels.
    let cgroup_final = match std::fs::read_to_string(format!("/proc/{pid}/cgroup")) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if container_id_from_cgroup(&cgroup_final).as_deref() != Some(container_id.as_str()) {
        return Ok(None);
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let mut parts = raw.trim().splitn(7, '|');
    let resolved_id = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default().trim_start_matches('/');
    let runner_label = parts.next().unwrap_or_default();
    let repository = parts.next().unwrap_or_default();
    let workflow_job = parts.next().unwrap_or_default();
    let workflow_run_id = parts.next().and_then(|value| value.parse::<u64>().ok());
    let workflow_job_id = match parts.next() {
        None | Some("") => None,
        Some(value) => match value.parse::<u64>() {
            Ok(id) if id > 0 => Some(id),
            _ => return Ok(None),
        },
    };

    if runner_label != "true"
        || resolved_id != container_id
        || name.is_empty()
        || repository.is_empty()
        || (workflow_job_id.is_some() && (workflow_job.is_empty() || workflow_run_id.is_none()))
    {
        return Ok(None);
    }

    Ok(Some(ApiRunnerIdentity {
        name: name.to_owned(),
        repository: repository.to_owned(),
        container_id: resolved_id.to_owned(),
        workflow_job: (!workflow_job.is_empty()).then(|| workflow_job.to_owned()),
        workflow_run_id,
        workflow_job_id,
    }))
}

#[cfg(not(target_os = "linux"))]
pub fn runner_for_peer_pid(_pid: i32) -> Result<Option<ApiRunnerIdentity>> {
    Ok(None)
}

/// Extracts a Docker-style 64-hex container id from a Linux cgroup path.
#[cfg(target_os = "linux")]
fn container_id_from_cgroup(cgroup: &str) -> Option<String> {
    for component in cgroup.split('/') {
        if is_full_container_id(component) {
            return Some(component.to_owned());
        }
        let scoped_id = component
            .strip_prefix("docker-")
            .and_then(|value| value.strip_suffix(".scope"))
            .or_else(|| {
                component
                    .strip_prefix("cri-containerd-")
                    .and_then(|value| value.strip_suffix(".scope"))
            });
        if let Some(id) = scoped_id.filter(|value| is_full_container_id(value)) {
            return Some(id.to_owned());
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn is_full_container_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Executes a command in a managed container after the caller has already
/// passed the relevant GSR authorization gate.
fn exec_user_arguments(user: Option<&str>) -> Vec<&str> {
    match user {
        Some(user) => vec!["--user", user],
        None => Vec::new(),
    }
}

pub fn exec_container(container: &str, args: &[&str]) -> Result<Output> {
    exec_container_as(container, None, args)
}

/// Executes workflow-controlled commands as the non-root Actions runner account.
/// Do not rely on the image's configured USER: GitRun's entrypoint intentionally
/// starts as root to initialize protected runtime files before dropping the
/// long-lived runner process to its unprivileged account.
pub fn exec_container_as_runner(container: &str, args: &[&str]) -> Result<Output> {
    exec_container_as(container, Some("runner"), args)
}

fn exec_container_as(container: &str, user: Option<&str>, args: &[&str]) -> Result<Output> {
    // Reuse the bounded-output/timeout runner instead of Command::output(),
    // which would buffer arbitrary workflow-controlled output in scheduler RAM.
    let mut command_args = vec!["exec"];
    command_args.extend(exec_user_arguments(user));
    command_args.push(container);
    command_args.extend_from_slice(args);
    run_on(&DockerHost::Local, &command_args)
}

/// Executes a command in a managed container and writes a bounded payload to
/// its stdin. Used by controlled GitWriteRun operations.
pub fn exec_container_with_stdin(container: &str, args: &[&str], input: &[u8]) -> Result<Output> {
    exec_container_with_stdin_as(container, None, args, input)
}

/// Writes a bounded payload into a container as the unprivileged Actions runner
/// account. User-controlled paths must never gain container-root access simply
/// because the host scheduler performs the Docker API call.
pub fn exec_container_with_stdin_as_runner(
    container: &str,
    args: &[&str],
    input: &[u8],
) -> Result<Output> {
    exec_container_with_stdin_as(container, Some("runner"), args, input)
}

fn exec_container_with_stdin_as(
    container: &str,
    user: Option<&str>,
    args: &[&str],
    input: &[u8],
) -> Result<Output> {
    if input.len() > 16 * 1024 * 1024 {
        return Err(DockerError::Command(
            "container stdin payload exceeds the 16 MiB safety limit".into(),
        ));
    }

    let mut command = Command::new("/usr/bin/docker");
    command
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .env("DOCKER_HOST", "unix:///var/run/docker.sock")
        .arg("exec")
        .arg("-i")
        .args(exec_user_arguments(user))
        .arg(container)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command.spawn()?;
    let stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            terminate_child(&mut child);
            return Err(DockerError::Command(
                "docker exec did not expose stdin".into(),
            ));
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_child(&mut child);
            return Err(DockerError::Command(
                "docker exec did not expose stdout".into(),
            ));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_child(&mut child);
            return Err(DockerError::Command(
                "docker exec did not expose stderr".into(),
            ));
        }
    };

    // Writing input and draining both output streams must happen concurrently;
    // otherwise a child that fills stdout/stderr before consuming stdin can deadlock.
    let input = input.to_vec();
    let stdin_thread = thread::spawn(move || {
        use std::io::Write as _;
        let mut stdin = stdin;
        stdin.write_all(&input)
    });
    let stdout_thread = thread::spawn(move || read_bounded_output(stdout));
    let stderr_thread = thread::spawn(move || read_bounded_output(stderr));
    let deadline = Instant::now() + DOCKER_COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                kill_command_group(child.id());
                break status;
            }
            Ok(None) if Instant::now() >= deadline => {
                terminate_child(&mut child);
                let _ = stdin_thread.join();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(DockerError::Command(format!(
                    "docker exec timed out after {}s",
                    DOCKER_COMMAND_TIMEOUT.as_secs()
                )));
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                terminate_child(&mut child);
                let _ = stdin_thread.join();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(DockerError::Spawn(error));
            }
        }
    };

    let stdin_result = stdin_thread
        .join()
        .map_err(|_| DockerError::Command("docker stdin writer thread panicked".into()))?;
    let stdout = stdout_thread
        .join()
        .map_err(|_| DockerError::Command("stdout reader thread panicked".into()))??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| DockerError::Command("stderr reader thread panicked".into()))??;
    if output_was_truncated(&stdout) || output_was_truncated(&stderr) {
        return Err(DockerError::Command(
            "docker exec output exceeded the 8 MiB safety limit".into(),
        ));
    }
    if let Err(error) = stdin_result {
        // A command that exited before consuming its bounded input is a failed
        // operation; do not silently report success after a partial write.
        return Err(DockerError::Spawn(error));
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Executes a command in one managed container and forwards bounded stdout/stderr
/// output to the caller. Each stream retains at most `MAX_DOCKER_OUTPUT_BYTES`;
/// output beyond that cap is drained but not retained. The Docker CLI is killed
/// after the shared command timeout, including descendants holding its pipes.
pub fn exec_container_stream<F>(container: &str, args: &[&str], on_output: F) -> Result<i32>
where
    F: FnMut(bool, &[u8]),
{
    exec_container_stream_as(container, None, args, on_output)
}

/// Streams a workflow-controlled command as the unprivileged Actions runner
/// account. This is separate from the long-lived ptrace-supervised runner tree:
/// Docker exec starts a daemon-created process, so explicitly drop its UID.
pub fn exec_container_stream_as_runner<F>(
    container: &str,
    args: &[&str],
    on_output: F,
) -> Result<i32>
where
    F: FnMut(bool, &[u8]),
{
    exec_container_stream_as(container, Some("runner"), args, on_output)
}

fn exec_container_stream_as<F>(
    container: &str,
    user: Option<&str>,
    args: &[&str],
    mut on_output: F,
) -> Result<i32>
where
    F: FnMut(bool, &[u8]),
{
    let mut command = Command::new("/usr/bin/docker");
    command
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .env("DOCKER_HOST", "unix:///var/run/docker.sock")
        .arg("exec")
        .args(exec_user_arguments(user))
        .arg(container)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command.spawn()?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_child(&mut child);
            return Err(DockerError::Command(
                "docker exec did not expose stdout".into(),
            ));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_child(&mut child);
            return Err(DockerError::Command(
                "docker exec did not expose stderr".into(),
            ));
        }
    };

    // Drain both pipes concurrently, but never accumulate one Vec per chunk:
    // retained memory stays bounded even if a workflow emits unbounded output.
    let stdout_thread = thread::spawn(move || read_bounded_output(stdout));
    let stderr_thread = thread::spawn(move || read_bounded_output(stderr));
    let deadline = Instant::now() + DOCKER_COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                kill_command_group(child.id());
                break status;
            }
            Ok(None) if Instant::now() >= deadline => {
                terminate_child(&mut child);
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(DockerError::Command(format!(
                    "docker exec timed out after {}s",
                    DOCKER_COMMAND_TIMEOUT.as_secs()
                )));
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                terminate_child(&mut child);
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(DockerError::Spawn(error));
            }
        }
    };

    let stdout = stdout_thread
        .join()
        .map_err(|_| DockerError::Command("stdout reader thread panicked".into()))??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| DockerError::Command("stderr reader thread panicked".into()))??;
    if output_was_truncated(&stdout) || output_was_truncated(&stderr) {
        return Err(DockerError::Command(
            "docker exec output exceeded the 8 MiB safety limit".into(),
        ));
    }
    for chunk in stdout.chunks(8192) {
        on_output(false, chunk);
    }
    for chunk in stderr.chunks(8192) {
        on_output(true, chunk);
    }

    Ok(status.code().unwrap_or(1))
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

    #[cfg(target_os = "linux")]
    #[test]
    fn melt_exclusions_cover_root_relative_and_dot_prefixed_tar_members() {
        use std::process::Command;

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock must be after Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gitrun-melt-exclusion-test-{}-{nonce}",
            std::process::id()
        ));
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();

        for directory in [
            "proc",
            "sys",
            "dev",
            "run",
            "etc",
            "var/run",
            "tmp",
            "var/tmp",
            "var/lib/gitrun/shared",
            "home/runner/actions-runner",
            "workspace",
        ] {
            std::fs::create_dir_all(source.join(directory)).unwrap();
        }
        for (relative, contents) in [
            ("proc/process-info", "proc"),
            ("sys/kernel-info", "sys"),
            ("dev/console", "dev"),
            ("run/daemon.pid", "run"),
            ("etc/hosts", "host-config"),
            ("etc/hostname", "host-name"),
            ("etc/resolv.conf", "resolver-config"),
            ("var/run/daemon.pid", "var-run"),
            ("tmp/transient", "tmp"),
            ("var/tmp/transient", "var-tmp"),
            ("var/lib/gitrun/shared/cache-key", "shared-cache"),
            ("home/runner/actions-runner/run.sh", "runner"),
            ("workspace/artifact.txt", "must-transfer"),
        ] {
            std::fs::write(source.join(relative), contents).unwrap();
        }

        for (index, dot_prefix) in [false, true].into_iter().enumerate() {
            let archive = root.join(format!("source-{index}.tar"));
            let mut create = Command::new("tar");
            create.current_dir(&source).arg("-cf").arg(&archive);
            if dot_prefix {
                create.arg(".");
            } else {
                create.args([
                    "proc",
                    "sys",
                    "dev",
                    "run",
                    "etc",
                    "tmp",
                    "var",
                    "home",
                    "workspace",
                ]);
            }
            let created = create
                .output()
                .expect("tar must be installed for this test");
            assert!(
                created.status.success(),
                "creating test archive failed: {}",
                String::from_utf8_lossy(&created.stderr)
            );

            let destination = root.join(format!("destination-{index}"));
            std::fs::create_dir_all(&destination).unwrap();
            let extracted = Command::new("tar")
                .arg("-xpf")
                .arg(&archive)
                .arg("-C")
                .arg(&destination)
                .arg("--no-same-owner")
                .args(MELT_TAR_EXCLUDE_ARGS)
                .output()
                .expect("tar must be installed for this test");
            assert!(
                extracted.status.success(),
                "extracting test archive failed: {}",
                String::from_utf8_lossy(&extracted.stderr)
            );

            assert_eq!(
                std::fs::read_to_string(destination.join("workspace/artifact.txt")).unwrap(),
                "must-transfer"
            );
            for excluded in [
                "proc/process-info",
                "sys/kernel-info",
                "dev/console",
                "run/daemon.pid",
                "etc/hosts",
                "etc/hostname",
                "etc/resolv.conf",
                "var/run/daemon.pid",
                "tmp/transient",
                "var/tmp/transient",
                "var/lib/gitrun/shared/cache-key",
                "home/runner/actions-runner/run.sh",
            ] {
                assert!(
                    !destination.join(excluded).exists(),
                    "protected path {excluded} was extracted (dot_prefix={dot_prefix})"
                );
            }
        }

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn melt_rejects_unprotected_writable_docker_mounts() {
        let safe = serde_json::json!([
            {"Destination": "/tmp", "RW": true},
            {"Destination": "/var/lib/gitrun/shared/cache", "RW": true},
            {"Destination": "/etc/hosts", "RW": false},
        ]);
        assert!(validate_melt_target_mount_inventory("target-id", &safe).is_ok());

        let unsafe_mount = serde_json::json!([
            {"Destination": "/tmp", "RW": true},
            {"Destination": "/workspace/host-data", "RW": true},
        ]);
        let error = validate_melt_target_mount_inventory("target-id", &unsafe_mount)
            .expect_err("unprotected writable mounts must block MELT");
        assert!(error.to_string().contains("/workspace/host-data"));

        let incomplete = serde_json::json!([{"Destination": "/workspace", "Type": "bind"}]);
        assert!(validate_melt_target_mount_inventory("target-id", &incomplete).is_err());
        assert!(validate_melt_target_mount_inventory("target-id", &serde_json::json!({})).is_err());
    }

    #[test]
    fn workflow_exec_user_is_explicitly_unprivileged() {
        assert_eq!(exec_user_arguments(Some("runner")), ["--user", "runner"]);
        assert!(exec_user_arguments(None).is_empty());
    }

    #[test]
    fn melt_writable_mount_gate_only_allows_excluded_roots() {
        for destination in [
            "/proc",
            "/proc/1",
            "/sys",
            "/dev",
            "/run",
            "/run/gitrun/api.sock",
            "/var/run/docker.sock",
            "/etc/hosts",
            "/etc/hostname",
            "/etc/resolv.conf",
            "/tmp",
            "/tmp/work",
            "/var/tmp",
            "/var/tmp/staging",
            "/var/lib/gitrun/shared",
            "/var/lib/gitrun/shared/cache",
            "/home/runner/actions-runner",
            "/home/runner/actions-runner/_work",
        ] {
            assert!(
                melt_excludes_mount_destination(destination),
                "{destination} should be excluded from MELT archives"
            );
        }
        for destination in [
            "/",
            "/workspace",
            "/workspace/data",
            "/etc",
            "/var/lib/gitrun",
            "/var/lib/gitrun/shared-escape",
            "/tmpish",
            "/home/runner",
            "relative/path",
            "/workspace/../etc",
        ] {
            assert!(
                !melt_excludes_mount_destination(destination),
                "{destination} must not be treated as an excluded mount root"
            );
        }
    }

    #[test]
    fn remote_linux_runners_fail_closed_without_api_socket_forwarding() {
        assert!(!remote_linux_runner_api_unsupported(
            &DockerHost::Local,
            false
        ));
        assert!(remote_linux_runner_api_unsupported(
            &DockerHost::Remote("tcp://192.168.56.10:2376".into()),
            false
        ));
        assert!(remote_linux_runner_api_unsupported(
            &DockerHost::RemoteTls {
                endpoint: "tcp://192.168.56.10:2376".into(),
                cert_dir: "/etc/gitrun/vm-certs".into(),
            },
            false
        ));
        // Windows runners do not consume the Linux Unix-socket mount.
        assert!(!remote_linux_runner_api_unsupported(
            &DockerHost::RemoteTls {
                endpoint: "tcp://192.168.56.10:2376".into(),
                cert_dir: "/etc/gitrun/vm-certs".into(),
            },
            true
        ));
    }

    #[test]
    fn runner_name_prefix_is_collision_free_and_within_docker_name_limit() {
        let first = "foo-bar/baz";
        let second = "foo/bar-baz";
        assert_eq!(sanitize(first, '-'), sanitize(second, '-'));
        assert_ne!(runner_name_prefix(first), runner_name_prefix(second));

        let maximum_repo = format!("{}/{}", "a".repeat(39), "b".repeat(100));
        // uuid_like_suffix is at most 36 ASCII characters; the encoded
        // prefix must leave room under Docker's 255-character name limit.
        assert!(runner_name_prefix(&maximum_repo).len() + 36 <= 255);
    }

    #[test]
    fn docker_env_passthrough_keeps_secret_values_out_of_argv() {
        let mut args = vec![
            "run".to_owned(),
            "-e".to_owned(),
            "RUNNER_TOKEN".to_owned(),
            "gitrun-runner:latest".to_owned(),
        ];
        let secrets = vec![
            ("API_TOKEN".to_owned(), "plain-secret".to_owned()),
            (
                "MULTILINE_KEY".to_owned(),
                "first line\nsecond line".to_owned(),
            ),
        ];
        let mut env = vec![("RUNNER_TOKEN".to_owned(), "registration-secret".to_owned())];

        append_env_passthrough_args(&mut args, &secrets, &mut env);

        assert!(args.iter().any(|arg| arg == "API_TOKEN"));
        assert!(args.iter().any(|arg| arg == "MULTILINE_KEY"));
        assert!(!args.iter().any(|arg| arg.contains("plain-secret")));
        assert!(!args.iter().any(|arg| arg.contains("registration-secret")));
        assert!(!args.iter().any(|arg| arg.contains("first line")));
        assert_eq!(
            env[0],
            ("RUNNER_TOKEN".to_owned(), "registration-secret".to_owned())
        );
        assert_eq!(env[1], ("API_TOKEN".to_owned(), "plain-secret".to_owned()));
        assert_eq!(
            env[2],
            (
                "MULTILINE_KEY".to_owned(),
                "first line\nsecond line".to_owned()
            )
        );
    }

    #[test]
    fn secret_environment_rejects_protected_runner_variables() {
        for name in [
            "RUNNER_TOKEN",
            "RUNNER_URL",
            "GITRUN_GSR_COMMAND_POLICY_ENABLED",
            "GITRUN_GSR_ALLOW_UNSAFE_RUNNER",
            "GITRUN_API_SOCKET",
            "GITHUB_TOKEN",
            "GITHUB_WORKSPACE",
            "ACTIONS_RUNTIME_TOKEN",
            "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_SSH_COMMAND",
            "PYTHONPATH",
            "PYTHONHOME",
            "NODE_OPTIONS",
            "RUBYOPT",
            "PERL5OPT",
            "LD_PRELOAD",
            "LD_AUDIT",
            "LD_DEBUG",
            "LD_ORIGIN_PATH",
            "SHELLOPTS",
            "BASHOPTS",
            "GLIBC_TUNABLES",
            "GCONV_PATH",
            "LOCPATH",
            "NLSPATH",
            "MALLOC_TRACE",
        ] {
            assert!(
                validate_secret_env(&[(name.into(), "value".into())]).is_err(),
                "{name} must not be overridden by a vault secret"
            );
        }
        assert!(validate_secret_env(&[("DEPLOY_KEY".into(), "secret".into())]).is_ok());
        assert!(validate_secret_env(&[("DEPLOY_KEY".into(), "bad\0value".into())]).is_err());
    }

    #[test]
    fn docker_output_capture_is_bounded_and_continues_draining() {
        let input = vec![b'x'; MAX_DOCKER_OUTPUT_BYTES + 1024];
        let output = read_bounded_output(std::io::Cursor::new(input)).unwrap();
        assert!(output.len() <= MAX_DOCKER_OUTPUT_BYTES + 64);
        assert!(output.ends_with(b"[Docker output truncated at 8 MiB]"));
        assert!(output_was_truncated(&output));
        assert!(!output_was_truncated(b"complete output"));
    }

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
    fn container_label_format_emits_valid_docker_go_template() {
        assert_eq!(
            container_label_format("gitrun.dock_target"),
            r#"{{index .Config.Labels "gitrun.dock_target"}}"#
        );
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
    fn runner_network_rejects_bridge_with_inter_container_connectivity() {
        assert!(
            validate_runner_network_metadata("gitrun-runner", "bridge|true|true", true,)
                .unwrap_err()
                .contains("enable_icc=false")
        );
        assert!(
            validate_runner_network_metadata("gitrun-runner", "bridge|true|<no value>", true,)
                .is_err()
        );
    }

    #[test]
    fn runner_network_accepts_managed_bridge_with_icc_disabled() {
        assert!(
            validate_runner_network_metadata("gitrun-runner", "bridge|true|false", true,).is_ok()
        );
        assert!(validate_runner_network_metadata("gitrun-runner", "bridge|true", false,).is_ok());
    }

    #[test]
    fn runner_network_rejects_unmanaged_or_non_bridge_networks() {
        assert!(
            validate_runner_network_metadata("gitrun-runner", "bridge|false|false", true,)
                .unwrap_err()
                .contains("GitRun-managed bridge")
        );
        assert!(
            validate_runner_network_metadata("gitrun-runner", "host|true|false", true,).is_err()
        );
    }

    #[test]
    fn dock_only_control_mount_must_be_exact_and_read_only() {
        assert!(
            validate_control_volume_attachment("runner-control", "runner-control|false").is_ok()
        );
        assert!(validate_control_volume_attachment("runner-control", "").is_err());
        assert!(
            validate_control_volume_attachment("runner-control", "runner-control|true")
                .unwrap_err()
                .contains("read-only")
        );
        assert!(
            validate_control_volume_attachment("runner-control", "other-control|false").is_err()
        );
        assert!(validate_control_volume_attachment("runner-control", "runner-control").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn dock_only_marker_is_atomically_written_and_removed_from_control_volume() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "gitrun-dock-control-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let uid = std::fs::metadata(&directory).unwrap().uid();

        set_dock_only_marker_in_volume(&directory, true, uid).unwrap();
        let marker = directory.join("dock-only");
        assert_eq!(std::fs::read(&marker).unwrap(), b"GitRun dock-only v1\n");
        let metadata = std::fs::symlink_metadata(&marker).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

        set_dock_only_marker_in_volume(&directory, false, uid).unwrap();
        assert!(std::fs::symlink_metadata(&marker).is_err());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dock_only_marker_rejects_symlink_and_hardlink_entries() {
        use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "gitrun-dock-control-unsafe-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let uid = std::fs::metadata(&directory).unwrap().uid();
        let outside = directory.with_extension("outside");
        std::fs::write(&outside, b"do not replace").unwrap();
        symlink(&outside, directory.join("dock-only")).unwrap();
        assert!(set_dock_only_marker_in_volume(&directory, true, uid).is_err());
        std::fs::remove_file(directory.join("dock-only")).unwrap();

        std::fs::write(directory.join("dock-only"), b"marker").unwrap();
        let hardlink = directory.with_extension("hardlink");
        std::fs::hard_link(directory.join("dock-only"), &hardlink).unwrap();
        assert!(set_dock_only_marker_in_volume(&directory, false, uid).is_err());
        std::fs::remove_file(hardlink).unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
        std::fs::remove_file(outside).unwrap();
    }

    #[test]
    fn control_volume_mount_is_read_only_and_uses_its_own_named_volume() {
        assert_eq!(
            control_volume_mount_argument("runner-abc"),
            "type=volume,source=runner-abc-control,target=/var/lib/gitrun/control,readonly"
        );
    }

    #[test]
    fn control_volume_name_is_deterministic_and_separate_from_runner_home() {
        assert_eq!(control_volume_name("runner-abc"), "runner-abc-control");
        assert_ne!(
            control_volume_name("runner-abc"),
            home_volume_name("runner-abc")
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
    fn cgroup_parser_rejects_unrelated_hex_text_and_accepts_docker_scope() {
        let id = "0123456789abcdef".repeat(4);
        assert_eq!(
            container_id_from_cgroup(&format!("0::/user.slice/session-{id}-scope")),
            None
        );
        assert_eq!(
            container_id_from_cgroup(&format!("0::/system.slice/docker-{id}.scope")).as_deref(),
            Some(id.as_str())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn docker_cgroup_parser_extracts_only_full_container_id() {
        let id = "0123456789abcdef".repeat(4);
        let cgroup = format!("0::/system.slice/docker-{id}.scope");
        assert_eq!(
            container_id_from_cgroup(&cgroup).as_deref(),
            Some(id.as_str())
        );
    }
}
