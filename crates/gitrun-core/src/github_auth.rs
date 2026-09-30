//! Authentication source selection shared by GitRun frontends and services.

use crate::{app_auth::AppAuth, Config, ConfigError};
use std::{fs, path::Path};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitHubAuthError {
    #[error("GITHUB_TOKEN is not configured and no GitHub App authentication is configured")]
    MissingCredentials,
    #[error("unable to read GitHub App private key at {path}: {source}")]
    PrivateKeyIo {
        path: String,
        source: std::io::Error,
    },
    #[error("unable to read authentication env file at {path}: {source}")]
    EnvFileIo {
        path: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    App(#[from] crate::app_auth::AppAuthError),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

pub enum GitHubAuth {
    Pat(String),
    App(AppAuth),
}

impl GitHubAuth {
    pub fn from_config(config: &Config) -> Result<Self, GitHubAuthError> {
        config.validate()?;
        if config.uses_github_app() {
            let path = config.github_app_private_key_path.clone();
            let private_key = fs::read_to_string(&path)
                .map_err(|source| GitHubAuthError::PrivateKeyIo { path, source })?;
            return Ok(Self::App(AppAuth::new(
                &config.github_app_id,
                &config.github_app_installation_id,
                &private_key,
                std::time::Duration::from_secs(config.github_connect_timeout),
                std::time::Duration::from_secs(config.github_request_timeout),
            )?));
        }

        match std::env::var("GITHUB_TOKEN") {
            Ok(token) if !token.trim().is_empty() => Ok(Self::Pat(token.trim().to_owned())),
            _ => Err(GitHubAuthError::MissingCredentials),
        }
    }

    pub fn from_config_file(config: &Config, path: &Path) -> Result<Self, GitHubAuthError> {
        if config.uses_github_app() {
            return Self::from_config(config);
        }

        match read_env_file_value(path, "GITHUB_TOKEN") {
            Ok(token) if !token.trim().is_empty() => Ok(Self::Pat(token.trim().to_owned())),
            Ok(_) | Err(GitHubAuthError::MissingCredentials) => Self::from_config(config),
            Err(error) => Err(error),
        }
    }

    pub fn bearer_token(&self) -> Result<String, GitHubAuthError> {
        match self {
            Self::Pat(token) => Ok(token.clone()),
            Self::App(app) => Ok(app.token()?),
        }
    }

    pub fn is_app(&self) -> bool {
        matches!(self, Self::App(_))
    }
}

fn read_env_file_value(path: &Path, wanted_key: &str) -> Result<String, GitHubAuthError> {
    let content = fs::read_to_string(path).map_err(|source| GitHubAuthError::EnvFileIo {
        path: path.display().to_string(),
        source,
    })?;
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == wanted_key {
            return Ok(unquote(value.trim()));
        }
    }
    Err(GitHubAuthError::MissingCredentials)
}

fn unquote(value: &str) -> String {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes[0];
        let last = bytes[value.len() - 1];
        if (first == 34 && last == 34) || (first == 39 && last == 39) {
            return value[1..value.len() - 1].to_owned();
        }
    }
    value.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_env_file(contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "gitrun-github-auth-{}-{}.env",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(|c: char| !c.is_ascii_alphanumeric(), "_")
        ));
        fs::write(&path, contents).expect("write temporary env file");
        path
    }

    #[test]
    fn env_file_reads_and_unquotes_token() {
        let path = temp_env_file("# comment\nGITHUB_TOKEN = \" token \"\nOTHER=value\n");
        let token = read_env_file_value(&path, "GITHUB_TOKEN").expect("token");
        fs::remove_file(path).expect("remove temporary env file");
        assert_eq!(token, " token ");
    }

    #[test]
    fn env_file_missing_token_returns_missing_credentials() {
        let path = temp_env_file("OTHER=value\n");
        let result = read_env_file_value(&path, "GITHUB_TOKEN");
        fs::remove_file(path).expect("remove temporary env file");
        assert!(matches!(result, Err(GitHubAuthError::MissingCredentials)));
    }

    #[test]
    fn env_file_io_errors_are_distinct() {
        let path = std::env::temp_dir().join(format!(
            "gitrun-github-auth-missing-{}.env",
            std::process::id()
        ));
        let result = read_env_file_value(&path, "GITHUB_TOKEN");
        assert!(matches!(result, Err(GitHubAuthError::EnvFileIo { .. })));
    }

    #[test]
    fn env_file_token_is_normalized_by_auth_selection() {
        let path = temp_env_file("GITHUB_TOKEN = \" token \"\n");
        let config = Config::default();
        let auth = GitHubAuth::from_config_file(&config, &path).expect("auth");
        fs::remove_file(path).expect("remove temporary env file");
        assert!(matches!(auth, GitHubAuth::Pat(token) if token == "token"));
    }
}
