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
    let token = required_env("GITRUN_API_TOKEN")?;

    let request = WireRequest {
        token,
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

    println!("GITRUN {}", api.as_str());

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
        for line in reader.lines() {
            let line = line.map_err(|error| format!("cannot read API response: {error}"))?;
            let response: WireResponse = serde_json::from_str(&line)
                .map_err(|error| format!("invalid GitRun API response: {error}"))?;
            match response {
                WireResponse::Accepted => println!("GSR VALIDATE: PASS"),
                WireResponse::Error { message } => return Err(message),
                WireResponse::Event(ExecutionEvent::Started) => {
                    println!("GITEXECRUN: START")
                }
                WireResponse::Event(ExecutionEvent::Stdout(value)) => print!("{value}"),
                WireResponse::Event(ExecutionEvent::Stderr(value)) => eprint!("{value}"),
                WireResponse::Event(ExecutionEvent::Finished { exit_code }) => {
                    println!("GITEXECRUN RESULT: {exit_code}");
                    if exit_code != 0 {
                        return Err(format!(
                            "GitRun API operation failed with exit code {exit_code}"
                        ));
                    }
                    println!("GITEXECRUN EXEC DONE");
                    break;
                }
            }
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

fn parse_api(
    api: GitRunApi,
    args: &[String],
) -> Result<(GitRunOperation, Option<String>, BTreeMap<String, String>), String> {
    let mut values = BTreeMap::new();
    let mut resource = None;
    let mut operation = None;
    let mut i = 0usize;

    if api == GitRunApi::GitInstallRun && !args.is_empty() && !args[0].starts_with('-') {
        values.insert("package".into(), args[0].clone());
        if let Some(version) = args.get(1) {
            values.insert("version".into(), version.clone());
        }
        return Ok((GitRunOperation::Install, None, values));
    }

    while i < args.len() {
        match args[i].as_str() {
            "--read" | "--get" => {
                operation = Some(GitRunOperation::Read);
                i += 1;
                if i < args.len() {
                    let key = if api == GitRunApi::GitVaultRun {
                        "name"
                    } else {
                        "path"
                    };
                    values.insert(key.into(), args[i].clone());
                    i += 1;
                }
            }
            "--write" => {
                operation = Some(GitRunOperation::Write);
                i += 1;
                if i < args.len() {
                    let key = if api == GitRunApi::GitVaultRun {
                        "name"
                    } else {
                        "path"
                    };
                    values.insert(key.into(), args[i].clone());
                    i += 1;
                }
                if i < args.len() {
                    values.insert("value".into(), args[i].clone());
                    i += 1;
                }
            }
            "--exists" => {
                operation = Some(GitRunOperation::Exists);
                i += 1;
                if i < args.len() {
                    values.insert("name".into(), args[i].clone());
                    i += 1;
                }
            }
            "--delete" => {
                operation = Some(GitRunOperation::Delete);
                i += 1;
                if i < args.len() {
                    values.insert("name".into(), args[i].clone());
                    i += 1;
                }
            }
            "--list" => {
                operation = Some(GitRunOperation::List);
                i += 1;
            }
            "--connect" => {
                operation = Some(GitRunOperation::Connect);
                i += 1;
            }
            "--disconnect" => {
                operation = Some(GitRunOperation::Disconnect);
                i += 1;
            }
            "--execute" => {
                operation = Some(GitRunOperation::Execute);
                i += 1;
                if i < args.len() {
                    values.insert("command".into(), args[i..].join(" "));
                }
                i = args.len();
            }
            "--melt" => {
                operation = Some(GitRunOperation::Melt);
                i += 1;
                if i < args.len() && !args[i].starts_with('-') {
                    values.insert("target".into(), args[i].clone());
                    i += 1;
                }
            }
            "--file" => {
                operation = Some(GitRunOperation::File);
                i += 1;
                if i < args.len() {
                    values.insert("path".into(), args[i].clone());
                    i += 1;
                }
            }
            "--logs" => {
                operation = Some(GitRunOperation::Logs);
                i += 1;
            }
            "--namefile" => {
                i += 1;
                if i < args.len() {
                    values.insert("namefile".into(), args[i].clone());
                    i += 1;
                }
            }
            "--name" => {
                i += 1;
                if i < args.len() {
                    values.insert("name".into(), args[i].clone());
                    i += 1;
                }
            }
            "--entry" => {
                i += 1;
                if i < args.len() {
                    values.insert("entry".into(), args[i].clone());
                    i += 1;
                }
            }
            "--permanent" => {
                values.insert("permanent".into(), "true".into());
                i += 1;
            }
            "--remove" => {
                operation = Some(GitRunOperation::Remove);
                i += 1;
                if i < args.len() {
                    values.insert("package".into(), args[i].clone());
                    i += 1;
                }
            }
            "--update" => {
                operation = Some(GitRunOperation::Update);
                i += 1;
                if i < args.len() {
                    values.insert("package".into(), args[i].clone());
                    i += 1;
                }
            }
            "--docked" => {
                i += 1;
                if i >= args.len() {
                    return Err("--docked requires a container id".into());
                }
                resource = Some(args[i].clone());
                i += 1;
            }
            "--job" => {
                i += 1;
                if i >= args.len() {
                    return Err("--job requires a job name".into());
                }
                values.insert("job".into(), args[i].clone());
                i += 1;
            }
            value => return Err(format!("unknown argument {value}")),
        }
    }

    if api == GitRunApi::GitRegisterRun {
        if !values.contains_key("name") || !values.contains_key("entry") {
            return Err("GitRegisterRun requires --name <name> --entry <path>".into());
        }
        operation = Some(GitRunOperation::Register);
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

    if api == GitRunApi::GitDockRun
        && matches!(
            operation,
            GitRunOperation::Read
                | GitRunOperation::Write
                | GitRunOperation::Execute
                | GitRunOperation::Melt
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
