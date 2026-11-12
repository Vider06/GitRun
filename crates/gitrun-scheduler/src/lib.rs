#[cfg(not(any(kani, verus)))]
pub mod api_service;
#[cfg(not(any(kani, verus)))]
pub mod backoff;
#[cfg(not(any(kani, verus)))]
pub mod dock_registry;
#[cfg(not(any(kani, verus)))]
pub mod docker;
#[cfg(not(any(kani, verus)))]
pub mod github;
#[cfg(not(any(kani, verus)))]
pub mod gsr_bridge;
#[cfg(not(any(kani, verus)))]
pub mod gsr_poll;
#[cfg(not(any(kani, verus)))]
pub mod gtuu;
#[cfg(not(any(kani, verus)))]
pub mod gtuu_startup;
#[cfg(not(any(kani, verus)))]
pub mod logic_containers;
#[cfg(not(any(kani, verus)))]
pub mod reconcile;
#[cfg(not(any(kani, verus)))]
pub mod resource_pressure;
#[cfg(not(any(kani, verus)))]
pub mod state;
#[cfg(not(any(kani, verus)))]
pub mod vm;
#[cfg(not(any(kani, verus)))]
pub mod vm_resolution;

#[cfg(not(any(kani, verus)))]
pub use backoff::RateLimitTracker;
#[cfg(not(any(kani, verus)))]
pub use github::{GitHubClient, GitHubError, Runner};
#[cfg(not(any(kani, verus)))]
pub use gtuu_startup::GtuuStartupReport;
#[cfg(not(any(kani, verus)))]
pub use logic_containers::{Backend, LogicRule};
#[cfg(not(any(kani, verus)))]
pub use reconcile::{Action, ReconcileInput};

// GitRun autoscaler entry point. Rust replacement for `autoscaler/gitrun_manager.py`'s
// `main()`/reconcile loop.
//
// Flow per poll cycle, per configured repository:
// 1. Snapshot the world: managed Docker containers, GitHub runner
//    registrations, queued self-hosted jobs.
// 2. Feed the snapshot to `reconcile::plan()` (pure, no I/O) to get an
//    ordered `Vec<Action>`.
// 3. Execute each action against Docker/GitHub, logging and continuing past
//    individual failures instead of aborting the whole cycle — an
//    improvement over the Python version, where one unhandled exception
//    anywhere in `reconcile()` skipped that repo entirely for the cycle.
// 4. Persist idle/recovery timestamps.
//
// GTUU runs on its own schedule in a background thread, same shape as the
// Python `gtuu_schedule_loop`.

#[cfg(not(any(kani, verus)))]
use crate::docker::ManagedContainer;
#[cfg(not(any(kani, verus)))]
use crate::gsr_bridge::VaultToGsrBridge;
#[cfg(not(any(kani, verus)))]
use crate::gtuu::{GtuuConfig, GtuuLock};
#[cfg(not(any(kani, verus)))]
use crate::reconcile::{ContainerHealth, ContainerView, IdleInfo, RunnerView};
#[cfg(not(any(kani, verus)))]
use crate::state::SchedulerState;
#[cfg(not(any(kani, verus)))]
use crate::vm_resolution::VmResolutionRegistry;
#[cfg(not(any(kani, verus)))]
use gitrun_core::{Config, GitHubAuth, StateStore};
#[cfg(not(any(kani, verus)))]
use gitrun_vault::Vault;
#[cfg(not(any(kani, verus)))]
use std::path::{Path, PathBuf};
#[cfg(not(any(kani, verus)))]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(not(any(kani, verus)))]
use std::sync::Arc;
#[cfg(not(any(kani, verus)))]
use std::time::{Duration, Instant};

#[cfg(not(any(kani, verus)))]
static CONTAINER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

// Bound per-repository Docker process spawning for Kitty's 30-second telemetry poll.
#[cfg(not(any(kani, verus)))]
const MAX_ACTIVE_RESOURCE_SAMPLES_PER_REPOSITORY: usize = 32;

#[cfg(not(any(kani, verus)))]
pub fn run() {
    let stopping = Arc::new(AtomicBool::new(false));
    if let Err(error) = install_signal_handlers(&stopping) {
        eprintln!("gitrun-autoscaler: failed to install signal handlers: {error}");
        std::process::exit(1);
    }

    let config = match load_config() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("gitrun-autoscaler: invalid configuration: {error}");
            std::process::exit(2);
        }
    };

    if config.repositories.is_empty() {
        eprintln!("gitrun-autoscaler: no repositories configured");
        std::process::exit(2);
    }

    let settings_path = gitrun_core::GitRunSettings::path_for_state_dir(&config.state_dir);
    match gitrun_core::GitRunSettings::load_or_default(&settings_path) {
        Ok(settings) => {
            for issue in settings.lint() {
                eprintln!(
                    "gitrun-autoscaler: security policy lint [{}] warning: {}",
                    issue.path, issue.message
                );
            }
        }
        Err(error) => {
            eprintln!("gitrun-autoscaler: security policy could not be linted: {error}");
            std::process::exit(2);
        }
    }

    let client = match build_github_client(&config) {
        Ok(client) => Arc::new(client),
        Err(error) => {
            eprintln!("gitrun-autoscaler: {error}");
            std::process::exit(2);
        }
    };

    let state_dir = PathBuf::from(&config.state_dir);
    let rate_limits = RateLimitTracker::new();
    let vm_registry = vm_resolution::new_registry();
    let mut kitty_runtime = gitrun_smart_scheduler::KittyRuntime::new(&state_dir);

    if let Err(error) = vm_resolution::load_vm_definitions(&state_dir) {
        eprintln!(
            "gitrun-autoscaler: invalid {}: {error}",
            state_dir.join("vm-configs.json").display()
        );
        std::process::exit(2);
    }

    let pid_file = state_dir.join("gitrun-autoscaler.pid");
    let _pid_lock = match acquire_pid_file(&pid_file) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!(
                "gitrun-autoscaler: could not acquire scheduler PID file at {}: {error}",
                pid_file.display()
            );
            std::process::exit(2);
        }
    };

    if let Err(error) =
        api_service::spawn(config.clone(), Arc::clone(&client), Arc::clone(&stopping))
    {
        eprintln!("gitrun-autoscaler: GitRun API service failed to start: {error}");
        let _ = std::fs::remove_file(&pid_file);
        std::process::exit(2);
    }
    if let Err(error) = spawn_gtuu_thread(&config, &stopping) {
        eprintln!("gitrun-autoscaler: failed to start GTUU worker: {error}");
        let _ = std::fs::remove_file(&pid_file);
        std::process::exit(2);
    }
    if let Err(error) = spawn_gsr_poll_thread(&config, &vm_registry, &stopping) {
        eprintln!("gitrun-autoscaler: failed to start GSR poll worker: {error}");
        stopping.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&pid_file);
        std::process::exit(2);
    }

    println!(
        "gitrun-autoscaler: starting, polling every {}s",
        config.poll_interval
    );
    while !stopping.load(Ordering::Relaxed) {
        let mut resource_context = ResourcePressureContext {
            allow_new_runner_creation: true,
            snapshot: None,
        };
        if config.resource_pressure_enabled {
            match resource_pressure::sample(&config) {
                Ok(snapshot) => {
                    resource_context.snapshot = Some(snapshot);
                    if snapshot.is_pressured(&config) {
                        eprintln!("gitrun-autoscaler: host resource pressure high (cpu={:.1}% memory={:.1}% disk={:.1}%), deferring runner creation while continuing safe cleanup/recovery", snapshot.cpu_percent, snapshot.memory_percent, snapshot.disk_percent);
                        resource_context.allow_new_runner_creation = false;
                    }
                }
                Err(error @ resource_pressure::ResourcePressureError::Disk { .. }) => {
                    eprintln!("gitrun-autoscaler: critical filesystem pressure check failed: {error}; deferring runner creation while continuing safe cleanup/recovery");
                    resource_context.allow_new_runner_creation = false;
                }
                Err(error) => {
                    eprintln!("gitrun-autoscaler: host resource pressure check unavailable: {error}; deferring runner creation while continuing safe cleanup/recovery");
                    resource_context.allow_new_runner_creation = false;
                }
            }
        }
        let mut adaptive_resource_budget = AdaptiveResourceBudgetState::for_cycle(&state_dir);
        if adaptive_resource_budget.enabled {
            if let Some(error) = adaptive_resource_budget.unavailable_reason.as_deref() {
                eprintln!(
                    "gitrun-autoscaler: Kitty Adaptive Resource Management could not verify host capacity: {error}; runner creation will be deferred"
                );
            }
        }
        for repo in &config.repositories {
            if let Some(remaining) = rate_limits.remaining_cooldown(repo) {
                println!(
                    "gitrun-autoscaler: {repo} is rate-limited, skipping for {}s",
                    remaining.as_secs()
                );
                continue;
            }
            let mut reconcile_resource_context = ReconcileResourceContext {
                pressure: resource_context,
                adaptive_budget: &mut adaptive_resource_budget,
            };
            if let Err(error) = reconcile_repo(
                &client,
                &config,
                &state_dir,
                &vm_registry,
                &mut kitty_runtime,
                repo,
                &mut reconcile_resource_context,
            ) {
                if let Some(GitHubError::RateLimited { retry_after }) =
                    error.downcast_ref::<GitHubError>()
                {
                    rate_limits.record_rate_limited(repo, *retry_after);
                }
                eprintln!("gitrun-autoscaler: reconciliation failed for {repo}: {error}");
                let _ = record_crash(&state_dir, &format!("{repo}: {error}"));
            } else {
                rate_limits.clear(repo);
            }
        }
        sleep_interruptible(Duration::from_secs(config.poll_interval), &stopping);
    }
    let _ = std::fs::remove_file(&pid_file);
    println!("gitrun-autoscaler: stopped");
}

#[cfg(unix)]
#[cfg(not(any(kani, verus)))]
fn open_protected_pid_file(
    path: &std::path::Path,
    read: bool,
    write: bool,
    create: bool,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let mut options = std::fs::OpenOptions::new();
    options
        .read(read)
        .write(write)
        .create(create)
        .truncate(false);
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "scheduler PID/lock file must be a regular file without hard links",
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(unix)]
#[cfg(not(any(kani, verus)))]
fn acquire_pid_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::io::{Error, ErrorKind, Read, Write};
    use std::os::fd::AsRawFd;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let lock_path = path.with_extension("pid.lock");
    let lock = open_protected_pid_file(&lock_path, true, true, true)?;
    let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(Error::new(
                ErrorKind::AlreadyExists,
                "another gitrun-autoscaler instance holds the scheduler lock",
            ));
        }
        return Err(error);
    }

    let existing_pid = match open_protected_pid_file(path, true, false, false) {
        Ok(mut file) => {
            let mut raw = String::new();
            file.read_to_string(&mut raw)?;
            let pid = raw.trim().parse::<u32>().map_err(|_| {
                Error::new(ErrorKind::InvalidData, "scheduler PID file is malformed")
            })?;
            if pid == 0 || pid > i32::MAX as u32 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "scheduler PID is out of range",
                ));
            }
            Some(pid)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(pid) = existing_pid {
        if pid != std::process::id() && process_is_alive(pid) {
            return Err(Error::new(
                ErrorKind::AlreadyExists,
                format!("another gitrun-autoscaler instance is already running (pid {pid})"),
            ));
        }
    }

    let mut pid = open_protected_pid_file(path, false, true, true)?;
    pid.set_len(0)?;
    pid.write_all(std::process::id().to_string().as_bytes())?;
    pid.sync_all()?;
    Ok(lock)
}

#[cfg(not(unix))]
#[cfg(not(any(kani, verus)))]
fn acquire_pid_file(path: &std::path::Path) -> std::io::Result<Option<std::fs::File>> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing_pid = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok());
    if let Some(pid) = existing_pid {
        if process_is_alive(pid) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("another gitrun-autoscaler instance is already running (pid {pid})"),
            ));
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(std::process::id().to_string().as_bytes())?;
    file.sync_all()?;
    Ok(None)
}

#[cfg(unix)]
#[cfg(not(any(kani, verus)))]
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    unsafe extern "C" {
        #[link_name = "kill"]
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let result = unsafe { kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
#[cfg(not(any(kani, verus)))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

#[cfg(not(any(kani, verus)))]
fn load_config() -> Result<Config, gitrun_core::ConfigError> {
    match std::env::var("GITRUN_CONFIG_FILE") {
        Ok(path) => Config::from_env_file(path),
        Err(_) => Config::from_env(),
    }
}

#[cfg(not(any(kani, verus)))]
fn build_github_client(config: &Config) -> Result<GitHubClient, Box<dyn std::error::Error>> {
    let connect_timeout = Duration::from_secs(config.github_connect_timeout);
    let request_timeout = Duration::from_secs(config.github_request_timeout);

    match GitHubAuth::from_config(config)? {
        GitHubAuth::App(auth) => Ok(GitHubClient::with_app_auth(
            auth,
            connect_timeout,
            request_timeout,
        )?),
        GitHubAuth::Pat(token) => Ok(GitHubClient::with_timeouts(
            token,
            connect_timeout,
            request_timeout,
        )?),
    }
}

#[cfg(not(any(kani, verus)))]
fn install_signal_handlers(stopping: &Arc<AtomicBool>) -> Result<(), std::io::Error> {
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stopping.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, stopping.clone())?;
    Ok(())
}

#[cfg(not(any(kani, verus)))]
fn sleep_interruptible(total: Duration, stopping: &Arc<AtomicBool>) {
    let deadline = Instant::now() + total;
    while !stopping.load(Ordering::Relaxed) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(250)));
    }
}
