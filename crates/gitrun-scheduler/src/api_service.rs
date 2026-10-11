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
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

const DOCK_ONLY_MARKER: &str = "/home/runner/actions-runner/.gitrun-dock-only";
const API_MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_VAULT_WRITE_BYTES: usize = 1024 * 1024;
const MOUNT_REGISTRY_FILE: &str = "gitdock-mount-registry.json";
static GITDOCK_MOUNT_LOCK: Mutex<()> = Mutex::new(());

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
    kernel_mount_id: Option<u32>,
    #[serde(default)]
    state: String,
    created_at: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct MountRegistry {
    mounts: Vec<MountRecord>,
}

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
        let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
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

        let completed = job.status.eq_ignore_ascii_case("completed") || job.conclusion.is_some();

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

        let completed =
            job_info.status.eq_ignore_ascii_case("completed") || job_info.conclusion.is_some();
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

        let completed =
            job_info.status.eq_ignore_ascii_case("completed") || job_info.conclusion.is_some();

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

        if binding.dynamic {
            let job_info = self
                .github
                .find_workflow_job(&request.repository, run_id, job)
                .map_err(|error| failed(error.to_string()))?
                .ok_or_else(|| failed(format!("workflow job {job} could not be verified")))?;
            let completed =
                job_info.status.eq_ignore_ascii_case("completed") || job_info.conclusion.is_some();
            if !completed {
                return Err(failed(
                    "GitDockRun --disconnect cannot remove a Dynamic container while its source job is running",
                ));
            }
        }

        let binding = registry
            .remove(&request.repository, run_id, job)
            .ok_or_else(|| failed(format!("no GitDockRun binding exists for job {job}")))?;
        registry
            .save(&self.state_dir)
            .map_err(|error| failed(error.to_string()))?;

        if binding.dynamic {
            let _ = docker::remove_container(&binding.container_id);
        } else if docker_container_has_marker(&binding.container_id)? {
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

    fn dock_mount(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let _guard = GITDOCK_MOUNT_LOCK
            .lock()
            .map_err(|_| failed("GitDockRun mount registry lock is poisoned"))?;
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
        let mount_policy = settings
            .effective_for_repository(&request.repository)
            .docker
            .allowed_mounts;
        let canonical_source = mount_policy
            .authorize_source(Path::new(source), read_only)
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
        if current_id != binding.container_id || current_id != self.caller_container_id {
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
            let mounted = match record.kernel_mount_id {
                Some(id) => mountinfo_by_id(pid, id).map_err(failed)?,
                None => mountinfo_by_target(pid, target).map_err(failed)?,
            };
            if mounted.is_some() {
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
            kernel_mount_id: None,
            state: "preparing".into(),
            created_at: now(),
        };
        registry.mounts.push(record.clone());
        // Persist intent before the privileged side effect so recovery can
        // identify and roll back a partially-created mount.
        registry.save(&self.state_dir).map_err(failed)?;

        if let Err(error) = execute_mount_namespace(
            pid,
            Some(source_handle.as_raw_fd()),
            target,
            MountAction::Bind,
        ) {
            if mountinfo_by_target(pid, target).ok().flatten().is_some() {
                if execute_mount_namespace(pid, None, target, MountAction::Unmount).is_err() {
                    record.state = "cleanup-required".into();
                    registry.mounts.retain(|item| item.id != mount_id);
                    registry.mounts.push(record);
                    let _ = registry.save(&self.state_dir);
                    return Err(failed(format!(
                        "mount failed and rollback failed; mount {mount_id} is registered for cleanup: {error}"
                    )));
                }
            }
            registry.mounts.retain(|item| item.id != mount_id);
            let _ = registry.save(&self.state_dir);
            return Err(failed(error));
        }

        if read_only {
            if let Err(error) =
                execute_mount_namespace(pid, None, target, MountAction::RemountReadOnly)
            {
                if execute_mount_namespace(pid, None, target, MountAction::Unmount).is_err() {
                    record.state = "cleanup-required".into();
                    record.kernel_mount_id = mountinfo_by_target(pid, target)
                        .ok()
                        .flatten()
                        .map(|mount| mount.id);
                    registry.mounts.retain(|item| item.id != mount_id);
                    registry.mounts.push(record);
                    let _ = registry.save(&self.state_dir);
                    return Err(failed(format!(
                        "read-only remount failed and rollback failed; mount {mount_id} is registered for cleanup: {error}"
                    )));
                }
                registry.mounts.retain(|item| item.id != mount_id);
                let _ = registry.save(&self.state_dir);
                return Err(failed(format!(
                    "could not enforce read-only mount: {error}"
                )));
            }
        }

        let mount_info = mountinfo_by_target(pid, target)
            .map_err(failed)?
            .ok_or_else(|| {
                failed("mount command returned success but no matching mount appeared")
            })?;
        record.kernel_mount_id = Some(mount_info.id);
        record.state = "mounted".into();
        registry.mounts.retain(|item| item.id != mount_id);
        registry.mounts.push(record);
        registry.save(&self.state_dir).map_err(|error| {
            failed(format!(
                "mount succeeded but registry update failed; mount {mount_id} remains in the preparing record: {error}"
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

    fn dock_unmount(
        &self,
        request: &AuthorizedOperation,
    ) -> Result<ExecutionResult, ExecutionError> {
        let _guard = GITDOCK_MOUNT_LOCK
            .lock()
            .map_err(|_| failed("GitDockRun mount registry lock is poisoned"))?;
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
        if current_id != record.container_id || current_id != self.caller_container_id {
            return Err(failed(
                "GitDockRun target container identity changed before unmount",
            ));
        }
        let pid = container_pid_for_mount(&record.container_id).map_err(failed)?;
        let mount_info = match record.kernel_mount_id {
            Some(id) => mountinfo_by_id(pid, id).map_err(failed)?,
            None => mountinfo_by_target(pid, &record.target).map_err(failed)?,
        };
        if let Some(mount_info) = mount_info {
            execute_mount_namespace(pid, None, &mount_info.target, MountAction::Unmount)
                .map_err(failed)?;
            let still_present = match record.kernel_mount_id {
                Some(id) => mountinfo_by_id(pid, id).map_err(failed)?.is_some(),
                None => mountinfo_by_target(pid, &record.target)
                    .map_err(failed)?
                    .is_some(),
            };
            if still_present {
                return Err(failed(
                    "kernel still reports the GitDockRun mount after unmount",
                ));
            }
        }

        registry.mounts.retain(|item| item.id != mount_id);
        registry.save(&self.state_dir).map_err(failed)?;
        Ok(success(format!(
            "GitDockRun UNMOUNT: PASS\nmount_id={mount_id}\ntarget={}\n",
            record.target
        )))
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

#[derive(Debug, Clone, Copy)]
enum MountAction {
    Bind,
    RemountReadOnly,
    Unmount,
}

#[derive(Debug, Clone)]
struct MountInfo {
    id: u32,
    target: String,
}

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

#[cfg(not(target_os = "linux"))]
fn open_mount_source(_source: &Path) -> Result<fs::File, String> {
    Err("dynamic GitDockRun mounts are currently supported only on Linux".into())
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
    Ok(pid)
}

#[cfg(not(target_os = "linux"))]
fn container_pid_for_mount(_container_id: &str) -> Result<i32, String> {
    Err("dynamic GitDockRun mounts are currently supported only on Linux".into())
}

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
    let allowed_prefixes = [
        "/workspace/",
        "/home/runner/actions-runner/_work/",
        "/tmp/gitrun-mounts/",
        "/var/lib/gitrun/shared/",
    ];
    if !allowed_prefixes
        .iter()
        .any(|prefix| target.starts_with(prefix))
    {
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

fn decode_mountinfo_path(path: &str) -> String {
    path.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

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

fn mountinfo_by_target(pid: i32, target: &str) -> Result<Option<MountInfo>, String> {
    Ok(mountinfo_entries(pid)?
        .into_iter()
        .find(|mount| mount.target == target))
}

fn mountinfo_by_id(pid: i32, mount_id: u32) -> Result<Option<MountInfo>, String> {
    Ok(mountinfo_entries(pid)?
        .into_iter()
        .find(|mount| mount.id == mount_id))
}

#[cfg(target_os = "linux")]
fn execute_mount_namespace(
    pid: i32,
    source_fd: Option<i32>,
    target: &str,
    action: MountAction,
) -> Result<(), String> {
    let mount_namespace = fs::File::open(format!("/proc/{pid}/ns/mnt"))
        .map_err(|error| format!("open target mount namespace: {error}"))?;
    let pid_namespace = fs::File::open(format!("/proc/{pid}/ns/pid"))
        .map_err(|error| format!("open target PID namespace: {error}"))?;
    let container_root = open_path_handle(&PathBuf::from(format!("/proc/{pid}/root")), false)?;
    let target_c =
        CString::new(target).map_err(|_| "mount target contains a NUL byte".to_owned())?;
    let source_c = source_fd
        .map(|fd| CString::new(format!("/proc/self/fd/{fd}")))
        .transpose()
        .map_err(|_| "invalid mount source file descriptor".to_owned())?;
    if matches!(action, MountAction::Bind) && source_c.is_none() {
        return Err("mount source descriptor was not provided".into());
    }
    let source_ptr = source_c
        .as_ref()
        .map_or(std::ptr::null(), |value| value.as_ptr());

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
                    || libc::chroot(b".\0".as_ptr().cast()) != 0
                    || libc::chdir(b"/\0".as_ptr().cast()) != 0
                {
                    libc::_exit(44);
                }
                match action {
                    MountAction::Bind => {
                        if libc::mount(
                            source_ptr,
                            target_c.as_ptr(),
                            std::ptr::null(),
                            libc::MS_BIND,
                            std::ptr::null(),
                        ) != 0
                        {
                            libc::_exit(45);
                        }
                        libc::_exit(0);
                    }
                    MountAction::RemountReadOnly => {
                        if libc::mount(
                            std::ptr::null(),
                            target_c.as_ptr(),
                            std::ptr::null(),
                            libc::MS_BIND
                                | libc::MS_REMOUNT
                                | libc::MS_RDONLY
                                | libc::MS_NOSUID
                                | libc::MS_NODEV,
                            std::ptr::null(),
                        ) != 0
                        {
                            libc::_exit(46);
                        }
                        libc::_exit(0);
                    }
                    MountAction::Unmount => {
                        if libc::umount2(target_c.as_ptr(), 0) != 0 {
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
        let stage = match code {
            41 => "enter mount namespace",
            42 => "enter PID namespace",
            43 => "fork inside PID namespace",
            44 => "enter container root",
            45 => "bind source",
            46 => "enforce read-only mount",
            47 => "unmount target",
            48 => "wait for namespace operation",
            _ => "complete namespace operation",
        };
        return Err(format!("failed to {stage} (helper status {code})"));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn execute_mount_namespace(
    _pid: i32,
    _source_fd: Option<i32>,
    _target: &str,
    _action: MountAction,
) -> Result<(), String> {
    Err("dynamic GitDockRun mounts are currently supported only on Linux".into())
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
    let mut registry = MountRegistry::load(state_dir)?;
    let snapshot = registry.mounts.clone();
    let mut remove_ids = Vec::new();

    for record in snapshot
        .iter()
        .filter(|record| record.repository == repository)
    {
        let job = match client.find_workflow_job(repository, record.run_id, &record.job) {
            Ok(Some(job)) => job,
            Ok(None) => {
                remove_ids.push(record.id.clone());
                continue;
            }
            Err(error) => {
                eprintln!(
                    "gitrun-api: cannot verify job {} for mount {}: {error}",
                    record.job, record.id
                );
                continue;
            }
        };
        if job.status.eq_ignore_ascii_case("in_progress") && job.conclusion.is_none() {
            continue;
        }

        match docker::container_id(&record.container) {
            Ok(Some(current_id)) if current_id == record.container_id => {
                let pid = match container_pid_for_mount(&record.container_id) {
                    Ok(pid) => pid,
                    Err(_) => {
                        // A stopped container has already lost its private
                        // mount namespace; a later start gets a fresh namespace.
                        if docker::container_status(&record.container)
                            .ok()
                            .flatten()
                            .is_some()
                        {
                            remove_ids.push(record.id.clone());
                        }
                        continue;
                    }
                };
                let mount = match record.kernel_mount_id {
                    Some(id) => mountinfo_by_id(pid, id)?,
                    None => mountinfo_by_target(pid, &record.target)?,
                };
                if let Some(mount) = mount {
                    if let Err(error) =
                        execute_mount_namespace(pid, None, &mount.target, MountAction::Unmount)
                    {
                        eprintln!(
                            "gitrun-api: could not clean mount {} after job completion: {error}",
                            record.id
                        );
                        continue;
                    }
                }
                remove_ids.push(record.id.clone());
            }
            Ok(None) => remove_ids.push(record.id.clone()),
            Ok(Some(_different_id)) => {
                // Never apply an old record to a replacement container.
                remove_ids.push(record.id.clone());
            }
            Err(error) => {
                eprintln!(
                    "gitrun-api: cannot inspect mount container {}: {error}",
                    record.container
                );
            }
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
