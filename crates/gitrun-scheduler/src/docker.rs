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
use std::process::Command;
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
}

impl DockerHost {
    fn env_value(&self) -> Option<&str> {
        match self {
            DockerHost::Local => None,
            DockerHost::Remote(addr) => Some(addr.as_str()),
        }
    }
}

fn run_on(host: &DockerHost, args: &[&str]) -> Result<std::process::Output> {
    let mut command = Command::new("docker");
    if let Some(addr) = host.env_value() {
        command.env("DOCKER_HOST", addr);
    }
    Ok(command.args(args).output()?)
}

fn run(args: &[&str]) -> Result<std::process::Output> {
    run_on(&DockerHost::Local, args)
}

fn run_checked_on(host: &DockerHost, args: &[&str]) -> Result<String> {
    let output = run_on(host, args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(DockerError::Command(if stderr.is_empty() {
            format!("docker {} failed", args.join(" "))
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
}

pub fn shared_cache_volume(configured: Option<&str>) -> String {
    configured
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(SHARED_CACHE_VOLUME_DEFAULT)
        .to_owned()
}

pub fn ensure_shared_cache_volume(name: &str) -> Result<()> {
    let inspected = run(&["volume", "inspect", name])?;
    if inspected.status.success() {
        return Ok(());
    }
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
        Err(DockerError::Command(message))
            if message.contains("is not running") || message.contains("No such container") =>
        {
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
    output
        .lines()
        .skip(1) // header line: "COMMAND"
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
        let status = container_status_string(host, name).unwrap_or_default();
        let permanent = container_is_permanent_on(host, name).unwrap_or(true); // fail-safe: assume permanent, matching gitrun_updater_utility.py's upgrade-safety default
        containers.push(ManagedContainer {
            name: name.to_owned(),
            status,
            permanent,
        });
    }
    Ok(containers)
}

/// Raw `.State.Status` string (e.g. "running", "exited"), empty if the
/// container can't be inspected (already removed, etc).
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
        return Ok(None);
    }
    let label = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(if label.is_empty() { None } else { Some(label) })
}

fn container_status_string(host: &DockerHost, name: &str) -> Result<String> {
    let output = run_on(host, &["inspect", "-f", "{{json .State}}", name])?;
    if !output.status.success() {
        return Ok(String::new());
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(raw.trim()).unwrap_or(Value::Null);
    Ok(value
        .get("Status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned())
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

pub fn remove_container_on(host: &DockerHost, name: &str) -> Result<()> {
    // `docker rm -f` on an already-missing container is a no-op failure we
    // don't care about — mirrors the Python `check=False` + warn-only style.
    let _ = run_on(host, &["rm", "-f", name]);
    // Best-effort: remove the disk-backed home volume, if this container
    // was created with RunnerHomeBackend::Volume. Harmless no-op (fails
    // silently) for tmpfs-backed containers, which never had one — without
    // this, switching to the "volume" backend would leak one named volume
    // on disk per runner ever created, forever.
    let _ = run_on(host, &["volume", "rm", "-f", &home_volume_name(name)]);
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
    /// Decrypted GitVault secrets to inject as environment variables, as
    /// (name, value) pairs. Decryption happens just before this call and the
    /// plaintext lives only long enough to build the `docker run` argument
    /// list — see `main.rs::vault_env_for_repo`. Names are validated to be
    /// safe environment variable identifiers before reaching here.
    pub secret_env: &'a [(String, String)],
    /// True for a Windows container runner (Logic Containers). Changes which
    /// flags are valid: Windows containers don't support `--read-only`,
    /// `--tmpfs`, `--pids-limit`, or Unix-style socket/group-add mounts —
    /// those are Linux-kernel-specific. The Docker socket bind-mount is also
    /// skipped for Windows today: Docker-in-Docker via a mounted
    /// `//./pipe/docker_engine` named pipe is possible but not yet
    /// implemented here, so a Windows runner cannot itself run Docker builds
    /// until that's added.
    pub is_windows: bool,
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

pub fn create_runner_on(host: &DockerHost, spec: &RunnerSpec) -> Result<()> {
    let slug = sanitize(spec.repo, '_');
    let labels = ensure_label(spec.labels, "gitrun-ci");

    let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), spec.name.into()];
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
        "--cpus".into(),
        spec.cpus.into(),
        "--memory".into(),
        spec.memory.into(),
        "--restart".into(),
        "unless-stopped".into(),
    ]);

    if spec.is_windows {
        // Windows containers: no --read-only/--tmpfs/--pids-limit/Unix
        // group-add support. The runner's home directory is simply the
        // container's own writable filesystem layer — Windows containers
        // don't get the same read-only-root treatment Linux runners do here
        // yet; hardening Windows containers is tracked as GSR follow-up
        // work, not solved by this function.
        args.push("-e".into());
        args.push("GITRUN_SHARED_CACHE_DIR=C:\\gitrun\\shared".into());
    } else {
        args.extend([
            "--pids-limit".into(),
            spec.pids_limit.into(),
            "--read-only".into(),
            // Fix: --read-only alone with only /tmp writable broke the GitHub
            // runner in practice ("Read-only file system" on .env and on
            // actions-runner/_diag) — the runner writes its registration state,
            // diagnostic logs, and job checkouts (_work/) under its home
            // directory, not just /tmp. Both are now writable, the runner
            // binary/config baked into the image stays read-only, everything it
            // needs to write at runtime does not.
        ]);
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
            "/tmp:rw,nosuid,nodev,size=256m".into(),
            "--volume".into(),
            "/var/run/docker.sock:/var/run/docker.sock".into(),
            "--group-add".into(),
            spec.docker_socket_gid.into(),
            "--mount".into(),
            format!(
                "type=volume,source={},target=/var/lib/gitrun/shared",
                spec.shared_cache_volume
            ),
            "-e".into(),
            "GITRUN_SHARED_CACHE_DIR=/var/lib/gitrun/shared".into(),
            "-e".into(),
            "CARGO_HOME=/var/lib/gitrun/shared/cargo".into(),
            "-e".into(),
            format!("CARGO_TARGET_DIR=/var/lib/gitrun/shared/cargo-target/{slug}"),
            "-e".into(),
            "PIP_CACHE_DIR=/var/lib/gitrun/shared/pip".into(),
            "-e".into(),
            "NPM_CONFIG_CACHE=/var/lib/gitrun/shared/npm".into(),
            "-e".into(),
            "DOCKER_CONFIG=/tmp/docker-config".into(),
            "--tmpfs".into(),
            "/run/gitrun:rw,nosuid,nodev,noexec,size=16m,mode=0755".into(),
        ]);
        if spec.docker_socket_hardening {
            args.extend(docker_socket_hardening_args());
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

/// Extra `docker run` flags applied to a Linux runner container when
/// `RunnerSpec::docker_socket_hardening` is true (the default — see the
/// GSR "danger gate", `gitrun_core::Config::gsr_docker_socket_hardening`).
///
/// **What this does and does not do**, stated plainly because it's easy to
/// over-trust a list like this: the mounted `/var/run/docker.sock` still
/// gives the runner's own processes full Docker control by design (that's
/// the whole point of the mount — Docker-in-Docker for build/test jobs).
/// Nothing short of removing that mount (a larger, separately-scoped
/// change — see the GSR session notes) closes that specific door. What
/// these flags DO reduce is the container's *own* kernel-level attack
/// surface for an attacker who has code execution in the job but hasn't
/// yet reached the socket, or who is trying to escalate via the container
/// runtime itself rather than via Docker API calls it's already allowed to
/// make:
/// - `--cap-drop=ALL` plus back only `CHOWN`/`SETUID`/`SETGID`/`DAC_OVERRIDE`
///   (needed for the runner process and build tools to `chown`/run as
///   themselves and write normal files) removes every other Linux
///   capability, including `SYS_ADMIN` (mount/namespace operations),
///   `SYS_PTRACE` (process injection/debugging), and `NET_RAW`.
/// - `--security-opt no-new-privileges` blocks setuid/setgid/file-capability
///   escalation for the lifetime of the container, closing the most common
///   "gained a foothold, now escalate" path even if a setuid binary exists
///   somewhere in the image.
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
        "--security-opt",
        "no-new-privileges",
    ]
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
        // SYS_ADMIN and SYS_PTRACE must never be added back - that would
        // defeat the point of dropping ALL in the first place.
        assert!(!args.iter().any(|a| a == "SYS_ADMIN" || a == "SYS_PTRACE"));
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
}
