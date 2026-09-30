pub mod backoff;
pub mod docker;
pub mod github;
pub mod gsr_bridge;
pub mod gsr_poll;
pub mod gtuu;
pub mod logic_containers;
pub mod reconcile;
pub mod state;
pub mod vm;
pub mod vm_resolution;

pub use backoff::RateLimitTracker;
pub use github::{GitHubClient, GitHubError, Runner};
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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

    let client = match build_github_client(&config) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("gitrun-autoscaler: {error}");
            std::process::exit(2);
        }
    };

    let state_dir = PathBuf::from(&config.state_dir);
    let rate_limits = RateLimitTracker::new();
    spawn_gtuu_thread(&config, &stopping);
    spawn_gsr_poll_thread(&config, &vm_registry, &stopping);

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
    let vm_registry = vm_resolution::new_registry();

    // GSR integration: write our own PID to a well-known file so the
    // separate `gitrun-gsr` watchdog process can supervise us — see
    // `gitrun-gsr/src/watchdog.rs`. Removed on clean shutdown below; if this
    // process instead crashes hard, the file survives with a now-dead PID
    // in it, which is exactly the signal the watchdog looks for.
    let pid_file = state_dir.join("gitrun-autoscaler.pid");
    if let Err(error) = acquire_pid_file(&pid_file) {
        eprintln!(
            "gitrun-autoscaler: could not acquire scheduler PID file at {}: {error}",
            pid_file.display()
        );
        std::process::exit(2);
    }

    println!(
        "gitrun-autoscaler: starting, polling every {}s",
        config.poll_interval
    );
    while !stopping.load(Ordering::Relaxed) {
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
            if let Err(error) = reconcile_repo(&client, &config, &state_dir, &vm_registry, repo) {
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

fn acquire_pid_file(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(std::process::id().to_string().as_bytes())?;
            file.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
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

            std::fs::remove_file(path)?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            use std::io::Write;
            file.write_all(std::process::id().to_string().as_bytes())?;
            file.sync_all()?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    unsafe extern "C" {
        #[link_name = "kill"]
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let result = unsafe { kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(1)
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
    while Instant::now() < deadline {
        if stopping.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200).min(deadline - Instant::now()));
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
/// yet this call, THIS cycle still falls back to local, but a background
/// thread is working on bringing the VM up (KVM preferred, operator asked
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
    let rules_path = std::path::Path::new(&config.state_dir).join("logic-containers.json");
    let rules = logic_containers::load_rules(&rules_path).unwrap_or_else(|error| {
        eprintln!("gitrun-autoscaler: could not load Logic Containers rules: {error}");
        Vec::new()
    });

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
            let vm_defs = vm::load_vm_configs(state_dir).unwrap_or_else(|error| {
                eprintln!("gitrun-autoscaler: could not load VM configs: {error}");
                Vec::new()
            });
            let Some(vm_config) = vm::find_vm_config(&vm_defs, vm_name) else {
                eprintln!(
                    "gitrun-autoscaler: Logic Containers rule targets VM '{vm_name}', but no VM with that name is configured — falling back to local host for this runner"
                );
                return None;
            };
            match vm_resolution::resolve_or_spawn(vm_registry, state_dir, vm_config) {
                Some(host) => Some((host, image.to_owned(), vm_config.is_windows)),
                None => {
                    // Not resolved yet this cycle — a background thread is
                    // (or is about to be) working on it; see this
                    // function's doc comment. Not an error, just "not
                    // ready", so this stays an eprintln rather than a
                    // recorded crash.
                    eprintln!(
                        "gitrun-autoscaler: VM '{vm_name}' isn't ready yet (hypervisor setup in progress) — using local host for this runner this cycle"
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

/// Runs one reconcile cycle for a single repository: snapshot, plan, execute.
fn reconcile_repo(
    client: &GitHubClient,
    config: &Config,
    state_dir: &std::path::Path,
    vm_registry: &VmResolutionRegistry,
    repo: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let containers = docker::managed_containers(repo)?;
    let runners = client.list_runners(repo)?;
    // Was: client.queued_self_hosted_jobs(repo)? (count only). Switched to
    // the labels-carrying variant so Logic Containers can route dynamic
    // runners per job; `queued_jobs` below is just this list's length, so
    // desired_count()'s formula (busy + queued, clamped) is unchanged.
    let queued_job_labels = client.queued_self_hosted_jobs_with_labels(repo)?;
    let queued_jobs = queued_job_labels.len() as u32;

    let mut state = SchedulerState::load(state_dir)?;
    let live_names: Vec<String> = containers
        .iter()
        .filter(|c| c.status != "exited")
        .map(|c| c.name.clone())
        .collect();
    state.prune(&live_names);

    let idle: Vec<IdleInfo> = live_names
        .iter()
        .filter(|name| {
            runners
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

    // Bug fix: a container being tracked as "needs recovery" (via a prior
    // RestartUnresponsive) that becomes healthy again (online and not busy)
    // on its own, without ever going through RecreateOrphaned, previously
    // never had its recovery_since entry cleared — plan()'s "healthy, nothing
    // to do" branch doesn't emit any Action for it, and execute() only clears
    // recovery on RemoveExited/RecreateOrphaned. That stale timestamp would
    // then make a *future*, unrelated recovery episode for the same
    // container look like it's been ongoing since the old episode, defeating
    // the cooldown logic. Clear it here, once, based on the actual observed
    // health, independent of what plan() decides to do this cycle.
    for name in &live_names {
        let healthy = runners
            .iter()
            .find(|r| &r.name == name)
            .map(|r| r.is_online() && !r.busy)
            .unwrap_or(false);
        if healthy {
            state.clear_recovery(name);
        }
    }

    let recovery_age = state.recovery_ages();

    let input = ReconcileInput {
        min_runners: config.min_runners,
        max_runners: config.max_runners,
        containers: containers.iter().map(to_container_view).collect(),
        runners: runners
            .iter()
            .map(|r| RunnerView {
                name: r.name.clone(),
                online: r.is_online(),
                busy: r.busy,
            })
            .collect(),
        queued_jobs,
        queued_job_labels,
        idle,
        recovery_enabled: config.auto_container_recovery,
        recovery_cooldown: Duration::from_secs(config.container_recovery_cooldown),
        recovery_age,
        ephemeral: config.ephemeral,
        idle_timeout: Duration::from_secs(config.idle_timeout),
    };

    let actions = reconcile::plan(&input);
    for action in actions {
        // Each action is executed independently: a failure on one (e.g. a
        // transient Docker error creating one runner) is logged and the loop
        // continues with the rest, rather than the Python behavior of one
        // exception aborting the entire repo's cycle.
        if let Err(error) = execute(
            client,
            config,
            state_dir,
            vm_registry,
            repo,
            &action,
            &mut state,
        ) {
            eprintln!("gitrun-autoscaler: action {action:?} failed for {repo}: {error}");
        }
    }

    state.save()?;
    Ok(())
}

fn to_container_view(container: &ManagedContainer) -> ContainerView {
    ContainerView {
        name: container.name.clone(),
        health: if container.status == "exited" {
            ContainerHealth::Exited
        } else {
            ContainerHealth::Running
        },
        permanent: container.permanent,
    }
}

fn execute(
    client: &GitHubClient,
    config: &Config,
    state_dir: &std::path::Path,
    vm_registry: &VmResolutionRegistry,
    repo: &str,
    action: &Action,
    state: &mut SchedulerState,
) -> Result<(), Box<dyn std::error::Error>> {
    match action {
        Action::RemoveExited { name } => {
            deregister_and_remove(client, repo, name)?;
            state.clear_idle(name);
            state.clear_recovery(name);
        }
        Action::CreateRunner {
            permanent,
            job_labels,
        } => {
            create_runner(
                client,
                config,
                state_dir,
                vm_registry,
                repo,
                *permanent,
                job_labels,
            )?;
        }
        Action::RecreateOrphaned { name, permanent } => {
            deregister_and_remove(client, repo, name)?;
            // Recreation isn't tied to a specific queued job (it's replacing
            // a container GitHub lost track of, not opening new capacity),
            // so it keeps the pre-existing behavior of an empty label set.
            create_runner(
                client,
                config,
                state_dir,
                vm_registry,
                repo,
                *permanent,
                &[],
            )?;
            state.clear_recovery(name);
        }
        Action::RestartUnresponsive { name } => {
            // Track when this container first needed a restart, so the
            // cooldown in ReconcileInput.recovery_age is meaningful on the
            // *next* cycle. This call also seeds the timer on first sight.
            state.recovery_age(name);
            docker::restart_container(name)?;
        }
        Action::RemoveIdle { name } => {
            deregister_and_remove(client, repo, name)?;
            state.clear_idle(name);
        }
    }
    Ok(())
}

fn deregister_and_remove(
    client: &GitHubClient,
    repo: &str,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(runners) = client.list_runners(repo) {
        if let Some(runner) = runners.iter().find(|r| r.name == name) {
            client.delete_runner(repo, runner.id)?;
        }
    }
    docker::remove_container(name)?;
    Ok(())
}

fn create_runner(
    client: &GitHubClient,
    config: &Config,
    state_dir: &std::path::Path,
    vm_registry: &VmResolutionRegistry,
    repo: &str,
    permanent: bool,
    job_labels: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(ban_store) = gsr_poll::BanStore::load(state_dir) {
        if ban_store.is_banned(repo) {
            println!("gitrun-autoscaler: skipping runner creation for {repo}: temporarily banned by GSR after a policy violation");
            return Ok(());
        }
    }

    let registration_token = client.registration_token(repo)?;

    if config.gsr_workflow_validation_enabled {
        validate_repo_workflows_best_effort(client, config, repo);
    }

    let safe = docker::sanitize(repo, '-');
    let name = format!("gitrun-{safe}-{}", uuid_like_suffix());
    docker::ensure_shared_cache_volume(&config.shared_cache_volume)?;
    let docker_socket_gid = resolve_docker_socket_gid()?;
    let secret_env = vault_env_for_repo(config, repo);

    let Some((backend, image, is_windows)) =
        resolve_backend_and_image(config, state_dir, vm_registry, job_labels)
    else {
        // Never create a VM-targeted job runner on the local host while VM
        // resolution is pending; doing so could let it claim the wrong job.
        return Ok(());
    };

    docker::create_runner_on(
        &backend,
        &docker::RunnerSpec {
            name: &name,
            repo,
            permanent,
            registration_token: &registration_token,
            image: &image,
            labels: &config.runner_labels,
            ephemeral: config.ephemeral,
            disable_update: config.runner_disable_update,
            cpus: &config.container_cpus,
            memory: &config.container_memory,
            pids_limit: &config.container_pids_limit,
            shared_cache_volume: &config.shared_cache_volume,
            docker_socket_gid: &docker_socket_gid,
            runner_home_size: &config.runner_home_size,
            home_backend: docker::RunnerHomeBackend::from_config_str(&config.runner_home_backend),
            secret_env: &secret_env,
            is_windows,
            docker_socket_hardening: config.gsr_docker_socket_hardening,
        },
    )?;
    Ok(())
}

/// Best-effort pre-flight workflow validation, run just before a runner is
/// created for `repo` — see `gitrun_core::workflow_validation` for the
/// scanning logic itself and `GitHubClient::workflow_files` for why this
/// fetches via the Contents API rather than waiting for the runner's own
/// checkout (too late for a pre-flight check to matter by then).
///
/// Deliberately never fails `create_runner` — a network hiccup fetching
/// workflow files, or a finding in someone's workflow, should not block a
/// job that would otherwise run fine; this is advisory, not enforcement
/// (enforcement of *commands actually run* is `gitrun_gsr::agent` and
/// `gsr_poll`'s job, which unlike this can and does act). Findings are
/// written to the GSR event log so an operator can review a growing
/// pattern of risky workflow changes even though no single one blocks
/// anything by itself.
fn validate_repo_workflows_best_effort(client: &GitHubClient, config: &Config, repo: &str) {
    let events_path = gitrun_gsr::events::default_queue_path(&config.state_dir);
    let files = match client.workflow_files(repo) {
        Ok(files) => files,
        Err(error) => {
            eprintln!("gitrun-autoscaler: workflow validation for {repo}: could not fetch workflow files: {error}");
            return;
        }
    };

    let mut findings = Vec::new();
    for (name, content) in &files {
        findings.extend(gitrun_core::scan(name, content));
    }

    if config.gsr_zizmor_enabled {
        // zizmor works off files on disk, not in-memory strings - write
        // the fetched content to a scratch directory under state_dir so
        // it can run against exactly what was fetched, then clean up
        // regardless of outcome.
        let scratch_dir = std::path::PathBuf::from(&config.state_dir)
            .join("gsr-workflow-scratch")
            .join(docker::sanitize(repo, '_'));
        if let Err(error) = write_scratch_workflows(&scratch_dir, &files) {
            eprintln!("gitrun-autoscaler: workflow validation for {repo}: could not stage files for zizmor: {error}");
        } else {
            match gitrun_core::run_zizmor(&scratch_dir) {
                Ok(Some(zizmor_findings)) => findings.extend(zizmor_findings),
                Ok(None) => {} // zizmor not installed - silently skipped, per its own design.
                Err(error) => eprintln!(
                    "gitrun-autoscaler: workflow validation for {repo}: zizmor run failed: {error}"
                ),
            }
        }
        let _ = std::fs::remove_dir_all(&scratch_dir);
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
            eprintln!("gitrun-autoscaler: workflow validation for {repo}: failed to write security event: {error}");
        }
    }
}

fn write_scratch_workflows(
    dir: &std::path::Path,
    files: &[(String, String)],
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, content) in files {
        std::fs::write(dir.join(name), content)?;
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
fn vault_env_for_repo(config: &Config, repo: &str) -> Vec<(String, String)> {
    if config.vault_dir.trim().is_empty() {
        return Vec::new();
    }
    let bridge = VaultToGsrBridge::new(&config.state_dir);
    let vault = match Vault::open_with_sink(&config.vault_dir, Box::new(bridge)) {
        Ok(vault) => vault,
        Err(error) => {
            eprintln!(
                "gitrun-autoscaler: could not open GitVault at {}: {error}",
                config.vault_dir
            );
            return Vec::new();
        }
    };

    let groups = config.vault_groups_for_repo(repo);
    // resolve_for_repo already applies repo > group > global precedence and
    // only decrypts secrets that actually apply to this repo — see
    // gitrun-vault's module docs for the full resolution rules.
    vault
        .resolve_for_repo(repo, &groups)
        .into_iter()
        .filter(|(name, _)| {
            if is_valid_env_var_name(name) {
                true
            } else {
                eprintln!("gitrun-autoscaler: skipping vault secret '{name}': not a valid environment variable name");
                false
            }
        })
        .collect()
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

/// Short random-ish suffix for container names, avoiding a dependency on the
/// `uuid` crate for eight hex characters.
fn uuid_like_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("{:08x}", nanos ^ pid)
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
    let docker_socket_gid = resolve_docker_socket_gid()?;
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
        docker_socket_gid: &docker_socket_gid,
        runner_home_size: &config.runner_home_size,
        runner_home_backend: docker::RunnerHomeBackend::from_config_str(
            &config.runner_home_backend,
        ),
        secret_env_for_repo: &|repo: &str| vault_env_for_repo(&config, repo),
        online_wait_timeout: Duration::from_secs(120),
        docker_socket_hardening: config.gsr_docker_socket_hardening,
    };
    Ok(gtuu::update_permanent_containers(&client, &gtuu_config)?)
}
fn spawn_gtuu_thread(config: &Config, stopping: &Arc<AtomicBool>) {
    let config = config.clone();
    let auto_update = config.auto_container_update;
    let update_time = config.container_update_time.clone();
    let use_local_time = config.gtuu_schedule_timezone == "local";
    let state_dir = PathBuf::from(&config.state_dir);
    let stopping = stopping.clone();

    std::thread::Builder::new()
        .name("gitrun-gtuu".into())
        .spawn(move || {
            let mut last_run_date: Option<String> = None;
            while !stopping.load(Ordering::Relaxed) {
                let now = chrono_like_now(use_local_time);
                if auto_update
                    && now.time_hhmm == update_time
                    && last_run_date.as_deref() != Some(&now.date)
                {
                    println!("gitrun-autoscaler: GTUU scheduled run starting");
                    match GtuuLock::acquire(&state_dir) {
                        Ok(_lock) => match run_gtuu_once() {
                            Ok(count) => println!(
                                "gitrun-autoscaler: GTUU updated {count} permanent runner(s)"
                            ),
                            Err(error) => eprintln!("gitrun-autoscaler: GTUU run failed: {error}"),
                        },
                        Err(error) => eprintln!("gitrun-autoscaler: GTUU skipped: {error}"),
                    }
                    last_run_date = Some(now.date);
                }
                std::thread::sleep(Duration::from_secs(15));
            }
        })
        .expect("failed to spawn GTUU thread");
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
) {
    let Some(policy) = config.command_policy() else {
        return;
    };
    let action = config.violation_action();
    let state_dir = PathBuf::from(&config.state_dir);
    let events_path = gitrun_gsr::events::default_queue_path(&config.state_dir);
    let stopping = stopping.clone();
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
                    let vm_registry = Arc::clone(vm_registry);
                    move || {
                        let mut hosts = vec![docker::DockerHost::Local];
                        let guard = vm_registry.lock().unwrap_or_else(|p| p.into_inner());
                        for resolution in guard.values() {
                            if let vm_resolution::VmResolution::Resolved(host) = resolution {
                                if !hosts.contains(host) {
                                    hosts.push(host.clone());
                                }
                            }
                        }
                        hosts
                    }
                },
                move |violation| {
                action,
                poll_interval,
                move |violation| {
                    let repo_note = violation.repo.as_deref().unwrap_or("unknown repo");
                    let message = format!(
                        "container {} ({repo_note}) ran a denied command: {} — {} (action: {:?})",
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
        .expect("failed to spawn GSR poll thread");
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

/// Runs `date +%H:%M` and `date +%Y-%m-%d` (no `-u`, so system local time,
/// DST-aware) and parses the output. Returns `None` on any failure —
/// binary missing, non-zero exit, unparseable output — so the caller can
/// fall back rather than panic.
fn local_now_via_date_command() -> Option<SimpleNow> {
    let time_out = std::process::Command::new("date")
        .arg("+%H:%M")
        .output()
        .ok()?;
    let date_out = std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()?;
    if !time_out.status.success() || !date_out.status.success() {
        return None;
    }
    let time_hhmm = String::from_utf8(time_out.stdout).ok()?.trim().to_owned();
    let date = String::from_utf8(date_out.stdout).ok()?.trim().to_owned();
    if time_hhmm.len() != 5 || date.len() != 10 {
        return None; // sanity check: "HH:MM" / "YYYY-MM-DD", reject anything malformed
    }
    Some(SimpleNow { time_hhmm, date })
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
