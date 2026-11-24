//! Native Linux client-side log export helper for \`gitrun manage\`.
//!
//! This module replaces the former Python helper. It deliberately uses only
//! the standard library: the remote protocol is bounded, authenticated by a
//! one-use token, and transported through an SSH reverse tunnel bound to
//! loopback on both machines.

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        env,
        fs::{self, OpenOptions},
        io::{self, Read, Write},
        net::{TcpListener, TcpStream},
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    const MAX_LOG_BYTES: usize = 32 * 1024 * 1024;
    const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(600);
    const TOKEN_PREFIX: &str = "/tmp/GitRun-logs-token-";

    fn random_hex(bytes: usize) -> io::Result<String> {
        let mut data = vec![0_u8; bytes];
        #[cfg(unix)]
        {
            let mut random = fs::File::open("/dev/urandom")?;
            random.read_exact(&mut data)?;
        }
        #[cfg(not(unix))]
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure random source unavailable",
            ));
        }
        Ok(data.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    fn valid_target(target: &str) -> bool {
        !target.is_empty()
            && !target.starts_with('-')
            && target.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'@' | b':' | b'-')
            })
    }

    fn remote_token(ssh: &str, target: &str, token: &str, path: &str) -> io::Result<()> {
        let command = format!("umask 077; cat > {path} && chmod 600 {path}");
        let status = Command::new(ssh)
            .args(["-T", target, &command])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .and_then(|mut child| {
                child
                    .stdin
                    .take()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "SSH stdin unavailable")
                    })?
                    .write_all(format!("{token}\n").as_bytes())?;
                child.wait()
            })?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "could not create private SSH token file (exit {status})"
            )));
        }
        Ok(())
    }

    fn read_line(stream: &mut TcpStream, limit: usize) -> io::Result<String> {
        let mut bytes = Vec::with_capacity(64);
        loop {
            let mut byte = [0_u8; 1];
            stream.read_exact(&mut byte)?;
            match byte[0] {
                b'\n' => break,
                b'\r' => {}
                value if bytes.len() < limit => bytes.push(value),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "protocol line is too long",
                    ))
                }
            }
        }
        String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    fn write_line(stream: &mut TcpStream, line: &str) -> io::Result<()> {
        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\n")
    }

    fn read_payload(stream: &mut TcpStream, header: &str) -> io::Result<Vec<u8>> {
        let Some(length_text) = header.strip_prefix("LOGS ") else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid log payload header",
            ));
        };
        if length_text.is_empty()
            || length_text.len() > 8
            || !length_text.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid log payload length",
            ));
        }
        let length = length_text.parse::<usize>().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid log payload length")
        })?;
        if length > MAX_LOG_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "log payload exceeds 32 MiB",
            ));
        }
        let mut data = vec![0_u8; length];
        stream.read_exact(&mut data)?;
        Ok(data)
    }

    fn desktop_available() -> bool {
        env::var_os("DISPLAY").is_some() || env::var_os("WAYLAND_DISPLAY").is_some()
    }

    fn clipboard(payload: &[u8]) -> Result<&'static str, String> {
        let mut candidates: Vec<(&str, Vec<&str>)> = Vec::new();
        if env::var_os("WAYLAND_DISPLAY").is_some() {
            candidates.push(("wl-copy", vec![]));
        }
        if env::var_os("DISPLAY").is_some() {
            candidates.push(("xclip", vec!["-selection", "clipboard", "-in"]));
            candidates.push(("xsel", vec!["--clipboard", "--input"]));
        }
        let mut reasons = Vec::new();
        for (program, args) in candidates {
            let mut child = match Command::new(program)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(error) => {
                    reasons.push(format!("{program}: {error}"));
                    continue;
                }
            };
            let Some(mut stdin) = child.stdin.take() else {
                reasons.push(format!("{program}: stdin unavailable"));
                let _ = child.kill();
                continue;
            };
            let bytes = payload.to_vec();
            let (tx, rx) = std::sync::mpsc::channel();
            thread::spawn(move || {
                let result = stdin.write_all(&bytes);
                drop(stdin);
                let _ = tx.send(result);
            });
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut write_done = false;
            let mut write_ok = false;
            loop {
                if !write_done {
                    if let Ok(result) = rx.try_recv() {
                        write_done = true;
                        write_ok = result.is_ok();
                        if !write_ok {
                            break;
                        }
                    }
                }
                match child.try_wait() {
                    Ok(Some(status)) => {
                        if status.success() && write_ok {
                            return Ok(program);
                        }
                        reasons.push(format!("{program} exited {status}"));
                        break;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Ok(None) if write_ok => return Ok(program),
                    Ok(None) => {
                        reasons.push(format!("{program} did not accept logs in time"));
                        let _ = child.kill();
                        break;
                    }
                    Err(error) => {
                        reasons.push(format!("{program}: {error}"));
                        let _ = child.kill();
                        break;
                    }
                }
            }
        }
        Err(if reasons.is_empty() {
            "no local desktop clipboard helper".into()
        } else {
            reasons.join("; ")
        })
    }

    fn save_dialog(default_name: &str) -> (Option<PathBuf>, bool) {
        if desktop_available() {
            let proposed = dirs_downloads().join(default_name);
            if command_exists("zenity") {
                let result = Command::new("zenity")
                    .args([
                        "--file-selection",
                        "--save",
                        "--confirm-overwrite",
                        "--title=Save GitRun logs",
                    ])
                    .arg(format!("--filename={}", proposed.display()))
                    .output();
                return match result {
                    Ok(output) if output.status.success() => {
                        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                        if value.is_empty() {
                            (None, true)
                        } else {
                            (Some(PathBuf::from(value)), false)
                        }
                    }
                    _ => (None, true),
                };
            }
            if command_exists("kdialog") {
                let result = Command::new("kdialog")
                    .args(["--getsavefilename"])
                    .arg(&proposed)
                    .args(["*.txt|Text files", "--title", "Save GitRun logs"])
                    .output();
                return match result {
                    Ok(output) if output.status.success() => {
                        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                        if value.is_empty() {
                            (None, true)
                        } else {
                            (Some(PathBuf::from(value)), false)
                        }
                    }
                    _ => (None, true),
                };
            }
        }
        let downloads = dirs_downloads();
        if fs::create_dir_all(&downloads).is_ok() {
            (Some(downloads.join(default_name)), false)
        } else {
            (Some(home_dir().join(default_name)), false)
        }
    }

    fn home_dir() -> PathBuf {
        env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(env::temp_dir)
    }

    fn dirs_downloads() -> PathBuf {
        home_dir().join("Downloads")
    }

    fn command_exists(program: &str) -> bool {
        env::var_os("PATH")
            .into_iter()
            .flat_map(|paths| env::split_paths(&paths))
            .any(|directory| directory.join(program).is_file())
    }

    fn save_payload(payload: &[u8]) -> io::Result<Option<PathBuf>> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let default_name = format!("GitRun-logs-{stamp}.txt");
        let (selected, cancelled) = save_dialog(&default_name);
        if cancelled {
            return Ok(None);
        }
        let Some(path) = selected else {
            return Ok(None);
        };
        let path = if path.to_string_lossy().starts_with('~') {
            home_dir().join(path.to_string_lossy().trim_start_matches("~/"))
        } else {
            path
        };
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "save path has no parent")
        })?;
        fs::create_dir_all(parent)?;
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        file.write_all(payload)?;
        file.sync_all()?;
        if desktop_available() {
            if command_exists("xdg-open") {
                let _ = Command::new("xdg-open")
                    .arg(&path)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            } else if command_exists("gio") {
                let _ = Command::new("gio")
                    .arg("open")
                    .arg(&path)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            }
        }
        Ok(Some(path))
    }

    fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
        if left.len() != right.len() {
            return false;
        }
        left.iter()
            .zip(right)
            .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
            == 0
    }

    fn handle_client(
        mut stream: TcpStream,
        token: &str,
        saved: &mut Vec<PathBuf>,
    ) -> io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let hello = read_line(&mut stream, 512)?;
        if !constant_time_equal(
            hello.as_bytes(),
            format!("GITRUN_LOGS_V1 {token}").as_bytes(),
        ) {
            let _ = write_line(&mut stream, "DENIED");
            return Ok(());
        }
        write_line(&mut stream, "READY")?;
        let first = read_line(&mut stream, 512)?;
        let payload = match read_payload(&mut stream, &first) {
            Ok(payload) => payload,
            Err(error) => {
                let _ = write_line(&mut stream, "ERROR invalid log header");
                return Err(error);
            }
        };
        match clipboard(&payload) {
            Ok(_) => write_line(&mut stream, "COPIED")?,
            Err(_) => {
                write_line(&mut stream, "FALLBACK")?;
                stream.set_read_timeout(Some(PROTOCOL_TIMEOUT))?;
                stream.set_write_timeout(Some(PROTOCOL_TIMEOUT))?;
                let header = read_line(&mut stream, 512)?;
                let fallback = read_payload(&mut stream, &header)?;
                match save_payload(&fallback) {
                    Ok(Some(path)) => {
                        saved.push(path);
                        write_line(&mut stream, "SAVED")?;
                    }
                    Ok(None) => write_line(&mut stream, "CANCELLED")?,
                    Err(error) => {
                        let _ = write_line(&mut stream, "ERROR local save failed");
                        return Err(error);
                    }
                }
            }
        }
        Ok(())
    }

    fn run_target(target: &str) -> io::Result<i32> {
        if !valid_target(target) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "target must be user@MainNode or a Tailscale address",
            ));
        }
        let ssh = find_program("ssh")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "OpenSSH client is required"))?;
        let token = random_hex(16)?;
        let token_path = format!("{TOKEN_PREFIX}{}.txt", random_hex(16)?);
        remote_token(&ssh, target, &token, &token_path)?;
        let random_port = u16::from_str_radix(&random_hex(2)?, 16)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let remote_port = 41000_u16 + (random_port % 16000);
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let local_port = listener.local_addr()?.port();
        let forward = format!("127.0.0.1:{remote_port}:127.0.0.1:{local_port}");
        let remote = format!("trap 'rm -f {token_path}' EXIT; GITRUN_LOG_EXPORT_PORT={remote_port} GITRUN_LOG_EXPORT_TOKEN_FILE={token_path} gitrun manage");
        let mut child = Command::new(&ssh)
            .args([
                "-o",
                "ExitOnForwardFailure=no",
                "-R",
                &forward,
                "-tt",
                target,
                &remote,
            ])
            .spawn()?;
        let mut saved = Vec::new();
        let result = loop {
            match child.try_wait()? {
                Some(status) => break status.code().unwrap_or(1),
                None => {}
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Err(error) = handle_client(stream, &token, &mut saved) {
                        eprintln!("GitRun local log export: {error}");
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100))
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            }
        };
        drop(listener);
        if !saved.is_empty() {
            println!("\nGitRun logs saved locally:");
            for path in saved {
                println!("  {}", path.display());
            }
            println!("The file was opened with the default text editor when a graphical session was available.");
        }
        Ok(result)
    }

    fn find_program(name: &str) -> Option<String> {
        env::var_os("PATH")
            .into_iter()
            .flat_map(|paths| env::split_paths(&paths))
            .map(|directory| directory.join(name))
            .find(|path| path.is_file())
            .map(|path| path.to_string_lossy().into_owned())
    }

    fn valid_source(source: &str) -> Option<(String, String, String)> {
        let (login_host, path) = source.split_once(':')?;
        let (user, host) = login_host.split_once('@')?;
        let safe_user = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        };
        let safe_host = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        };
        let suffix = path
            .strip_prefix("/tmp/GitRun-logs-")?
            .strip_suffix(".txt")?;
        if !safe_user(user)
            || !safe_host(host)
            || suffix.len() != 32
            || !suffix.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return None;
        }
        Some((user.to_owned(), host.to_owned(), path.to_owned()))
    }

    fn receive_source(source: &str) -> io::Result<i32> {
        let (user, host, path) = valid_source(source).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "source must be user@host:/tmp/GitRun-logs-<32 hex characters>.txt",
            )
        })?;
        let scp = find_program("scp").ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "OpenSSH scp client is required")
        })?;
        let destination = env::temp_dir().join(format!(
            "gitrun-logs-{}-{}.tmp",
            std::process::id(),
            random_hex(8)?
        ));
        let remote = format!("{user}@{host}:{path}");
        let status = Command::new(scp).arg(remote).arg(&destination).status()?;
        if !status.success() {
            let _ = fs::remove_file(&destination);
            return Ok(status.code().unwrap_or(1));
        }
        let result = (|| {
            let metadata = fs::metadata(&destination)?;
            if metadata.len() > MAX_LOG_BYTES as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "downloaded logs exceed 32 MiB",
                ));
            }
            let data = fs::read(&destination)?;
            match save_payload(&data)? {
                Some(path) => {
                    println!("Saved GitRun logs to {}", path.display());
                    Ok(0)
                }
                None => {
                    println!("Save cancelled; no local file was written.");
                    Ok(1)
                }
            }
        })();
        let _ = fs::remove_file(&destination);
        result
    }

    pub(super) fn run(target: Option<&str>, source: Option<&str>) -> io::Result<i32> {
        match (target, source) {
            (Some(target), None) => run_target(target),
            (None, Some(source)) => receive_source(source),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "specify exactly one of --target or --source",
            )),
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn run(target: Option<&str>, source: Option<&str>) -> io::Result<i32> {
    linux::run(target, source)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn run(_target: Option<&str>, _source: Option<&str>) -> io::Result<i32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the SSH log-export wrapper is currently available on Linux",
    ))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::linux::{valid_source, valid_target};

    #[test]
    fn ssh_targets_reject_options_and_shell_metacharacters() {
        assert!(valid_target("operator@MainNode"));
        assert!(valid_target("100.64.0.12"));
        assert!(!valid_target("-oProxyCommand=sh"));
        assert!(!valid_target("host;touch /tmp/pwned"));
        assert!(!valid_target(""));
    }

    #[test]
    fn source_accepts_only_generated_log_file_paths() {
        assert!(valid_source(
            "operator@100.64.0.12:/tmp/GitRun-logs-0123456789abcdef0123456789abcdef.txt"
        )
        .is_some());
        assert!(valid_source("operator@host:/etc/passwd").is_none());
        assert!(valid_source(
            "operator@host:/tmp/GitRun-logs-0123456789abcdef0123456789abcdeg.txt"
        )
        .is_none());
        assert!(valid_source(
            "-oProxyCommand@host:/tmp/GitRun-logs-0123456789abcdef0123456789abcdef.txt"
        )
        .is_none());
    }
}
