//! Core domain types and shared services for GitRun.\n//!\n//! This crate is the common foundation used by the CLI, scheduler, GSR,\n//! setup, recovery, updater, and dashboard layers.\n//! It intentionally exposes stable domain APIs while keeping implementation\n//! details inside their respective modules.\n\npub mod app_auth;
pub mod command_policy;
pub mod config;
pub mod github_auth;
pub mod hypervisor_decision;
pub mod runner;
pub mod state;
pub mod workflow_validation;

pub use app_auth::{AppAuth, AppAuthError};
pub use command_policy::{
    baseline_patterns, CommandPolicy, Decision, PatternList, ViolationAction,
};
pub use config::{Config, ConfigError};
pub use github_auth::{GitHubAuth, GitHubAuthError};
pub use runner::{Runner, RunnerPool, RunnerState};
pub use state::{HealthReport, StateError, StateStore};
pub use workflow_validation::{
    ensure_zizmor_installed, run_zizmor, scan, validate_workflows_dir, zizmor_info, Finding,
    InstallOutcome, ValidationReport, ZizmorInfo,
};
