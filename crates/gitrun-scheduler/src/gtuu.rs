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
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
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
    #[error("GitVault secret resolution failed: {0}")]
    Vault(String),
    #[error("local runner image refresh failed: {0}")]
    RunnerImageRefresh(String),
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

/// Filesystem lock preventing two GTUU runs from overlapping.
///
/// On Unix, a persistent sidecar file is locked with nonblocking `flock`
/// before inspecting or reclaiming the legacy PID directory. The kernel
/// releases that lock on process exit, including SIGKILL, and the sidecar is
/// deliberately never unlinked: unlinking a lock file can split contenders
/// across different inodes. Holding this guard across the stale-check and
/// directory rename prevents concurrent current-version GTUU runs from
/// reclaiming a newly-created live lock based on an earlier stale observation.
///
/// Non-Unix platforms retain the PID-directory fallback; its race and crash
/// recovery semantics are therefore weaker and must not be described as
/// cross-platform-safe.
pub struct GtuuLock {
    path: PathBuf,
    #[cfg(unix)]
    _guard: fs::File,
}

/// Fallback age for a lock that has no usable PID. A valid live PID is
/// never reclaimed solely because the run is old, so a slow but legitimate
/// update cannot accidentally overlap with another GTUU instance.
const STALE_LOCK_AGE: Duration = Duration::from_secs(60 * 60);

impl GtuuLock {
    pub fn acquire(state_dir: &Path) -> Result<Self> {
        fs::create_dir_all(state_dir)?;
        // Serialize stale detection, reclamation, and creation as one critical
        // section. A PID check followed by rename alone is racy: another
        // process can replace the stale directory between those operations.
        #[cfg(unix)]
        let guard = acquire_guard(state_dir)?;
        let path = state_dir.join("gtuu.lock");

        loop {
            match fs::create_dir(&path) {
                Ok(()) => {
                    if let Err(error) = fs::write(path.join("pid"), std::process::id().to_string())
                    {
                        let _ = fs::remove_dir_all(&path);
                        return Err(error.into());
                    }
                    return Ok(Self {
                        path,
                        #[cfg(unix)]
                        _guard: guard,
                    });
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

#[cfg(unix)]
fn acquire_guard(state_dir: &Path) -> Result<fs::File> {
    let guard_path = state_dir.join("gtuu.lock.guard");
    let guard = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(guard_path)?;
    // The guard file is intentionally persistent. Never unlink it after
    // unlocking, or a waiter could hold the old inode while a third process
    // locks a newly-created inode at the same path.
    let result = unsafe { libc::flock(guard.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK)
            || error.raw_os_error() == Some(libc::EAGAIN)
        {
            return Err(GtuuError::AlreadyRunning);
        }
        return Err(error.into());
    }
    Ok(guard)
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
    // PID 0 and values that cannot fit in kill(2)'s signed pid_t are not
    // valid process IDs here. In particular, casting u32::MAX would turn it
    // into -1, which has process-group-wide signal semantics.
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // Signal 0 performs no actual signal delivery, only existence/permission
    // checks (see kill(2)) — the standard portable way to ask "is this PID
    // alive" without a `/proc` dependency.
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
        // _guard is dropped after this method, releasing flock automatically.
    }
}

type SecretEnvForRepo<'a> = dyn Fn(&str) -> std::result::Result<Vec<(String, String)>, String> + 'a;

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
    pub cache_scope: &'a str,
    pub network: &'a str,
    pub seccomp_profile: &'a str,
    pub apparmor_profile: &'a str,
    pub docker_socket_gid: &'a str,
    pub runner_home_size: &'a str,
    /// See `docker::RunnerHomeBackend` / `Config::runner_home_backend`.
    pub runner_home_backend: docker::RunnerHomeBackend,
    pub runner_rootfs_read_only: bool,
    pub runner_windows_hyperv_isolation: bool,
    /// Looks up (and decrypts) GitVault secrets for a given repo, returning
    /// them ready to inject as environment variables. Passed as a closure
    /// rather than a fixed list because GTUU updates containers across
    /// potentially many repos in one run, each needing its own repo-scoped
    /// secrets — see `main.rs::vault_env_for_repo`, which this wraps.
    pub secret_env_for_repo: &'a SecretEnvForRepo<'a>,
    pub online_wait_timeout: Duration,
    /// See `Config::gsr_docker_socket_hardening` / `RunnerSpec::docker_socket_hardening`.
    pub docker_socket_hardening: bool,
    /// GSR policy values snapshotted by the runner entrypoint before startup.
    pub gsr_policy_env: &'a [(String, String)],
    /// Repository-specific Docker socket compatibility policy.
    pub docker_socket_enabled_for_repo: &'a dyn Fn(&str) -> bool,
}

/// Updates every permanent container for the configured repositories whose
/// image differs from the freshly pulled one. Returns the number actually
/// updated (mirrors the Python `update_permanent_containers` return value).
pub fn update_permanent_containers(client: &GitHubClient, config: &GtuuConfig) -> Result<u32> {
    update_permanent_containers_with_options(client, config, None, false, false)
}

/// Reconcile selected permanent runners. A forced rebuild validates a fresh
/// local image before promoting its tag; a forced replacement bypasses only
/// the image-ID equality shortcut, never the busy-runner safety gate.
pub fn update_permanent_containers_with_options(
    client: &GitHubClient,
    config: &GtuuConfig,
    repository_filter: Option<&str>,
    force_rebuild: bool,
    force_replace: bool,
) -> Result<u32> {
    if force_rebuild {
        gitrun_setup::rebuild_local_runner_image(config.image)
            .map_err(|error| GtuuError::RunnerImageRefresh(error.to_string()))?;
    } else {
        gitrun_setup::refresh_local_runner_image_if_stale(config.image)
            .map_err(|error| GtuuError::RunnerImageRefresh(error.to_string()))?;
    }
    docker::ensure_shared_cache_volume(config.shared_cache_volume)?;
    let new_image_id = pull_image(config.image)?;

    let mut updated = 0u32;
    for repo in config
        .repositories
        .iter()
        .filter(|repo| repository_filter.map_or(true, |filter| repo.eq_ignore_ascii_case(filter)))
    {
        for container in docker::managed_containers(repo)? {
            if !container.permanent {
                continue;
            }
            if update_one(
                client,
                config,
                repo,
                &container.name,
                &new_image_id,
                force_replace,
            )? == UpdateOutcome::Updated
            {
                updated += 1;
            }
        }
    }
    Ok(updated)
}

/// Read-only inventory for the GTUU CLI. It reports the configured image,
/// permanent container state, and whether each container uses the current
/// image without contacting GitHub or mutating Docker resources.
pub fn check_permanent_containers(
    config: &GtuuConfig,
    repository_filter: Option<&str>,
) -> Result<Vec<String>> {
    let expected_image = docker::image_id(config.image).ok();
    let mut report = vec![format!(
        "configured_image={} image_id={}",
        config.image,
        expected_image.as_deref().unwrap_or("MISSING")
    )];
    let mut found = 0usize;
    for repo in config
        .repositories
        .iter()
        .filter(|repo| repository_filter.map_or(true, |filter| repo.eq_ignore_ascii_case(filter)))
    {
        let containers = docker::managed_containers(repo)?;
        for container in containers.into_iter().filter(|item| item.permanent) {
            found += 1;
            let image_id = docker::container_image_id(&container.name)?
                .unwrap_or_else(|| "MISSING".to_owned());
            let matches_image = expected_image.as_deref() == Some(image_id.as_str());
            report.push(format!(
                "repo={} name={} status={} image_id={} image_match={}",
                repo, container.name, container.status, image_id, matches_image
            ));
        }
    }
    if found == 0 {
        report.push(
            "No permanent GitRun runner containers found for the selected repository scope."
                .to_owned(),
        );
    }
    Ok(report)
}

fn is_registry_image_reference(image: &str) -> bool {
    image.contains('/')
}

fn pull_image(image: &str) -> Result<String> {
    // Bootstrap builds the default `gitrun-runner:latest` image locally.
    // An unqualified reference has no registry/namespace, so blindly running
    // `docker pull` rewrites it to Docker Hub's `library/<name>` and fails
    // even when the locally built runner image is present.
    if !is_registry_image_reference(image) {
        return docker::image_id(image).map_err(|error| {
            GtuuError::Docker(crate::docker::DockerError::Command(format!(
                "local runner image {image:?} is unavailable; rebuild it with GitRun setup or configure a registry-qualified runner image: {error}"
            )))
        });
    }

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
    force_replace: bool,
) -> Result<UpdateOutcome> {
    let old_image_id = match current_image_id(name)? {
        UpdateCurrentImage::Present(id) => id,
        UpdateCurrentImage::Missing => return Ok(UpdateOutcome::Missing),
    };
    if old_image_id == new_image_id && !force_replace {
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
    let replacement_name = format!("{}-gtuu-{}", sanitize(name, '-'), replacement_suffix());
    let secret_env = (config.secret_env_for_repo)(repo).map_err(GtuuError::Vault)?;
    let docker_socket_enabled = (config.docker_socket_enabled_for_repo)(repo);

    docker::create_runner(&crate::docker::RunnerSpec {
        name: &replacement_name,
        repo,
        permanent: true,
        registration_token: &registration_token,
        image: config.image,
        labels: config.runner_labels,
        no_default_labels: false,
        ephemeral: config.ephemeral,
        disable_update: config.disable_update,
        cpus: config.cpus,
        memory: config.memory,
        pids_limit: config.pids_limit,
        shared_cache_volume: config.shared_cache_volume,
        cache_scope: config.cache_scope,
        network: config.network,
        seccomp_profile: config.seccomp_profile,
        apparmor_profile: config.apparmor_profile,
        docker_socket_gid: config.docker_socket_gid,
        runner_home_size: config.runner_home_size,
        home_backend: config.runner_home_backend,
        rootfs_read_only: config.runner_rootfs_read_only,
        secret_env: &secret_env,
        gsr_policy_env: config.gsr_policy_env,
        is_windows: false,
        workflow_job_name: None,
        workflow_run_id: None,
        workflow_job_id: None,
        workload_routing_id: None,
        prewarm_workload_id: None,
        prewarm_target_epoch: None,
        prewarm_hold_until_epoch: None,
        dock_target: false,
        docker_socket_enabled,
        docker_socket_hardening: config.docker_socket_hardening,
        windows_hyperv_isolation: config.runner_windows_hyperv_isolation,
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
        // The replacement has already appeared online and may have accepted a
        // job between the last API poll and this Docker failure. Never remove
        // it as cleanup: preserve the potentially busy, registered runner and
        // surface the incomplete swap. The old container may now be
        // unregistered, but deleting the healthy replacement could interrupt
        // an active workflow and would turn a recoverable partial swap into
        // job loss.
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

    #[cfg(unix)]
    #[test]
    fn invalid_pid_values_are_not_treated_as_live_processes() {
        assert!(!process_is_alive(0));
        assert!(!process_is_alive(u32::MAX));
        assert!(!process_is_alive(i32::MAX as u32 + 1));
    }

    #[test]
    fn local_default_runner_reference_is_not_treated_as_a_registry_image() {
        assert!(!is_registry_image_reference("gitrun-runner:latest"));
        assert!(is_registry_image_reference(
            "ghcr.io/vider06/gitrun-runner:latest"
        ));
        assert!(is_registry_image_reference("vider06/gitrun-runner:latest"));
    }

    #[test]
    fn replacement_suffix_contains_pid_and_timestamp() {
        let suffix = replacement_suffix();
        assert!(suffix.contains(&format!("{:x}", std::process::id())));
        assert!(suffix.len() > 8);
    }
}
