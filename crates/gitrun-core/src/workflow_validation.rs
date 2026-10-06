//! Pre-flight validation of a repository's `.github/workflows/*.yml` files,
//! run before a job is handed to a runner (see `Config::gsr_workflow_validation_enabled`).
//!
//! This is deliberately a *line-pattern* scan, not a full YAML parser: the
//! workflow text is attacker-influenced (anyone who can open a PR can shape
//! it), and a full parser is itself attack surface we'd rather not carry
//! here, on top of the workspace's existing pinned-toolchain fragility (see
//! the project's session notes on `Cargo.lock`/edition2024 constraints —
//! adding a YAML crate is more dependency-graph risk than this module's
//! job justifies). The tradeoff is the same one made in
//! `command_policy::CommandPolicy::evaluate`: some false negatives on
//! deliberately obfuscated YAML, in exchange for a scanner that cannot
//! itself be exploited via crafted input and has no new dependencies.
//!
//! Two layers, matching the operator's request:
//! 1. Our own built-in checks (`scan`), always available, based on the
//!    well-documented GitHub Actions risk patterns (`pull_request_target`
//!    combined with an explicit checkout of the PR head, unquoted/untrusted
//!    `${{ }}` expansion directly in `run:`, `ACTIONS_ALLOW_UNSECURE_COMMANDS`).
//! 2. An optional shell-out to the third-party `zizmor` static analyzer
//!    (MIT license, <https://github.com/zizmorcore/zizmor> — credited in
//!    this project's README/NOTICE) if installed and enabled via
//!    `Config::gsr_zizmor_enabled`. `zizmor` is a far more thorough
//!    workflow analyzer than anything reasonable to reimplement here; we
//!    do not vendor it as a Rust library dependency because it is
//!    published and versioned as a CLI tool (not a stable embeddable
//!    library) and because it targets a newer Rust edition than this
//!    workspace's pinned toolchain currently supports. Invoking it as a
//!    subprocess avoids both problems. A missing binary is not an error —
//!    see `run_zizmor` — since it's an optional enhancement, not a
//!    dependency GitRun installs on the operator's behalf.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

/// One finding from either the built-in scan or `zizmor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub file: String,
    pub line: Option<usize>,
    pub rule: String,
    pub message: String,
}

/// Result of validating one workflow file (or a whole directory's worth,
/// see `validate_workflows_dir`).
#[derive(Debug, Clone, Default)]
pub struct ValidationReport {
    pub findings: Vec<Finding>,
    pub dock_requests: Vec<DockRequest>,
}

/// One statically detected GitDockRun job reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockRequest {
    pub file: String,
    pub line: usize,
    pub calling_job: Option<String>,
    pub target_job: String,
    pub operation: DockOperation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DockOperation {
    Connect,
    Disconnect,
    Melt,
    Read,
    Write,
    Execute,
}

impl DockOperation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Disconnect => "disconnect",
            Self::Melt => "melt",
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
        }
    }
}

impl ValidationReport {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Finds workflow calls to the closed GitDockRun API and extracts the
/// logical job name. This never talks to Docker and never executes a job.
pub fn scan_dock_requests(file_label: &str, content: &str) -> Vec<DockRequest> {
    let mut requests = Vec::new();
    let mut in_jobs = false;
    let mut current_job: Option<String> = None;
    let mut run_block_indent: Option<usize> = None;

    for (idx, raw_line) in content.lines().enumerate() {
        let indent = raw_line.chars().take_while(|c| c.is_whitespace()).count();
        let line = raw_line.trim_start();
        if line == "jobs:" {
            in_jobs = true;
            current_job = None;
            continue;
        }
        if in_jobs && !line.is_empty() && indent == 0 && !line.starts_with('#') {
            in_jobs = false;
            current_job = None;
        }
        if in_jobs && indent == 2 && line.ends_with(':') && !line.starts_with('-') {
            current_job = Some(line[..line.len() - 1].trim().to_owned());
        }
        if let Some(block_indent) = run_block_indent {
            if !line.is_empty() && indent <= block_indent {
                run_block_indent = None;
            } else {
                collect_dock_request_from_line(
                    file_label,
                    idx + 1,
                    current_job.as_deref(),
                    line,
                    &mut requests,
                );
                continue;
            }
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("run:") || lower.starts_with("- run:") || lower.starts_with("-run:") {
            collect_dock_request_from_line(
                file_label,
                idx + 1,
                current_job.as_deref(),
                line,
                &mut requests,
            );
            let run_value = lower
                .strip_prefix("run:")
                .or_else(|| lower.strip_prefix("- run:"))
                .or_else(|| lower.strip_prefix("-run:"))
                .unwrap_or_default()
                .trim();
            if matches!(run_value, "|" | ">" | "|-" | "|+" | ">-" | ">+") {
                run_block_indent = Some(indent);
            }
        }
    }
    requests
}

fn collect_dock_request_from_line(
    file_label: &str,
    line_number: usize,
    calling_job: Option<&str>,
    line: &str,
    requests: &mut Vec<DockRequest>,
) {
    let lower = line.to_ascii_lowercase();
    let Some(api_index) = lower.find("gitdockrun") else {
        return;
    };
    let operation = if lower.contains("--connect") {
        DockOperation::Connect
    } else if lower.contains("--disconnect") {
        DockOperation::Disconnect
    } else if lower.contains("--melt") {
        DockOperation::Melt
    } else if lower.contains("--execute") {
        DockOperation::Execute
    } else if lower.contains("--write") {
        DockOperation::Write
    } else if lower.contains("--read") || lower.contains("--get") {
        DockOperation::Read
    } else {
        return;
    };
    let tail = &line[api_index + "gitdockrun".len()..];
    let lower_tail = tail.to_ascii_lowercase();
    let Some(job_index) = lower_tail.find("--job") else {
        return;
    };
    let mut value = tail[job_index + "--job".len()..].trim_start();
    if let Some(rest) = value.strip_prefix('=') {
        value = rest.trim_start();
    }
    let token = value
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|ch| ch == '"' || ch == '\'' || ch == '`');
    if token.is_empty() {
        return;
    }
    requests.push(DockRequest {
        file: file_label.to_owned(),
        line: line_number,
        calling_job: calling_job.map(str::to_owned),
        target_job: token.to_owned(),
        operation,
    });
}
/// Built-in line-pattern checks against one workflow file's raw text.
/// `file_label` is used only for the `Finding::file` field (typically the
/// path relative to the repo root, e.g. `.github/workflows/ci.yml`).
pub fn scan(file_label: &str, content: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let lower_whole = content.to_ascii_lowercase();

    // `pull_request_target` runs with the base repo's secrets/token even
    // for a fork PR. That's fine on its own, but combined with an explicit
    // checkout of the PR's *head* ref, it hands attacker-controlled code
    // the base repo's credentials - one of the most common real-world
    // GitHub Actions compromise patterns. We can't fully resolve `${{ }}`
    // expressions here (that needs real YAML+expression evaluation), so
    // this flags the combination for a human to check rather than trying
    // to prove exploitability.
    if lower_whole.contains("pull_request_target")
        && (lower_whole.contains("github.event.pull_request.head")
            || lower_whole.contains("refs/pull/"))
    {
        findings.push(Finding {
            file: file_label.to_owned(),
            line: None,
            rule: "pull_request_target-with-head-checkout".into(),
            message: "workflow uses pull_request_target together with what looks like a checkout of the PR head — this can hand a fork PR's untrusted code access to the base repo's secrets".into(),
        });
    }

    // The opt-out flag for GitHub's own injection-safety fix on the old
    // `set-env`/`add-path` workflow commands. Legitimate uses exist but
    // are rare; worth a flag either way.
    if lower_whole.contains("actions_allow_unsecure_commands") {
        findings.push(Finding {
            file: file_label.to_owned(),
            line: None,
            rule: "unsecure-commands-enabled".into(),
            message: "workflow sets ACTIONS_ALLOW_UNSECURE_COMMANDS, re-enabling workflow commands with known injection history".into(),
        });
    }

    const UNTRUSTED_CONTEXTS: [&str; 6] = [
        "github.event.issue.title",
        "github.event.issue.body",
        "github.event.pull_request.title",
        "github.event.pull_request.body",
        "github.head_ref",
        "github.event.comment.body",
    ];

    let mut run_block_indent: Option<usize> = None;

    for (idx, raw_line) in content.lines().enumerate() {
        let indent = raw_line.chars().take_while(|c| c.is_whitespace()).count();
        let line = raw_line.trim_start();
        let lower_line = line.to_ascii_lowercase();

        if let Some(block_indent) = run_block_indent {
            if !line.is_empty() && indent <= block_indent {
                run_block_indent = None;
            } else {
                for ctx in UNTRUSTED_CONTEXTS {
                    if lower_line.contains(ctx) && lower_line.contains("${{") {
                        findings.push(Finding {
                            file: file_label.to_owned(),
                            line: Some(idx + 1),
                            rule: "template-injection-risk".into(),
                            message: format!("line splices an attacker-controlled expression context ({ctx}) directly into a shell command"),
                        });
                    }
                }
                continue;
            }
        }

        let inline_run = lower_line.starts_with("run:")
            || lower_line.starts_with("- run:")
            || lower_line.starts_with("-run:");
        if !inline_run {
            continue;
        }

        for ctx in UNTRUSTED_CONTEXTS {
            if lower_line.contains(ctx) && lower_line.contains("${{") {
                findings.push(Finding {
                    file: file_label.to_owned(),
                    line: Some(idx + 1),
                    rule: "template-injection-risk".into(),
                    message: format!("line splices an attacker-controlled expression context ({ctx}) directly into a shell command"),
                });
            }
        }

        let run_value = lower_line
            .strip_prefix("run:")
            .or_else(|| lower_line.strip_prefix("- run:"))
            .or_else(|| lower_line.strip_prefix("-run:"))
            .unwrap_or_default()
            .trim();
        if matches!(run_value, "|" | ">" | "|-" | "|+" | ">-" | ">+") {
            run_block_indent = Some(indent);
        }
    }
    findings
}

/// Validates every `*.yml`/`*.yaml` file directly under `workflows_dir`
/// (typically `<repo>/.github/workflows`). Missing directory is not an
/// error - a repo with no workflows yet, or one whose checkout step
/// hasn't run, is not itself a validation failure.
pub fn validate_workflows_dir(workflows_dir: &Path) -> std::io::Result<ValidationReport> {
    let mut report = ValidationReport::default();
    let entries = match std::fs::read_dir(workflows_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let is_yaml = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yml") | Some("yaml")
        );
        if !is_yaml || !path.is_file() {
            continue;
        }
        let content = std::fs::read_to_string(&path)?;
        let label = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<workflow>")
            .to_owned();
        report.findings.extend(scan(&label, &content));
        report
            .dock_requests
            .extend(scan_dock_requests(&label, &content));
    }
    Ok(report)
}

/// Shells out to `zizmor --format json <path>` if the binary is on `PATH`.
/// Returns `Ok(None)` (not an error) if `zizmor` isn't installed — this is
/// an optional third-party enhancement (see module docs), and its absence
/// must never block a job from running. Returns `Ok(Some(findings))` on a
/// successful run, parsing zizmor's own JSON finding format loosely (only
/// the fields this module needs) so an unrecognized future zizmor output
/// shape degrades to "no additional findings" rather than a hard error.
pub fn run_zizmor(workflows_dir: &Path) -> std::io::Result<Option<Vec<Finding>>> {
    run_zizmor_command("zizmor", workflows_dir)
}

fn run_zizmor_command(
    program: &str,
    workflows_dir: &Path,
) -> std::io::Result<Option<Vec<Finding>>> {
    let output = match Command::new(program)
        .arg("--format")
        .arg("json")
        .arg(workflows_dir)
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // zizmor exits non-zero when it finds issues - that's expected, not a
    // failure of the invocation itself. We only bail on stdout we can't
    // read at all.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = match serde_json::from_str(&stdout) {
        Ok(value) => value,
        Err(_) => return Ok(Some(Vec::new())),
    };
    let Some(items) = parsed.as_array() else {
        return Ok(Some(Vec::new()));
    };
    let findings = items
        .iter()
        .map(|item| Finding {
            file: item
                .get("file")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>")
                .to_owned(),
            line: item
                .get("line")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize),
            rule: item
                .get("ident")
                .or_else(|| item.get("rule"))
                .and_then(|v| v.as_str())
                .unwrap_or("zizmor-finding")
                .to_owned(),
            message: item
                .get("desc")
                .or_else(|| item.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("zizmor reported a finding with no description")
                .to_owned(),
        })
        .collect();
    Ok(Some(findings))
}

/// Static metadata about the optional `zizmor` integration, shown on the
/// dashboard's "Learn more" page before an operator enables it. Kept as
/// plain data (not fetched from anywhere) so the credit/license summary
/// shown to the operator can't silently drift from what's actually
/// bundled or change without a GitRun release — if zizmor's real license
/// terms ever change, this struct is the place to update to match.
#[derive(Debug, Clone, Serialize)]
pub struct ZizmorInfo {
    pub name: &'static str,
    pub author: &'static str,
    pub homepage: &'static str,
    pub repository: &'static str,
    pub license_name: &'static str,
    pub license_url: &'static str,
    pub description: &'static str,
    /// Short, factual summary of what accepting means, shown directly
    /// above the accept button - not the full license text (linked
    /// instead via `license_url`), because most people won't read a full
    /// license in a modal and a short accurate summary serves the consent
    /// step better than a wall of legal text no one scrolls through.
    pub terms_summary: &'static str,
}

// Keep the optional analyzer metadata centralized so enabling it remains auditable.
pub fn zizmor_info() -> ZizmorInfo {
    ZizmorInfo {
        name: "zizmor",
        author: "William Woodruff and the zizmor project (zizmorcore)",
        homepage: "https://docs.zizmor.sh",
        repository: "https://github.com/zizmorcore/zizmor",
        license_name: "MIT License",
        license_url: "https://github.com/zizmorcore/zizmor/blob/main/LICENSE",
        description: "zizmor is a third-party, open-source static analysis tool for GitHub Actions. GitRun can optionally run it against a repository's workflow files before a job starts, in addition to GitRun's own built-in checks, to catch a wider range of known workflow security issues (template injection, credential leakage, excessive permissions, and more).",
        terms_summary: "zizmor is MIT-licensed: free to use, including here, with no warranty and no obligation on GitRun's part beyond keeping this credit. GitRun does not modify zizmor or bundle its source — enabling this downloads the official zizmor binary via `cargo install` from crates.io the first time it's needed. Full license text at the link above.",
    }
}

/// Installs `zizmor` via `cargo install zizmor` if it isn't already on
/// `PATH`. Called by the dashboard's "enable" flow ONLY after the operator
/// has accepted the license/terms dialog (see
/// `Config::gsr_zizmor_license_accepted` — this function does not itself
/// check that flag; the dashboard command wrapping it is responsible for
/// gating on it, matching how every other GSR danger-gate check lives in
/// `Config::validate` rather than being re-implemented at each call site).
///
/// Already-installed is treated as success with no reinstall — this is an
/// idempotent "make sure it's available" call, not a forced upgrade.
pub fn ensure_zizmor_installed() -> std::io::Result<InstallOutcome> {
    if Command::new("zizmor")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
    {
        return Ok(InstallOutcome::AlreadyInstalled);
    }
    let output = Command::new("cargo")
        .arg("install")
        .arg("--locked")
        .arg("--version")
        .arg("1.30.1")
        .arg("zizmor")
        .output()?;
    if output.status.success() {
        Ok(InstallOutcome::Installed)
    } else {
        Ok(InstallOutcome::Failed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "detail")]
pub enum InstallOutcome {
    AlreadyInstalled,
    Installed,
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_gitdockrun_job_reference() {
        let workflow =
            "jobs:\n  build:\n    steps:\n      - run: GitDockRun --job build-cache --connect\n";
        let requests = scan_dock_requests("ci.yml", workflow);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].target_job, "build-cache");
        assert_eq!(requests[0].calling_job.as_deref(), Some("build"));
        assert_eq!(requests[0].operation, DockOperation::Connect);
    }
    #[test]
    fn flags_pull_request_target_with_head_checkout() {
        let workflow = "on: pull_request_target\njobs:\n  build:\n    steps:\n      - uses: actions/checkout@v4\n        with:\n          ref: ${{ github.event.pull_request.head.sha }}\n";
        let findings = scan("ci.yml", workflow);
        assert!(findings
            .iter()
            .any(|f| f.rule == "pull_request_target-with-head-checkout"));
    }

    #[test]
    fn does_not_flag_pull_request_target_alone() {
        let workflow =
            "on: pull_request_target\njobs:\n  build:\n    steps:\n      - run: echo hello\n";
        let findings = scan("ci.yml", workflow);
        assert!(findings.is_empty());
    }

    #[test]
    fn flags_unsecure_commands_opt_out() {
        let workflow = "env:\n  ACTIONS_ALLOW_UNSECURE_COMMANDS: true\n";
        let findings = scan("ci.yml", workflow);
        assert!(findings
            .iter()
            .any(|f| f.rule == "unsecure-commands-enabled"));
    }

    #[test]
    fn flags_template_injection_from_issue_title() {
        let workflow =
            "jobs:\n  build:\n    steps:\n      - run: echo \"${{ github.event.issue.title }}\"\n";
        let findings = scan("ci.yml", workflow);
        assert!(findings.iter().any(|f| f.rule == "template-injection-risk"));
    }

    #[test]
    fn flags_template_injection_inside_multiline_run_block() {
        let workflow = "jobs:\n  build:\n    steps:\n      - run: |\n          echo \"${{ github.event.issue.title }}\"\n";
        let findings = scan("ci.yml", workflow);
        assert!(findings
            .iter()
            .any(|f| f.rule == "template-injection-risk" && f.line == Some(5)));
    }

    #[test]
    fn does_not_scan_expressions_outside_run_blocks() {
        let workflow = "env:\n  TITLE: \"${{ github.event.issue.title }}\"\njobs:\n  build:\n    steps:\n      - run: echo safe\n";
        assert!(scan("ci.yml", workflow).is_empty());
    }

    #[test]
    fn does_not_flag_safe_expression_contexts() {
        let workflow = "jobs:\n  build:\n    steps:\n      - run: echo \"${{ matrix.os }}\"\n";
        let findings = scan("ci.yml", workflow);
        assert!(findings.is_empty());
    }

    #[test]
    fn clean_workflow_produces_no_findings() {
        let workflow = "name: CI\non:\n  push:\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: cargo build\n";
        assert!(scan("ci.yml", workflow).is_empty());
    }

    #[test]
    fn validate_workflows_dir_on_missing_dir_is_empty_not_an_error() {
        let dir =
            std::env::temp_dir().join(format!("gitrun-workflows-missing-{}", std::process::id()));
        let report = validate_workflows_dir(&dir).unwrap();
        assert!(report.is_clean());
    }

    #[test]
    fn validate_workflows_dir_scans_yml_files() {
        let dir = std::env::temp_dir().join(format!("gitrun-workflows-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("risky.yml"),
            "env:\n  ACTIONS_ALLOW_UNSECURE_COMMANDS: true\n",
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "not a workflow").unwrap();
        let report = validate_workflows_dir(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].file, "risky.yml");
    }

    #[test]
    fn run_zizmor_missing_binary_is_none_not_an_error() {
        // In this sandbox zizmor is not installed - this exercises the
        // exact "optional enhancement, absence is fine" path described in
        // the module docs. If zizmor ever IS present in a future CI/dev
        // environment for this repo, this assertion would need updating
        // to accept Some(_) too; documented here rather than silently
        // becoming a flaky test.
        let dir = std::env::temp_dir();
        let result =
            run_zizmor_command("__gitrun_zizmor_binary_that_should_not_exist__", &dir).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn zizmor_info_has_required_credit_fields() {
        let info = zizmor_info();
        assert_eq!(info.license_name, "MIT License");
        assert!(info.repository.contains("github.com"));
        assert!(!info.author.is_empty());
        assert!(!info.terms_summary.is_empty());
    }
}
