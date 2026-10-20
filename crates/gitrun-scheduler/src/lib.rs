pub mod api_service;
pub mod backoff;
pub mod dock_registry;
pub mod docker;
pub mod github;
pub mod gsr_bridge;
pub mod gsr_poll;
pub mod gtuu;
pub mod gtuu_startup;
pub mod logic_containers;
pub mod reconcile;
pub mod resource_pressure;
pub mod state;
pub mod vm;
pub mod vm_resolution;

pub use backoff::RateLimitTracker;
pub use github::{GitHubClient, GitHubError, Runner};
pub use gtuu_startup::GtuuStartupReport;
pub use logic_containers::{Backend, LogicRule};
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

use crate::docker::ManagedContainer;
use crate::gsr_bridge::VaultToGsrBridge;
use crate::gtuu::{GtuuConfig, GtuuLock};
use crate::reconcile::{ContainerHealth, ContainerView, IdleInfo, RunnerView};
use crate::state::SchedulerState;
use crate::vm_resolution::VmResolutionRegistry;
use gitrun_core::{Config, GitHubAuth, StateStore};
use gitrun_vault::Vault;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static CONTAINER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

// Bound per-repository Docker process spawning for Kitty's 30-second telemetry poll.
const MAX_ACTIVE_RESOURCE_SAMPLES_PER_REPOSITORY: usize = 32;

// The scheduler keeps reconciliation in Rust; the legacy Python path is no longer executed.
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
    // Kitty is an optional statistical add-on. Its runtime can recommend only
    // a bounded minimum-runner floor; the deterministic planner and execution
    // path remain the authority and are used unchanged on any Kitty failure.
    let mut kitty_runtime = gitrun_smart_scheduler::KittyRuntime::new(&state_dir);

    // Logic Containers' VM backend: the registry (non-blocking hypervisor
    // resolution state) is shared across the process lifetime, but the VM
    // *definitions themselves* are deliberately NOT loaded once here and
    // cached — `resolve_backend_and_image` reloads `vm-configs.json` fresh
    // every reconcile cycle, exactly like it already does for
    // `logic-containers.json`. Both files are dashboard-editable while the
    // autoscaler is running; caching either at startup would mean an
    // operator's edit silently not taking effect until a restart. This
    // block only does a one-time *validation* pass, so a malformed file is
    // caught at boot rather than only showing up as "every VM job falls
    // back to local host" with no clear error later.
    if let Err(error) = vm_resolution::load_vm_definitions(&state_dir) {
        eprintln!(
            "gitrun-autoscaler: invalid {}: {error}",
            state_dir.join("vm-configs.json").display()
        );
        std::process::exit(2);
    }
    // GSR integration: write our own PID to a well-known file so the
    // separate `gitrun-gsr` watchdog process can supervise us — see
    // `gitrun-gsr/src/watchdog.rs`. Removed on clean shutdown below; if this
    // process instead crashes hard, the file survives with a now-dead PID
    // in it, which is exactly the signal the watchdog looks for.
    let pid_file = state_dir.join("gitrun-autoscaler.pid");
    // Keep the OS lock handle alive for the entire scheduler lifetime. The
    // sidecar lock file is intentionally persistent: unlinking a locked inode
    // would let a second process create and lock a different inode at the same
    // path, defeating singleton enforcement.
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

    // Only start background workers after every fatal startup check has passed
    // and this process owns the singleton PID file. Otherwise GTUU/GSR/API could
    // act briefly and concurrently while startup is about to abort.
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
        // Pressure is a backpressure signal for capacity growth, not a reason
        // to stop all lifecycle work. Keep safe cleanup and recovery running,
        // but fail closed for any action that would create/recreate a runner.
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
        // One budget is shared by every configured repository in this poll
        // tick, so separate repo reconciliations cannot each consume the full
        // host headroom independently.
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
                // Skip this repo entirely for this tick rather than making
                // any GitHub call at all — the whole point of centralizing
                // this is that individual calls no longer each decide for
                // themselves whether to retry; the scheduler decides once,
                // up front, per repo.
                println!(
                    "gitrun-autoscaler: {repo} is rate-limited, skipping for {}s",
                    remaining.as_secs()
                );
                continue;
            }
            if let Err(error) = reconcile_repo(
                &client,
                &config,
                &state_dir,
                &vm_registry,
                &mut kitty_runtime,
                repo,
                resource_context,
                &mut adaptive_resource_budget,
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
    // Clean shutdown: remove the PID file so GSR doesn't mistake a
    // deliberate stop for a crash on its next poll.
    let _ = std::fs::remove_file(&pid_file);
    println!("gitrun-autoscaler: stopped");
}

#[cfg(unix)]
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
fn acquire_pid_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::io::{Error, ErrorKind, Read, Write};
    use std::os::fd::AsRawFd;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let lock_path = path.with_extension("pid.lock");
    // Do not remove this file after unlocking: flock applies to the opened
    // inode, so unlinking it can create two independently locked inodes.
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

    // Check the marker while holding the lock as well, so a live instance
    // started by an older version (which may not use the sidecar lock yet)
    // cannot be overwritten during an upgrade. A malformed or unsafe marker
    // is an error, not an invitation to overwrite an unknown file.
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

    // The kernel lock serializes stale-PID replacement. Open without truncation,
    // validate the inode, and only then truncate it so symlink/hardlink targets
    // are never followed or modified.
    let mut pid = open_protected_pid_file(path, false, true, true)?;
    pid.set_len(0)?;
    pid.write_all(std::process::id().to_string().as_bytes())?;
    pid.sync_all()?;
    Ok(lock)
}

#[cfg(not(unix))]
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
fn process_is_alive(pid: u32) -> bool {
    // Never cast an untrusted oversized PID to i32: a negative value to
    // kill(2) targets a process group rather than one process.
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
fn process_is_alive(_pid: u32) -> bool {
    true
}

fn load_config() -> Result<Config, gitrun_core::ConfigError> {
    match std::env::var("GITRUN_CONFIG_FILE") {
        Ok(path) => Config::from_env_file(path),
        Err(_) => Config::from_env(),
    }
}

/// Builds the GitHub client using whichever auth method the config selects.
/// GitHub App auth (all three `github_app_*` fields present, guaranteed
/// all-or-nothing by `Config::validate`) takes priority when configured;
/// otherwise falls back to the `GITHUB_TOKEN` PAT, exactly as before App
/// support existed — so an operator who never touches the new fields sees no
/// behavior change at all.
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

fn install_signal_handlers(stopping: &Arc<AtomicBool>) -> Result<(), std::io::Error> {
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stopping.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, stopping.clone())?;
    Ok(())
}

fn sleep_interruptible(total: Duration, stopping: &Arc<AtomicBool>) {
    let deadline = Instant::now() + total;
    loop {
        if stopping.load(Ordering::Relaxed) {
            return;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return;
        };
        if remaining.is_zero() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200).min(remaining));
    }
}

/// Resolves which Docker backend and runner image a dynamic runner should
/// use, applying Logic Containers rules (see `logic_containers.rs`) against
/// a job's labels, falling back to the configured default (local host,
/// `config.runner_image`) when no rule matches or no rules are configured.
///
/// Wired end-to-end as of this session: `reconcile::plan()` attaches each
/// dynamic `Action::CreateRunner` the labels of the queued job it's
/// covering, and `create_runner` passes them straight through to this
/// function. Permanent warm-pool runners and `RecreateOrphaned`
/// intentionally still pass an empty label set, since neither is "for" one
/// specific job.
///
/// A `Backend::Vm` match no longer silently falls back to the local host:
/// it goes through `vm_resolution::resolve_or_spawn`, which is itself
/// non-blocking (see that module's doc comment) — if the VM isn't resolved
/// yet this call, no runner is created locally, but a background thread
/// is working on bringing the VM up (KVM preferred, operator asked
/// via the dashboard if it fails, VirtualBox as the explicit fallback) and
/// a *later* cycle will get the real `DockerHost::Remote` once it's ready.
///
/// Returns `(backend, image, is_windows)` — `is_windows` comes from the
/// matched VM's own definition (`VmConfig::is_windows`) rather than being
/// guessed from the backend shape, since a `Remote` backend alone doesn't
/// say what OS is on the other end.
fn resolve_backend_and_image(
    config: &Config,
    state_dir: &std::path::Path,
    vm_registry: &VmResolutionRegistry,
    job_labels: &[String],
) -> Option<(docker::DockerHost, String, bool)> {
    let rules_path = state_dir.join("logic-containers.json");
    let rules = match logic_containers::load_rules(&rules_path) {
        Ok(rules) => rules,
        Err(error) => {
            eprintln!("gitrun-autoscaler: could not load Logic Containers rules: {error}");
            // Missing is already represented by Ok(Vec::new()) in load_rules;
            // every other failure is a real configuration error and must not
            // silently route a VM-targeted job to the local Docker daemon.
            return None;
        }
    };

    match logic_containers::resolve(&rules, job_labels) {
        Some((logic_containers::Backend::LocalLinux, image)) => {
            Some((docker::DockerHost::Local, image.to_owned(), false))
        }
        Some((logic_containers::Backend::Vm { vm_name }, image)) => {
            // Reloaded fresh every call, same as `rules` above — an
            // operator can add/edit VMs from the dashboard while the
            // autoscaler is running, and this needs to see that without a
            // restart. `vm-configs.json` is small and this runs once per
            // dynamic runner creation, not in a tight loop, so re-reading
            // it here isn't a meaningful cost.
            let vm_defs = match vm::load_vm_configs(state_dir) {
                Ok(defs) => defs,
                Err(error) => {
                    eprintln!("gitrun-autoscaler: could not load VM configs: {error}");
                    // A configured VM target must never degrade to the local
                    // host because its definition file is unreadable/corrupt.
                    return None;
                }
            };
            let Some(vm_config) = vm::find_vm_config(&vm_defs, vm_name) else {
                eprintln!(
                    "gitrun-autoscaler: Logic Containers rule targets VM '{vm_name}', but no VM with that name is configured — runner creation deferred"
                );
                return None;
            };
            match vm_resolution::resolve_or_spawn(vm_registry, state_dir, vm_config) {
                vm_resolution::VmResolutionResult::Ready(host) => {
                    Some((host, image.to_owned(), vm_config.is_windows))
                }
                vm_resolution::VmResolutionResult::Resolving => {
                    eprintln!(
                        "gitrun-autoscaler: VM '{vm_name}' isn't ready yet; runner creation deferred while hypervisor setup or liveness verification is in progress"
                    );
                    None
                }
                vm_resolution::VmResolutionResult::Failed(error) => {
                    eprintln!(
                        "gitrun-autoscaler: VM '{vm_name}' resolution failed; runner creation deferred: {error}"
                    );
                    None
                }
            }
        }
        None => Some((
            docker::DockerHost::Local,
            config.runner_image.clone(),
            false,
        )),
    }
}

/// Records that a repo's reconcile cycle panicked/errored hard, so an
/// operator (or GSR, see `gsr_bridge.rs`) can see the last crash without
/// having to scroll back through logs.
fn record_crash(state_dir: &std::path::Path, message: &str) -> std::io::Result<()> {
    let now = std::time::SystemTime::now();
    StateStore::new(state_dir)
        .record_crash(format!("{now:?}\n{message}"))
        .map_err(|error| std::io::Error::other(error.to_string()))
}

fn kitty_automation_policy(
    config: &Config,
    settings: &gitrun_smart_scheduler::KittySettings,
) -> gitrun_gsr::automation_gate::AutomationPolicy {
    gitrun_gsr::automation_gate::AutomationPolicy {
        kitty_enabled: settings.enabled,
        telemetry_enabled: settings.telemetry_enabled,
        predictive_analytics_enabled: settings.predictive_analytics_enabled,
        predictive_warm_pool_enabled: settings.predictive_warm_pool_enabled,
        shadow_mode: settings.shadow_mode,
        automation_mode: settings.automation_mode,
        smart_fast_stop_enabled: settings.smart_fast_stop_enabled,
        minimum_readiness_score: settings.minimum_quality_score,
        fast_stop_max_runtime_seconds: settings.fast_stop_max_runtime_seconds,
        allowed_repositories: settings.automation_allowed_repositories.clone(),
        configured_repositories: config.repositories.clone(),
    }
}

fn maybe_fast_stop_workflow(
    client: &GitHubClient,
    config: &Config,
    state_dir: &std::path::Path,
    repo: &str,
    settings: &gitrun_smart_scheduler::KittySettings,
) -> Result<bool, Box<dyn std::error::Error>> {
    let threshold = settings.fast_stop_max_runtime_seconds;
    if !(300..=604_800).contains(&threshold) || !settings.smart_fast_stop_enabled {
        return Ok(false);
    }
    if !config
        .repositories
        .iter()
        .any(|configured| configured.eq_ignore_ascii_case(repo))
        || !settings
            .automation_allowed_repositories
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(repo))
    {
        return Ok(false);
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut runs = match client.list_active_workflow_runs(repo) {
        Ok(runs) => runs,
        Err(error) => {
            eprintln!("gitrun-autoscaler: cannot verify active workflow runs for Fast Stop in {repo}: {error}");
            return Ok(false);
        }
    };
    runs.sort_by_key(|run| run.started_at_epoch.unwrap_or(u64::MAX));

    for run in runs {
        let Some(started) = run.started_at_epoch else {
            continue;
        };
        if now < started {
            continue;
        }
        let age = now.saturating_sub(started);
        if age < threshold {
            continue;
        }
        let policy = kitty_automation_policy(config, settings);
        let intent = gitrun_gsr::automation_gate::AutomationIntent {
            action: gitrun_gsr::automation_gate::AutomationAction::FastStopWorkflowRun,
            repository: repo.to_owned(),
            run_id: Some(run.id),
            job_id: None,
            runner_id: None,
            runner_name: None,
            readiness_score: None,
            retaining_previously_authorized_floor: false,
            configured_min_runners: None,
            proposed_min_runners: None,
            max_runners: None,
            runner_is_verified_idle: false,
            runner_is_dock_bound: false,
            workflow_run_is_verified_active: true,
            run_age_seconds: Some(age),
            reason: format!(
                "workflow run {} ({}) exceeded the configured runtime threshold of {} seconds; GitHub cancellation affects the entire run",
                run.id, run.name, threshold
            ),
        };
        let audit_path = gitrun_gsr::events::default_queue_path(&state_dir.to_string_lossy());
        let permit =
            match gitrun_gsr::automation_gate::authorize_action(&policy, intent, &audit_path) {
                Ok(permit) => permit,
                Err(error) => {
                    eprintln!(
                        "gitrun-autoscaler: GSR denied Kitty Fast Stop for {repo} run {}: {error}",
                        run.id
                    );
                    return Ok(false);
                }
            };
        match client.force_cancel_workflow_run(repo, run.id) {
            Ok(()) => {
                if let Err(error) = permit.record_outcome(
                    true,
                    "GitHub accepted force-cancel request for the verified active workflow run",
                ) {
                    eprintln!("gitrun-autoscaler: GSR outcome audit failed after cancelling {repo} run {}: {error}", run.id);
                }
                eprintln!(
                    "gitrun-autoscaler: Kitty Fast Stop cancelled allowlisted workflow run {} for {repo} after {} seconds",
                    run.id, age
                );
                return Ok(true);
            }
            Err(error) => {
                if let Err(audit_error) =
                    permit.record_outcome(false, &format!("GitHub force-cancel failed: {error}"))
                {
                    eprintln!("gitrun-autoscaler: GSR outcome audit also failed for {repo} run {}: {audit_error}", run.id);
                }
                return Err(error.into());
            }
        }
    }
    Ok(false)
}

/// Resource pressure blocks capacity growth, but should not freeze lifecycle
/// safety work. Suppress whole replacement actions (rather than splitting
/// remove/create), and suppress scale-down plans computed assuming deferred
/// creations will succeed.
fn bound_workload_prewarms_to_budget(
    targets: &mut Vec<gitrun_smart_scheduler::WorkloadPrewarm>,
    extra_runner_budget: u32,
) -> u32 {
    let mut remaining = extra_runner_budget;
    for target in targets.iter_mut() {
        target.runner_count = target.runner_count.min(remaining);
        remaining = remaining.saturating_sub(target.runner_count);
    }
    targets.retain(|target| target.runner_count > 0);
    targets.iter().fold(0u32, |count, target| {
        count.saturating_add(target.runner_count)
    })
}

fn actions_for_resource_pressure(
    actions: Vec<reconcile::Action>,
    allow_creation: bool,
) -> Vec<reconcile::Action> {
    if allow_creation {
        return actions;
    }

    actions
        .into_iter()
        .filter(|action| {
            !matches!(
                action,
                reconcile::Action::CreateRunner { .. }
                    | reconcile::Action::PrewarmWorkload { .. }
                    | reconcile::Action::RecreateOrphaned { .. }
                    | reconcile::Action::RemoveIdle { .. }
            )
        })
        .collect()
}

/// Runs one reconcile cycle for a single repository: snapshot, plan, execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KittyObservationCounts {
    queued_jobs: u32,
    busy_runners: u32,
    online_runners: u32,
    managed_runners: u32,
}

/// Forecast only the runner pool Kitty can actually influence. GitDockRun
/// targets are served by a reserved pool and must not inflate demand for the
/// ordinary pool's predictive minimum.
fn kitty_observation_counts(
    queued_job_names: &[String],
    dock_target_jobs: &[String],
    runner_views: &[RunnerView],
    managed_pool_runner_names: &[String],
) -> KittyObservationCounts {
    let bounded_count = |count: usize| count.min(u32::MAX as usize) as u32;
    KittyObservationCounts {
        queued_jobs: bounded_count(
            queued_job_names
                .iter()
                .filter(|job| {
                    let is_known_dock_target = dock_target_jobs.iter().any(|target| target == *job);
                    // The current shared routing contract identifies dock
                    // targets by job name, not by workflow/run ID. If multiple
                    // queued jobs have the same name, do not guess which one
                    // owns the reserved slot: count all of them conservatively.
                    let name_is_unique = queued_job_names
                        .iter()
                        .filter(|candidate| *candidate == *job)
                        .take(2)
                        .count()
                        == 1;
                    !is_known_dock_target || !name_is_unique
                })
                .count(),
        ),
        busy_runners: bounded_count(
            runner_views
                .iter()
                .filter(|runner| {
                    managed_pool_runner_names
                        .iter()
                        .any(|name| name == &runner.name)
                        && runner.busy
                })
                .count(),
        ),
        online_runners: bounded_count(
            runner_views
                .iter()
                .filter(|runner| {
                    managed_pool_runner_names
                        .iter()
                        .any(|name| name == &runner.name)
                        && runner.online
                })
                .count(),
        ),
        managed_runners: bounded_count(managed_pool_runner_names.len()),
    }
}

#[derive(Clone, Copy)]
struct ResourcePressureContext {
    allow_new_runner_creation: bool,
    snapshot: Option<resource_pressure::ResourceSnapshot>,
}

fn reconcile_repo(
    client: &GitHubClient,
    config: &Config,
    state_dir: &std::path::Path,
    vm_registry: &VmResolutionRegistry,
    kitty_runtime: &mut gitrun_smart_scheduler::KittyRuntime,
    repo: &str,
    resource_context: ResourcePressureContext,
    adaptive_resource_budget: &mut AdaptiveResourceBudgetState,
) -> Result<(), Box<dyn std::error::Error>> {
    let ResourcePressureContext {
        allow_new_runner_creation,
        snapshot: host_resources,
    } = resource_context;

    // GTUU replaces permanent runners. Hold its process-shared lifecycle lock
    // across the complete snapshot/plan/execute cycle so an update cannot
    // replace a container between inventory and a destructive scheduler action.
    let _lifecycle_lock = match GtuuLock::acquire(state_dir) {
        Ok(lock) => lock,
        Err(crate::gtuu::GtuuError::AlreadyRunning) => {
            eprintln!("gitrun-autoscaler: deferring reconciliation for {repo}; GTUU owns the runner lifecycle lock");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    api_service::reconcile_dock_bindings(client, state_dir, repo).map_err(std::io::Error::other)?;

    // GitDockRun target metadata is required for correct scheduling even when
    // the advisory workflow scanner is disabled. Refresh it on every reconcile
    // and fail this repository's cycle if GitHub cannot be read or metadata
    // cannot be durably persisted; never route using stale/missing metadata.
    validate_repo_workflows_best_effort(
        client,
        config,
        repo,
        config.gsr_workflow_validation_enabled,
    )
    .map_err(std::io::Error::other)?;

    // Fast Stop is an explicit, allowlisted run deadline. Settings must be
    // trusted and valid; an unreadable policy disables this optional action.
    if let Ok(settings) = gitrun_smart_scheduler::load_settings_for_dashboard(state_dir) {
        if settings.enabled
            && settings.automation_mode
            && settings.smart_fast_stop_enabled
            && settings
                .automation_allowed_repositories
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(repo))
        {
            match maybe_fast_stop_workflow(client, config, state_dir, repo, &settings) {
                Ok(true) => {
                    // Cancellation changes GitHub's live queue, so do not plan
                    // against snapshots taken before the cancellation.
                    return Ok(());
                }
                Ok(false) => {}
                Err(error) => eprintln!(
                    "gitrun-autoscaler: Kitty Fast Stop failed closed for {repo}: {error}"
                ),
            }
        }
    }

    let containers = docker::managed_containers(repo)?;
    // GitDockRun requirements are routing/security metadata: if the file
    // exists but cannot be read or parsed, abort this repo's reconcile cycle
    // rather than silently treating targeted jobs as ordinary local jobs.
    let dock_target_jobs = load_dock_target_jobs(state_dir, repo)?;
    api_service::reconcile_dock_target_containers(client, state_dir, repo, &containers)
        .map_err(std::io::Error::other)?;
    let mut preserved_docks =
        api_service::preserved_dock_containers(state_dir, repo).map_err(std::io::Error::other)?;
    preserved_docks.extend(
        containers
            .iter()
            .filter(|container| {
                container.dock_target
                    && container
                        .workflow_job
                        .as_deref()
                        .map(|job| dock_target_jobs.iter().any(|target| target == job))
                        .unwrap_or(false)
            })
            .map(|container| container.name.clone()),
    );
    let plan_containers: Vec<_> = containers
        .iter()
        .filter(|container| !preserved_docks.contains(&container.name))
        .cloned()
        .collect();
    let runners = client.list_runners(repo)?;
    // A runner registration can disappear while its job is still reported as
    // in progress. Query only these rare orphan cases and mark the planner
    // view busy when GitHub still assigns an active job to that runner name;
    // this prevents recovery from killing a job based on inconsistent snapshots.
    for container in &containers {
        let matches = runners
            .iter()
            .filter(|runner| runner.name == container.name)
            .count();
        if matches > 1 {
            return Err(std::io::Error::other(format!(
                "ambiguous GitHub runner identity for managed container {} in {repo}: {matches} registrations share the name",
                container.name
            )).into());
        }
    }
    // The repository-level GitHub API also returns self-hosted runners that
    // are not owned by GitRun. They must not inflate this scheduler's busy
    // count or be joined to a managed container unless the Docker inventory
    // confirms the exact name exists on this host.
    let mut planner_runners: Vec<RunnerView> = runners
        .iter()
        .filter(|runner| {
            containers
                .iter()
                .any(|container| container.name == runner.name)
        })
        .map(|runner| RunnerView {
            name: runner.name.clone(),
            online: runner.is_online(),
            busy: runner.busy,
        })
        .collect();
    for container in plan_containers.iter().filter(|container| {
        container.status.eq_ignore_ascii_case("running")
            && !runners.iter().any(|runner| runner.name == container.name)
    }) {
        if client
            .find_active_workflow_job_for_runner(repo, &container.name)?
            .is_some()
        {
            planner_runners.push(RunnerView {
                name: container.name.clone(),
                online: false,
                busy: true,
            });
        }
    }
    // Was: client.queued_self_hosted_jobs(repo)? (count only). Switched to
    // the labels-carrying variant so Logic Containers can route dynamic
    // runners per job; `queued_jobs` below is just this list's length, so
    // desired_count()'s formula (busy + queued, clamped) is unchanged.
    let queued_jobs_info = client.queued_self_hosted_jobs_with_info(repo)?;
    let queued_jobs = queued_jobs_info.len() as u32;
    let queued_job_labels: Vec<Vec<String>> = queued_jobs_info
        .iter()
        .map(|job| job.labels.clone())
        .collect();
    let queued_job_names: Vec<String> = queued_jobs_info
        .iter()
        .map(|job| job.name.clone())
        .collect();
    let queued_job_run_ids: Vec<u64> = queued_jobs_info.iter().map(|job| job.run_id).collect();
    let queued_job_ids: Vec<u64> = queued_jobs_info.iter().map(|job| job.job_id).collect();
    let job_occurrences: Vec<gitrun_smart_scheduler::QueuedJobOccurrence> = queued_jobs_info
        .iter()
        // GitDockRun targets have a reserved lifecycle and must never seed
        // ordinary-pool recurring prewarm profiles.
        .filter(|job| !dock_target_jobs.iter().any(|target| target == &job.name))
        .take(512)
        .map(|job| gitrun_smart_scheduler::QueuedJobOccurrence {
            run_id: job.run_id,
            job_id: job.job_id,
            workflow_id: job.workflow_id,
            workflow_path: job.workflow_path.clone(),
            name: job.name.clone(),
            labels: job.labels.clone(),
            started_at_epoch: None,
        })
        .collect();
    let logic_rules = logic_containers::load_rules(&state_dir.join("logic-containers.json"))
        .map_err(|error| {
            std::io::Error::other(format!(
                "invalid Logic Containers configuration for {repo}: {error}"
            ))
        })?;

    let mut state = SchedulerState::load(state_dir)?;
    let tracked_names: Vec<String> = plan_containers
        .iter()
        .filter(|container| !container.status.eq_ignore_ascii_case("exited"))
        .map(|container| container.name.clone())
        .collect();
    let live_names: Vec<String> = plan_containers
        .iter()
        .filter(|container| container.status.eq_ignore_ascii_case("running"))
        .map(|container| container.name.clone())
        .collect();
    // Recovery timestamps must survive for transitional Docker states too;
    // only idle accounting is restricted to running containers.
    state.prune(repo, &tracked_names);
    for name in &tracked_names {
        if !live_names.iter().any(|live_name| live_name == name) {
            // Transitional containers must not retain an old idle timer that
            // could trigger immediate scale-down after they become running again.
            state.clear_idle(name);
        }
    }

    let wall_clock_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let idle: Vec<IdleInfo> = live_names
        .iter()
        .filter(|name| {
            // A predictive reservation is not eligible for idle cleanup before
            // its target plus bounded hold; clear its idle timer until then.
            let prewarm_hold_active = containers
                .iter()
                .find(|container| &container.name == *name)
                .and_then(|container| container.prewarm_hold_until_epoch)
                .is_some_and(|hold_until| wall_clock_epoch < hold_until);
            !prewarm_hold_active
                && runners
                    .iter()
                    .find(|r| &r.name == *name)
                    .map(|r| r.is_online() && !r.busy)
                    .unwrap_or(false)
        })
        .map(|name| IdleInfo {
            name: name.clone(),
            idle_for: state.mark_idle(name),
        })
        .collect();
    for name in &live_names {
        if !idle.iter().any(|i| &i.name == name) {
            state.clear_idle(name);
        }
    }

    // Seed recovery cooldowns from the first observed unhealthy snapshot,
    // including missing registrations. A fresh offline/orphan observation
    // must not trigger an immediate destructive restart/recreation.
    for name in &tracked_names {
        let container_running = live_names.iter().any(|live_name| live_name == name);
        match runners.iter().find(|runner| &runner.name == name) {
            Some(runner) if container_running && runner.is_online() && !runner.busy => {
                state.clear_recovery(name)
            }
            Some(runner) if !runner.busy && (!runner.is_online() || !container_running) => {
                state.recovery_age(name);
            }
            None => {
                state.recovery_age(name);
            }
            _ => {}
        }
    }

    let recovery_age = state.recovery_ages();

    // Kitty models only the ordinary pool affected by its minimum-runner floor.
    // Exclude reserved GitDockRun workloads and containers that reconciliation
    // intentionally keeps outside the ordinary managed pool.
    let managed_pool_runner_names: Vec<String> = plan_containers
        .iter()
        .filter(|container| !container.status.eq_ignore_ascii_case("exited"))
        .map(|container| container.name.clone())
        .collect();
    let kitty_counts = kitty_observation_counts(
        &queued_job_names,
        &dock_target_jobs,
        &planner_runners,
        &managed_pool_runner_names,
    );

    let observed_at_epoch = wall_clock_epoch;
    // Per-job resource learning is a separate, bounded telemetry cadence. Only
    // jobs that GitHub says are assigned to a known GitRun container are sampled.
    // Runner stats are batched so a busy repository doesn't spawn one Docker CLI
    // process per job during the 30-second telemetry cadence.
    if kitty_runtime.should_poll_job_resources(repo, observed_at_epoch) {
        match client.active_self_hosted_jobs_on_runners(repo) {
            Ok(active_jobs) => {
                let mut runner_assignments = std::collections::HashMap::<String, usize>::new();
                for active in &active_jobs {
                    let count = runner_assignments.entry(active.runner_name.clone()).or_default();
                    *count = count.saturating_add(1);
                }

                let mut resource_candidates = Vec::new();
                let mut resource_sample_attempts = 0usize;
                for active in active_jobs {
                    // GitHub snapshots can briefly disagree during retries;
                    // never attribute one container's stats to two active jobs.
                    if runner_assignments.get(&active.runner_name) != Some(&1) {
                        continue;
                    }
                    let Some(container) = containers.iter().find(|container| {
                        container.name == active.runner_name
                            && container.status.eq_ignore_ascii_case("running")
                    }) else {
                        continue;
                    };
                    if resource_sample_attempts >= MAX_ACTIVE_RESOURCE_SAMPLES_PER_REPOSITORY {
                        break;
                    }
                    resource_sample_attempts += 1;
                    resource_candidates.push((active, container.id.clone()));
                }

                let container_ids: Vec<String> = resource_candidates
                    .iter()
                    .map(|(_, container_id)| container_id.clone())
                    .collect();
                let resource_usages = match docker::container_resource_usages(&container_ids) {
                    Ok(usages) => usages,
                    Err(error) => {
                        eprintln!(
                            "gitrun-autoscaler: Kitty batched resource telemetry unavailable for {repo}: {error}"
                        );
                        std::collections::HashMap::new()
                    }
                };
                let mut resource_samples = Vec::with_capacity(resource_candidates.len());
                for (active, container_id) in resource_candidates {
                    let Some(usage) = resource_usages.get(&container_id).copied() else {
                        continue;
                    };
                    resource_samples.push(
                        gitrun_smart_scheduler::ActiveJobResourceSample {
                            job: gitrun_smart_scheduler::QueuedJobOccurrence {
                                run_id: active.job.run_id,
                                job_id: active.job.job_id,
                                workflow_id: active.job.workflow_id,
                                workflow_path: active.job.workflow_path,
                                name: active.job.name,
                                labels: active.job.labels,
                                started_at_epoch: active.job.started_at_epoch,
                            },
                            cpu_cores: usage.cpu_cores,
                            memory_bytes: usage.memory_bytes,
                            sampled_at_epoch: observed_at_epoch,
                        },
                    );
                }
                if let Err(error) = kitty_runtime.record_resource_samples(
                    repo,
                    &resource_samples,
                    observed_at_epoch,
                ) {
                    eprintln!(
                        "gitrun-autoscaler: Kitty resource sample persistence failed for {repo}; runner scheduling remains unchanged: {error}"
                    );
                }
            }
            Err(error) => eprintln!(
                "gitrun-autoscaler: Kitty active-job resource telemetry unavailable for {repo}: {error}"
            ),
        }
    }

    // Historical job-start sampling is deliberately slower than queue
    // reconciliation. API failures affect only Kitty's learning, not scheduling.
    let started_job_occurrences: Vec<gitrun_smart_scheduler::QueuedJobOccurrence> = if kitty_runtime
        .should_poll_workload_history(repo, observed_at_epoch)
    {
        match client.recent_self_hosted_job_starts(repo, 3) {
            Ok(jobs) => jobs
                .into_iter()
                .filter(|job| !dock_target_jobs.iter().any(|target| target == &job.name))
                .take(256)
                .map(|job| gitrun_smart_scheduler::QueuedJobOccurrence {
                    run_id: job.run_id,
                    job_id: job.job_id,
                    workflow_id: job.workflow_id,
                    workflow_path: job.workflow_path,
                    name: job.name,
                    labels: job.labels,
                    started_at_epoch: job.started_at_epoch,
                })
                .collect(),
            Err(error) => {
                eprintln!(
                        "gitrun-autoscaler: Kitty actual-start history sampling failed for {repo}; existing model retained: {error}"
                    );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let mut kitty_decision = match kitty_runtime.observe(
        gitrun_smart_scheduler::ObservationInput {
            repository: repo.to_owned(),
            observed_at_epoch,
            queued_job_occurrences: job_occurrences.clone(),
            started_job_occurrences,
            // recent_self_hosted_job_starts inspects a bounded set of runs; its
            // empty response cannot prove that no job started in an hour.
            started_job_history_complete: false,
            poll_interval_seconds: config.poll_interval,
            queued_jobs: kitty_counts.queued_jobs,
            busy_runners: kitty_counts.busy_runners,
            online_runners: kitty_counts.online_runners,
            managed_runners: kitty_counts.managed_runners,
            host_cpu_percent: host_resources.map(|snapshot| f64::from(snapshot.cpu_percent)),
            host_memory_percent: host_resources.map(|snapshot| f64::from(snapshot.memory_percent)),
            host_disk_percent: host_resources.map(|snapshot| f64::from(snapshot.disk_percent)),
        },
        config.min_runners,
        config.max_runners,
        allow_new_runner_creation,
    ) {
        Ok(decision) => decision,
        Err(error) => {
            eprintln!("gitrun-autoscaler: Kitty unavailable for {repo}; deterministic minimum retained: {error}");
            gitrun_smart_scheduler::KittyDecision::fallback(
                repo,
                config.min_runners,
                format!("Kitty observation/model failed: {error}"),
            )
        }
    };
    let kitty_settings_snapshot = kitty_runtime.settings().clone();
    let can_schedule_workload_prewarms = kitty_settings_snapshot.enabled
        && kitty_settings_snapshot.telemetry_enabled
        && kitty_settings_snapshot.predictive_analytics_enabled
        && kitty_settings_snapshot.predictive_warm_pool_enabled
        && !kitty_settings_snapshot.shadow_mode
        && allow_new_runner_creation
        && kitty_decision
            .confidence_score
            .is_some_and(|score| score >= kitty_settings_snapshot.minimum_quality_score);
    let mut pending_workload_prewarms = if can_schedule_workload_prewarms {
        kitty_decision
            .workload_prewarms
            .iter()
            .filter_map(|target| {
                let existing = containers
                    .iter()
                    .filter(|container| {
                        !container.status.eq_ignore_ascii_case("exited")
                            && container.prewarm_workload_id.as_deref()
                                == Some(target.workload_id.as_str())
                            && container.prewarm_target_epoch == Some(target.target_epoch)
                    })
                    .count()
                    .min(u32::MAX as usize) as u32;
                // A created runner slot remains fulfilled even when its
                // ephemeral container later exits. Live inventory covers older
                // runners created before this completion counter was persisted.
                let fulfilled = existing.max(target.prewarm_runners_created);
                let missing = target.runner_count.saturating_sub(fulfilled);
                (missing > 0).then(|| {
                    let mut pending = target.clone();
                    pending.runner_count = missing;
                    pending
                })
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    // The operator's extra-runner cap applies to the combined predictive
    // floor and per-workload prewarm count, not to each feature separately.
    let configured_floor = config.min_runners.min(config.max_runners);
    let floor_extra = kitty_decision
        .effective_min_runners
        .saturating_sub(configured_floor);
    let prewarm_runner_count = bound_workload_prewarms_to_budget(
        &mut pending_workload_prewarms,
        kitty_settings_snapshot
            .max_additional_warm_runners
            .saturating_sub(floor_extra),
    );
    let floor_growth_requested = matches!(
        kitty_decision.status,
        gitrun_smart_scheduler::DecisionStatus::AppliedWarmPoolFloor
            | gitrun_smart_scheduler::DecisionStatus::WarmPoolHold
    ) && kitty_decision.effective_min_runners > config.min_runners;
    let workload_prewarm_requested = !pending_workload_prewarms.is_empty();
    let mut kitty_automation_permit = None;
    if floor_growth_requested || workload_prewarm_requested {
        let settings = &kitty_settings_snapshot;
        let policy = gitrun_gsr::automation_gate::AutomationPolicy {
            kitty_enabled: settings.enabled,
            telemetry_enabled: settings.telemetry_enabled,
            predictive_analytics_enabled: settings.predictive_analytics_enabled,
            predictive_warm_pool_enabled: settings.predictive_warm_pool_enabled,
            shadow_mode: settings.shadow_mode,
            automation_mode: settings.automation_mode,
            smart_fast_stop_enabled: settings.smart_fast_stop_enabled,
            minimum_readiness_score: settings.minimum_quality_score,
            fast_stop_max_runtime_seconds: settings.fast_stop_max_runtime_seconds,
            allowed_repositories: settings.automation_allowed_repositories.clone(),
            configured_repositories: config.repositories.clone(),
        };
        let intent = gitrun_gsr::automation_gate::AutomationIntent {
            action: gitrun_gsr::automation_gate::AutomationAction::PredictiveWarmPool,
            repository: repo.to_owned(),
            run_id: None,
            job_id: None,
            runner_id: None,
            runner_name: None,
            readiness_score: kitty_decision.confidence_score,
            retaining_previously_authorized_floor: kitty_decision.status
                == gitrun_smart_scheduler::DecisionStatus::WarmPoolHold,
            configured_min_runners: Some(config.min_runners),
            proposed_min_runners: Some(
                kitty_decision
                    .effective_min_runners
                    .max(config.min_runners.min(config.max_runners))
                    .saturating_add(prewarm_runner_count)
                    .min(config.max_runners),
            ),
            max_runners: Some(config.max_runners),
            runner_is_verified_idle: false,
            runner_is_dock_bound: false,
            workflow_run_is_verified_active: false,
            run_age_seconds: None,
            reason: {
                let workload_summary = pending_workload_prewarms
                    .iter()
                    .map(|target| {
                        format!(
                            "{}x{}@{}",
                            target.workload_id, target.runner_count, target.target_epoch
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let combined = if workload_summary.is_empty() {
                    kitty_decision.reason.clone()
                } else {
                    format!(
                        "{}; recurring workload prewarm request: {}",
                        kitty_decision.reason, workload_summary
                    )
                };
                combined.chars().take(800).collect()
            },
        };
        let authorization_reason = intent.reason.clone();
        let audit_path = gitrun_gsr::events::default_queue_path(&state_dir.to_string_lossy());
        match gitrun_gsr::automation_gate::authorize_action(&policy, intent, &audit_path) {
            Ok(permit) => {
                match kitty_runtime.record_gsr_authorization(
                    &mut kitty_decision,
                    true,
                    config.min_runners.min(config.max_runners),
                    &authorization_reason,
                ) {
                    Ok(()) => kitty_automation_permit = Some(permit),
                    Err(error) => {
                        let reason = format!(
                            "GSR grant could not be committed to the Kitty model audit: {error}"
                        );
                        let _ = permit.record_outcome(false, &reason);
                        kitty_decision.status =
                            gitrun_smart_scheduler::DecisionStatus::SafeFallback;
                        kitty_decision.effective_min_runners =
                            config.min_runners.min(config.max_runners);
                        kitty_decision.reason = reason;
                    }
                }
            }
            Err(error) => {
                let reason = error.to_string();
                if let Err(persist_error) = kitty_runtime.record_gsr_authorization(
                    &mut kitty_decision,
                    false,
                    config.min_runners.min(config.max_runners),
                    &reason,
                ) {
                    eprintln!("gitrun-autoscaler: failed to persist GSR denial in Kitty audit for {repo}: {persist_error}");
                    kitty_decision.status = gitrun_smart_scheduler::DecisionStatus::SafeFallback;
                    kitty_decision.effective_min_runners =
                        config.min_runners.min(config.max_runners);
                    kitty_decision.reason = format!(
                        "GSR denied the predictive floor and the Kitty audit could not be updated: {reason}"
                    );
                }
            }
        }
    }
    let effective_min_runners = if config.min_runners <= config.max_runners {
        kitty_decision
            .effective_min_runners
            .max(config.min_runners)
            .min(config.max_runners)
    } else {
        config.min_runners
    };

    let mut routed_workload_capacity = std::collections::HashMap::<String, u32>::new();
    for container in containers.iter().filter(|container| {
        !container.status.eq_ignore_ascii_case("exited")
            && !container.permanent
            && !container.dock_target
    }) {
        let Some(workload_id) = container.workload_routing_id.as_ref() else {
            continue;
        };
        let registered_runner = planner_runners
            .iter()
            .find(|runner| runner.name == container.name);
        let unavailable = registered_runner.is_some_and(|runner| {
            runner.busy || (container.status.eq_ignore_ascii_case("running") && !runner.online)
        });
        if !unavailable {
            let count = routed_workload_capacity
                .entry(workload_id.clone())
                .or_default();
            *count = count.saturating_add(1);
        }
    }
    let routed_workload_capacity: Vec<(String, u32)> =
        routed_workload_capacity.into_iter().collect();

    let input = ReconcileInput {
        min_runners: effective_min_runners,
        max_runners: config.max_runners,
        reserved_runners: preserved_docks
            .iter()
            .filter(|name| {
                containers.iter().any(|container| {
                    &container.name == *name && !container.status.eq_ignore_ascii_case("exited")
                })
            })
            .count()
            .min(u32::MAX as usize) as u32,
        containers: plan_containers.iter().map(to_container_view).collect(),
        runners: planner_runners,
        queued_jobs,
        queued_job_labels,
        queued_job_names,
        queued_job_run_ids,
        queued_job_ids,
        routed_workload_capacity,
        workload_prewarms: if kitty_automation_permit.is_some() {
            std::mem::take(&mut pending_workload_prewarms)
        } else {
            Vec::new()
        },
        dock_target_jobs,
        logic_rules,
        idle,
        recovery_enabled: config.auto_container_recovery,
        recovery_cooldown: Duration::from_secs(config.container_recovery_cooldown),
        recovery_age,
        ephemeral: config.ephemeral,
        idle_timeout: Duration::from_secs(config.idle_timeout),
    };

    let actions = actions_for_resource_pressure(reconcile::plan(&input), allow_new_runner_creation);
    let has_explicit_workload_route = input
        .queued_job_labels
        .iter()
        .any(|labels| gitrun_smart_scheduler::has_workload_routing_marker(labels))
        || containers
            .iter()
            .any(|container| container.workload_routing_id.is_some());
    let workload_routing_profiles = if has_explicit_workload_route {
        match kitty_runtime.workload_routing_profiles(repo) {
            Ok(profiles) => profiles,
            Err(error) => {
                eprintln!(
                    "gitrun-autoscaler: cannot load the validated Kitty workload profile map for routed jobs in {repo}: {error}"
                );
                std::collections::HashMap::new()
            }
        }
    } else {
        std::collections::HashMap::new()
    };
    let execution_context = ExecuteContext {
        client,
        config,
        state_dir,
        vm_registry,
        repo,
        workload_routing_profiles: &workload_routing_profiles,
    };
    let adaptive_resources_enabled = adaptive_resource_budget.enabled;

    let planned_action_count = actions.len();
    let mut action_execution_succeeded = true;
    for action in actions {
        // Resolve a per-workload envelope before any runner-creating action.
        // A failed measurement, invalid estimate or exhausted host budget
        // prevents creation; the static scheduler remains unchanged when the
        // operator has not enabled Adaptive Resource Management.
        // A replacement frees the old container's unused hard-limit reservation.
        // If replacement fails, this plan stops and the next tick re-probes everything.
        if let Action::RecreateOrphaned { name, .. } = &action {
            adaptive_resource_budget.release_container_reservation(name);
        }
        let allocation_attempt: Result<Option<RunnerResourceAllocation>, String> =
            if adaptive_resources_enabled {
                match adaptive_resource_identity(&action, &containers) {
                    None => Ok(None),
                    Some((job_name, job_labels)) => {
                        let attempt = (|| {
                            ensure_adaptive_resource_backend_is_local(state_dir, &job_labels)?;
                            let estimate = kitty_runtime
                                .resource_demand_estimate(repo, "", &job_name, &job_labels)
                                .ok_or_else(|| {
                                    "Kitty could not produce a validated resource estimate; refusing to guess CPU/RAM".to_owned()
                                })?;
                            adaptive_resource_budget.allocate(estimate, config).map(Some)
                        })();
                        attempt
                    }
                }
            } else {
                Ok(None)
            };
        let resource_allocation = match allocation_attempt {
            Ok(allocation) => allocation,
            Err(error) => {
                eprintln!(
                    "gitrun-autoscaler: Kitty Adaptive Resource Management deferred {action:?} for {repo}: {error}"
                );
                action_execution_succeeded = false;
                break;
            }
        };
        if let Some(allocation) = resource_allocation.as_ref() {
            eprintln!(
                "gitrun-autoscaler: Kitty allocated {} CPU and {} bytes RAM to workload {} for {repo} (learned={}, reason={})",
                allocation.cpus,
                allocation.memory,
                allocation.workload_id,
                allocation.learned,
                allocation.reason
            );
        }

        // The plan is ordered and later actions may rely on earlier removals
        // or creations having succeeded (for example, RemoveExited followed
        // by CreateRunner assumes the exited container was actually removed).
        // Stop this repo's plan at the first failure and re-snapshot/replan on
        // the next tick; continuing can exceed capacity or act on stale state.
        match execute(
            &execution_context,
            &containers,
            &action,
            &mut state,
            resource_allocation.as_ref(),
        ) {
            Ok(Some((workload_id, target_epoch))) => {
                if let Err(error) = kitty_runtime.record_workload_prewarm_created(
                    repo,
                    &workload_id,
                    target_epoch,
                    1,
                ) {
                    eprintln!(
                        "gitrun-autoscaler: prewarm runner was created for {repo}/{workload_id}, but its durable completion record failed: {error}"
                    );
                    action_execution_succeeded = false;
                    break;
                }
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("gitrun-autoscaler: action {action:?} failed for {repo}; aborting remaining planned actions until next reconcile: {error}");
                action_execution_succeeded = false;
                break;
            }
        }
    }

    // Kitty audit persistence is deliberately non-fatal: if its storage fails,
    // the next cycle retains the deterministic scheduler path and configured floor.
    if let Err(error) = kitty_runtime.finish_cycle(
        &kitty_decision,
        planned_action_count,
        action_execution_succeeded,
    ) {
        eprintln!("gitrun-autoscaler: Kitty audit persistence failed for {repo}; scheduler remains deterministic next cycle: {error}");
    }

    if let Some(permit) = kitty_automation_permit {
        let outcome = if action_execution_succeeded {
            format!(
                "scheduler completed {} planned actions",
                planned_action_count
            )
        } else {
            "scheduler stopped after an action error; next reconcile will re-snapshot".into()
        };
        if let Err(error) = permit.record_outcome(action_execution_succeeded, &outcome) {
            eprintln!("gitrun-autoscaler: GSR could not persist Kitty warm-pool action outcome for {repo}: {error}");
        }
    }

    state.save()?;
    Ok(())
}

fn to_container_view(container: &ManagedContainer) -> ContainerView {
    ContainerView {
        name: container.name.clone(),
        // Docker has several states besides "running"/"exited"
        // (created, restarting, paused, dead, ...). Only the exact
        // "running" state is healthy enough to be treated as live.
        health: match container.status.as_str() {
            "running" => ContainerHealth::Running,
            "exited" => ContainerHealth::Exited,
            _ => ContainerHealth::Starting,
        },
        permanent: container.permanent,
    }
}

struct AdaptiveResourceBudgetState {
    enabled: bool,
    budget: Option<AdaptiveResourceBudget>,
    unavailable_reason: Option<String>,
}

impl AdaptiveResourceBudgetState {
    fn for_cycle(state_dir: &std::path::Path) -> Self {
        let settings = match gitrun_smart_scheduler::load_settings_for_dashboard(state_dir) {
            Ok(settings) => settings,
            Err(_) => {
                // Invalid/unreadable optional settings never enable adaptive
                // behavior; Kitty's validated runtime will report the error.
                return Self { enabled: false, budget: None, unavailable_reason: None };
            }
        };
        if !settings.adaptive_resource_management_enabled {
            return Self { enabled: false, budget: None, unavailable_reason: None };
        }

        // A single capacity sample and a single reservation snapshot are shared
        // across all configured repositories for the complete scheduler tick.
        let capacity = match resource_pressure::available_capacity() {
            Ok(capacity) => capacity,
            Err(error) => {
                return Self {
                    enabled: true,
                    budget: None,
                    unavailable_reason: Some(error.to_string()),
                };
            }
        };
        let reservations = match docker::running_managed_container_resource_reservations() {
            Ok(reservations) => reservations,
            Err(error) => {
                return Self {
                    enabled: true,
                    budget: None,
                    unavailable_reason: Some(format!(
                        "existing GitRun runner limits could not be measured safely: {error}"
                    )),
                };
            }
        };

        match AdaptiveResourceBudget::from_capacity(capacity, reservations) {
            Some(budget) => Self {
                enabled: true,
                budget: Some(budget),
                unavailable_reason: None,
            },
            None => Self {
                enabled: true,
                budget: None,
                unavailable_reason: Some(
                    "existing runner reservations were invalid or outside safe bounds".into(),
                ),
            },
        }
    }

    fn allocate(
        &mut self,
        estimate: gitrun_smart_scheduler::ResourceDemandEstimate,
        config: &Config,
    ) -> Result<RunnerResourceAllocation, String> {
        if self.budget.is_none() {
            return Err(format!(
                "host capacity unavailable{}",
                self.unavailable_reason
                    .as_deref()
                    .map(|error| format!(": {error}"))
                    .unwrap_or_default()
            ));
        }
        self.budget.as_mut().ok_or_else(|| {
            "host resource budget disappeared unexpectedly".to_owned()
        })?.allocate(estimate, config).ok_or_else(|| {
            "safe host CPU/RAM budget or operator ceilings are too small for another runner; deferring creation".to_owned()
        })
    }

    fn release_container_reservation(&mut self, name: &str) {
        if let Some(budget) = self.budget.as_mut() {
            budget.release_container_reservation(name);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct RunnerResourceAllocation {
    cpus: String,
    memory: String,
    workload_id: String,
    learned: bool,
    reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ContainerResourceReservation {
    cpu_cores: f64,
    memory_bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct AdaptiveResourceBudget {
    capacity_cpu_cores: f64,
    capacity_memory_bytes: u64,
    reserved_cpu_cores: f64,
    reserved_memory_bytes: u64,
    allocated_cpu_cores: f64,
    allocated_memory_bytes: u64,
    reservations_by_name: std::collections::HashMap<String, ContainerResourceReservation>,
}

impl AdaptiveResourceBudget {
    fn from_capacity(
        capacity: resource_pressure::ResourceCapacity,
        reservations: Vec<docker::ManagedContainerResourceReservation>,
    ) -> Option<Self> {
        if !capacity.cpu_cores.is_finite()
            || capacity.cpu_cores < 0.0
            || capacity.memory_bytes > (1_u64 << 62)
        {
            return None;
        }
        let mut reservations_by_name = std::collections::HashMap::new();
        let mut reserved_cpu_cores = 0.0_f64;
        let mut reserved_memory_bytes = 0_u64;
        for reservation in reservations {
            if reservation.name.trim().is_empty()
                || !reservation.cpu_headroom_cores.is_finite()
                || reservation.cpu_headroom_cores < 0.0
                || reservation.memory_headroom_bytes > (1_u64 << 60)
            {
                return None;
            }
            reserved_cpu_cores += reservation.cpu_headroom_cores;
            if !reserved_cpu_cores.is_finite() {
                return None;
            }
            reserved_memory_bytes = reserved_memory_bytes.checked_add(reservation.memory_headroom_bytes)?;
            if reserved_memory_bytes > (1_u64 << 62)
                || reservations_by_name.insert(
                    reservation.name,
                    ContainerResourceReservation {
                        cpu_cores: reservation.cpu_headroom_cores,
                        memory_bytes: reservation.memory_headroom_bytes,
                    },
                ).is_some()
            {
                return None;
            }
        }

        Some(Self {
            capacity_cpu_cores: capacity.cpu_cores,
            capacity_memory_bytes: capacity.memory_bytes,
            reserved_cpu_cores,
            reserved_memory_bytes,
            allocated_cpu_cores: 0.0,
            allocated_memory_bytes: 0,
            reservations_by_name,
        })
    }

    fn available_cpu_cores(&self) -> f64 {
        (self.capacity_cpu_cores - self.reserved_cpu_cores - self.allocated_cpu_cores).max(0.0)
    }

    fn available_memory_bytes(&self) -> u64 {
        self.capacity_memory_bytes
            .saturating_sub(self.reserved_memory_bytes)
            .saturating_sub(self.allocated_memory_bytes)
    }

    fn release_container_reservation(&mut self, name: &str) {
        if let Some(reservation) = self.reservations_by_name.remove(name) {
            self.reserved_cpu_cores = (self.reserved_cpu_cores - reservation.cpu_cores).max(0.0);
            self.reserved_memory_bytes =
                self.reserved_memory_bytes.saturating_sub(reservation.memory_bytes);
        }
    }

    fn allocate(
        &mut self,
        estimate: gitrun_smart_scheduler::ResourceDemandEstimate,
        config: &Config,
    ) -> Option<RunnerResourceAllocation> {
        const MIN_CPU_CORES: f64 = 0.25;
        const MIN_MEMORY_BYTES: u64 = 512 * 1024 * 1024;
        if !estimate.cpu_cores.is_finite()
            || estimate.cpu_cores <= 0.0
            || estimate.memory_bytes == 0
            || estimate.memory_bytes > (1_u64 << 60)
        {
            return None;
        }

        let operator_cpu_cap = config.container_cpus.parse::<f64>().ok()?;
        let operator_memory_cap = docker::parse_docker_bytes(&config.container_memory)?;
        if !operator_cpu_cap.is_finite()
            || operator_cpu_cap <= 0.0
            || operator_cpu_cap > 256.0
            || operator_memory_cap == 0
        {
            return None;
        }

        // Configured limits are hard ceilings. Unused headroom in already-running
        // managed containers is deducted, then each new allocation consumes the
        // same budget shared by all repositories during this scheduler tick.
        let requested_cpu = estimate
            .cpu_cores
            .min(operator_cpu_cap)
            .min(self.available_cpu_cores());
        let memory_bytes = estimate
            .memory_bytes
            .min(operator_memory_cap)
            .min(self.available_memory_bytes());
        if !requested_cpu.is_finite()
            || requested_cpu < MIN_CPU_CORES
            || memory_bytes < MIN_MEMORY_BYTES
        {
            return None;
        }

        // Docker's --cpus value is rounded down to millicores. Never round an
        // allocation upward beyond the available shared budget.
        let milli_cores = (requested_cpu * 1000.0).floor() as u64;
        let cpu_cores = milli_cores as f64 / 1000.0;
        if cpu_cores < MIN_CPU_CORES {
            return None;
        }
        let next_allocated_memory = self.allocated_memory_bytes.checked_add(memory_bytes)?;
        let next_allocated_cpu = self.allocated_cpu_cores + cpu_cores;
        if !next_allocated_cpu.is_finite() {
            return None;
        }

        self.allocated_cpu_cores = next_allocated_cpu;
        self.allocated_memory_bytes = next_allocated_memory;
        let cpu_limit = format!("{cpu_cores:.3}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned();
        Some(RunnerResourceAllocation {
            cpus: cpu_limit,
            memory: memory_bytes.to_string(),
            workload_id: estimate.workload_id,
            learned: estimate.learned,
            reason: estimate.reason,
        })
    }
}

fn ensure_adaptive_resource_backend_is_local(
    state_dir: &std::path::Path,
    job_labels: &[String],
) -> Result<(), String> {
    let rules = logic_containers::load_rules(&state_dir.join("logic-containers.json"))
        .map_err(|error| format!("could not verify Logic Containers backend: {error}"))?;
    if let Some((logic_containers::Backend::Vm { vm_name }, _)) =
        logic_containers::resolve(&rules, job_labels)
    {
        return Err(format!(
            "VM-backed workload '{vm_name}' requires a per-VM resource probe; refusing to use miniserver host capacity"
        ));
    }
    Ok(())
}

fn adaptive_resource_identity(
    action: &Action,
    inventory: &[docker::ManagedContainer],
) -> Option<(String, Vec<String>)> {
    match action {
        Action::CreateRunner { job_name, job_labels, .. } => Some((
            job_name.clone().unwrap_or_else(|| "general runner".into()),
            job_labels.clone(),
        )),
        Action::PrewarmWorkload { job_name, job_labels, .. } => {
            Some((job_name.clone(), job_labels.clone()))
        }
        Action::RecreateOrphaned { name, .. } => {
            let container = inventory.iter().find(|container| container.name == *name)?;
            let labels = container
                .workload_routing_id
                .as_ref()
                .map(|id| vec![format!("gitrun-job-{id}")])
                .unwrap_or_default();
            Some((
                container
                    .workflow_job
                    .clone()
                    .unwrap_or_else(|| "recovered runner".into()),
                labels,
            ))
        }
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct ExecuteContext<'a> {
    client: &'a GitHubClient,
    config: &'a Config,
    state_dir: &'a std::path::Path,
    vm_registry: &'a VmResolutionRegistry,
    repo: &'a str,
    workload_routing_profiles: &'a std::collections::HashMap<String, Vec<String>>,
}

fn execute(
    context: &ExecuteContext<'_>,
    inventory: &[docker::ManagedContainer],
    action: &Action,
    state: &mut SchedulerState,
    resource_allocation: Option<&RunnerResourceAllocation>,
) -> Result<Option<(String, u64)>, Box<dyn std::error::Error>> {
    let ExecuteContext {
        client,
        config,
        state_dir,
        vm_registry,
        repo,
        workload_routing_profiles,
    } = *context;
    match action {
        Action::RemoveExited { name } => {
            ensure_not_dock_bound(state_dir, repo, name, "remove exited container")?;
            let expected_id = expected_container_id(inventory, name)?;
            ensure_current_container_identity(name, expected_id)?;
            // An exited runner crosses a trust boundary: never restart/reuse
            // its filesystem or credentials. Quarantine the identity first,
            // then remove the container and create a fresh runner if capacity
            // is still required.
            state.quarantine(name);
            let events_path = gitrun_gsr::events::default_queue_path(&state_dir.to_string_lossy());
            let event = gitrun_gsr::SecurityEvent::new(
                "scheduler",
                gitrun_gsr::Severity::Critical,
                format!("runner {name} for {repo} exited unexpectedly and was quarantined; a fresh runner must be created"),
            );
            if let Err(error) = gitrun_gsr::events::emit(&events_path, &event) {
                eprintln!("gitrun-autoscaler: failed to emit runner quarantine event: {error}");
            }
            deregister_and_remove(client, repo, name, expected_id)?;
        }
        Action::CreateRunner {
            permanent,
            job_labels,
            job_name,
            job_run_id,
            job_id,
        } => {
            if !create_runner(
                client,
                config,
                state_dir,
                vm_registry,
                repo,
                workload_routing_profiles,
                RunnerCreateContext {
                    permanent: *permanent,
                    job_name: job_name.as_deref(),
                    job_run_id: *job_run_id,
                    job_id: *job_id,
                    job_labels,
                    prewarm_workload_id: None,
                    prewarm_target_epoch: None,
                    prewarm_hold_until_epoch: None,
                    resource_allocation,
                },
            )? {
                return Err(std::io::Error::other(
                    "runner creation was skipped by GSR policy or backend readiness",
                )
                .into());
            }
        }
        Action::PrewarmWorkload {
            workload_id,
            target_epoch,
            hold_until_epoch,
            job_name,
            job_labels,
        } => {
            if !create_runner(
                client,
                config,
                state_dir,
                vm_registry,
                repo,
                workload_routing_profiles,
                RunnerCreateContext {
                    permanent: false,
                    // Do not turn a predictive profile into a GitDockRun
                    // lifecycle binding. Labels select the backend/image;
                    // these fields only annotate this prewarm reservation.
                    job_name: None,
                    job_run_id: None,
                    job_id: None,
                    job_labels,
                    prewarm_workload_id: Some(workload_id),
                    prewarm_target_epoch: Some(*target_epoch),
                    prewarm_hold_until_epoch: Some(*hold_until_epoch),
                    resource_allocation,
                },
            )? {
                return Err(std::io::Error::other(
                    "Kitty prewarm runner creation was skipped by GSR policy or backend readiness",
                )
                .into());
            }
            eprintln!(
                "gitrun-autoscaler: prewarmed workload {} ({}) for {} at target epoch {}",
                workload_id, job_name, repo, target_epoch
            );
            return Ok(Some((workload_id.clone(), *target_epoch)));
        }
        Action::RecreateOrphaned { name, permanent } => {
            ensure_not_dock_bound(state_dir, repo, name, "recreate orphaned runner")?;
            let expected_id = expected_container_id(inventory, name)?;
            ensure_current_container_identity(name, expected_id)?;
            let prior = inventory.iter().find(|container| container.name == *name);
            let route_labels = prior
                .and_then(|container| container.workload_routing_id.as_deref())
                .map(|workload_id| vec![format!("gitrun-job-{workload_id}")]);
            let job_labels = route_labels.as_deref().unwrap_or(&[]);
            let prewarm_workload_id =
                prior.and_then(|container| container.prewarm_workload_id.as_deref());
            let prewarm_target_epoch = prior.and_then(|container| container.prewarm_target_epoch);
            let prewarm_hold_until_epoch =
                prior.and_then(|container| container.prewarm_hold_until_epoch);
            deregister_and_remove(client, repo, name, expected_id)?;
            // Preserve the validated workload family/profile on recovery,
            // rather than silently turning a routed runner into a generic one.
            if !create_runner(
                client,
                config,
                state_dir,
                vm_registry,
                repo,
                workload_routing_profiles,
                RunnerCreateContext {
                    permanent: *permanent,
                    job_name: None,
                    job_run_id: None,
                    job_id: None,
                    job_labels,
                    prewarm_workload_id,
                    prewarm_target_epoch,
                    prewarm_hold_until_epoch,
                    resource_allocation,
                },
            )? {
                return Err(std::io::Error::other(
                    "orphaned runner replacement was skipped by GSR policy or backend readiness",
                )
                .into());
            }
            state.clear_recovery(name);
        }
        Action::RestartUnresponsive { name } => {
            ensure_not_dock_bound(state_dir, repo, name, "restart unresponsive runner")?;
            let expected_id = expected_container_id(inventory, name)?;
            ensure_current_container_identity(name, expected_id)?;
            // Track when this container first needed a restart, so the
            // cooldown in ReconcileInput.recovery_age is meaningful on the
            // *next* cycle. This call also seeds the timer on first sight.
            state.recovery_age(name);
            docker::restart_container(expected_id)?;
        }
        Action::RemoveIdle { name } => {
            ensure_not_dock_bound(state_dir, repo, name, "remove idle runner")?;
            let expected_id = expected_container_id(inventory, name)?;
            ensure_current_container_identity(name, expected_id)?;

            // In Automation mode, an idle-runner stop in an allowlisted repo
            // requires a fresh GitHub runner snapshot and a GSR permit before
            // destructive work. The normal deterministic idle cleanup remains
            // unchanged outside this explicit automation scope.
            let kitty_settings = match gitrun_smart_scheduler::load_settings_for_dashboard(
                state_dir,
            ) {
                Ok(settings) => settings,
                Err(error) => {
                    // A malformed/untrusted policy must not silently turn
                    // an automatic destructive path into the legacy path.
                    eprintln!(
                            "gitrun-autoscaler: preserving idle runner {name} for {repo}; Kitty settings could not be verified before removal: {error}"
                        );
                    return Ok(None);
                }
            };
            let automation_repository_allowed = kitty_settings
                .automation_allowed_repositories
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(repo));
            if kitty_settings.automation_mode && !automation_repository_allowed {
                eprintln!(
                    "gitrun-autoscaler: Kitty Stop withheld for {repo}/{name}: repository is not on the explicit Automation allowlist"
                );
                state.clear_idle(name);
                return Ok(None);
            }
            let automation_scope = kitty_settings.automation_mode && automation_repository_allowed;
            if automation_scope {
                let registered_runner = client
                    .list_runners(repo)?
                    .into_iter()
                    .find(|runner| runner.name == *name);
                if let Some(runner) =
                    registered_runner.filter(|runner| runner.is_online() && !runner.busy)
                {
                    let policy = kitty_automation_policy(config, &kitty_settings);
                    let intent = gitrun_gsr::automation_gate::AutomationIntent {
                        action: gitrun_gsr::automation_gate::AutomationAction::StopIdleRunner,
                        repository: repo.to_owned(),
                        run_id: None,
                        job_id: None,
                        runner_id: Some(runner.id),
                        runner_name: Some(name.clone()),
                        readiness_score: None,
                        retaining_previously_authorized_floor: false,
                        configured_min_runners: None,
                        proposed_min_runners: None,
                        max_runners: None,
                        runner_is_verified_idle: true,
                        runner_is_dock_bound: false,
                        workflow_run_is_verified_active: false,
                        run_age_seconds: None,
                        reason: format!(
                            "deterministic planner selected online idle runner {name} for scale-down"
                        ),
                    };
                    let audit_path =
                        gitrun_gsr::events::default_queue_path(&state_dir.to_string_lossy());
                    let permit =
                        gitrun_gsr::automation_gate::authorize_action(&policy, intent, &audit_path)
                            .map_err(|error| std::io::Error::other(error.to_string()))?;
                    let removed =
                        remove_if_still_idle(client, repo, name, expected_id, Some(runner.id))?;
                    if let Err(error) = permit.record_outcome(
                        removed,
                        if removed {
                            "verified idle runner deregistered and container removed"
                        } else {
                            "runner state changed before deletion; container preserved"
                        },
                    ) {
                        eprintln!(
                            "gitrun-autoscaler: GSR could not persist Stop outcome for {repo}/{name}: {error}"
                        );
                    }
                    state.clear_idle(name);
                    return Ok(None);
                } else {
                    // Once Automation is opted in for this repository, missing,
                    // offline, or busy GitHub state must not fall through to the
                    // legacy un-gated removal path.
                    eprintln!(
                        "gitrun-autoscaler: Kitty Stop withheld for {repo}/{name}: runner was not freshly verified online and idle by GitHub"
                    );
                    state.clear_idle(name);
                    return Ok(None);
                }
            }

            let _ = remove_if_still_idle(client, repo, name, expected_id, None)?;
            state.clear_idle(name);
        }
    }
    Ok(None)
}

/// A bound GitDockRun target is owned by the active dock lifecycle, not
/// ordinary autoscaler recovery. Refuse destructive actions explicitly so
/// reconciliation stops before later actions can overprovision around a
/// container it deliberately left in place.
fn ensure_not_dock_bound(
    state_dir: &std::path::Path,
    repo: &str,
    name: &str,
    operation: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !api_service::is_dock_bound(state_dir, repo, name).map_err(std::io::Error::other)? {
        return Ok(());
    }
    let message = format!(
        "refusing to {operation} GitDockRun-bound runner {name} for {repo}; dock lifecycle ownership must be resolved first"
    );
    let events_path = gitrun_gsr::events::default_queue_path(&state_dir.to_string_lossy());
    let event = gitrun_gsr::SecurityEvent::new(
        "scheduler",
        gitrun_gsr::Severity::Critical,
        message.clone(),
    );
    if let Err(error) = gitrun_gsr::events::emit(&events_path, &event) {
        eprintln!("gitrun-autoscaler: {message} (failed to emit GSR event: {error})");
    }
    Err(std::io::Error::other(message).into())
}

fn expected_container_id<'a>(
    inventory: &'a [docker::ManagedContainer],
    name: &str,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    inventory
        .iter()
        .find(|container| container.name == name)
        .map(|container| container.id.as_str())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            std::io::Error::other(format!(
            "refusing destructive scheduler action: no immutable Docker ID in inventory for {name}"
        ))
            .into()
        })
}

fn check_container_identity(
    name: &str,
    expected_id: &str,
    current_id: Option<&str>,
) -> Result<(), String> {
    match current_id {
        Some(current_id) if current_id == expected_id => Ok(()),
        Some(current_id) => Err(format!(
            "refusing destructive scheduler action for {name}: Docker identity changed from {expected_id} to {current_id}"
        )),
        None => Err(format!(
            "refusing destructive scheduler action for {name}: inventoried container {expected_id} no longer exists"
        )),
    }
}

fn ensure_current_container_identity(
    name: &str,
    expected_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let current_id = docker::container_id(name)?;
    check_container_identity(name, expected_id, current_id.as_deref())
        .map_err(std::io::Error::other)?;
    Ok(())
}

fn deregister_and_remove(
    client: &GitHubClient,
    repo: &str,
    name: &str,
    expected_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Recheck the mutable name before touching GitHub registration. Docker
    // removal itself is always pinned to the immutable ID captured at snapshot.
    ensure_current_container_identity(name, expected_id)?;
    // Do not remove the container if GitHub cannot be queried: otherwise
    // a transient API failure can leave an orphaned runner registration behind.
    let runners = client.list_runners(repo)?;
    if let Some(runner) = runners.iter().find(|r| r.name == name) {
        ensure_current_container_identity(name, expected_id)?;
        client.delete_runner(repo, runner.id)?;
    }
    docker::remove_container(expected_id)?;
    Ok(())
}

fn expected_runner_matches_authorized_id(
    expected_runner_id: Option<u64>,
    observed_runner_id: u64,
) -> bool {
    expected_runner_id.is_none_or(|expected| expected == observed_runner_id)
}

fn remove_if_still_idle(
    client: &GitHubClient,
    repo: &str,
    name: &str,
    expected_id: &str,
    expected_runner_id: Option<u64>,
) -> Result<bool, Box<dyn std::error::Error>> {
    ensure_current_container_identity(name, expected_id)?;
    let runner = client
        .list_runners(repo)?
        .into_iter()
        .find(|runner| runner.name == name);

    let Some(runner) = runner else {
        // The registration disappeared on its own; this remains safe for the
        // legacy deterministic cleanup. Automation must not replace an
        // authorized numeric ID with an absent/new identity during its TOCTOU window.
        if expected_runner_id.is_some() {
            return Ok(false);
        }
        docker::remove_container(expected_id)?;
        return Ok(true);
    };

    if !expected_runner_matches_authorized_id(expected_runner_id, runner.id) {
        eprintln!(
            "gitrun-autoscaler: preserving Automation Stop candidate {name} for {repo}: GitHub runner identity changed after GSR authorization"
        );
        return Ok(false);
    }

    if runner.busy || !runner.is_online() {
        eprintln!(
            "gitrun-autoscaler: preserving idle-scale-down candidate {name} for {repo}: runner is no longer safely removable (online={}, busy={})",
            runner.is_online(),
            runner.busy
        );
        return Ok(false);
    }

    ensure_current_container_identity(name, expected_id)?;
    client.delete_runner(repo, runner.id)?;
    docker::remove_container(expected_id)?;
    Ok(true)
}

struct RunnerCreateContext<'a> {
    permanent: bool,
    job_name: Option<&'a str>,
    job_run_id: Option<u64>,
    job_id: Option<u64>,
    job_labels: &'a [String],
    prewarm_workload_id: Option<&'a str>,
    prewarm_target_epoch: Option<u64>,
    prewarm_hold_until_epoch: Option<u64>,
    resource_allocation: Option<&'a RunnerResourceAllocation>,
}

fn create_runner(
    client: &GitHubClient,
    config: &Config,
    state_dir: &std::path::Path,
    vm_registry: &VmResolutionRegistry,
    repo: &str,
    workload_routing_profiles: &std::collections::HashMap<String, Vec<String>>,
    context: RunnerCreateContext<'_>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let RunnerCreateContext {
        permanent,
        job_name,
        job_run_id,
        job_id,
        job_labels,
        prewarm_workload_id,
        prewarm_target_epoch,
        prewarm_hold_until_epoch,
        resource_allocation,
    } = context;
    let explicit_workload_id = explicit_workload_routing_id(job_labels)?;
    if let (Some(explicit_id), Some(prewarm_id)) =
        (explicit_workload_id.as_deref(), prewarm_workload_id)
    {
        if explicit_id != prewarm_id {
            return Err(std::io::Error::other(
                "a runner cannot mix different workload-family routing and prewarm identities",
            )
            .into());
        }
    }
    let routed_workload_id = prewarm_workload_id
        .map(str::to_owned)
        .or(explicit_workload_id.clone());
    let backend_labels: &[String] = match explicit_workload_id.as_deref() {
        Some(workload_id) => workload_routing_profiles
            .get(workload_id)
            .map(Vec::as_slice)
            .ok_or_else(|| {
                std::io::Error::other(format!(
                    "no validated Kitty workload profile exists for routed workload {workload_id}; refusing to guess a backend or image"
                ))
            })?,
        None => job_labels,
    };
    let ban_store = gsr_poll::BanStore::load(state_dir).map_err(|error| {
        std::io::Error::other(format!(
            "cannot safely create runner for {repo}: GSR ban state is unreadable: {error}"
        ))
    })?;
    if ban_store.is_banned(repo) {
        println!("gitrun-autoscaler: skipping runner creation for {repo}: temporarily banned by GSR after a policy violation");
        return Ok(false);
    }

    let gsr_policy_env = gsr_policy_env(config);
    let name = format!("{}{}", docker::runner_name_prefix(repo), uuid_like_suffix());
    docker::ensure_shared_cache_volume(&config.shared_cache_volume)?;
    let settings = gitrun_core::GitRunSettings::load_or_default(
        gitrun_core::GitRunSettings::path_for_state_dir(state_dir),
    )?;
    let repository_settings = settings.effective_for_repository(repo);
    let docker_socket_gid = if repository_settings.docker.direct_socket_enabled {
        resolve_docker_socket_gid()?
    } else {
        String::new()
    };
    let secret_env = vault_env_for_repo(config, repo)?;

    let Some((backend, image, is_windows)) =
        resolve_backend_and_image(config, state_dir, vm_registry, backend_labels)
    else {
        // Never create a VM-targeted job runner on the local host while VM
        // resolution is pending; doing so could let it claim the wrong job.
        return Ok(false);
    };
    if routed_workload_id.is_some() && is_windows {
        return Err(std::io::Error::other(
            "exclusive workload-family routing is not supported by the configured Windows runner bootstrap; use a managed Linux runner profile",
        ).into());
    }

    let runner_labels = match routed_workload_id.as_deref() {
        Some(workload_id) => exclusive_workload_routing_label(workload_id)?,
        None => runner_labels_for_job(&config.runner_labels, permanent, job_labels),
    };
    let dock_target = load_dock_target_jobs(state_dir, repo)?
        .into_iter()
        .any(|target| Some(target.as_str()) == job_name);
    if routed_workload_id.is_some() && dock_target {
        return Err(std::io::Error::other(
            "a GitDockRun-owned lifecycle cannot be mixed with a Kitty workload-family route",
        )
        .into());
    }
    let (container_job_name, container_run_id, container_job_id) =
        runner_workflow_binding(routed_workload_id.as_deref(), job_name, job_run_id, job_id);
    let registration_token = client.registration_token(repo)?;

    docker::create_runner_on(
        &backend,
        &docker::RunnerSpec {
            name: &name,
            repo,
            permanent,
            registration_token: &registration_token,
            image: &image,
            labels: &runner_labels,
            no_default_labels: routed_workload_id.is_some(),
            ephemeral: config.ephemeral || (is_windows && !permanent),
            disable_update: config.runner_disable_update,
            cpus: resource_allocation.map(|allocation| allocation.cpus.as_str()).unwrap_or(&config.container_cpus),
            memory: resource_allocation.map(|allocation| allocation.memory.as_str()).unwrap_or(&config.container_memory),
            pids_limit: &config.container_pids_limit,
            shared_cache_volume: &config.shared_cache_volume,
            cache_scope: &config.shared_cache_scope,
            network: &config.runner_network,
            seccomp_profile: &config.runner_seccomp_profile,
            apparmor_profile: &config.runner_apparmor_profile,
            docker_socket_gid: &docker_socket_gid,
            docker_socket_enabled: repository_settings.docker.direct_socket_enabled,
            runner_home_size: &config.runner_home_size,
            home_backend: docker::RunnerHomeBackend::from_config_str(&config.runner_home_backend),
            rootfs_read_only: config.runner_rootfs_read_only,
            secret_env: &secret_env,
            gsr_policy_env: &gsr_policy_env,
            is_windows,
            workflow_job_name: container_job_name,
            workflow_run_id: container_run_id,
            workflow_job_id: container_job_id,
            workload_routing_id: routed_workload_id.as_deref(),
            prewarm_workload_id,
            prewarm_target_epoch,
            prewarm_hold_until_epoch,
            dock_target,
            docker_socket_hardening: config.gsr_docker_socket_hardening,
            windows_hyperv_isolation: config.runner_windows_hyperv_isolation,
        },
    )?;
    Ok(true)
}

fn load_dock_target_jobs(
    state_dir: &std::path::Path,
    repo: &str,
) -> Result<Vec<String>, std::io::Error> {
    use std::io::Read;

    let state_metadata = match std::fs::symlink_metadata(state_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    if state_metadata.file_type().is_symlink() || !state_metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "GitDockRun requirement state root must be a real directory",
        ));
    }

    let directory = state_dir.join("workflow-dock-requirements");
    let directory_metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "GitDockRun requirements path must be a real directory",
        ));
    }

    let path = directory.join(format!("{}.json", repo_storage_key(repo)));
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(std::io::Error::other(format!(
                "cannot safely read GitDockRun requirements at {}: {error}",
                path.display()
            )));
        }
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "GitDockRun requirements at {} are not a regular file",
                path.display()
            ),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 || metadata.mode() & 0o022 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "GitDockRun requirements at {} must have one link and no group/world write permission",
                    path.display()
                ),
            ));
        }
    }
    let mut raw = String::new();
    file.read_to_string(&mut raw)?;

    let requests =
        serde_json::from_str::<Vec<gitrun_core::DockRequest>>(&raw).map_err(|error| {
            std::io::Error::other(format!(
                "invalid GitDockRun requirements at {}: {error}",
                path.display()
            ))
        })?;
    let mut jobs = Vec::new();
    for request in requests {
        if !jobs.iter().any(|job| job == &request.target_job) {
            jobs.push(request.target_job);
        }
    }
    Ok(jobs)
}

/// Combines the globally configured runner labels with the labels of the
/// queued job that caused a dynamic runner to be created. This prevents a
/// dynamically routed runner (for example a Windows/VM runner) from becoming
/// eligible for an unrelated job after registration.
fn runner_workflow_binding<'a>(
    routed_workload_id: Option<&str>,
    job_name: Option<&'a str>,
    run_id: Option<u64>,
    job_id: Option<u64>,
) -> (Option<&'a str>, Option<u64>, Option<u64>) {
    // A family-labeled runner can be assigned by GitHub to any job requesting
    // that family label, so it must not claim a single occurrence ID in the
    // runner container metadata. The API independently verifies the actual
    // active run/job and that GitHub reports this runner name for that job.
    if routed_workload_id.is_some() {
        (None, None, None)
    } else {
        (job_name, run_id, job_id)
    }
}

fn runner_labels_for_job(configured: &str, permanent: bool, job_labels: &[String]) -> String {
    if permanent || job_labels.is_empty() {
        return configured.to_owned();
    }

    let mut labels: Vec<String> = configured
        .split(',')
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
        .collect();

    for job_label in job_labels {
        let job_label = job_label.trim();
        if job_label.is_empty() {
            continue;
        }
        if !labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case(job_label))
        {
            labels.push(job_label.to_owned());
        }
    }

    labels.join(",")
}

/// Adds the stable workload-family routing label to predictive prewarm runners.
/// Workflows must explicitly request this label in `runs-on` to prefer the
/// family. It does not and cannot hard-pin a future GitHub run/job ID.
fn explicit_workload_routing_id(job_labels: &[String]) -> Result<Option<String>, std::io::Error> {
    if !gitrun_smart_scheduler::has_workload_routing_marker(job_labels) {
        return Ok(None);
    }
    let Some(workload_id) = gitrun_smart_scheduler::workload_id_from_routing_labels(job_labels)
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "queued job contains a malformed or conflicting GitRun workload-family label",
        ));
    };
    let expected = format!("gitrun-job-{workload_id}");
    if job_labels.len() != 1 || !job_labels[0].eq_ignore_ascii_case(&expected) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "exclusive GitRun routing requires runs-on to contain only the gitrun-job workload-family label",
        ));
    }
    Ok(Some(workload_id))
}

fn exclusive_workload_routing_label(workload_id: &str) -> Result<String, std::io::Error> {
    let label = format!("gitrun-job-{workload_id}");
    if gitrun_smart_scheduler::workload_id_from_routing_labels(std::slice::from_ref(&label))
        .as_deref()
        != Some(workload_id)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid Kitty workload-family identifier",
        ));
    }
    Ok(label)
}

#[cfg(test)]
mod workload_routing_label_tests {
    use super::*;

    #[test]
    fn family_runners_do_not_claim_a_single_run_or_job_binding() {
        assert_eq!(
            runner_workflow_binding(
                Some("wk-0123456789abcdef"),
                Some("build"),
                Some(41),
                Some(410),
            ),
            (None, None, None),
        );
        assert_eq!(
            runner_workflow_binding(None, Some("build"), Some(41), Some(410)),
            (Some("build"), Some(41), Some(410)),
        );
    }

    #[test]
    fn family_label_is_exclusive_and_rejects_untrusted_ids() {
        assert_eq!(
            exclusive_workload_routing_label("wk-0123456789abcdef").unwrap(),
            "gitrun-job-wk-0123456789abcdef"
        );
        assert!(exclusive_workload_routing_label("wk-../../bad").is_err());
        assert!(
            explicit_workload_routing_id(&["gitrun-job-wk-0123456789abcdef".into()])
                .unwrap()
                .is_some()
        );
        assert!(explicit_workload_routing_id(&[
            "self-hosted".into(),
            "gitrun-job-wk-0123456789abcdef".into()
        ])
        .is_err());
        assert!(explicit_workload_routing_id(&[
            "gitrun-job-wk-0123456789abcdef".into(),
            "gitrun-job-wk-fedcba9876543210".into()
        ])
        .is_err());
    }
}

/// Refreshes required GitDockRun routing metadata and optionally performs
/// advisory workflow scans. Fetch/persist failures are fatal to this repo's
/// reconcile cycle; advisory findings and optional zizmor failures remain
/// warnings and do not replace command enforcement in the runner.
fn validate_repo_workflows_best_effort(
    client: &GitHubClient,
    config: &Config,
    repo: &str,
    advisory_scan_enabled: bool,
) -> Result<(), String> {
    let events_path = gitrun_gsr::events::default_queue_path(&config.state_dir);
    let files = match client.workflow_files(repo) {
        Ok(files) => files,
        Err(error) => {
            emit_workflow_validation_warning(
                &events_path,
                repo,
                &format!("could not fetch workflow files; GitDockRun routing metadata is unavailable and reconciliation must stop: {error}"),
            );
            return Err(format!(
                "required GitDockRun workflow metadata could not be refreshed: {error}"
            ));
        }
    };

    let mut findings =
        match process_workflow_files(&config.state_dir, repo, &files, advisory_scan_enabled) {
            Ok(findings) => findings,
            Err(error) => {
                emit_workflow_validation_warning(&events_path, repo, &error);
                return Err(error);
            }
        };

    if advisory_scan_enabled && config.gsr_zizmor_enabled {
        let invocation = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let scratch_dir = std::path::PathBuf::from(&config.state_dir)
            .join("gsr-workflow-scratch")
            .join(repo_storage_key(repo))
            .join(format!("{}-{invocation}", std::process::id()));
        if let Err(error) = write_scratch_workflows(&scratch_dir, &files) {
            emit_workflow_validation_warning(
                &events_path,
                repo,
                &format!("could not stage files for zizmor; optional scan was skipped: {error}"),
            );
        } else {
            match gitrun_core::run_zizmor(&scratch_dir) {
                Ok(Some(zizmor_findings)) => findings.extend(zizmor_findings),
                Ok(None) => emit_workflow_validation_warning(
                    &events_path,
                    repo,
                    "zizmor is enabled but its binary is missing; optional scan was skipped",
                ),
                Err(error) => emit_workflow_validation_warning(
                    &events_path,
                    repo,
                    &format!("zizmor run failed; optional scan was skipped: {error}"),
                ),
            }
        }
        if let Err(error) = std::fs::remove_dir_all(&scratch_dir) {
            if error.kind() != std::io::ErrorKind::NotFound {
                emit_workflow_validation_warning(
                    &events_path,
                    repo,
                    &format!("could not remove workflow scan scratch directory: {error}"),
                );
            }
        }
    }

    for finding in findings {
        let message = format!(
            "{repo}: {}{}: {} [{}]",
            finding.file,
            finding.line.map(|l| format!(":{l}")).unwrap_or_default(),
            finding.message,
            finding.rule,
        );
        println!("gitrun-autoscaler: workflow validation finding: {message}");
        let event = gitrun_gsr::SecurityEvent::new(
            "workflow-validation",
            gitrun_gsr::Severity::Warning,
            message,
        );
        if let Err(error) = gitrun_gsr::events::emit(&events_path, &event) {
            eprintln!(
                "gitrun-autoscaler: workflow validation: failed to write security event: {error}"
            );
        }
    }
    Ok(())
}

fn process_workflow_files(
    state_dir: &str,
    repo: &str,
    files: &[(String, String)],
    advisory_scan_enabled: bool,
) -> Result<Vec<gitrun_core::Finding>, String> {
    let mut findings = Vec::new();
    let mut dock_requests = Vec::new();

    for (name, content) in files {
        if advisory_scan_enabled {
            findings.extend(gitrun_core::scan(name, content));
        }
        // This metadata protects scheduler routing and is refreshed regardless
        // of whether the advisory scanner is enabled.
        dock_requests.extend(gitrun_core::scan_dock_requests(name, content));
    }

    save_workflow_dock_requirements(state_dir, repo, &dock_requests).map_err(|error| {
        format!("required GitDockRun routing metadata could not be persisted: {error}")
    })?;
    Ok(findings)
}

fn emit_workflow_validation_warning(events_path: &std::path::Path, repo: &str, detail: &str) {
    let message = format!("{repo}: workflow validation warning: {detail}");
    eprintln!("gitrun-autoscaler: {message}");
    let event = gitrun_gsr::SecurityEvent::new(
        "workflow-validation",
        gitrun_gsr::Severity::Warning,
        message,
    );
    if let Err(error) = gitrun_gsr::events::emit(events_path, &event) {
        eprintln!("gitrun-autoscaler: unable to persist workflow validation warning: {error}");
    }
}

/// Collision-free filesystem key for repository identifiers. Replacing punctuation
/// with a placeholder is not injective (for example, owner/repo names containing
/// underscores can collide after replacing the slash), so security state must use
/// the original bytes encoded losslessly.
fn repo_storage_key(repo: &str) -> String {
    let mut key = String::with_capacity(repo.len() * 2);
    for byte in repo.bytes() {
        use std::fmt::Write as _;
        write!(&mut key, "{byte:02x}").expect("writing to String cannot fail");
    }
    key
}

fn save_workflow_dock_requirements(
    state_dir: &str,
    repo: &str,
    requests: &[gitrun_core::DockRequest],
) -> std::io::Result<()> {
    let state_root = std::path::Path::new(state_dir);
    match std::fs::symlink_metadata(state_root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "workflow state root must be a real directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(state_root)?;
            let metadata = std::fs::symlink_metadata(state_root)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "workflow state root must be a real directory",
                ));
            }
        }
        Err(error) => return Err(error),
    }

    let directory = state_root.join("workflow-dock-requirements");
    match std::fs::create_dir(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let directory_metadata = std::fs::symlink_metadata(&directory)?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workflow requirements path must be a real directory",
        ));
    }

    let path = directory.join(format!("{}.json", repo_storage_key(repo)));
    if requests.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => sync_directory(&directory),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }?;
        return Ok(());
    }

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = path.with_extension(format!("json.{}-{nonce}.tmp", std::process::id()));
    let raw = serde_json::to_vec_pretty(requests).map_err(std::io::Error::other)?;
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = options.open(&temp)?;
        use std::io::Write;
        if let Err(error) = file
            .write_all(&raw)
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_all())
        {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
    }
    if let Err(error) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    sync_directory(&directory)
}

#[cfg(unix)]
fn sync_directory(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

fn write_scratch_workflows(
    dir: &std::path::Path,
    files: &[(String, String)],
) -> std::io::Result<()> {
    use std::path::Component;

    std::fs::create_dir_all(dir)?;
    let directory_metadata = std::fs::symlink_metadata(dir)?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workflow scratch path must be a real directory",
        ));
    }
    for (name, content) in files {
        let relative = std::path::Path::new(name);
        let mut components = relative.components();
        let is_single_file_name =
            matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
        if relative.is_absolute() || !is_single_file_name {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("workflow path must be a direct filename: {name}"),
            ));
        }
        let target = dir.join(relative);
        write_scratch_file_atomic(&target, content)?;
    }
    Ok(())
}

fn write_scratch_file_atomic(target: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workflow target has no parent",
        )
    })?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = parent.join(format!(
        ".gitrun-workflow-{}-{nonce}.tmp",
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(&temp)?;
    if let Err(error) = file
        .write_all(content.as_bytes())
        .and_then(|_| file.sync_all())
    {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    drop(file);
    if let Err(error) = std::fs::rename(&temp, target) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

/// Looks up and decrypts GitVault secrets meant for `repo`, returning them
/// as (name, value) pairs ready to inject as environment variables.
///
/// Naming convention: a secret named `<repo-slug>__SECRET_NAME` (repo slug
/// from `docker::sanitize(repo, '_')`, e.g. `owner_repo__DEPLOY_KEY`) is
/// injected only into that repo's runners; a secret named `global__NAME` is
/// injected into every repo's runners. This keeps the vault's flat
/// name->value store simple while still letting an operator scope secrets
/// per-repo without a separate access-control layer (deferred — see
/// `gitrun-vault`'s module docs on what GSR will eventually add on top).
///
/// Returns an empty list (not an error) if `vault_dir` is unset or the vault
/// can't be opened — a broken/missing vault should not prevent runners from
/// being created at all, only prevent secrets from being available to them.
/// Failures are logged, not silently swallowed, so an operator relying on a
/// secret notices it's missing rather than debugging a mysteriously empty
/// environment variable inside a job.
fn gsr_policy_env(config: &Config) -> Vec<(String, String)> {
    vec![
        (
            "GITRUN_GSR_COMMAND_POLICY_ENABLED".into(),
            config.gsr_command_policy_enabled.to_string(),
        ),
        (
            "GITRUN_GSR_ALLOW_UNSAFE_RUNNER".into(),
            config.gsr_allow_unsafe_runner.to_string(),
        ),
        (
            "GITRUN_GSR_COMMAND_BASELINE_BLACKLIST_ENABLED".into(),
            config.gsr_command_baseline_blacklist_enabled.to_string(),
        ),
        (
            "GITRUN_GSR_COMMAND_BLACKLIST_ENABLED".into(),
            config.gsr_command_blacklist_enabled.to_string(),
        ),
        (
            "GITRUN_GSR_COMMAND_BLACKLIST".into(),
            config.gsr_command_blacklist.clone(),
        ),
        (
            "GITRUN_GSR_COMMAND_WHITELIST_ENABLED".into(),
            config.gsr_command_whitelist_enabled.to_string(),
        ),
        (
            "GITRUN_GSR_COMMAND_WHITELIST".into(),
            config.gsr_command_whitelist.clone(),
        ),
        (
            "GITRUN_GSR_VIOLATION_ACTION".into(),
            config.gsr_violation_action.clone(),
        ),
    ]
}

fn vault_env_for_repo(
    config: &Config,
    repo: &str,
) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    if config.vault_dir.trim().is_empty() {
        return Ok(Vec::new());
    }
    let vault_path = Path::new(&config.vault_dir);
    match std::fs::symlink_metadata(vault_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "gitrun-autoscaler: GitVault directory {} is missing; creating runner without vault secrets",
                vault_path.display()
            );
            return Ok(Vec::new());
        }
        Err(error) => return Err(error.into()),
    }
    let bridge = VaultToGsrBridge::new(&config.state_dir);
    let vault = Vault::open_with_sink(&config.vault_dir, Box::new(bridge)).map_err(|error| {
        std::io::Error::other(format!(
            "cannot safely create runner for {repo}: GitVault could not be opened: {error}"
        ))
    })?;

    let groups = config.vault_groups_for_repo(repo);
    // Never launch a runner with a partial secret environment: any selected
    // secret decryption failure is fatal and propagated by GitVault.
    let resolved = vault.resolve_for_repo(repo, &groups).map_err(|error| {
        std::io::Error::other(format!(
            "cannot safely create runner for {repo}: GitVault secret resolution failed: {error}"
        ))
    })?;
    let mut env = Vec::with_capacity(resolved.len());
    for (name, value) in resolved {
        if is_valid_env_var_name(&name) {
            env.push((name, value));
        } else {
            return Err(std::io::Error::other(format!(
                "cannot safely create runner for {repo}: GitVault secret name '{name}' is not a valid environment variable name"
            )).into());
        }
    }
    Ok(env)
}

/// A conservative environment variable name check: uppercase letters,
/// digits, underscores, must not start with a digit. Stricter than POSIX
/// technically requires, but there's no reason a secret's env var name
/// should need anything looser, and being strict here is cheap insurance
/// against shell-metacharacter or injection surprises downstream.
fn is_valid_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Unique-enough suffix for container names without adding a dependency:
/// epoch milliseconds + process ID + an atomic per-process sequence.
fn uuid_like_suffix() -> String {
    let epoch_millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let pid = std::process::id();
    let sequence = CONTAINER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{epoch_millis:x}-{pid:x}-{sequence:x}")
}

#[cfg(unix)]
fn unix_gid_of_docker_socket() -> String {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/var/run/docker.sock")
        .map(|meta| meta.gid().to_string())
        .unwrap_or_default()
}

#[cfg(not(unix))]
fn unix_gid_of_docker_socket() -> String {
    String::new()
}

/// Resolves the Docker socket's group ID for `--group-add`, failing loudly
/// instead of silently degrading to `--group-add ""` (which Docker either
/// rejects with a confusing error or, worse, silently ignores depending on
/// version — either way the runner container would come up without the
/// group membership it needs to actually use the socket). Distinguishes the
/// two real failure modes explicitly: socket missing (Docker not ready /
/// wrong path) vs. its metadata being unreadable for some other reason.
fn resolve_docker_socket_gid() -> Result<String, Box<dyn std::error::Error>> {
    let socket_path = std::path::Path::new("/var/run/docker.sock");
    if !socket_path.exists() {
        return Err(format!(
            "docker socket not found at {} — is Docker running?",
            socket_path.display()
        )
        .into());
    }
    let gid = unix_gid_of_docker_socket();
    if gid.is_empty() {
        return Err(format!(
            "docker socket exists at {} but its group ownership could not be read",
            socket_path.display()
        )
        .into());
    }
    Ok(gid)
}

pub fn run_gtuu_once() -> Result<u32, Box<dyn std::error::Error>> {
    let config = load_config()?;
    let client = build_github_client(&config)?;
    let settings = gitrun_core::GitRunSettings::load_or_default(
        gitrun_core::GitRunSettings::path_for_state_dir(&config.state_dir),
    )?;
    let socket_enabled_for_repo = |repo: &str| {
        settings
            .effective_for_repository(repo)
            .docker
            .direct_socket_enabled
    };
    let docker_socket_gid = if config
        .repositories
        .iter()
        .any(|repo| socket_enabled_for_repo(repo))
    {
        resolve_docker_socket_gid()?
    } else {
        String::new()
    };
    let gsr_policy_env = gsr_policy_env(&config);
    let gtuu_config = GtuuConfig {
        image: &config.runner_image,
        repositories: &config.repositories,
        runner_labels: &config.runner_labels,
        ephemeral: config.ephemeral,
        disable_update: config.runner_disable_update,
        cpus: &config.container_cpus,
        memory: &config.container_memory,
        pids_limit: &config.container_pids_limit,
        shared_cache_volume: &config.shared_cache_volume,
        cache_scope: &config.shared_cache_scope,
        network: &config.runner_network,
        seccomp_profile: &config.runner_seccomp_profile,
        apparmor_profile: &config.runner_apparmor_profile,
        docker_socket_gid: &docker_socket_gid,
        runner_home_size: &config.runner_home_size,
        runner_home_backend: docker::RunnerHomeBackend::from_config_str(
            &config.runner_home_backend,
        ),
        runner_rootfs_read_only: config.runner_rootfs_read_only,
        runner_windows_hyperv_isolation: config.runner_windows_hyperv_isolation,
        secret_env_for_repo: &|repo: &str| {
            vault_env_for_repo(&config, repo).map_err(|error| error.to_string())
        },
        online_wait_timeout: Duration::from_secs(120),
        docker_socket_hardening: config.gsr_docker_socket_hardening,
        gsr_policy_env: &gsr_policy_env,
        docker_socket_enabled_for_repo: &socket_enabled_for_repo,
    };
    Ok(gtuu::update_permanent_containers(&client, &gtuu_config)?)
}
fn spawn_gtuu_thread(_config: &Config, stopping: &Arc<AtomicBool>) -> std::io::Result<()> {
    let stopping = stopping.clone();

    std::thread::Builder::new()
        .name("gitrun-gtuu".into())
        .spawn(move || {
            let mut last_run_date: Option<String> = None;
            while !stopping.load(Ordering::Relaxed) {
                // The dashboard can change the GTUU schedule while the
                // scheduler is running. Reload local configuration every
                // tick so those settings apply without a service restart;
                // this is a file read, not a GitHub API poll.
                let config = match load_config() {
                    Ok(config) => config,
                    Err(error) => {
                        eprintln!("gitrun-autoscaler: GTUU schedule config reload failed: {error}");
                        std::thread::sleep(Duration::from_secs(15));
                        continue;
                    }
                };
                let now = chrono_like_now(config.gtuu_schedule_timezone == "local");
                let scheduled_time = config.container_update_time.clone();
                if config.auto_container_update
                    && now.time_hhmm == scheduled_time
                    && last_run_date.as_deref() != Some(&now.date)
                {
                    println!("gitrun-autoscaler: GTUU scheduled run starting");
                    let mut completed = false;
                    match GtuuLock::acquire(&PathBuf::from(&config.state_dir)) {
                        Ok(_lock) => match run_gtuu_once() {
                            Ok(count) => {
                                println!(
                                    "gitrun-autoscaler: GTUU updated {count} permanent runner(s)"
                                );
                                completed = true;
                            }
                            Err(error) => eprintln!("gitrun-autoscaler: GTUU run failed: {error}"),
                        },
                        Err(error) => eprintln!("gitrun-autoscaler: GTUU skipped: {error}"),
                    }
                    if completed {
                        last_run_date = Some(now.date);
                    }
                }
                let mut remaining = Duration::from_secs(15);
                while !remaining.is_zero() && !stopping.load(Ordering::Relaxed) {
                    let slice = remaining.min(Duration::from_millis(200));
                    std::thread::sleep(slice);
                    remaining = remaining.saturating_sub(slice);
                }
            }
        })
        .map(|_| ())
}
/// Spawns GSR's Layer 2 (external, host-side) enforcement poll loop — see
/// `crate::gsr_poll` for the full design and
/// `gitrun_gsr::agent` for Layer 1 (internal, preventive). Started only
/// when `Config::gsr_command_policy_enabled` is true; when it's false
/// there is no policy to enforce and this thread has nothing to do, so it
/// isn't started at all rather than spun up to immediately no-op every
/// tick.
fn spawn_gsr_poll_thread(
    config: &Config,
    vm_registry: &VmResolutionRegistry,
    stopping: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    let Some(policy) = config.command_policy() else {
        return Ok(());
    };
    let action = config.violation_action();
    let state_dir = PathBuf::from(&config.state_dir);
    let events_path = gitrun_gsr::events::default_queue_path(&config.state_dir);
    let stopping = stopping.clone();
    let vm_registry = Arc::clone(vm_registry);
    let poll_interval = Duration::from_secs(5);

    std::thread::Builder::new()
        .name("gitrun-gsr-poll".into())
        .spawn(move || {
            let stopping_check = stopping.clone();
            gsr_poll::run_with_hosts(
                &state_dir,
                policy,
                action,
                poll_interval,
                {
                    let vm_registry = Arc::clone(&vm_registry);
                    move || {
                        let mut hosts = vec![docker::DockerHost::Local];
                        let guard = vm_registry.lock().unwrap_or_else(|p| p.into_inner());
                        for resolution in guard.values() {
                            if let vm_resolution::VmResolution::Resolved { host, .. } = resolution {
                                if !hosts.contains(host) {
                                    hosts.push(host.clone());
                                }
                            }
                        }
                        hosts
                    }
                },
                move |violation| {
                    let repo_note = violation.repo.as_deref().unwrap_or("unknown repo");
                    let action_error = violation
                        .action_error
                        .as_deref()
                        .map(|error| format!("; enforcement error: {error}"))
                        .unwrap_or_default();
                    let message = format!(
                        "container {} ({repo_note}) ran a denied command: {} — {} (action: {:?}{action_error})",
                        violation.container_name, violation.command_line, violation.reason, violation.action_taken
                    );
                    eprintln!("gitrun-autoscaler: gsr_poll: {message}");
                    let event = gitrun_gsr::SecurityEvent::new("gsr-poll", gitrun_gsr::Severity::Critical, message);
                    if let Err(error) = gitrun_gsr::events::emit(&events_path, &event) {
                        eprintln!("gitrun-autoscaler: gsr_poll: additionally failed to write security event: {error}");
                    }
                },
                move || stopping_check.load(Ordering::Relaxed),
            );
        })
        .map(|_| ())
}

/// Minimal HH:MM + date string, UTC by default, without pulling in `chrono`
/// for something checked once every ~15 seconds. Local time (when
/// `Config::gtuu_schedule_timezone == "local"`) shells out to the system
/// `date` command instead of hand-rolling timezone/DST math, which is
/// genuinely hard to get right without a tz database — `date` already
/// knows the host's rules. Falls back to the UTC calculation if `date`
/// isn't available or its output can't be parsed, so a minimal container
/// missing the `date` binary degrades to the old (UTC) behavior instead of
/// breaking GTUU scheduling entirely.
struct SimpleNow {
    time_hhmm: String,
    date: String,
}

fn chrono_like_now(use_local: bool) -> SimpleNow {
    if use_local {
        if let Some(now) = local_now_via_date_command() {
            return now;
        }
        eprintln!(
            "gitrun-autoscaler: GTUU could not read local time via the `date` command, falling back to UTC for this check"
        );
    }
    utc_now()
}

/// Runs one `date +%H:%M %Y-%m-%d` command (no `-u`, so system local time,
/// DST-aware) and parses the output. A single command keeps the date/time pair
/// consistent across a midnight boundary. Returns `None` on any failure —
/// binary missing, non-zero exit, unparseable output — so the caller can
/// fall back rather than panic.
fn local_now_via_date_command() -> Option<SimpleNow> {
    let output = std::process::Command::new("date")
        .arg("+%H:%M %Y-%m-%d")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let (time_hhmm, date) = raw.trim().split_once(' ')?;
    if time_hhmm.len() != 5 || date.len() != 10 {
        return None; // sanity check: "HH:MM" / "YYYY-MM-DD", reject anything malformed
    }
    Some(SimpleNow {
        time_hhmm: time_hhmm.to_owned(),
        date: date.to_owned(),
    })
}

fn utc_now() -> SimpleNow {
    // SAFETY-note-in-comment-form (not unsafe code): uses only libc via the
    // standard library's SystemTime + a manual civil-from-days calculation,
    // avoiding a new dependency for a once-per-15-seconds string comparison.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = now.as_secs();
    let days = (total_secs / 86400) as i64;
    let secs_of_day = total_secs % 86400;
    let (hour, minute) = ((secs_of_day / 3600) % 24, (secs_of_day / 60) % 60);

    // Civil calendar from days-since-epoch (Howard Hinnant's algorithm),
    // UTC-based. This is the default schedule timezone (see
    // Config::gtuu_schedule_timezone); set it to "local" instead of
    // converting GITRUN_CONTAINER_UPDATE_TIME to UTC by hand if the
    // operator wants a wall-clock-local schedule.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    SimpleNow {
        time_hhmm: format!("{hour:02}:{minute:02}"),
        date: format!("{year:04}-{m:02}-{d:02}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn dock_requirement_reader_rejects_symlinked_file() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!(
            "gitrun-dock-requirements-link-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let directory = base.join("workflow-dock-requirements");
        std::fs::create_dir_all(&directory).unwrap();
        let repo = "owner/repo";
        let target = base.join("external.json");
        std::fs::write(&target, "[]").unwrap();
        symlink(
            &target,
            directory.join(format!("{}.json", repo_storage_key(repo))),
        )
        .unwrap();
        assert!(load_dock_target_jobs(&base, repo).is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn dock_requirement_reader_rejects_hard_linked_file() {
        let base = std::env::temp_dir().join(format!(
            "gitrun-dock-requirements-hardlink-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let directory = base.join("workflow-dock-requirements");
        std::fs::create_dir_all(&directory).unwrap();
        let repo = "owner/repo";
        let target = base.join("external.json");
        let path = directory.join(format!("{}.json", repo_storage_key(repo)));
        std::fs::write(&target, "[]").unwrap();
        std::fs::hard_link(&target, &path).unwrap();
        assert!(load_dock_target_jobs(&base, repo).is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn workflow_dock_requirements_use_exact_repo_key_and_remove_atomically() {
        let base = std::env::temp_dir().join(format!(
            "gitrun-workflow-requirements-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = "owner/repo";
        let request = gitrun_core::DockRequest {
            file: "ci.yml".into(),
            line: 4,
            calling_job: Some("build".into()),
            target_job: "cache".into(),
            operation: gitrun_core::DockOperation::Connect,
        };
        save_workflow_dock_requirements(
            base.to_str().unwrap(),
            repo,
            std::slice::from_ref(&request),
        )
        .unwrap();
        let path = base
            .join("workflow-dock-requirements")
            .join(format!("{}.json", repo_storage_key(repo)));
        let stored: Vec<gitrun_core::DockRequest> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored, vec![request]);
        save_workflow_dock_requirements(base.to_str().unwrap(), repo, &[]).unwrap();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn dock_routing_metadata_refreshes_when_advisory_validation_is_disabled() {
        let base = std::env::temp_dir().join(format!(
            "gitrun-dock-requirements-disabled-scan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = "owner/repo";
        let files = vec![(
            "ci.yml".to_owned(),
            "jobs:\n  build:\n    steps:\n      - run: GitDockRun --job build-cache --connect\n"
                .to_owned(),
        )];

        let findings = process_workflow_files(base.to_str().unwrap(), repo, &files, false)
            .expect("required routing metadata should be stored");
        assert!(findings.is_empty());
        assert_eq!(
            load_dock_target_jobs(&base, repo).unwrap(),
            vec!["build-cache".to_owned()]
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn workflow_routing_metadata_persistence_failure_is_propagated() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!(
            "gitrun-dock-requirements-untrusted-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let real_state = base.join("real-state");
        std::fs::create_dir(&real_state).unwrap();
        let state_link = base.join("state-link");
        symlink(&real_state, &state_link).unwrap();
        let files = vec![(
            "ci.yml".to_owned(),
            "jobs:\n  build:\n    steps:\n      - run: GitDockRun --job build-cache --connect\n"
                .to_owned(),
        )];

        let result =
            process_workflow_files(state_link.to_str().unwrap(), "owner/repo", &files, false);
        assert!(matches!(result, Err(error) if error.contains("could not be persisted")));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn repository_storage_keys_are_collision_free_for_sanitization_collisions() {
        let first = "foo_bar/baz";
        let second = "foo/bar_baz";
        assert_eq!(docker::sanitize(first, '_'), docker::sanitize(second, '_'));
        assert_ne!(repo_storage_key(first), repo_storage_key(second));
        assert_eq!(repo_storage_key("owner/repo"), "6f776e65722f7265706f");
    }

    #[test]
    fn dynamic_runner_labels_include_job_labels() {
        let labels = runner_labels_for_job(
            "self-hosted,Linux,gitrun-ci",
            false,
            &["windows".into(), "GPU".into(), "linux".into()],
        );
        assert_eq!(labels, "self-hosted,Linux,gitrun-ci,windows,GPU");
    }

    #[test]
    fn permanent_runner_labels_ignore_job_labels() {
        let labels =
            runner_labels_for_job("self-hosted,Linux,gitrun-ci", true, &["windows".into()]);
        assert_eq!(labels, "self-hosted,Linux,gitrun-ci");
    }

    #[cfg(unix)]
    #[test]
    fn workflow_scratch_writer_replaces_symlinks_without_touching_targets() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!(
            "gitrun-workflow-scratch-symlink-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("outside");
        let scratch = base.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let workflow = scratch.join("ci.yml");
        std::fs::write(&target, "keep").unwrap();
        symlink(&target, &workflow).unwrap();
        write_scratch_workflows(&scratch, &[("ci.yml".into(), "name: safe".into())]).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(&workflow).unwrap(), "name: safe");
        assert!(
            write_scratch_workflows(&scratch, &[("../outside".into(), "bad".into())],).is_err()
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn gsr_policy_env_preserves_explicit_unsafe_runner_opt_in() {
        let config = Config {
            gsr_command_policy_enabled: false,
            gsr_allow_unsafe_runner: true,
            ..Config::default()
        };
        let env = gsr_policy_env(&config);
        assert!(env.iter().any(|(key, value)| {
            key == "GITRUN_GSR_COMMAND_POLICY_ENABLED" && value == "false"
        }));
        assert!(env
            .iter()
            .any(|(key, value)| { key == "GITRUN_GSR_ALLOW_UNSAFE_RUNNER" && value == "true" }));
    }

    #[test]
    fn missing_configured_vault_does_not_get_initialized_silently() {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-scheduler-missing-vault-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let config = Config {
            vault_dir: dir.to_string_lossy().into_owned(),
            ..Config::default()
        };
        let env = vault_env_for_repo(&config, "owner/repo").unwrap();
        assert!(env.is_empty());
        assert!(
            !dir.exists(),
            "missing vault must not be initialized implicitly"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn scheduler_pid_file_rejects_symlink_without_modifying_target() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!(
            "gitrun-scheduler-pid-symlink-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        let pid_path = dir.join("gitrun-autoscaler.pid");
        std::fs::write(&target, "preserve me").unwrap();
        symlink(&target, &pid_path).unwrap();
        assert!(acquire_pid_file(&pid_path).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "preserve me");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn scheduler_pid_lock_rejects_symlink() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!(
            "gitrun-scheduler-lock-symlink-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        let pid_path = dir.join("gitrun-autoscaler.pid");
        let lock_path = pid_path.with_extension("pid.lock");
        std::fs::write(&target, "preserve me").unwrap();
        symlink(&target, &lock_path).unwrap();
        assert!(acquire_pid_file(&pid_path).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "preserve me");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn valid_env_var_names_are_accepted() {
        assert!(is_valid_env_var_name("DEPLOY_KEY"));
        assert!(is_valid_env_var_name("_PRIVATE"));
        assert!(is_valid_env_var_name("API_KEY_2"));
    }

    #[test]
    fn lowercase_or_leading_digit_is_rejected() {
        assert!(!is_valid_env_var_name("deploy_key"));
        assert!(!is_valid_env_var_name("2FA_TOKEN"));
    }

    #[test]
    fn shell_metacharacters_are_rejected() {
        assert!(!is_valid_env_var_name("KEY; rm -rf /"));
        assert!(!is_valid_env_var_name("KEY=value"));
        assert!(!is_valid_env_var_name("KEY$(whoami)"));
        assert!(!is_valid_env_var_name(""));
    }
}

#[cfg(test)]
mod resource_pressure_reconciliation_tests {
    use super::*;
    use crate::reconcile::Action;

    #[test]
    fn recurring_workload_prewarms_share_the_global_predictive_runner_budget() {
        let target =
            |workload_id: &str, runner_count: u32| gitrun_smart_scheduler::WorkloadPrewarm {
                workload_id: workload_id.into(),
                workflow_id: 9,
                workflow_path: ".github/workflows/build.yml".into(),
                job_name: workload_id.into(),
                labels: vec!["self-hosted".into(), "linux".into()],
                target_epoch: 7_200,
                hold_until_epoch: 8_100,
                runner_count,
                prewarm_runners_created: 0,
                observed_distinct_weeks: 6,
                historical_occurrences_per_week: f64::from(runner_count),
            };
        let mut targets = vec![
            target("wk-0123456789abcdef", 3),
            target("wk-fedcba9876543210", 4),
        ];

        assert_eq!(bound_workload_prewarms_to_budget(&mut targets, 5), 5);
        assert_eq!(
            targets
                .iter()
                .map(|target| target.runner_count)
                .collect::<Vec<_>>(),
            vec![3, 2],
            "a warm-pool floor increase and workload warmups must share one cap"
        );

        assert_eq!(bound_workload_prewarms_to_budget(&mut targets, 0), 0);
        assert!(targets.is_empty());
    }

    #[test]
    fn adaptive_resource_budget_respects_operator_caps_and_shared_host_budget() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let config = Config {
            container_cpus: "2".into(),
            container_memory: "4g".into(),
            ..Config::default()
        };
        let mut budget = AdaptiveResourceBudget::from_capacity(
            resource_pressure::ResourceCapacity { cpu_cores: 3.0, memory_bytes: 8 * GIB },
            Vec::new(),
        ).unwrap();
        let estimate = |workload_id: &str| gitrun_smart_scheduler::ResourceDemandEstimate {
            workload_id: workload_id.into(),
            cpu_cores: 4.0,
            memory_bytes: 8 * GIB,
            sample_count: 0,
            learned: false,
            reason: "test prior".into(),
        };

        let first = budget.allocate(estimate("wk-0123456789abcdef"), &config).unwrap();
        assert_eq!(first.cpus, "2");
        assert_eq!(first.memory, (4 * GIB).to_string());
        assert_eq!(budget.available_cpu_cores(), 1.0);
        assert_eq!(budget.available_memory_bytes(), 4 * GIB);

        let second = budget.allocate(estimate("wk-fedcba9876543210"), &config).unwrap();
        assert_eq!(second.cpus, "1");
        assert_eq!(second.memory, (4 * GIB).to_string());
        assert!(budget
            .allocate(estimate("wk-aaaaaaaaaaaaaaaa"), &config)
            .is_none());
    }

    #[test]
    fn adaptive_resource_budget_fails_closed_below_minimum_allocation() {
        let config = Config {
            container_cpus: "2".into(),
            container_memory: "4g".into(),
            ..Config::default()
        };
        let mut budget = AdaptiveResourceBudget::from_capacity(
            resource_pressure::ResourceCapacity {
                cpu_cores: 0.2,
                memory_bytes: 4 * 1024 * 1024 * 1024,
            },
            Vec::new(),
        ).unwrap();
        let estimate = gitrun_smart_scheduler::ResourceDemandEstimate {
            workload_id: "wk-0123456789abcdef".into(),
            cpu_cores: 2.0,
            memory_bytes: 2 * 1024 * 1024 * 1024,
            sample_count: 0,
            learned: false,
            reason: "test prior".into(),
        };
        assert!(budget.allocate(estimate, &config).is_none());
    }

    #[test]
    fn adaptive_budget_reserves_active_runner_headroom_and_releases_replaced_runner() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let mut budget = AdaptiveResourceBudget::from_capacity(
            resource_pressure::ResourceCapacity { cpu_cores: 3.0, memory_bytes: 8 * GIB },
            vec![docker::ManagedContainerResourceReservation {
                name: "runner-a".into(),
                cpu_headroom_cores: 2.0,
                memory_headroom_bytes: 4 * GIB,
            }],
        ).unwrap();
        assert_eq!(budget.available_cpu_cores(), 1.0);
        assert_eq!(budget.available_memory_bytes(), 4 * GIB);
        budget.release_container_reservation("runner-a");
        assert_eq!(budget.available_cpu_cores(), 3.0);
        assert_eq!(budget.available_memory_bytes(), 8 * GIB);
        budget.release_container_reservation("runner-a");
        assert_eq!(budget.available_cpu_cores(), 3.0);
    }

    #[test]
    fn adaptive_resource_backend_guard_defers_vm_targets() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let path = std::env::temp_dir().join(format!(
            "gitrun-adaptive-vm-guard-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let rules = vec![logic_containers::LogicRule {
            name: "windows".into(),
            match_labels: vec!["windows".into()],
            backend: logic_containers::Backend::Vm { vm_name: "win-host".into() },
            image: "gitrun-runner:windows".into(),
        }];
        logic_containers::save_rules(&path.join("logic-containers.json"), &rules).unwrap();
        let vm_labels = vec!["self-hosted".into(), "Windows".into()];
        let error = ensure_adaptive_resource_backend_is_local(&path, &vm_labels).unwrap_err();
        assert!(error.contains("per-VM resource probe"));
        let local_labels = vec!["self-hosted".into(), "Linux".into()];
        assert!(ensure_adaptive_resource_backend_is_local(&path, &local_labels).is_ok());
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn pressure_defers_creation_replacement_and_plan_dependent_scale_down_only() {
        let actions = vec![
            Action::RemoveExited {
                name: "exited".into(),
            },
            Action::CreateRunner {
                permanent: true,
                job_labels: Vec::new(),
                job_name: None,
                job_run_id: None,
                job_id: None,
            },
            Action::PrewarmWorkload {
                workload_id: "wk-0123456789abcdef".into(),
                target_epoch: 3_600,
                hold_until_epoch: 4_500,
                job_name: "build".into(),
                job_labels: vec!["self-hosted".into()],
            },
            Action::RestartUnresponsive {
                name: "offline".into(),
            },
            Action::RecreateOrphaned {
                name: "orphan".into(),
                permanent: false,
            },
            Action::RemoveIdle {
                name: "idle".into(),
            },
        ];

        let filtered = actions_for_resource_pressure(actions, false);
        assert_eq!(
            filtered,
            vec![
                Action::RemoveExited {
                    name: "exited".into()
                },
                Action::RestartUnresponsive {
                    name: "offline".into()
                },
            ]
        );
    }

    #[test]
    fn prewarm_capacity_shares_the_operator_extra_runner_budget() {
        let mut targets = vec![
            gitrun_smart_scheduler::WorkloadPrewarm {
                workload_id: "wk-0123456789abcdef".into(),
                workflow_id: 7,
                workflow_path: ".github/workflows/build.yml".into(),
                job_name: "build".into(),
                labels: vec!["self-hosted".into(), "linux".into()],
                target_epoch: 3_600,
                hold_until_epoch: 4_500,
                runner_count: 2,
                prewarm_runners_created: 0,
                observed_distinct_weeks: 8,
                historical_occurrences_per_week: 2.0,
            },
            gitrun_smart_scheduler::WorkloadPrewarm {
                workload_id: "wk-fedcba9876543210".into(),
                workflow_id: 8,
                workflow_path: ".github/workflows/test.yml".into(),
                job_name: "test".into(),
                labels: vec!["self-hosted".into(), "linux".into()],
                target_epoch: 3_600,
                hold_until_epoch: 4_500,
                runner_count: 1,
                prewarm_runners_created: 0,
                observed_distinct_weeks: 6,
                historical_occurrences_per_week: 1.0,
            },
        ];

        let total = bound_workload_prewarms_to_budget(&mut targets, 1);
        assert_eq!(total, 1);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].runner_count, 1);
        assert_eq!(bound_workload_prewarms_to_budget(&mut targets, 0), 0);
        assert!(targets.is_empty());
    }

    #[test]
    fn healthy_resource_sample_preserves_the_full_plan() {
        let actions = vec![
            Action::CreateRunner {
                permanent: false,
                job_labels: vec!["linux".into()],
                job_name: Some("build".into()),
                job_run_id: Some(42),
                job_id: Some(420),
            },
            Action::RemoveIdle {
                name: "idle".into(),
            },
        ];
        assert_eq!(
            actions_for_resource_pressure(actions.clone(), true),
            actions
        );
    }
}

#[cfg(test)]
mod reconcile_identity_pin_tests {
    use super::*;

    #[test]
    fn kitty_observation_excludes_reserved_dock_pool_and_jobs() {
        let queue = vec![
            "build".to_owned(),
            "docker-special".to_owned(),
            "release".to_owned(),
        ];
        let dock_targets = vec!["docker-special".to_owned()];
        let runner_views = vec![
            RunnerView {
                name: "ordinary-busy".into(),
                online: true,
                busy: true,
            },
            RunnerView {
                name: "ordinary-idle".into(),
                online: true,
                busy: false,
            },
            RunnerView {
                name: "gitdock-reserved".into(),
                online: true,
                busy: true,
            },
        ];
        let ordinary_managed = vec!["ordinary-busy".into(), "ordinary-idle".into()];
        let counts =
            kitty_observation_counts(&queue, &dock_targets, &runner_views, &ordinary_managed);
        assert_eq!(
            counts,
            KittyObservationCounts {
                queued_jobs: 2,
                busy_runners: 1,
                online_runners: 2,
                managed_runners: 2,
            }
        );
    }

    #[test]
    fn kitty_observation_does_not_guess_when_target_job_names_are_duplicated() {
        let queue = vec!["docker-special".to_owned(), "docker-special".to_owned()];
        let dock_targets = vec!["docker-special".to_owned()];
        let counts = kitty_observation_counts(&queue, &dock_targets, &[], &[]);
        // Without a workflow/run identity in the routing contract, excluding
        // just one copy would be an unsupported guess. Conservative demand is 2.
        assert_eq!(counts.queued_jobs, 2);
    }

    #[test]
    fn kitty_observation_keeps_busy_orphan_in_pool_when_still_owned() {
        let queue = Vec::new();
        let dock_targets: Vec<String> = Vec::new();
        let runner_views = vec![RunnerView {
            name: "ordinary-orphan".into(),
            online: false,
            busy: true,
        }];
        let ordinary_managed = vec!["ordinary-orphan".into()];
        let counts =
            kitty_observation_counts(&queue, &dock_targets, &runner_views, &ordinary_managed);
        assert_eq!(counts.busy_runners, 1);
        assert_eq!(counts.online_runners, 0);
        assert_eq!(counts.managed_runners, 1);
    }

    #[test]
    fn destructive_action_requires_id_from_original_inventory() {
        let inventory = vec![docker::ManagedContainer {
            name: "gitrun-owner-repo-1".into(),
            id: "immutable-container-id".into(),
            status: "running".into(),
            permanent: false,
            dock_target: false,
            workflow_job: None,
            workflow_run_id: None,
            workflow_job_id: None,
            workload_routing_id: None,
            prewarm_workload_id: None,
            prewarm_target_epoch: None,
            prewarm_hold_until_epoch: None,
        }];
        assert_eq!(
            expected_container_id(&inventory, "gitrun-owner-repo-1").unwrap(),
            "immutable-container-id"
        );
        assert!(expected_container_id(&inventory, "replacement-name").is_err());
    }

    #[test]
    fn automation_stop_is_bound_to_the_exact_authorized_runner_id() {
        assert!(expected_runner_matches_authorized_id(Some(41), 41));
        assert!(!expected_runner_matches_authorized_id(Some(42), 41));
        assert!(expected_runner_matches_authorized_id(None, 41));
    }

    #[test]
    fn destructive_action_rejects_missing_or_replaced_container_identity() {
        assert!(check_container_identity("runner", "expected-id", None).is_err());
        assert!(check_container_identity("runner", "expected-id", Some("replacement-id")).is_err());
        assert!(check_container_identity("runner", "expected-id", Some("expected-id")).is_ok());
    }

    #[test]
    fn destructive_action_rejects_empty_inventory_id() {
        let inventory = vec![docker::ManagedContainer {
            name: "runner".into(),
            id: String::new(),
            status: "running".into(),
            permanent: false,
            dock_target: false,
            workflow_job: None,
            workflow_run_id: None,
            workflow_job_id: None,
            workload_routing_id: None,
            prewarm_workload_id: None,
            prewarm_target_epoch: None,
            prewarm_hold_until_epoch: None,
        }];
        assert!(expected_container_id(&inventory, "runner").is_err());
    }
}
