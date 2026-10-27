use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum JsonRecoveryAction {
    Repair,
    RestoreBackups,
    RestoreMainLibrary,
}

impl JsonRecoveryAction {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "repair" => Ok(Self::Repair),
            "restore-backups" => Ok(Self::RestoreBackups),
            "restore-main-library" => Ok(Self::RestoreMainLibrary),
            _ => Err(format!("unknown JSON recovery action: {value}")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Repair => "repair",
            Self::RestoreBackups => "restore-backups",
            Self::RestoreMainLibrary => "restore-main-library",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct JsonRecoveryReport {
    pub action: String,
    pub scanned: usize,
    pub changed: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
    pub details: Vec<String>,
}

/// Route privileged changes through the installed GitRun helper; the UI never
/// supplies an arbitrary path or shell command.
pub fn run_json_recovery_action(action: JsonRecoveryAction) -> Result<JsonRecoveryReport, String> {
    if super::is_root() {
        // The Control Manager runs inside a hardened systemd mount namespace.
        // A normal child inherits that namespace and can still see the library
        // as read-only. Run the fixed-purpose restore in its own transient unit
        // with only the official library destination writable.
        if action == JsonRecoveryAction::RestoreMainLibrary {
            return restore_main_library_via_transient_unit();
        }
        return run_json_recovery_root_action(action.as_str());
    }

    let executable = super::find_trusted_installed_gitrun_binary()
        .ok_or("trusted installed GitRun executable was not found for JSON recovery")?;
    let pkexec =
        super::find_trusted_pkexec().ok_or("trusted /usr/bin/pkexec executable was not found")?;
    let output = Command::new(pkexec)
        .arg(executable)
        .arg("--no-cat")
        .arg("--json-recovery-root")
        .arg(action.as_str())
        .output()
        .map_err(|error| format!("unable to start privileged JSON recovery: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if detail.is_empty() {
            format!("privileged JSON recovery exited with {}", output.status)
        } else {
            format!(
                "privileged JSON recovery exited with {}: {detail}",
                output.status
            )
        });
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("decode JSON recovery report: {error}"))
}

#[cfg(target_os = "linux")]
fn restore_main_library_via_transient_unit() -> Result<JsonRecoveryReport, String> {
    let executable = super::find_trusted_installed_gitrun_binary()
        .ok_or("trusted installed GitRun executable was not found for Main Library restore")?;
    let systemd_run = Path::new("/usr/bin/systemd-run");
    if !super::is_trusted_root_executable(systemd_run) {
        return Err("trusted root-owned /usr/bin/systemd-run executable was not found".into());
    }

    let unit_name = format!("gitrun-main-library-restore-{}.service", std::process::id());
    let output = Command::new(systemd_run)
        .args(["--system", "--wait", "--pipe", "--collect", "--unit"])
        .arg(&unit_name)
        .args(main_library_restore_unit_properties())
        .args(["--description=GitRun signed Main Library restore", "--"])
        .arg(executable)
        .args(["--no-cat", "--json-recovery-root", "restore-main-library"])
        .output()
        .map_err(|error| format!("unable to start isolated Main Library restore: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let mut detail = if stderr.is_empty() { stdout } else { stderr };
        if detail.is_empty() {
            detail = transient_unit_journal(&unit_name).unwrap_or_else(|| {
                format!(
                    "no stdout/stderr captured; inspect the unit journal with `journalctl --no-pager --unit {unit_name} --lines 40`"
                )
            });
        }
        return Err(format!(
            "isolated Main Library restore exited with {} (unit {unit_name}): {detail}",
            output.status
        ));
    }

    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("decode isolated Main Library restore report: {error}"))
}

#[cfg(target_os = "linux")]
fn main_library_restore_unit_properties() -> [&'static str; 6] {
    [
        "--property=Type=oneshot",
        "--property=PrivateTmp=yes",
        "--property=ProtectSystem=full",
        "--property=ProtectHome=yes",
        "--property=NoNewPrivileges=yes",
        "--property=ReadWritePaths=/usr/share/gitrun/library",
    ]
}

#[cfg(target_os = "linux")]
fn transient_unit_journal(unit_name: &str) -> Option<String> {
    let journalctl = Path::new("/usr/bin/journalctl");
    if !super::is_trusted_root_executable(journalctl) {
        return None;
    }
    let output = Command::new(journalctl)
        .args([
            "--no-pager",
            "--unit",
            unit_name,
            "--lines=40",
            "--output=cat",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let detail = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if detail.is_empty() {
        return None;
    }
    Some(detail.chars().take(12_000).collect())
}

#[cfg(not(target_os = "linux"))]
fn restore_main_library_via_transient_unit() -> Result<JsonRecoveryReport, String> {
    Err("Main Library restore through the system service is supported only on Linux".into())
}

/// Root-only target for a fixed-purpose CLI entry point.
pub fn run_json_recovery_root_action(action: &str) -> Result<JsonRecoveryReport, String> {
    if !super::is_root() {
        return Err("JSON recovery requires administrator privileges".into());
    }
    let action = JsonRecoveryAction::parse(action)?;
    if action == JsonRecoveryAction::RestoreMainLibrary {
        let installed = gitrun_updater::install_main_library_from_latest_release(
            "Vider06/GitRun",
            Path::new("/usr/share/gitrun/library"),
            true,
        )
        .map_err(|error| format!("restore official Main Library: {error}"))?;
        return Ok(JsonRecoveryReport {
            action: action.as_str().into(),
            scanned: installed,
            changed: installed,
            skipped: 0,
            errors: Vec::new(),
            details: vec![format!(
                "Restored {installed} validated .json.Back reference file(s) in /usr/share/gitrun/library; active configuration files were not overwritten."
            )],
        });
    }

    let roots = json_recovery_roots();
    let mut report = JsonRecoveryReport {
        action: action.as_str().into(),
        ..JsonRecoveryReport::default()
    };
    let targets = collect_recovery_json_targets(
        &roots,
        action == JsonRecoveryAction::RestoreBackups,
        &mut report,
    );
    report.scanned = targets.len();
    match action {
        JsonRecoveryAction::Repair => repair_json_targets(&targets, &mut report),
        JsonRecoveryAction::RestoreBackups => restore_json_targets(&targets, &mut report),
        JsonRecoveryAction::RestoreMainLibrary => unreachable!("handled above"),
    }
    Ok(report)
}

fn json_recovery_roots() -> Vec<PathBuf> {
    let system_config = Path::new("/etc/gitrun/gitrun.env");
    let mut roots = Vec::new();
    if system_config.is_file() {
        roots.push(PathBuf::from("/etc/gitrun"));
        match gitrun_core::Config::from_env_file(system_config) {
            Ok(config) => roots.push(PathBuf::from(config.state_dir)),
            Err(_) => roots.push(PathBuf::from("/var/lib/gitrun")),
        }
    } else if let Some(path) = super::configured_config_file() {
        if let Some(parent) = path.parent() {
            roots.push(parent.to_path_buf());
        }
        if let Ok(config) = gitrun_core::Config::from_env_file(&path) {
            roots.push(PathBuf::from(config.state_dir));
        }
        if Path::new("config").is_dir() {
            roots.push(PathBuf::from("config"));
        }
    } else {
        roots.push(PathBuf::from("/etc/gitrun"));
        roots.push(PathBuf::from("/var/lib/gitrun"));
        if Path::new("config").is_dir() {
            roots.push(PathBuf::from("config"));
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn recovery_json_candidate(path: &Path, root: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    if !name.ends_with(".json") || name.ends_with("-status.json") || name.starts_with("model-") {
        return false;
    }
    if root == Path::new("/etc/gitrun") || root == Path::new("config") {
        return true;
    }
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    matches!(
        relative.as_str(),
        "gitrun-settings.json"
            | "vm-configs.json"
            | "logic-containers.json"
            | "kitty/settings.json"
            | "api-policies.json"
            | "repo-api-policies.json"
    )
}

fn collect_recovery_json_targets(
    roots: &[PathBuf],
    include_missing_backed_up_files: bool,
    report: &mut JsonRecoveryReport,
) -> Vec<PathBuf> {
    let mut targets = Vec::new();
    for root in roots {
        let metadata = match fs::symlink_metadata(root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                push_recovery_error(report, format!("{}: {error}", root.display()));
                continue;
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            push_recovery_error(
                report,
                format!("{} is not a real directory; skipped", root.display()),
            );
            continue;
        }
        let mut pending = vec![root.clone()];
        while let Some(directory) = pending.pop() {
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) => {
                    push_recovery_error(report, format!("{}: {error}", directory.display()));
                    continue;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        push_recovery_error(report, format!("directory entry: {error}"));
                        continue;
                    }
                };
                let path = entry.path();
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        push_recovery_error(report, format!("{}: {error}", path.display()));
                        continue;
                    }
                };
                if metadata.file_type().is_symlink() {
                    if recovery_json_candidate(&path, root) {
                        push_recovery_error(
                            report,
                            format!("{} is a symbolic link; not followed", path.display()),
                        );
                    }
                    continue;
                }
                if metadata.is_dir() {
                    pending.push(path);
                    continue;
                }
                if !metadata.is_file() {
                    continue;
                }
                if recovery_json_candidate(&path, root) {
                    targets.push(path);
                } else if include_missing_backed_up_files {
                    if let Some(target) = active_path_for_backup(&path) {
                        if recovery_json_candidate(&target, root) {
                            // Restore can recreate a deleted active JSON file from its backup.
                            // Any existing symlink is still rejected at write time.
                            targets.push(target);
                        }
                    }
                }
                if targets.len() >= 10_000 {
                    push_recovery_error(
                        report,
                        "JSON recovery scan reached the 10,000-file safety limit".into(),
                    );
                    return targets;
                }
            }
        }
    }
    targets.sort();
    targets.dedup();
    targets
}

fn active_path_for_backup(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let active_name = name
        .strip_suffix(".Back")
        .or_else(|| name.strip_suffix(".back"))?;
    if !active_name.to_ascii_lowercase().ends_with(".json") {
        return None;
    }
    Some(path.with_file_name(active_name))
}

fn append_backup_suffix(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".Back");
    PathBuf::from(name)
}

fn read_valid_json(path: &Path) -> Result<(Vec<u8>, serde_json::Value), String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{} is not a regular, non-symlink file",
            path.display()
        ));
    }
    if metadata.len() > 16 * 1024 * 1024 {
        return Err(format!(
            "{} exceeds the 16 MiB JSON safety limit",
            path.display()
        ));
    }
    let bytes = fs::read(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            format!("permission denied reading {}: {error}", path.display())
        } else {
            format!("unable to read {}: {error}", path.display())
        }
    })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "{} is invalid JSON at line {}, column {}: {error}",
            path.display(),
            error.line(),
            error.column()
        )
    })?;
    if !(value.is_object() || value.is_array()) {
        return Err(format!(
            "{} has a scalar JSON root; expected an object or array",
            path.display()
        ));
    }
    Ok((bytes, value))
}

fn repaired_json_value(content: &str, path: &Path) -> Result<serde_json::Value, String> {
    let without_bom = content.strip_prefix('\u{feff}').unwrap_or(content);
    // Missing closing braces/brackets at end-of-file can be repaired
    // deterministically when every opener and closer still matches. Do not
    // guess missing values, commas, or the end of an unterminated string.
    let completed = close_unfinished_containers(without_bom)
        .map_err(|detail| format!("{} could not be repaired safely: {detail}", path.display()))?;
    let normalized = remove_trailing_json_commas(&completed);
    let value: serde_json::Value = serde_json::from_str(&normalized).map_err(|error| {
        format!(
            "{} remains invalid after safe normalization at line {}, column {}: {error}",
            path.display(),
            error.line(),
            error.column()
        )
    })?;
    if !(value.is_object() || value.is_array()) {
        return Err(format!(
            "{} has a scalar JSON root; expected an object or array",
            path.display()
        ));
    }
    Ok(value)
}

fn close_unfinished_containers(input: &str) -> Result<String, &'static str> {
    let mut output = String::with_capacity(input.len().saturating_add(8));
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for ch in input.chars() {
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                output.push(ch);
            }
            '{' | '[' => {
                stack.push(ch);
                output.push(ch);
            }
            '}' => {
                if stack.pop() != Some('{') {
                    return Err("mismatched or unexpected closing brace");
                }
                output.push(ch);
            }
            ']' => {
                if stack.pop() != Some('[') {
                    return Err("mismatched or unexpected closing bracket");
                }
                output.push(ch);
            }
            _ => output.push(ch),
        }
    }

    if in_string {
        return Err("unterminated string; refusing to guess its value");
    }

    while let Some(open) = stack.pop() {
        output.push(match open {
            '{' => '}',
            '[' => ']',
            _ => return Err("unknown JSON container delimiter"),
        });
    }
    Ok(output)
}

fn remove_trailing_json_commas(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut output = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if ch == '"' {
            in_string = true;
            output.push(ch);
            index += 1;
            continue;
        }
        if ch == ',' {
            let mut lookahead = index + 1;
            while lookahead < chars.len() && chars[lookahead].is_whitespace() {
                lookahead += 1;
            }
            if lookahead < chars.len() && matches!(chars[lookahead], '}' | ']') {
                index += 1;
                continue;
            }
        }
        output.push(ch);
        index += 1;
    }
    output
}

fn same_json_root_shape(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    (left.is_object() && right.is_object()) || (left.is_array() && right.is_array())
}

fn mode_for_existing_json(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o777)
            .unwrap_or(0o600)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0o600
    }
}

fn write_recovered_json(path: &Path, content: &[u8]) -> Result<(), String> {
    let mode = mode_for_existing_json(path);
    gitrun_updater::atomic_write_installed_file(path, content, mode)
        .map_err(|error| format!("atomic write failed for {}: {error}", path.display()))
}

fn repair_json_targets(targets: &[PathBuf], report: &mut JsonRecoveryReport) {
    for path in targets {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) => {
                report.skipped += 1;
                push_recovery_error(
                    report,
                    format!("{} cannot be read: {error}", path.display()),
                );
                continue;
            }
        };
        if raw.len() > 16 * 1024 * 1024 {
            report.skipped += 1;
            push_recovery_error(
                report,
                format!("{} exceeds the 16 MiB repair limit", path.display()),
            );
            continue;
        }
        let reference_path = append_backup_suffix(path);
        let reference_value = match fs::symlink_metadata(&reference_path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                report.skipped += 1;
                push_recovery_error(
                    report,
                    format!("{} is not a regular backup file", reference_path.display()),
                );
                continue;
            }
            Ok(_) => match read_valid_json(&reference_path) {
                Ok((_, value)) => Some(value),
                Err(error) => {
                    report.skipped += 1;
                    push_recovery_error(report, format!("backup reference rejected: {error}"));
                    continue;
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                report.skipped += 1;
                push_recovery_error(report, format!("{}: {error}", reference_path.display()));
                continue;
            }
        };
        let value = match repaired_json_value(&raw, path) {
            Ok(value) => value,
            Err(error) => {
                report.skipped += 1;
                push_recovery_error(report, error);
                continue;
            }
        };
        if reference_value
            .as_ref()
            .is_some_and(|reference| !same_json_root_shape(reference, &value))
        {
            report.skipped += 1;
            push_recovery_error(
                report,
                format!(
                    "{} differs from the object/array shape in its .Back reference; left untouched",
                    path.display()
                ),
            );
            continue;
        }
        let mut formatted = match serde_json::to_vec_pretty(&value) {
            Ok(bytes) => bytes,
            Err(error) => {
                report.skipped += 1;
                push_recovery_error(
                    report,
                    format!("{} cannot be serialized safely: {error}", path.display()),
                );
                continue;
            }
        };
        formatted.push(b'\n');
        if formatted == raw.as_bytes() {
            report.skipped += 1;
            continue;
        }
        match write_recovered_json(path, &formatted) {
            Ok(()) => {
                report.changed += 1;
                push_recovery_detail(report, format!("Reformatted/normalized {}", path.display()));
            }
            Err(error) => {
                report.skipped += 1;
                push_recovery_error(report, error);
            }
        }
    }
    if report.details.is_empty() {
        report
            .details
            .push("No JSON files needed a safe formatting repair.".into());
    }
}

fn restore_json_targets(targets: &[PathBuf], report: &mut JsonRecoveryReport) {
    for path in targets {
        let backup = append_backup_suffix(path);
        match fs::symlink_metadata(&backup) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                report.skipped += 1;
                continue;
            }
            Err(error) => {
                report.skipped += 1;
                push_recovery_error(report, format!("{}: {error}", backup.display()));
                continue;
            }
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                report.skipped += 1;
                push_recovery_error(
                    report,
                    format!("{} is not a regular, non-symlink backup", backup.display()),
                );
                continue;
            }
            Ok(_) => {}
        }
        let backup_bytes = match read_valid_json(&backup) {
            Ok((bytes, _)) => bytes,
            Err(error) => {
                report.skipped += 1;
                push_recovery_error(report, format!("backup rejected: {error}"));
                continue;
            }
        };
        match write_recovered_json(path, &backup_bytes) {
            Ok(()) => {
                report.changed += 1;
                push_recovery_detail(report, format!("Restored {}", path.display()));
            }
            Err(error) => {
                report.skipped += 1;
                push_recovery_error(report, error);
            }
        }
    }
    if report.details.is_empty() {
        report
            .details
            .push("No usable sibling .json.Back backups were found.".into());
    }
}

fn push_recovery_error(report: &mut JsonRecoveryReport, detail: String) {
    if report.errors.len() < 100 {
        report.errors.push(detail);
    } else if report.errors.len() == 100 {
        report
            .errors
            .push("Additional errors omitted after the 100-entry report limit.".into());
    }
}

fn push_recovery_detail(report: &mut JsonRecoveryReport, detail: String) {
    if report.details.len() < 100 {
        report.details.push(detail);
    } else if report.details.len() == 100 {
        report
            .details
            .push("Additional changed paths omitted after the 100-entry report limit.".into());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        active_path_for_backup, append_backup_suffix, remove_trailing_json_commas,
        repaired_json_value, same_json_root_shape, JsonRecoveryAction,
    };
    use std::path::Path;

    #[cfg(target_os = "linux")]
    #[test]
    fn main_library_restore_oneshot_does_not_force_an_immediate_start_timeout() {
        use super::main_library_restore_unit_properties;
        let properties = main_library_restore_unit_properties();
        assert!(properties.contains(&"--property=Type=oneshot"));
        assert!(!properties.contains(&"--property=TimeoutStartSec=0"));
        assert!(properties.contains(&"--property=ReadWritePaths=/usr/share/gitrun/library"));
    }

    #[test]
    fn recovery_action_parser_accepts_only_the_three_supported_operations() {
        assert_eq!(
            JsonRecoveryAction::parse("repair").unwrap(),
            JsonRecoveryAction::Repair
        );
        assert_eq!(
            JsonRecoveryAction::parse("restore-backups").unwrap(),
            JsonRecoveryAction::RestoreBackups
        );
        assert_eq!(
            JsonRecoveryAction::parse("restore-main-library").unwrap(),
            JsonRecoveryAction::RestoreMainLibrary
        );
        assert!(JsonRecoveryAction::parse("../../etc/shadow").is_err());
        assert!(JsonRecoveryAction::parse("restore-anything").is_err());
    }

    #[test]
    fn repair_normalizes_bom_trailing_commas_and_formatting_without_changing_values() {
        let raw = "\u{feff}{\n  \"enabled\": true,\n  \"items\": [1, 2,],\n}";
        let value = repaired_json_value(raw, Path::new("example.json")).unwrap();
        assert_eq!(value["enabled"], true);
        assert_eq!(value["items"][1], 2);
        assert!(serde_json::to_string_pretty(&value)
            .unwrap()
            .contains("\"enabled\": true"));
    }

    #[test]
    fn repair_closes_only_unambiguous_end_of_file_containers() {
        let value = repaired_json_value(r#"{"nested":[1,2"#, Path::new("truncated.json"))
            .expect("missing terminal brackets can be restored without guessing values");
        assert_eq!(value["nested"][0], 1);
        assert_eq!(value["nested"][1], 2);

        let value = repaired_json_value(
            r#"{"items":[1,2, "#,
            Path::new("trailing-comma-and-brackets.json"),
        )
        .expect("terminal containers and a trailing comma can be repaired safely");
        assert_eq!(value["items"][1], 2);

        assert!(repaired_json_value(r#"{"enabled": "#, Path::new("missing-value.json")).is_err());
        assert!(
            repaired_json_value(r#"{"message":"unterminated"#, Path::new("open-string.json"))
                .is_err()
        );
        assert!(repaired_json_value(r#"{"items":[1,2}}"#, Path::new("mismatched.json")).is_err());
    }

    #[test]
    fn trailing_comma_removal_ignores_commas_inside_strings() {
        let raw = r#"{"message":"hello,}", "items":[1,]}"#;
        let normalized = remove_trailing_json_commas(raw);
        let value: serde_json::Value = serde_json::from_str(&normalized).unwrap();
        assert_eq!(value["message"], "hello,}");
        assert_eq!(value["items"][0], 1);
    }

    #[test]
    fn restore_can_map_a_backup_to_a_missing_active_json_file() {
        assert_eq!(
            active_path_for_backup(Path::new("/var/lib/gitrun/kitty/settings.json.Back"))
                .unwrap()
                .as_path(),
            Path::new("/var/lib/gitrun/kitty/settings.json")
        );
        assert!(active_path_for_backup(Path::new("/var/lib/gitrun/model-123.json")).is_none());
    }

    #[test]
    fn backup_suffix_and_root_shape_are_explicit() {
        assert_eq!(
            append_backup_suffix(Path::new("/etc/gitrun/kitty.json")).to_string_lossy(),
            "/etc/gitrun/kitty.json.Back"
        );
        let object = serde_json::json!({"ok":true});
        let array = serde_json::json!([1, 2]);
        assert!(same_json_root_shape(
            &object,
            &serde_json::json!({"ok":false})
        ));
        assert!(!same_json_root_shape(&object, &array));
    }
}
