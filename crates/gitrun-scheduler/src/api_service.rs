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
use std::path::{Path, PathBuf, Component};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

const DOCK_ONLY_MARKER: &str = "/home/runner/.gitrun-dock-only";
const API_MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_VAULT_WRITE_BYTES: usize = 1024 * 1024;

pub fn spawn(
    config: Config,
    github: Arc<GitHubClient>,
    stopping: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>, String> {
    #[cfg(unix)]
    {
        let socket_path = std::env::var("GITRUN_API_SOCKET")
            .unwrap_or_else(|_| DEFAULT_SOCKET_PATH.to_owned());
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

        // The socket itself is not the authorization boundary: every request
        // still needs a random runner capability. 0666 is required so the
        // unprivileged runner user inside each bind-mounted container can
        // connect; the random token and container identity checks remain the
        // gate.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666))
            .map_err(|e| format!("set API socket permissions: {e}"))?;

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
    let mut registry =
        DockRegistry::load(state_dir).map_err(|error| format!("load GitDockRun registry: {error}"))?;
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

        let completed = job.status.eq_ignore_ascii_case("completed")
            || job.conclusion.is_some();

        if completed {
            make_dock_only(&binding.container, state_dir)
                .map_err(|error| format!("freeze GitRun Dock container {}: {error}", binding.container))?;
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

pub(crate) fn preserved_dock_containers(
    state_dir: &Path,
    repository: &str,
) -> Result<std::collections::BTreeSet<String>, String> {
    let registry =
        DockRegistry::load(state_dir).map_err(|error| format!("load GitDockRun registry: {error}"))?;
    Ok(registry
        .bindings
        .into_iter()
        .filter(|binding| binding.repository == repository)
        .map(|binding| binding.container)
        .collect())
}

pub(crate) fn is_dock_bound(
    state_dir: &Path,
    repository: &str,
    container: &str,
) -> Result<bool, String> {
    let registry =
        DockRegistry::load(state_dir).map_err(|error| format!("load GitDockRun registry: {error}"))?;
    Ok(registry.bindings.iter().any(|binding| {
        binding.repository == repository && binding.container == container
    }))
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

    let identity = match docker::runner_for_api_token(&request.token) {
        Ok(Some(identity)) => identity,
        Ok(None) => {
            send_error(&mut stream, "runner API authentication failed")?;
            return Ok(());
        }
        Err(error) => {
            send_error(&mut stream, "runner API authentication could not be checked")?;
            return Err(error.to_string());
        }
    };

    if identity.name != request.invocation.runner
        || identity.repository != request.invocation.repository
    {
        send_error(&mut stream, "runner API identity does not match the managed container")?;
        return Ok(());
    }

    if let Some(run_id) = request.invocation.run_id {
        let job = request.invocation.job.clone();
        let current_job = github
            .find_workflow_job(&request.invocation.repository, run_id, &job)
            .map_err(|error| format!("verify current workflow job: {error}"))?;
        let Some(current_job) = current_job else {
            send_error(&mut stream, "current GitHub workflow job could not be verified")?;
            return Ok(());
        };
        if current_job.runner_name.as_deref() != Some(identity.name.as_str()) {
            send_error(
                &mut stream,
                "GitRun API request is not being made by the runner assigned to this workflow job",
            )?;
            return Ok(());
        }
    }

    let caller = VerifiedCaller::new(
        &request.invocation.repository,
        &request.invocation.workflow,
        &request.invocation.job,
        &identity.name,
    );

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
                let policy = GitRunSettings::load_or_default(
                    GitRunSettings::path_for_state_dir(&self.state_dir),
                )
                .map_err(|e| failed(e.to_string()))?
                .effective_for_repository(&request.repository)
                .storage;

                if size > policy.max_file_size_bytes {
                    return Err(failed("GitSaveRun file exceeds repository storage limit"));
                }

                docker_exec_checked(
                    runner,
                    &[
                        "mkdir",
                        "-p",
                        "/var/lib/gitrun/shared/files",
                    ],
                )?;
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

                let mut logs = docker::container_logs(runner)
                    .map_err(|error| failed(error.to_string()))?;
                logs = redact_vault_values(&self.config, &request.repository, &logs);

                let policy = GitRunSettings::load_or_default(
                    GitRunSettings::path_for_state_dir(&self.state_dir),
                )
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
            _ => Err(failed("unsupported GitDockRun operation")),
        }
    }

    fn dock_connect(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let run_id = request_run_id(request);
        if run_id == 0 {
            return Err(failed("GitDockRun --connect requires GITHUB_RUN_ID"));
        }
        let job = arg(request, "job")?;

        let job_info = self
            .github
            .find_workflow_job(&request.repository, run_id, job)
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| failed(format!("workflow job {job} was not found")))?;

        let runner_name = job_info
            .runner_name
            .ok_or_else(|| failed(format!("workflow job {job} has no runner identity")))?;

        let container = docker::container_id(&runner_name)
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| failed(format!("runner container {runner_name} was not found")))?;

        let dynamic = docker::container_is_permanent(&runner_name)
            .map_err(|error| failed(error.to_string()))?
            == false;

        let completed = job_info.status.eq_ignore_ascii_case("completed")
            || job_info.conclusion.is_some();

        if dynamic && docker::container_status(&runner_name)
            .map_err(|error| failed(error.to_string()))?
            .is_none()
        {
            return Err(failed(format!("runner container {runner_name} disappeared during connect")));
        }

        if dynamic && completed {
            make_dock_only(&runner_name, &self.state_dir)?;
        }

        let mut registry = DockRegistry::load(&self.state_dir)?;
        registry.upsert(DockBinding {
            repository: request.repository.clone(),
            run_id,
            job: job.to_owned(),
            container: runner_name.clone(),
            dynamic,
            dock_only: dynamic && completed,
            requester_runner: self.caller.runner.clone(),
            connected_at: now(),
        });
        registry.save(&self.state_dir)?;

        Ok(success(format!(
            "GitDockRun CONNECT: PASS\njob={job}\ncontainer={runner_name}\nid={container}\n"
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

        let mut registry = DockRegistry::load(&self.state_dir)?;
        let binding = registry
            .binding(&request.repository, run_id, job)
            .cloned()
            .ok_or_else(|| failed(format!("no GitDockRun binding exists for job {job}")))?;

        if binding.dynamic {
            let job_info = self
                .github
                .find_workflow_job(&request.repository, run_id, job)
                .map_err(|error| failed(error.to_string()))?
                .ok_or_else(|| failed(format!("workflow job {job} could not be verified")))?;
            let completed = job_info.status.eq_ignore_ascii_case("completed")
                || job_info.conclusion.is_some();
            if !completed {
                return Err(failed(
                    "GitDockRun --disconnect cannot remove a Dynamic container while its source job is running",
                ));
            }
        }

        let binding = registry
            .remove(&request.repository, run_id, job)
            .ok_or_else(|| failed(format!("no GitDockRun binding exists for job {job}")))?;
        registry.save(&self.state_dir)?;

        if binding.dynamic {
            let _ = docker::remove_container(&binding.container);
        } else if docker_container_has_marker(&binding.container)? {
            restore_runner(&binding.container)?;
        }

        Ok(success(format!(
            "GitDockRun DISCONNECT: PASS\njob={job}\ncontainer={}\n"
        )))
    }

    fn dock_read(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let container = request
            .resource
            .as_deref()
            .ok_or_else(|| failed("missing docked container"))?;
        let path = arg(request, "path")?;
        ensure_safe_runner_path(path)?;

        let output = docker::exec_container(container, &["cat", "--", path])
            .map_err(|error| failed(error.to_string()))?;
        command_output(output)
    }

    fn dock_write(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let container = request
            .resource
            .as_deref()
            .ok_or_else(|| failed("missing docked container"))?;
        let path = arg(request, "path")?;
        let value = arg(request, "value")?;
        ensure_safe_runner_path(path)?;

        let output = docker::exec_container_with_stdin(
            container,
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
        let container = request
            .resource
            .as_deref()
            .ok_or_else(|| failed("missing docked container"))?;
        let command = arg(request, "command")?;

        docker::exec_container_stream(
            container,
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

    fn dock_execute(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let container = request
            .resource
            .as_deref()
            .ok_or_else(|| failed("missing docked container"))?;
        let command = arg(request, "command")?;
        run_docker_command(
            container,
            &["sh", "-c", command],
        )
    }

    fn dock_melt(&self, request: &AuthorizedOperation) -> Result<ExecutionResult, ExecutionError> {
        let source = request
            .resource
            .as_deref()
            .ok_or_else(|| failed("missing docked source container"))?;
        let target = match request.arguments.get("target").map(String::as_str) {
            None | Some("runner") => self.caller.runner.as_str(),
            Some(value) => value,
        };

        if source == target {
            return Err(failed("GitDockRun --melt source and target are identical"));
        }

        let source_repo = docker::container_repo_label_on(
            &docker::DockerHost::Local,
            source,
        )
        .map_err(|error| failed(error.to_string()))?;
        if source_repo.is_none() {
            return Err(failed("GitDockRun --melt source is not a GitRun-managed container"));
        }

        let status = docker::container_status(source)
            .map_err(|error| failed(error.to_string()))?
            .unwrap_or_default();
        if status == "running" {
            return Err(failed(
                "GitDockRun --melt requires a stopped or dock-only source container",
            ));
        }

        if docker::container_id(target)
            .map_err(|error| failed(error.to_string()))?
            .is_none()
        {
            return Err(failed(format!("melt target container {target} was not found")));
        }

        let target_repo = docker::container_repo_label_on(&docker::DockerHost::Local, target)
            .map_err(|error| failed(error.to_string()))?;
        if target_repo.as_deref() != Some(request.repository.as_str()) {
            return Err(failed("GitDockRun --melt target is not a GitRun runner for this repository"));
        }

        docker::melt_filesystem(source, target)
            .map_err(|error| failed(error.to_string()))?;
        let _ = docker::remove_container(source);

        let mut registry = DockRegistry::load(&self.state_dir)
            .map_err(|error| failed(error.to_string()))?;
        registry.bindings.retain(|binding| binding.container != source);
        registry
            .save(&self.state_dir)
            .map_err(|error| failed(error.to_string()))?;

        Ok(success(format!(
            "GitDockRun MELT: PASS\nsource={source}\ntarget={target}\n"
        )))
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

fn docker_exec_checked(
    container: &str,
    args: &[&str],
) -> Result<ExecutionResult, ExecutionError> {
    let output = docker::exec_container(container, args)
        .map_err(|error| failed(error.to_string()))?;
    command_output(output)
}

fn run_docker_command(
    container: &str,
    args: &[&str],
) -> Result<ExecutionResult, ExecutionError> {
    let output = docker::exec_container(container, args)
        .map_err(|error| failed(error.to_string()))?;
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
    let output = docker::exec_container(
        container,
        &["stat", "-c", "%s", "--", path],
    )
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
        return Err(failed("path is outside the GitRun workflow filesystem allowlist"));
    }
    Ok(())
}

fn ensure_safe_file_name(name: &str) -> Result<(), ExecutionError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 255
        || name.chars().any(|c| c.is_control() || c == '/' || c == '\\')
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
        return Err(failed("package name/version contains unsupported characters"));
    }
    Ok(())
}

fn redact_vault_values(config: &Config, repository: &str, text: &str) -> String {
    if config.vault_dir.trim().is_empty() {
        return text.to_owned();
    }

    let groups = config.vault_groups_for_repo(repository);
    let vault = match Vault::open(&config.vault_dir) {
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

fn make_dock_only(
    container: &str,
    state_dir: &Path,
) -> Result<(), ExecutionError> {
    match docker::container_status(container).map_err(|error| failed(error.to_string()))? {
        Some(status) if status == "running" => {
            docker_exec_checked(
                container,
                &["touch", "--", DOCK_ONLY_MARKER],
            )?;
            docker::stop_container(container)
                .map_err(|error| failed(error.to_string()))?;
        }
        Some(_) => {
            let marker = state_dir.join(format!(
                ".dock-marker-{}-{}",
                std::process::id(),
                now()
            ));
            fs::write(&marker, b"GitDockRun\n")
                .map_err(|error| failed(error.to_string()))?;
            docker::copy_to_container(container, &marker, DOCK_ONLY_MARKER)
                .map_err(|error| failed(error.to_string()))?;
            let _ = fs::remove_file(&marker);
        }
        None => return Err(failed("dock source container disappeared")),
    }

    docker::start_container(container).map_err(|error| failed(error.to_string()))?;
    Ok(())
}

fn request_has_marker(container: &str) -> Result<bool, ExecutionError> {
    docker_container_has_marker(container)
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
