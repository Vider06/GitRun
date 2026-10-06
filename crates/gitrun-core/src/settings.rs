//! Persistent GitRun security/settings policy.
//!
//! Settings are deliberately separate from Config: Config contains
//! daemon/bootstrap configuration, while this file contains workflow
//! capability policy that may later be edited by the dashboard or CLI.
//!
//! The hierarchy is:
//!
//! `global policy` -> `repository restrictions` -> `effective policy`
//!
//! Repository settings can only narrow the global policy. They cannot
//! re-enable an API or operation that the global policy disabled.

use crate::api_policy::{ApiPolicy, GitRunApi, GitRunOperation, PolicyMatrix};
