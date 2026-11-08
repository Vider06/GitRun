//! Authentication source selection shared by GitRun frontends and services.

#[cfg(not(any(kani, verus)))]
use crate::{app_auth::AppAuth, Config, ConfigError};
#[cfg(not(any(kani, verus)))]
use std::{fs, path::Path};
#[cfg(not(any(kani, verus)))]
use thiserror::Error;

#[derive(Debug, Error)]
#[cfg(not(any(kani, verus)))]
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
    #[error("authentication env file contains more than one {key} entry")]
    DuplicateEnvKey { key: String },
    #[error(transparent)]
    App(#[from] crate::app_auth::AppAuthError),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

#[cfg(not(any(kani, verus)))]
pub enum GitHubAuth {
    Pat(String),
    App(AppAuth),
}

#[cfg(not(any(kani, verus)))]
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

#[cfg(not(any(kani, verus)))]
fn read_env_file_value(path: &Path, wanted_key: &str) -> Result<String, GitHubAuthError> {
    let content = fs::read_to_string(path).map_err(|source| GitHubAuthError::EnvFileIo {
        path: path.display().to_string(),
        source,
    })?;
    let mut found: Option<String> = None;
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == wanted_key {
            if found.is_some() {
                return Err(GitHubAuthError::DuplicateEnvKey {
                    key: wanted_key.to_owned(),
                });
            }
            found = Some(unquote(value.trim()));
        }
    }
    found.ok_or(GitHubAuthError::MissingCredentials)
}

#[cfg_attr(verus, verus_verify(dual_spec, open))]
#[cfg_attr(
    verus,
    verus_spec(
        returns (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'')
    )
)]
fn has_matching_quote_bytes(first: u8, last: u8) -> bool {
    (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'')
}

fn unquote(value: &str) -> String {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if has_matching_quote_bytes(bytes[0], bytes[value.len() - 1]) {
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
    fn env_file_rejects_duplicate_token_entries() {
        let path = temp_env_file("GITHUB_TOKEN=first\nGITHUB_TOKEN=second\n");
        let result = read_env_file_value(&path, "GITHUB_TOKEN");
        fs::remove_file(path).expect("remove temporary env file");
        assert!(matches!(
            result,
            Err(GitHubAuthError::DuplicateEnvKey { key }) if key == "GITHUB_TOKEN"
        ));
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

#[cfg(kani)]
mod kani_verification {
    use super::{has_matching_quote_bytes, unquote};

    #[kani::proof]
    fn matching_quote_bytes_is_exact_over_all_byte_pairs() {
        let first: u8 = kani::any();
        let last: u8 = kani::any();
        let expected = (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'');
        assert_eq!(has_matching_quote_bytes(first, last), expected);
    }

    #[kani::proof]
    fn env_token_unquoting_preserves_unmatched_and_unquoted_values() {
        assert!(unquote("\"token\"") == "token");
        assert!(unquote("'token'") == "token");
        assert!(unquote("\"token'") == "\"token'");
        assert!(unquote("'token\"") == "'token\"");
        assert!(unquote("token") == "token");
        assert!(unquote("\"\"") == "");
    }
}

#[cfg(verus)]
mod verus_verification {
    use super::has_matching_quote_bytes;
    use vstd::prelude::*;

    verus! {
        proof fn matching_quote_bytes_is_exact_over_all_byte_pairs(first: u8, last: u8)
            ensures
                has_matching_quote_bytes(first, last)
                    == ((first == b'"' && last == b'"') || (first == b'\'' && last == b'\\''))
        {
        }
    }
}

#[cfg(kani)]
fn main() {}

#[cfg(verus)]
fn main() {}
