//! Host-side workflow Git*Run API service.
//!
//! The runner image contains only a small CLI client. This service owns the
//! privileged side: it authenticates the runner capability, asks GSR to
//! authorize the exact operation, and only then invokes GitExecuteRun's
//! backend implementation.

use crate::dock_registry::{DockBinding, DockRegistry};
use crate::docker;
use crate::GitHubClient;
use gitrun_core::{Config, GitRunApi, GitRunOperation, GitRunSettings};
use gitrun_exe::{
    ipc::{WireRequest, WireResponse, DEFAULT_SOCKET_PATH},
    AuthorizedOperation, ExecutionBackend, ExecutionError, ExecutionEvent, ExecutionResult,
};
use gitrun_gsr::api_gate::{authorize, ExecutionAuthority, VerifiedCaller};
use gitrun_vault::{Scope, Vault};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::Mutex;
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

const DOCK_ONLY_MARKER: &str = "/home/runner/actions-runner/.gitrun-dock-only";
const API_MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_VAULT_WRITE_BYTES: usize = 1024 * 1024;
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
        if !file
            .metadata()
            .map_err(|error| format!("inspect mount registry lock: {error}"))?
            .is_file()
        {
            return Err("mount registry lock path is not a regular file".into());
        }
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("secure mount registry lock permissions: {error}"))?;
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
impl MountRegistry {
    fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(MOUNT_REGISTRY_FILE)
    }

    fn load(state_dir: &Path) -> Result<Self, String> {
        match fs::read_to_string(Self::path(state_dir)) {
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|error| format!("decode GitDockRun mount registry: {error}")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("read GitDockRun mount registry: {error}")),
        }
    }

    fn save(&self, state_dir: &Path) -> Result<(), String> {
        fs::create_dir_all(state_dir)
            .map_err(|error| format!("create mount registry directory: {error}"))?;
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
                        Ok((stream, _)) => {
                            let service_config = config.clone();
                            let service_github = Arc::clone(&github);
                            let service_state = state_dir.clone();
                            let service_authority = Arc::clone(&execution_authority);
                            std::thread::Builder::new()
                                .name("gitrun-api-request".into())
                                .spawn(move || {
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
                                .ok();
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
    if let Err(error) = reconcile_completed_mounts(client, state_dir, repository) {
        eprintln!("gitrun-api: mount cleanup reconciliation failed: {error}");
    }
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

pub(crate) fn is_dock_bound(
    state_dir: &Path,
    repository: &str,
    container: &str,
) -> Result<bool, String> {
    let registry = DockRegistry::load(state_dir)
        .map_err(|error| format!("load GitDockRun registry: {error}"))?;
    Ok(registry
        .bindings
        .iter()
        .any(|binding| binding.repository == repository && binding.container == container))
}

#[cfg(unix)]
fn handle_stream(
    mut stream: UnixStream,
    config: Config,
    github: Arc<GitHubClient>,
    state_dir: std::path::PathBuf,
    execution_authority: Arc<ExecutionAuthority>,
) -> Result<(), String> {
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|e| format!("clone API socket: {e}"))?,
    );
    let mut encoded = String::new();
    let read = reader
        .by_ref()
        .take(API_MAX_REQUEST_BYTES as u64 + 1)
        .read_line(&mut encoded)
        .map_err(|e| format!("read API request: {e}"))?;
    if read == 0 {
        return Ok(());
    }
    if encoded.len() > API_MAX_REQUEST_BYTES {
        send_error(&mut stream, "API request is too large")?;
        return Ok(());
    }

    let request: WireRequest = match serde_json::from_str(encoded.trim_end()) {
        Ok(request) => request,
        Err(error) => {
            send_error(&mut stream, &format!("invalid API request: {error}"))?;
            return Ok(());
        }
    };

    let peer_pid =
        peer_pid(&stream).map_err(|error| format!("read API socket peer credentials: {error}"))?;
    let identity = match docker::runner_for_peer_pid(peer_pid) {
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

    if identity.name != request.invocation.runner
        || identity.repository != request.invocation.repository
    {
        send_error(
            &mut stream,
            "runner API identity does not match the managed container",
        )?;
        return Ok(());
    }

    let Some(run_id) = request.invocation.run_id else {
        send_error(
            &mut stream,
            "GITHUB_RUN_ID is required for every GitRun API invocation",
        )?;
        return Ok(());
    };

    match (identity.workflow_run_id, identity.workflow_job.as_deref()) {
        (Some(bound_run), Some(bound_job))
            if bound_run != run_id || bound_job != request.invocation.job =>
        {
            send_error(
                &mut stream,
                "workflow job identity does not match the runner container binding",
            )?;
            return Ok(());
        }
        (Some(_), None) | (None, Some(_)) => {
            send_error(
                &mut stream,
                "runner container has incomplete workflow binding metadata",
            )?;
            return Ok(());
        }
        (Some(_), Some(_)) => {
            // The guarded arm above rejects mismatched bindings; this arm
            // covers the valid case where the bound run/job matches exactly.
        }
        (None, None) => {
            let Some(active_job) = github
                .find_active_workflow_job_for_runner(&request.invocation.repository, &identity.name)
                .map_err(|error| format!("verify active workflow job: {error}"))?
            else {
                send_error(
                    &mut stream,
                    "no active GitHub workflow job is assigned to this runner",
                )?;
                return Ok(());
            };
            if active_job.name != request.invocation.job {
                send_error(
                    &mut stream,
                    "requested workflow job is not the active job assigned to this runner",
                )?;
                return Ok(());
            }
        }
    }

    let job = request.invocation.job.clone();
    let current_job = github
        .find_workflow_job(&request.invocation.repository, run_id, &job)
        .map_err(|error| format!("verify current workflow job: {error}"))?;
    let Some(current_job) = current_job else {
        send_error(
            &mut stream,
            "current GitHub workflow job could not be verified",
        )?;
        return Ok(());
    };
    if current_job.runner_name.as_deref() != Some(identity.name.as_str())
        || !current_job.status.eq_ignore_ascii_case("in_progress")
        || current_job.conclusion.is_some()
    {
        send_error(
            &mut stream,
            "GitRun API request is not being made by the active runner for this workflow job",
        )?;
        return Ok(());
    }

    let workflow_run = github
        .find_workflow_run(&request.invocation.repository, run_id)
        .map_err(|error| format!("verify authoritative workflow run: {error}"))?;
    let Some(workflow_run) = workflow_run else {
        send_error(
            &mut stream,
            "authoritative GitHub workflow run could not be verified",
        )?;
        return Ok(());
    };
    if !request.invocation.workflow.is_empty() && request.invocation.workflow != workflow_run.name {
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

    let authorized = match authorize(
        &settings,
        &caller,
        request.invocation.api,
        request.invocation.operation,
        request.invocation.resource.as_deref(),
        request.invocation.arguments.clone(),
        command_policy.as_ref(),
    ) {
        Ok(operation) => operation,
        Err(error) => {
            send_error(&mut stream, &format!("GSR validation failed: {error}"))?;
            return Ok(());
        }
    };

    let authorized = execution_authority
        .handoff_to_executor(authorized)
        .map_err(|error| format!("GSR executor handoff failed: {error}"))?;

    send_response(&mut stream, WireResponse::Accepted)?;

    let backend = ApiExecutionBackend {
        config,
        state_dir,
        github,
        caller,
        caller_container_id: identity.container_id.clone(),
    };

    let mut send = |event: ExecutionEvent| {
        let _ = send_response(&mut stream, WireResponse::Event(event));
    };

    if let Err(error) = backend.execute_stream(&authorized, &mut send) {
        send_error(&mut stream, &format!("GitExecuteRun failed: {error}"))?;
    }

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

                Ok(success(format!("::add-mask::{value}\n{value}\n")))
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
                Ok(success(format!(
                    "{}\n",
                    vault.effective_contains_for_repo(name, &request.repository, &groups)
                )))
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
        let runner = &self.caller.runner;

        match request.operation {
            GitRunOperation::File => {
                let source = arg(request, "path")?;
                ensure_safe_runner_path(source)?;

                let name = Path::new(source)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| failed("GitSaveRun source has no safe file name"))?;
                ensure_safe_file_name(name)?;

                let size = docker_file_size(runner, source)?;
                let policy = GitRunSettings::load_or_default(GitRunSettings::path_for_state_dir(
                    &self.state_dir,
                ))
                .map_err(|e| failed(e.to_string()))?
                .effective_for_repository(&request.repository)
                .storage;

                if size > policy.max_file_size_bytes {
                    return Err(failed("GitSaveRun file exceeds repository storage limit"));
                }

                docker_exec_checked(runner, &["mkdir", "-p", "/var/lib/gitrun/shared/files"])?;
                docker_exec_checked(
                    runner,
                    &[
                        "cp",
                        "--",
                        source,
                        &format!("/var/lib/gitrun/shared/files/{name}"),
                    ],
                )?;

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

                let output = docker::exec_container_with_stdin(
                    runner,
                    &[
                        "sh",
                        "-c",
                        "mkdir -p /var/lib/gitrun/shared/logs && cat > \"$1\"",
                        "gitrun-save-logs",
                        &format!("/var/lib/gitrun/shared/logs/{name}"),
                    ],
                    logs.as_bytes(),
                )
                .map_err(|error| failed(error.to_string()))?;
                if !output.status.success() {
                    return Err(failed("unable to save GitRun logs into shared storage"));
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
        let permanent = arg(request, "permanent")? == "true";

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
            &self.caller.runner,
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
        run_docker_command(&self.caller.runner, &args)
    }

    fn read(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let path = arg(request, "path")?;
        ensure_safe_runner_path(path)?;

        let output = docker::exec_container(&self.caller.runner, &["cat", "--", path])
            .map_err(|error| failed(error.to_string()))?;
        command_output(output)
    }

    fn write(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let path = arg(request, "path")?;
        let value = arg(request, "value")?;
        ensure_safe_runner_path(path)?;

        let output = docker::exec_container_with_stdin(
            &self.caller.runner,
            &["sh", "-c", "cat > \"$1\"", "gitrun-write", path],
            value.as_bytes(),
        )
        .map_err(|error| failed(error.to_string()))?;
        command_output(output)
    }

    fn verify(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let path = arg(request, "path")?;
        ensure_safe_runner_path(path)?;

        let output = docker::exec_container(&self.caller.runner, &["sha256sum", "--", path])
            .map_err(|error| failed(error.to_string()))?;
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

        // Capture the immutable container ID used for the actual execution.
        // The binding is invalid if this name later resolves to another ID.
        let container_id = docker::container_id(&runner_name)
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| {
                failed(format!(
                    "runner container {runner_name} disappeared during connect"
                ))
            })?;

        let dynamic = !docker::container_is_permanent(&runner_name)
            .map_err(|error| failed(error.to_string()))?;

        let completed = workflow_job_is_completed(&job_info.status, job_info.conclusion.as_deref());

        if dynamic
            && docker::container_status(&runner_name)
                .map_err(|error| failed(error.to_string()))?
                .is_none()
        {
            return Err(failed(format!(
                "runner container {runner_name} disappeared during connect"
            )));
        }

        if dynamic && completed {
            make_dock_only(&runner_name, &self.state_dir)
                .map_err(|error| failed(error.to_string()))?;
        }

        let mut registry =
            DockRegistry::load(&self.state_dir).map_err(|error| failed(error.to_string()))?;
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
        registry
            .save(&self.state_dir)
            .map_err(|error| failed(error.to_string()))?;

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
        let mount_guard = GITDOCK_MOUNT_LOCK
            .lock()
            .map_err(|_| failed("GitDockRun mount registry lock is poisoned"))?;
        #[cfg(target_os = "linux")]
        let mount_registry_lock = MountRegistryLock::acquire(&self.state_dir).map_err(failed)?;

        let mut registry =
            DockRegistry::load(&self.state_dir).map_err(|error| failed(error.to_string()))?;
        let binding = registry
            .binding(&request.repository, run_id, job)
            .cloned()
            .ok_or_else(|| failed(format!("no GitDockRun binding exists for job {job}")))?;
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
            let owned_mounts: Vec<MountRecord> = mount_registry
                .mounts
                .iter()
                .filter(|mount| {
                    mount.repository == request.repository
                        && mount.run_id == run_id
                        && mount.job == job
                        && mount.requester_runner == self.caller.runner
                })
                .cloned()
                .collect();

            for mount in &owned_mounts {
                cleanup_mount_record(mount).map_err(|error| {
                    failed(format!(
                        "cannot disconnect while GitDockRun mount {} needs cleanup: {error}",
                        mount.id
                    ))
                })?;
            }

            if !owned_mounts.is_empty() {
                let owned_ids: Vec<&str> =
                    owned_mounts.iter().map(|mount| mount.id.as_str()).collect();
                mount_registry
                    .mounts
                    .retain(|mount| !owned_ids.contains(&mount.id.as_str()));
                mount_registry.save(&self.state_dir).map_err(failed)?;
            }
        }

        let binding = registry
            .remove(&request.repository, run_id, job)
            .ok_or_else(|| failed(format!("no GitDockRun binding exists for job {job}")))?;
        registry
            .save(&self.state_dir)
            .map_err(|error| failed(error.to_string()))?;

        #[cfg(target_os = "linux")]
        {
            drop(mount_registry_lock);
            drop(mount_guard);
        }

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

        let output = docker::exec_container(container.as_str(), &["cat", "--", path])
            .map_err(|error| failed(error.to_string()))?;
        command_output(output)
    }

    fn dock_write(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let container = self.require_dock_binding(request)?;
        let path = arg(request, "path")?;
        let value = arg(request, "value")?;
        ensure_safe_runner_path(path)?;

        let output = docker::exec_container_with_stdin(
            container.as_str(),
            &["sh", "-c", "cat > \"$1\"", "gitrun-dock-write", path],
            value.as_bytes(),
        )
        .map_err(|error| failed(error.to_string()))?;
        command_output(output)
    }

    fn dock_execute_stream(
        &self,
        request: &AuthorizedOperation,
        sink: &mut dyn FnMut(ExecutionEvent),
    ) -> Result<i32, ExecutionError> {
        let container = self.require_dock_binding(request)?;
        let command = arg(request, "command")?;

        docker::exec_container_stream(
            container.as_str(),
            &["sh", "-c", command],
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

    fn dock_execute(
        &self,
        request: &AuthorizedOperation,
    ) -> Result<ExecutionResult, ExecutionError> {
        let container = self.require_dock_binding(request)?;
        let command = arg(request, "command")?;
        run_docker_command(container.as_str(), &["sh", "-c", command])
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
        let _guard = GITDOCK_MOUNT_LOCK
            .lock()
            .map_err(|_| failed("GitDockRun mount registry lock is poisoned"))?;
        let _registry_lock = MountRegistryLock::acquire(&self.state_dir).map_err(failed)?;
        let binding = self.require_dock_binding_record(request)?;
        let source = arg(request, "source")?;
        let target = arg(request, "target")?;
        let read_only = match arg(request, "read_only")? {
            "true" => true,
            "false" => false,
            _ => return Err(failed("read_only must be true or false")),
        };

        let settings =
            GitRunSettings::load_or_default(GitRunSettings::path_for_state_dir(&self.state_dir))
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
                return match registry.save(&self.state_dir) {
                        Ok(()) => Err(failed(format!(
                            "reload GitDockRun mount policy before attach: {error}"
                        ))),
                        Err(save_error) => Err(failed(format!(
                            "reload GitDockRun mount policy before attach: {error}; failed to remove pre-attach intent: {save_error}"
                        ))),
                    };
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
        let source = self.require_dock_binding(request)?;
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
        if status == "running" {
            return Err(failed(
                "GitDockRun --melt requires a stopped or dock-only source container",
            ));
        }

        let mut registry =
            DockRegistry::load(&self.state_dir).map_err(|error| failed(error.to_string()))?;

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
            let target_binding = registry.bindings.iter().find(|binding| {
                binding.repository == request.repository
                    && binding.run_id == request_run_id(request)
                    && binding.job == request.job
                    && binding.requester_runner == self.caller.runner
                    && binding.container == target_name
                    && binding.container_id == target_id
            });
            if target_binding.is_none() {
                return Err(failed(
                    "GitDockRun --melt target is outside the caller's exact trust binding",
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

        docker::melt_filesystem(&source, &target_id).map_err(|error| failed(error.to_string()))?;
        let _ = docker::remove_container(&source);

        registry
            .bindings
            .retain(|binding| binding.container != source && binding.container_id != source);
        registry
            .save(&self.state_dir)
            .map_err(|error| failed(error.to_string()))?;

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
    // race it if its capability bounding set permits CAP_SYS_ADMIN. Inspect
    // every currently visible process in this immutable container cgroup and
    // every thread's CapBnd. Bounding sets can only shrink, so requiring the
    // capability to be absent everywhere prevents descendants from acquiring
    // it after this check. If a matching process cannot be inspected, fail
    // closed rather than treating the init process as representative.
    if container_has_cap_sys_admin_in_bounding_set(container_id)? {
        return Err(
            "live GitDockRun mounts are refused because a process in the container can obtain CAP_SYS_ADMIN"
                .into(),
        );
    }
    Ok(pid)
}

#[cfg(target_os = "linux")]
fn container_has_cap_sys_admin_in_bounding_set(container_id: &str) -> Result<bool, String> {
    let cap_sys_admin = 1u64 << 21;
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
            let bounding = status
                .lines()
                .find_map(|line| line.strip_prefix("CapBnd:"))
                .and_then(|value| u64::from_str_radix(value.trim(), 16).ok())
                .ok_or_else(|| format!("thread {tid} has missing or invalid CapBnd"))?;
            if bounding & cap_sys_admin != 0 {
                return Ok(true);
            }
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
    // If the service crashed after the kernel bind but before persisting its
    // mount ID, recovery may identify it only by a unique match at the exact
    // durable target and original source device/inode. Never fall back to
    // unmounting by target path alone; ambiguous or legacy leases are preserved.
    let expected_mount_id = match record.kernel_mount_id {
        Some(mount_id) => Some(mount_id),
        None => recover_mount_id_by_source_identity(pid, record)?,
    };
    let Some(expected_mount_id) = expected_mount_id else {
        // Only the durable "preparing" state proves that the bind worker had
        // not yet been entered. Once state is "attaching" (or a later recovery
        // state), the mount may have landed on a renamed target or the service
        // may have crashed before its mount ID was persisted. Preserve the
        // lease for manual recovery instead of deleting the only journal entry.
        if record.state == "preparing" {
            return Ok(());
        }
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
    let _guard = GITDOCK_MOUNT_LOCK
        .lock()
        .map_err(|_| "GitDockRun mount registry lock is poisoned".to_owned())?;
    let _registry_lock = MountRegistryLock::acquire(state_dir)?;
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
    fn workflow_mount_cleanup_requires_completed_job_status() {
        assert!(!workflow_job_is_completed("queued", None));
        assert!(!workflow_job_is_completed("waiting", None));
        assert!(!workflow_job_is_completed("in_progress", None));
        assert!(workflow_job_is_completed("completed", None));
        assert!(!workflow_job_is_completed("in_progress", Some("cancelled")));
        assert!(workflow_job_is_completed("COMPLETED", Some("success")));
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
        let mut wrong_identity_lease = interrupted_lease.clone();
        wrong_identity_lease.kernel_mount_id = Some(recovered_candidate.id);
        wrong_identity_lease.source_inode = Some(source_metadata.ino().wrapping_add(1));
        assert!(cleanup_mount_lease_in_namespace(pid, &wrong_identity_lease).is_err());
        assert!(mountinfo_by_id(pid, recovered_candidate.id)
            .expect("verify source mismatch did not unmount")
            .is_some());

        cleanup_mount_lease_in_namespace(pid, &interrupted_lease)
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

        drop(container_guard);
        drop(source_guard);
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

fn request_run_id(request: &AuthorizedOperation) -> u64 {
    request.run_id.unwrap_or(0)
}

fn docker_exec_checked(container: &str, args: &[&str]) -> Result<ExecutionResult, ExecutionError> {
    let output =
        docker::exec_container(container, args).map_err(|error| failed(error.to_string()))?;
    command_output(output)
}

fn run_docker_command(container: &str, args: &[&str]) -> Result<ExecutionResult, ExecutionError> {
    let output =
        docker::exec_container(container, args).map_err(|error| failed(error.to_string()))?;
    command_output(output)
}

fn command_output(output: std::process::Output) -> Result<ExecutionResult, ExecutionError> {
    Ok(ExecutionResult {
        exit_code: output.status.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn docker_file_size(container: &str, path: &str) -> Result<u64, ExecutionError> {
    let output = docker::exec_container(container, &["stat", "-c", "%s", "--", path])
        .map_err(|error| failed(error.to_string()))?;
    if !output.status.success() {
        return Err(failed("GitSaveRun could not stat source file"));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|_| failed("GitSaveRun source file size was invalid"))
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

    let allowed = [
        "/home/runner/",
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

fn redact_vault_values(config: &Config, repository: &str, text: &str) -> String {
    if config.vault_dir.trim().is_empty() {
        return text.to_owned();
    }

    let groups = config.vault_groups_for_repo(repository);
    let bridge = crate::gsr_bridge::VaultToGsrBridge::new(&config.state_dir);
    let vault = match Vault::open_with_sink(&config.vault_dir, Box::new(bridge)) {
        Ok(vault) => vault,
        Err(_) => return text.to_owned(),
    };
    let mut result = text.to_owned();
    for (_, value) in vault.resolve_for_repo(repository, &groups) {
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
    let _ = docker::remove_file_in_container(container, DOCK_ONLY_MARKER);
    match docker::container_status(container).map_err(|error| failed(error.to_string()))? {
        Some(status) if status == "running" => {
            docker::stop_container(container).map_err(|error| failed(error.to_string()))?;
            docker::start_container(container).map_err(|error| failed(error.to_string()))?;
        }
        Some(_) => docker::start_container(container).map_err(|error| failed(error.to_string()))?,
        None => return Err(failed("runner container disappeared during restore")),
    }
    Ok(())
}

fn make_dock_only(container: &str, state_dir: &Path) -> Result<(), ExecutionError> {
    match docker::container_status(container).map_err(|error| failed(error.to_string()))? {
        Some(status) if status == "running" => {
            docker_exec_checked(container, &["touch", "--", DOCK_ONLY_MARKER])?;
            docker::stop_container(container).map_err(|error| failed(error.to_string()))?;
        }
        Some(_) => {
            let marker = state_dir.join(format!(".dock-marker-{}-{}", std::process::id(), now()));
            fs::write(&marker, b"GitDockRun\n").map_err(|error| failed(error.to_string()))?;
            docker::copy_to_container(container, &marker, DOCK_ONLY_MARKER)
                .map_err(|error| failed(error.to_string()))?;
            let _ = fs::remove_file(&marker);
        }
        None => return Err(failed("dock source container disappeared")),
    }

    docker::start_container(container).map_err(|error| failed(error.to_string()))?;
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

impl RegisterRegistry {
    fn load(path: &Path) -> Result<Self, ExecutionError> {
        match fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|error| failed(format!("decode register registry: {error}"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(failed(format!("read register registry: {error}"))),
        }
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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| failed(format!("create register registry directory: {error}")))?;
        }
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        let raw = serde_json::to_vec_pretty(self)
            .map_err(|error| failed(format!("encode register registry: {error}")))?;
        fs::write(&tmp, raw)
            .map_err(|error| failed(format!("write register registry: {error}")))?;
        fs::rename(&tmp, path)
            .map_err(|error| failed(format!("commit register registry: {error}")))?;
        Ok(())
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}
