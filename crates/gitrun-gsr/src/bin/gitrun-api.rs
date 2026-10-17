//! Workflow-facing Git*Run command launcher.
//!
//! This is only a transport client. It never performs the requested
//! privileged operation itself. It translates the closed CLI grammar into
//! an ApiInvocation and sends it to the host-side GitRun service, where GSR
//! authorizes the request before gitrun-exe sees it.

use gitrun_core::{GitRunApi, GitRunOperation};
use gitrun_exe::{
    ipc::{WireRequest, WireResponse, DEFAULT_SOCKET_PATH},
    ApiInvocation, ExecutionEvent,
};
use std::collections::BTreeMap;
use std::env;
use std::io::{BufRead, BufReader, Write};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

fn main() {
    match run() {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("GitRun API: FAIL — {error}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), String> {
    let invocation_name = env::args().next().unwrap_or_else(|| "GitStatusRun".into());
    let api = api_from_invocation(&invocation_name)?;
    let args: Vec<String> = env::args().skip(1).collect();
    let (operation, resource, arguments) = parse_api(api, &args)?;

    let repository = required_env("GITHUB_REPOSITORY")?;
    let workflow = env::var("GITHUB_WORKFLOW").unwrap_or_else(|_| "unknown".into());
    let run_id = env::var("GITHUB_RUN_ID")
        .ok()
        .and_then(|value| value.parse().ok());
    let job = env::var("GITHUB_JOB").unwrap_or_else(|_| "unknown".into());
    let runner = required_env("RUNNER_NAME")?;

    let request = WireRequest {
        invocation: ApiInvocation {
            api,
            operation,
            repository,
            workflow,
            run_id,
            job,
            runner,
            resource,
            arguments,
        },
    };

    let secret_read = api == GitRunApi::GitVaultRun && operation == GitRunOperation::Read;
    if !secret_read {
        println!("GITRUN {}", api.as_str());
    }

    #[cfg(unix)]
    {
        let socket_path =
            env::var("GITRUN_API_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET_PATH.into());
        let mut stream = UnixStream::connect(&socket_path).map_err(|error| {
            format!("cannot connect to GitRun API service at {socket_path}: {error}")
        })?;
        let encoded = serde_json::to_string(&request)
            .map_err(|error| format!("cannot encode API request: {error}"))?;
        stream
            .write_all(encoded.as_bytes())
            .map_err(|error| format!("cannot send API request: {error}"))?;
        stream
            .write_all(b"\n")
            .map_err(|error| format!("cannot finish API request: {error}"))?;
        stream
            .flush()
            .map_err(|error| format!("cannot flush API request: {error}"))?;

        let reader = BufReader::new(stream);
        let mut finished = false;
        for line in reader.lines() {
            let line = line.map_err(|error| format!("cannot read API response: {error}"))?;
            let response: WireResponse = serde_json::from_str(&line)
                .map_err(|error| format!("invalid GitRun API response: {error}"))?;
            match response {
                WireResponse::Accepted => {
                    if secret_read {
                        eprintln!("GSR VALIDATE: PASS");
                    } else {
                        println!("GSR VALIDATE: PASS");
                    }
                }
                WireResponse::Error { message } => return Err(message),
                WireResponse::Event(ExecutionEvent::Started) => {
                    println!("GITEXECRUN: START")
                }
                WireResponse::Event(ExecutionEvent::Stdout(value)) => print!("{value}"),
                WireResponse::Event(ExecutionEvent::Stderr(value)) => eprint!("{value}"),
                WireResponse::Event(ExecutionEvent::Finished { exit_code }) => {
                    finished = true;
                    if secret_read {
                        eprintln!("GITEXECRUN RESULT: {exit_code}");
                    } else {
                        println!("GITEXECRUN RESULT: {exit_code}");
                    }
                    if exit_code != 0 {
                        return Err(format!(
                            "GitRun API operation failed with exit code {exit_code}"
                        ));
                    }
                    if !secret_read {
                        println!("GITEXECRUN EXEC DONE");
                    }
                    break;
                }
            }
        }
        if !finished {
            return Err("GitRun API connection closed before the Finished event".into());
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = request;
        Err("GitRun API IPC is not implemented on this platform yet".into())
    }
}

fn required_env(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("required environment variable {name} is missing"))
}

fn api_from_invocation(invocation: &str) -> Result<GitRunApi, String> {
    let name = std::path::Path::new(invocation)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(invocation);

    match name {
        "GitVaultRun" => Ok(GitRunApi::GitVaultRun),
        "GitDockRun" => Ok(GitRunApi::GitDockRun),
        "GitSaveRun" => Ok(GitRunApi::GitSaveRun),
        "GitRegisterRun" => Ok(GitRunApi::GitRegisterRun),
        "GitInstallRun" => Ok(GitRunApi::GitInstallRun),
        "GitReadRun" => Ok(GitRunApi::GitReadRun),
        "GitWriteRun" => Ok(GitRunApi::GitWriteRun),
        "GitVerifyRun" => Ok(GitRunApi::GitVerifyRun),
        "GitStatusRun" => Ok(GitRunApi::GitStatusRun),
        other => Err(format!("unknown GitRun API command {other}")),
    }
}

fn insert_argument(
    values: &mut BTreeMap<String, String>,
    key: &str,
    value: String,
) -> Result<(), String> {
    if values.contains_key(key) {
        return Err(format!("argument {key} may only be specified once"));
    }
    values.insert(key.to_owned(), value);
    Ok(())
}

fn select_operation(
    selected: &mut Option<GitRunOperation>,
    next: GitRunOperation,
) -> Result<(), String> {
    if selected.is_some() {
        return Err("only one GitRun API operation may be specified".into());
    }
    *selected = Some(next);
    Ok(())
}

type ParsedApiRequest = (GitRunOperation, Option<String>, BTreeMap<String, String>);

fn parse_api(api: GitRunApi, args: &[String]) -> Result<ParsedApiRequest, String> {
    let mut values = BTreeMap::new();
    let mut resource = None;
    let mut operation = None;
    let mut i = 0usize;

    if api == GitRunApi::GitInstallRun && !args.is_empty() && !args[0].starts_with('-') {
        if args.len() > 2 {
            return Err("GitInstallRun accepts only <package> [version]".into());
        }
        if args[0].trim().is_empty() {
            return Err("GitInstallRun package cannot be empty".into());
        }
        insert_argument(&mut values, "package", args[0].clone())?;
        if let Some(version) = args.get(1) {
            if version.trim().is_empty() {
                return Err("GitInstallRun version cannot be empty".into());
            }
            insert_argument(&mut values, "version", version.clone())?;
        }
        return Ok((GitRunOperation::Install, None, values));
    }

    while i < args.len() {
        match args[i].as_str() {
            "--read" | "--get" => {
                select_operation(&mut operation, GitRunOperation::Read)?;
                i += 1;
                if i < args.len() {
                    let key = if api == GitRunApi::GitVaultRun {
                        "name"
                    } else {
                        "path"
                    };
                    insert_argument(&mut values, key, args[i].clone())?;
                    i += 1;
                }
            }
            "--write" => {
                select_operation(&mut operation, GitRunOperation::Write)?;
                i += 1;
                if i < args.len() {
                    let key = if api == GitRunApi::GitVaultRun {
                        "name"
                    } else {
                        "path"
                    };
                    insert_argument(&mut values, key, args[i].clone())?;
                    i += 1;
                }
                if i < args.len() {
                    insert_argument(&mut values, "value", args[i].clone())?;
                    i += 1;
                }
            }
            "--exists" => {
                select_operation(&mut operation, GitRunOperation::Exists)?;
                i += 1;
                if i < args.len() {
                    insert_argument(&mut values, "name", args[i].clone())?;
                    i += 1;
                }
            }
            "--delete" => {
                select_operation(&mut operation, GitRunOperation::Delete)?;
                i += 1;
                if i < args.len() {
                    insert_argument(&mut values, "name", args[i].clone())?;
                    i += 1;
                }
            }
            "--list" => {
                select_operation(&mut operation, GitRunOperation::List)?;
                i += 1;
            }
            "--connect" => {
                select_operation(&mut operation, GitRunOperation::Connect)?;
                i += 1;
            }
            "--disconnect" => {
                select_operation(&mut operation, GitRunOperation::Disconnect)?;
                i += 1;
            }
            "--execute" => {
                select_operation(&mut operation, GitRunOperation::Execute)?;
                i += 1;
                if i < args.len() {
                    insert_argument(&mut values, "command", args[i..].join(" "))?;
                }
                i = args.len();
            }
            "--melt" => {
                select_operation(&mut operation, GitRunOperation::Melt)?;
                i += 1;
                if i < args.len() && !args[i].starts_with('-') {
                    insert_argument(&mut values, "target", args[i].clone())?;
                    i += 1;
                }
            }
            "--mount" => {
                select_operation(&mut operation, GitRunOperation::Mount)?;
                i += 1;
                if i < args.len() {
                    insert_argument(&mut values, "source", args[i].clone())?;
                    i += 1;
                }
            }
            "--target" => {
                i += 1;
                if i < args.len() {
                    insert_argument(&mut values, "target", args[i].clone())?;
                    i += 1;
                }
            }
            "--read-only" => {
                if values.contains_key("read_only") {
                    return Err("--read-only/--read-write may be specified only once".into());
                }
                values.insert("read_only".into(), "true".into());
                i += 1;
            }
            "--read-write" => {
                if values.contains_key("read_only") {
                    return Err("--read-only/--read-write may be specified only once".into());
                }
                values.insert("read_only".into(), "false".into());
                i += 1;
            }
            "--unmount" => {
                select_operation(&mut operation, GitRunOperation::Unmount)?;
                i += 1;
                if i < args.len() {
                    insert_argument(&mut values, "mount_id", args[i].clone())?;
                    i += 1;
                }
            }
            "--file" => {
                select_operation(&mut operation, GitRunOperation::File)?;
                i += 1;
                if i < args.len() {
                    insert_argument(&mut values, "path", args[i].clone())?;
                    i += 1;
                }
            }
            "--logs" => {
                select_operation(&mut operation, GitRunOperation::Logs)?;
                i += 1;
            }
            "--namefile" => {
                i += 1;
                if i < args.len() {
                    insert_argument(&mut values, "namefile", args[i].clone())?;
                    i += 1;
                }
            }
            "--name" => {
                i += 1;
                if i < args.len() {
                    insert_argument(&mut values, "name", args[i].clone())?;
                    i += 1;
                }
            }
            "--entry" => {
                i += 1;
                if i < args.len() {
                    insert_argument(&mut values, "entry", args[i].clone())?;
                    i += 1;
                }
            }
            "--permanent" => {
                insert_argument(&mut values, "permanent", "true".into())?;
                i += 1;
            }
            "--remove" => {
                select_operation(&mut operation, GitRunOperation::Remove)?;
                i += 1;
                if i < args.len() {
                    insert_argument(&mut values, "package", args[i].clone())?;
                    i += 1;
                }
            }
            "--update" => {
                select_operation(&mut operation, GitRunOperation::Update)?;
                i += 1;
                if i < args.len() {
                    insert_argument(&mut values, "package", args[i].clone())?;
                    i += 1;
                }
            }
            "--docked" => {
                i += 1;
                if i >= args.len() {
                    return Err("--docked requires a container id".into());
                }
                if resource.is_some() {
                    return Err("--docked may be specified only once".into());
                }
                resource = Some(args[i].clone());
                i += 1;
            }
            "--job" => {
                i += 1;
                if i >= args.len() {
                    return Err("--job requires a job name".into());
                }
                insert_argument(&mut values, "job", args[i].clone())?;
                i += 1;
            }
            value => return Err(format!("unknown argument {value}")),
        }
    }

    if api == GitRunApi::GitRegisterRun {
        if !values.contains_key("name") || !values.contains_key("entry") {
            return Err("GitRegisterRun requires --name <name> --entry <path>".into());
        }
        select_operation(&mut operation, GitRunOperation::Register)?;
        values
            .entry("permanent".into())
            .or_insert_with(|| "false".into());
    }

    if api == GitRunApi::GitSaveRun
        && operation == Some(GitRunOperation::Logs)
        && !values.contains_key("namefile")
    {
        return Err("--logs requires --namefile <file>".into());
    }

    let operation = operation.ok_or_else(|| "no GitRun API operation was specified".to_owned())?;

    if api == GitRunApi::GitDockRun && operation == GitRunOperation::Mount {
        values
            .entry("read_only".into())
            .or_insert_with(|| "true".into());
        if !values.contains_key("target") {
            return Err("--mount requires --target <container-path>".into());
        }
    }

    if api == GitRunApi::GitDockRun
        && matches!(
            operation,
            GitRunOperation::Read
                | GitRunOperation::Write
                | GitRunOperation::Execute
                | GitRunOperation::Melt
                | GitRunOperation::Mount
                | GitRunOperation::Unmount
        )
        && resource.is_none()
    {
        return Err("this GitDockRun operation requires --docked <id>".into());
    }

    if api == GitRunApi::GitDockRun
        && matches!(
            operation,
            GitRunOperation::Connect | GitRunOperation::Disconnect
        )
        && resource.is_some()
    {
        return Err("--docked cannot be combined with GitDockRun --connect/--disconnect".into());
    }

    Ok((operation, resource, values))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_rejects_multiple_operations() {
        assert!(parse_api(
            GitRunApi::GitVaultRun,
            &[
                "--read".into(),
                "first".into(),
                "--delete".into(),
                "second".into()
            ]
        )
        .is_err());
    }

    #[test]
    fn parser_rejects_conflicting_mount_modes_and_duplicate_resources() {
        assert!(parse_api(
            GitRunApi::GitDockRun,
            &[
                "--mount".into(),
                "/source".into(),
                "--target".into(),
                "/target".into(),
                "--docked".into(),
                "runner-1".into(),
                "--read-only".into(),
                "--read-write".into(),
            ]
        )
        .is_err());
        assert!(parse_api(
            GitRunApi::GitDockRun,
            &[
                "--mount".into(),
                "/source".into(),
                "--target".into(),
                "/target".into(),
                "--docked".into(),
                "runner-1".into(),
                "--docked".into(),
                "runner-2".into(),
            ]
        )
        .is_err());
    }

    #[test]
    fn install_shortcut_rejects_ignored_extra_arguments() {
        assert!(parse_api(
            GitRunApi::GitInstallRun,
            &["package".into(), "1.0".into(), "unexpected".into()]
        )
        .is_err());
    }
}
