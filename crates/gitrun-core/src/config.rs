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
    /// Host sizing profile: standard, small, or large.
    pub host_profile: String,
    pub min_runners: u32,
    pub max_runners: u32,
    pub idle_timeout: u64,
    pub poll_interval: u64,
    pub runner_image: String,
    pub runner_labels: String,
    /// Docker network to attach runner containers to. Operators can point
    /// this at a pre-created egress-controlled network.
    pub runner_network: String,
    /// Docker seccomp profile: `default` or a daemon-visible profile path.
    pub runner_seccomp_profile: String,
    /// Optional Docker AppArmor profile name; empty leaves Docker default.
    pub runner_apparmor_profile: String,
    pub ephemeral: bool,
    pub state_dir: String,
    pub log_dir: String,
    pub auto_container_update: bool,
    pub container_update_time: String,
    pub auto_container_recovery: bool,
    pub container_recovery_cooldown: u64,
    /// CPU limit passed to `docker run --cpus` for each runner container.
    pub container_cpus: String,
    /// Memory limit passed to `docker run --memory` for each runner container.
    pub container_memory: String,
    /// Process count limit passed to `docker run --pids-limit`.
    pub container_pids_limit: String,
    /// Whether the runner should disable its own self-update mechanism
    /// (GitRun controls runner versioning itself via GTUU instead).
    pub runner_disable_update: bool,
    /// Name of the Docker volume shared across runner containers for
    /// package-manager caches (Cargo/pip/npm) — the embryonic GitVault.
    pub shared_cache_volume: String,
    /// Cache isolation scope: `repository` (default), `runner`, or `global`.
    /// Repository and runner scopes prevent one workflow trust domain from
    /// reading another domain's package cache through a shared Docker volume.
    pub shared_cache_scope: String,
    /// Size of the runner's home directory (registration state, diagnostics,
    /// job checkouts), applied whether it's backed by tmpfs or a disk volume
    /// — see `runner_home_backend`. Needs headroom for a real checkout +
    /// build, not just scratch space — see the fix note in
    /// gitrun-scheduler/src/docker.rs for why this exists at all.
    pub runner_home_size: String,
    /// Where the runner's home directory lives: `"tmpfs"` (RAM-backed,
    /// fastest, but counts against host memory per concurrent runner — the
    /// original, still the default) or `"volume"` (a Docker-managed named
    /// volume on disk, one per runner container, removed alongside it).
    /// Any other value is rejected by `validate()`.
    pub runner_home_backend: String,
    /// How long to wait for a TCP connection to GitHub's API before giving
    /// up, in seconds. Separate from the overall request timeout so a dead
    /// route fails fast without capping legitimately slow-but-working
    /// paginated requests.
    pub github_connect_timeout: u64,
    /// Overall timeout for a single GitHub API request/response, in seconds.
    pub github_request_timeout: u64,
    /// GitHub App ID, if authenticating as an App installation instead of a
    /// PAT. All three App fields must be set together, or none at all —
    /// see `Config::validate`.
    pub github_app_id: String,
    pub github_app_installation_id: String,
    /// Path to a PEM file holding the App's private key. Not the key
    /// content itself — this stays out of the env file/config struct so the
    /// key material isn't duplicated into `gitrun.env` (which already holds
    /// the PAT when using that path instead); the file's own permissions
    /// (0600, same treatment as gitrun.env) are the boundary.
    pub github_app_private_key_path: String,
    /// Directory where GitVault stores its encrypted secrets and master key.
    /// Empty means GitVault is disabled — no secrets are looked up or
    /// injected into runner containers, same as before GitVault existed.
    pub vault_dir: String,
    /// Maps a repo to the GitVault groups it belongs to, as
    /// "owner/repo=group1,group2;owner/other=group1" pairs (semicolon
    /// between repos, comma between a repo's groups). Kept as a single
    /// env-friendly string rather than a nested structure because `Config`'s
    /// env-var-based load path (`from_lookup`) doesn't have a natural way to
    /// represent nested per-repo lists — see `vault_groups_for_repo` for the
    /// parser. A dedicated JSON file (mirroring `logic-containers.json`)
    /// would scale better as this grows past a handful of repos; revisit if
    /// this string becomes unwieldy in practice.
    pub vault_group_membership: String,
    /// Whether GTUU's daily scheduled-update check compares
    /// `container_update_time` against UTC (`"utc"`, the default, and the
    /// only option before this was configurable) or the host's local time
    /// (`"local"`). The Python original used naive local time; the Rust
    /// port originally hardcoded UTC to avoid a `chrono` dependency, which
    /// is a silent behavior change on any server not itself running in
    /// UTC — see `main.rs::chrono_like_now`. Any other value is rejected
    /// by `validate()`.
    pub gtuu_schedule_timezone: String,
    /// The GSR "danger gate" (operator's term): Docker-level hardening
    /// (cap-drop, `--security-opt no-new-privileges`, seccomp) applied to
    /// every runner container that mounts the Docker socket. On by
    /// default. Turning it off requires *also* setting
    /// `gsr_allow_unsafe_runner = true` — see that field — so it can't be
    /// disabled by a single accidental unset/typo.
    pub gsr_docker_socket_hardening: bool,
    /// Explicit opt-in required to run without `gsr_docker_socket_hardening`.
    /// Deliberately a separate field rather than making
    /// `gsr_docker_socket_hardening` itself default-on-unless-set: an
    /// operator must affirmatively set *this* to bypass the safe default,
    /// so a bug or omission elsewhere in config loading fails closed
    /// (hardening stays on) rather than open.
    pub gsr_allow_unsafe_runner: bool,
    /// Master switch for GSR's runtime command policy (whitelist/blacklist
    /// enforcement inside runner containers). If false, none of the three
    /// lists below are consulted regardless of their own `_enabled` flags —
    /// this is the single kill switch for the whole feature.
    pub gsr_command_policy_enabled: bool,
    /// Our shipped list of known-dangerous commands/patterns (see
    /// `command_policy::baseline_patterns`). On by default.
    pub gsr_command_baseline_blacklist_enabled: bool,
    /// Operator-supplied blacklist, independent of the baseline. Off by
    /// default (empty list is a no-op either way, but the flag lets an
    /// operator pre-stage a list before turning it on).
    pub gsr_command_blacklist_enabled: bool,
    /// Comma-separated substrings for `gsr_command_blacklist_enabled`.
    pub gsr_command_blacklist: String,
    /// Operator-supplied whitelist: if enabled, ONLY matching commands are
    /// allowed. Off by default — the strictest mode, needs real tuning.
    pub gsr_command_whitelist_enabled: bool,
    /// Comma-separated substrings for `gsr_command_whitelist_enabled`.
    pub gsr_command_whitelist: String,
    /// What GSR does on a policy violation: `"log_only"`, `"kill"`
    /// (default), or `"kill_and_ban"`. See `command_policy::ViolationAction`.
    pub gsr_violation_action: String,
    /// Validate a workflow's `.github/workflows/*.yml` for known-risky
    /// patterns before a runner picks up its job. On by default — this is
    /// a pre-flight check, not enforcement inside the container, and is
    /// cheap enough to always run.
    pub gsr_workflow_validation_enabled: bool,
    /// Whether to additionally shell out to the third-party `zizmor`
    /// static analyzer (MIT-licensed, <https://github.com/zizmorcore/zizmor>)
    /// if it's installed on the host, for deeper workflow analysis than
    /// our own built-in checks. Off by default because it's an optional
    /// external binary GitRun does not install for the operator — see
    /// `gitrun-core::workflow_validation` for how it's invoked and how a
    /// missing binary is handled (silently skipped, never a hard failure).
    pub gsr_zizmor_enabled: bool,
    /// Whether the operator has accepted zizmor's license/terms via the
    /// dashboard's consent dialog. Kept separate from `gsr_zizmor_enabled`
    /// so an operator can later turn the feature off and back on without
    /// re-accepting (the acceptance is durable), but `validate()` requires
    /// this to be true whenever `gsr_zizmor_enabled` is true — enabling
    /// the feature without ever having shown the dialog is not a state
    /// this config allows to exist. Set only by
    /// `accept_zizmor_license_and_install` in the dashboard backend, never
    /// hand-edited by ordinary settings UI.
    pub gsr_zizmor_license_accepted: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            repositories: Vec::new(),
            host_profile: "standard".into(),
            min_runners: 3,
            max_runners: 8,
            idle_timeout: 120,
            poll_interval: 5,
            runner_image: "gitrun-runner:latest".into(),
            runner_labels: "self-hosted,Linux,X64".into(),
            runner_network: "bridge".into(),
            runner_seccomp_profile: "default".into(),
            runner_apparmor_profile: String::new(),
            ephemeral: false,
            state_dir: "/var/lib/gitrun".into(),
            log_dir: "/var/log/gitrun".into(),
            auto_container_update: false,
            container_update_time: "03:00".into(),
            auto_container_recovery: true,
            container_recovery_cooldown: 60,
            container_cpus: "1".into(),
            container_memory: "1g".into(),
            container_pids_limit: "1024".into(),
            runner_disable_update: false,
            shared_cache_volume: "gitrun-runner-shared".into(),
            shared_cache_scope: "repository".into(),
            runner_home_size: "8g".into(),
            runner_home_backend: "tmpfs".into(),
            github_connect_timeout: 5,
            github_request_timeout: 20,
            github_app_id: String::new(),
            github_app_installation_id: String::new(),
            github_app_private_key_path: String::new(),
            vault_dir: String::new(),
            vault_group_membership: String::new(),
            gtuu_schedule_timezone: "utc".into(),
            gsr_docker_socket_hardening: true,
            gsr_allow_unsafe_runner: false,
            gsr_command_policy_enabled: true,
            gsr_command_baseline_blacklist_enabled: true,
            gsr_command_blacklist_enabled: false,
            gsr_command_blacklist: String::new(),
            gsr_command_whitelist_enabled: false,
            gsr_command_whitelist: String::new(),
            gsr_violation_action: "kill".into(),
            gsr_workflow_validation_enabled: true,
            gsr_zizmor_enabled: false,
            gsr_zizmor_license_accepted: false,
            resource_pressure_enabled: true,
            resource_pressure_cpu_percent: 90,
            resource_pressure_memory_percent: 90,
            resource_pressure_disk_percent: 90,
        }
    }
}

impl Config {
    /// Reads configuration from the real process environment. Passes
    /// through every `GITRUN_`-prefixed variable rather than an explicit
    /// key list: an explicit list here previously drifted out of sync with
    /// the fields `from_lookup` actually reads (14 keys — container
    /// cpus/memory/pids, GitHub App auth, GitVault, runner home
    /// size/backend — were silently ignored when set as real environment
    /// variables instead of in `gitrun.env`, since `from_env_file` parses
    /// every line unconditionally but `from_env` only forwarded a stale
    /// subset). A prefix filter can't go stale the same way.
    pub fn from_env() -> Result<Self, ConfigError> {
        let lookup: std::collections::HashMap<String, String> = env::vars()
            .filter(|(key, _)| key.starts_with("GITRUN_"))
            .collect();
        Self::from_lookup(&lookup)
    }

    pub fn from_env_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path)?;
        let mut lookup = std::collections::HashMap::new();
        for raw in content.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
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
    fn from_lookup(
        lookup: &std::collections::HashMap<String, String>,
    ) -> Result<Self, ConfigError> {
        let get = |key: &str| lookup.get(key).cloned();
        let mut c = Self::default();
        // Fallback for compatibility with the Python autoscaler: it accepts
        // GITRUN_DEFAULT_REPOSITORY as a single-repo fallback when
        // GITRUN_REPOSITORIES is unset/empty. Without this, an operator
        // relying on the old variable name would see zero repos configured
        // after switching to the Rust binary, with no error — a silent
        // behavior change during migration.
        let repositories_raw = get("GITRUN_REPOSITORIES")
            .filter(|v| !v.trim().is_empty())
            .or_else(|| get("GITRUN_DEFAULT_REPOSITORY"));
        if let Some(raw) = repositories_raw {
            c.repositories = parse_repositories(&raw)?;
        }
        c.min_runners = value_u32(
            &get("GITRUN_MIN_RUNNERS"),
            "GITRUN_MIN_RUNNERS",
            c.min_runners,
        )?;
        c.max_runners = value_u32(
            &get("GITRUN_MAX_RUNNERS"),
            "GITRUN_MAX_RUNNERS",
            c.max_runners,
        )?;
        c.idle_timeout = value_u64(
            &get("GITRUN_IDLE_TIMEOUT"),
            "GITRUN_IDLE_TIMEOUT",
            c.idle_timeout,
        )?;
        c.poll_interval = value_u64(
            &get("GITRUN_POLL_INTERVAL"),
            "GITRUN_POLL_INTERVAL",
            c.poll_interval,
        )?;
        if let Some(v) = get("GITRUN_HOST_PROFILE") {
            c.host_profile = v.trim().to_ascii_lowercase();
        }
        if c.host_profile == "small" {
            if !lookup.contains_key("GITRUN_MIN_RUNNERS") { c.min_runners = 1; }
            if !lookup.contains_key("GITRUN_MAX_RUNNERS") { c.max_runners = 2; }
            if !lookup.contains_key("GITRUN_CONTAINER_CPUS") { c.container_cpus = "0.5".into(); }
            if !lookup.contains_key("GITRUN_CONTAINER_MEMORY") { c.container_memory = "768m".into(); }
            if !lookup.contains_key("GITRUN_RESOURCE_PRESSURE_CPU_PERCENT") { c.resource_pressure_cpu_percent = 80; }
            if !lookup.contains_key("GITRUN_RESOURCE_PRESSURE_MEMORY_PERCENT") { c.resource_pressure_memory_percent = 85; }
            if !lookup.contains_key("GITRUN_RESOURCE_PRESSURE_DISK_PERCENT") { c.resource_pressure_disk_percent = 90; }
        } else if c.host_profile == "large" {
            if !lookup.contains_key("GITRUN_MAX_RUNNERS") { c.max_runners = 16; }
        }
        if let Some(v) = get("GITRUN_RUNNER_IMAGE") {
            c.runner_image = v;
        }
        if let Some(v) = get("GITRUN_RUNNER_NETWORK") { c.runner_network = v.trim().to_owned(); }
        if let Some(v) = get("GITRUN_RUNNER_SECCOMP_PROFILE") { c.runner_seccomp_profile = v.trim().to_owned(); }
        if let Some(v) = get("GITRUN_RUNNER_APPARMOR_PROFILE") { c.runner_apparmor_profile = v.trim().to_owned(); }
        if let Some(v) = get("GITRUN_RUNNER_LABELS") {
            c.runner_labels = v;
        }
        if let Some(v) = get("GITRUN_EPHEMERAL") {
            c.ephemeral = parse_bool("GITRUN_EPHEMERAL", &v)?;
        }
        if let Some(v) = get("GITRUN_STATE_DIR") {
            c.state_dir = v;
        }
        if let Some(v) = get("GITRUN_LOG_DIR") {
            c.log_dir = v;
        }
        if let Some(v) = get("GITRUN_AUTO_CONTAINER_UPDATE") {
            c.auto_container_update = parse_bool("GITRUN_AUTO_CONTAINER_UPDATE", &v)?;
        }
        if let Some(v) = get("GITRUN_CONTAINER_UPDATE_TIME") {
            c.container_update_time = v;
        }
        if let Some(v) = get("GITRUN_AUTO_CONTAINER_RECOVERY") {
            c.auto_container_recovery = parse_bool("GITRUN_AUTO_CONTAINER_RECOVERY", &v)?;
        }
        c.container_recovery_cooldown = value_u64(
            &get("GITRUN_CONTAINER_RECOVERY_COOLDOWN"),
            "GITRUN_CONTAINER_RECOVERY_COOLDOWN",
            c.container_recovery_cooldown,
        )?;
        if let Some(v) = get("GITRUN_CONTAINER_CPUS") {
            c.container_cpus = v;
        }
        if let Some(v) = get("GITRUN_CONTAINER_MEMORY") {
            c.container_memory = v;
        }
        if let Some(v) = get("GITRUN_CONTAINER_PIDS") {
            c.container_pids_limit = v;
        }
        if let Some(v) = get("GITRUN_DISABLE_UPDATE") {
            c.runner_disable_update = parse_bool("GITRUN_DISABLE_UPDATE", &v)?;
        }
        if let Some(v) = get("GITRUN_SHARED_CACHE_VOLUME") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                c.shared_cache_volume = trimmed.to_owned();
            }
        }
        if let Some(v) = get("GITRUN_SHARED_CACHE_SCOPE") {
            c.shared_cache_scope = v.trim().to_ascii_lowercase();
        }
        if let Some(v) = get("GITRUN_RUNNER_HOME_SIZE") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                c.runner_home_size = trimmed.to_owned();
            }
        }
        if let Some(v) = get("GITRUN_RUNNER_HOME_BACKEND") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                c.runner_home_backend = trimmed.to_ascii_lowercase();
            }
        }
        c.github_connect_timeout = value_u64(
            &get("GITRUN_GITHUB_CONNECT_TIMEOUT"),
            "GITRUN_GITHUB_CONNECT_TIMEOUT",
            c.github_connect_timeout,
        )?;
        c.github_request_timeout = value_u64(
            &get("GITRUN_GITHUB_REQUEST_TIMEOUT"),
            "GITRUN_GITHUB_REQUEST_TIMEOUT",
            c.github_request_timeout,
        )?;
        if let Some(v) = get("GITRUN_GITHUB_APP_ID") {
            c.github_app_id = v.trim().to_owned();
        }
        if let Some(v) = get("GITRUN_GITHUB_APP_INSTALLATION_ID") {
            c.github_app_installation_id = v.trim().to_owned();
        }
        if let Some(v) = get("GITRUN_GITHUB_APP_PRIVATE_KEY_PATH") {
            c.github_app_private_key_path = v.trim().to_owned();
        }
        if let Some(v) = get("GITRUN_VAULT_DIR") {
            c.vault_dir = v.trim().to_owned();
        }
        if let Some(v) = get("GITRUN_VAULT_GROUPS") {
            c.vault_group_membership = v.trim().to_owned();
        }
        if let Some(v) = get("GITRUN_GTUU_SCHEDULE_TIMEZONE") {
            c.gtuu_schedule_timezone = v.trim().to_ascii_lowercase();
        }
        if let Some(v) = get("GITRUN_GSR_DOCKER_SOCKET_HARDENING") {
            c.gsr_docker_socket_hardening = parse_bool("GITRUN_GSR_DOCKER_SOCKET_HARDENING", &v)?;
        }
        if let Some(v) = get("GITRUN_GSR_ALLOW_UNSAFE_RUNNER") {
            c.gsr_allow_unsafe_runner = parse_bool("GITRUN_GSR_ALLOW_UNSAFE_RUNNER", &v)?;
        }
        if let Some(v) = get("GITRUN_GSR_COMMAND_POLICY_ENABLED") {
            c.gsr_command_policy_enabled = parse_bool("GITRUN_GSR_COMMAND_POLICY_ENABLED", &v)?;
        }
        if let Some(v) = get("GITRUN_GSR_COMMAND_BASELINE_BLACKLIST_ENABLED") {
            c.gsr_command_baseline_blacklist_enabled =
                parse_bool("GITRUN_GSR_COMMAND_BASELINE_BLACKLIST_ENABLED", &v)?;
        }
        if let Some(v) = get("GITRUN_GSR_COMMAND_BLACKLIST_ENABLED") {
            c.gsr_command_blacklist_enabled =
                parse_bool("GITRUN_GSR_COMMAND_BLACKLIST_ENABLED", &v)?;
        }
        if let Some(v) = get("GITRUN_GSR_COMMAND_BLACKLIST") {
            c.gsr_command_blacklist = v;
        }
        if let Some(v) = get("GITRUN_GSR_COMMAND_WHITELIST_ENABLED") {
            c.gsr_command_whitelist_enabled =
                parse_bool("GITRUN_GSR_COMMAND_WHITELIST_ENABLED", &v)?;
        }
        if let Some(v) = get("GITRUN_GSR_COMMAND_WHITELIST") {
            c.gsr_command_whitelist = v;
        }
        if let Some(v) = get("GITRUN_GSR_VIOLATION_ACTION") {
            c.gsr_violation_action = v.trim().to_ascii_lowercase();
        }
        if let Some(v) = get("GITRUN_GSR_WORKFLOW_VALIDATION_ENABLED") {
            c.gsr_workflow_validation_enabled =
                parse_bool("GITRUN_GSR_WORKFLOW_VALIDATION_ENABLED", &v)?;
        }
        if let Some(v) = get("GITRUN_GSR_ZIZMOR_ENABLED") {
            c.gsr_zizmor_enabled = parse_bool("GITRUN_GSR_ZIZMOR_ENABLED", &v)?;
        }
        if let Some(v) = get("GITRUN_GSR_ZIZMOR_LICENSE_ACCEPTED") {
            c.gsr_zizmor_license_accepted = parse_bool("GITRUN_GSR_ZIZMOR_LICENSE_ACCEPTED", &v)?;
        }
        if let Some(v) = get("GITRUN_RESOURCE_PRESSURE_ENABLED") {
            c.resource_pressure_enabled = parse_bool("GITRUN_RESOURCE_PRESSURE_ENABLED", &v)?;
        }
        c.resource_pressure_cpu_percent = value_u8(
            &get("GITRUN_RESOURCE_PRESSURE_CPU_PERCENT"),
            "GITRUN_RESOURCE_PRESSURE_CPU_PERCENT",
            c.resource_pressure_cpu_percent,
        )?;
        c.resource_pressure_memory_percent = value_u8(
            &get("GITRUN_RESOURCE_PRESSURE_MEMORY_PERCENT"),
            "GITRUN_RESOURCE_PRESSURE_MEMORY_PERCENT",
            c.resource_pressure_memory_percent,
        )?;
        c.resource_pressure_disk_percent = value_u8(
            &get("GITRUN_RESOURCE_PRESSURE_DISK_PERCENT"),
            "GITRUN_RESOURCE_PRESSURE_DISK_PERCENT",
            c.resource_pressure_disk_percent,
        )?;
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.min_runners == 0 || self.max_runners < self.min_runners {
            return Err(ConfigError::Invalid(format!(
                "runner bounds are invalid: {}..{}",
                self.min_runners, self.max_runners
            )));
        }
        if self.poll_interval == 0 {
            return Err(ConfigError::Invalid(
                "poll interval must be greater than zero".into(),
            ));
        }
        if self.idle_timeout == 0 {
            return Err(ConfigError::Invalid(
                "idle timeout must be greater than zero".into(),
            ));
        }
        if self.container_recovery_cooldown == 0 {
            return Err(ConfigError::Invalid(
                "container recovery cooldown must be greater than zero".into(),
            ));
        }
        if self.runner_image.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "runner image must not be empty".into(),
            ));
        }
        if self.runner_labels.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "runner labels must not be empty".into(),
            ));
        }
        if !is_positive_decimal(&self.container_cpus) {
            return Err(ConfigError::Invalid(format!(
                "container cpus must be a positive decimal number, got {:?}",
                self.container_cpus
            )));
        }
        if !is_positive_docker_size(&self.container_memory) {
            return Err(ConfigError::Invalid(format!(
                "container memory must be a positive Docker size, got {:?}",
                self.container_memory
            )));
        }
        if !is_positive_u64(&self.container_pids_limit) {
            return Err(ConfigError::Invalid(format!(
                "container pids limit must be a positive integer, got {:?}",
                self.container_pids_limit
            )));
        }
        if !matches!(self.shared_cache_scope.as_str(), "global" | "repository" | "runner") {
            return Err(ConfigError::Invalid(format!("shared cache scope must be global, repository, or runner, got {:?}", self.shared_cache_scope)));
        }
        if !is_valid_docker_name(&self.shared_cache_volume) {
            return Err(ConfigError::Invalid(format!(
                "shared cache volume must be a valid Docker volume name, got {:?}",
                self.shared_cache_volume
            )));
        }
        if !is_positive_docker_size(&self.runner_home_size) {
            return Err(ConfigError::Invalid(format!(
                "runner home size must be a positive Docker size, got {:?}",
                self.runner_home_size
            )));
        }
        if !matches!(self.runner_home_backend.as_str(), "tmpfs" | "volume") {
            return Err(ConfigError::Invalid(format!(
                "runner home backend must be \"tmpfs\" or \"volume\", got {:?}",
                self.runner_home_backend
            )));
        }
        if !matches!(self.host_profile.as_str(), "small" | "standard" | "large") {
            return Err(ConfigError::Invalid(format!("host profile must be small, standard, or large, got {:?}", self.host_profile)));
        }
        if self.runner_network.trim().is_empty() || self.runner_network.chars().any(char::is_control) || self.runner_network.chars().any(char::is_whitespace) {
            return Err(ConfigError::Invalid("runner network must be a non-empty Docker network name".into()));
        }
        if self.runner_seccomp_profile.trim().is_empty() || self.runner_seccomp_profile.chars().any(char::is_control) || self.runner_seccomp_profile.chars().any(char::is_whitespace) {
            return Err(ConfigError::Invalid("runner seccomp profile must be a non-empty value without whitespace".into()));
        }
        if self.runner_apparmor_profile.chars().any(char::is_control) || self.runner_apparmor_profile.chars().any(char::is_whitespace) {
            return Err(ConfigError::Invalid("runner AppArmor profile must not contain whitespace or control characters".into()));
        }
        if !matches!(self.gtuu_schedule_timezone.as_str(), "utc" | "local") {
            return Err(ConfigError::Invalid(format!(
                "GTUU schedule timezone must be \"utc\" or \"local\", got {:?}",
                self.gtuu_schedule_timezone
            )));
        }
        if self.github_connect_timeout == 0 {
            return Err(ConfigError::Invalid(
                "github connect timeout must be greater than zero".into(),
            ));
        }
        if self.github_request_timeout == 0 {
            return Err(ConfigError::Invalid(
                "github request timeout must be greater than zero".into(),
            ));
        }
        if self.github_connect_timeout > self.github_request_timeout {
            return Err(ConfigError::Invalid(
                "github connect timeout must not exceed the overall request timeout".into(),
            ));
        }
        let app_fields = [
            !self.github_app_id.trim().is_empty(),
            !self.github_app_installation_id.trim().is_empty(),
            !self.github_app_private_key_path.trim().is_empty(),
        ];
        if app_fields.iter().any(|set| *set) && !app_fields.iter().all(|set| *set) {
            return Err(ConfigError::Invalid(
                "GitHub App auth requires GITRUN_GITHUB_APP_ID, GITRUN_GITHUB_APP_INSTALLATION_ID, and GITRUN_GITHUB_APP_PRIVATE_KEY_PATH to all be set together".into(),
            ));
        }
        validate_time(&self.container_update_time)?;
        let mut seen_repositories =
            std::collections::HashSet::with_capacity(self.repositories.len());
        for repo in &self.repositories {
            if !is_repository(repo) {
                return Err(ConfigError::Repository(repo.clone()));
            }
            if !seen_repositories.insert(repo) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate repository configured: {repo}"
                )));
            }
        }
        // The danger gate: disabling Docker-socket hardening requires the
        // separate explicit opt-in, so a single unset/mistyped variable
        // can never silently drop this protection - see the field docs.
        if !self.gsr_docker_socket_hardening && !self.gsr_allow_unsafe_runner {
            return Err(ConfigError::Invalid(
                "GITRUN_GSR_DOCKER_SOCKET_HARDENING=false requires GITRUN_GSR_ALLOW_UNSAFE_RUNNER=true as an explicit, separate opt-in".into(),
            ));
        }
        if !matches!(
            self.gsr_violation_action.as_str(),
            "log_only" | "kill" | "kill_and_ban"
        ) {
            return Err(ConfigError::Invalid(format!(
                "GSR violation action must be \"log_only\", \"kill\", or \"kill_and_ban\", got {:?}",
                self.gsr_violation_action
            )));
        }
        validate_single_line("GITRUN_GSR_COMMAND_BLACKLIST", &self.gsr_command_blacklist)?;
        validate_single_line("GITRUN_GSR_COMMAND_WHITELIST", &self.gsr_command_whitelist)?;
        validate_single_line("GITRUN_GSR_VIOLATION_ACTION", &self.gsr_violation_action)?;

        if self.gsr_zizmor_enabled && !self.gsr_zizmor_license_accepted {
            return Err(ConfigError::Invalid(
                "GITRUN_GSR_ZIZMOR_ENABLED=true requires the zizmor license/terms to have been accepted first (GITRUN_GSR_ZIZMOR_LICENSE_ACCEPTED=true) — this is normally set by the dashboard's consent dialog, not by hand".into(),
            ));
        }
        for (name, value) in [
            ("CPU", self.resource_pressure_cpu_percent),
            ("memory", self.resource_pressure_memory_percent),
            ("disk", self.resource_pressure_disk_percent),
        ] {
            if !(1..=100).contains(&value) {
                return Err(ConfigError::Invalid(format!(
                    "resource pressure {name} threshold must be between 1 and 100%, got {value}"
                )));
            }
        }
        Ok(())
    }

    /// Builds the runtime `CommandPolicy` from this config's flags/lists.
    /// Returns `None` if `gsr_command_policy_enabled` is false — the single
    /// kill switch for the whole feature — so callers don't need to
    /// separately check that flag before calling `evaluate`.
    pub fn command_policy(&self) -> Option<crate::command_policy::CommandPolicy> {
        if !self.gsr_command_policy_enabled {
            return None;
        }
        Some(crate::command_policy::CommandPolicy {
            baseline_blacklist: crate::command_policy::PatternList::new(
                self.gsr_command_baseline_blacklist_enabled,
                crate::command_policy::baseline_patterns(),
            ),
            user_blacklist: crate::command_policy::PatternList::new(
                self.gsr_command_blacklist_enabled,
                split_csv(&self.gsr_command_blacklist),
            ),
            user_whitelist: crate::command_policy::PatternList::new(
                self.gsr_command_whitelist_enabled,
                split_csv(&self.gsr_command_whitelist),
            ),
        })
    }

    /// Parses `gsr_violation_action` into the typed enum.
    pub fn violation_action(&self) -> crate::command_policy::ViolationAction {
        crate::command_policy::ViolationAction::from_config_str(&self.gsr_violation_action)
    }

    /// Whether this config is set up for GitHub App auth (all three App
    /// fields present) rather than a raw PAT. `validate()` already
    /// guarantees these are all-or-nothing, so checking just one is safe.
    pub fn uses_github_app(&self) -> bool {
        !self.github_app_id.trim().is_empty()
    }

    /// Parses `vault_group_membership` and returns the GitVault group names
    /// `repo` belongs to. Malformed entries (missing `=`, empty repo/group
    /// names) are skipped rather than causing the whole config to fail to
    /// load — a typo in one repo's group list shouldn't take down GitRun
    /// entirely, just leave that repo without its group-scoped secrets
    /// (global and repo-scoped secrets are unaffected either way).
    pub fn vault_groups_for_repo(&self, repo: &str) -> Vec<String> {
        self.vault_group_membership
            .split(';')
            .filter_map(|entry| entry.trim().split_once('='))
            .filter(|(entry_repo, _)| entry_repo.trim() == repo)
            .flat_map(|(_, groups)| groups.split(',').map(|g| g.trim().to_owned()))
            .filter(|g| !g.is_empty())
            .collect()
    }
}

fn validate_time(value: &str) -> Result<(), ConfigError> {
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || bytes[2] != b':'
        || !bytes[..2].iter().all(|byte| byte.is_ascii_digit())
        || !bytes[3..].iter().all(|byte| byte.is_ascii_digit())
    {
        return Err(ConfigError::Invalid(format!(
            "invalid container update time: {value}"
        )));
    }
    let hour = value[..2].parse::<u8>().unwrap_or(99);
    let minute = value[3..].parse::<u8>().unwrap_or(99);
    if hour > 23 || minute > 59 {
        return Err(ConfigError::Invalid(format!(
            "invalid container update time: {value}"
        )));
    }
    Ok(())
}

fn validate_single_line(key: &str, value: &str) -> Result<(), ConfigError> {
    if value.contains('\n') || value.contains('\r') {
        return Err(ConfigError::Invalid(format!(
            "{key} must not contain newlines"
        )));
    }
    Ok(())
}

fn parse_repositories(raw: &str) -> Result<Vec<String>, ConfigError> {
    let mut repositories = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for repo in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if !is_repository(repo) {
            return Err(ConfigError::Repository(repo.to_owned()));
        }
        if !seen.insert(repo) {
            return Err(ConfigError::Invalid(format!(
                "duplicate repository configured: {repo}"
            )));
        }
        repositories.push(repo.to_owned());
    }
    Ok(repositories)
}
/// Splits a comma-separated config string into trimmed, non-empty patterns.
/// Used for `gsr_command_blacklist`/`gsr_command_whitelist` — same shape as
/// `parse_repositories` above but without the repo-format check, since
/// these are free-form substrings rather than `owner/repo` pairs.
fn split_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
fn is_positive_u64(value: &str) -> bool {
    value.trim().parse::<u64>().map(|n| n > 0).unwrap_or(false)
}

fn is_positive_decimal(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.starts_with('.') || value.ends_with('.') {
        return false;
    }
    let mut dots = 0;
    let mut digits = 0;
    for byte in value.bytes() {
        match byte {
            b'0'..=b'9' => digits += 1,
            b'.' => {
                dots += 1;
                if dots > 1 {
                    return false;
                }
            }
            _ => return false,
        }
    }
    digits > 0
        && value
            .parse::<f64>()
            .map(|n| n.is_finite() && n > 0.0)
            .unwrap_or(false)
}

fn is_positive_docker_size(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    let split_at = value
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(value.len());
    let number = &value[..split_at];
    let suffix = value[split_at..].to_ascii_lowercase();
    if !is_positive_decimal(number) {
        return false;
    }
    matches!(
        suffix.as_str(),
        "b" | "k" | "kb" | "m" | "mb" | "g" | "gb" | "t" | "tb" | "p" | "pb"
    )
}

fn is_valid_docker_name(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        && value
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
}

fn is_repository(value: &str) -> bool {
    let mut parts = value.split('/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(a), Some(b), None) if !a.is_empty() && !b.is_empty())
}
fn value_u32(value: &Option<String>, key: &str, default: u32) -> Result<u32, ConfigError> {
    match value {
        Some(v) => v.parse().map_err(|_| ConfigError::Integer {
            key: key.into(),
            value: v.clone(),
        }),
        None => Ok(default),
    }
}
fn value_u64(value: &Option<String>, key: &str, default: u64) -> Result<u64, ConfigError> {
    match value {
        Some(v) => v.parse().map_err(|_| ConfigError::Integer {
            key: key.into(),
            value: v.clone(),
        }),
        None => Ok(default),
    }
}
fn parse_bool(key: &str, value: &str) -> Result<bool, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::Boolean {
            key: key.into(),
            value: value.into(),
        }),
    }
}
fn unquote(value: &str) -> String {
    if value.len() >= 2 {
        let b = value.as_bytes();
        if (b[0] == b'"' && b[value.len() - 1] == b'"')
            || (b[0] == b'\'' && b[value.len() - 1] == b'\'')
        {
            return value[1..value.len() - 1].to_owned();
        }
    }
    value.to_owned()
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };
    #[test]
    fn repository_validation_is_strict() {
        assert!(is_repository("owner/repo"));
        assert!(!is_repository("owner"));
        assert!(!is_repository("owner/repo/extra"));
    }

    #[test]
    fn vault_groups_for_repo_parses_multiple_repos_and_groups() {
        let mut config = Config::default();
        config.vault_group_membership = "owner/a=production,shared;owner/b=staging".to_owned();
        assert_eq!(
            config.vault_groups_for_repo("owner/a"),
            vec!["production", "shared"]
        );
        assert_eq!(config.vault_groups_for_repo("owner/b"), vec!["staging"]);
        assert!(config.vault_groups_for_repo("owner/unlisted").is_empty());
    }

    #[test]
    fn vault_groups_for_repo_skips_malformed_entries() {
        let mut config = Config::default();
        config.vault_group_membership = "malformed-no-equals;owner/a=production".to_owned();
        assert_eq!(config.vault_groups_for_repo("owner/a"), vec!["production"]);
    }

    #[test]
    fn vault_groups_for_repo_handles_empty_membership() {
        let config = Config::default();
        assert!(config.vault_groups_for_repo("owner/a").is_empty());
    }
    #[test]
    fn env_file_does_not_mutate_process_environment() {
        let path = std::env::temp_dir().join(format!(
            "gitrun-config-{}.env",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
    fn invalid_boolean_is_rejected() {
        assert!(parse_bool("TEST", "maybe").is_err());
    }

    #[test]
    fn idle_timeout_must_be_positive() {
        let mut config = Config::default();
        config.idle_timeout = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn duplicate_repositories_are_rejected() {
        let mut config = Config::default();
        config.repositories = vec!["owner/repo".into(), "owner/repo".into()];
        assert!(config.validate().is_err());
    }

    #[test]
    fn docker_resource_limits_are_validated() {
        let mut config = Config::default();
        assert!(config.validate().is_ok());

        config.container_cpus = "0".into();
        assert!(config.validate().is_err());
        config.container_cpus = "0.5".into();

        config.container_memory = "banana".into();
        assert!(config.validate().is_err());
        config.container_memory = "1g".into();

        config.container_pids_limit = "0".into();
        assert!(config.validate().is_err());
        config.container_pids_limit = "1024".into();

        config.runner_home_size = "nope".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn shared_cache_volume_name_is_validated() {
        let mut config = Config::default();
        config.shared_cache_volume = "gitrun-cache.prod-1".into();
        assert!(config.validate().is_ok());
        config.shared_cache_volume = "gitrun/cache".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn container_update_time_is_validated() {
        assert!(validate_time("03:00").is_ok());
        assert!(validate_time("23:59").is_ok());
        assert!(validate_time("24:00").is_err());
        assert!(validate_time("3:00").is_err());
    }

    #[test]
    fn default_config_validates() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn danger_gate_blocks_disabling_hardening_without_explicit_opt_in() {
        let mut config = Config::default();
        config.gsr_docker_socket_hardening = false;
        assert!(config.validate().is_err());
        config.gsr_allow_unsafe_runner = true;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn violation_action_string_is_validated() {
        let mut config = Config::default();
        config.gsr_violation_action = "not-a-real-action".into();
        assert!(config.validate().is_err());
        config.gsr_violation_action = "log_only".into();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn command_policy_respects_master_switch() {
        let mut config = Config::default();
        assert!(config.command_policy().is_some());
        config.gsr_command_policy_enabled = false;
        assert!(config.command_policy().is_none());
    }

    #[test]
    fn command_policy_builds_lists_from_csv_fields() {
        let mut config = Config::default();
        config.gsr_command_blacklist_enabled = true;
        config.gsr_command_blacklist = "rm -rf, curl evil.com ".into();
        let policy = config.command_policy().unwrap();
        assert!(matches!(
            policy.evaluate("rm -rf /tmp"),
            crate::command_policy::Decision::Denied { .. }
        ));
        assert_eq!(
            policy.evaluate("cargo build"),
            crate::command_policy::Decision::Allowed
        );
    }

    #[test]
    fn env_file_loads_gsr_fields() {
        let path = std::env::temp_dir().join(format!(
            "gitrun-config-gsr-{}.env",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, "GITRUN_GSR_COMMAND_WHITELIST_ENABLED=true\nGITRUN_GSR_COMMAND_WHITELIST=cargo,npm\nGITRUN_GSR_VIOLATION_ACTION=kill_and_ban\n").unwrap();
        let config = Config::from_env_file(&path).unwrap();
        fs::remove_file(path).unwrap();
        assert!(config.gsr_command_whitelist_enabled);
        assert_eq!(config.gsr_command_whitelist, "cargo,npm");
        assert_eq!(
            config.violation_action(),
            crate::command_policy::ViolationAction::KillAndBan
        );
    }

    #[test]
    fn zizmor_enabled_requires_license_acceptance() {
        let mut config = Config::default();
        config.gsr_zizmor_enabled = true;
        assert!(config.validate().is_err());
        config.gsr_zizmor_license_accepted = true;
        assert!(config.validate().is_ok());
    }
}
