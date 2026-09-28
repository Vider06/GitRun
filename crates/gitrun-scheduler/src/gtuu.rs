//! GTUU — GitRun Updater Utility.
//!
//! Updates GitRun-managed *permanent* runner containers one at a time when a
//! new runner image is available. Dynamic runners are left alone: they pick
//! up the current image automatically whenever the autoscaler recreates them,
//! so there's no need to touch them here.
//!
//! Ported from `autoscaler/gitrun_updater_utility.py`, with one correctness
//! fix: the Python `token()` helper computed the token, checked it wasn't
//! empty, but never returned it — so every `Authorization: Bearer {token()}`
//! header actually sent the literal string "Bearer None". GTUU would have
//! failed on its very first authenticated call any time it was actually
//! invoked with `GITRUN_AUTO_CONTAINER_UPDATE=true`. This module takes the
//! token as an explicit constructor argument instead, so it can't be
//! silently dropped again.

use crate::docker::{self, sanitize};
use crate::github::GitHubClient;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GtuuError {
    #[error("another GTUU run is already active")]
    AlreadyRunning,
    #[error(transparent)]
    Docker(#[from] crate::docker::DockerError),
    #[error(transparent)]
    GitHub(#[from] crate::github::GitHubError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, GtuuError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOutcome {
    Updated,
    Current,
    Busy,
    Missing,
    Offline,
}

/// Filesystem lock preventing two GTUU runs from overlapping. Mirrors the
/// Python version's `state_dir/gtuu.lock` directory-as-mutex, which is atomic
/// on POSIX (`mkdir` fails if the directory already exists).
///
/// `Drop` releases the lock on a normal return or an unwinding panic, but
/// that alone is not a complete guarantee: a `SIGKILL`, an OOM-kill, or a
/// host power loss skips `Drop` entirely and can leave the lock directory
/// behind forever, wedging every future GTUU run. To recover from that,
/// `acquire` also checks whether an *existing* lock is stale — either its
/// recorded PID is no longer a running process, or it's simply older than
/// any real GTUU run should take — and reclaims it in that case instead of
/// refusing forever.
pub struct GtuuLock {
    path: PathBuf,
}

/// A lock older than this is assumed stale regardless of PID liveness: GTUU
/// updates one container at a time with a bounded `online_wait_timeout`
/// (typically ~2 minutes) per container, so even a large fleet finishing
/// updates back-to-back should not plausibly run past this.
const STALE_LOCK_AGE: Duration = Duration::from_secs(60 * 60);

impl GtuuLock {
    pub fn acquire(state_dir: &Path) -> Result<Self> {
        fs::create_dir_all(state_dir)?;
        let path = state_dir.join("gtuu.lock");
        match fs::create_dir(&path) {
            Ok(()) => {
                let _ = fs::write(path.join("pid"), std::process::id().to_string());
                Ok(Self { path })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale(&path) {
                    // Reclaim: remove the abandoned lock and retry once. Not
                    // recursive/looping further than one retry so a genuinely
                    // stuck (non-stale) concurrent run still gets a clean
                    // `AlreadyRunning` instead of a fight over reclaiming.
                    let _ = fs::remove_dir_all(&path);
                    fs::create_dir(&path)?;
                    let _ = fs::write(path.join("pid"), std::process::id().to_string());
                    Ok(Self { path })
                } else {
                    Err(GtuuError::AlreadyRunning)
                }
            }
            Err(error) => Err(GtuuError::Io(error)),
        }
    }
}

/// A lock is stale if its age exceeds `STALE_LOCK_AGE`, or if it recorded a
/// PID that is no longer running. Either check alone has a failure mode (age
/// alone can't tell a slow-but-legitimate run from a stuck one; PID alone
/// can't tell a dead lock from one whose PID got reused by an unrelated
/// process) — combined, both must indicate "abandoned" is at least plausible
/// before we hold that a genuinely-alive lock never gets falsely reclaimed
/// unless it's also old enough that reclaiming is the safer bet regardless.
fn is_stale(lock_path: &Path) -> bool {
    let age_exceeded = fs::metadata(lock_path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .map(|age| age > STALE_LOCK_AGE)
        .unwrap_or(false);
    if age_exceeded {
        return true;
    }

    let recorded_pid: Option<u32> = fs::read_to_string(lock_path.join("pid"))
        .ok()
        .and_then(|raw| raw.trim().parse().ok());
    match recorded_pid {
        Some(pid) => !process_is_alive(pid),
        // No PID file at all (e.g. write failed originally) — fall back to
        // age alone, which we already know hasn't been exceeded here, so
        // treat as not-yet-stale rather than guessing.
        None => false,
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    // Signal 0 performs no actual signal delivery, only existence/permission
    // checks (see kill(2)) — the standard portable way to ask "is this PID
    // alive" without a `/proc` dependency (which isn't guaranteed on every
    // Unix, though it is on Linux, GitRun's only supported host OS today).
    let result = unsafe { libc_kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(1) // EPERM: alive, just not ours
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    // Conservative default off Unix: assume alive so staleness falls back
    // to the age check only.
    true
}

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

impl Drop for GtuuLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path.join("pid"));
        let _ = fs::remove_dir(&self.path);
    }
}

pub struct GtuuConfig<'a> {
    pub image: &'a str,
    pub repositories: &'a [String],
    pub runner_labels: &'a str,
    pub ephemeral: bool,
    pub disable_update: bool,
    pub cpus: &'a str,
    pub memory: &'a str,
    pub pids_limit: &'a str,
    pub shared_cache_volume: &'a str,
    pub docker_socket_gid: &'a str,
    pub runner_home_size: &'a str,
    /// See `docker::RunnerHomeBackend` / `Config::runner_home_backend`.
    pub runner_home_backend: docker::RunnerHomeBackend,
    /// Looks up (and decrypts) GitVault secrets for a given repo, returning
    /// them ready to inject as environment variables. Passed as a closure
    /// rather than a fixed list because GTUU updates containers across
    /// potentially many repos in one run, each needing its own repo-scoped
    /// secrets — see `main.rs::vault_env_for_repo`, which this wraps.
    pub secret_env_for_repo: &'a dyn Fn(&str) -> Vec<(String, String)>,
    pub online_wait_timeout: Duration,
    /// See `Config::gsr_docker_socket_hardening` / `RunnerSpec::docker_socket_hardening`.
    pub docker_socket_hardening: bool,
}

/// Updates every permanent container for the configured repositories whose
/// image differs from the freshly pulled one. Returns the number actually
/// updated (mirrors the Python `update_permanent_containers` return value).
pub fn update_permanent_containers(client: &GitHubClient, config: &GtuuConfig) -> Result<u32> {
    docker::ensure_shared_cache_volume(config.shared_cache_volume)?;
    let new_image_id = pull_image(config.image)?;

    let mut updated = 0u32;
    for repo in config.repositories {
        for container in docker::managed_containers(repo)? {
            if !container.permanent {
                continue;
            }
            if update_one(client, config, repo, &container.name, &new_image_id)? == UpdateOutcome::Updated {
                updated += 1;
            }
        }
    }
    Ok(updated)
}

fn pull_image(image: &str) -> Result<String> {
    let output = Command::new("docker").args(["pull", image]).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(crate::docker::DockerError::Command(if stderr.is_empty() {
            format!("docker pull failed for {image}")
        } else {
            stderr
        })
        .into());
    }
    let id_output = Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", image])
        .output()?;
    Ok(String::from_utf8_lossy(&id_output.stdout).trim().to_owned())
}

fn current_image_id(container_name: &str) -> Result<String> {
    let output = Command::new("docker")
        .args(["inspect", "-f", "{{.Image}}", container_name])
        .output()?;
    if !output.status.success() {
        return Ok(String::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn update_one(
    client: &GitHubClient,
    config: &GtuuConfig,
    repo: &str,
    name: &str,
    new_image_id: &str,
) -> Result<UpdateOutcome> {
    let old_image_id = current_image_id(name)?;
    if old_image_id.is_empty() {
        return Ok(UpdateOutcome::Missing);
    }
    if old_image_id == new_image_id {
        return Ok(UpdateOutcome::Current);
    }

    let runners = client.list_runners(repo)?;
    let runner = runners.iter().find(|r| r.name == name);
    if let Some(runner) = runner {
        if runner.busy {
            return Ok(UpdateOutcome::Busy);
        }
        client.delete_runner(repo, runner.id)?;
    }

    docker::remove_container(name)?;

    let registration_token = client.registration_token(repo)?;
    let replacement_name = format!(
        "{}-gtuu-{}-{}",
        sanitize(name, '-'),
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs() % 100_000)
            .unwrap_or(0)
    );

    docker::create_runner(&crate::docker::RunnerSpec {
        name: &replacement_name,
        repo,
        permanent: true,
        registration_token: &registration_token,
        image: config.image,
        labels: config.runner_labels,
        ephemeral: config.ephemeral,
        disable_update: config.disable_update,
        cpus: config.cpus,
        memory: config.memory,
        pids_limit: config.pids_limit,
        shared_cache_volume: config.shared_cache_volume,
        docker_socket_gid: config.docker_socket_gid,
        runner_home_size: config.runner_home_size,
        home_backend: config.runner_home_backend,
        secret_env: &(config.secret_env_for_repo)(repo),
        is_windows: false,
        docker_socket_hardening: config.docker_socket_hardening,
    })?;

    // Bug fix: this previously waited on `name` (the OLD runner being
    // replaced) instead of `replacement_name` (the container we just
    // created). If the old runner briefly still reported "online" to GitHub
    // right after deletion, this could return true while watching the wrong
    // runner entirely, then proceed to rename a replacement that may never
    // have come online for real.
    if !wait_for_online(client, repo, &replacement_name, config.online_wait_timeout)? {
        // Leave the differently-named replacement running for diagnosis
        // rather than silently discarding it — same call as the Python
        // version made deliberately.
        return Ok(UpdateOutcome::Offline);
    }

    rename_container(&replacement_name, name)?;
    Ok(UpdateOutcome::Updated)
}

fn wait_for_online(client: &GitHubClient, repo: &str, runner_name: &str, timeout: Duration) -> Result<bool> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let runners = client.list_runners(repo)?;
        if runners.iter().any(|r| r.name == runner_name && r.is_online()) {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    Ok(false)
}

fn rename_container(from: &str, to: &str) -> Result<()> {
    let output = Command::new("docker").args(["rename", from, to]).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(crate::docker::DockerError::Command(if stderr.is_empty() {
            format!("unable to rename replacement {from}")
        } else {
            stderr
        })
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_prevents_concurrent_acquire() {
        let dir = std::env::temp_dir().join(format!("gitrun-gtuu-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let first = GtuuLock::acquire(&dir).expect("first lock should succeed");
        // Second acquire should be refused: the first lock is fresh (just
        // created, by this same live process), so it's not stale by either
        // the age or the PID-liveness check.
        let second = GtuuLock::acquire(&dir);
        assert!(matches!(second, Err(GtuuError::AlreadyRunning)));
        drop(first);
        // Lock released on drop, so a new acquire should now succeed.
        let third = GtuuLock::acquire(&dir);
        assert!(third.is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_lock_from_dead_pid_is_reclaimed() {
        let dir = std::env::temp_dir().join(format!("gitrun-gtuu-stale-pid-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join("gtuu.lock");
        fs::create_dir(&lock_path).unwrap();
        // A PID astronomically unlikely to be alive right now.
        fs::write(lock_path.join("pid"), "999999").unwrap();

        // This must succeed by reclaiming the abandoned lock, not error out
        // with AlreadyRunning forever — this is exactly the "SIGKILL left a
        // lock behind" scenario the fix addresses.
        let reclaimed = GtuuLock::acquire(&dir);
        assert!(reclaimed.is_ok(), "expected stale lock (dead PID) to be reclaimed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_lock_from_old_age_is_reclaimed_even_with_live_pid() {
        let dir = std::env::temp_dir().join(format!("gitrun-gtuu-stale-age-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join("gtuu.lock");
        fs::create_dir(&lock_path).unwrap();
        // Record our own PID (definitely alive) but backdate the directory's
        // mtime past STALE_LOCK_AGE to simulate a run that's been "stuck"
        // implausibly long.
        fs::write(lock_path.join("pid"), std::process::id().to_string()).unwrap();
        let old_time = filetime::FileTime::from_system_time(
            SystemTime::now() - Duration::from_secs(60 * 60 * 2),
        );
        let _ = filetime::set_file_mtime(&lock_path, old_time);

        let reclaimed = GtuuLock::acquire(&dir);
        assert!(reclaimed.is_ok(), "expected an implausibly old lock to be reclaimed regardless of PID");
        let _ = fs::remove_dir_all(&dir);
    }
}
