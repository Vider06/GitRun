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
//! Windows support: a `Backend::Vm` target names a persistent VM whose
//! Docker daemon is addressed through the VM resolution layer (rather than
//! a raw address), allowing the VM's IP to change across restarts.
//!//! VirtualBox VM — see `vm.rs` for the VM lifecycle side of this, which
//! creates that one shared VM ahead of time rather than one per runner).
//! Containers are still the unit of scaling: many Windows *containers* run
//! inside that one VM, exactly like Linux containers run on the bare host.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

static RULES_TMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
pub fn resolve<'a>(
    rules: &'a [LogicRule],
    job_labels: &[String],
) -> Option<(&'a Backend, &'a str)> {
    let normalized: Vec<String> = job_labels
        .iter()
        .map(|label| label.to_ascii_lowercase())
        .collect();

    rules
        .iter()
        .find(|rule| {
            !rule.match_labels.is_empty()
                && rule.match_labels.iter().all(|required| {
                    let required = required.to_ascii_lowercase();
                    normalized.iter().any(|label| label == &required)
                })
        })
        .map(|rule| (&rule.backend, rule.image.as_str()))
}

pub fn validate_rules(rules: &[LogicRule]) -> Result<(), LogicRulesError> {
    for (index, rule) in rules.iter().enumerate() {
        if rule.name.trim().is_empty() {
            return Err(LogicRulesError::Invalid(format!(
                "rule {index} has an empty name"
            )));
        }
        if rule.name.chars().any(char::is_control) {
            return Err(LogicRulesError::Invalid(format!(
                "rule {index} has a name containing control characters"
            )));
        }
        if rule.match_labels.is_empty() {
            return Err(LogicRulesError::Invalid(format!(
                "rule '{}' must require at least one label",
                rule.name
            )));
        }

        for label in &rule.match_labels {
            if label.trim().is_empty() {
                return Err(LogicRulesError::Invalid(format!(
                    "rule '{}' contains an empty label",
                    rule.name
                )));
            }
            if label.chars().any(char::is_control) {
                return Err(LogicRulesError::Invalid(format!(
                    "rule '{}' contains a label with control characters",
                    rule.name
                )));
            }
        }

        match &rule.backend {
            Backend::LocalLinux => {}
            Backend::Vm { vm_name } => {
                if vm_name.trim().is_empty() {
                    return Err(LogicRulesError::Invalid(format!(
                        "rule '{}' targets a VM with an empty name",
                        rule.name
                    )));
                }
                if vm_name.chars().any(char::is_control) {
                    return Err(LogicRulesError::Invalid(format!(
                        "rule '{}' targets a VM with control characters in its name",
                        rule.name
                    )));
                }
            }
        }

        if rule.image.trim().is_empty() {
            return Err(LogicRulesError::Invalid(format!(
                "rule '{}' has an empty image",
                rule.name
            )));
        }
        if rule.image.chars().any(char::is_whitespace) {
            return Err(LogicRulesError::Invalid(format!(
                "rule '{}' has an image containing whitespace",
                rule.name
            )));
        }
        if rule.image.chars().any(char::is_control) {
            return Err(LogicRulesError::Invalid(format!(
                "rule '{}' has an image containing control characters",
                rule.name
            )));
        }
    }

    for (index, left) in rules.iter().enumerate() {
        for right in rules.iter().skip(index + 1) {
            if left.name == right.name {
                return Err(LogicRulesError::Invalid(format!(
                    "duplicate Logic Containers rule name '{}'",
                    left.name
                )));
            }
        }
    }

    Ok(())
}

/// Loads rules from a JSON file (dashboard writes/reads the same format).
/// Missing file means "no rules configured yet" — not an error, since a
/// fresh GitRun install has none and should fall back to default behavior.
pub fn load_rules(path: &std::path::Path) -> Result<Vec<LogicRule>, LogicRulesError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let rules: Vec<LogicRule> = serde_json::from_str(&raw)?;
            validate_rules(&rules)?;
            Ok(rules)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub fn save_rules(path: &std::path::Path, rules: &[LogicRule]) -> Result<(), LogicRulesError> {
    validate_rules(rules)?;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;

    let sequence = RULES_TMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| LogicRulesError::Invalid("rules path has no valid file name".into()))?;
    let tmp = parent.join(format!(
        ".{file_name}.tmp-{}-{sequence}",
        std::process::id()
    ));

    let serialized = serde_json::to_string_pretty(rules)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    use std::io::Write;
    file.write_all(serialized.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);

    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }

    #[cfg(unix)]
    {
        let directory = std::fs::File::open(parent)?;
        directory.sync_all()?;
    }

    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum LogicRulesError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid Logic Containers rules file: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("invalid Logic Containers rule: {0}")]
    Invalid(String),
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
        let rules = vec![rule(
            "windows-jobs",
            &["windows"],
            Backend::Vm {
                vm_name: "win-host".into(),
            },
            "gitrun-runner:windows",
        )];
        let result = resolve(&rules, &["self-hosted".into(), "windows".into()]);
        assert_eq!(
            result,
            Some((
                &Backend::Vm {
                    vm_name: "win-host".into()
                },
                "gitrun-runner:windows"
            ))
        );
    }

    #[test]
    fn does_not_match_when_a_required_label_is_missing() {
        let rules = vec![rule(
            "gpu-jobs",
            &["gpu", "cuda"],
            Backend::LocalLinux,
            "gitrun-runner:cuda",
        )];
        // Has "gpu" but not "cuda" — rule requires both.
        assert_eq!(resolve(&rules, &["self-hosted".into(), "gpu".into()]), None);
    }

    #[test]
    fn matching_is_case_insensitive() {
        let rules = vec![rule(
            "windows-jobs",
            &["Windows"],
            Backend::Vm {
                vm_name: "win-host".into(),
            },
            "gitrun-runner:windows",
        )];
        let result = resolve(&rules, &["self-hosted".into(), "WINDOWS".into()]);
        assert!(result.is_some());
    }

    #[test]
    fn first_matching_rule_wins() {
        let rules = vec![
            rule(
                "catch-all-linux",
                &["self-hosted"],
                Backend::LocalLinux,
                "gitrun-runner:default",
            ),
            rule(
                "windows-jobs",
                &["windows"],
                Backend::Vm {
                    vm_name: "win-host".into(),
                },
                "gitrun-runner:windows",
            ),
        ];
        // Both rules could match; the first one in the list wins even though
        // a later, more specific rule also applies.
        let result = resolve(&rules, &["self-hosted".into(), "windows".into()]);
        assert_eq!(
            result,
            Some((&Backend::LocalLinux, "gitrun-runner:default"))
        );
    }

    #[test]
    fn invalid_rules_are_rejected() {
        let rules = vec![rule(
            "",
            &["windows"],
            Backend::Vm {
                vm_name: "".into(),
            },
            "",
        )];
        assert!(matches!(
            validate_rules(&rules),
            Err(LogicRulesError::Invalid(_))
        ));
    }

    #[test]
    fn duplicate_rule_names_are_rejected() {
        let rules = vec![
            rule("same", &["windows"], Backend::LocalLinux, "image:one"),
            rule("same", &["linux"], Backend::LocalLinux, "image:two"),
        ];
        assert!(matches!(
            validate_rules(&rules),
            Err(LogicRulesError::Invalid(_))
        ));
    }

    #[test]
    fn invalid_rules_cannot_be_saved() {
        let dir = std::env::temp_dir().join(format!(
            "gitrun-logic-rules-invalid-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("logic-containers.json");
        let rules = vec![rule("", &["windows"], Backend::LocalLinux, "image:latest")];
        assert!(matches!(
            save_rules(&path, &rules),
            Err(LogicRulesError::Invalid(_))
        ));
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rule_with_no_labels_is_never_matched() {
        // An empty match_labels list is treated as misconfigured, not "always
        // match" — an operator wanting a default should configure
        // Config::runner_image, not a labelless Logic Containers rule.
        let rules = vec![rule(
            "broken-rule",
            &[],
            Backend::LocalLinux,
            "gitrun-runner:should-not-apply",
        )];
        assert_eq!(resolve(&rules, &["self-hosted".into()]), None);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir =
            std::env::temp_dir().join(format!("gitrun-logic-rules-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("logic-containers.json");
        let rules = vec![rule(
            "windows-jobs",
            &["windows"],
            Backend::Vm {
                vm_name: "win-host".into(),
            },
            "gitrun-runner:windows",
        )];
        save_rules(&path, &rules).unwrap();
        let loaded = load_rules(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "windows-jobs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_loads_as_empty_not_error() {
        let path = std::env::temp_dir().join(format!(
            "gitrun-logic-rules-missing-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let loaded = load_rules(&path).unwrap();
        assert!(loaded.is_empty());
    }
}
