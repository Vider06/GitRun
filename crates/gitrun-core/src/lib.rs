//! Core domain types and shared services for GitRun.
//!
//! This crate is the common foundation used by the CLI, scheduler, GSR,
//! setup, recovery, updater, and dashboard layers.
//! It intentionally exposes stable domain APIs while keeping implementation
//! details inside their respective modules.

pub mod api_policy;
#[path = "app_auth.rs"]
pub mod app_auth;
pub mod command_policy;
pub mod compatibility;
pub mod config;
pub mod github_auth;
pub mod hypervisor_decision;
pub mod runner;
pub mod settings;
pub mod state;
pub mod workflow_validation;

pub use api_policy::{ApiPolicy, GitRunApi, GitRunOperation, PolicyMatrix};
pub use app_auth::{AppAuth, AppAuthError};
pub use command_policy::{
    baseline_patterns, CommandPolicy, Decision, PatternList, ViolationAction,
};
pub use config::{Config, ConfigError};
pub use github_auth::{GitHubAuth, GitHubAuthError};
pub use runner::{Runner, RunnerPool, RunnerState};
pub use settings::{
    ApiPolicyOverride, DockerPolicy, EffectiveRepositorySettings, GitRunSettings,
    LogicContainerPolicy, MountPolicy, MountRule, RegisterPolicy, RepositorySettings,
    SharedStoragePolicy, VaultPolicy, SETTINGS_SCHEMA_VERSION,
};
pub use state::{HealthReport, StateError, StateStore};
pub use workflow_validation::{
    ensure_zizmor_installed, run_zizmor, scan, scan_dock_requests, validate_workflows_dir,
    zizmor_info, DockOperation, DockRequest, Finding, InstallOutcome, ValidationReport, ZizmorInfo,
};

pub use compatibility::{
    analyze as analyze_compatibility, CompatibilityFinding, CompatibilityReport,
    CompatibilityStatus,
};
