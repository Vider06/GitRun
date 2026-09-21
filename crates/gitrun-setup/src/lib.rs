use gitrun_core::Config;
use std::{path::{Path, PathBuf}, process::Command};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyStatus {
    pub name: &'static str,
    pub available: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupReport {
    pub dependencies: Vec<DependencyStatus>,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub log_dir: PathBuf,
}

#[derive(Debug, Error)]
pub enum SetupError {
    #[error("failed to create setup directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid GitRun configuration: {0}")]
    Config(#[from] gitrun_core::ConfigError),
    #[error("configuration directory must not be a file: {0}")]
    InvalidConfigDir(PathBuf),
    #[error("state/log directories must not be files")]
    InvalidRuntimeDir,
}

pub fn check_dependencies() -> Vec<DependencyStatus> {
    vec![
        command_status("docker", &["--version"]),
        docker_daemon_status(),
        compose_status(),
        command_status("git", &["--version"]),
    ]
}

pub fn prepare_directories(
    config: &Config,
    config_dir: impl Into<PathBuf>,
) -> Result<SetupReport, SetupError> {
    config.validate()?;
    let config_dir = config_dir.into();
    if config_dir.exists() && !config_dir.is_dir() {
        return Err(SetupError::InvalidConfigDir(config_dir));
    }

    let state_dir = PathBuf::from(&config.state_dir);
    let log_dir = PathBuf::from(&config.log_dir);
    for path in [&config_dir, &state_dir, &log_dir] {
        if path.exists() && !path.is_dir() {
            return Err(SetupError::InvalidRuntimeDir);
        }
        std::fs::create_dir_all(path)?;
    }

    Ok(SetupReport {
        dependencies: check_dependencies(),
        config_dir,
        state_dir,
        log_dir,
    })
}

pub fn config_file_path(config_dir: &Path) -> PathBuf {
    config_dir.join("gitrun.env")
}

fn command_status(name: &'static str, args: &[&str]) -> DependencyStatus {
    match Command::new(name).args(args).output() {
        Ok(output) if output.status.success() => DependencyStatus {
            name,
            available: true,
            version: String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .map(str::to_owned),
        },
        _ => DependencyStatus {
            name,
            available: false,
            version: None,
        },
    }
}

fn docker_daemon_status() -> DependencyStatus {
    match Command::new("docker").arg("info").output() {
        Ok(output) if output.status.success() => DependencyStatus {
            name: "docker daemon",
            available: true,
            version: None,
        },
        _ => DependencyStatus {
            name: "docker daemon",
            available: false,
            version: None,
        },
    }
}

fn compose_status() -> DependencyStatus {
    match Command::new("docker").args(["compose", "version"]).output() {
        Ok(output) if output.status.success() => DependencyStatus {
            name: "docker compose",
            available: true,
            version: String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .map(str::to_owned),
        },
        _ => DependencyStatus {
            name: "docker compose",
            available: false,
            version: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::{SystemTime, UNIX_EPOCH}};

    #[test]
    fn config_path_is_inside_config_dir() {
        assert_eq!(
            config_file_path(Path::new("/tmp/gitrun")),
            PathBuf::from("/tmp/gitrun/gitrun.env")
        );
    }

    #[test]
    fn dependency_report_has_required_tools() {
        let names: Vec<_> = check_dependencies().into_iter().map(|d| d.name).collect();
        assert!(
            names.contains(&"docker")
                && names.contains(&"docker daemon")
                && names.contains(&"docker compose")
                && names.contains(&"git")
        );
    }

    #[test]
    fn prepare_directories_rejects_file_as_config_dir() {
        let path = std::env::temp_dir().join(format!(
            "gitrun-setup-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::write(&path, "not a directory").unwrap();
        let config = Config::default();
        assert!(matches!(
            prepare_directories(&config, &path),
            Err(SetupError::InvalidConfigDir(_))
        ));
        fs::remove_file(path).unwrap();
    }
}
