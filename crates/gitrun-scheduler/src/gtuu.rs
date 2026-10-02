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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
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
/// `acquire` checks whether an *existing* lock is stale — a recorded PID
/// must be dead, while a lock with no usable PID may be reclaimed after the
/// conservative age cutoff — and reclaims it atomically instead of
/// refusing forever.
pub struct GtuuLock {
    path: PathBuf,
}

/// Fallback age for a lock that has no usable PID. A valid live PID is
/// never reclaimed solely because the run is old, so a slow but legitimate
/// update cannot accidentally overlap with another GTUU instance.
const STALE_LOCK_AGE: Duration = Duration::from_secs(60 * 60);

impl GtuuLock {
    pub fn acquire(state_dir: &Path) -> Result<Self> {
        fs::create_dir_all(state_dir)?;
        let path = state_dir.join("gtuu.lock");

        loop {
            match fs::create_dir(&path) {
                Ok(()) => {
                    if let Err(error) = fs::write(path.join("pid"), std::process::id().to_string())
                    {
                        let _ = fs::remove_dir_all(&path);
                        return Err(error.into());
                    }
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !is_stale(&path) {
                        return Err(GtuuError::AlreadyRunning);
                    }

                    // Reclaim atomically: rename the stale lock out of the
                    // canonical path first. If another GTUU process wins the
                    // race, its successful rename removes our stale source
                    // and we simply retry against the canonical path.
                    let quarantine = state_dir.join(format!(
                        "gtuu.lock.reclaim-{}-{}",
                        std::process::id(),
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_nanos())
                            .unwrap_or(0)
                    ));
                    match fs::rename(&path, &quarantine) {
                        Ok(()) => {
                            let _ = fs::remove_dir_all(&quarantine);
                        }
                        Err(rename_error)
                            if rename_error.kind() == std::io::ErrorKind::NotFound =>
                        {
                            continue;
                        }
                        Err(rename_error) => return Err(rename_error.into()),
                    }
                }
                Err(error) => return Err(GtuuError::Io(error)),
            }
        }
    }
}

/// A lock is stale when its recorded PID is no longer running. If the PID
/// file is absent or unreadable, the age cutoff provides a conservative
/// fallback for abandoned locks whose owner cannot be identified.
fn is_stale(lock_path: &Path) -> bool {
    let recorded_pid: Option<u32> = fs::read_to_string(lock_path.join("pid"))
        .ok()
        .and_then(|raw| raw.trim().parse().ok());
    match recorded_pid {
        Some(pid) => !process_is_alive(pid),
        None => fs::metadata(lock_path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > STALE_LOCK_AGE),
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
    /// GSR policy values snapshotted by the runner entrypoint before startup.
    pub gsr_policy_env: &'a [(String, String)],
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
            if update_one(client, config, repo, &container.name, &new_image_id)?
                == UpdateOutcome::Updated
            {
                updated += 1;
            }
        }
    }
    Ok(updated)
}

fn pull_image(image: &str) -> Result<String> {
    docker::pull_image(image)?;
    docker::image_id(image).map_err(Into::into)
}

fn current_image_id(container_name: &str) -> Result<UpdateCurrentImage> {
    match docker::container_image_id(container_name)? {
        Some(id) => Ok(UpdateCurrentImage::Present(id)),
        None => Ok(UpdateCurrentImage::Missing),
    }
}

enum UpdateCurrentImage {
    Present(String),
    Missing,
}


fn update_one(
    client: &GitHubClient,
    config: &GtuuConfig,
    repo: &str,
    name: &str,
    new_image_id: &str,
) -> Result<UpdateOutcome> {
    let old_image_id = match current_image_id(name)? {
        UpdateCurrentImage::Present(id) => id,
        UpdateCurrentImage::Missing => return Ok(UpdateOutcome::Missing),
    };
    if old_image_id == new_image_id {
        return Ok(UpdateOutcome::Current);
    }

    let runners = client.list_runners(repo)?;
    let runner = runners.iter().find(|r| r.name == name);
    if let Some(runner) = runner {
        if runner.busy {
            return Ok(UpdateOutcome::Busy);
        }
    }

    // Register and start the replacement before touching the old runner.
    // This preserves the existing runner while the new container is being
    // pulled up and authenticated with GitHub.
    let registration_token = client.registration_token(repo)?;
    let replacement_name = format!(
        "{}-gtuu-{}",
        sanitize(name, '-'),
        replacement_suffix()
    );
    let secret_env = (config.secret_env_for_repo)(repo);

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
        secret_env: &secret_env,
        gsr_policy_env: config.gsr_policy_env,
        is_windows: false,
        docker_socket_hardening: config.docker_socket_hardening,
    })?;

    if !wait_for_online(client, repo, &replacement_name, config.online_wait_timeout)? {
        // Leave the differently-named replacement running for diagnosis and
        // keep the old runner untouched.
        return Ok(UpdateOutcome::Offline);
    }

    if let Some(runner) = runner {
        client.delete_runner(repo, runner.id)?;
    }

    if let Err(error) = docker::remove_container(name) {
        // The old GitHub registration has already been removed, so don't
        // risk leaving an unregistered replacement around under a temporary
        // name if Docker cannot remove the old container. Best effort cleanup
        // of the replacement is safe because it is not serving any job yet.
        let _ = docker::remove_container(&replacement_name);
        return Err(error.into());
    }

    if let Err(error) = docker::rename_container(&replacement_name, name) {
        // The replacement is online, but couldn't take the canonical name.
        // Preserve the live replacement rather than destroying a healthy
        // runner; surface the rename failure to the operator.
        return Err(error.into());
    }

    Ok(UpdateOutcome::Updated)
}

fn replacement_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{:x}", std::process::id())
}


fn wait_for_online(
    client: &GitHubClient,
    repo: &str,
    runner_name: &str,
    timeout: Duration,
) -> Result<bool> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);

    loop {
        if Instant::now() >= deadline {
            return Ok(false);
        }

        match client.list_runners(repo) {
            Ok(runners) => {
                if runners
                    .iter()
                    .any(|r| r.name == runner_name && r.is_online())
                {
                    return Ok(true);
                }
            }
            Err(crate::github::GitHubError::RateLimited { retry_after }) => {
                let delay = retry_after.unwrap_or(Duration::from_secs(60));
                std::thread::sleep(delay.min(deadline.saturating_duration_since(Instant::now())));
                continue;
            }
            Err(crate::github::GitHubError::Network(_)) => {
                // A transient network failure should not tear down the old
                // runner while the replacement is still booting.
            }
            Err(error) => return Err(error.into()),
        }

        std::thread::sleep(
            Duration::from_secs(3).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
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
        let dir =
            std::env::temp_dir().join(format!("gitrun-gtuu-stale-pid-{}", std::process::id()));
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
        assert!(
            reclaimed.is_ok(),
            "expected stale lock (dead PID) to be reclaimed"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_lock_without_pid_is_reclaimed_after_old_age() {
        let dir =
            std::env::temp_dir().join(format!("gitrun-gtuu-stale-age-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join("gtuu.lock");
        fs::create_dir(&lock_path).unwrap();
        let old_time = filetime::FileTime::from_system_time(
            SystemTime::now() - Duration::from_secs(60 * 60 * 2),
        );
        let _ = filetime::set_file_mtime(&lock_path, old_time);

        let reclaimed = GtuuLock::acquire(&dir);
        assert!(
            reclaimed.is_ok(),
            "expected an old lock without a usable PID to be reclaimed"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replacement_suffix_contains_pid_and_timestamp() {
        let suffix = replacement_suffix();
        assert!(suffix.contains(&format!("{:x}", std::process::id())));
        assert!(suffix.len() > 8);
    }

}
