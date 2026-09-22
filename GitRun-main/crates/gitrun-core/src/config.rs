use serde::{Deserialize, Serialize};
use std::{env, fs, path::Path};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("unable to read configuration: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid integer for {key}: {value}")]
    Integer { key: String, value: String },
    #[error("invalid boolean for {key}: {value}")]
    Boolean { key: String, value: String },
    #[error("invalid repository: {0}")]
    Repository(String),
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub repositories: Vec<String>,
    pub min_runners: u32,
    pub max_runners: u32,
    pub idle_timeout: u64,
    pub poll_interval: u64,
    pub runner_image: String,
    pub runner_labels: String,
    pub ephemeral: bool,
    pub state_dir: String,
    pub log_dir: String,
    pub auto_container_update: bool,
    pub container_update_time: String,
    pub auto_container_recovery: bool,
    pub container_recovery_cooldown: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            repositories: Vec::new(), min_runners: 3, max_runners: 8,
            idle_timeout: 120, poll_interval: 5,
            runner_image: "gitrun-runner:latest".into(),
            runner_labels: "self-hosted,Linux,X64".into(), ephemeral: false,
            state_dir: "/var/lib/gitrun".into(), log_dir: "/var/log/gitrun".into(),
            auto_container_update: false, container_update_time: "03:00".into(),
            auto_container_recovery: true, container_recovery_cooldown: 60,
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let lookup: std::collections::HashMap<String, String> = [
            "GITRUN_REPOSITORIES",
            "GITRUN_MIN_RUNNERS",
            "GITRUN_MAX_RUNNERS",
            "GITRUN_IDLE_TIMEOUT",
            "GITRUN_POLL_INTERVAL",
            "GITRUN_RUNNER_IMAGE",
            "GITRUN_RUNNER_LABELS",
            "GITRUN_EPHEMERAL",
            "GITRUN_STATE_DIR",
            "GITRUN_LOG_DIR",
            "GITRUN_AUTO_CONTAINER_UPDATE",
            "GITRUN_CONTAINER_UPDATE_TIME",
            "GITRUN_AUTO_CONTAINER_RECOVERY",
            "GITRUN_CONTAINER_RECOVERY_COOLDOWN",
        ]
        .into_iter()
        .filter_map(|key| env::var(key).ok().map(|value| (key.to_owned(), value)))
        .collect();
        Self::from_lookup(&lookup)
    }

    pub fn from_env_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path)?;
        let mut lookup = std::collections::HashMap::new();
        for raw in content.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let Some((key, value)) = line.split_once('=') else {
                return Err(ConfigError::Invalid(format!("invalid env line: {line}")));
            };
            lookup.insert(key.trim().to_owned(), unquote(value.trim()));
        }
        Self::from_lookup(&lookup)
    }

    /// Shared field-by-field build, driven by a plain string lookup. Both
    /// `from_env` (process environment) and `from_env_file` (a `.env`-style
    /// file) funnel into this single place so the 14 config fields are only
    /// ever enumerated once instead of twice in lockstep.
    fn from_lookup(lookup: &std::collections::HashMap<String, String>) -> Result<Self, ConfigError> {
        let get = |key: &str| lookup.get(key).cloned();
        let mut c = Self::default();
        if let Some(raw) = get("GITRUN_REPOSITORIES") { c.repositories = parse_repositories(&raw)?; }
        c.min_runners = value_u32(&get("GITRUN_MIN_RUNNERS"), "GITRUN_MIN_RUNNERS", c.min_runners)?;
        c.max_runners = value_u32(&get("GITRUN_MAX_RUNNERS"), "GITRUN_MAX_RUNNERS", c.max_runners)?;
        c.idle_timeout = value_u64(&get("GITRUN_IDLE_TIMEOUT"), "GITRUN_IDLE_TIMEOUT", c.idle_timeout)?;
        c.poll_interval = value_u64(&get("GITRUN_POLL_INTERVAL"), "GITRUN_POLL_INTERVAL", c.poll_interval)?;
        if let Some(v) = get("GITRUN_RUNNER_IMAGE") { c.runner_image = v; }
        if let Some(v) = get("GITRUN_RUNNER_LABELS") { c.runner_labels = v; }
        if let Some(v) = get("GITRUN_EPHEMERAL") { c.ephemeral = parse_bool("GITRUN_EPHEMERAL", &v)?; }
        if let Some(v) = get("GITRUN_STATE_DIR") { c.state_dir = v; }
        if let Some(v) = get("GITRUN_LOG_DIR") { c.log_dir = v; }
        if let Some(v) = get("GITRUN_AUTO_CONTAINER_UPDATE") { c.auto_container_update = parse_bool("GITRUN_AUTO_CONTAINER_UPDATE", &v)?; }
        if let Some(v) = get("GITRUN_CONTAINER_UPDATE_TIME") { c.container_update_time = v; }
        if let Some(v) = get("GITRUN_AUTO_CONTAINER_RECOVERY") { c.auto_container_recovery = parse_bool("GITRUN_AUTO_CONTAINER_RECOVERY", &v)?; }
        c.container_recovery_cooldown = value_u64(&get("GITRUN_CONTAINER_RECOVERY_COOLDOWN"), "GITRUN_CONTAINER_RECOVERY_COOLDOWN", c.container_recovery_cooldown)?;
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.min_runners == 0 || self.max_runners < self.min_runners {
            return Err(ConfigError::Invalid(format!("runner bounds are invalid: {}..{}", self.min_runners, self.max_runners)));
        }
        if self.poll_interval == 0 { return Err(ConfigError::Invalid("poll interval must be greater than zero".into())); }
        if self.container_recovery_cooldown == 0 { return Err(ConfigError::Invalid("container recovery cooldown must be greater than zero".into())); }
        if self.runner_image.trim().is_empty() { return Err(ConfigError::Invalid("runner image must not be empty".into())); }
        if self.runner_labels.trim().is_empty() { return Err(ConfigError::Invalid("runner labels must not be empty".into())); }
        validate_time(&self.container_update_time)?;
        for repo in &self.repositories {
            if !is_repository(repo) { return Err(ConfigError::Repository(repo.clone())); }
        }
        Ok(())
    }
}

fn validate_time(value: &str) -> Result<(), ConfigError> {
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || bytes[2] != b':'
        || !bytes[..2].iter().all(|byte| byte.is_ascii_digit())
        || !bytes[3..].iter().all(|byte| byte.is_ascii_digit())
    {
        return Err(ConfigError::Invalid(format!("invalid container update time: {value}")));
    }
    let hour = value[..2].parse::<u8>().unwrap_or(99);
    let minute = value[3..].parse::<u8>().unwrap_or(99);
    if hour > 23 || minute > 59 {
        return Err(ConfigError::Invalid(format!("invalid container update time: {value}")));
    }
    Ok(())
}

fn parse_repositories(raw: &str) -> Result<Vec<String>, ConfigError> {
    raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(|repo| {
        if is_repository(repo) { Ok(repo.to_owned()) } else { Err(ConfigError::Repository(repo.to_owned())) }
    }).collect()
}
fn is_repository(value: &str) -> bool {
    let mut parts = value.split('/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(a), Some(b), None) if !a.is_empty() && !b.is_empty())
}
fn value_u32(value: &Option<String>, key: &str, default: u32) -> Result<u32, ConfigError> {
    match value { Some(v) => v.parse().map_err(|_| ConfigError::Integer { key: key.into(), value: v.clone() }), None => Ok(default) }
}
fn value_u64(value: &Option<String>, key: &str, default: u64) -> Result<u64, ConfigError> {
    match value { Some(v) => v.parse().map_err(|_| ConfigError::Integer { key: key.into(), value: v.clone() }), None => Ok(default) }
}
fn parse_bool(key: &str, value: &str) -> Result<bool, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::Boolean { key: key.into(), value: value.into() }),
    }
}
fn unquote(value: &str) -> String {
    if value.len() >= 2 {
        let b = value.as_bytes();
        if (b[0] == b'"' && b[value.len()-1] == b'"') || (b[0] == b'\'' && b[value.len()-1] == b'\'') {
            return value[1..value.len()-1].to_owned();
        }
    }
    value.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::{SystemTime, UNIX_EPOCH}};
    #[test]
    fn repository_validation_is_strict() {
        assert!(is_repository("owner/repo"));
        assert!(!is_repository("owner"));
        assert!(!is_repository("owner/repo/extra"));
    }
    #[test]
    fn env_file_does_not_mutate_process_environment() {
        let path = std::env::temp_dir().join(format!("gitrun-config-{}.env", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::write(&path, "GITRUN_MIN_RUNNERS=2\nGITRUN_MAX_RUNNERS=4\nGITRUN_EPHEMERAL=true\nGITRUN_AUTO_CONTAINER_RECOVERY=false\nGITRUN_CONTAINER_RECOVERY_COOLDOWN=90\n").unwrap();
        let config = Config::from_env_file(&path).unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(config.min_runners, 2);
        assert_eq!(config.max_runners, 4);
        assert!(config.ephemeral);
        assert!(!config.auto_container_recovery);
        assert_eq!(config.container_recovery_cooldown, 90);
    }
    #[test]
    fn invalid_boolean_is_rejected() { assert!(parse_bool("TEST", "maybe").is_err()); }

    #[test]
    fn container_update_time_is_validated() {
        assert!(validate_time("03:00").is_ok());
        assert!(validate_time("23:59").is_ok());
        assert!(validate_time("24:00").is_err());
        assert!(validate_time("3:00").is_err());
    }
}
