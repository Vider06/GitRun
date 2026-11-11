//! Core domain types and shared services for GitRun.
//!
//! This crate is the common foundation used by the CLI, scheduler, GSR,
//! setup, recovery, updater, and dashboard layers.
//! It intentionally exposes stable domain APIs while keeping implementation
//! details inside their respective modules.

// Keep shared modules explicit so crate consumers can audit the public surface.
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

#[cfg(not(any(kani, verus)))]
pub use api_policy::{ApiPolicy, GitRunApi, GitRunOperation, PolicyMatrix};
#[cfg(not(any(kani, verus)))]
pub use app_auth::{AppAuth, AppAuthError};
#[cfg(not(any(kani, verus)))]
pub use command_policy::{
    baseline_patterns, CommandPolicy, Decision, PatternList, ViolationAction,
};
#[cfg(not(any(kani, verus)))]
pub use config::{Config, ConfigError};
#[cfg(not(any(kani, verus)))]
pub use github_auth::{GitHubAuth, GitHubAuthError};
pub use runner::{Runner, RunnerPool, RunnerState};
#[cfg(not(any(kani, verus)))]
pub use settings::{
    ApiPolicyOverride, DockerPolicy, EffectiveRepositorySettings, GitRunSettings,
    LogicContainerPolicy, MountPolicy, MountRule, RegisterPolicy, RepositorySettings,
    SharedStoragePolicy, VaultPolicy, SETTINGS_SCHEMA_VERSION,
};
#[cfg(not(any(kani, verus)))]
pub use state::{HealthReport, StateError, StateStore};
#[cfg(not(any(kani, verus)))]
pub use workflow_validation::{
    ensure_zizmor_installed, run_zizmor, scan, scan_dock_requests, validate_workflows_dir,
    zizmor_info, zizmor_is_installed, DockOperation, DockRequest, Finding, InstallOutcome,
    ValidationReport, ZizmorInfo,
};

#[cfg(not(any(kani, verus)))]
pub use compatibility::{
    analyze as analyze_compatibility, CompatibilityFinding, CompatibilityReport,
    CompatibilityStatus,
};
