//! Authentication source selection shared by GitRun frontends and services.

use crate::{app_auth::AppAuth, Config, ConfigError};
use std::{fs, path::Path};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitHubAuthError {
    #[error("GITHUB_TOKEN is not configured and no GitHub App authentication is configured")]
    MissingCredentials,
    #[error("unable to read GitHub App private key at {path}: {source}")]
    PrivateKeyIo { path: String, source: std::io::Error },
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
            )?));
        }

        match std::env::var("GITHUB_TOKEN") {
            Ok(token) if !token.trim().is_empty() => Ok(Self::Pat(token)),
            _ => Err(GitHubAuthError::MissingCredentials),
        }
    }

    pub fn from_config_file(config: &Config, path: &Path) -> Result<Self, GitHubAuthError> {
        if config.uses_github_app() {
            return Self::from_config(config);
        }
        if let Ok(token) = read_env_file_value(path, "GITHUB_TOKEN") {
            if !token.trim().is_empty() {
                return Ok(Self::Pat(token));
            }
        }
        Self::from_config(config)
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
    let content = fs::read_to_string(path).map_err(|source| GitHubAuthError::PrivateKeyIo {
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
