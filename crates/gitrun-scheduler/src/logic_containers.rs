//! Logic Containers: rules that decide which Docker backend and image a
//! *dynamic* runner should use, based on the queued job's labels.
//!
//! Scope as designed with the operator: permanent runners are configured
//! individually by hand (their own "gear icon" settings in the dashboard —
//! not modeled here, that's dashboard-side state). This module governs only
//! dynamic (autoscaled) runners: a small ordered list of rules, each saying
//! "if the job's labels match this, use this backend + image". First match
//! wins; no match falls back to the existing default (local Docker host,
//! `config.runner_image` — today's only behavior, unchanged for anyone who
//! never touches Logic Containers).
//!
//! Windows support: a `Backend::Windows` target names a *host* (an IP/DNS
//! name for a Docker daemon expected to be running inside a persistent
//! VirtualBox VM — see `vm.rs` for the VM lifecycle side of this, which
//! creates that one shared VM ahead of time rather than one per runner).
//! Containers are still the unit of scaling: many Windows *containers* run
//! inside that one VM, exactly like Linux containers run on the bare host.

use serde::{Deserialize, Serialize};

/// Where a matched rule's containers should run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    /// The host GitRun itself runs on (today's only behavior).
    LocalLinux,
    /// A Docker daemon inside a specific VM, addressed by the VM's name (see
    /// `vm.rs::VmConfig::name`) rather than a raw address, so the rule
    /// survives the VM's IP changing across restarts.
    Vm { vm_name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogicRule {
    /// Human-readable name shown in the dashboard, purely descriptive.
    pub name: String,
    /// Labels that must ALL be present on the job (case-insensitive) for
    /// this rule to match — e.g. `["windows"]` or `["gpu", "cuda"]`. An
    /// empty list never matches (an operator meaning "always" should instead
    /// just set the default backend/image in `Config`, not a Logic Containers
    /// rule with no conditions, so an empty-label rule is treated as
    /// misconfigured and skipped rather than silently matching everything).
    pub match_labels: Vec<String>,
    pub backend: Backend,
    pub image: String,
}

/// Evaluates rules in order, returning the first match's (backend, image).
/// `job_labels` should be the same labels already fetched from GitHub's jobs
/// API (see `github.rs`'s `Job.labels`, used today only to filter for
/// `self-hosted`).
pub fn resolve<'a>(rules: &'a [LogicRule], job_labels: &[String]) -> Option<(&'a Backend, &'a str)> {
    let normalized: Vec<String> = job_labels.iter().map(|l| l.to_ascii_lowercase()).collect();
    rules
        .iter()
        .filter(|rule| !rule.match_labels.is_empty())
        .find(|rule| {
            rule.match_labels
                .iter()
                .all(|required| normalized.iter().any(|label| label == &required.to_ascii_lowercase()))
        })
        .map(|rule| (&rule.backend, rule.image.as_str()))
}

/// Loads rules from a JSON file (dashboard writes/reads the same format).
/// Missing file means "no rules configured yet" — not an error, since a
/// fresh GitRun install has none and should fall back to default behavior.
pub fn load_rules(path: &std::path::Path) -> Result<Vec<LogicRule>, LogicRulesError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(serde_json::from_str(&raw)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub fn save_rules(path: &std::path::Path, rules: &[LogicRule]) -> Result<(), LogicRulesError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(rules)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum LogicRulesError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid Logic Containers rules file: {0}")]
    Decode(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(name: &str, labels: &[&str], backend: Backend, image: &str) -> LogicRule {
        LogicRule {
            name: name.to_owned(),
            match_labels: labels.iter().map(|s| s.to_string()).collect(),
            backend,
            image: image.to_owned(),
        }
    }

    #[test]
    fn no_rules_means_no_match() {
        assert_eq!(resolve(&[], &["self-hosted".into()]), None);
    }

    #[test]
    fn matches_when_all_required_labels_present() {
        let rules = vec![rule("windows-jobs", &["windows"], Backend::Vm { vm_name: "win-host".into() }, "gitrun-runner:windows")];
        let result = resolve(&rules, &["self-hosted".into(), "windows".into()]);
        assert_eq!(result, Some((&Backend::Vm { vm_name: "win-host".into() }, "gitrun-runner:windows")));
    }

    #[test]
    fn does_not_match_when_a_required_label_is_missing() {
        let rules = vec![rule("gpu-jobs", &["gpu", "cuda"], Backend::LocalLinux, "gitrun-runner:cuda")];
        // Has "gpu" but not "cuda" — rule requires both.
        assert_eq!(resolve(&rules, &["self-hosted".into(), "gpu".into()]), None);
    }

    #[test]
    fn matching_is_case_insensitive() {
        let rules = vec![rule("windows-jobs", &["Windows"], Backend::Vm { vm_name: "win-host".into() }, "gitrun-runner:windows")];
        let result = resolve(&rules, &["self-hosted".into(), "WINDOWS".into()]);
        assert!(result.is_some());
    }

    #[test]
    fn first_matching_rule_wins() {
        let rules = vec![
            rule("catch-all-linux", &["self-hosted"], Backend::LocalLinux, "gitrun-runner:default"),
            rule("windows-jobs", &["windows"], Backend::Vm { vm_name: "win-host".into() }, "gitrun-runner:windows"),
        ];
        // Both rules could match; the first one in the list wins even though
        // a later, more specific rule also applies.
        let result = resolve(&rules, &["self-hosted".into(), "windows".into()]);
        assert_eq!(result, Some((&Backend::LocalLinux, "gitrun-runner:default")));
    }

    #[test]
    fn rule_with_no_labels_is_never_matched() {
        // An empty match_labels list is treated as misconfigured, not "always
        // match" — an operator wanting a default should configure
        // Config::runner_image, not a labelless Logic Containers rule.
        let rules = vec![rule("broken-rule", &[], Backend::LocalLinux, "gitrun-runner:should-not-apply")];
        assert_eq!(resolve(&rules, &["self-hosted".into()]), None);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("gitrun-logic-rules-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("logic-containers.json");
        let rules = vec![rule("windows-jobs", &["windows"], Backend::Vm { vm_name: "win-host".into() }, "gitrun-runner:windows")];
        save_rules(&path, &rules).unwrap();
        let loaded = load_rules(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "windows-jobs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_loads_as_empty_not_error() {
        let path = std::env::temp_dir().join(format!("gitrun-logic-rules-missing-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let loaded = load_rules(&path).unwrap();
        assert!(loaded.is_empty());
    }
}
