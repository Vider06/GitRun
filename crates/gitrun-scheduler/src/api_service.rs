//! Host-side workflow Git*Run API service.
//!
//! The runner image contains only a small CLI client. This service owns the
//! privileged side: it authenticates the runner capability, asks GSR to
//! authorize the exact operation, and only then invokes GitExecuteRun's
//! backend implementation.

use crate::dock_registry::{validate_state_directory_parent, DockBinding, DockRegistry};
use crate::docker;
use crate::GitHubClient;
use gitrun_core::{
    api_policy::parse_boolean_argument, command_policy::parse_simple_command, Config, GitRunApi,
    GitRunOperation, GitRunSettings,
};
use gitrun_exe::{
    ipc::{WireRequest, WireResponse, DEFAULT_SOCKET_PATH},
    ApiInvocation, AuthorizedOperation, ExecutionBackend, ExecutionError, ExecutionEvent,
    ExecutionResult,
};
use gitrun_gsr::api_gate::{authorize, ExecutionAuthority, VerifiedCaller};
use gitrun_vault::{Scope, Vault};
use std::fs;
#[cfg(unix)]
use std::io::Read;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command;
#[cfg(unix)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::Mutex;
#[cfg(unix)]
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

const DOCK_ONLY_MARKER: &str = "/var/lib/gitrun/control/dock-only";
const API_MAX_REQUEST_BYTES: usize = 1024 * 1024;
#[cfg(unix)]
const MAX_API_REQUESTS: usize = 64;
#[cfg(unix)]
const API_READ_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(unix)]
const API_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_VAULT_WRITE_BYTES: usize = 1024 * 1024;

#[cfg(unix)]
static ACTIVE_API_REQUESTS: AtomicUsize = AtomicUsize::new(0);

/// Bounds the number of request threads that can be kept alive by socket peers.
#[cfg(unix)]
struct ApiRequestPermit;

#[cfg(unix)]
impl ApiRequestPermit {
    fn try_acquire() -> Option<Self> {
        let mut active = ACTIVE_API_REQUESTS.load(Ordering::Acquire);
        loop {
            if active >= MAX_API_REQUESTS {
                return None;
            }

            match ACTIVE_API_REQUESTS.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self),
                Err(observed) => active = observed,
            }
        }
    }
}

#[cfg(unix)]
impl Drop for ApiRequestPermit {
    fn drop(&mut self) {
        ACTIVE_API_REQUESTS.fetch_sub(1, Ordering::AcqRel);
    }
}
#[cfg(target_os = "linux")]
const MOUNT_REGISTRY_FILE: &str = "gitdock-mount-registry.json";
#[cfg(target_os = "linux")]
const MOUNT_REGISTRY_LOCK_FILE: &str = "gitdock-mount-registry.lock";
#[cfg(target_os = "linux")]
static GITDOCK_MOUNT_LOCK: Mutex<()> = Mutex::new(());

#[cfg(target_os = "linux")]
struct MountRegistryLock(fs::File);

#[cfg(target_os = "linux")]
impl MountRegistryLock {
    fn acquire(state_dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(state_dir)
            .map_err(|error| format!("create mount registry lock directory: {error}"))?;
        validate_mount_registry_directory(state_dir)?;
        let path = state_dir.join(MOUNT_REGISTRY_LOCK_FILE);
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        }
        let file = options
            .open(&path)
            .map_err(|error| format!("open mount registry lock: {error}"))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("inspect mount registry lock: {error}"))?;
        if !metadata.is_file() {
            return Err("mount registry lock path is not a regular file".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() != 1 {
                return Err("mount registry lock must not have hard links".into());
            }
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("secure mount registry lock permissions: {error}"))?;
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "lock GitDockRun mount registry: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self(file))
    }
}

#[cfg(target_os = "linux")]
impl Drop for MountRegistryLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(target_os = "linux")]
struct GitDockLifecycleGuard {
    _local: std::sync::MutexGuard<'static, ()>,
    _cross_process: MountRegistryLock,
}

#[cfg(target_os = "linux")]
fn acquire_gitdock_lifecycle_guard(state_dir: &Path) -> Result<GitDockLifecycleGuard, String> {
    // Use the same lock order for every GitDockRun lifecycle writer: first the
    // process-local mutex, then the cross-process flock. Atomic file replacement
    // protects readers from partial JSON, but does not prevent lost updates.
    let local = GITDOCK_MOUNT_LOCK
        .lock()
        .map_err(|_| "GitDockRun lifecycle lock is poisoned".to_owned())?;
    let cross_process = MountRegistryLock::acquire(state_dir)?;
    Ok(GitDockLifecycleGuard {
        _local: local,
        _cross_process: cross_process,
    })
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MountRecord {
    id: String,
    repository: String,
    run_id: u64,
    job: String,
    requester_runner: String,
    container: String,
    container_id: String,
    source: String,
    target: String,
    read_only: bool,
    #[serde(default)]
    source_device: Option<u64>,
    #[serde(default)]
    source_inode: Option<u64>,
    #[serde(default)]
    kernel_mount_id: Option<u32>,
    #[serde(default)]
    state: String,
    created_at: u64,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct MountRegistry {
    mounts: Vec<MountRecord>,
}

#[cfg(target_os = "linux")]
fn validate_mount_registry_directory(state_dir: &Path) -> Result<(), String> {
    validate_state_directory_parent(state_dir).map_err(|error| error.to_string())?;
    match fs::symlink_metadata(state_dir) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err("GitDockRun registry state path must be a real directory".into());
            }
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            // This directory contains leases trusted by a root-privileged mount
            // worker. Reject service state that is owned by another identity.
            // Checking mode bits alone is not enough: an untrusted directory
            // owner can unlink a mode-0600 file and replace the directory entry.
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(
                    "GitDockRun registry state directory must be owned by the service identity"
                        .into(),
                );
            }
            if metadata.permissions().mode() & 0o022 != 0 {
                return Err(
                    "GitDockRun registry state directory must not be group- or world-writable"
                        .into(),
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "inspect GitDockRun registry state directory: {error}"
        )),
    }
}

#[cfg(target_os = "linux")]
fn read_secure_mount_registry(path: &Path) -> Result<Option<String>, String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open GitDockRun mount registry safely: {error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect GitDockRun mount registry: {error}"))?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err("GitDockRun mount registry must be a regular file without hard links".into());
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err("GitDockRun mount registry must be owned by the service identity".into());
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err("GitDockRun mount registry must not be group- or world-writable".into());
    }
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|error| format!("read GitDockRun mount registry: {error}"))?;
    Ok(Some(raw))
}

#[cfg(target_os = "linux")]
impl MountRegistry {
    fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(MOUNT_REGISTRY_FILE)
    }

    fn load(state_dir: &Path) -> Result<Self, String> {
        validate_mount_registry_directory(state_dir)?;
        let Some(raw) = read_secure_mount_registry(&Self::path(state_dir))? else {
            return Ok(Self::default());
        };
        let registry: Self = serde_json::from_str(&raw)
            .map_err(|error| format!("decode GitDockRun mount registry: {error}"))?;

        registry.validate()?;
        Ok(registry)
    }

    fn validate(&self) -> Result<(), String> {
        // Mount IDs are capabilities used by UNMOUNT and cleanup. A duplicate
        // ID makes ownership ambiguous and removing by ID could erase unrelated
        // leases after cleaning only one record. Enforce this invariant on both
        // load and save, not only when the registry is read back later.
        let mut ids = std::collections::BTreeSet::new();
        for mount in &self.mounts {
            if mount.id.trim().is_empty() {
                return Err("GitDockRun mount registry contains an empty mount ID".into());
            }
            if !ids.insert(mount.id.as_str()) {
                return Err(format!(
                    "GitDockRun mount registry contains duplicate mount ID {:?}",
                    mount.id
                ));
            }
            if mount.repository.trim().is_empty()
                || mount.run_id == 0
                || mount.job.trim().is_empty()
                || mount.container_id.trim().is_empty()
                || mount.requester_runner.trim().is_empty()
                || mount.target.trim().is_empty()
            {
                return Err(
                    "GitDockRun mount registry contains an incomplete ownership record".into(),
                );
            }
        }
        Ok(())
    }

    fn save(&self, state_dir: &Path) -> Result<(), String> {
        self.validate()?;
        validate_state_directory_parent(state_dir).map_err(|error| error.to_string())?;
        fs::create_dir_all(state_dir)
            .map_err(|error| format!("create mount registry directory: {error}"))?;
        validate_mount_registry_directory(state_dir)?;
        let path = Self::path(state_dir);
        let temp = path.with_extension(format!(
            "json.{}.{}.tmp",
            std::process::id(),
            new_mount_id()?
        ));
        let raw = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("encode GitDockRun mount registry: {error}"))?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .map_err(|error| format!("create temporary mount registry: {error}"))?;
        if let Err(error) = file.write_all(&raw).and_then(|()| file.sync_all()) {
            let _ = fs::remove_file(&temp);
            return Err(format!("write mount registry: {error}"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("secure mount registry permissions: {error}"))?;
        }
        if let Err(error) = fs::rename(&temp, &path) {
            let _ = fs::remove_file(&temp);
            return Err(format!("commit mount registry: {error}"));
        }
        fs::File::open(state_dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("sync mount registry directory: {error}"))?;
        Ok(())
    }
}

pub fn spawn(
    config: Config,
    github: Arc<GitHubClient>,
    stopping: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>, String> {
    #[cfg(unix)]
    {
        let socket_path =
            std::env::var("GITRUN_API_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET_PATH.to_owned());
        let path = PathBuf::from(&socket_path);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create API socket directory: {e}"))?;
        }
        if path.exists() {
            fs::remove_file(&path)
                .map_err(|e| format!("remove stale API socket {}: {e}", path.display()))?;
        }

        let listener = UnixListener::bind(&path)
            .map_err(|e| format!("bind GitRun API socket {}: {e}", path.display()))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("set GitRun API socket nonblocking: {e}"))?;

        // The socket is a defense-in-depth filesystem boundary in addition to
        // SO_PEERCRED/cgroup verification. The runner image joins this numeric
        // group at startup; unrelated host users are excluded by the mode bits.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o660))
            .map_err(|e| format!("set API socket permissions: {e}"))?;
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::chown;
            chown(&path, Some(0), Some(10000)).map_err(|e| format!("set API socket group: {e}"))?;
        }

        let state_dir = Path::new(&config.state_dir).to_path_buf();
        let execution_authority = Arc::new(
            ExecutionAuthority::new()
                .map_err(|error| format!("initialize GSR execution authority: {error}"))?,
        );
        let thread_stopping = stopping.clone();

        let handle = std::thread::Builder::new()
            .name("gitrun-api".into())
            .spawn(move || {
                while !thread_stopping.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            // A local peer must not be able to pin a worker indefinitely
                            // with a partial request or a client that never reads replies.
                            let timeout_result = stream
                                .set_read_timeout(Some(API_READ_TIMEOUT))
                                .and_then(|()| stream.set_write_timeout(Some(API_WRITE_TIMEOUT)));
                            if let Err(error) = timeout_result {
                                eprintln!("gitrun-api: unable to set socket deadlines: {error}");
                                continue;
                            }

                            // Enforce backpressure before spawning: the service has a
                            // fixed upper bound on request threads, even under a local
                            // connection flood.
                            let Some(permit) = ApiRequestPermit::try_acquire() else {
                                let _ = send_error(
                                    &mut stream,
                                    "GitRun API is busy; concurrent request limit reached",
                                );
                                continue;
                            };

                            let service_config = config.clone();
                            let service_github = Arc::clone(&github);
                            let service_state = state_dir.clone();
                            let service_authority = Arc::clone(&execution_authority);
                            if let Err(error) = std::thread::Builder::new()
                                .name("gitrun-api-request".into())
                                .spawn(move || {
                                    let _permit = permit;
                                    if let Err(error) = handle_stream(
                                        stream,
                                        service_config,
                                        service_github,
                                        service_state,
                                        service_authority,
                                    ) {
                                        eprintln!("gitrun-api: request failed: {error}");
                                    }
                                })
                            {
                                // The stream is dropped if thread creation fails. The client
                                // must treat that EOF as failure unless it received Finished.
                                eprintln!("gitrun-api: unable to spawn request handler: {error}");
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                        Err(error) => {
                            eprintln!("gitrun-api: accept failed: {error}");
                            std::thread::sleep(std::time::Duration::from_millis(250));
                        }
                    }
                }

                let _ = fs::remove_file(&path);
            })
            .map_err(|e| format!("spawn GitRun API service: {e}"))?;

        Ok(handle)
    }

    #[cfg(not(unix))]
    {
        let _ = (config, github, stopping);
        Err("GitRun API socket service is not implemented on this platform yet".into())
    }
}

pub(crate) fn reconcile_dock_bindings(
    client: &GitHubClient,
    state_dir: &Path,
    repository: &str,
) -> Result<(), String> {
    // Mount leases are the recovery journal for privileged namespace changes.
    // If the journal cannot be read, locked, reconciled, or durably updated,
    // do not continue lifecycle reconciliation as if cleanup were trustworthy.
    reconcile_completed_mounts(client, state_dir, repository)
        .map_err(|error| format!("reconcile GitDockRun mounts before binding cleanup: {error}"))?;
    #[cfg(target_os = "linux")]
    let _lifecycle_guard = acquire_gitdock_lifecycle_guard(state_dir)?;

    let mut registry = DockRegistry::load(state_dir)
        .map_err(|error| format!("load GitDockRun registry: {error}"))?;
    let mut changed = false;

    for binding in registry
        .bindings
        .iter_mut()
        .filter(|binding| binding.repository == repository && binding.dynamic && !binding.dock_only)
    {
        let Some(job) = client
            .find_workflow_job(repository, binding.run_id, &binding.job)
            .map_err(|error| format!("check GitDockRun job {}: {error}", binding.job))?
        else {
            continue;
        };

        let Some(current_id) = docker::container_id(&binding.container)
            .map_err(|error| format!("resolve Dock container {}: {error}", binding.container))?
        else {
            continue;
        };
        if binding.container_id.is_empty() || current_id != binding.container_id {
            // Never mutate a different container that happens to reuse the
            // logical name recorded by an old binding.
            continue;
        }

        let completed = workflow_job_is_completed(&job.status, job.conclusion.as_deref());

        if completed {
            make_dock_only(&binding.container_id, state_dir).map_err(|error| {
                format!(
                    "freeze GitRun Dock container {}: {error}",
                    binding.container
                )
            })?;
            binding.dock_only = true;
            changed = true;
        }
    }

    if changed {
        registry
            .save(state_dir)
            .map_err(|error| format!("save GitDockRun registry: {error}"))?;
    }

    Ok(())
}

pub(crate) fn reconcile_dock_target_containers(
    client: &GitHubClient,
    state_dir: &Path,
    repository: &str,
    containers: &[docker::ManagedContainer],
) -> Result<(), String> {
    for container in containers.iter().filter(|container| container.dock_target) {
        let (Some(run_id), Some(job)) =
            (container.workflow_run_id, container.workflow_job.as_deref())
        else {
            continue;
        };

        let Some(job_info) = client
            .find_workflow_job(repository, run_id, job)
            .map_err(|error| format!("check Dock target {job}: {error}"))?
        else {
            continue;
        };

        let completed = workflow_job_is_completed(&job_info.status, job_info.conclusion.as_deref());
        if completed
            && container.status.eq_ignore_ascii_case("running")
            && container.container_is_dynamic()
        {
            make_dock_only(&container.name, state_dir)
                .map_err(|error| format!("freeze Dock target {}: {error}", container.name))?;
        }
    }

    Ok(())
}

trait ManagedContainerDockExt {
    fn container_is_dynamic(&self) -> bool;
}

impl ManagedContainerDockExt for docker::ManagedContainer {
    fn container_is_dynamic(&self) -> bool {
        !self.permanent
    }
}

pub(crate) fn preserved_dock_containers(
    state_dir: &Path,
    repository: &str,
) -> Result<std::collections::BTreeSet<String>, String> {
    let registry = DockRegistry::load(state_dir)
        .map_err(|error| format!("load GitDockRun registry: {error}"))?;
    let mut names = std::collections::BTreeSet::new();
    for binding in registry
        .bindings
        .into_iter()
        .filter(|binding| binding.repository == repository)
    {
        let Some(current_id) = docker::container_id(&binding.container)
            .map_err(|error| format!("resolve preserved Dock {}: {error}", binding.container))?
        else {
            continue;
        };
        if !binding.container_id.is_empty() && current_id == binding.container_id {
            names.insert(binding.container);
        }
    }
    Ok(names)
}

fn dock_binding_matches_current_id(binding_id: &str, current_id: &str) -> bool {
    !binding_id.is_empty() && binding_id == current_id
}

pub(crate) fn is_dock_bound(
    state_dir: &Path,
    repository: &str,
    container: &str,
) -> Result<bool, String> {
    let registry = DockRegistry::load(state_dir)
        .map_err(|error| format!("load GitDockRun registry: {error}"))?;
    for binding in registry
        .bindings
        .iter()
        .filter(|binding| binding.repository == repository && binding.container == container)
    {
        // A binding protects only the exact Docker object captured at connect
        // time. Reusing a container name must not inherit an old job's
        // lifecycle protection.
        let Some(current_id) = docker::container_id(&binding.container)
            .map_err(|error| format!("resolve bound Dock {}: {error}", binding.container))?
        else {
            continue;
        };
        if dock_binding_matches_current_id(&binding.container_id, &current_id) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(unix)]
fn read_api_request(stream: &UnixStream) -> Result<Option<Vec<u8>>, String> {
    read_api_request_with_timeout(stream, API_READ_TIMEOUT)
}

/// Read exactly one newline-terminated request line under an absolute deadline.
/// A socket read timeout alone is only an inactivity timeout; recalculating the
/// remaining budget on every read prevents a slow peer from trickling bytes forever.
#[cfg(unix)]
fn read_api_request_with_timeout(
    stream: &UnixStream,
    timeout: Duration,
) -> Result<Option<Vec<u8>>, String> {
    let deadline = Instant::now() + timeout;
    let mut encoded = Vec::with_capacity(4096);
    let mut reader = stream;
    let mut chunk = [0u8; 8192];

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // Some platforms reject sub-millisecond socket timeouts after rounding
        // them down to zero. Treat the final fraction of the deadline as expired
        // rather than leaking a platform-specific set_read_timeout error.
        if remaining < Duration::from_millis(1) {
            return Err("API request read deadline exceeded".into());
        }
        if let Err(error) = stream.set_read_timeout(Some(remaining)) {
            // The absolute deadline can expire between calculating the remaining
            // duration and applying the socket timeout. Report the deadline in that
            // case rather than leaking a platform-specific configuration error.
            if Instant::now() >= deadline {
                return Err("API request read deadline exceeded".into());
            }
            return Err(format!("set API request read deadline: {error}"));
        }

        let read_result = reader.read(&mut chunk);
        // A read can complete successfully after the OS timeout when the
        // process is descheduled or the peer's final byte races the deadline.
        // Enforce the absolute deadline after the syscall as well as before it.
        if Instant::now() >= deadline {
            return Err("API request read deadline exceeded".into());
        }

        match read_result {
            Ok(0) if encoded.is_empty() => return Ok(None),
            Ok(0) => {
                return Err("API request ended before its newline terminator".into());
            }
            Ok(count) => {
                if let Some(newline) = chunk[..count].iter().position(|byte| *byte == b'\n') {
                    encoded.extend_from_slice(&chunk[..newline]);
                    if encoded.len() > API_MAX_REQUEST_BYTES {
                        return Err("API request is too large".into());
                    }
                    return Ok(Some(encoded));
                }

                encoded.extend_from_slice(&chunk[..count]);
                if encoded.len() > API_MAX_REQUEST_BYTES {
                    return Err("API request is too large".into());
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                return Err("API request read deadline exceeded".into());
            }
            Err(error) => return Err(format!("read API request: {error}")),
        }
    }
}

#[cfg(unix)]
trait ApiIdentityProvider: Send + Sync {
    fn runner_for_peer_pid(
        &self,
        peer_pid: i32,
    ) -> Result<Option<docker::ApiRunnerIdentity>, String>;

    fn find_active_workflow_job_for_runner(
        &self,
        repository: &str,
        runner_name: &str,
    ) -> Result<Option<crate::github::WorkflowJobInfo>, String>;

    fn find_workflow_job(
        &self,
        repository: &str,
        run_id: u64,
        job_name: &str,
    ) -> Result<Option<crate::github::WorkflowJobInfo>, String>;

    fn find_workflow_run(
        &self,
        repository: &str,
        run_id: u64,
    ) -> Result<Option<crate::github::WorkflowRunInfo>, String>;
}

#[cfg(unix)]
struct SystemApiIdentityProvider {
    github: Arc<GitHubClient>,
}

#[cfg(unix)]
impl ApiIdentityProvider for SystemApiIdentityProvider {
    fn runner_for_peer_pid(
        &self,
        peer_pid: i32,
    ) -> Result<Option<docker::ApiRunnerIdentity>, String> {
        docker::runner_for_peer_pid(peer_pid).map_err(|error| error.to_string())
    }

    fn find_active_workflow_job_for_runner(
        &self,
        repository: &str,
        runner_name: &str,
    ) -> Result<Option<crate::github::WorkflowJobInfo>, String> {
        self.github
            .find_active_workflow_job_for_runner(repository, runner_name)
            .map_err(|error| error.to_string())
    }

    fn find_workflow_job(
        &self,
        repository: &str,
        run_id: u64,
        job_name: &str,
    ) -> Result<Option<crate::github::WorkflowJobInfo>, String> {
        self.github
            .find_workflow_job(repository, run_id, job_name)
            .map_err(|error| error.to_string())
    }

    fn find_workflow_run(
        &self,
        repository: &str,
        run_id: u64,
    ) -> Result<Option<crate::github::WorkflowRunInfo>, String> {
        self.github
            .find_workflow_run(repository, run_id)
            .map_err(|error| error.to_string())
    }
}

#[cfg(unix)]
fn handle_stream(
    stream: UnixStream,
    config: Config,
    github: Arc<GitHubClient>,
    state_dir: std::path::PathBuf,
    execution_authority: Arc<ExecutionAuthority>,
) -> Result<(), String> {
    let identity_provider = SystemApiIdentityProvider {
        github: Arc::clone(&github),
    };
    handle_stream_with_identity_provider(
        stream,
        config,
        github,
        state_dir,
        execution_authority,
        &identity_provider,
    )
}

#[cfg(unix)]
fn handle_stream_with_identity_provider(
    mut stream: UnixStream,
    config: Config,
    github: Arc<GitHubClient>,
    state_dir: std::path::PathBuf,
    execution_authority: Arc<ExecutionAuthority>,
    identity_provider: &dyn ApiIdentityProvider,
) -> Result<(), String> {
    let encoded = match read_api_request(&stream) {
        Ok(Some(encoded)) => encoded,
        Ok(None) => return Ok(()),
        Err(error) => {
            send_error(&mut stream, &error)?;
            return Ok(());
        }
    };

    let request: WireRequest = match serde_json::from_slice(&encoded) {
        Ok(request) => request,
        Err(error) => {
            send_error(&mut stream, &format!("invalid API request: {error}"))?;
            return Ok(());
        }
    };

    let peer_pid =
        peer_pid(&stream).map_err(|error| format!("read API socket peer credentials: {error}"))?;
    let identity = match identity_provider.runner_for_peer_pid(peer_pid) {
        Ok(Some(identity)) => identity,
        Ok(None) => {
            send_error(
                &mut stream,
                "GitRun API caller is not a managed runner container",
            )?;
            return Ok(());
        }
        Err(error) => {
            send_error(
                &mut stream,
                "GitRun API caller identity could not be checked",
            )?;
            return Err(error.to_string());
        }
    };

    let binding = match validate_runner_invocation_binding(&identity, &request.invocation) {
        Ok(binding) => binding,
        Err(reason) => {
            send_error(&mut stream, reason)?;
            return Ok(());
        }
    };
    let run_id = binding.run_id;

    if binding.requires_active_job_lookup {
        let Some(active_job) = identity_provider
            .find_active_workflow_job_for_runner(&request.invocation.repository, &identity.name)
            .map_err(|error| format!("verify active workflow job: {error}"))?
        else {
            send_error(
                &mut stream,
                "no active GitHub workflow job is assigned to this runner",
            )?;
            return Ok(());
        };
        if !active_job_matches(&active_job.name, &request.invocation.job) {
            send_error(
                &mut stream,
                "requested workflow job is not the active job assigned to this runner",
            )?;
            return Ok(());
        }
    }

    let job = request.invocation.job.clone();
    let current_job = identity_provider
        .find_workflow_job(&request.invocation.repository, run_id, &job)
        .map_err(|error| format!("verify current workflow job: {error}"))?;
    let Some(current_job) = current_job else {
        send_error(
            &mut stream,
            "current GitHub workflow job could not be verified",
        )?;
        return Ok(());
    };
    if !workflow_job_id_matches(identity.workflow_job_id, current_job.id) {
        send_error(
            &mut stream,
            "authoritative GitHub job ID does not match the runner's exact queued-job binding",
        )?;
        return Ok(());
    }
    if !workflow_job_is_active_on_runner(
        current_job.runner_name.as_deref(),
        &current_job.status,
        current_job.conclusion.as_deref(),
        &identity.name,
    ) {
        send_error(
            &mut stream,
            "GitRun API request is not being made by the active runner for this workflow job",
        )?;
        return Ok(());
    }

    let workflow_run = identity_provider
        .find_workflow_run(&request.invocation.repository, run_id)
        .map_err(|error| format!("verify authoritative workflow run: {error}"))?;
    let Some(workflow_run) = workflow_run else {
        send_error(
            &mut stream,
            "authoritative GitHub workflow run could not be verified",
        )?;
        return Ok(());
    };
    if !workflow_claim_matches(&request.invocation.workflow, &workflow_run.name) {
        send_error(&mut stream, "workflow identity claim does not match GitHub")?;
        return Ok(());
    }

    let caller = VerifiedCaller::new(
        &request.invocation.repository,
        &workflow_run.name,
        &request.invocation.job,
        &identity.name,
    )
    .with_run_id(Some(workflow_run.id));

    let settings_path = GitRunSettings::path_for_state_dir(&state_dir);
    let settings = GitRunSettings::load_or_default(&settings_path)
        .map_err(|error| format!("unable to load GitRun settings: {error}"))?;
    let command_policy = config.command_policy();

    let backend = ApiExecutionBackend {
        config,
        state_dir,
        github,
        caller: caller.clone(),
        caller_container_id: identity.container_id.clone(),
    };

    let mut write_wire_response = |response: WireResponse| {
        send_response(&mut stream, response).map_err(|error| error.to_string())
    };
    execute_gsr_authorized_stream(
        &settings,
        &caller,
        &request.invocation,
        command_policy.as_ref(),
        &execution_authority,
        &backend,
        &mut write_wire_response,
    )?;

    Ok(())
}

#[cfg(unix)]
#[cfg(target_os = "linux")]
fn peer_pid(stream: &UnixStream) -> Result<i32, std::io::Error> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(cred.pid)
}

#[cfg(not(target_os = "linux"))]
fn peer_pid(_stream: &UnixStream) -> Result<i32, std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "peer credential authentication requires Linux",
    ))
}

fn send_response(stream: &mut UnixStream, response: WireResponse) -> Result<(), String> {
    let encoded =
        serde_json::to_string(&response).map_err(|e| format!("encode API response: {e}"))?;
    stream
        .write_all(encoded.as_bytes())
        .map_err(|e| format!("write API response: {e}"))?;
    stream
        .write_all(b"\n")
        .map_err(|e| format!("finish API response: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("flush API response: {e}"))
}

#[cfg(unix)]
fn send_error(stream: &mut UnixStream, message: &str) -> Result<(), String> {
    send_response(
        stream,
        WireResponse::Error {
            message: message.to_owned(),
        },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunnerInvocationBinding {
    run_id: u64,
    requires_active_job_lookup: bool,
}

/// Validate request claims against the identity discovered from SO_PEERCRED's
/// owning container. This function is deliberately pure so all deny branches
/// can be tested without mocking or invoking the Docker CLI.
fn validate_runner_invocation_binding(
    identity: &docker::ApiRunnerIdentity,
    invocation: &ApiInvocation,
) -> Result<RunnerInvocationBinding, &'static str> {
    if identity.name != invocation.runner || identity.repository != invocation.repository {
        return Err("runner API identity does not match the managed container");
    }

    let run_id = invocation
        .run_id
        .ok_or("GITHUB_RUN_ID is required for every GitRun API invocation")?;

    let requires_active_job_lookup =
        match (identity.workflow_run_id, identity.workflow_job.as_deref()) {
            (Some(bound_run), Some(bound_job))
                if bound_run != run_id || bound_job != invocation.job =>
            {
                return Err("workflow job identity does not match the runner container binding");
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err("runner container has incomplete workflow binding metadata");
            }
            (Some(_), Some(_)) => false,
            (None, None) => true,
        };

    Ok(RunnerInvocationBinding {
        run_id,
        requires_active_job_lookup,
    })
}

fn workflow_job_id_matches(bound_job_id: Option<u64>, authoritative_job_id: u64) -> bool {
    bound_job_id.is_none_or(|bound| bound == authoritative_job_id)
}

fn active_job_matches(active_job_name: &str, requested_job: &str) -> bool {
    active_job_name == requested_job
}

fn workflow_job_is_active_on_runner(
    runner_name: Option<&str>,
    status: &str,
    conclusion: Option<&str>,
    expected_runner: &str,
) -> bool {
    runner_name == Some(expected_runner)
        && status.eq_ignore_ascii_case("in_progress")
        && conclusion.is_none()
}

fn workflow_claim_matches(claim: &str, authoritative_workflow: &str) -> bool {
    claim.is_empty() || claim == authoritative_workflow
}

/// Execute only the exact operation that GSR authorized and authenticated.
/// Keeping this ordering in one helper makes the privileged boundary testable
/// with a recording backend, without requiring a live Docker daemon or GitHub API.
fn execute_gsr_authorized_stream(
    settings: &GitRunSettings,
    caller: &VerifiedCaller,
    invocation: &ApiInvocation,
    command_policy: Option<&gitrun_core::command_policy::CommandPolicy>,
    execution_authority: &ExecutionAuthority,
    backend: &dyn ExecutionBackend,
    write_response: &mut dyn FnMut(WireResponse) -> Result<(), String>,
) -> Result<(), String> {
    if let Err(reason) = validate_verified_caller_binding(caller, invocation) {
        write_response(WireResponse::Error {
            message: format!("GSR caller binding failed: {reason}"),
        })?;
        return Ok(());
    }

    let authorized = match authorize(
        settings,
        caller,
        invocation.api,
        invocation.operation,
        invocation.resource.as_deref(),
        invocation.arguments.clone(),
        command_policy,
    ) {
        Ok(operation) => operation,
        Err(error) => {
            write_response(WireResponse::Error {
                message: format!("GSR validation failed: {error}"),
            })?;
            return Ok(());
        }
    };

    let authorized = execution_authority
        .handoff_to_executor(authorized)
        .map_err(|error| format!("GSR executor handoff failed: {error}"))?;

    // A client must observe acceptance before any execution event.
    write_response(WireResponse::Accepted)?;

    let mut send_event = |event: ExecutionEvent| {
        // Match the existing streaming contract: a disconnected client must
        // not panic the privileged worker while it is reporting output.
        let _ = write_response(WireResponse::Event(event));
    };
    let result = backend.execute_stream(&authorized, &mut send_event);
    drop(send_event);

    if let Err(error) = result {
        write_response(WireResponse::Error {
            message: format!("GitExecuteRun failed: {error}"),
        })?;
    }
    Ok(())
}

/// Re-check the invocation claims against the trusted caller object at the
/// last boundary before authorization and dispatch. The transport layer still
/// performs peer/container and GitHub verification; this check prevents future
/// callers from accidentally skipping the claim-to-identity binding.
fn validate_verified_caller_binding(
    caller: &VerifiedCaller,
    invocation: &ApiInvocation,
) -> Result<(), &'static str> {
    if invocation.repository != caller.repository {
        return Err("repository claim differs from verified caller");
    }
    if !invocation.workflow.is_empty() && invocation.workflow != caller.workflow {
        return Err("workflow claim differs from verified caller");
    }
    if invocation.run_id != caller.run_id {
        return Err("workflow run ID differs from verified caller");
    }
    if invocation.job != caller.job {
        return Err("workflow job claim differs from verified caller");
    }
    if invocation.runner != caller.runner {
        return Err("runner claim differs from verified caller");
    }
    Ok(())
}

struct ApiExecutionBackend {
    config: Config,
    state_dir: std::path::PathBuf,
    github: Arc<GitHubClient>,
    caller: VerifiedCaller,
    caller_container_id: String,
}

impl ExecutionBackend for ApiExecutionBackend {
    fn execute(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        match request.api {
            GitRunApi::GitStatusRun => self.status(request),
            GitRunApi::GitDispatchRun => self.dispatch_plan(request),
            GitRunApi::GitVaultRun => self.vault(request),
            GitRunApi::GitSaveRun => self.save(request),
            GitRunApi::GitRegisterRun => self.register(request),
            GitRunApi::GitInstallRun => self.install(request),
            GitRunApi::GitReadRun => self.read(request),
            GitRunApi::GitWriteRun => self.write(request),
            GitRunApi::GitVerifyRun => self.verify(request),
            GitRunApi::GitDockRun => self.dock(request),
        }
    }

    fn execute_stream(
        &self,
        request: &AuthorizedOperation,
        sink: &mut dyn FnMut(ExecutionEvent),
    ) -> Result<i32, ExecutionError> {
        sink(ExecutionEvent::Started);

        let exit_code = match (request.api, request.operation) {
            (GitRunApi::GitDockRun, GitRunOperation::Execute) => {
                self.dock_execute_stream(request, sink)?
            }
            (GitRunApi::GitInstallRun, GitRunOperation::Install) => {
                self.install_stream(request, sink)?
            }
            _ => {
                let result = self.execute(request)?;
                if !result.stdout.is_empty() {
                    sink(ExecutionEvent::Stdout(result.stdout.clone()));
                }
                if !result.stderr.is_empty() {
                    sink(ExecutionEvent::Stderr(result.stderr.clone()));
                }
                result.exit_code
            }
        };

        sink(ExecutionEvent::Finished { exit_code });
        Ok(exit_code)
    }
}

impl ApiExecutionBackend {
    fn dispatch_plan(
        &self,
        request: &AuthorizedOperation,
    ) -> Result<ExecutionResult, ExecutionError> {
        let target_job = arg(request, "target_job")?;
        let target_workflow = arg(request, "target_workflow")?;
        let target_labels: Vec<String> = arg(request, "target_labels")?
            .split(',')
            .map(str::trim)
            .map(str::to_owned)
            .collect();
        // Dispatch is opt-in and deliberately falls back to normal GitHub label
        // matching for every disabled, stale, unsupported or ambiguous state.
        let mut kitty = gitrun_smart_scheduler::KittyRuntime::new(&self.state_dir);
        // This call reloads the persisted, validated settings on a fresh runtime.
        // Do it before reading settings(); otherwise the in-memory defaults would
        // make every dispatch appear disabled even when Kitty is configured.
        let estimate = kitty.resource_demand_estimate(
            request.repository.as_str(),
            target_workflow.as_str(),
            target_job.as_str(),
            &target_labels,
        );
        let settings = kitty.settings().clone();
        // Report Kitty availability independently from the selected route:
        // Kitty can be enabled while a prerequisite or shadow mode keeps this
        // job on ordinary GitHub label matching.
        let fallback = |status: &str, reason: &str| {
            kitty_dispatch_plan_result("github", settings.enabled, status, reason, &target_labels)
        };
        if !settings.enabled {
            return fallback("disabled", "kitty-disabled-or-settings-invalid");
        }
        if !settings.telemetry_enabled {
            return fallback("degraded", "aggregate-telemetry-disabled");
        }
        if !settings.adaptive_resource_management_enabled {
            return fallback("disabled", "adaptive-resource-management-disabled");
        }
        if !settings.kitty_dispatch_enabled {
            return fallback("disabled", "kitty-dispatch-opt-in-disabled");
        }
        if settings.shadow_mode {
            return fallback("shadow", "shadow-mode-keeps-routing-observational");
        }
        let estimate = match estimate {
            Some(estimate) => estimate,
            None => return fallback("degraded", "resource-estimate-unavailable"),
        };
        let profiles = match kitty.workload_routing_profiles(&request.repository) {
            Ok(profiles) => profiles,
            Err(_) => return fallback("degraded", "validated-workload-profiles-unavailable"),
        };
        let Some(profile_labels) = profiles.get(&estimate.workload_id) else {
            // Do not invent a family route for a job Kitty has not observed and
            // validated yet. Let GitHub choose a conventional compatible runner.
            return fallback("learning", "workload-family-not-yet-learned");
        };

        // Adaptive host accounting is currently local-Linux only. Never return
        // a Kitty route for a family whose Logic Containers rule resolves to a
        // VM or unreadable configuration, because that budget cannot be proven.
        let rules = match crate::logic_containers::load_rules(
            &self.state_dir.join("logic-containers.json"),
        ) {
            Ok(rules) => rules,
            Err(_) => return fallback("degraded", "logic-container-backend-could-not-be-verified"),
        };
        if matches!(
            crate::logic_containers::resolve(&rules, profile_labels),
            Some((crate::logic_containers::Backend::Vm { .. }, _))
        ) {
            return fallback(
                "unsupported",
                "vm-resource-probing-is-not-supported-for-dispatch",
            );
        }
        let route_id = match new_kitty_dispatch_route_id() {
            Some(route_id) => route_id,
            None => return fallback("degraded", "secure-route-id-generation-failed"),
        };
        let runs_on = vec![
            format!("gitrun-job-{}", estimate.workload_id),
            format!("gitrun-route-{route_id}"),
        ];
        kitty_dispatch_plan_result(
            "kitty",
            true,
            "online",
            "validated-profile-and-exclusive-route-issued",
            &runs_on,
        )
    }

    fn status(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        Ok(success(format!(
            "GitRun API STATUS\nrunner={}\nrepository={}\nworkflow={}\njob={}\n",
            self.caller.runner, request.repository, request.workflow, request.job
        )))
    }

    fn vault(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        if self.config.vault_dir.trim().is_empty() {
            return Err(failed("GitVault is disabled"));
        }

        let bridge = crate::gsr_bridge::VaultToGsrBridge::new(&self.config.state_dir);
        let mut vault = Vault::open_with_sink(&self.config.vault_dir, Box::new(bridge))
            .map_err(|error| failed(error.to_string()))?;
        let groups = self.config.vault_groups_for_repo(&request.repository);

        match request.operation {
            GitRunOperation::Read => {
                let name = arg(request, "name")?;
                let value = vault
                    .get_effective_for_repo(name, &request.repository, &groups)
                    .map_err(|error| failed(error.to_string()))?;

                let mask_data = escape_workflow_command_data(&value);
                Ok(success(format!("::add-mask::{mask_data}\n{value}\n")))
            }
            GitRunOperation::Write => {
                let name = arg(request, "name")?;
                let value = arg(request, "value")?;
                if value.len() > MAX_VAULT_WRITE_BYTES {
                    return Err(failed("GitVaultRun write exceeds the 1 MiB limit"));
                }
                vault
                    .set_scoped(name, value, Scope::Repo(request.repository.clone()))
                    .map_err(|error| failed(error.to_string()))?;
                Ok(success("GitVaultRun write: PASS\n".into()))
            }
            GitRunOperation::Exists => {
                let name = arg(request, "name")?;
                let exists = vault
                    .effective_contains_for_repo(name, &request.repository, &groups)
                    .map_err(|error| failed(error.to_string()))?;
                Ok(success(format!("{exists}\n")))
            }
            GitRunOperation::Delete => {
                let name = arg(request, "name")?;
                vault
                    .delete_scoped(name, &Scope::Repo(request.repository.clone()))
                    .map_err(|error| failed(error.to_string()))?;
                Ok(success("GitVaultRun delete: PASS\n".into()))
            }
            GitRunOperation::List => {
                let entries = vault.list_in_scope(&Scope::Repo(request.repository.clone()));
                let body = entries
                    .into_iter()
                    .map(|(name, updated)| format!("{name}\t{updated}\n"))
                    .collect::<String>();
                Ok(success(body))
            }
            _ => Err(failed("unsupported GitVaultRun operation")),
        }
    }

    fn save(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        // Pin all Docker effects to the immutable ID authenticated from the peer's
        // cgroup. A mutable container name may be rebound after request auth.
        let runner = &self.caller_container_id;

        match request.operation {
            GitRunOperation::File => {
                let source = arg(request, "path")?;
                ensure_safe_runner_path(source)?;

                let name = Path::new(source)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| failed("GitSaveRun source has no safe file name"))?;
                ensure_safe_file_name(name)?;

                let policy = GitRunSettings::load_or_default(GitRunSettings::path_for_state_dir(
                    &self.state_dir,
                ))
                .map_err(|e| failed(e.to_string()))?
                .effective_for_repository(&request.repository)
                .storage;

                // Validate the initial size for a fast rejection, then enforce the
                // same cap while copying from an already-open source descriptor.
                // The helper rejects symlink components and publishes atomically,
                // so a workflow cannot swap the source between stat and copy.
                let size = docker_file_size(runner, source)?;
                if size > policy.max_file_size_bytes {
                    return Err(failed("GitSaveRun file exceeds repository storage limit"));
                }

                let destination = format!("/var/lib/gitrun/shared/files/{name}");
                let directory = docker_secure_file_operation_as_runner(
                    runner,
                    &["mkdir", "/var/lib/gitrun/shared/files"],
                )?;
                if !directory.status.success() {
                    return Err(failed(format!(
                        "GitSaveRun could not safely prepare shared storage: {}",
                        String::from_utf8_lossy(&directory.stderr).trim()
                    )));
                }
                let limit = policy.max_file_size_bytes.to_string();
                let copied = docker_secure_file_operation_as_runner(
                    runner,
                    &["copy", source, destination.as_str(), limit.as_str()],
                )?;
                if !copied.status.success() {
                    return Err(failed(format!(
                        "GitSaveRun could not safely copy the file: {}",
                        String::from_utf8_lossy(&copied.stderr).trim()
                    )));
                }

                Ok(success(format!(
                    "GitSaveRun file: saved /var/lib/gitrun/shared/files/{name}\n"
                )))
            }
            GitRunOperation::Logs => {
                let name = arg(request, "namefile")?;
                ensure_safe_file_name(name)?;

                let mut logs =
                    docker::container_logs(runner).map_err(|error| failed(error.to_string()))?;
                logs = redact_vault_values(&self.config, &request.repository, &logs);

                let policy = GitRunSettings::load_or_default(GitRunSettings::path_for_state_dir(
                    &self.state_dir,
                ))
                .map_err(|e| failed(e.to_string()))?
                .effective_for_repository(&request.repository)
                .storage;

                if logs.len() as u64 > policy.max_file_size_bytes {
                    return Err(failed("GitSaveRun logs exceed repository storage limit"));
                }

                let directory = docker_secure_file_operation_as_runner(
                    runner,
                    &["mkdir", "/var/lib/gitrun/shared/logs"],
                )?;
                if !directory.status.success() {
                    return Err(failed("unable to safely prepare GitRun log storage"));
                }
                let destination = format!("/var/lib/gitrun/shared/logs/{name}");
                let output = docker_secure_file_operation_with_stdin_as_runner(
                    runner,
                    &["write", destination.as_str()],
                    logs.as_bytes(),
                )?;
                if !output.status.success() {
                    return Err(failed(
                        "unable to safely save GitRun logs into shared storage",
                    ));
                }

                Ok(success(format!(
                    "GitSaveRun logs: saved /var/lib/gitrun/shared/logs/{name}\n"
                )))
            }
            _ => Err(failed("unsupported GitSaveRun operation")),
        }
    }

    fn register(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let name = arg(request, "name")?;
        let entry = arg(request, "entry")?;
        let permanent = parse_boolean_argument(arg(request, "permanent")?)
            .ok_or_else(|| failed("permanent must be true or false"))?;

        ensure_safe_logical_name(name)?;
        ensure_safe_runner_path(entry)?;

        let path = self.state_dir.join("register-registry.json");
        let mut registry = RegisterRegistry::load(&path)?;
        registry.upsert(RegisteredPath {
            name: name.to_owned(),
            entry: entry.to_owned(),
            repository: request.repository.clone(),
            workflow: request.workflow.clone(),
            run_id: request_run_id(request),
            permanent,
            updated_at: now(),
        });
        registry.save(&path)?;

        Ok(success(format!(
            "GitRegisterRun: registered {name} -> {entry} ({})\n",
            if permanent { "permanent" } else { "workflow" }
        )))
    }

    fn install_stream(
        &self,
        request: &AuthorizedOperation,
        sink: &mut dyn FnMut(ExecutionEvent),
    ) -> Result<i32, ExecutionError> {
        let package = arg(request, "package")?;
        ensure_safe_package(package)?;
        let spec = match request.arguments.get("version") {
            Some(version) => {
                ensure_safe_package(version)?;
                format!("{package}={version}")
            }
            None => package.to_owned(),
        };

        docker::exec_container_stream(
            &self.caller_container_id,
            &[
                "sh",
                "-c",
                "if command -v apt-get >/dev/null 2>&1; then apt-get install -y --no-install-recommends \"$1\"; elif command -v dnf >/dev/null 2>&1; then dnf install -y \"$1\"; elif command -v pacman >/dev/null 2>&1; then pacman -S --noconfirm \"$1\"; else echo 'no supported package manager' >&2; exit 127; fi",
                "gitrun-install",
                spec.as_str(),
            ],
            |is_stderr, bytes| {
                let value = String::from_utf8_lossy(bytes).into_owned();
                if is_stderr {
                    sink(ExecutionEvent::Stderr(value));
                } else {
                    sink(ExecutionEvent::Stdout(value));
                }
            },
        )
        .map_err(|error| failed(error.to_string()))
    }

    fn install(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let package = arg(request, "package")?;
        ensure_safe_package(package)?;

        let spec = match request.arguments.get("version") {
            Some(version) => {
                ensure_safe_package(version)?;
                format!("{package}={version}")
            }
            None => package.to_owned(),
        };

        let args = [
            "sh",
            "-c",
            "if command -v apt-get >/dev/null 2>&1; then apt-get install -y --no-install-recommends \"$1\"; elif command -v dnf >/dev/null 2>&1; then dnf install -y \"$1\"; elif command -v pacman >/dev/null 2>&1; then pacman -S --noconfirm \"$1\"; else echo 'no supported package manager' >&2; exit 127; fi",
            "gitrun-install",
            spec.as_str(),
        ];
        // Package installation is a separately authorized privileged
        // operation: it intentionally uses the image's root user. All ordinary
        // filesystem/execute helpers above and below this method use runner.
        docker_exec_checked_as_container_root(&self.caller_container_id, &args)
    }

    fn read(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let path = arg(request, "path")?;
        ensure_safe_runner_path(path)?;

        let output =
            docker_secure_file_operation_as_runner(&self.caller_container_id, &["read", path])?;
        command_output(output)
    }

    fn write(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let path = arg(request, "path")?;
        let value = arg(request, "value")?;
        ensure_safe_runner_path(path)?;

        let output = docker_secure_file_operation_with_stdin_as_runner(
            &self.caller_container_id,
            &["write", path],
            value.as_bytes(),
        )?;
        command_output(output)
    }

    fn verify(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let path = arg(request, "path")?;
        ensure_safe_runner_path(path)?;

        let output =
            docker_secure_file_operation_as_runner(&self.caller_container_id, &["sha256", path])?;
        command_output(output)
    }

    fn dock(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        match request.operation {
            GitRunOperation::Connect => self.dock_connect(request),
            GitRunOperation::Disconnect => self.dock_disconnect(request),
            GitRunOperation::Read => self.dock_read(request),
            GitRunOperation::Write => self.dock_write(request),
            GitRunOperation::Execute => self.dock_execute(request),
            GitRunOperation::Melt => self.dock_melt(request),
            GitRunOperation::Mount => self.dock_mount(request),
            GitRunOperation::Unmount => self.dock_unmount(request),
            _ => Err(failed("unsupported GitDockRun operation")),
        }
    }

    fn dock_connect(
        &self,
        request: &AuthorizedOperation,
    ) -> Result<ExecutionResult, ExecutionError> {
        let run_id = request_run_id(request);
        if run_id == 0 {
            return Err(failed("GitDockRun --connect requires GITHUB_RUN_ID"));
        }
        let job = arg(request, "job")?;
        if job != request.job {
            return Err(failed(
                "GitDockRun --connect may only target the caller's current workflow job",
            ));
        }

        let job_info = self
            .github
            .find_workflow_job(&request.repository, run_id, job)
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| failed(format!("workflow job {job} was not found")))?;

        let runner_name = job_info
            .runner_name
            .ok_or_else(|| failed(format!("workflow job {job} has no runner identity")))?;

        // The API handler validated that this workflow job belonged to the
        // authenticated caller. GitHub can change assignment between that
        // check and this second lookup, so never bind a different runner.
        if runner_name != self.caller.runner {
            return Err(failed(
                "GitDockRun --connect workflow runner no longer matches the authenticated caller",
            ));
        }

        // Capture the immutable container ID used for the actual execution.
        // The binding is invalid if this name later resolves to another ID.
        let container_id = docker::container_id(&runner_name)
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| {
                failed(format!(
                    "runner container {runner_name} disappeared during connect"
                ))
            })?;
        validate_dock_connect_caller_identity(
            &self.caller.runner,
            &self.caller_container_id,
            &runner_name,
            &container_id,
        )
        .map_err(failed)?;

        // Lifecycle metadata must be read through the same immutable identity
        // captured above. Looking it up again by name permits a replacement
        // container to influence this binding's dynamic/permanent classification.
        let dynamic = resolve_dock_connect_dynamic(
            &runner_name,
            &container_id,
            |id| docker::container_is_permanent(id).map_err(|error| error.to_string()),
            |id| docker::container_status(id).map_err(|error| error.to_string()),
        )
        .map_err(failed)?;

        let completed = workflow_job_is_completed(&job_info.status, job_info.conclusion.as_deref());

        // Serialize load/modify/save against other connects, disconnects,
        // melts, and scheduler reconciliation. Atomic rename alone prevents
        // torn reads, not lost updates between concurrent writers.
        #[cfg(target_os = "linux")]
        let _lifecycle_guard = acquire_gitdock_lifecycle_guard(&self.state_dir).map_err(failed)?;

        // Load and prepare the registry before changing container lifecycle;
        // a registry read/decode failure must not leave a runner disabled with
        // no durable binding describing why.
        let mut registry =
            DockRegistry::load(&self.state_dir).map_err(|error| failed(error.to_string()))?;

        // A persistent runner can be assigned a later workflow job while the
        // DockRegistry still contains its previous job binding. If that old job
        // left host mounts attached in this same container namespace, replacing
        // the binding without cleaning those leases would expose them to the new
        // job. Reconcile the prior job before upserting the new binding.
        if let Some(previous) = registry.binding_for_container(&runner_name).cloned() {
            if previous.container_id == container_id {
                if previous.repository != request.repository {
                    return Err(failed(
                        "GitDockRun refuses to reuse a container bound to a different repository",
                    ));
                }
                if connect_reuses_container_from_prior_binding(
                    &previous,
                    &request.repository,
                    run_id,
                    job,
                    &self.caller.runner,
                    &runner_name,
                    &container_id,
                ) {
                    if previous.dock_only {
                        return Err(failed(
                            "GitDockRun cannot reconnect a container still marked as dock-only",
                        ));
                    }
                    let previous_job = self
                        .github
                        .find_workflow_job(
                            &previous.repository,
                            previous.run_id,
                            &previous.job,
                        )
                        .map_err(|error| failed(error.to_string()))?
                        .ok_or_else(|| {
                            failed(
                                "GitDockRun previous container binding has no verifiable GitHub job",
                            )
                        })?;
                    if !workflow_job_is_completed(
                        &previous_job.status,
                        previous_job.conclusion.as_deref(),
                    ) {
                        return Err(failed(
                            "GitDockRun refuses to reuse a runner before its previous bound job is completed",
                        ));
                    }

                    #[cfg(target_os = "linux")]
                    {
                        let mut mount_registry =
                            MountRegistry::load(&self.state_dir).map_err(failed)?;
                        cleanup_owned_mounts_for_disconnect(
                            &mut mount_registry,
                            &self.state_dir,
                            MountCleanupOwner {
                                repository: &previous.repository,
                                run_id: previous.run_id,
                                job: &previous.job,
                                container_id: &previous.container_id,
                                requester_runner: &previous.requester_runner,
                            },
                            cleanup_mount_record,
                        )
                        .map_err(|error| {
                            failed(format!(
                                "cannot reuse runner until its previous GitDockRun mounts are cleaned: {error}"
                            ))
                        })?;
                    }
                }
            }
        }

        registry.upsert(DockBinding {
            repository: request.repository.clone(),
            run_id,
            job: job.to_owned(),
            container: runner_name.clone(),
            container_id: container_id.clone(),
            dynamic,
            dock_only: dynamic && completed,
            requester_runner: self.caller.runner.clone(),
            connected_at: now(),
        });
        if dynamic && completed {
            // Use the immutable Docker ID captured above, not a name that
            // could be rebound between lookup and mutation.
            make_dock_only(&container_id, &self.state_dir)
                .map_err(|error| failed(error.to_string()))?;
        }
        if let Err(error) = registry.save(&self.state_dir) {
            if dynamic && completed {
                if let Err(rollback_error) = restore_runner(&container_id) {
                    return Err(failed(format!(
                        "persist GitDockRun binding failed: {error}; additionally failed to restore runner after rollback: {rollback_error}"
                    )));
                }
            }
            return Err(failed(format!("persist GitDockRun binding: {error}")));
        }

        Ok(success(format!(
            "GitDockRun CONNECT: PASS\njob={job}\ncontainer={runner_name}\nid={container_id}\n"
        )))
    }

    fn dock_disconnect(
        &self,
        request: &AuthorizedOperation,
    ) -> Result<ExecutionResult, ExecutionError> {
        let run_id = request_run_id(request);
        if run_id == 0 {
            return Err(failed("GitDockRun --disconnect requires GITHUB_RUN_ID"));
        }
        let job = arg(request, "job")?;
        if job != request.job {
            return Err(failed(
                "GitDockRun --disconnect may only target the caller's current workflow job",
            ));
        }

        // Serialize disconnect with mount/unmount across both threads and
        // service processes. Keep these locks through binding removal so a
        // concurrent MOUNT cannot create a fresh lease after cleanup.
        #[cfg(target_os = "linux")]
        let lifecycle_guard = acquire_gitdock_lifecycle_guard(&self.state_dir).map_err(failed)?;

        let mut registry =
            DockRegistry::load(&self.state_dir).map_err(|error| failed(error.to_string()))?;
        let binding = registry
            .binding(&request.repository, run_id, job)
            .cloned()
            .ok_or_else(|| failed(format!("no GitDockRun binding exists for job {job}")))?;
        validate_dock_binding_owner(&binding, &self.caller.runner).map_err(failed)?;
        let current_container_id = docker::container_id(&binding.container)
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| failed("GitDockRun bound container no longer exists"))?;
        if binding.container_id.is_empty() || current_container_id != binding.container_id {
            return Err(failed(
                "GitDockRun binding is stale: container name now resolves to a different ID",
            ));
        }

        // Disconnect revokes this workflow's mount capability too. Remove
        // those mounts before dropping the binding, and retain the binding if
        // cleanup cannot be verified so callers never receive a false success.
        #[cfg(target_os = "linux")]
        {
            let mut mount_registry = MountRegistry::load(&self.state_dir).map_err(failed)?;
            cleanup_owned_mounts_for_disconnect(
                &mut mount_registry,
                &self.state_dir,
                MountCleanupOwner {
                    repository: &request.repository,
                    run_id,
                    job,
                    container_id: &binding.container_id,
                    requester_runner: &self.caller.runner,
                },
                cleanup_mount_record,
            )
            .map_err(failed)?;
        }

        let binding = registry
            .remove(&request.repository, run_id, job)
            .ok_or_else(|| failed(format!("no GitDockRun binding exists for job {job}")))?;
        registry
            .save(&self.state_dir)
            .map_err(|error| failed(error.to_string()))?;

        #[cfg(target_os = "linux")]
        drop(lifecycle_guard);

        if !binding.dynamic && docker_container_has_marker(&binding.container_id)? {
            restore_runner(&binding.container_id)?;
        }

        Ok(success(format!(
            "GitDockRun DISCONNECT: PASS\njob={job}\ncontainer={}\n",
            binding.container
        )))
    }

    fn dock_read(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let container = self.require_dock_binding(request)?;
        let path = arg(request, "path")?;
        ensure_safe_runner_path(path)?;

        let output = docker_secure_file_operation_as_runner(container.as_str(), &["read", path])?;
        command_output(output)
    }

    fn dock_write(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let container = self.require_dock_binding(request)?;
        let path = arg(request, "path")?;
        let value = arg(request, "value")?;
        ensure_safe_runner_path(path)?;

        let output = docker_secure_file_operation_with_stdin_as_runner(
            container.as_str(),
            &["write", path],
            value.as_bytes(),
        )?;
        command_output(output)
    }

    fn dock_execute_stream(
        &self,
        request: &AuthorizedOperation,
        sink: &mut dyn FnMut(ExecutionEvent),
    ) -> Result<i32, ExecutionError> {
        let container = self.require_dock_binding(request)?;
        let command = arg(request, "command")?;
        let argv = parse_simple_command(command).map_err(failed)?;
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();

        docker::exec_container_stream_as_runner(container.as_str(), &argv, |is_stderr, bytes| {
            let value = String::from_utf8_lossy(bytes).into_owned();
            if is_stderr {
                sink(ExecutionEvent::Stderr(value));
            } else {
                sink(ExecutionEvent::Stdout(value));
            }
        })
        .map_err(|error| failed(error.to_string()))
    }

    fn dock_execute(
        &self,
        request: &AuthorizedOperation,
    ) -> Result<ExecutionResult, ExecutionError> {
        let container = self.require_dock_binding(request)?;
        let command = arg(request, "command")?;
        let argv = parse_simple_command(command).map_err(failed)?;
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let output = docker::exec_container_as_runner(container.as_str(), &argv)
            .map_err(|error| failed(error.to_string()))?;
        command_output(output)
    }

    fn require_dock_binding_record(
        &self,
        request: &AuthorizedOperation,
    ) -> Result<DockBinding, ExecutionError> {
        let requested_container = request
            .resource
            .as_deref()
            .ok_or_else(|| failed("missing docked container"))?;
        let run_id = request_run_id(request);
        if run_id == 0 {
            return Err(failed("GitDockRun requires an authoritative workflow run"));
        }
        let registry =
            DockRegistry::load(&self.state_dir).map_err(|error| failed(error.to_string()))?;
        let binding = if requested_container == "runner" {
            registry.bindings.iter().find(|binding| {
                binding.repository == request.repository
                    && binding.run_id == run_id
                    && binding.job == request.job
                    && binding.requester_runner == self.caller.runner
                    && binding.container == self.caller.runner
                    && binding.container_id == self.caller_container_id
            })
        } else {
            registry.binding_for_authorized_container(
                &request.repository,
                run_id,
                &request.job,
                &self.caller.runner,
                requested_container,
            )
        }
        .ok_or_else(|| failed("GitDockRun resource is not bound to this runner/workflow/job"))?;

        if binding.container_id.is_empty() {
            return Err(failed(
                "GitDockRun binding has no immutable container ID; reconnect the dock",
            ));
        }

        let current_id = docker::container_id(&binding.container)
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| failed("GitDockRun bound container no longer exists"))?;
        if current_id != binding.container_id {
            return Err(failed(
                "GitDockRun binding is stale: container name now resolves to another container",
            ));
        }

        Ok(binding.clone())
    }

    fn require_dock_binding(
        &self,
        request: &AuthorizedOperation,
    ) -> Result<String, ExecutionError> {
        self.require_dock_binding_record(request)
            .map(|binding| binding.container_id)
    }

    #[cfg(target_os = "linux")]
    fn dock_mount(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let _lifecycle_guard = acquire_gitdock_lifecycle_guard(&self.state_dir).map_err(failed)?;
        let binding = self.require_dock_binding_record(request)?;
        let source = arg(request, "source")?;
        let target = arg(request, "target")?;
        let read_only = match arg(request, "read_only")? {
            "true" => true,
            "false" => false,
            _ => return Err(failed("read_only must be true or false")),
        };

        let settings_path = GitRunSettings::path_for_state_dir(&self.state_dir);
        // Hold the same cross-process policy lock used by GitRunSettings::save
        // from the first policy read until attach, read-only enforcement and
        // mount-ID verification are complete. A concurrent dashboard/CLI policy
        // replacement therefore cannot slip between the final authorization
        // check and the privileged namespace operation.
        let _policy_guard = gitrun_core::settings::SettingsPolicyLock::acquire(&settings_path)
            .map_err(|error| failed(format!("lock GitDockRun policy during MOUNT: {error}")))?;
        let settings = GitRunSettings::load_or_default(&settings_path)
            .map_err(|error| failed(error.to_string()))?;
        let resource = request
            .resource
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| failed("GitDockRun MOUNT has no logical container resource"))?;
        let canonical_source = authorize_live_mount_source(
            &settings,
            &request.repository,
            resource,
            Path::new(source),
            read_only,
        )
        .map_err(failed)?;
        let source_handle = open_mount_source(&canonical_source).map_err(failed)?;
        let source_metadata = source_handle
            .metadata()
            .map_err(|error| failed(format!("inspect mount source: {error}")))?;
        if !source_metadata.is_file() && !source_metadata.is_dir() {
            return Err(failed("only regular files and directories can be mounted"));
        }

        let pid = container_pid_for_mount(&binding.container_id).map_err(failed)?;
        validate_mount_target(pid, target, source_metadata.is_dir()).map_err(failed)?;
        let current_id = docker::container_id(&binding.container)
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| failed("GitDockRun target container disappeared"))?;
        if current_id != binding.container_id {
            return Err(failed(
                "GitDockRun target container identity changed before mount",
            ));
        }
        if mountinfo_by_target(pid, target).map_err(failed)?.is_some() {
            return Err(failed("mount target is already a mount point"));
        }

        let mut registry = MountRegistry::load(&self.state_dir).map_err(failed)?;
        let mut changed_stale = false;
        let existing = registry.mounts.clone();
        for record in existing
            .iter()
            .filter(|record| record.container_id == binding.container_id && record.target == target)
        {
            let Some(kernel_mount_id) = record.kernel_mount_id else {
                return Err(failed(format!(
                    "mount lease {} has no kernel mount ID; it is preserved for manual recovery rather than guessing from a path",
                    record.id
                )));
            };
            if mountinfo_by_id(pid, kernel_mount_id)
                .map_err(failed)?
                .is_some()
            {
                return Err(failed(
                    "a GitDockRun mount is already registered at this target",
                ));
            }
            registry.mounts.retain(|item| item.id != record.id);
            changed_stale = true;
        }
        if changed_stale {
            registry.save(&self.state_dir).map_err(failed)?;
        }

        let mount_id = new_mount_id().map_err(failed)?;
        let mut record = MountRecord {
            id: mount_id.clone(),
            repository: request.repository.clone(),
            run_id: request_run_id(request),
            job: request.job.clone(),
            requester_runner: self.caller.runner.clone(),
            container: binding.container.clone(),
            container_id: binding.container_id.clone(),
            source: canonical_source.to_string_lossy().into_owned(),
            target: target.to_owned(),
            read_only,
            source_device: Some(source_metadata.dev()),
            source_inode: Some(source_metadata.ino()),
            kernel_mount_id: None,
            state: "preparing".into(),
            created_at: now(),
        };
        registry.mounts.push(record.clone());
        // Persist intent before the privileged side effect. "preparing" means
        // the bind syscall has not been entered; after the durable transition
        // to "attaching", a crash may have happened on either side of the
        // kernel side effect and an absent pathname is no longer proof that no
        // mount exists.
        registry.save(&self.state_dir).map_err(failed)?;

        // Persist the conservative recovery boundary before the final policy
        // read. If the service crashes during the check below, retain the lease:
        // recovery must not guess whether the privileged worker was entered.
        // While the process is alive, however, a failed check can safely remove
        // this intent because the worker has not yet been invoked.
        record.state = "attaching".into();
        registry.mounts.retain(|item| item.id != mount_id);
        registry.mounts.push(record.clone());
        registry.save(&self.state_dir).map_err(|error| {
            failed(format!(
                "cannot persist mount attaching state; refusing bind: {error}"
            ))
        })?;

        // Settings can change after GSR has authorized the request and while
        // the target/source are being inspected. Reload the effective policy
        // after the durable state write, as close as possible to the namespace
        // operation; a previously pinned source cannot bypass a policy change.
        let current_settings = match GitRunSettings::load_or_default(
            GitRunSettings::path_for_state_dir(&self.state_dir),
        ) {
            Ok(settings) => settings,
            Err(error) => {
                registry.mounts.retain(|item| item.id != mount_id);
                if let Err(save_error) = registry.save(&self.state_dir) {
                    return Err(failed(format!(
                        "policy reload failed: {error}; pre-attach intent cleanup failed: {save_error}"
                    )));
                }
                return Err(failed(format!(
                    "reload GitDockRun mount policy before attach: {error}"
                )));
            }
        };
        let final_authorization = authorize_live_mount_source(
            &current_settings,
            &request.repository,
            resource,
            Path::new(&record.source),
            read_only,
        )
        .and_then(|current_source| {
            if current_source == canonical_source {
                Ok(())
            } else {
                Err("mount source canonical identity changed after initial authorization".into())
            }
        });
        if let Err(policy_error) = final_authorization {
            registry.mounts.retain(|item| item.id != mount_id);
            registry.save(&self.state_dir).map_err(|save_error| {
                failed(format!(
                    "mount was blocked because policy changed before attach, but its intent lease could not be removed: {policy_error}; {save_error}"
                ))
            })?;
            return Err(failed(format!(
                "mount was blocked because its effective policy changed before attach: {policy_error}"
            )));
        }

        if let Err(error) = execute_mount_namespace(
            &binding.container_id,
            pid,
            Some(source_handle.as_raw_fd()),
            Some(&canonical_source),
            target,
            MountAction::Bind { read_only },
        ) {
            // A worker can be interrupted after the kernel side effect but
            // before returning success. Without an identity returned by that
            // worker, a pathname lookup cannot prove ownership. Keep the
            // intent lease and refuse to unmount whatever happens to occupy
            // the target; recovery can inspect it without risking another mount.
            record.state = "cleanup-required".into();
            registry.mounts.retain(|item| item.id != mount_id);
            registry.mounts.push(record);
            let persistence = registry.save(&self.state_dir);
            return Err(failed(format!(
                "bind did not report successful completion; mount lease {mount_id} retained without unsafe pathname rollback: {error}; registry: {}",
                persistence.err().map(|save_error| save_error.to_string()).unwrap_or_else(|| "saved".into())
            )));
        }

        // Capture and durably record the kernel mount ID immediately after
        // bind, before trying to change mount attributes. Recovery must never
        // unmount an unowned path merely because a lease lacks an ID.
        let mount_info = match mountinfo_by_target(pid, target) {
            Ok(Some(info)) => info,
            Ok(None) => {
                record.state = "cleanup-required".into();
                registry.mounts.retain(|item| item.id != mount_id);
                registry.mounts.push(record);
                let _ = registry.save(&self.state_dir);
                return Err(failed(format!(
                    "bind completed but no kernel mount ID could be captured; mount lease {mount_id} retained for safe recovery"
                )));
            }
            Err(error) => {
                record.state = "cleanup-required".into();
                registry.mounts.retain(|item| item.id != mount_id);
                registry.mounts.push(record);
                let _ = registry.save(&self.state_dir);
                return Err(failed(format!(
                    "bind completed but mount identity inspection failed; mount lease {mount_id} retained: {error}"
                )));
            }
        };

        // mountinfo at the original pathname is not enough if a workflow
        // replaced the target while the helper was attaching by pinned FD.
        // Confirm that the visible target resolves to the original source
        // device/inode before adopting its kernel mount ID into the lease.
        let visible_target = PathBuf::from(format!("/proc/{pid}/root{target}"));
        let visible_metadata = match fs::metadata(&visible_target) {
            Ok(metadata) => metadata,
            Err(error) => {
                record.state = "cleanup-required".into();
                registry.mounts.retain(|item| item.id != mount_id);
                registry.mounts.push(record);
                let persistence = registry.save(&self.state_dir);
                return Err(failed(format!(
                    "bind completed but its visible target could not be verified; lease {mount_id} retained: {error}; registry: {}",
                    persistence.err().map(|save_error| save_error.to_string()).unwrap_or_else(|| "saved".into())
                )));
            }
        };
        if visible_metadata.dev() != source_metadata.dev()
            || visible_metadata.ino() != source_metadata.ino()
        {
            record.state = "cleanup-required".into();
            registry.mounts.retain(|item| item.id != mount_id);
            registry.mounts.push(record);
            let persistence = registry.save(&self.state_dir);
            return Err(failed(format!(
                "bind completed but the visible target no longer matches the pinned source identity; lease {mount_id} retained for recovery; registry: {}",
                persistence.err().map(|save_error| save_error.to_string()).unwrap_or_else(|| "saved".into())
            )));
        }

        record.kernel_mount_id = Some(mount_info.id);
        record.state = if read_only {
            "read-only-pending"
        } else {
            "mounted"
        }
        .into();
        registry.mounts.retain(|item| item.id != mount_id);
        registry.mounts.push(record.clone());
        if let Err(persist_error) = registry.save(&self.state_dir) {
            if execute_mount_namespace(
                &binding.container_id,
                pid,
                None,
                None,
                target,
                MountAction::Unmount {
                    expected_mount_id: mount_info.id,
                },
            )
            .is_ok()
            {
                registry.mounts.retain(|item| item.id != mount_id);
                let _ = registry.save(&self.state_dir);
                return Err(failed(format!(
                    "mount intent could not be durably updated and the mount was rolled back: {persist_error}"
                )));
            }
            record.state = "cleanup-required".into();
            registry.mounts.retain(|item| item.id != mount_id);
            registry.mounts.push(record);
            let recovery = registry.save(&self.state_dir);
            return Err(failed(format!(
                "mount succeeded but its kernel ID could not be durably saved; cleanup lease {mount_id} retained: {persist_error}; recovery save: {}",
                recovery.err().map(|error| error.to_string()).unwrap_or_else(|| "ok".into())
            )));
        }

        if read_only {
            if let Err(error) = execute_mount_namespace(
                &binding.container_id,
                pid,
                None,
                None,
                target,
                MountAction::RemountReadOnly {
                    expected_mount_id: mount_info.id,
                },
            ) {
                if let Err(rollback_error) = execute_mount_namespace(
                    &binding.container_id,
                    pid,
                    None,
                    None,
                    target,
                    MountAction::Unmount {
                        expected_mount_id: mount_info.id,
                    },
                ) {
                    record.state = "cleanup-required".into();
                    registry.mounts.retain(|item| item.id != mount_id);
                    registry.mounts.push(record);
                    let _ = registry.save(&self.state_dir);
                    return Err(failed(format!(
                        "read-only enforcement failed and rollback failed; mount {mount_id} remains registered for cleanup: {error}; rollback: {rollback_error}"
                    )));
                }
                registry.mounts.retain(|item| item.id != mount_id);
                registry.save(&self.state_dir).map_err(|save_error| {
                    failed(format!(
                        "read-only enforcement failed; mount was removed but lease cleanup failed: {error}; {save_error}"
                    ))
                })?;
                return Err(failed(format!(
                    "could not enforce read-only mount; mount was safely rolled back: {error}"
                )));
            }
            record.state = "mounted".into();
            registry.mounts.retain(|item| item.id != mount_id);
            registry.mounts.push(record.clone());
            registry.save(&self.state_dir).map_err(|error| {
                failed(format!(
                    "read-only mount is active but registry state update failed; lease {mount_id} remains in read-only-pending state for recovery: {error}"
                ))
            })?;
        }

        let verified_mount = match mountinfo_by_id(pid, mount_info.id) {
            Ok(Some(info)) if info.target == target => info,
            Ok(_) => {
                record.state = "cleanup-required".into();
                registry.mounts.retain(|item| item.id != mount_id);
                registry.mounts.push(record);
                let _ = registry.save(&self.state_dir);
                return Err(failed(format!(
                    "mount {mount_id} changed identity or target after attachment; cleanup lease retained"
                )));
            }
            Err(error) => {
                record.state = "cleanup-required".into();
                registry.mounts.retain(|item| item.id != mount_id);
                registry.mounts.push(record);
                let _ = registry.save(&self.state_dir);
                return Err(failed(format!(
                    "could not verify mount {mount_id} after attachment; cleanup lease retained: {error}"
                )));
            }
        };
        record.kernel_mount_id = Some(verified_mount.id);
        record.state = "mounted".into();
        registry.mounts.retain(|item| item.id != mount_id);
        registry.mounts.push(record);
        registry.save(&self.state_dir).map_err(|error| {
            failed(format!(
                "mount succeeded but registry durability could not be confirmed; mount {mount_id} remains registered for recovery: {error}"
            ))
        })?;

        Ok(success(format!(
            "GitDockRun MOUNT: PASS\nmount_id={mount_id}\ncontainer={}\nsource={}\ntarget={}\nread_only={}\n",
            binding.container,
            canonical_source.display(),
            target,
            read_only
        )))
    }

    #[cfg(target_os = "linux")]
    fn dock_unmount(
        &self,
        request: &AuthorizedOperation,
    ) -> Result<ExecutionResult, ExecutionError> {
        let _guard = GITDOCK_MOUNT_LOCK
            .lock()
            .map_err(|_| failed("GitDockRun mount registry lock is poisoned"))?;
        let _registry_lock = MountRegistryLock::acquire(&self.state_dir).map_err(failed)?;
        let binding = self.require_dock_binding_record(request)?;
        let mount_id = arg(request, "mount_id")?;
        let mut registry = MountRegistry::load(&self.state_dir).map_err(failed)?;
        let Some(index) = registry.mounts.iter().position(|record| {
            record.id == mount_id
                && record.repository == request.repository
                && record.run_id == request_run_id(request)
                && record.job == request.job
                && record.requester_runner == self.caller.runner
                && record.container_id == binding.container_id
        }) else {
            return Err(failed(
                "mount ID is not registered to this workflow and container",
            ));
        };
        let record = registry.mounts[index].clone();

        let current_id = docker::container_id(&binding.container)
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| failed("GitDockRun target container disappeared"))?;
        if current_id != record.container_id {
            return Err(failed(
                "GitDockRun target container identity changed before unmount",
            ));
        }
        cleanup_mount_record(&record).map_err(failed)?;

        registry.mounts.retain(|item| item.id != mount_id);
        registry.save(&self.state_dir).map_err(failed)?;
        Ok(success(format!(
            "GitDockRun UNMOUNT: PASS\nmount_id={mount_id}\ntarget={}\n",
            record.target
        )))
    }

    #[cfg(not(target_os = "linux"))]
    fn dock_mount(
        &self,
        _request: &AuthorizedOperation,
    ) -> Result<ExecutionResult, ExecutionError> {
        Err(failed(
            "dynamic GitDockRun mounts are currently supported only on Linux local Docker hosts",
        ))
    }

    #[cfg(not(target_os = "linux"))]
    fn dock_unmount(
        &self,
        _request: &AuthorizedOperation,
    ) -> Result<ExecutionResult, ExecutionError> {
        Err(failed(
            "dynamic GitDockRun mounts are currently supported only on Linux local Docker hosts",
        ))
    }

    fn dock_melt(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        // MELT removes a bound container and rewrites the binding registry.
        // Serialize it with concurrent connect/disconnect and mount lifecycle
        // operations so no stale snapshot can overwrite a newer registry.
        #[cfg(target_os = "linux")]
        let _lifecycle_guard = acquire_gitdock_lifecycle_guard(&self.state_dir).map_err(failed)?;

        let run_id = request_run_id(request);
        if run_id == 0 {
            return Err(failed(
                "GitDockRun --melt requires an authoritative workflow run",
            ));
        }
        let source_name = request
            .resource
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| failed("GitDockRun --melt has no source container resource"))?;
        let mut registry =
            DockRegistry::load(&self.state_dir).map_err(|error| failed(error.to_string()))?;
        let source_binding = find_dock_melt_source_binding(
            &registry,
            &request.repository,
            run_id,
            &request.job,
            source_name,
        )
        .cloned()
        .ok_or_else(|| {
            failed("GitDockRun --melt source must be bound to a different job in this workflow run")
        })?;
        let source = source_binding.container_id.clone();
        if source.is_empty() {
            return Err(failed(
                "GitDockRun --melt source binding has no immutable container ID",
            ));
        }
        let current_source_id = docker::container_id(&source_binding.container)
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| failed("GitDockRun --melt source container no longer exists"))?;
        if current_source_id != source {
            return Err(failed(
                "GitDockRun --melt source binding is stale: container identity changed",
            ));
        }
        let source_job = self
            .github
            .find_workflow_job(
                &request.repository,
                source_binding.run_id,
                &source_binding.job,
            )
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| failed("GitDockRun --melt source workflow job could not be verified"))?;
        if !workflow_job_is_completed(&source_job.status, source_job.conclusion.as_deref()) {
            return Err(failed(
                "GitDockRun --melt source workflow job must be completed before transfer",
            ));
        }

        let target_name = match request.arguments.get("target").map(String::as_str) {
            None | Some("runner") => self.caller.runner.as_str(),
            Some(value) => value,
        };

        let target_id = docker::container_id(target_name)
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| failed(format!("melt target container {target_name} was not found")))?;

        if source == target_id {
            return Err(failed("GitDockRun --melt source and target are identical"));
        }

        let source_repo = docker::container_repo_label_on(&docker::DockerHost::Local, &source)
            .map_err(|error| failed(error.to_string()))?;
        if source_repo.as_deref() != Some(request.repository.as_str()) {
            return Err(failed(
                "GitDockRun --melt source is not bound to this repository",
            ));
        }

        let status = docker::container_status(&source)
            .map_err(|error| failed(error.to_string()))?
            .unwrap_or_default();
        // A dock-only container is intentionally running with its runner
        // disabled and a marker installed. Ordinary running containers must
        // never be exported into another container. Only an exited container
        // or a running container carrying that marker is eligible for MELT.
        let dock_only_marker_present = if status == "running" {
            let marker = docker::exec_container(&source, &["test", "-f", DOCK_ONLY_MARKER])
                .map_err(|error| failed(error.to_string()))?;
            marker.status.success()
        } else {
            false
        };
        if !dock_melt_source_status_allowed(&status, dock_only_marker_present) {
            return Err(failed(
                "GitDockRun --melt requires an exited or dock-only source container",
            ));
        }

        // The source and target checks below use the same registry snapshot
        // while the lifecycle transaction lock is held.

        // The default target must still be the exact Docker container that
        // authenticated this API request. An explicit target is allowed only
        // when it has a binding in the same exact repository/run/job/requester
        // trust domain.
        if target_name == self.caller.runner {
            if target_id != self.caller_container_id {
                return Err(failed(
                    "GitDockRun --melt target is no longer the authenticated caller container",
                ));
            }
        } else {
            let target_binding = find_dock_melt_target_binding(
                &registry,
                &request.repository,
                run_id,
                target_name,
                &source,
            )
            .cloned()
            .ok_or_else(|| {
                failed("GitDockRun --melt target is not bound to this repository and workflow run")
            })?;
            validate_dock_melt_target_identity(&target_id, &target_binding.container_id)
                .map_err(failed)?;
            let target_job = self
                .github
                .find_workflow_job(
                    &request.repository,
                    target_binding.run_id,
                    &target_binding.job,
                )
                .map_err(|error| failed(error.to_string()))?
                .ok_or_else(|| {
                    failed("GitDockRun --melt target workflow job could not be verified")
                })?;
            if !workflow_job_is_active_on_runner(
                target_job.runner_name.as_deref(),
                &target_job.status,
                target_job.conclusion.as_deref(),
                &target_binding.container,
            ) {
                return Err(failed(
                    "GitDockRun --melt explicit target must be the exact runner of an active workflow job",
                ));
            }
            if target_job.runner_name.as_deref() != Some(target_binding.requester_runner.as_str()) {
                return Err(failed(
                    "GitDockRun --melt target binding no longer matches its recorded requester runner",
                ));
            }
        }

        // Re-resolve the target immediately before the privileged operation.
        // This closes the name-reuse window between authorization and docker exec.
        let target_recheck = docker::container_id(target_name)
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| failed("GitDockRun --melt target disappeared during authorization"))?;
        if target_recheck != target_id {
            return Err(failed(
                "GitDockRun --melt target changed during authorization",
            ));
        }

        // MELT extracts archive members under the target container's root.
        // If a GitDockRun bind mount is still attached in that namespace, tar
        // can write matching archive paths through it into the approved host
        // source. Refuse this cross-operation path rather than letting MELT
        // implicitly inherit MOUNT's host-filesystem write capability.
        #[cfg(target_os = "linux")]
        let mut mount_registry = MountRegistry::load(&self.state_dir).map_err(failed)?;
        #[cfg(target_os = "linux")]
        validate_melt_target_has_no_mount_leases(&mount_registry, &target_id).map_err(failed)?;

        // The lease registry cannot see ordinary Docker bind mounts, named
        // volumes or tmpfs mounts attached outside the GitDockRun MOUNT API.
        // Refuse MELT if any writable Docker mount can be reached by a tar
        // member that is not covered by the shared protected-path exclusions.
        docker::validate_melt_target_mounts_on(&docker::DockerHost::Local, &target_id)
            .map_err(|error| failed(error.to_string()))?;

        // The source can be a completed dock-only runner that still has mount
        // leases in its namespace. Remove only leases owned by this exact old
        // binding before the source container is destroyed.
        #[cfg(target_os = "linux")]
        {
            cleanup_owned_mounts_for_disconnect(
                &mut mount_registry,
                &self.state_dir,
                MountCleanupOwner {
                    repository: &source_binding.repository,
                    run_id: source_binding.run_id,
                    job: &source_binding.job,
                    container_id: &source_binding.container_id,
                    requester_runner: &source_binding.requester_runner,
                },
                cleanup_mount_record,
            )
            .map_err(failed)?;
        }

        // Recheck eligibility after lease cleanup. If the dock-only source is
        // still running, stop the pinned object before export so its filesystem
        // cannot change between the marker check and the transfer.
        let final_source_status = docker::container_status(&source)
            .map_err(|error| failed(error.to_string()))?
            .unwrap_or_default();
        if final_source_status == "running" {
            let marker = docker::exec_container(&source, &["test", "-f", DOCK_ONLY_MARKER])
                .map_err(|error| failed(error.to_string()))?;
            if !marker.status.success() {
                return Err(failed(
                    "GitDockRun --melt source lost its dock-only marker before transfer",
                ));
            }
            docker::stop_container(&source).map_err(|error| failed(error.to_string()))?;
            let stopped_status = docker::container_status(&source)
                .map_err(|error| failed(error.to_string()))?
                .unwrap_or_default();
            if stopped_status != "exited" {
                return Err(failed(
                    "GitDockRun --melt source could not be stopped safely before transfer",
                ));
            }
        } else if final_source_status != "exited" {
            return Err(failed(
                "GitDockRun --melt source changed to an ineligible lifecycle state",
            ));
        }

        docker::melt_filesystem(&source, &target_id).map_err(|error| failed(error.to_string()))?;
        docker::remove_container(&source).map_err(|error| {
            failed(format!(
                "filesystem transfer completed, but source container removal failed; GitDockRun binding retained for cleanup: {error}"
            ))
        })?;

        registry.bindings.retain(|binding| {
            !(binding.repository == source_binding.repository
                && binding.run_id == source_binding.run_id
                && binding.job == source_binding.job
                && binding.requester_runner == source_binding.requester_runner
                && binding.container == source_binding.container
                && binding.container_id == source_binding.container_id)
        });
        registry.save(&self.state_dir).map_err(|error| {
            failed(format!(
                "filesystem transfer and source removal completed, but the GitDockRun binding cleanup could not be persisted: {error}"
            ))
        })?;

        Ok(success(format!(
            "GitDockRun MELT: PASS\nsource={source}\ntarget={target_name}\nid={target_id}\n"
        )))
    }
}

#[cfg(target_os = "linux")]
fn authorize_live_mount_source(
    settings: &GitRunSettings,
    repository: &str,
    resource: &str,
    source: &Path,
    read_only: bool,
) -> Result<PathBuf, String> {
    let effective = settings.effective_for_repository(repository);
    if !effective
        .api_policy
        .allows(GitRunApi::GitDockRun, GitRunOperation::Mount)
    {
        return Err("GitDockRun MOUNT is disabled by the current effective API policy".into());
    }
    if !effective.docker.allows_container(resource) {
        return Err(format!(
            "GitDockRun MOUNT resource {resource} is not currently allowlisted"
        ));
    }
    let logic = effective
        .docker
        .logic_container(resource)
        .ok_or_else(|| format!("GitDockRun MOUNT has no policy for logic container {resource}"))?;
    if !logic.mountable {
        return Err(format!(
            "GitDockRun MOUNT is disabled for logic container {resource}"
        ));
    }
    effective
        .docker
        .allowed_mounts
        .authorize_source(source, read_only)
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
enum MountAction {
    Bind { read_only: bool },
    RemountReadOnly { expected_mount_id: u32 },
    Unmount { expected_mount_id: u32 },
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
struct MountInfo {
    id: u32,
    target: String,
}

#[cfg(target_os = "linux")]
struct MountStagingPath {
    path: PathBuf,
    is_dir: bool,
}

#[cfg(target_os = "linux")]
impl Drop for MountStagingPath {
    fn drop(&mut self) {
        let result = if self.is_dir {
            fs::remove_dir(&self.path)
        } else {
            fs::remove_file(&self.path)
        };
        if let Err(error) = result {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "gitrun-api: failed to remove temporary mount staging path {}: {error}",
                    self.path.display()
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

#[cfg(target_os = "linux")]
fn new_mount_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|error| format!("generate mount identifier: {error}"))?;
    let mut encoded = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").map_err(|error| error.to_string())?;
    }
    Ok(format!("gm-{encoded}"))
}

#[cfg(target_os = "linux")]
fn open_path_handle(path: &Path, no_follow: bool) -> Result<fs::File, String> {
    use std::os::unix::ffi::OsStrExt;
    let value = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "filesystem path contains a NUL byte".to_owned())?;
    let mut flags = libc::O_PATH | libc::O_CLOEXEC;
    if no_follow {
        flags |= libc::O_NOFOLLOW;
    }
    let fd = unsafe { libc::open(value.as_ptr(), flags) };
    if fd < 0 {
        return Err(format!(
            "open filesystem path {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn open_mount_source(source: &Path) -> Result<fs::File, String> {
    let handle = open_path_handle(source, true)?;
    let metadata = handle.metadata().map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
        return Err(
            "mount source must be a regular file or directory, not a symlink or special file"
                .into(),
        );
    }
    let fd_path = PathBuf::from(format!("/proc/self/fd/{}", handle.as_raw_fd()));
    let opened_source = fs::canonicalize(&fd_path)
        .map_err(|error| format!("verify opened mount source: {error}"))?;
    if opened_source != source {
        return Err("mount source changed while it was being opened".into());
    }
    Ok(handle)
}

#[cfg(target_os = "linux")]
fn container_pid_for_mount(container_id: &str) -> Result<i32, String> {
    if container_id.len() != 64 || !container_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("mount target is not an immutable Docker container ID".into());
    }
    let output = Command::new("docker")
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .env("DOCKER_HOST", "unix:///var/run/docker.sock")
        .args(["inspect", "--format", "{{.State.Pid}}", container_id])
        .output()
        .map_err(|error| format!("inspect mount target container: {error}"))?;
    if !output.status.success() {
        return Err("Docker could not inspect the bound mount target".into());
    }
    let pid = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<i32>()
        .map_err(|_| "Docker returned an invalid container PID".to_owned())?;
    if pid <= 1 {
        return Err("mount target container is not running".into());
    }
    let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .map_err(|error| format!("verify mount target cgroup: {error}"))?;
    if !cgroup
        .split(|character: char| !character.is_ascii_hexdigit())
        .any(|part| part.eq_ignore_ascii_case(container_id))
    {
        return Err("mount target PID no longer belongs to the bound Docker container".into());
    }

    // umount2 requires a pathname, so an identity check on the init process
    // alone is insufficient: another process/thread in the container could
    // race the expected-mount-ID check. Inspect every visible thread's current
    // effective, permitted, inheritable, ambient and bounding capability sets.
    // Checking CapBnd alone is insufficient because dropping a capability from
    // that set does not revoke it from CapEff/CapPrm or necessarily from exec
    // inheritance. If any matching thread cannot be inspected, fail closed.
    if container_has_mount_admin_capability(container_id)? {
        return Err(
            "live GitDockRun mounts are refused because a process in the container holds or can acquire CAP_SYS_ADMIN"
                .into(),
        );
    }
    Ok(pid)
}

#[cfg(target_os = "linux")]
fn container_has_mount_admin_capability(container_id: &str) -> Result<bool, String> {
    let proc_entries = fs::read_dir("/proc")
        .map_err(|error| format!("enumerate processes for mount capability check: {error}"))?;
    for entry in proc_entries {
        let entry = entry.map_err(|error| format!("enumerate processes: {error}"))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let process_root = entry.path();
        let cgroup_path = process_root.join("cgroup");
        let cgroup = match fs::read_to_string(&cgroup_path) {
            Ok(cgroup) => cgroup,
            // Processes can exit while /proc is enumerated. If the directory
            // still exists but cannot be inspected, do not silently skip a
            // process whose cgroup identity cannot be established.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!("inspect cgroup for process {pid}: {error}"));
            }
        };
        let belongs_to_container = cgroup
            .split(|character: char| !character.is_ascii_hexdigit())
            .any(|part| part.eq_ignore_ascii_case(container_id));
        if !belongs_to_container {
            continue;
        }

        let tasks = match fs::read_dir(process_root.join("task")) {
            Ok(tasks) => tasks,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "enumerate threads for container process {pid}: {error}"
                ));
            }
        };
        for task in tasks {
            let task = task.map_err(|error| format!("enumerate container threads: {error}"))?;
            let tid = task.file_name().to_string_lossy().into_owned();
            let status_path = task.path().join("status");
            let status = match fs::read_to_string(&status_path) {
                Ok(status) => status,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!(
                        "inspect capability status for thread {tid}: {error}"
                    ));
                }
            };
            if status_has_mount_admin_capability(&status)
                .map_err(|error| format!("inspect capabilities for thread {tid}: {error}"))?
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn status_has_mount_admin_capability(status: &str) -> Result<bool, String> {
    const CAP_SYS_ADMIN: u64 = 1u64 << 21;
    const CAPABILITY_FIELDS: [&str; 5] = ["CapEff:", "CapPrm:", "CapInh:", "CapAmb:", "CapBnd:"];

    for field in CAPABILITY_FIELDS {
        let value = status
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .ok_or_else(|| format!("missing {field} capability mask"))?;
        let mask = u64::from_str_radix(value.trim(), 16)
            .map_err(|_| format!("invalid {field} capability mask"))?;
        if mask & CAP_SYS_ADMIN != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn validate_mount_target(pid: i32, target: &str, source_is_dir: bool) -> Result<(), String> {
    let path = Path::new(target);
    if !path.is_absolute()
        || target.ends_with('/')
        || target.contains("//")
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        return Err("mount target must be a clean absolute container path".into());
    }
    if !mount_target_is_allowed(target) {
        return Err("mount targets are limited to workspace and GitRun data directories".into());
    }

    let root_path = PathBuf::from(format!("/proc/{pid}/root"));
    let mut host_path = root_path.clone();
    let components: Vec<_> = path.components().collect();
    let normal_components: Vec<_> = components
        .iter()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(*name),
            _ => None,
        })
        .collect();
    if normal_components.is_empty() {
        return Err("mount target cannot be the container root".into());
    }

    let mut final_metadata = None;
    for (index, name) in normal_components.iter().enumerate() {
        host_path.push(name);
        let current = fs::symlink_metadata(&host_path)
            .map_err(|error| format!("mount target component does not exist: {error}"))?;
        if current.file_type().is_symlink() {
            return Err("mount target path must not contain symlinks".into());
        }
        if index + 1 != normal_components.len() && !current.is_dir() {
            return Err("mount target parent is not a directory".into());
        }
        final_metadata = Some(current);
    }
    let metadata =
        final_metadata.ok_or_else(|| "mount target cannot be the container root".to_owned())?;
    if source_is_dir != metadata.is_dir() {
        return Err("mount source and target must both be directories or both be files".into());
    }
    // Do not stack a GitRun mount over a target that is already a mountpoint.
    // Besides avoiding ambiguous mount-ID lookup, this makes a pre-existing
    // mount an explicit policy failure rather than relying on syscall ordering.
    if mountinfo_by_target(pid, target)?.is_some() {
        return Err("mount target is already a mount point".into());
    }
    if source_is_dir {
        let mut entries = fs::read_dir(&host_path)
            .map_err(|error| format!("inspect mount target directory: {error}"))?;
        if entries
            .next()
            .transpose()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err(
                "directory mount targets must be empty to avoid hiding existing files".into(),
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn decode_mountinfo_path(path: &str) -> String {
    path.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(target_os = "linux")]
fn mountinfo_entries(pid: i32) -> Result<Vec<MountInfo>, String> {
    let raw = fs::read_to_string(format!("/proc/{pid}/mountinfo"))
        .map_err(|error| format!("read target mount namespace: {error}"))?;
    let mut entries = Vec::new();
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        let Some(id) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let _parent = fields.next();
        let _device = fields.next();
        let _root = fields.next();
        let Some(target) = fields.next() else {
            continue;
        };
        entries.push(MountInfo {
            id,
            target: decode_mountinfo_path(target),
        });
    }
    Ok(entries)
}

#[cfg(target_os = "linux")]
fn mountinfo_by_target(pid: i32, target: &str) -> Result<Option<MountInfo>, String> {
    Ok(mountinfo_entries(pid)?
        .into_iter()
        .find(|mount| mount.target == target))
}

#[cfg(target_os = "linux")]
fn mountinfo_by_id(pid: i32, mount_id: u32) -> Result<Option<MountInfo>, String> {
    Ok(mountinfo_entries(pid)?
        .into_iter()
        .find(|mount| mount.id == mount_id))
}

#[cfg(target_os = "linux")]
fn mount_id_from_fd(fd: libc::c_int) -> Option<u32> {
    // statx(2) exposes the same mount ID that appears in /proc/<pid>/mountinfo.
    // Use a fixed, correctly aligned kernel statx buffer so this helper remains
    // allocation-free when called in the post-fork namespace worker.
    const STATX_MNT_ID_MASK: u32 = 0x1000;
    const STATX_MNT_ID_OFFSET: usize = 144;
    let mut statx_buffer = [0u64; 32];
    let result = unsafe {
        libc::syscall(
            libc::SYS_statx,
            fd,
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            STATX_MNT_ID_MASK,
            statx_buffer.as_mut_ptr(),
        )
    };
    if result != 0 {
        return None;
    }
    let bytes = statx_buffer.as_ptr().cast::<u8>();
    let mask = unsafe { std::ptr::read_unaligned(bytes.cast::<u32>()) };
    if mask & STATX_MNT_ID_MASK == 0 {
        return None;
    }
    let mount_id =
        unsafe { std::ptr::read_unaligned(bytes.add(STATX_MNT_ID_OFFSET).cast::<u64>()) };
    u32::try_from(mount_id).ok()
}

#[cfg(target_os = "linux")]
fn containing_mountpoint(pid: i32, target: &str) -> Result<String, String> {
    mountinfo_entries(pid)?
        .into_iter()
        .filter(|mount| {
            mount.target == target
                || (mount.target == "/" && target.starts_with('/'))
                || target
                    .strip_prefix(&mount.target)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
        .max_by_key(|mount| mount.target.len())
        .map(|mount| mount.target)
        .ok_or_else(|| "could not identify mount containing GitDockRun target".to_owned())
}

#[cfg(target_os = "linux")]
fn same_file_identity(left: &fs::File, right: &fs::File) -> Result<bool, String> {
    let left = left
        .metadata()
        .map_err(|error| format!("inspect pinned namespace descriptor: {error}"))?;
    let right = right
        .metadata()
        .map_err(|error| format!("re-inspect namespace descriptor: {error}"))?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

#[cfg(target_os = "linux")]
fn execute_mount_namespace(
    expected_container_id: &str,
    pid: i32,
    source_fd: Option<i32>,
    source_path: Option<&Path>,
    target: &str,
    action: MountAction,
) -> Result<(), String> {
    let mount_namespace = fs::File::open(format!("/proc/{pid}/ns/mnt"))
        .map_err(|error| format!("open target mount namespace: {error}"))?;
    let pid_namespace = fs::File::open(format!("/proc/{pid}/ns/pid"))
        .map_err(|error| format!("open target PID namespace: {error}"))?;
    let container_root = open_path_handle(&PathBuf::from(format!("/proc/{pid}/root")), false)?;

    // Docker's PID can be recycled or the same container can restart between
    // inspect and opening /proc/PID/ns/* and /proc/PID/root. Re-resolve the
    // immutable container ID, then compare fresh proc handles to the pinned
    // descriptors before forking. Any mismatch fails closed.
    if container_pid_for_mount(expected_container_id)? != pid {
        return Err("bound container PID changed while pinning its namespaces".into());
    }
    let current_mount_namespace = fs::File::open(format!("/proc/{pid}/ns/mnt"))
        .map_err(|error| format!("recheck target mount namespace: {error}"))?;
    let current_pid_namespace = fs::File::open(format!("/proc/{pid}/ns/pid"))
        .map_err(|error| format!("recheck target PID namespace: {error}"))?;
    let current_root = open_path_handle(&PathBuf::from(format!("/proc/{pid}/root")), false)?;
    if !same_file_identity(&mount_namespace, &current_mount_namespace)?
        || !same_file_identity(&pid_namespace, &current_pid_namespace)?
        || !same_file_identity(&container_root, &current_root)?
    {
        return Err("target container namespace or root changed while being pinned".into());
    }

    let target_relative = target
        .strip_prefix('/')
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "mount target must be a non-root absolute path".to_owned())?;
    let target_relative_c =
        CString::new(target_relative).map_err(|_| "mount target contains a NUL byte".to_owned())?;
    let private_mountpoint = if matches!(action, MountAction::Bind { .. }) {
        let mountpoint = containing_mountpoint(pid, target)?;
        Some(
            CString::new(mountpoint)
                .map_err(|_| "container mountpoint contains a NUL byte".to_owned())?,
        )
    } else {
        None
    };
    // A source file/directory is not necessarily itself a mount point, so
    // open_tree(OPEN_TREE_CLONE) cannot be applied directly to its O_PATH FD.
    // Create a private-namespace bind of the source at a temporary, matching
    // mountpoint, clone that mount tree, then move the detached tree into the
    // target namespace. The staging mount never propagates into the host.
    // The source bind is intentionally non-recursive (MS_BIND, not MS_REC):
    // nested source mountpoints are not transferred. Read-only attributes are
    // applied recursively to the mount tree that was actually cloned.
    let staging = if matches!(action, MountAction::Bind { .. }) {
        use std::os::unix::ffi::OsStrExt;
        let source_fd =
            source_fd.ok_or_else(|| "mount source descriptor was not provided".to_owned())?;
        let mut source_stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(source_fd, source_stat.as_mut_ptr()) } != 0 {
            return Err(format!(
                "inspect mount source descriptor: {}",
                std::io::Error::last_os_error()
            ));
        }
        let source_stat = unsafe { source_stat.assume_init() };
        let source_type = source_stat.st_mode & libc::S_IFMT;
        let is_dir = source_type == libc::S_IFDIR;
        if !is_dir && source_type != libc::S_IFREG {
            return Err("mount source descriptor is not a regular file or directory".into());
        }
        let id = new_mount_id()?;
        let path = PathBuf::from(format!("/tmp/.gitrun-mount-stage-{id}"));
        if is_dir {
            fs::create_dir(&path)
                .map_err(|error| format!("create temporary mount staging directory: {error}"))?;
        } else {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|error| format!("create temporary mount staging file: {error}"))?;
        }
        let guard = MountStagingPath {
            path: path.clone(),
            is_dir,
        };
        let source_path =
            source_path.ok_or_else(|| "mount source path was not provided".to_owned())?;
        let source_path = CString::new(source_path.as_os_str().as_bytes())
            .map_err(|_| "invalid mount source path".to_owned())?;
        let staging_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| "invalid temporary mount staging path".to_owned())?;
        Some((guard, source_path, staging_path))
    } else {
        None
    };

    // Pin the namespaces and root before fork so a reused PID can never redirect
    // the mount operation to a replacement container.
    let outer_pid = unsafe { libc::fork() };
    if outer_pid < 0 {
        return Err(format!(
            "fork mount namespace worker: {}",
            std::io::Error::last_os_error()
        ));
    }
    if outer_pid == 0 {
        unsafe {
            // Stage the source in a private copy of the host mount namespace,
            // then keep only a detached mount FD before entering the target
            // namespace. This supports ordinary files/directories whose
            // original mounts are not visible inside the container.
            let source_tree_fd = if let Some((_, source_path, staging_path)) = staging.as_ref() {
                if libc::unshare(libc::CLONE_NEWNS) != 0 {
                    libc::_exit(56);
                }
                if libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                ) != 0
                {
                    libc::_exit(57);
                }
                if libc::mount(
                    source_path.as_ptr(),
                    staging_path.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                ) != 0
                {
                    libc::_exit(58);
                }
                // The canonical path is reopened inside the cloned host
                // namespace. Compare its mounted inode with the original
                // pinned source FD to detect path replacement races.
                let Some(source_fd) = source_fd else {
                    libc::_exit(61);
                };
                let mut original_stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                let mut staged_stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                if libc::fstat(source_fd, original_stat.as_mut_ptr()) != 0
                    || libc::stat(staging_path.as_ptr(), staged_stat.as_mut_ptr()) != 0
                {
                    libc::_exit(61);
                }
                let original_stat = original_stat.assume_init();
                let staged_stat = staged_stat.assume_init();
                if original_stat.st_dev != staged_stat.st_dev
                    || original_stat.st_ino != staged_stat.st_ino
                    || (original_stat.st_mode & libc::S_IFMT)
                        != (staged_stat.st_mode & libc::S_IFMT)
                {
                    libc::_exit(62);
                }
                let tree_fd = libc::syscall(
                    libc::SYS_open_tree,
                    libc::AT_FDCWD,
                    staging_path.as_ptr(),
                    libc::O_CLOEXEC | 1, // OPEN_TREE_CLONE
                ) as libc::c_int;
                if tree_fd < 0 {
                    libc::_exit(59);
                }
                if libc::umount2(staging_path.as_ptr(), libc::MNT_DETACH) != 0 {
                    libc::_exit(60);
                }
                if matches!(action, MountAction::Bind { read_only: true }) {
                    // Make the detached tree read-only before it is visible in
                    // the live container namespace. Applying this after
                    // move_mount would expose a writeable window to the job.
                    let attr = MountAttr {
                        attr_set: 0x01 | 0x02 | 0x04, // RDONLY | NOSUID | NODEV
                        attr_clr: 0,
                        propagation: 0,
                        userns_fd: 0,
                    };
                    let changed = libc::syscall(
                        libc::SYS_mount_setattr,
                        tree_fd,
                        c"".as_ptr(),
                        libc::AT_EMPTY_PATH | 0x8000, // AT_EMPTY_PATH | AT_RECURSIVE
                        &attr as *const MountAttr,
                        std::mem::size_of::<MountAttr>(),
                    );
                    if changed != 0 {
                        libc::_exit(64);
                    }
                }
                tree_fd
            } else {
                -1
            };

            if libc::setns(mount_namespace.as_raw_fd(), libc::CLONE_NEWNS) != 0 {
                libc::_exit(41);
            }
            // Joining the PID namespace affects only children, so fork again.
            if libc::setns(pid_namespace.as_raw_fd(), libc::CLONE_NEWPID) != 0 {
                libc::_exit(42);
            }
            let inner_pid = libc::fork();
            if inner_pid < 0 {
                libc::_exit(43);
            }
            if inner_pid == 0 {
                if libc::fchdir(container_root.as_raw_fd()) != 0
                    || libc::chroot(c".".as_ptr()) != 0
                    || libc::chdir(c"/".as_ptr()) != 0
                {
                    libc::_exit(44);
                }

                let root_fd = libc::open(
                    c"/".as_ptr(),
                    libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
                );
                if root_fd < 0 {
                    libc::_exit(51);
                }
                let how = OpenHow {
                    flags: (libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
                    mode: 0,
                    // RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH.
                    resolve: 0x02 | 0x04 | 0x08,
                };
                let target_fd = libc::syscall(
                    libc::SYS_openat2,
                    root_fd,
                    target_relative_c.as_ptr(),
                    &how as *const OpenHow,
                    std::mem::size_of::<OpenHow>(),
                ) as libc::c_int;
                if target_fd < 0 {
                    // Includes kernels without openat2: fail closed instead of
                    // falling back to raceable string-path resolution.
                    libc::_exit(52);
                }

                let mut target_stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                if libc::fstat(target_fd, target_stat.as_mut_ptr()) != 0 {
                    libc::_exit(53);
                }
                let target_stat = target_stat.assume_init();
                let target_type = target_stat.st_mode & libc::S_IFMT;
                if target_type != libc::S_IFDIR && target_type != libc::S_IFREG {
                    libc::_exit(54);
                }

                // Make only the mount that contains this target private.
                // This prevents propagation to peer namespaces without
                // recursively changing every mount in the container.
                if matches!(action, MountAction::Bind { .. }) {
                    let Some(private_mountpoint) = private_mountpoint.as_ref() else {
                        libc::_exit(50);
                    };
                    if libc::mount(
                        std::ptr::null(),
                        private_mountpoint.as_ptr(),
                        std::ptr::null(),
                        libc::MS_PRIVATE,
                        std::ptr::null(),
                    ) != 0
                    {
                        libc::_exit(50);
                    }
                }
                match action {
                    MountAction::Bind { .. } => {
                        if source_tree_fd < 0 {
                            libc::_exit(45);
                        }
                        // Both ends are addressed by pinned FDs. This avoids
                        // resolving a host-only source path inside the
                        // container namespace and avoids path replacement
                        // races between validation and the actual bind.
                        let moved = libc::syscall(
                            libc::SYS_move_mount,
                            source_tree_fd,
                            c"".as_ptr(),
                            target_fd,
                            c"".as_ptr(),
                            0x04 | 0x40, // MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH
                        );
                        if moved != 0 {
                            // Preserve errno through waitpid so CI reports
                            // the kernel reason, not just a generic helper code.
                            let errno = *libc::__errno_location();
                            libc::_exit(100 + errno.clamp(1, 154));
                        }
                        libc::_exit(0);
                    }
                    MountAction::RemountReadOnly { expected_mount_id } => {
                        // Re-open the path after the bind so the FD refers to
                        // the mounted filesystem, not the covered old target.
                        let mounted_fd = libc::syscall(
                            libc::SYS_openat2,
                            root_fd,
                            target_relative_c.as_ptr(),
                            &how as *const OpenHow,
                            std::mem::size_of::<OpenHow>(),
                        ) as libc::c_int;
                        if mounted_fd < 0 {
                            libc::_exit(46);
                        }
                        // The path may have been replaced after the parent
                        // inspected mountinfo. Never change attributes on a
                        // different mount that happens to occupy this target.
                        if mount_id_from_fd(mounted_fd) != Some(expected_mount_id) {
                            let _ = libc::close(mounted_fd);
                            libc::_exit(63);
                        }
                        let attr = MountAttr {
                            attr_set: 0x01 | 0x02 | 0x04, // RDONLY | NOSUID | NODEV
                            attr_clr: 0,
                            propagation: 0,
                            userns_fd: 0,
                        };
                        let changed = libc::syscall(
                            libc::SYS_mount_setattr,
                            mounted_fd,
                            c"".as_ptr(),
                            libc::AT_EMPTY_PATH | 0x8000, // AT_EMPTY_PATH | AT_RECURSIVE
                            &attr as *const MountAttr,
                            std::mem::size_of::<MountAttr>(),
                        );
                        let _ = libc::close(mounted_fd);
                        if changed != 0 {
                            libc::_exit(46);
                        }
                        libc::_exit(0);
                    }
                    MountAction::Unmount { expected_mount_id } => {
                        // Check mount identity inside the pinned target
                        // namespace, immediately before the unmount operation.
                        if mount_id_from_fd(target_fd) != Some(expected_mount_id) {
                            let _ = libc::close(target_fd);
                            libc::_exit(63);
                        }
                        // umount2 requires a pathname. The target container is
                        // rejected if CAP_SYS_ADMIN is in its effective or
                        // bounding set, preventing a workflow process from
                        // racing this identity check by replacing mounts.
                        let _ = libc::close(target_fd);
                        if libc::umount2(target_relative_c.as_ptr(), 0) != 0 {
                            libc::_exit(47);
                        }
                        libc::_exit(0);
                    }
                }
            }
            let mut status: libc::c_int = 0;
            if libc::waitpid(inner_pid, &mut status, 0) < 0 {
                libc::_exit(48);
            }
            if libc::WIFEXITED(status) {
                libc::_exit(libc::WEXITSTATUS(status));
            }
            libc::_exit(49);
        }
    }

    let mut status: libc::c_int = 0;
    let waited = unsafe { libc::waitpid(outer_pid, &mut status, 0) };
    if waited < 0 {
        return Err(format!(
            "wait for mount namespace worker: {}",
            std::io::Error::last_os_error()
        ));
    }
    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            -1
        };
        if (100..=254).contains(&code) {
            let errno = code - 100;
            return Err(format!(
                "failed to attach detached source mount: {}",
                std::io::Error::from_raw_os_error(errno)
            ));
        }
        let stage = match code {
            41 => "enter mount namespace",
            42 => "enter PID namespace",
            43 => "fork inside PID namespace",
            44 => "enter container root",
            45 => "attach detached source mount",
            46 => "enforce read-only mount",
            47 => "unmount target",
            48 => "wait for namespace operation",
            50 => "make target mount namespace private",
            51 => "open container root for target resolution",
            52 => "resolve target without symlinks (kernel openat2 support is required)",
            53 => "inspect pinned mount target",
            54 => "reject special-file mount target",
            56 => "create private source mount staging namespace",
            57 => "make source staging mounts private",
            58 => "bind source into staging mountpoint",
            59 => "clone staged source mount tree",
            60 => "detach temporary source mount",
            61 => "inspect staged source identity",
            62 => "reject source path replacement during mount staging",
            63 => "refuse operation because the target kernel mount ID changed or is unavailable",
            64 => "set read-only attributes on detached source mount before attachment (kernel mount_setattr support is required)",
            _ => "complete namespace operation",
        };
        return Err(format!("failed to {stage} (helper status {code})"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn mount_target_is_allowed(target: &str) -> bool {
    let path = Path::new(target);
    if !path.is_absolute()
        || target.ends_with('/')
        || target.contains("//")
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        return false;
    }
    [
        "/workspace/",
        "/home/runner/actions-runner/_work/",
        "/tmp/gitrun-mounts/",
        "/var/lib/gitrun/shared/",
    ]
    .iter()
    .any(|prefix| target.starts_with(prefix))
}

#[cfg(target_os = "linux")]
fn recover_mount_id_by_source_identity(
    pid: i32,
    record: &MountRecord,
) -> Result<Option<u32>, String> {
    // Only look at the exact destination captured in the durable intent. A
    // global scan by inode could otherwise select an unrelated mount of the
    // same directory elsewhere in this container.
    if !mount_target_is_allowed(&record.target) {
        return Err(format!(
            "mount lease {} has a target outside the recovery allowlist",
            record.id
        ));
    }
    let matching_entries: Vec<MountInfo> = mountinfo_entries(pid)?
        .into_iter()
        .filter(|mount| mount.target == record.target)
        .collect();
    let mount = match matching_entries.as_slice() {
        [] => return Ok(None), // A crash may happen after persisting intent but before bind.
        [mount] => mount,
        _ => {
            return Err(format!(
                "mount lease {} has multiple mount entries at its recorded target; preserving it for manual recovery",
                record.id
            ));
        }
    };
    let (Some(source_device), Some(source_inode)) = (record.source_device, record.source_inode)
    else {
        return Err(format!(
            "mount lease {} has no recorded source identity; automatic recovery is unsafe",
            record.id
        ));
    };
    let mounted_path = PathBuf::from(format!("/proc/{pid}/root{}", record.target));
    let metadata = fs::metadata(&mounted_path)
        .map_err(|error| format!("inspect recorded mount target {}: {error}", record.target))?;
    if metadata.dev() != source_device || metadata.ino() != source_inode {
        return Err(format!(
            "mount lease {} target no longer matches its original source inode; preserving it for manual recovery",
            record.id
        ));
    }
    Ok(Some(mount.id))
}

#[cfg(target_os = "linux")]
fn cleanup_mount_lease_in_namespace(pid: i32, record: &MountRecord) -> Result<(), String> {
    // The durable "preparing" state proves the privileged worker was never
    // entered. Do not inspect or infer a mount ID in this state: a different
    // actor may have mounted the same source at this target after the lease was
    // written, and inode equality does not make that external mount ours.
    if record.state == "preparing" {
        return match record.kernel_mount_id {
            None => Ok(()),
            Some(mount_id) => Err(format!(
                "mount lease {} is still preparing but unexpectedly records kernel mount ID {mount_id}; preserving it for manual review",
                record.id
            )),
        };
    }

    // If the service crashed after the kernel bind but before persisting its
    // mount ID, recovery may identify it only by a unique match at the exact
    // durable target and original source device/inode. Never fall back to
    // unmounting by target path alone; ambiguous or legacy leases are preserved.
    let expected_mount_id = match record.kernel_mount_id {
        Some(mount_id) => Some(mount_id),
        None => recover_mount_id_by_source_identity(pid, record)?,
    };
    let Some(expected_mount_id) = expected_mount_id else {
        // Once state is "attaching" (or a later recovery state), the mount may
        // have landed on a renamed target or the service may have crashed
        // before its mount ID was persisted. Preserve the lease for manual
        // recovery instead of deleting the only journal entry.
        return Err(format!(
            "mount lease {} has no identifiable mount at its recorded target; preserving the lease because attach may have raced a target rename",
            record.id
        ));
    };
    let Some(mount) = mountinfo_by_id(pid, expected_mount_id)? else {
        // The exact kernel mount ID is already absent: cleanup has succeeded.
        return Ok(());
    };

    // Treat the registry as a recovery journal, not as sufficient authority by
    // itself. Validate the target and the mounted source identity before ever
    // unmounting an ID loaded from disk. This prevents a stale/corrupt record
    // from being used to unmount a different mount in the same container.
    if mount.target != record.target || !mount_target_is_allowed(&mount.target) {
        return Err(format!(
            "mount lease {} kernel ID {expected_mount_id} resolves to unexpected target {}; preserving lease",
            record.id, mount.target
        ));
    }
    let (Some(source_device), Some(source_inode)) = (record.source_device, record.source_inode)
    else {
        return Err(format!(
            "mount lease {} is missing the recorded source identity; refusing automatic unmount",
            record.id
        ));
    };
    let mounted_path = PathBuf::from(format!("/proc/{pid}/root{}", record.target));
    let metadata = fs::metadata(&mounted_path)
        .map_err(|error| format!("inspect recorded mount target {}: {error}", record.target))?;
    if metadata.dev() != source_device || metadata.ino() != source_inode {
        return Err(format!(
            "mount lease {} source identity at {} does not match; preserving lease",
            record.id, record.target
        ));
    }

    execute_mount_namespace(
        &record.container_id,
        pid,
        None,
        None,
        &mount.target,
        MountAction::Unmount { expected_mount_id },
    )?;
    if mountinfo_by_id(pid, expected_mount_id)?.is_some() {
        return Err(format!(
            "kernel still reports mount ID {expected_mount_id} after cleanup"
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
struct MountCleanupOwner<'a> {
    repository: &'a str,
    run_id: u64,
    job: &'a str,
    container_id: &'a str,
    requester_runner: &'a str,
}

#[cfg(target_os = "linux")]
fn validate_melt_target_has_no_mount_leases(
    registry: &MountRegistry,
    target_container_id: &str,
) -> Result<(), String> {
    if registry
        .mounts
        .iter()
        .any(|mount| mount.container_id == target_container_id)
    {
        return Err(
            "GitDockRun --melt refuses a target with outstanding mount leases; unmount first to protect host-mounted data"
                .into(),
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn cleanup_owned_mounts_for_disconnect<F>(
    registry: &mut MountRegistry,
    state_dir: &Path,
    owner: MountCleanupOwner<'_>,
    mut cleanup: F,
) -> Result<(), String>
where
    F: FnMut(&MountRecord) -> Result<(), String>,
{
    let owned_mounts: Vec<MountRecord> = registry
        .mounts
        .iter()
        .filter(|mount| {
            mount.repository == owner.repository
                && mount.run_id == owner.run_id
                && mount.job == owner.job
                && mount.container_id == owner.container_id
                && mount.requester_runner == owner.requester_runner
        })
        .cloned()
        .collect();

    // Do not drop any lease records until every owned mount was cleaned.
    // If one cleanup fails after an earlier one succeeded, retaining the
    // earlier now-stale lease is safe: the next reconciliation can confirm
    // that its exact kernel mount ID is absent.
    for mount in &owned_mounts {
        cleanup(mount).map_err(|error| {
            format!(
                "cannot disconnect while GitDockRun mount {} needs cleanup: {error}",
                mount.id
            )
        })?;
    }

    if !owned_mounts.is_empty() {
        registry.mounts.retain(|mount| {
            mount.repository != owner.repository
                || mount.run_id != owner.run_id
                || mount.job != owner.job
                || mount.container_id != owner.container_id
                || mount.requester_runner != owner.requester_runner
        });
        registry.save(state_dir)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn mount_cleanup_status_is_terminal(status: &str) -> bool {
    matches!(
        status.to_ascii_lowercase().as_str(),
        "created" | "dead" | "exited"
    )
}

#[cfg(target_os = "linux")]
fn cleanup_mount_record(record: &MountRecord) -> Result<(), String> {
    match container_pid_for_mount(&record.container_id) {
        Ok(pid) => cleanup_mount_lease_in_namespace(pid, record),
        Err(pid_error) => match docker::container_status(&record.container_id) {
            Ok(Some(status)) if mount_cleanup_status_is_terminal(&status) => Ok(()),
            Ok(None) => Ok(()),
            Ok(Some(status)) => Err(format!(
                "original container reports status {status:?}; preserve mount lease because its namespace cannot be inspected: {pid_error}"
            )),
            Err(status_error) => Err(format!(
                "cannot safely determine whether original container is gone: {pid_error}; {status_error}"
            )),
        },
    }
}

fn workflow_job_is_completed(status: &str, _conclusion: Option<&str>) -> bool {
    // The status is authoritative; a conclusion-like field must never override
    // an in-progress/queued state and cause the mount to disappear early.
    status.eq_ignore_ascii_case("completed")
}

#[cfg(target_os = "linux")]
fn reconcile_completed_mounts(
    client: &GitHubClient,
    state_dir: &Path,
    repository: &str,
) -> Result<(), String> {
    let _lifecycle_guard = acquire_gitdock_lifecycle_guard(state_dir)?;
    let mut registry = MountRegistry::load(state_dir)?;
    let snapshot = registry.mounts.clone();
    let mut remove_ids = Vec::new();

    for record in snapshot
        .iter()
        .filter(|record| record.repository == repository)
    {
        // A crash during mount setup or read-only enforcement must fail closed.
        // If the lease has a known kernel ID, remove only that exact mount.
        // If the ID was not persisted before a crash, recovery may identify it
        // only by a unique match for the durable source device/inode. Ambiguous
        // or legacy leases remain for manual recovery.
        if matches!(
            record.state.as_str(),
            "preparing" | "attaching" | "read-only-pending" | "cleanup-required"
        ) {
            match cleanup_mount_record(record) {
                Ok(()) => remove_ids.push(record.id.clone()),
                Err(error) => eprintln!(
                    "gitrun-api: preserving incomplete mount lease {} for safe recovery: {error}",
                    record.id
                ),
            }
            continue;
        }

        let job_completed = match client.find_workflow_job(repository, record.run_id, &record.job) {
            Ok(Some(job)) => workflow_job_is_completed(&job.status, job.conclusion.as_deref()),
            // A missing job does not prove completion: preserve the mount until
            // the workflow API can positively confirm a terminal job state.
            Ok(None) => false,
            Err(error) => {
                eprintln!(
                    "gitrun-api: cannot verify job {} for mount {}: {error}",
                    record.job, record.id
                );
                continue;
            }
        };
        if !job_completed {
            continue;
        }

        match cleanup_mount_record(record) {
            Ok(()) => remove_ids.push(record.id.clone()),
            Err(error) => eprintln!(
                "gitrun-api: preserving mount lease {} because cleanup could not be verified: {error}",
                record.id
            ),
        }
    }

    if !remove_ids.is_empty() {
        registry
            .mounts
            .retain(|record| !remove_ids.contains(&record.id));
        registry.save(state_dir)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn reconcile_completed_mounts(
    _client: &GitHubClient,
    _state_dir: &Path,
    _repository: &str,
) -> Result<(), String> {
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod mount_runtime_tests {
    use super::*;
    use std::process::Command;

    struct TemporaryContainer(String);

    impl Drop for TemporaryContainer {
        fn drop(&mut self) {
            let _ = Command::new("docker")
                .args(["rm", "-f", self.0.as_str()])
                .output();
        }
    }

    struct TemporaryDirectory(PathBuf);

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct TemporaryMount(PathBuf);

    impl Drop for TemporaryMount {
        fn drop(&mut self) {
            use std::os::unix::ffi::OsStrExt;

            if let Ok(path) = std::ffi::CString::new(self.0.as_os_str().as_bytes()) {
                // Best-effort cleanup if an assertion exits the test early.
                unsafe {
                    libc::umount2(path.as_ptr(), libc::MNT_DETACH);
                }
            }
        }
    }

    #[test]
    fn runtime_mount_policy_rechecks_api_resource_capability_and_source() {
        use gitrun_core::api_policy::{ApiPolicy, PolicyMatrix};
        use gitrun_core::settings::{
            DockerPolicy, LogicContainerPolicy, MountPolicy, MountRule, RepositorySettings,
        };

        let dir = std::env::temp_dir().join(format!(
            "gitrun-mount-runtime-policy-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("source.txt"), b"payload").unwrap();

        let make_settings = |api_enabled: bool, container_allowed: bool, mountable: bool| {
            let mut settings = GitRunSettings::default();
            let mut matrix = PolicyMatrix::default();
            matrix.set(
                GitRunApi::GitDockRun,
                if api_enabled {
                    ApiPolicy::enabled_with([GitRunOperation::Mount])
                } else {
                    ApiPolicy::disabled()
                },
            );
            settings.global = matrix;

            let mut docker = DockerPolicy {
                allowed_container_names: if container_allowed {
                    vec!["runner".into()]
                } else {
                    Vec::new()
                },
                ..DockerPolicy::default()
            };
            docker.logic_containers.insert(
                "runner".into(),
                LogicContainerPolicy {
                    mountable,
                    ..LogicContainerPolicy::default()
                },
            );
            docker.allowed_mounts = MountPolicy {
                enabled: true,
                rules: vec![MountRule {
                    source: dir.to_string_lossy().into_owned(),
                    recursive: true,
                    read_only: true,
                    allow: true,
                }],
            };
            settings.repositories.insert(
                "owner/repo".into(),
                RepositorySettings {
                    docker,
                    ..RepositorySettings::default()
                },
            );
            settings
        };

        let source = dir.join("source.txt");
        let enabled = make_settings(true, true, true);
        assert!(
            authorize_live_mount_source(&enabled, "owner/repo", "runner", &source, true).is_ok()
        );
        assert!(
            authorize_live_mount_source(&enabled, "owner/repo", "runner", &source, false).is_err()
        );
        assert!(authorize_live_mount_source(
            &make_settings(false, true, true),
            "owner/repo",
            "runner",
            &source,
            true
        )
        .is_err());
        assert!(authorize_live_mount_source(
            &make_settings(true, false, true),
            "owner/repo",
            "runner",
            &source,
            true
        )
        .is_err());
        assert!(authorize_live_mount_source(
            &make_settings(true, true, false),
            "owner/repo",
            "runner",
            &source,
            true
        )
        .is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mount_capability_check_rejects_effective_and_reacquirable_sys_admin() {
        let fields = ["CapEff:", "CapPrm:", "CapInh:", "CapAmb:", "CapBnd:"];
        let empty_mask = "0000000000000000";
        let sys_admin = format!("{:016x}", 1u64 << 21);
        let status_with = |enabled_field: Option<&str>| {
            fields
                .iter()
                .map(|field| {
                    let mask = if Some(*field) == enabled_field {
                        sys_admin.as_str()
                    } else {
                        empty_mask
                    };
                    format!("{field} {mask}")
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        assert!(!status_has_mount_admin_capability(&status_with(None)).unwrap());
        for field in fields {
            assert!(
                status_has_mount_admin_capability(&status_with(Some(field))).unwrap(),
                "{field} containing CAP_SYS_ADMIN must block live mounts"
            );
        }
        assert!(status_has_mount_admin_capability("Name: worker").is_err());
    }

    #[test]
    fn workflow_mount_cleanup_requires_completed_job_status() {
        assert!(!workflow_job_is_completed("queued", None));
        assert!(!workflow_job_is_completed("waiting", None));
        assert!(!workflow_job_is_completed("in_progress", None));
        assert!(workflow_job_is_completed("completed", None));
        assert!(!workflow_job_is_completed("in_progress", Some("cancelled")));
        assert!(workflow_job_is_completed("COMPLETED", Some("success")));
    }

    #[cfg(unix)]
    #[test]
    fn mount_registry_rejects_symlinked_and_hard_linked_files() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!(
            "gitrun-mount-registry-links-{}-{}",
            std::process::id(),
            now()
        ));
        let _dir_guard = TemporaryDirectory(dir.clone());
        fs::create_dir_all(&dir).unwrap();
        let path = MountRegistry::path(&dir);
        let target = dir.join("target.json");
        fs::write(&target, r#"{"mounts":[]}"#).unwrap();
        symlink(&target, &path).unwrap();
        assert!(MountRegistry::load(&dir).is_err());
        fs::remove_file(&path).unwrap();
        fs::hard_link(&target, &path).unwrap();
        assert!(MountRegistry::load(&dir).is_err());
    }

    #[test]
    fn mount_registry_rejects_duplicate_or_empty_mount_ids() {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-mount-registry-duplicate-{}-{}",
            std::process::id(),
            now()
        ));
        let _dir_guard = TemporaryDirectory(dir.clone());
        let record = |id: &str, runner: &str| MountRecord {
            id: id.into(),
            repository: "owner/repo".into(),
            run_id: 42,
            job: "build".into(),
            requester_runner: runner.into(),
            container: "runner-container".into(),
            container_id: "test-container-id".into(),
            source: "/tmp/source".into(),
            target: "/workspace/mount".into(),
            read_only: true,
            source_device: Some(1),
            source_inode: Some(2),
            kernel_mount_id: Some(10),
            state: "mounted".into(),
            created_at: now(),
        };

        for (name, mounts, expected) in [
            (
                "duplicate",
                vec![record("same-id", "runner-a"), record("same-id", "runner-b")],
                "duplicate mount ID",
            ),
            ("empty", vec![record("  ", "runner-a")], "empty mount ID"),
        ] {
            let registry = MountRegistry { mounts };
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                MountRegistry::path(&dir),
                serde_json::to_vec(&registry).unwrap(),
            )
            .unwrap();
            let error =
                MountRegistry::load(&dir).expect_err("ambiguous mount registry must fail closed");
            assert!(error.contains(expected), "{name}: {error}");
            fs::remove_file(MountRegistry::path(&dir)).unwrap();
        }
    }

    #[test]
    fn mount_registry_save_rejects_duplicate_or_empty_mount_ids() {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-mount-registry-save-invalid-{}-{}",
            std::process::id(),
            now()
        ));
        let _dir_guard = TemporaryDirectory(dir.clone());
        let record = |id: &str, runner: &str| MountRecord {
            id: id.into(),
            repository: "owner/repo".into(),
            run_id: 42,
            job: "build".into(),
            requester_runner: runner.into(),
            container: "runner-container".into(),
            container_id: "test-container-id".into(),
            source: "/tmp/source".into(),
            target: "/workspace/mount".into(),
            read_only: true,
            source_device: Some(1),
            source_inode: Some(2),
            kernel_mount_id: Some(10),
            state: "mounted".into(),
            created_at: now(),
        };

        for (name, mounts, expected) in [
            (
                "duplicate",
                vec![record("same-id", "runner-a"), record("same-id", "runner-b")],
                "duplicate mount ID",
            ),
            ("empty", vec![record("  ", "runner-a")], "empty mount ID"),
        ] {
            let registry = MountRegistry { mounts };
            let error = registry
                .save(&dir)
                .expect_err("invalid mount registry must not be persisted");
            assert!(error.contains(expected), "{name}: {error}");
            assert!(
                !MountRegistry::path(&dir).exists(),
                "{name}: invalid registry file must not be created"
            );
        }
    }

    #[test]
    fn disconnect_mount_cleanup_preserves_leases_on_failure_and_filters_ownership() {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-disconnect-mounts-{}-{}",
            std::process::id(),
            now()
        ));
        let _dir_guard = TemporaryDirectory(dir.clone());
        let record = |id: &str, repository: &str, run_id, job: &str, runner: &str| MountRecord {
            id: id.into(),
            repository: repository.into(),
            run_id,
            job: job.into(),
            requester_runner: runner.into(),
            container: "runner-container".into(),
            container_id: "test-container-id".into(),
            source: "/tmp/source".into(),
            target: "/workspace/mount".into(),
            read_only: true,
            source_device: Some(1),
            source_inode: Some(2),
            kernel_mount_id: Some(10),
            state: "mounted".into(),
            created_at: now(),
        };
        let owned_a = record("owned-a", "owner/repo", 42, "build", "runner-a");
        let owned_b = record("owned-b", "owner/repo", 42, "build", "runner-a");
        let other_runner = record("other-runner", "owner/repo", 42, "build", "runner-b");
        let other_job = record("other-job", "owner/repo", 42, "test", "runner-a");
        let other_repo = record("other-repo", "someone/else", 42, "build", "runner-a");
        let mut other_container = record("other-container", "owner/repo", 42, "build", "runner-a");
        other_container.container_id = "different-container-id".into();
        let other_runner_same_scope = record(
            "other-runner-same-scope",
            "owner/repo",
            42,
            "build",
            "runner-b",
        );
        let original = MountRegistry {
            mounts: vec![
                owned_a,
                owned_b,
                other_runner,
                other_job,
                other_repo,
                other_container,
                other_runner_same_scope,
            ],
        };
        original.save(&dir).expect("persist fixture mount leases");
        let mut registry = MountRegistry::load(&dir).expect("load fixture mount leases");

        let mut cleaned = Vec::new();
        let failed = cleanup_owned_mounts_for_disconnect(
            &mut registry,
            &dir,
            MountCleanupOwner {
                repository: "owner/repo",
                run_id: 42,
                job: "build",
                container_id: "test-container-id",
                requester_runner: "runner-a",
            },
            |mount| {
                cleaned.push(mount.id.clone());
                if mount.id == "owned-b" {
                    Err("simulated cleanup failure".into())
                } else {
                    Ok(())
                }
            },
        );
        assert!(failed
            .expect_err("failed mount cleanup must abort disconnect")
            .contains("owned-b"));
        assert_eq!(
            cleaned.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["owned-a", "owned-b"]
        );
        assert_eq!(registry.mounts.len(), 7);
        assert_eq!(
            MountRegistry::load(&dir)
                .expect("load leases after cleanup failure")
                .mounts
                .len(),
            7,
            "failed disconnect must preserve all lease records on disk"
        );

        let mut cleaned_after_retry = Vec::new();
        cleanup_owned_mounts_for_disconnect(
            &mut registry,
            &dir,
            MountCleanupOwner {
                repository: "owner/repo",
                run_id: 42,
                job: "build",
                container_id: "test-container-id",
                requester_runner: "runner-a",
            },
            |mount| {
                cleaned_after_retry.push(mount.id.clone());
                Ok(())
            },
        )
        .expect("successful cleanup removes only this runner's leases");
        assert_eq!(
            cleaned_after_retry
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["owned-a", "owned-b"]
        );
        let persisted = MountRegistry::load(&dir).expect("load leases after successful cleanup");
        let remaining: Vec<&str> = persisted
            .mounts
            .iter()
            .map(|mount| mount.id.as_str())
            .collect();
        assert_eq!(
            remaining,
            vec![
                "other-runner",
                "other-job",
                "other-repo",
                "other-container",
                "other-runner-same-scope",
            ]
        );
    }

    #[test]
    fn cleanup_requires_an_explicit_terminal_container_status() {
        for status in ["created", "dead", "exited", "CREATED", "Exited"] {
            assert!(mount_cleanup_status_is_terminal(status), "{status}");
        }
        for status in ["running", "paused", "restarting", "removing", "unknown"] {
            assert!(!mount_cleanup_status_is_terminal(status), "{status}");
        }
    }

    #[test]
    fn mount_registry_lock_file_is_private() {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-mount-lock-test-{}-{}",
            std::process::id(),
            now()
        ));
        fs::create_dir_all(&dir).unwrap();
        let lock = MountRegistryLock::acquire(&dir).expect("acquire mount registry lock");
        let metadata = fs::metadata(dir.join(MOUNT_REGISTRY_LOCK_FILE)).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        drop(lock);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mount_registry_lock_serializes_independent_openers() {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-mount-lock-concurrency-{}-{}",
            std::process::id(),
            now()
        ));
        fs::create_dir_all(&dir).unwrap();
        let first = MountRegistryLock::acquire(&dir).expect("acquire initial mount lock");
        let worker_dir = dir.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let second = MountRegistryLock::acquire(&worker_dir)
                .expect("acquire second mount lock after first is released");
            acquired_tx.send(()).unwrap();
            drop(second);
        });

        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("second lock requester started");
        assert!(
            matches!(
                acquired_rx.recv_timeout(std::time::Duration::from_millis(100)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "second opener acquired the registry lock while it was held"
        );

        drop(first);
        acquired_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("second opener acquires the lock after release");
        worker.join().expect("lock requester thread must complete");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lifecycle_guard_prevents_lost_updates_between_concurrent_registry_writers() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = std::env::temp_dir().join(format!(
            "gitrun-dock-registry-writer-race-{}-{}",
            std::process::id(),
            now()
        ));
        fs::create_dir_all(&dir).unwrap();
        let _temporary = TemporaryDirectory(dir.clone());

        let first_guard = acquire_gitdock_lifecycle_guard(&dir).unwrap();
        let worker_dir = dir.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _guard = acquire_gitdock_lifecycle_guard(&worker_dir).unwrap();
            acquired_tx.send(()).unwrap();

            let mut registry = DockRegistry::load(&worker_dir).unwrap();
            registry.upsert(DockBinding {
                repository: "owner/repo".into(),
                run_id: 42,
                job: "job-b".into(),
                container: "runner-b".into(),
                container_id: "test-container-b".into(),
                dynamic: true,
                dock_only: false,
                requester_runner: "runner-b".into(),
                connected_at: now(),
            });
            registry.save(&worker_dir).unwrap();
        });

        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("second writer started");
        assert!(
            matches!(
                acquired_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "second registry writer must wait for the lifecycle transaction lock"
        );

        let mut registry = DockRegistry::load(&dir).unwrap();
        registry.upsert(DockBinding {
            repository: "owner/repo".into(),
            run_id: 42,
            job: "job-a".into(),
            container: "runner-a".into(),
            container_id: "test-container-a".into(),
            dynamic: true,
            dock_only: false,
            requester_runner: "runner-a".into(),
            connected_at: now(),
        });
        registry.save(&dir).unwrap();

        drop(first_guard);
        acquired_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("second writer acquires lock after first transaction");
        worker.join().unwrap();

        let registry = DockRegistry::load(&dir).unwrap();
        assert_eq!(registry.bindings.len(), 2);
        assert!(registry.binding("owner/repo", 42, "job-a").is_some());
        assert!(registry.binding("owner/repo", 42, "job-b").is_some());
    }

    #[test]
    fn mount_registry_lock_serializes_across_processes() {
        const CHILD_MODE: &str = "GITRUN_MOUNT_LOCK_CHILD";
        const CHILD_DIR: &str = "GITRUN_MOUNT_LOCK_CHILD_DIR";

        if std::env::var_os(CHILD_MODE).is_some() {
            let dir = PathBuf::from(std::env::var_os(CHILD_DIR).expect("child lock directory"));
            fs::write(dir.join("child-ready"), b"ready").expect("signal child started");
            let _lock = MountRegistryLock::acquire(&dir)
                .expect("child acquires the mount lock after the parent releases it");
            fs::write(dir.join("child-acquired"), b"acquired").expect("signal child acquired lock");
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "gitrun-mount-lock-process-{}-{}",
            std::process::id(),
            now()
        ));
        let _dir_guard = TemporaryDirectory(dir.clone());
        fs::create_dir_all(&dir).expect("create process-lock test directory");
        let parent_lock = MountRegistryLock::acquire(&dir).expect("acquire parent mount lock");
        let executable = std::env::current_exe().expect("locate current test executable");
        let mut child = Command::new(executable)
            .arg("mount_registry_lock_serializes_across_processes")
            .arg("--nocapture")
            .env(CHILD_MODE, "1")
            .env(CHILD_DIR, &dir)
            .spawn()
            .expect("spawn child test process");

        let ready_path = dir.join("child-ready");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready_path.exists() && std::time::Instant::now() < deadline {
            if let Some(status) = child.try_wait().expect("check child process") {
                panic!("child exited before requesting lock: {status}");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            ready_path.exists(),
            "child process should reach lock acquisition"
        );

        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            child
                .try_wait()
                .expect("check blocked child process")
                .is_none(),
            "independent process unexpectedly acquired the lock while parent held it"
        );
        assert!(
            !dir.join("child-acquired").exists(),
            "child must not signal lock acquisition while parent holds it"
        );

        drop(parent_lock);
        let output = child
            .wait_with_output()
            .expect("wait for child lock process");
        assert!(
            output.status.success(),
            "child lock test failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            dir.join("child-acquired").exists(),
            "child should acquire the lock after parent releases it"
        );
    }

    #[test]
    fn privileged_mount_namespace_bind_unmount_smoke_test() {
        if std::env::var("GITRUN_ENABLE_PRIVILEGED_MOUNT_TESTS")
            .ok()
            .as_deref()
            != Some("1")
        {
            // The default workspace suite must not mutate the host mount table.
            return;
        }
        assert_eq!(
            unsafe { libc::geteuid() },
            0,
            "the privileged namespace test must run as root"
        );

        // Isolate this test's temporary nested source mount from the host mount
        // namespace. The bind itself is non-recursive, and this test must never
        // leave its auxiliary mount propagated onto the runner host.
        assert_eq!(
            unsafe { libc::unshare(libc::CLONE_NEWNS) },
            0,
            "create an isolated mount namespace for the privileged smoke test"
        );
        assert_eq!(
            unsafe {
                libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                )
            },
            0,
            "make the privileged smoke-test mount namespace private"
        );

        let suffix = format!("{}-{}", std::process::id(), now());
        let name = format!("gitrun-mount-smoke-{suffix}");
        let container_guard = TemporaryContainer(name.clone());
        let source_dir = std::env::temp_dir().join(format!("gitrun-mount-src-{suffix}"));
        let source_guard = TemporaryDirectory(source_dir.clone());
        fs::create_dir_all(&source_dir).expect("create host source");
        fs::write(source_dir.join("payload.txt"), b"mount-payload\n")
            .expect("write host source file");
        let target = format!("/workspace/gitrun-mount-{suffix}");
        let image =
            std::env::var("GITRUN_MOUNT_TEST_IMAGE").unwrap_or_else(|_| "alpine:3.21".into());
        let output = Command::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                name.as_str(),
                image.as_str(),
                "sh",
                "-c",
                &format!("mkdir -p '{target}' && sleep 300"),
            ])
            .output()
            .expect("invoke Docker for disposable mount target");
        assert!(
            output.status.success(),
            "could not start disposable container: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let container_id = docker::container_id(&name)
            .expect("inspect Docker container")
            .expect("container ID");
        let pid = container_pid_for_mount(&container_id).expect("resolve container PID");
        validate_mount_target(pid, &target, true).expect("validate empty directory target");
        assert!(mountinfo_by_target(pid, &target)
            .expect("read mount table before mounting")
            .is_none());
        assert!(validate_mount_target(pid, "/workspace/../etc", true).is_err());

        let symlink_target = format!("/workspace/gitrun-mount-link-{suffix}");
        let nonempty_target = format!("/workspace/gitrun-mount-nonempty-{suffix}");
        let prepare_invalid_targets = Command::new("docker")
            .args([
                "exec",
                name.as_str(),
                "sh",
                "-c",
                &format!(
                    "ln -s '{target}' '{symlink_target}' && mkdir -p '{nonempty_target}' && printf hidden > '{nonempty_target}/hidden.txt'"
                ),
            ])
            .output()
            .expect("prepare invalid mount targets");
        assert!(
            prepare_invalid_targets.status.success(),
            "could not prepare invalid mount targets: {}",
            String::from_utf8_lossy(&prepare_invalid_targets.stderr)
        );
        assert!(validate_mount_target(pid, &symlink_target, true).is_err());
        assert!(validate_mount_target(pid, &nonempty_target, true).is_err());

        // Put a real submount below the source directory. The GitDockRun
        // contract is a non-recursive bind: the destination must expose the
        // underlying directory contents, not the nested mount's contents.
        use std::os::unix::ffi::OsStrExt;
        let source_nested = source_dir.join("nested");
        fs::create_dir(&source_nested).expect("create nested source mountpoint");
        fs::write(source_nested.join("underlay.txt"), b"underlay\n")
            .expect("write nested source underlay");
        let nested_overlay =
            std::env::temp_dir().join(format!("gitrun-mount-nested-overlay-{suffix}"));
        let _nested_overlay_guard = TemporaryDirectory(nested_overlay.clone());
        fs::create_dir_all(&nested_overlay).expect("create nested source overlay");
        fs::write(nested_overlay.join("submount-only.txt"), b"submount\n")
            .expect("write nested source overlay file");
        let nested_source_c = std::ffi::CString::new(source_nested.as_os_str().as_bytes()).unwrap();
        let nested_overlay_c =
            std::ffi::CString::new(nested_overlay.as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe {
                libc::mount(
                    nested_overlay_c.as_ptr(),
                    nested_source_c.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                )
            },
            0,
            "create nested source mount in the isolated test namespace"
        );
        let _nested_source_mount_guard = TemporaryMount(source_nested.clone());

        let canonical_source = fs::canonicalize(&source_dir).unwrap();
        let source = open_mount_source(&canonical_source).expect("open canonical host source");
        execute_mount_namespace(
            &container_id,
            pid,
            Some(std::os::fd::AsRawFd::as_raw_fd(&source)),
            Some(&canonical_source),
            &target,
            MountAction::Bind { read_only: true },
        )
        .expect("bind read-only mount into target namespace");

        let before = mountinfo_by_target(pid, &target)
            .expect("read mount table after bind")
            .expect("bind mount should appear");
        assert!(
            validate_mount_target(pid, &target, true).is_err(),
            "an existing mountpoint must not be accepted as a new mount target"
        );
        let pre_remount_write = Command::new("docker")
            .args([
                "exec",
                name.as_str(),
                "sh",
                "-c",
                &format!("printf changed > '{target}/payload.txt'"),
            ])
            .output()
            .expect("attempt write immediately after read-only bind");
        assert!(
            !pre_remount_write.status.success(),
            "read-only bind exposed a writable window before post-attach verification"
        );
        assert_eq!(
            fs::read_to_string(source_dir.join("payload.txt")).unwrap(),
            "mount-payload\n",
            "pre-remount write attempt changed the host source"
        );
        let wrong_mount_id = before.id.wrapping_add(1);
        assert!(execute_mount_namespace(
            &container_id,
            pid,
            None,
            None,
            &target,
            MountAction::RemountReadOnly {
                expected_mount_id: wrong_mount_id,
            },
        )
        .is_err());
        assert!(mountinfo_by_id(pid, before.id)
            .expect("verify identity mismatch did not remove the mount")
            .is_some());
        execute_mount_namespace(
            &container_id,
            pid,
            None,
            None,
            &target,
            MountAction::RemountReadOnly {
                expected_mount_id: before.id,
            },
        )
        .expect("make bind mount read-only");

        let read = Command::new("docker")
            .args([
                "exec",
                name.as_str(),
                "cat",
                &format!("{target}/payload.txt"),
            ])
            .output()
            .expect("read mounted file from target container");
        assert!(
            read.status.success(),
            "mounted file is not readable: {}",
            String::from_utf8_lossy(&read.stderr)
        );
        assert_eq!(read.stdout, b"mount-payload\n");

        let underlay_read = Command::new("docker")
            .args([
                "exec",
                name.as_str(),
                "cat",
                &format!("{target}/nested/underlay.txt"),
            ])
            .output()
            .expect("read source underlay at nested mountpoint");
        assert!(
            underlay_read.status.success(),
            "non-recursive bind did not expose the nested mountpoint underlay: {}",
            String::from_utf8_lossy(&underlay_read.stderr)
        );
        assert_eq!(underlay_read.stdout, b"underlay\n");
        let nested_mount_absent = Command::new("docker")
            .args([
                "exec",
                name.as_str(),
                "test",
                "!",
                "-e",
                &format!("{target}/nested/submount-only.txt"),
            ])
            .output()
            .expect("check nested source mount was not transferred");
        assert!(
            nested_mount_absent.status.success(),
            "non-recursive GitDockRun bind unexpectedly transferred a nested source mount"
        );

        let write = Command::new("docker")
            .args([
                "exec",
                name.as_str(),
                "sh",
                "-c",
                &format!("printf changed > '{target}/payload.txt'"),
            ])
            .output()
            .expect("attempt write to read-only mount");
        assert!(
            !write.status.success(),
            "write unexpectedly succeeded on read-only mount"
        );

        assert!(execute_mount_namespace(
            &container_id,
            pid,
            None,
            None,
            &target,
            MountAction::Unmount {
                expected_mount_id: wrong_mount_id,
            },
        )
        .is_err());
        assert!(mountinfo_by_id(pid, before.id)
            .expect("verify identity mismatch left mount intact")
            .is_some());
        execute_mount_namespace(
            &container_id,
            pid,
            None,
            None,
            &target,
            MountAction::Unmount {
                expected_mount_id: before.id,
            },
        )
        .expect("unmount from target namespace");
        assert!(mountinfo_by_id(pid, before.id)
            .expect("read mount table after unmount")
            .is_none());
        assert!(mountinfo_by_target(pid, &target)
            .expect("verify target no longer mounted")
            .is_none());

        // Exercise the same owned-lease cleanup helper called by
        // DISCONNECT against a real mount inside this disposable container
        // namespace. This is stronger than testing only registry selection:
        // cleanup must remove the exact kernel mount and persist lease removal.
        execute_mount_namespace(
            &container_id,
            pid,
            Some(source.as_raw_fd()),
            Some(&canonical_source),
            &target,
            MountAction::Bind { read_only: true },
        )
        .expect("recreate mount for disconnect-cleanup integration");
        let disconnect_mount = mountinfo_by_target(pid, &target)
            .expect("inspect mount before disconnect cleanup")
            .expect("mount must exist before disconnect cleanup");
        let disconnect_state_dir =
            std::env::temp_dir().join(format!("gitrun-disconnect-live-cleanup-{suffix}"));
        let _disconnect_state_guard = TemporaryDirectory(disconnect_state_dir.clone());
        let disconnect_source_metadata = fs::metadata(&canonical_source).unwrap();
        let disconnect_record = MountRecord {
            id: "gm-disconnect-live-cleanup".into(),
            repository: "owner/repo".into(),
            run_id: 73,
            job: "integration".into(),
            requester_runner: "integration-runner".into(),
            container: name.clone(),
            container_id: container_id.clone(),
            source: canonical_source.to_string_lossy().into_owned(),
            target: target.clone(),
            read_only: true,
            source_device: Some(disconnect_source_metadata.dev()),
            source_inode: Some(disconnect_source_metadata.ino()),
            kernel_mount_id: Some(disconnect_mount.id),
            state: "mounted".into(),
            created_at: now(),
        };
        let mut disconnect_registry = MountRegistry::default();
        disconnect_registry.mounts.push(disconnect_record);
        disconnect_registry
            .save(&disconnect_state_dir)
            .expect("persist simulated disconnect lease");
        cleanup_owned_mounts_for_disconnect(
            &mut disconnect_registry,
            &disconnect_state_dir,
            MountCleanupOwner {
                repository: "owner/repo",
                run_id: 73,
                job: "integration",
                container_id: &container_id,
                requester_runner: "integration-runner",
            },
            cleanup_mount_record,
        )
        .expect("disconnect helper should unmount the owned lease");
        assert!(mountinfo_by_id(pid, disconnect_mount.id)
            .expect("verify disconnect removed exact mount ID")
            .is_none());
        assert!(mountinfo_by_target(pid, &target)
            .expect("verify disconnect left target unmounted")
            .is_none());
        assert!(
            MountRegistry::load(&disconnect_state_dir)
                .expect("reload disconnect registry")
                .mounts
                .is_empty(),
            "disconnect cleanup must durably remove the cleaned lease"
        );

        // Simulate a service crash on both sides of the bind syscall. An
        // intent-only lease with no mount at its target should be safely
        // discarded; a lease pointing at an unrelated source must never
        // unmount a live mount, even when a kernel mount ID is present.
        let source_metadata = fs::metadata(&canonical_source).unwrap();
        let interrupted_lease = MountRecord {
            id: "gm-interrupted-recovery-test".into(),
            repository: "owner/repo".into(),
            run_id: 1,
            job: "test".into(),
            requester_runner: "test-runner".into(),
            container: name.clone(),
            container_id: container_id.clone(),
            source: canonical_source.to_string_lossy().into_owned(),
            target: target.clone(),
            read_only: true,
            source_device: Some(source_metadata.dev()),
            source_inode: Some(source_metadata.ino()),
            kernel_mount_id: None,
            state: "preparing".into(),
            created_at: now(),
        };
        cleanup_mount_lease_in_namespace(pid, &interrupted_lease)
            .expect("clean intent-only lease without a mount");
        assert!(mountinfo_by_target(pid, &target)
            .expect("verify no mount was created")
            .is_none());

        // A different actor can create a mount at the same path after a
        // "preparing" lease was persisted. Even when it exposes the same
        // source inode, that lease proves GitRun never entered the attach
        // worker and must not be allowed to unmount the other actor's mount.
        execute_mount_namespace(
            &container_id,
            pid,
            Some(source.as_raw_fd()),
            Some(&canonical_source),
            &target,
            MountAction::Bind { read_only: true },
        )
        .expect("create competing mount at preparing lease target");
        let competing_mount = mountinfo_by_target(pid, &target)
            .expect("inspect competing mount")
            .expect("competing mount should be present");
        cleanup_mount_lease_in_namespace(pid, &interrupted_lease)
            .expect("preparing lease must not unmount a mount created by another actor");
        assert!(mountinfo_by_id(pid, competing_mount.id)
            .expect("verify preparing lease left competing mount intact")
            .is_some());
        execute_mount_namespace(
            &container_id,
            pid,
            None,
            None,
            &target,
            MountAction::Unmount {
                expected_mount_id: competing_mount.id,
            },
        )
        .expect("remove competing mount created by the test");
        assert!(mountinfo_by_target(pid, &target)
            .expect("verify competing mount was removed")
            .is_none());

        // Once the durable state says the privileged attach may have begun,
        // absence at the original pathname is ambiguous (for example, the
        // target may have been renamed). Never discard this lease automatically.
        let mut attaching_lease = interrupted_lease.clone();
        attaching_lease.state = "attaching".into();
        assert!(
            cleanup_mount_lease_in_namespace(pid, &attaching_lease).is_err(),
            "an attaching lease without a discoverable mount must be preserved"
        );

        execute_mount_namespace(
            &container_id,
            pid,
            Some(source.as_raw_fd()),
            Some(&canonical_source),
            &target,
            MountAction::Bind { read_only: true },
        )
        .expect("recreate read-only mount for interrupted-lease recovery");
        let recovered_candidate = mountinfo_by_target(pid, &target)
            .expect("inspect recreated interrupted mount")
            .expect("recreated source bind should be present");
        let mut wrong_identity_lease = attaching_lease.clone();
        wrong_identity_lease.kernel_mount_id = Some(recovered_candidate.id);
        wrong_identity_lease.source_inode = Some(source_metadata.ino().wrapping_add(1));
        assert!(cleanup_mount_lease_in_namespace(pid, &wrong_identity_lease).is_err());
        assert!(mountinfo_by_id(pid, recovered_candidate.id)
            .expect("verify source mismatch did not unmount")
            .is_some());

        cleanup_mount_lease_in_namespace(pid, &attaching_lease)
            .expect("recover incomplete mount by exact target and source identity");
        assert!(mountinfo_by_target(pid, &target)
            .expect("read mount table after interrupted-lease recovery")
            .is_none());

        // Also exercise a regular-file source/target. The source path is a
        // host-only path, so this catches regressions in namespace handoff
        // that a directory-only test can miss.
        let source_file = source_dir.join("source-file.txt");
        fs::write(&source_file, b"file-payload\n").expect("write host source file");
        let target_file = format!("{target}-file.txt");
        let prepare_file_target = Command::new("docker")
            .args([
                "exec",
                name.as_str(),
                "sh",
                "-c",
                &format!("printf initial > '{target_file}'"),
            ])
            .output()
            .expect("create file mount target");
        assert!(
            prepare_file_target.status.success(),
            "could not create file mount target: {}",
            String::from_utf8_lossy(&prepare_file_target.stderr)
        );
        validate_mount_target(pid, &target_file, false)
            .expect("validate regular-file mount target");
        assert!(validate_mount_target(pid, &target_file, true).is_err());
        let canonical_source_file = fs::canonicalize(&source_file).unwrap();
        let source_file_handle =
            open_mount_source(&canonical_source_file).expect("open canonical host source file");
        execute_mount_namespace(
            &container_id,
            pid,
            Some(source_file_handle.as_raw_fd()),
            Some(&canonical_source_file),
            &target_file,
            MountAction::Bind { read_only: false },
        )
        .expect("bind regular file into target namespace");
        let file_read = Command::new("docker")
            .args(["exec", name.as_str(), "cat", target_file.as_str()])
            .output()
            .expect("read mounted file from target container");
        assert!(
            file_read.status.success(),
            "mounted regular file is not readable: {}",
            String::from_utf8_lossy(&file_read.stderr)
        );
        assert_eq!(file_read.stdout, b"file-payload\n");
        let file_write = Command::new("docker")
            .args([
                "exec",
                name.as_str(),
                "sh",
                "-c",
                &format!("printf updated > '{target_file}'"),
            ])
            .output()
            .expect("write to read-write file mount");
        assert!(
            file_write.status.success(),
            "write unexpectedly failed on read-write mount: {}",
            String::from_utf8_lossy(&file_write.stderr)
        );
        assert_eq!(fs::read_to_string(&source_file).unwrap(), "updated");
        let file_mount = mountinfo_by_target(pid, &target_file)
            .expect("read file mount table")
            .expect("file bind mount should appear");
        execute_mount_namespace(
            &container_id,
            pid,
            None,
            None,
            &target_file,
            MountAction::Unmount {
                expected_mount_id: file_mount.id,
            },
        )
        .expect("unmount regular file from target namespace");
        assert!(mountinfo_by_id(pid, file_mount.id)
            .expect("read mount table after file unmount")
            .is_none());
        assert!(mountinfo_by_target(pid, &target_file)
            .expect("verify file target no longer mounted")
            .is_none());

        drop(_nested_source_mount_guard);
        drop(_nested_overlay_guard);
        drop(container_guard);
        drop(source_guard);
    }
}

fn kitty_dispatch_plan_result(
    mode: &str,
    kitty_online: bool,
    status: &str,
    reason: &str,
    runs_on: &[String],
) -> Result<ExecutionResult, ExecutionError> {
    let encoded_runs_on = serde_json::to_string(runs_on)
        .map_err(|error| failed(format!("encode Kitty dispatch runs-on labels: {error}")))?;
    Ok(success(format!(
        "GITRUN_DISPATCH_STATUS={status}\nGITRUN_DISPATCH_MODE={mode}\nGITRUN_KITTY_ONLINE={kitty_online}\nGITRUN_DISPATCH_RUNS_ON={encoded_runs_on}\nGITRUN_DISPATCH_REASON={reason}\n"
    )))
}

fn new_kitty_dispatch_route_id() -> Option<String> {
    #[cfg(unix)]
    {
        use std::fmt::Write as _;
        let mut bytes = [0u8; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut bytes))
            .ok()?;
        let mut encoded = String::with_capacity(32);
        for byte in bytes {
            write!(&mut encoded, "{byte:02x}").ok()?;
        }
        Some(encoded)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn success(stdout: String) -> ExecutionResult {
    ExecutionResult {
        exit_code: 0,
        stdout,
        stderr: String::new(),
    }
}

fn failed(message: impl Into<String>) -> ExecutionError {
    ExecutionError::Failed(message.into())
}

fn arg<'a>(request: &'a AuthorizedOperation, name: &str) -> Result<&'a str, ExecutionError> {
    request
        .arguments
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| failed(format!("missing argument: {name}")))
}

fn find_dock_melt_source_binding<'a>(
    registry: &'a DockRegistry,
    repository: &str,
    run_id: u64,
    current_job: &str,
    source_container: &str,
) -> Option<&'a DockBinding> {
    if run_id == 0 || source_container.trim().is_empty() {
        return None;
    }
    registry.bindings.iter().find(|binding| {
        binding.repository == repository
            && binding.run_id == run_id
            && binding.job != current_job
            && binding.container == source_container
            && !binding.container_id.is_empty()
    })
}

fn connect_reuses_container_from_prior_binding(
    previous: &DockBinding,
    repository: &str,
    run_id: u64,
    job: &str,
    requester_runner: &str,
    container: &str,
    container_id: &str,
) -> bool {
    previous.container == container
        && previous.container_id == container_id
        && (previous.repository != repository
            || previous.run_id != run_id
            || previous.job != job
            || previous.requester_runner != requester_runner)
}

fn find_dock_melt_target_binding<'a>(
    registry: &'a DockRegistry,
    repository: &str,
    run_id: u64,
    target_container: &str,
    source_container_id: &str,
) -> Option<&'a DockBinding> {
    if run_id == 0 || target_container.trim().is_empty() || source_container_id.trim().is_empty() {
        return None;
    }
    registry.bindings.iter().find(|binding| {
        binding.repository == repository
            && binding.run_id == run_id
            && binding.container == target_container
            && !binding.container_id.is_empty()
            && binding.container_id != source_container_id
            && !binding.requester_runner.trim().is_empty()
    })
}

fn validate_dock_melt_target_identity(
    resolved_container_id: &str,
    bound_container_id: &str,
) -> Result<(), &'static str> {
    if !resolved_container_id.is_empty() && resolved_container_id == bound_container_id {
        Ok(())
    } else {
        Err("GitDockRun --melt target is not the container recorded by its binding")
    }
}

fn validate_dock_binding_owner(
    binding: &DockBinding,
    requester_runner: &str,
) -> Result<(), &'static str> {
    if binding.requester_runner == requester_runner {
        Ok(())
    } else {
        Err("GitDockRun binding belongs to a different requester runner")
    }
}

fn request_run_id(request: &AuthorizedOperation) -> u64 {
    request.run_id.unwrap_or(0)
}

fn dock_melt_source_status_allowed(status: &str, dock_only_marker_present: bool) -> bool {
    status == "exited" || (status == "running" && dock_only_marker_present)
}

fn validate_dock_connect_caller_identity(
    caller_runner: &str,
    caller_container_id: &str,
    job_runner: &str,
    job_container_id: &str,
) -> Result<(), &'static str> {
    if job_runner != caller_runner {
        return Err("GitDockRun --connect runner does not match the authenticated caller");
    }
    if job_container_id != caller_container_id {
        return Err("GitDockRun --connect container does not match the authenticated caller");
    }
    Ok(())
}

fn resolve_dock_connect_dynamic(
    runner_name: &str,
    container_id: &str,
    is_permanent: impl FnOnce(&str) -> Result<bool, String>,
    status: impl FnOnce(&str) -> Result<Option<String>, String>,
) -> Result<bool, String> {
    let dynamic = !is_permanent(container_id)?;
    if dynamic && status(container_id)?.is_none() {
        return Err(format!(
            "runner container {runner_name} disappeared during connect"
        ));
    }
    Ok(dynamic)
}

#[cfg(test)]
mod dock_melt_binding_tests {
    use super::{
        find_dock_melt_source_binding, find_dock_melt_target_binding,
        validate_dock_melt_target_identity, DockBinding, DockRegistry,
    };

    fn binding(job: &str, container: &str, runner: &str) -> DockBinding {
        DockBinding {
            repository: "owner/repo".into(),
            run_id: 42,
            job: job.into(),
            container: container.into(),
            container_id: format!("test-id-{container}"),
            dynamic: true,
            dock_only: job == "build",
            requester_runner: runner.into(),
            connected_at: 1,
        }
    }

    #[test]
    fn melt_can_select_a_completed_job_source_in_the_same_workflow_run() {
        let registry = DockRegistry {
            bindings: vec![
                binding("build", "build-runner", "runner-build"),
                binding("publish", "publish-runner", "runner-publish"),
            ],
        };

        let source =
            find_dock_melt_source_binding(&registry, "owner/repo", 42, "publish", "build-runner")
                .expect("prior completed job source must be selectable");

        assert_eq!(source.job, "build");
        assert_eq!(source.container_id, "test-id-build-runner");
    }

    #[test]
    fn melt_can_select_an_explicit_target_from_another_active_job_in_the_same_run() {
        let registry = DockRegistry {
            bindings: vec![
                binding("build", "build-runner", "build-runner"),
                binding("publish", "publish-runner", "publish-runner"),
            ],
        };

        let target = find_dock_melt_target_binding(
            &registry,
            "owner/repo",
            42,
            "publish-runner",
            "test-id-build-runner",
        )
        .expect("configured same-run target must be selectable");

        assert_eq!(target.job, "publish");
        assert_eq!(target.container_id, "test-id-publish-runner");
    }

    #[test]
    fn melt_rejects_target_container_name_reuse_against_bound_identity() {
        assert_eq!(
            validate_dock_melt_target_identity("replacement-container-id", "bound-container-id"),
            Err("GitDockRun --melt target is not the container recorded by its binding")
        );
        assert_eq!(
            validate_dock_melt_target_identity("", "bound-container-id"),
            Err("GitDockRun --melt target is not the container recorded by its binding")
        );
    }

    #[test]
    fn melt_accepts_the_exact_bound_target_identity() {
        assert_eq!(
            validate_dock_melt_target_identity("bound-container-id", "bound-container-id"),
            Ok(())
        );
    }

    #[test]
    fn melt_target_lookup_rejects_other_runs_repositories_and_source_identity() {
        let registry = DockRegistry {
            bindings: vec![
                binding("build", "build-runner", "build-runner"),
                binding("publish", "publish-runner", "publish-runner"),
            ],
        };

        assert!(find_dock_melt_target_binding(
            &registry,
            "owner/repo",
            99,
            "publish-runner",
            "test-id-build-runner"
        )
        .is_none());
        assert!(find_dock_melt_target_binding(
            &registry,
            "other/repo",
            42,
            "publish-runner",
            "test-id-build-runner"
        )
        .is_none());
        assert!(find_dock_melt_target_binding(
            &registry,
            "owner/repo",
            42,
            "build-runner",
            "test-id-build-runner"
        )
        .is_none());
    }

    #[test]
    fn melt_source_lookup_rejects_current_job_other_runs_and_other_repositories() {
        let registry = DockRegistry {
            bindings: vec![
                binding("build", "build-runner", "runner-build"),
                binding("publish", "publish-runner", "runner-publish"),
            ],
        };

        assert!(find_dock_melt_source_binding(
            &registry,
            "owner/repo",
            42,
            "build",
            "build-runner"
        )
        .is_none());
        assert!(find_dock_melt_source_binding(
            &registry,
            "owner/repo",
            99,
            "publish",
            "build-runner"
        )
        .is_none());
        assert!(find_dock_melt_source_binding(
            &registry,
            "other/repo",
            42,
            "publish",
            "build-runner"
        )
        .is_none());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod dock_melt_mount_isolation_tests {
    use super::{validate_melt_target_has_no_mount_leases, MountRecord, MountRegistry};

    fn mount_record(container_id: &str) -> MountRecord {
        MountRecord {
            id: "mount-test".into(),
            repository: "owner/repo".into(),
            run_id: 42,
            job: "build".into(),
            requester_runner: "runner-a".into(),
            container: "runner-a".into(),
            container_id: container_id.into(),
            source: "/srv/approved".into(),
            target: "/workspace/cache".into(),
            read_only: false,
            source_device: Some(1),
            source_inode: Some(2),
            kernel_mount_id: Some(3),
            state: "mounted".into(),
            created_at: 1,
        }
    }

    #[test]
    fn melt_refuses_target_with_outstanding_gitdock_mount_lease() {
        let registry = MountRegistry {
            mounts: vec![mount_record("target-container-id")],
        };
        let error = validate_melt_target_has_no_mount_leases(&registry, "target-container-id")
            .expect_err("MELT must not extract through an outstanding host bind mount");
        assert!(error.contains("unmount first"));
    }

    #[test]
    fn melt_allows_target_without_its_own_mount_leases() {
        let registry = MountRegistry {
            mounts: vec![mount_record("different-container-id")],
        };
        assert!(validate_melt_target_has_no_mount_leases(&registry, "target-container-id").is_ok());
        assert!(validate_melt_target_has_no_mount_leases(
            &MountRegistry::default(),
            "target-container-id"
        )
        .is_ok());
    }
}

#[cfg(test)]
mod dock_connect_identity_tests {
    use super::{
        connect_reuses_container_from_prior_binding, resolve_dock_connect_dynamic,
        validate_dock_connect_caller_identity, DockBinding,
    };
    use std::cell::RefCell;

    #[test]
    fn connect_rejects_runner_assignment_changed_after_caller_verification() {
        assert_eq!(
            validate_dock_connect_caller_identity(
                "runner-a",
                "container-a",
                "runner-b",
                "container-b",
            ),
            Err("GitDockRun --connect runner does not match the authenticated caller")
        );
    }

    #[test]
    fn connect_rejects_container_name_reuse_after_caller_verification() {
        assert_eq!(
            validate_dock_connect_caller_identity(
                "runner-a",
                "container-original",
                "runner-a",
                "container-replacement",
            ),
            Err("GitDockRun --connect container does not match the authenticated caller")
        );
    }

    #[test]
    fn connect_accepts_exact_authenticated_runner_identity() {
        assert_eq!(
            validate_dock_connect_caller_identity(
                "runner-a",
                "container-a",
                "runner-a",
                "container-a",
            ),
            Ok(())
        );
    }

    #[test]
    fn connect_detects_reused_persistent_container_with_previous_job_binding() {
        let previous = DockBinding {
            repository: "owner/repo".into(),
            run_id: 41,
            job: "build".into(),
            container: "runner-a".into(),
            container_id: "container-a".into(),
            dynamic: false,
            dock_only: false,
            requester_runner: "runner-a".into(),
            connected_at: 1,
        };

        assert!(connect_reuses_container_from_prior_binding(
            &previous,
            "owner/repo",
            42,
            "publish",
            "runner-a",
            "runner-a",
            "container-a",
        ));
        assert!(!connect_reuses_container_from_prior_binding(
            &previous,
            "owner/repo",
            41,
            "build",
            "runner-a",
            "runner-a",
            "container-a",
        ));
        assert!(!connect_reuses_container_from_prior_binding(
            &previous,
            "owner/repo",
            42,
            "publish",
            "runner-a",
            "runner-a",
            "container-replacement",
        ));
    }

    #[test]
    fn connect_lifecycle_metadata_uses_pinned_container_id() {
        let inspected_ids = RefCell::new(Vec::new());
        let pinned_id = "test-container-id-pinned";

        let dynamic = resolve_dock_connect_dynamic(
            "runner-name",
            pinned_id,
            |id| {
                inspected_ids.borrow_mut().push(id.to_owned());
                Ok(false)
            },
            |id| {
                inspected_ids.borrow_mut().push(id.to_owned());
                Ok(Some("running".to_owned()))
            },
        )
        .unwrap();

        assert!(dynamic);
        assert_eq!(
            inspected_ids.borrow().as_slice(),
            &[pinned_id.to_owned(), pinned_id.to_owned()]
        );
    }

    #[test]
    fn connect_fails_closed_when_pinned_dynamic_container_disappears() {
        let result = resolve_dock_connect_dynamic(
            "runner-name",
            "test-container-id-pinned",
            |_| Ok(false),
            |_| Ok(None),
        );

        assert_eq!(
            result,
            Err("runner container runner-name disappeared during connect".to_owned())
        );
    }
}

#[cfg(test)]
mod dock_disconnect_owner_tests {
    use super::{validate_dock_binding_owner, DockBinding};

    fn binding(requester_runner: &str) -> DockBinding {
        DockBinding {
            repository: "owner/repo".into(),
            run_id: 42,
            job: "build".into(),
            container: "runner-a".into(),
            container_id: "test-container-id-runner-a".into(),
            dynamic: true,
            dock_only: false,
            requester_runner: requester_runner.into(),
            connected_at: 1,
        }
    }

    #[test]
    fn disconnect_owner_must_match_binding_requester() {
        assert!(validate_dock_binding_owner(&binding("runner-a"), "runner-a").is_ok());
        assert_eq!(
            validate_dock_binding_owner(&binding("runner-a"), "runner-b"),
            Err("GitDockRun binding belongs to a different requester runner")
        );
    }
}

#[cfg(test)]
mod dock_melt_source_status_tests {
    use super::dock_melt_source_status_allowed;

    #[test]
    fn melt_allows_only_exited_or_explicitly_dock_only_sources() {
        assert!(dock_melt_source_status_allowed("exited", false));
        assert!(dock_melt_source_status_allowed("running", true));
        for status in [
            "created",
            "paused",
            "restarting",
            "removing",
            "dead",
            "unknown",
            "",
        ] {
            assert!(
                !dock_melt_source_status_allowed(status, false),
                "unexpectedly allowed source status {status:?}"
            );
        }
        assert!(!dock_melt_source_status_allowed("running", false));
        assert!(!dock_melt_source_status_allowed("paused", true));
    }
}

fn command_output(output: std::process::Output) -> Result<ExecutionResult, ExecutionError> {
    Ok(ExecutionResult {
        exit_code: output.status.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Intentionally privileged container execution, reserved for the separately
/// authorized GitInstallRun operation. Callers must never use this for paths,
/// workflow commands or ordinary runner I/O.
fn docker_exec_checked_as_container_root(
    container: &str,
    args: &[&str],
) -> Result<ExecutionResult, ExecutionError> {
    let output =
        docker::exec_container(container, args).map_err(|error| failed(error.to_string()))?;
    command_output(output)
}

const GITRUN_SAFE_FILE_HELPER: &str = r#"import hashlib
import os
import secrets
import stat
import sys

MAX_READ_BYTES = 16 * 1024 * 1024
MAX_INPUT_BYTES = 16 * 1024 * 1024
MAX_HASH_BYTES = 1024 * 1024 * 1024
DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
FILE_FLAGS = os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
ROOTS = (
    ("home", "runner", "actions-runner", "_work"),
    ("tmp",),
    ("var", "lib", "gitrun", "shared"),
    ("workspace",),
)

def checked_parts(path):
    if not path.startswith("/") or path.startswith("//") or path.endswith("/") or "\x00" in path:
        raise ValueError("path must be a clean absolute file path")
    parts = tuple(path[1:].split("/"))
    if not parts or any(part in ("", ".", "..") for part in parts):
        raise ValueError("path has an empty or traversal component")
    return parts

def open_directory(parts):
    fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for part in parts:
            next_fd = os.open(part, DIR_FLAGS, dir_fd=fd)
            os.close(fd)
            fd = next_fd
        return fd
    except Exception:
        os.close(fd)
        raise

def matching_root(parts, allow_root=False):
    for root in ROOTS:
        if parts[:len(root)] == root and (len(parts) > len(root) or (allow_root and len(parts) == len(root))):
            return root
    raise PermissionError("path is outside the GitRun workflow filesystem allowlist")

def open_parent(path, create_parents=False):
    parts = checked_parts(path)
    root = matching_root(parts)
    fd = open_directory(root)
    try:
        for part in parts[len(root):-1]:
            try:
                next_fd = os.open(part, DIR_FLAGS, dir_fd=fd)
            except FileNotFoundError:
                if not create_parents:
                    raise
                try:
                    os.mkdir(part, 0o755, dir_fd=fd)
                except FileExistsError:
                    pass
                next_fd = os.open(part, DIR_FLAGS, dir_fd=fd)
            os.close(fd)
            fd = next_fd
        return fd, parts[-1]
    except Exception:
        os.close(fd)
        raise

def open_regular(path, flags, mode=0o666):
    parent_fd, name = open_parent(path)
    try:
        fd = os.open(name, flags | FILE_FLAGS, mode, dir_fd=parent_fd)
    finally:
        os.close(parent_fd)
    try:
        metadata = os.fstat(fd)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
            raise ValueError("target must be a regular file without hard links")
        return fd, metadata
    except Exception:
        os.close(fd)
        raise

def make_directories(path):
    parts = checked_parts(path)
    root = matching_root(parts, allow_root=True)
    fd = open_directory(root)
    try:
        for part in parts[len(root):]:
            try:
                next_fd = os.open(part, DIR_FLAGS, dir_fd=fd)
            except FileNotFoundError:
                try:
                    os.mkdir(part, 0o755, dir_fd=fd)
                except FileExistsError:
                    pass
                next_fd = os.open(part, DIR_FLAGS, dir_fd=fd)
            os.close(fd)
            fd = next_fd
    finally:
        os.close(fd)

def write_all(fd, data):
    view = memoryview(data)
    while view:
        written = os.write(fd, view)
        if written <= 0:
            raise OSError("short write")
        view = view[written:]

def safe_read(path):
    fd, metadata = open_regular(path, os.O_RDONLY)
    try:
        if metadata.st_size > MAX_READ_BYTES:
            raise ValueError("file exceeds the 16 MiB GitRun read limit")
        data = bytearray()
        while True:
            chunk = os.read(fd, 65536)
            if not chunk:
                break
            data.extend(chunk)
            if len(data) > MAX_READ_BYTES:
                raise ValueError("file grew beyond the 16 MiB GitRun read limit")
        sys.stdout.buffer.write(data)
    finally:
        os.close(fd)

def safe_write(path):
    data = sys.stdin.buffer.read(MAX_INPUT_BYTES + 1)
    if len(data) > MAX_INPUT_BYTES:
        raise ValueError("input exceeds the 16 MiB GitRun write limit")
    fd, _ = open_regular(path, os.O_WRONLY | os.O_CREAT)
    try:
        os.ftruncate(fd, 0)
        write_all(fd, data)
        os.fsync(fd)
    finally:
        os.close(fd)

def safe_size(path):
    fd, metadata = open_regular(path, os.O_RDONLY)
    try:
        print(metadata.st_size)
    finally:
        os.close(fd)

def safe_sha256(path):
    fd, metadata = open_regular(path, os.O_RDONLY)
    try:
        if metadata.st_size > MAX_HASH_BYTES:
            raise ValueError("file exceeds the 1 GiB GitRun verification limit")
        digest = hashlib.sha256()
        total = 0
        while True:
            chunk = os.read(fd, 65536)
            if not chunk:
                break
            total += len(chunk)
            if total > MAX_HASH_BYTES:
                raise ValueError("file grew beyond the 1 GiB GitRun verification limit")
            digest.update(chunk)
        print(digest.hexdigest() + "  " + path)
    finally:
        os.close(fd)

def safe_copy(source, destination, limit):
    limit = int(limit)
    if limit < 0:
        raise ValueError("copy size limit must not be negative")
    source_fd, source_metadata = open_regular(source, os.O_RDONLY)
    destination_fd = None
    destination_parent = None
    temporary_name = None
    try:
        if source_metadata.st_size > limit:
            raise ValueError("GitSaveRun file exceeds repository storage limit")
        destination_parent_path = destination.rsplit("/", 1)[0]
        make_directories(destination_parent_path)
        destination_parent, destination_name = open_parent(destination)
        temporary_name = ".gitrun-copy-" + str(os.getpid()) + "-" + secrets.token_hex(8)
        destination_fd = os.open(
            temporary_name,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | FILE_FLAGS,
            0o600,
            dir_fd=destination_parent,
        )
        total = 0
        while True:
            chunk = os.read(source_fd, 65536)
            if not chunk:
                break
            total += len(chunk)
            if total > limit:
                raise ValueError("GitSaveRun source grew beyond repository storage limit")
            write_all(destination_fd, chunk)
        os.fchmod(destination_fd, source_metadata.st_mode & 0o777 & ~0o022)
        os.fsync(destination_fd)
        os.close(destination_fd)
        destination_fd = None
        os.rename(
            temporary_name,
            destination_name,
            src_dir_fd=destination_parent,
            dst_dir_fd=destination_parent,
        )
        temporary_name = None
        os.fsync(destination_parent)
        print(total)
    finally:
        os.close(source_fd)
        if destination_fd is not None:
            os.close(destination_fd)
        if destination_parent is not None:
            if temporary_name is not None:
                try:
                    os.unlink(temporary_name, dir_fd=destination_parent)
                except FileNotFoundError:
                    pass
            os.close(destination_parent)

def main():
    args = sys.argv[1:]
    if not args:
        raise ValueError("missing operation")
    operation = args[0]
    if operation in ("read", "write", "size", "sha256", "mkdir") and len(args) == 2:
        path = args[1]
        if operation == "read":
            safe_read(path)
        elif operation == "write":
            safe_write(path)
        elif operation == "size":
            safe_size(path)
        elif operation == "sha256":
            safe_sha256(path)
        else:
            make_directories(path)
    elif operation == "copy" and len(args) == 4:
        safe_copy(args[1], args[2], args[3])
    else:
        raise ValueError("invalid GitRun file operation arguments")

try:
    main()
except Exception as error:
    print("gitrun-file: " + str(error), file=sys.stderr)
    sys.exit(2)"#;

fn docker_secure_file_operation_as_runner(
    container: &str,
    operation_args: &[&str],
) -> Result<std::process::Output, ExecutionError> {
    let mut args = vec!["python3", "-I", "-S", "-c", GITRUN_SAFE_FILE_HELPER];
    args.extend_from_slice(operation_args);
    docker::exec_container_as_runner(container, &args).map_err(|error| failed(error.to_string()))
}

fn docker_secure_file_operation_with_stdin_as_runner(
    container: &str,
    operation_args: &[&str],
    input: &[u8],
) -> Result<std::process::Output, ExecutionError> {
    let mut args = vec!["python3", "-I", "-S", "-c", GITRUN_SAFE_FILE_HELPER];
    args.extend_from_slice(operation_args);
    docker::exec_container_with_stdin_as_runner(container, &args, input)
        .map_err(|error| failed(error.to_string()))
}

fn docker_file_size(container: &str, path: &str) -> Result<u64, ExecutionError> {
    let output = docker_secure_file_operation_as_runner(container, &["size", path])?;
    if !output.status.success() {
        return Err(failed(format!(
            "GitRun could not safely inspect source file size: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|_| failed("GitRun source file size was invalid"))
}

fn ensure_safe_runner_path(path: &str) -> Result<(), ExecutionError> {
    if path.trim().is_empty()
        || path.chars().any(|c| c.is_control())
        || Path::new(path)
            .components()
            .any(|component| component == Component::ParentDir)
    {
        return Err(failed("path is not safe for a GitRun filesystem operation"));
    }

    // Workflow file APIs need the checkout, temp files and explicitly shared
    // data, not the Action runner's registration credentials under its parent
    // directory. The secure in-container helper enforces this again while
    // traversing every path component with O_NOFOLLOW and directory FDs.
    let allowed = [
        "/home/runner/actions-runner/_work/",
        "/tmp/",
        "/var/lib/gitrun/shared/",
        "/workspace/",
    ];
    if !allowed.iter().any(|prefix| path.starts_with(prefix)) {
        return Err(failed(
            "path is outside the GitRun workflow filesystem allowlist",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[cfg(test)]
mod secure_runner_file_helper_tests {
    use super::{ensure_safe_runner_path, GITRUN_SAFE_FILE_HELPER};
    use std::io::Write;
    use std::process::{Command, Output, Stdio};

    fn invoke(operation: &str, args: &[&str], input: &[u8]) -> Output {
        let mut command = Command::new("python3");
        command
            .args(["-I", "-S", "-c", GITRUN_SAFE_FILE_HELPER, operation])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .expect("python3 should be installed in the Linux CI environment");
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(input)
            .expect("write helper input");
        child
            .wait_with_output()
            .expect("wait for secure file helper")
    }

    #[test]
    fn path_allowlist_excludes_runner_registration_state() {
        assert!(ensure_safe_runner_path("/home/runner/actions-runner/_work/repo/file.txt").is_ok());
        assert!(ensure_safe_runner_path("/tmp/build.log").is_ok());
        assert!(ensure_safe_runner_path("/var/lib/gitrun/shared/files/artifact").is_ok());
        assert!(ensure_safe_runner_path("/home/runner/actions-runner/.credentials").is_err());
        assert!(ensure_safe_runner_path("/home/runner/actions-runner/.runner").is_err());
        assert!(ensure_safe_runner_path("/etc/passwd").is_err());
        assert!(
            ensure_safe_runner_path("/tmp/../home/runner/actions-runner/.credentials").is_err()
        );
    }

    fn exercise_secure_file_helper(directory: &std::path::Path) -> Result<(), String> {
        use std::os::unix::fs::symlink;

        let file = directory.join("safe.txt");
        let file_arg = file.to_str().expect("UTF-8 test path");
        let written = invoke("write", &[file_arg], b"safe contents");
        assert!(
            written.status.success(),
            "write failed: {}",
            String::from_utf8_lossy(&written.stderr)
        );
        let read = invoke("read", &[file_arg], b"");
        assert!(
            read.status.success(),
            "read failed: {}",
            String::from_utf8_lossy(&read.stderr)
        );
        assert_eq!(read.stdout, b"safe contents");

        let file_link = directory.join("file-link");
        symlink("/etc/passwd", &file_link).expect("create file symlink");
        let linked = invoke("read", &[file_link.to_str().unwrap()], b"");
        assert!(
            !linked.status.success(),
            "final symlink unexpectedly accepted"
        );
        assert!(linked.stdout.is_empty(), "rejected read returned file data");

        let directory_link = directory.join("directory-link");
        symlink("/etc", &directory_link).expect("create directory symlink");
        let nested = directory_link.join("passwd");
        let linked_parent = invoke("read", &[nested.to_str().unwrap()], b"");
        assert!(
            !linked_parent.status.success(),
            "parent symlink unexpectedly accepted"
        );
        assert!(
            linked_parent.stdout.is_empty(),
            "rejected read returned file data"
        );

        let out_of_scope = invoke("read", &["/etc/passwd"], b"");
        assert!(
            !out_of_scope.status.success(),
            "out-of-scope path unexpectedly accepted"
        );
        assert!(
            out_of_scope.stdout.is_empty(),
            "rejected read returned file data"
        );
        Ok(())
    }

    #[test]
    fn secure_file_helper_rejects_symlink_components_and_out_of_scope_paths() {
        // The helper intentionally allows /tmp, not arbitrary TMPDIR locations.
        // Keep this test independent of runner-specific TMPDIR configuration.
        let directory = std::path::Path::new("/tmp").join(format!(
            "gitrun-secure-file-helper-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir(&directory).expect("create test directory");
        let result = exercise_secure_file_helper(&directory);
        let _ = std::fs::remove_dir_all(&directory);
        result.expect("secure file helper regression checks");
    }
}

fn ensure_safe_file_name(name: &str) -> Result<(), ExecutionError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 255
        || name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
    {
        return Err(failed("file name is not safe"));
    }
    Ok(())
}

fn ensure_safe_logical_name(name: &str) -> Result<(), ExecutionError> {
    if name.trim().is_empty()
        || name.len() > 128
        || name.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(failed("logical resource name is not safe"));
    }
    Ok(())
}

fn ensure_safe_package(value: &str) -> Result<(), ExecutionError> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+:-".contains(&byte))
    {
        return Err(failed(
            "package name/version contains unsupported characters",
        ));
    }
    Ok(())
}

/// Escapes GitHub Actions workflow-command data so a multiline secret
/// cannot terminate the add-mask command and leave later lines unmasked.
fn escape_workflow_command_data(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

fn redact_vault_values(config: &Config, repository: &str, text: &str) -> String {
    if config.vault_dir.trim().is_empty() {
        return text.to_owned();
    }

    let groups = config.vault_groups_for_repo(repository);
    let bridge = crate::gsr_bridge::VaultToGsrBridge::new(&config.state_dir);
    let vault = match Vault::open_with_sink(&config.vault_dir, Box::new(bridge)) {
        Ok(vault) => vault,
        // Do not emit raw output when secret redaction cannot be guaranteed.
        Err(_) => return "[output withheld: GitVault unavailable]".to_owned(),
    };
    let secrets = match vault.resolve_for_repo(repository, &groups) {
        Ok(secrets) => secrets,
        Err(_) => return "[output withheld: GitVault secret resolution failed]".to_owned(),
    };
    let mut result = text.to_owned();
    for (_, value) in secrets {
        if value.is_empty() {
            continue;
        }
        result = result.replace(&value, "***");
    }
    result
}

fn docker_container_has_marker(container: &str) -> Result<bool, ExecutionError> {
    let output = docker::exec_container(container, &["test", "-f", DOCK_ONLY_MARKER])
        .map_err(|error| failed(error.to_string()))?;
    Ok(output.status.success())
}

fn restore_runner(container: &str) -> Result<(), ExecutionError> {
    match docker::container_status(container).map_err(|error| failed(error.to_string()))? {
        Some(status) if status == "running" => {
            // Stop first: never expose a live runner after clearing its trusted
            // dock-only marker. If marker removal fails, the container stays
            // stopped and therefore cannot be scheduled as an ordinary runner.
            docker::stop_container(container).map_err(|error| failed(error.to_string()))?;
            docker::set_dock_only_marker(container, false)
                .map_err(|error| failed(error.to_string()))?;
            docker::start_container(container).map_err(|error| failed(error.to_string()))?;
        }
        Some(_) => {
            // An already-stopped runner cannot race ordinary scheduling while
            // its host-owned marker is cleared.
            docker::set_dock_only_marker(container, false)
                .map_err(|error| failed(error.to_string()))?;
            docker::start_container(container).map_err(|error| failed(error.to_string()))?;
        }
        None => return Err(failed("runner container disappeared during restore")),
    }
    Ok(())
}

fn make_dock_only(container: &str, _state_dir: &Path) -> Result<(), ExecutionError> {
    let status = docker::container_status(container)
        .map_err(|error| failed(error.to_string()))?
        .ok_or_else(|| failed("dock source container disappeared"))?;
    let was_running = status == "running";
    let mut marker_may_exist = false;

    let result = (|| {
        // The control volume is read-only from inside the container. Set the
        // marker through its host-side Docker volume mountpoint so neither the
        // runner user nor a container-root install helper can modify it.
        docker::set_dock_only_marker(container, true).map_err(|error| failed(error.to_string()))?;
        marker_may_exist = true;
        if was_running {
            docker::stop_container(container).map_err(|error| failed(error.to_string()))?;
        }

        docker::start_container(container).map_err(|error| failed(error.to_string()))?;
        Ok(())
    })();

    if let Err(error) = result {
        if marker_may_exist {
            let rollback = if was_running {
                restore_runner(container)
            } else {
                docker::set_dock_only_marker(container, false)
                    .map_err(|rollback_error| failed(rollback_error.to_string()))
            };
            if let Err(rollback_error) = rollback {
                return Err(failed(format!(
                    "make runner dock-only failed: {error}; rollback also failed: {rollback_error}"
                )));
            }
        }
        return Err(error);
    }
    Ok(())
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct RegisterRegistry {
    entries: Vec<RegisteredPath>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RegisteredPath {
    name: String,
    entry: String,
    repository: String,
    workflow: String,
    run_id: u64,
    permanent: bool,
    updated_at: u64,
}

fn validate_register_registry_parent(path: &Path) -> Result<(), ExecutionError> {
    let parent = path
        .parent()
        .ok_or_else(|| failed("register registry path has no parent directory"))?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(failed("register registry parent must be a real directory"));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o022 != 0 {
                    return Err(failed(
                        "register registry parent must not be group- or world-writable",
                    ));
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(failed(format!("inspect register registry parent: {error}"))),
    }
}

fn read_secure_register_registry(path: &Path) -> Result<Option<String>, ExecutionError> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(failed(format!("open register registry safely: {error}"))),
        };
        let metadata = file.metadata().map_err(|error| failed(error.to_string()))?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(failed(
                "register registry must be a regular file without hard links",
            ));
        }
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(failed(
                "register registry must not be group- or world-writable",
            ));
        }
        let mut raw = String::new();
        std::io::Read::read_to_string(&mut file, &mut raw)
            .map_err(|error| failed(format!("read register registry: {error}")))?;
        Ok(Some(raw))
    }
    #[cfg(not(unix))]
    {
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(failed(format!("open register registry: {error}"))),
        };
        if !file
            .metadata()
            .map_err(|error| failed(error.to_string()))?
            .is_file()
        {
            return Err(failed("register registry must be a regular file"));
        }
        let mut raw = String::new();
        std::io::Read::read_to_string(&mut file, &mut raw)
            .map_err(|error| failed(format!("read register registry: {error}")))?;
        Ok(Some(raw))
    }
}

impl RegisterRegistry {
    fn load(path: &Path) -> Result<Self, ExecutionError> {
        validate_register_registry_parent(path)?;
        let Some(raw) = read_secure_register_registry(path)? else {
            return Ok(Self::default());
        };
        serde_json::from_str(&raw)
            .map_err(|error| failed(format!("decode register registry: {error}")))
    }

    fn upsert(&mut self, entry: RegisteredPath) {
        self.entries.retain(|existing| {
            !(existing.name == entry.name
                && existing.repository == entry.repository
                && existing.workflow == entry.workflow
                && existing.run_id == entry.run_id)
        });
        if entry.permanent {
            self.entries
                .retain(|existing| existing.name != entry.name || !existing.permanent);
        }
        self.entries.push(entry);
        self.entries.sort_by(|a, b| a.name.cmp(&b.name));
    }

    fn save(&self, path: &Path) -> Result<(), ExecutionError> {
        let parent = path
            .parent()
            .ok_or_else(|| failed("register registry path has no parent directory"))?;
        fs::create_dir_all(parent)
            .map_err(|error| failed(format!("create register registry directory: {error}")))?;
        validate_register_registry_parent(path)?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let tmp = path.with_extension(format!("json.{}.{}.tmp", std::process::id(), nonce));
        let raw = serde_json::to_vec_pretty(self)
            .map_err(|error| failed(format!("encode register registry: {error}")))?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .map_err(|error| failed(format!("create register registry temp file: {error}")))?;
        if let Err(error) = file.write_all(&raw).and_then(|()| file.sync_all()) {
            let _ = fs::remove_file(&tmp);
            return Err(failed(format!("write register registry: {error}")));
        }
        if let Err(error) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(failed(format!("commit register registry: {error}")));
        }
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| failed(format!("sync register registry directory: {error}")))?;
        Ok(())
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

#[cfg(all(test, unix))]
mod api_request_capacity_tests {
    use super::*;

    #[test]
    fn request_permits_bound_concurrent_handlers_and_release_on_drop() {
        let mut permits = Vec::with_capacity(MAX_API_REQUESTS);
        for _ in 0..MAX_API_REQUESTS {
            permits.push(
                ApiRequestPermit::try_acquire()
                    .expect("every slot below the capacity limit should be available"),
            );
        }
        assert!(ApiRequestPermit::try_acquire().is_none());

        drop(permits);
        assert!(ApiRequestPermit::try_acquire().is_some());
    }

    #[test]
    fn request_reader_requires_newline_terminated_frame() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;

        let (server, mut client) = UnixStream::pair().unwrap();
        client.write_all(b"{}").unwrap();
        drop(client);

        let result = read_api_request_with_timeout(&server, Duration::from_secs(1));
        assert!(matches!(result, Err(message) if message.contains("newline terminator")));
    }

    #[test]
    fn request_reader_accepts_one_newline_terminated_frame() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;

        let (server, mut client) = UnixStream::pair().unwrap();
        client.write_all(b"{}\n").unwrap();

        assert_eq!(
            read_api_request_with_timeout(&server, Duration::from_secs(1)).unwrap(),
            Some(b"{}".to_vec())
        );
    }

    #[test]
    fn request_reader_rejects_oversized_frames_before_json_parsing() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;

        let (server, mut client) = UnixStream::pair().unwrap();
        let sender = std::thread::spawn(move || {
            let mut payload = vec![b'x'; API_MAX_REQUEST_BYTES + 1];
            payload.push(b'\n');
            let _ = client.write_all(&payload);
        });

        let result = read_api_request_with_timeout(&server, Duration::from_secs(3));
        assert!(matches!(result, Err(message) if message.contains("too large")));
        drop(server);
        let _ = sender.join();
    }

    #[test]
    fn request_reader_rejects_malformed_and_truncated_json_frames() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;

        for frame in [b"{not-json}\n".as_slice(), b"{\"invocation\":\n".as_slice()] {
            let (server, mut client) = UnixStream::pair().unwrap();
            client.write_all(frame).unwrap();

            let bytes = read_api_request_with_timeout(&server, Duration::from_secs(1))
                .expect("newline-terminated frame should be read")
                .expect("peer wrote a frame");
            assert!(serde_json::from_slice::<WireRequest>(&bytes).is_err());
        }
    }

    #[test]
    fn request_reader_enforces_an_absolute_deadline_against_byte_trickling() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        use std::time::Instant;

        let (server, mut client) = UnixStream::pair().unwrap();
        let sender = std::thread::spawn(move || {
            for byte in b"slow-client" {
                std::thread::sleep(Duration::from_millis(35));
                if client.write_all(&[*byte]).is_err() {
                    break;
                }
            }
        });

        let started = Instant::now();
        let result = read_api_request_with_timeout(&server, Duration::from_millis(150));
        assert!(matches!(result, Err(message) if message.contains("deadline exceeded")));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(server);
        let _ = sender.join();
    }
}

/// Pure regression tests for the transport identity gates used by handle_stream.
#[cfg(all(test, target_os = "linux"))]
mod api_identity_validation_tests {
    use super::*;

    fn identity() -> docker::ApiRunnerIdentity {
        docker::ApiRunnerIdentity {
            name: "runner-1".into(),
            repository: "owner/repo".into(),
            container_id: "a".repeat(64),
            workflow_job: None,
            workflow_run_id: None,
            workflow_job_id: None,
        }
    }

    fn invocation() -> ApiInvocation {
        ApiInvocation {
            api: GitRunApi::GitStatusRun,
            operation: GitRunOperation::Status,
            repository: "owner/repo".into(),
            workflow: "CI".into(),
            run_id: Some(42),
            job: "build".into(),
            runner: "runner-1".into(),
            resource: None,
            arguments: Default::default(),
        }
    }

    #[test]
    fn runner_name_and_repository_must_match_container_identity() {
        let mut runner = identity();
        let request = invocation();
        assert!(validate_runner_invocation_binding(&runner, &request).is_ok());

        runner.name = "runner-other".into();
        assert!(matches!(
            validate_runner_invocation_binding(&runner, &request),
            Err(message) if message.contains("identity does not match")
        ));

        runner = identity();
        runner.repository = "other/repo".into();
        assert!(validate_runner_invocation_binding(&runner, &request).is_err());
    }

    #[test]
    fn run_id_is_required_before_any_job_validation() {
        let runner = identity();
        let mut request = invocation();
        request.run_id = None;
        assert!(matches!(
            validate_runner_invocation_binding(&runner, &request),
            Err(message) if message.contains("GITHUB_RUN_ID is required")
        ));
    }

    #[test]
    fn container_job_and_run_binding_must_match_both_claims() {
        let mut runner = identity();
        runner.workflow_job = Some("build".into());
        runner.workflow_run_id = Some(42);
        let request = invocation();
        assert_eq!(
            validate_runner_invocation_binding(&runner, &request),
            Ok(RunnerInvocationBinding {
                run_id: 42,
                requires_active_job_lookup: false,
            })
        );

        runner.workflow_run_id = Some(99);
        assert!(matches!(
            validate_runner_invocation_binding(&runner, &request),
            Err(message) if message.contains("binding")
        ));

        runner.workflow_run_id = Some(42);
        runner.workflow_job = Some("deploy".into());
        assert!(matches!(
            validate_runner_invocation_binding(&runner, &request),
            Err(message) if message.contains("binding")
        ));
    }

    #[test]
    fn partial_container_job_binding_fails_closed() {
        let mut runner = identity();
        runner.workflow_job = Some("build".into());
        assert!(matches!(
            validate_runner_invocation_binding(&runner, &invocation()),
            Err(message) if message.contains("incomplete")
        ));

        runner = identity();
        runner.workflow_run_id = Some(42);
        assert!(matches!(
            validate_runner_invocation_binding(&runner, &invocation()),
            Err(message) if message.contains("incomplete")
        ));
    }

    #[test]
    fn unbound_container_requires_authoritative_active_job_lookup() {
        let result = validate_runner_invocation_binding(&identity(), &invocation()).unwrap();
        assert_eq!(result.run_id, 42);
        assert!(result.requires_active_job_lookup);
        assert!(active_job_matches("build", "build"));
        assert!(!active_job_matches("deploy", "build"));
    }

    #[test]
    fn exact_queued_job_binding_rejects_a_different_job_id() {
        assert!(workflow_job_id_matches(Some(101), 101));
        assert!(!workflow_job_id_matches(Some(101), 102));
        assert!(workflow_job_id_matches(None, 102));
    }

    #[test]
    fn authoritative_github_job_must_be_in_progress_on_the_same_runner() {
        assert!(workflow_job_is_active_on_runner(
            Some("runner-1"),
            "in_progress",
            None,
            "runner-1",
        ));
        assert!(!workflow_job_is_active_on_runner(
            Some("runner-2"),
            "in_progress",
            None,
            "runner-1",
        ));
        assert!(!workflow_job_is_active_on_runner(
            Some("runner-1"),
            "queued",
            None,
            "runner-1",
        ));
        assert!(!workflow_job_is_active_on_runner(
            Some("runner-1"),
            "completed",
            None,
            "runner-1",
        ));
        assert!(!workflow_job_is_active_on_runner(
            Some("runner-1"),
            "in_progress",
            Some("success"),
            "runner-1",
        ));
        assert!(!workflow_job_is_active_on_runner(
            None,
            "in_progress",
            None,
            "runner-1",
        ));
    }

    #[test]
    fn workflow_claim_is_optional_but_cannot_override_github() {
        assert!(workflow_claim_matches("", "CI"));
        assert!(workflow_claim_matches("CI", "CI"));
        assert!(!workflow_claim_matches("untrusted.yml", "CI"));
    }
}

/// End-to-end tests for the verified-caller -> GSR -> authenticated handoff
/// -> execution-backend boundary. The fake backend records dispatches but
/// never performs a real privileged side effect.
#[cfg(test)]
mod gsr_execution_boundary_tests {
    use super::*;
    use std::sync::Mutex;

    struct TemporaryDirectory(std::path::PathBuf);

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct RecordingBackend {
        requests: Mutex<Vec<AuthorizedOperation>>,
    }

    impl ExecutionBackend for RecordingBackend {
        fn execute(
            &self,
            request: &AuthorizedOperation,
        ) -> Result<ExecutionResult, ExecutionError> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(ExecutionResult {
                exit_code: 0,
                stdout: "recorded execution".into(),
                stderr: String::new(),
            })
        }
    }

    fn caller() -> VerifiedCaller {
        VerifiedCaller::new("owner/repo", "CI", "build", "runner-1").with_run_id(Some(42))
    }

    fn invocation(api: GitRunApi, operation: GitRunOperation) -> ApiInvocation {
        ApiInvocation {
            api,
            operation,
            repository: "owner/repo".into(),
            // An empty workflow claim is permitted; GSR must use the
            // authoritative workflow in VerifiedCaller, not this claim.
            workflow: String::new(),
            run_id: Some(42),
            job: "build".into(),
            runner: "runner-1".into(),
            resource: None,
            arguments: Default::default(),
        }
    }

    fn settings_allow_status() -> GitRunSettings {
        let mut settings = GitRunSettings::default();
        settings.global.set(
            GitRunApi::GitStatusRun,
            gitrun_core::ApiPolicy::enabled_with([GitRunOperation::Status]),
        );
        settings
    }

    #[test]
    fn dock_binding_requires_exact_nonempty_container_identity() {
        assert!(dock_binding_matches_current_id("sha256:abc", "sha256:abc"));
        assert!(!dock_binding_matches_current_id("sha256:old", "sha256:new"));
        assert!(!dock_binding_matches_current_id("", "sha256:new"));
        assert!(!dock_binding_matches_current_id("sha256:abc", ""));
    }

    #[cfg(unix)]
    #[test]
    fn register_registry_rejects_symlink_and_hard_link_files() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!(
            "gitrun-register-registry-links-{}-{}",
            std::process::id(),
            now()
        ));
        let _dir_guard = TemporaryDirectory(dir.clone());
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("register-registry.json");
        let target = dir.join("target.json");
        fs::write(&target, r#"{"entries":[]}"#).unwrap();
        symlink(&target, &path).unwrap();
        assert!(RegisterRegistry::load(&path).is_err());
        fs::remove_file(&path).unwrap();
        fs::hard_link(&target, &path).unwrap();
        assert!(RegisterRegistry::load(&path).is_err());
    }

    #[test]
    fn workflow_mask_command_data_escapes_percent_and_line_breaks() {
        assert_eq!(
            escape_workflow_command_data("first%\r\n::warning::injected"),
            "first%25%0D%0A::warning::injected"
        );
        assert_eq!(escape_workflow_command_data("single-line"), "single-line");
    }

    #[test]
    fn authorized_request_hands_the_exact_gsr_operation_to_the_backend() {
        let settings = settings_allow_status();
        let caller = caller();
        let invocation = invocation(GitRunApi::GitStatusRun, GitRunOperation::Status);
        let expected = authorize(
            &settings,
            &caller,
            invocation.api,
            invocation.operation,
            invocation.resource.as_deref(),
            invocation.arguments.clone(),
            None,
        )
        .expect("the configured status operation should be authorized");
        let authority = ExecutionAuthority::new().unwrap();
        let backend = RecordingBackend::default();
        let mut responses = Vec::new();

        execute_gsr_authorized_stream(
            &settings,
            &caller,
            &invocation,
            None,
            &authority,
            &backend,
            &mut |response| {
                responses.push(response);
                Ok(())
            },
        )
        .expect("authorized request should complete");

        let dispatched = backend.requests.lock().unwrap();
        assert_eq!(dispatched.as_slice(), &[expected]);
        assert!(matches!(responses.first(), Some(WireResponse::Accepted)));
        assert!(matches!(
            responses.get(1),
            Some(WireResponse::Event(ExecutionEvent::Started))
        ));
        assert!(responses.iter().any(|response| matches!(
            response,
            WireResponse::Event(ExecutionEvent::Stdout(output))
                if output == "recorded execution"
        )));
        assert!(matches!(
            responses.last(),
            Some(WireResponse::Event(ExecutionEvent::Finished {
                exit_code: 0
            }))
        ));
    }

    #[test]
    fn denied_docked_resource_is_rejected_before_backend_dispatch() {
        let mut settings = GitRunSettings::default();
        settings.global.set(
            GitRunApi::GitDockRun,
            gitrun_core::ApiPolicy::enabled_with([GitRunOperation::Execute]),
        );
        // The repository uses the default deny-by-default Docker policy, so
        // even a syntactically valid operation cannot target this resource.
        let caller = caller();
        let mut invocation = invocation(GitRunApi::GitDockRun, GitRunOperation::Execute);
        invocation.resource = Some("production".into());
        invocation
            .arguments
            .insert("command".into(), "printf unsafe".into());
        let authority = ExecutionAuthority::new().unwrap();
        let backend = RecordingBackend::default();
        let mut responses = Vec::new();

        execute_gsr_authorized_stream(
            &settings,
            &caller,
            &invocation,
            None,
            &authority,
            &backend,
            &mut |response| {
                responses.push(response);
                Ok(())
            },
        )
        .expect("authorization rejection should be returned over the wire");

        assert!(backend.requests.lock().unwrap().is_empty());
        assert_eq!(
            responses.len(),
            1,
            "denial must not accept or stream execution"
        );
        assert!(matches!(
            responses.first(),
            Some(WireResponse::Error { message }) if message.contains("resource denied")
        ));
    }

    #[test]
    fn invocation_claim_mismatches_never_reach_gsr_or_backend() {
        let settings = settings_allow_status();
        let authority = ExecutionAuthority::new().unwrap();
        let caller = caller();
        let base = invocation(GitRunApi::GitStatusRun, GitRunOperation::Status);

        for claim in ["repository", "workflow", "run ID", "job", "runner"] {
            let backend = RecordingBackend::default();
            let mut request = base.clone();
            match claim {
                "repository" => request.repository = "other/repo".into(),
                "workflow" => request.workflow = "untrusted.yml".into(),
                "run ID" => request.run_id = Some(99),
                "job" => request.job = "deploy".into(),
                "runner" => request.runner = "other-runner".into(),
                _ => unreachable!("claim comes from the fixed test matrix"),
            }
            let mut responses = Vec::new();

            execute_gsr_authorized_stream(
                &settings,
                &caller,
                &request,
                None,
                &authority,
                &backend,
                &mut |response| {
                    responses.push(response);
                    Ok(())
                },
            )
            .expect("binding failure should be reported to the peer");

            assert!(
                backend.requests.lock().unwrap().is_empty(),
                "{claim} mismatch must not dispatch to the backend"
            );
            assert!(
                matches!(
                    responses.as_slice(),
                    [WireResponse::Error { message }]
                        if message.starts_with("GSR caller binding failed:")
                ),
                "{claim} mismatch should fail before API authorization"
            );
        }
    }
}

#[cfg(all(test, unix, target_os = "linux"))]
mod api_socket_identity_integration_tests {
    use super::*;
    use crate::github::{WorkflowJobInfo, WorkflowRunInfo};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct FakeApiIdentityProvider {
        identity: Option<docker::ApiRunnerIdentity>,
        active_job: Option<WorkflowJobInfo>,
        job: Option<WorkflowJobInfo>,
        run: Option<WorkflowRunInfo>,
        failure_at: Option<&'static str>,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl FakeApiIdentityProvider {
        fn record_call(&self, stage: &'static str) -> Result<(), String> {
            self.calls.lock().unwrap().push(stage);
            if self.failure_at == Some(stage) {
                Err(format!("injected {stage} lookup failure"))
            } else {
                Ok(())
            }
        }
    }

    impl ApiIdentityProvider for FakeApiIdentityProvider {
        fn runner_for_peer_pid(
            &self,
            _peer_pid: i32,
        ) -> Result<Option<docker::ApiRunnerIdentity>, String> {
            self.record_call("peer_identity")?;
            Ok(self.identity.clone())
        }

        fn find_active_workflow_job_for_runner(
            &self,
            _repository: &str,
            _runner_name: &str,
        ) -> Result<Option<WorkflowJobInfo>, String> {
            self.record_call("active_job")?;
            Ok(self.active_job.clone())
        }

        fn find_workflow_job(
            &self,
            _repository: &str,
            _run_id: u64,
            _job_name: &str,
        ) -> Result<Option<WorkflowJobInfo>, String> {
            self.record_call("workflow_job")?;
            Ok(self.job.clone())
        }

        fn find_workflow_run(
            &self,
            _repository: &str,
            _run_id: u64,
        ) -> Result<Option<WorkflowRunInfo>, String> {
            self.record_call("workflow_run")?;
            Ok(self.run.clone())
        }
    }

    fn valid_provider() -> FakeApiIdentityProvider {
        FakeApiIdentityProvider {
            identity: Some(docker::ApiRunnerIdentity {
                name: "runner-1".into(),
                repository: "owner/repo".into(),
                container_id: "a".repeat(64),
                workflow_job: Some("build".into()),
                workflow_run_id: Some(42),
                workflow_job_id: Some(101),
            }),
            active_job: None,
            job: Some(WorkflowJobInfo {
                id: 101,
                name: "build".into(),
                status: "in_progress".into(),
                conclusion: None,
                runner_name: Some("runner-1".into()),
            }),
            run: Some(WorkflowRunInfo {
                id: 42,
                name: "CI".into(),
                started_at_epoch: Some(1_700_000_000),
            }),
            failure_at: None,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn status_invocation() -> ApiInvocation {
        ApiInvocation {
            api: GitRunApi::GitStatusRun,
            operation: GitRunOperation::Status,
            repository: "owner/repo".into(),
            workflow: "CI".into(),
            run_id: Some(42),
            job: "build".into(),
            runner: "runner-1".into(),
            resource: None,
            arguments: Default::default(),
        }
    }

    #[derive(Debug)]
    struct SocketTestResult {
        handler_result: Result<(), String>,
        responses: Vec<WireResponse>,
        calls: Vec<&'static str>,
    }

    fn invoke_socket_request(
        tag: &str,
        invocation: ApiInvocation,
        provider: FakeApiIdentityProvider,
    ) -> SocketTestResult {
        let state_dir = std::env::temp_dir().join(format!(
            "gitrun-api-socket-integration-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&state_dir);
        fs::create_dir_all(&state_dir).unwrap();

        let mut settings = GitRunSettings::default();
        settings.global.set(
            GitRunApi::GitStatusRun,
            gitrun_core::ApiPolicy::enabled_with([GitRunOperation::Status]),
        );
        settings
            .save(GitRunSettings::path_for_state_dir(&state_dir))
            .unwrap();

        let github = Arc::new(GitHubClient::new("test-only-token").unwrap());
        let authority = Arc::new(ExecutionAuthority::new().unwrap());
        let calls = Arc::clone(&provider.calls);
        let (server, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        let handler_state_dir = state_dir.clone();
        let handler = std::thread::spawn(move || {
            handle_stream_with_identity_provider(
                server,
                Config::default(),
                github,
                handler_state_dir,
                authority,
                &provider,
            )
        });

        let request = WireRequest { invocation };
        let mut frame = serde_json::to_vec(&request).unwrap();
        frame.push(b'\n');
        client.write_all(&frame).unwrap();

        let mut response_text = String::new();
        client.read_to_string(&mut response_text).unwrap();
        drop(client);

        let handler_result = handler.join().unwrap();
        let responses = response_text
            .lines()
            .map(|line| serde_json::from_str::<WireResponse>(line).unwrap())
            .collect();
        let calls = calls.lock().unwrap().clone();
        let _ = fs::remove_dir_all(&state_dir);

        SocketTestResult {
            handler_result,
            responses,
            calls,
        }
    }

    fn contains_acceptance(responses: &[WireResponse]) -> bool {
        responses
            .iter()
            .any(|response| matches!(response, WireResponse::Accepted))
    }

    #[test]
    fn full_unix_socket_path_accepts_only_after_identity_github_and_gsr_checks() {
        let result = invoke_socket_request("accepted", status_invocation(), valid_provider());

        assert!(result.handler_result.is_ok(), "handler failed: {result:?}");
        assert!(contains_acceptance(&result.responses));
        assert!(matches!(
            result.responses.get(1),
            Some(WireResponse::Event(ExecutionEvent::Started))
        ));
        assert!(result.responses.iter().any(|response| matches!(
            response,
            WireResponse::Event(ExecutionEvent::Stdout(output))
                if output.contains("GitRun API STATUS")
        )));
        assert!(matches!(
            result.responses.last(),
            Some(WireResponse::Event(ExecutionEvent::Finished {
                exit_code: 0
            }))
        ));
        assert_eq!(
            result.calls,
            vec!["peer_identity", "workflow_job", "workflow_run"]
        );
    }

    #[test]
    fn unmanaged_socket_peer_is_rejected_before_github_lookups() {
        let mut provider = valid_provider();
        provider.identity = None;

        let result = invoke_socket_request("unmanaged-peer", status_invocation(), provider);

        assert!(
            result.handler_result.is_ok(),
            "expected a wire-level denial: {result:?}"
        );
        assert!(!contains_acceptance(&result.responses));
        assert!(matches!(
            result.responses.as_slice(),
            [WireResponse::Error { message }]
                if message.contains("not a managed runner container")
        ));
        assert_eq!(result.calls, vec!["peer_identity"]);
    }

    #[test]
    fn inactive_github_job_is_rejected_before_authorization_or_execution() {
        let mut provider = valid_provider();
        let job = provider.job.as_mut().unwrap();
        job.status = "completed".into();
        job.conclusion = Some("success".into());

        let result = invoke_socket_request("inactive-job", status_invocation(), provider);

        assert!(
            result.handler_result.is_ok(),
            "expected a wire-level denial: {result:?}"
        );
        assert!(!contains_acceptance(&result.responses));
        assert!(matches!(
            result.responses.as_slice(),
            [WireResponse::Error { message }]
                if message.contains("active runner for this workflow job")
        ));
        assert_eq!(result.calls, vec!["peer_identity", "workflow_job"]);
    }

    #[test]
    fn workflow_claim_mismatch_is_rejected_before_execution() {
        let mut invocation = status_invocation();
        invocation.workflow = "untrusted-workflow".into();

        let result = invoke_socket_request("workflow-mismatch", invocation, valid_provider());

        assert!(
            result.handler_result.is_ok(),
            "expected a wire-level denial: {result:?}"
        );
        assert!(!contains_acceptance(&result.responses));
        assert!(matches!(
            result.responses.as_slice(),
            [WireResponse::Error { message }]
                if message.contains("workflow identity claim does not match")
        ));
        assert_eq!(
            result.calls,
            vec!["peer_identity", "workflow_job", "workflow_run"]
        );
    }

    #[test]
    fn runner_claim_mismatch_is_rejected_before_github_lookups() {
        let mut invocation = status_invocation();
        invocation.runner = "untrusted-runner".into();

        let result = invoke_socket_request("runner-mismatch", invocation, valid_provider());

        assert!(
            result.handler_result.is_ok(),
            "expected a wire-level denial: {result:?}"
        );
        assert!(!contains_acceptance(&result.responses));
        assert!(matches!(
            result.responses.as_slice(),
            [WireResponse::Error { message }]
                if message.contains("identity does not match the managed container")
        ));
        assert_eq!(result.calls, vec!["peer_identity"]);
    }

    #[test]
    fn missing_run_id_is_rejected_before_github_lookups() {
        let mut invocation = status_invocation();
        invocation.run_id = None;

        let result = invoke_socket_request("missing-run-id", invocation, valid_provider());

        assert!(
            result.handler_result.is_ok(),
            "expected a wire-level denial: {result:?}"
        );
        assert!(!contains_acceptance(&result.responses));
        assert!(matches!(
            result.responses.as_slice(),
            [WireResponse::Error { message }]
                if message.contains("GITHUB_RUN_ID is required")
        ));
        assert_eq!(result.calls, vec!["peer_identity"]);
    }

    #[test]
    fn unbound_container_uses_authoritative_active_job_lookup() {
        let mut provider = valid_provider();
        let identity = provider.identity.as_mut().unwrap();
        identity.workflow_job = None;
        identity.workflow_run_id = None;
        provider.active_job = Some(WorkflowJobInfo {
            id: 99,
            name: "build".into(),
            status: "in_progress".into(),
            conclusion: None,
            runner_name: Some("runner-1".into()),
        });

        let result = invoke_socket_request("active-job-fallback", status_invocation(), provider);

        assert!(result.handler_result.is_ok(), "handler failed: {result:?}");
        assert!(contains_acceptance(&result.responses));
        assert_eq!(
            result.calls,
            vec![
                "peer_identity",
                "active_job",
                "workflow_job",
                "workflow_run"
            ]
        );
    }

    #[test]
    fn active_job_fallback_rejects_a_different_job_name() {
        let mut provider = valid_provider();
        let identity = provider.identity.as_mut().unwrap();
        identity.workflow_job = None;
        identity.workflow_run_id = None;
        provider.active_job = Some(WorkflowJobInfo {
            id: 99,
            name: "deploy".into(),
            status: "in_progress".into(),
            conclusion: None,
            runner_name: Some("runner-1".into()),
        });

        let result = invoke_socket_request("active-job-mismatch", status_invocation(), provider);

        assert!(
            result.handler_result.is_ok(),
            "expected a wire-level denial: {result:?}"
        );
        assert!(!contains_acceptance(&result.responses));
        assert!(matches!(
            result.responses.as_slice(),
            [WireResponse::Error { message }]
                if message.contains("not the active job assigned to this runner")
        ));
        assert_eq!(result.calls, vec!["peer_identity", "active_job"]);
    }

    #[test]
    fn github_lookup_error_fails_closed_without_accepting_or_executing() {
        let mut provider = valid_provider();
        provider.failure_at = Some("workflow_job");

        let result = invoke_socket_request("github-error", status_invocation(), provider);

        assert!(
            matches!(result.handler_result, Err(ref message) if message.contains("verify current workflow job"))
        );
        assert!(!contains_acceptance(&result.responses));
        assert!(result.responses.is_empty());
        assert_eq!(result.calls, vec!["peer_identity", "workflow_job"]);
    }
}

#[cfg(test)]
mod kitty_dispatch_contract_tests {
    use super::*;

    #[test]
    fn kitty_dispatch_plan_outputs_machine_readable_route_or_fallback() {
        for (mode, kitty_online, status, labels) in [
            (
                "kitty",
                true,
                "online",
                vec![
                    "gitrun-job-wk-0123456789abcdef",
                    "gitrun-route-0123456789abcdef0123456789abcdef",
                ],
            ),
            (
                "github",
                true,
                "shadow",
                vec!["self-hosted", "linux", "x64"],
            ),
            (
                "github",
                false,
                "disabled",
                vec!["self-hosted", "linux", "x64"],
            ),
        ] {
            let labels: Vec<String> = labels.into_iter().map(str::to_owned).collect();
            let result =
                kitty_dispatch_plan_result(mode, kitty_online, status, "test-reason", &labels)
                    .unwrap();
            let output: Vec<&str> = result.stdout.lines().collect();
            assert_eq!(output[0], format!("GITRUN_DISPATCH_STATUS={status}"));
            assert_eq!(output[1], format!("GITRUN_DISPATCH_MODE={mode}"));
            assert_eq!(output[2], format!("GITRUN_KITTY_ONLINE={kitty_online}"));
            assert_eq!(
                output[3].split_once('=').unwrap().0,
                "GITRUN_DISPATCH_RUNS_ON"
            );
            let decoded: Vec<String> =
                serde_json::from_str(output[3].split_once('=').unwrap().1).unwrap();
            assert_eq!(decoded, labels);
            assert_eq!(output[4], "GITRUN_DISPATCH_REASON=test-reason");
        }
    }

    #[cfg(unix)]
    #[test]
    fn dispatch_route_ids_are_random_canonical_selectors() {
        let first = new_kitty_dispatch_route_id().expect("Linux/Unix route entropy");
        let second = new_kitty_dispatch_route_id().expect("Linux/Unix route entropy");
        assert_eq!(first.len(), 32);
        assert!(first
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        assert_ne!(
            first, second,
            "two concurrent dispatches must use different route labels"
        );
    }
}
