//! Local IPC for the persistent, system-managed GitRun control service.
//!
//! The GUI and terminal UI remain unprivileged. The service exposes only the
//! typed operations already implemented by the dashboard backend; it does not
//! provide a shell, arbitrary executable launch, or arbitrary file access.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

const SOCKET_PATH: &str = "/run/gitrun-control/manager.sock";
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    operation: String,
    action: String,
    #[serde(default)]
    args: Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct Reply {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Reply {
    fn success(result: Value) -> Self {
        Self {
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    fn failure(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            result: None,
            error: Some(error.into()),
        }
    }
}

/// Whether this installation is expected to route protected control operations
/// through the persistent service instead of invoking pkexec from the UI.
pub fn enabled() -> bool {
    cfg!(target_os = "linux")
        && Path::new("/etc/gitrun/gitrun.env").is_file()
        && super::trusted_system_cli_path().is_some()
}

/// Send one bounded, structured request to the system control service.
pub fn request(operation: &str, action: &str, args: Value) -> Result<Value, String> {
    if !matches!(operation, "read" | "write") {
        return Err("unsupported control-manager operation".into());
    }
    if action.is_empty() || action.len() > 80 || action.chars().any(char::is_control) {
        return Err("invalid control-manager action".into());
    }

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let mut stream = UnixStream::connect(SOCKET_PATH).map_err(|error| {
            format!(
                "GitRun Control Manager is unavailable at {SOCKET_PATH}: {error}. Check that gitrun-control.service is installed and active."
            )
        })?;
        stream
            .set_read_timeout(Some(Duration::from_secs(180)))
            .map_err(|error| format!("unable to set control-manager read timeout: {error}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|error| format!("unable to set control-manager write timeout: {error}"))?;

        let request = Request {
            operation: operation.to_owned(),
            action: action.to_owned(),
            args,
        };
        let mut bytes = serde_json::to_vec(&request)
            .map_err(|error| format!("encode control-manager request: {error}"))?;
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err("control-manager request exceeds the 1 MiB limit".into());
        }
        bytes.push(b'\n');
        stream
            .write_all(&bytes)
            .map_err(|error| format!("send control-manager request: {error}"))?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let read = reader
            .by_ref()
            .take((MAX_MESSAGE_BYTES + 1) as u64)
            .read_line(&mut line)
            .map_err(|error| format!("read control-manager response: {error}"))?;
        if read == 0 || read > MAX_MESSAGE_BYTES {
            return Err("invalid or oversized control-manager response".into());
        }
        let reply: Reply = serde_json::from_str(&line)
            .map_err(|error| format!("decode control-manager response: {error}"))?;
        if reply.ok {
            reply
                .result
                .ok_or_else(|| "control-manager returned no result".to_owned())
        } else {
            Err(reply
                .error
                .unwrap_or_else(|| "control-manager operation failed".into()))
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (operation, action, args);
        Err("the persistent GitRun Control Manager is currently Linux-only".into())
    }
}

/// Entry point for the internal control-manager command, normally launched by systemd.
pub fn serve() -> i32 {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
        use std::os::unix::net::UnixListener;

        if !is_root() {
            eprintln!("gitrun control-manager: refusing to run without root privileges");
            return 77;
        }

        let directory = Path::new("/run/gitrun-control");
        let directory_meta = match std::fs::symlink_metadata(directory) {
            Ok(metadata) => metadata,
            Err(error) => {
                eprintln!(
                    "gitrun control-manager: systemd runtime directory is unavailable: {error}"
                );
                return 1;
            }
        };
        if directory_meta.file_type().is_symlink()
            || !directory_meta.is_dir()
            || directory_meta.uid() != 0
            || directory_meta.permissions().mode() & 0o022 != 0
        {
            eprintln!("gitrun control-manager: refusing an unsafe runtime directory");
            return 1;
        }

        let socket = Path::new(SOCKET_PATH);
        if let Ok(metadata) = std::fs::symlink_metadata(socket) {
            if !metadata.file_type().is_socket() || metadata.uid() != 0 {
                eprintln!("gitrun control-manager: refusing to replace an untrusted socket path");
                return 1;
            }
            if let Err(error) = std::fs::remove_file(socket) {
                eprintln!("gitrun control-manager: cannot remove stale socket: {error}");
                return 1;
            }
        }

        let listener = match UnixListener::bind(socket) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("gitrun control-manager: cannot bind local socket: {error}");
                return 1;
            }
        };
        if let Err(error) = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o660))
        {
            eprintln!("gitrun control-manager: cannot secure socket permissions: {error}");
            let _ = std::fs::remove_file(socket);
            return 1;
        }
        let socket_meta = match std::fs::symlink_metadata(socket) {
            Ok(metadata) => metadata,
            Err(error) => {
                eprintln!("gitrun control-manager: cannot verify local socket: {error}");
                return 1;
            }
        };
        if !socket_meta.file_type().is_socket()
            || socket_meta.uid() != 0
            || socket_meta.permissions().mode() & 0o777 != 0o660
        {
            eprintln!("gitrun control-manager: socket ownership or mode verification failed");
            let _ = std::fs::remove_file(socket);
            return 1;
        }

        eprintln!("gitrun control-manager: listening on {SOCKET_PATH}");
        for incoming in listener.incoming() {
            match incoming {
                Ok(stream) => {
                    let _ = std::thread::spawn(move || serve_client(stream));
                }
                Err(error) => {
                    eprintln!("gitrun control-manager: failed to accept local connection: {error}");
                }
            }
        }
        let _ = std::fs::remove_file(socket);
        0
    }

    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("gitrun control-manager is supported only on Linux");
        2
    }
}

#[cfg(target_os = "linux")]
fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                let values = line.strip_prefix("Uid:")?;
                values.split_whitespace().nth(1)?.parse::<u32>().ok()
            })
        })
        .is_some_and(|uid| uid == 0)
}

#[cfg(target_os = "linux")]
fn serve_client(mut stream: std::os::unix::net::UnixStream) {
    let reply = (|| -> Result<Reply, String> {
        let mut line = String::new();
        let read = BufReader::new(stream.try_clone().map_err(|error| error.to_string())?)
            .take((MAX_MESSAGE_BYTES + 1) as u64)
            .read_line(&mut line)
            .map_err(|error| format!("read request: {error}"))?;
        if read == 0 || read > MAX_MESSAGE_BYTES {
            return Err("request was empty or exceeded the 1 MiB limit".into());
        }
        let request: Request = serde_json::from_str(&line)
            .map_err(|error| format!("invalid control-manager request: {error}"))?;
        if !matches!(request.operation.as_str(), "read" | "write") {
            return Err("unsupported control-manager operation".into());
        }
        if request.action.is_empty()
            || request.action.len() > 80
            || request.action.chars().any(char::is_control)
        {
            return Err("invalid control-manager action".into());
        }

        let result = match request.operation.as_str() {
            "read" => super::management_tui_read(&request.action, request.args),
            "write" => {
                if needs_confirmation(&request.action, &request.args)
                    && !request
                        .args
                        .get("confirmed")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                {
                    return Err(format!(
                        "operation {} requires explicit confirmation",
                        request.action
                    ));
                }
                if request.action == "maintenance_task" {
                    schedule_maintenance_task(
                        request.args
                            .get("task")
                            .and_then(Value::as_str)
                            .ok_or("maintenance task name is required")?,
                    )
                } else {
                    super::management_tui_write(&request.action, request.args)
                }
            }
            _ => unreachable!("operation was validated above"),
        };
        result.map(Reply::success).map_err(Reply::failure)
    })()
    .unwrap_or_else(Reply::failure);

    match serde_json::to_vec(&reply) {
        Ok(mut bytes) => {
            bytes.push(b'\n');
            if let Err(error) = stream.write_all(&bytes) {
                eprintln!("gitrun control-manager: failed to return response: {error}");
            }
        }
        Err(error) => {
            eprintln!("gitrun control-manager: failed to encode response: {error}");
        }
    }
}

/// Start one fixed, allowlisted maintenance command in a transient system
/// service. This deliberately does not forward caller-provided arguments,
/// paths, or environment variables to a root process.
#[cfg(target_os = "linux")]
fn schedule_maintenance_task(task: &str) -> Result<Value, String> {
    use std::process::Command;

    let (arguments, label): (&[&str], &str) = match task {
        "update" => (&["--no-cat", "update"], "signed GitRun update"),
        "update_containers" => (
            &["--no-cat", "update", "--only-containers"],
            "permanent runner update",
        ),
        "reinstall_latest" => (
            &["--no-cat", "--reinstall-latest-root"],
            "verified GitRun reinstall",
        ),
        "reinstall_saved" => (
            &["--no-cat", "--reinstall-existing-root"],
            "GitRun runtime rebuild",
        ),
        "uninstall" => (&["--no-cat", "--uninstall-root"], "GitRun uninstall"),
        _ => return Err("unsupported Control Manager maintenance task".into()),
    };

    let active = Command::new("/usr/bin/systemctl")
        .args(["is-active", "--quiet", "gitrun-control-maintenance.service"])
        .status()
        .map_err(|error| format!("unable to inspect maintenance task state: {error}"))?;
    if active.success() {
        return Err(
            "another GitRun Control Manager maintenance task is already running".into(),
        );
    }

    // reset-failed clears a previous failed transient unit; it does not change
    // any installed service or retry an earlier command.
    let _ = Command::new("/usr/bin/systemctl")
        .args(["reset-failed", "gitrun-control-maintenance.service"])
        .status();

    let executable = super::trusted_system_cli_path()
        .ok_or("trusted system GitRun CLI was not found; refusing maintenance task")?;
    let output = Command::new("/usr/bin/systemd-run")
        .args([
            "--system",
            "--unit=gitrun-control-maintenance",
            "--collect",
            "--no-block",
            "--property=Type=oneshot",
            "--property=TimeoutStartSec=0",
            "--description",
        ])
        .arg(format!("GitRun Control Manager: {label}"))
        .arg(executable)
        .args(arguments)
        .output()
        .map_err(|error| format!("unable to schedule {label}: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if detail.is_empty() {
            format!("systemd could not schedule {label} (status {})", output.status)
        } else {
            format!("systemd could not schedule {label}: {detail}")
        });
    }

    Ok(serde_json::json!({
        "scheduled": true,
        "task": task,
        "label": label,
        "unit": "gitrun-control-maintenance.service",
        "message": format!("{label} was handed to systemd. Follow its journal for progress and final status.")
    }))
}

#[cfg(not(target_os = "linux"))]
fn schedule_maintenance_task(_task: &str) -> Result<Value, String> {
    Err("GitRun Control Manager maintenance tasks are Linux-only".into())
}

#[cfg(target_os = "linux")]
fn needs_confirmation(action: &str, args: &Value) -> bool {
    match action {
        "vault_delete" | "vm_delete" | "zizmor_install" | "zizmor_disable" => true,
        "gitrun_settings_save" => {
            args.get("confirm_socket_opt_out").and_then(Value::as_bool) == Some(true)
        }
        "maintenance_task" => true,
        "service_control" => matches!(
            args.get("action").and_then(Value::as_str),
            Some("stop" | "restart")
        ),
        "fast_stop" => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_tasks_are_allowlisted() {
        assert!(schedule_maintenance_task("arbitrary-command").is_err());
    }

    #[test]
    fn request_schema_rejects_unknown_fields() {
        let raw = r#"{"operation":"read","action":"config","args":{},"executable":"/bin/sh"}"#;
        assert!(serde_json::from_str::<Request>(raw).is_err());
    }

    #[test]
    fn confirmation_gate_covers_destructive_operations() {
        assert!(needs_confirmation("vault_delete", &serde_json::json!({})));
        assert!(needs_confirmation("vm_delete", &serde_json::json!({})));
        assert!(needs_confirmation("fast_stop", &serde_json::json!({})));
        assert!(needs_confirmation(
            "service_control",
            &serde_json::json!({"action":"restart"})
        ));
        assert!(!needs_confirmation(
            "service_control",
            &serde_json::json!({"action":"start"})
        ));
    }

    #[test]
    fn privileged_socket_policy_change_requires_confirmation() {
        assert!(needs_confirmation(
            "gitrun_settings_save",
            &serde_json::json!({"confirm_socket_opt_out":true})
        ));
        assert!(!needs_confirmation(
            "gitrun_settings_save",
            &serde_json::json!({"confirm_socket_opt_out":false})
        ));
    }
}
