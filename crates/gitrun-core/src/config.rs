use serde::{Deserialize, Serialize};
use std::{env, fs, path::Path};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("unable to read configuration: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid integer for {key}: {value}")]
    Integer { key: String, value: String },
    #[error("invalid repository: {0}")]
    Repository(String),
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            repositories: Vec::new(), min_runners: 3, max_runners: 8,
            idle_timeout: 120, poll_interval: 5,
            runner_image: "gitrun-runner:latest".into(),
            runner_labels: "self-hosted,Linux,X64".into(), ephemeral: false,
            state_dir: "/var/lib/gitrun".into(), log_dir: "/var/log/gitrun".into(),
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut c = Self::default();
        if let Ok(raw) = env::var("GITRUN_REPOSITORIES") {
            c.repositories = parse_repositories(&raw)?;
        }
        c.min_runners = env_u32("GITRUN_MIN_RUNNERS", c.min_runners)?;
        c.max_runners = env_u32("GITRUN_MAX_RUNNERS", c.max_runners)?;
        c.idle_timeout = env_u64("GITRUN_IDLE_TIMEOUT", c.idle_timeout)?;
        c.poll_interval = env_u64("GITRUN_POLL_INTERVAL", c.poll_interval)?;
        c.runner_image = env::var("GITRUN_RUNNER_IMAGE").unwrap_or(c.runner_image);
        c.runner_labels = env::var("GITRUN_RUNNER_LABELS").unwrap_or(c.runner_labels);
        c.ephemeral = env_bool("GITRUN_EPHEMERAL", c.ephemeral);
        c.state_dir = env::var("GITRUN_STATE_DIR").unwrap_or(c.state_dir);
        c.log_dir = env::var("GITRUN_LOG_DIR").unwrap_or(c.log_dir);
        c.validate()?;
        Ok(c)
    }

    pub fn from_env_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path)?;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            if let Some((key, value)) = line.split_once('=') { env::set_var(key.trim(), value.trim()); }
        }
        Self::from_env()
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_runners < self.min_runners || self.min_runners == 0 {
            return Err(ConfigError::Integer { key: "GITRUN_MIN/MAX_RUNNERS".into(), value: format!("{}..{}", self.min_runners, self.max_runners) });
        }
        for repo in &self.repositories {
            if !is_repository(repo) { return Err(ConfigError::Repository(repo.clone())); }
        }
        Ok(())
    }
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
fn env_u32(key: &str, default: u32) -> Result<u32, ConfigError> {
    match env::var(key) { Ok(v) => v.parse().map_err(|_| ConfigError::Integer { key: key.into(), value: v }), Err(_) => Ok(default) }
}
fn env_u64(key: &str, default: u64) -> Result<u64, ConfigError> {
    match env::var(key) { Ok(v) => v.parse().map_err(|_| ConfigError::Integer { key: key.into(), value: v }), Err(_) => Ok(default) }
}
fn env_bool(key: &str, default: bool) -> bool {
    env::var(key).map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repository_validation_is_strict() {
        assert!(is_repository("owner/repo"));
        assert!(!is_repository("owner"));
        assert!(!is_repository("owner/repo/extra"));
    }
}
