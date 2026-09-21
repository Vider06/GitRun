use eframe::egui;
use gitrun_core::{Config, HealthReport, StateStore};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const MANAGED_CONFIG_KEYS: [&str; 14] = [
    "GITRUN_REPOSITORIES",
    "GITRUN_MIN_RUNNERS",
    "GITRUN_MAX_RUNNERS",
    "GITRUN_IDLE_TIMEOUT",
    "GITRUN_POLL_INTERVAL",
    "GITRUN_RUNNER_IMAGE",
    "GITRUN_RUNNER_LABELS",
    "GITRUN_EPHEMERAL",
    "GITRUN_STATE_DIR",
    "GITRUN_LOG_DIR",
    "GITRUN_AUTO_CONTAINER_UPDATE",
    "GITRUN_CONTAINER_UPDATE_TIME",
    "GITRUN_AUTO_CONTAINER_RECOVERY",
    "GITRUN_CONTAINER_RECOVERY_COOLDOWN",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunnerSnapshot {
    name: String,
    repository: String,
    status: String,
    image: String,
    uptime: String,
}

#[derive(Debug, Clone, Default)]
struct DockerSnapshot {
    available: bool,
    daemon_message: String,
    runners: Vec<RunnerSnapshot>,
}

impl DockerSnapshot {
    fn total(&self) -> usize {
        self.runners.len()
    }

    fn running(&self) -> usize {
        self.runners
            .iter()
            .filter(|runner| is_running(&runner.status))
            .count()
    }

    fn stopped(&self) -> usize {
        self.total().saturating_sub(self.running())
    }

    fn repository_counts(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for runner in &self.runners {
            *counts.entry(runner.repository.clone()).or_default() += 1;
        }
        counts
    }
}

#[derive(Debug, Clone, Default)]
struct ServiceSnapshot {
    supported: bool,
    active: bool,
    message: String,
}

#[derive(Debug, Clone, Default)]
struct DashboardSnapshot {
    config: Option<Config>,
    config_error: Option<String>,
    health: Option<HealthReport>,
    crash: Option<String>,
    docker: DockerSnapshot,
    service: ServiceSnapshot,
    refreshed_at: Option<Instant>,
}

#[derive(Debug, Clone)]
struct ConfigDraft {
    repositories: String,
    min_runners: u32,
    max_runners: u32,
    idle_timeout: u64,
    poll_interval: u64,
    runner_image: String,
    runner_labels: String,
    ephemeral: bool,
    state_dir: String,
    log_dir: String,
    auto_container_update: bool,
    container_update_time: String,
    auto_container_recovery: bool,
    container_recovery_cooldown: u64,
}

impl From<&Config> for ConfigDraft {
    fn from(config: &Config) -> Self {
        Self {
            repositories: config.repositories.join(","),
            min_runners: config.min_runners,
            max_runners: config.max_runners,
            idle_timeout: config.idle_timeout,
            poll_interval: config.poll_interval,
            runner_image: config.runner_image.clone(),
            runner_labels: config.runner_labels.clone(),
            ephemeral: config.ephemeral,
            state_dir: config.state_dir.clone(),
            log_dir: config.log_dir.clone(),
            auto_container_update: config.auto_container_update,
            container_update_time: config.container_update_time.clone(),
            auto_container_recovery: config.auto_container_recovery,
            container_recovery_cooldown: config.container_recovery_cooldown,
        }
    }
}

impl ConfigDraft {
    fn to_config(&self) -> Result<Config, String> {
        let repositories = self
            .repositories
            .split(',')
            .map(str::trim)
            .filter(|repo| !repo.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();

        let config = Config {
            repositories,
            min_runners: self.min_runners,
            max_runners: self.max_runners,
            idle_timeout: self.idle_timeout,
            poll_interval: self.poll_interval,
            runner_image: self.runner_image.trim().to_owned(),
            runner_labels: self.runner_labels.trim().to_owned(),
            ephemeral: self.ephemeral,
            state_dir: self.state_dir.trim().to_owned(),
            log_dir: self.log_dir.trim().to_owned(),
            auto_container_update: self.auto_container_update,
            container_update_time: self.container_update_time.trim().to_owned(),
            auto_container_recovery: self.auto_container_recovery,
            container_recovery_cooldown: self.container_recovery_cooldown,
        };
        config.validate().map_err(|error| error.to_string())?;
        Ok(config)
    }
}

struct Dashboard {
    snapshot: DashboardSnapshot,
    state_dir: PathBuf,
    config_path: Option<PathBuf>,
    settings: Option<ConfigDraft>,
    settings_dirty: bool,
    action_message: Option<(bool, String)>,
    last_refresh: Instant,
    refreshing: bool,
}

impl Dashboard {
    fn new() -> Self {
        let state_dir = env::var("GITRUN_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("state"));
        let config_path = resolve_config_path();
        let mut dashboard = Self {
            snapshot: DashboardSnapshot::default(),
            state_dir,
            config_path,
            settings: None,
            settings_dirty: false,
            action_message: None,
            last_refresh: Instant::now() - REFRESH_INTERVAL,
            refreshing: false,
        };
        dashboard.refresh();
        dashboard
    }

    fn refresh(&mut self) {
        self.refreshing = true;

        self.snapshot.config = match &self.config_path {
            Some(path) => Config::from_env_file(path).ok(),
            None => Config::from_env().ok(),
        };
        self.snapshot.config_error = match &self.config_path {
            Some(path) => Config::from_env_file(path)
                .err()
                .map(|error| format!("{}: {error}", path.display())),
            None => Config::from_env().err().map(|error| error.to_string()),
        };

        if !self.settings_dirty {
            if let Some(config) = &self.snapshot.config {
                self.settings = Some(ConfigDraft::from(config));
            }
        }

        if let Some(config) = &self.snapshot.config {
            self.state_dir = PathBuf::from(&config.state_dir);
        }

        let state_store = StateStore::new(&self.state_dir);
        self.snapshot.health = state_store.read_health().ok().flatten();
        self.snapshot.crash = state_store.read_last_crash().ok().flatten();
        self.snapshot.docker = read_docker_snapshot();
        self.snapshot.service = read_service_snapshot();
        self.snapshot.refreshed_at = Some(Instant::now());

        self.last_refresh = Instant::now();
        self.refreshing = false;
    }

    fn set_action_message(&mut self, success: bool, message: impl Into<String>) {
        self.action_message = Some((success, message.into()));
    }

    fn save_settings(&mut self) -> Result<(), String> {
        let Some(path) = &self.config_path else {
            return Err(
                "Persistent settings are disabled: set GITRUN_CONFIG_FILE to the GitRun env file."
                    .into(),
            );
        };
        let Some(settings) = &self.settings else {
            return Err("Settings are unavailable until a valid configuration is loaded.".into());
        };

        let config = settings.to_config()?;
        update_env_file(path, &config)?;
        self.settings_dirty = false;
        self.refresh();
        Ok(())
    }

    fn save_settings_and_restart(&mut self) -> Result<(), String> {
        self.save_settings()?;
        service_action("restart")?;
        self.refresh();
        Ok(())
    }

    fn run_runner_action(&mut self, action: &str, runner: &str) {
        match docker_runner_action(action, runner) {
            Ok(message) => {
                self.set_action_message(true, message);
                self.refresh();
            }
            Err(error) => self.set_action_message(false, error),
        }
    }

    fn run_service_action(&mut self, action: &str) {
        match service_action(action) {
            Ok(message) => {
                self.set_action_message(true, message);
                self.refresh();
            }
            Err(error) => self.set_action_message(false, error),
        }
    }

    fn run_container_update(&mut self) {
        let cli = env::var("GITRUN_CLI").unwrap_or_else(|_| "gitrun".into());
        match Command::new(&cli)
            .args(["update", "--only-containers"])
            .output()
        {
            Ok(output) if output.status.success() => {
                let detail = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                self.set_action_message(
                    true,
                    if detail.is_empty() {
                        "GTUU container update completed.".into()
                    } else {
                        detail
                    },
                );
                self.refresh();
            }
            Ok(output) => {
                let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                self.set_action_message(
                    false,
                    if detail.is_empty() {
                        "GTUU container update failed.".into()
                    } else {
                        detail
                    },
                );
            }
            Err(error) => self.set_action_message(false, format!("unable to start GTUU: {error}")),
        }
    }

    fn render_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("GitRun");
            ui.label("Rust dashboard · operator controls");
            ui.add_space(12.0);

            if self.refreshing {
                ui.spinner();
            }
        });

        ui.horizontal(|ui| {
            let status = if self.snapshot.docker.available {
                ("Docker", "available")
            } else {
                ("Docker", "unavailable")
            };
            ui.label(format!("{}: {}", status.0, status.1));

            let service_status = if !self.snapshot.service.supported {
                "service controls unavailable"
            } else if self.snapshot.service.active {
                "service: active"
            } else {
                "service: stopped"
            };
            ui.label(service_status);

            if let Some(instant) = self.snapshot.refreshed_at {
                let age = instant.elapsed().as_secs();
                ui.label(format!("Last refresh: {age}s ago"));
            } else {
                ui.label("Last refresh: never");
            }

            if ui.button("Refresh").clicked() {
                self.refresh();
            }
        });

        if !self.snapshot.docker.daemon_message.is_empty() {
            ui.small(&self.snapshot.docker.daemon_message);
        }

        if let Some((success, message)) = &self.action_message {
            let color = if *success {
                egui::Color32::from_rgb(60, 150, 90)
            } else {
                egui::Color32::from_rgb(190, 70, 70)
            };
            ui.colored_label(color, message);
        }

        ui.separator();
    }

    fn render_overview(&self, ui: &mut egui::Ui) {
        let config = self.snapshot.config.as_ref();

        ui.heading("Overview");
        ui.columns(4, |columns| {
            metric(
                &mut columns[0],
                "Runner containers",
                self.snapshot.docker.total().to_string(),
            );
            metric(
                &mut columns[1],
                "Running",
                self.snapshot.docker.running().to_string(),
            );
            metric(
                &mut columns[2],
                "Stopped",
                self.snapshot.docker.stopped().to_string(),
            );
            metric(
                &mut columns[3],
                "Configured repositories",
                config
                    .map(|c| c.repositories.len())
                    .unwrap_or(0)
                    .to_string(),
            );
        });
        ui.add_space(8.0);

        ui.horizontal_wrapped(|ui| {
            if let Some(config) = config {
                status_chip(
                    ui,
                    "Pool",
                    &format!("{}..{}", config.min_runners, config.max_runners),
                );
                status_chip(ui, "Poll", &format!("{}s", config.poll_interval));
                status_chip(
                    ui,
                    "Idle timeout",
                    &format!("{}s", config.idle_timeout),
                );
                status_chip(
                    ui,
                    "Ephemeral",
                    if config.ephemeral { "yes" } else { "no" },
                );
            } else {
                status_chip(ui, "Pool", "configuration unavailable");
            }

            if let Some(health) = &self.snapshot.health {
                status_chip(
                    ui,
                    "Health",
                    if health.healthy {
                        "healthy"
                    } else {
                        "unhealthy"
                    },
                );
            } else {
                status_chip(ui, "Health", "not recorded");
            }
        });
    }

    fn render_configuration(&mut self, ui: &mut egui::Ui) {
        ui.heading("Configuration");

        if let Some(path) = &self.config_path {
            ui.small(format!("Persistent file: {}", path.display()));
        } else {
            ui.small(
                "Set GITRUN_CONFIG_FILE to edit and persist settings from this dashboard.",
            );
        }

        let Some(settings) = &mut self.settings else {
            ui.label("Configuration unavailable.");
            return;
        };

        ui.add_space(6.0);
        egui::Grid::new("configuration-editor")
            .striped(true)
            .num_columns(2)
            .show(ui, |ui| {
                ui.strong("Repositories");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut settings.repositories)
                            .desired_width(420.0)
                            .hint_text("owner/repository,owner/other"),
                    )
                    .changed()
                {
                    self.settings_dirty = true;
                }
                ui.end_row();

                ui.strong("Minimum runners");
                if ui
                    .add(
                        egui::DragValue::new(&mut settings.min_runners)
                            .range(1..=1024)
                            .speed(1),
                    )
                    .changed()
                {
                    self.settings_dirty = true;
                }
                ui.end_row();

                ui.strong("Maximum runners");
                if ui
                    .add(
                        egui::DragValue::new(&mut settings.max_runners)
                            .range(1..=1024)
                            .speed(1),
                    )
                    .changed()
                {
                    self.settings_dirty = true;
                }
                ui.end_row();

                ui.strong("Idle timeout");
                if ui
                    .add(
                        egui::DragValue::new(&mut settings.idle_timeout)
                            .range(1..=86_400)
                            .speed(1),
                    )
                    .changed()
                {
                    self.settings_dirty = true;
                }
                ui.end_row();

                ui.strong("Poll interval");
                if ui
                    .add(
                        egui::DragValue::new(&mut settings.poll_interval)
                            .range(1..=3_600)
                            .speed(1),
                    )
                    .changed()
                {
                    self.settings_dirty = true;
                }
                ui.end_row();

                ui.strong("Runner image");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut settings.runner_image)
                            .desired_width(420.0),
                    )
                    .changed()
                {
                    self.settings_dirty = true;
                }
                ui.end_row();

                ui.strong("Runner labels");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut settings.runner_labels)
                            .desired_width(420.0),
                    )
                    .changed()
                {
                    self.settings_dirty = true;
                }
                ui.end_row();

                ui.strong("Ephemeral runners");
                if ui.checkbox(&mut settings.ephemeral, "").changed() {
                    self.settings_dirty = true;
                }
                ui.end_row();

                ui.strong("Automatic container recovery");
                if ui.checkbox(&mut settings.auto_container_recovery, "").changed() {
                    self.settings_dirty = true;
                }
                ui.end_row();

                ui.strong("Container recovery cooldown");
                if ui
                    .add(egui::DragValue::new(&mut settings.container_recovery_cooldown).range(15..=86_400).speed(1))
                    .changed()
                {
                    self.settings_dirty = true;
                }
                ui.end_row();

                ui.strong("Automatic container updates");
                if ui.checkbox(&mut settings.auto_container_update, "").changed() {
                    self.settings_dirty = true;
                }
                ui.end_row();

                ui.strong("Container update time");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut settings.container_update_time)
                            .desired_width(120.0)
                            .hint_text("03:00"),
                    )
                    .changed()
                {
                    self.settings_dirty = true;
                }
                ui.end_row();

                ui.strong("State directory");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut settings.state_dir)
                            .desired_width(420.0),
                    )
                    .changed()
                {
                    self.settings_dirty = true;
                }
                ui.end_row();

                ui.strong("Log directory");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut settings.log_dir)
                            .desired_width(420.0),
                    )
                    .changed()
                {
                    self.settings_dirty = true;
                }
                ui.end_row();
            });

        if self.settings_dirty {
            ui.colored_label(
                egui::Color32::from_rgb(210, 160, 50),
                "Unsaved changes",
            );
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let save_enabled = self.settings_dirty && self.config_path.is_some();
            if ui
                .add_enabled(save_enabled, egui::Button::new("Save settings"))
                .clicked()
            {
                match self.save_settings() {
                    Ok(()) => self.set_action_message(true, "Settings saved."),
                    Err(error) => self.set_action_message(false, error),
                }
            }

            if ui
                .add_enabled(
                    save_enabled && self.snapshot.service.supported,
                    egui::Button::new("Save + restart GitRun"),
                )
                .clicked()
            {
                match self.save_settings_and_restart() {
                    Ok(()) => self.set_action_message(
                        true,
                        "Settings saved and GitRun service restarted.",
                    ),
                    Err(error) => self.set_action_message(false, error),
                }
            }

            if ui.button("Update containers now").clicked() {
                self.run_container_update();
            }

            if ui.button("Discard edits").clicked() {
                self.settings = self.snapshot.config.as_ref().map(ConfigDraft::from);
                self.settings_dirty = false;
            }
        });

        ui.small("GITHUB_TOKEN and other unknown env keys are preserved and are never shown.");
    }

    fn render_service_controls(&mut self, ui: &mut egui::Ui) {
        ui.heading("Service controls");

        if !self.snapshot.service.supported {
            ui.label("Systemd service controls are available only on Linux.");
            return;
        }

        ui.horizontal(|ui| {
            let active = self.snapshot.service.active;
            if ui
                .add_enabled(!active, egui::Button::new("Start GitRun"))
                .clicked()
            {
                self.run_service_action("start");
            }
            if ui
                .add_enabled(active, egui::Button::new("Stop GitRun"))
                .clicked()
            {
                self.run_service_action("stop");
            }
            if ui.button("Restart GitRun").clicked() {
                self.run_service_action("restart");
            }
        });

        if !self.snapshot.service.message.is_empty() {
            ui.small(&self.snapshot.service.message);
        }
    }

    fn render_health(&self, ui: &mut egui::Ui) {
        ui.heading("Health & recovery");

        if let Some(health) = &self.snapshot.health {
            ui.horizontal(|ui| {
                ui.label(if health.healthy {
                    "● HEALTHY"
                } else {
                    "● UNHEALTHY"
                });
                ui.label(&health.message);
                ui.monospace(format!("checked_at={}", health.checked_at));
            });
        } else {
            ui.label("No persisted health report found.");
        }

        ui.add_space(4.0);
        if let Some(crash) = &self.snapshot.crash {
            ui.collapsing("Last recorded crash", |ui| {
                ui.monospace(crash);
            });
        } else {
            ui.label("No recorded crash.");
        }

        if let Some(error) = &self.snapshot.config_error {
            ui.colored_label(
                egui::Color32::from_rgb(180, 70, 70),
                format!("Configuration: {error}"),
            );
        }
    }

    fn render_repositories(&self, ui: &mut egui::Ui) {
        ui.heading("Repositories");

        let configured = self
            .snapshot
            .config
            .as_ref()
            .map(|config| config.repositories.clone())
            .unwrap_or_default();
        let counts = self.snapshot.docker.repository_counts();

        if configured.is_empty() {
            ui.label("No repositories configured.");
            return;
        }

        egui::Grid::new("repositories")
            .striped(true)
            .num_columns(3)
            .show(ui, |ui| {
                ui.strong("Repository");
                ui.strong("Runner containers");
                ui.strong("Target pool");
                ui.end_row();

                for repository in configured {
                    ui.label(&repository);
                    ui.label(
                        counts
                            .get(&repository)
                            .copied()
                            .unwrap_or(0)
                            .to_string(),
                    );
                    if let Some(config) = &self.snapshot.config {
                        ui.label(format!("{}..{}", config.min_runners, config.max_runners));
                    } else {
                        ui.label("—");
                    }
                    ui.end_row();
                }
            });
    }

    fn render_runners(&mut self, ui: &mut egui::Ui) {
        ui.heading("Runner containers");

        if self.snapshot.docker.runners.is_empty() {
            ui.label("No managed runner containers found.");
            return;
        }

        egui::Grid::new("runners")
            .striped(true)
            .num_columns(8)
            .show(ui, |ui| {
                for heading in [
                    "Name",
                    "Repository",
                    "State",
                    "Image",
                    "Uptime",
                    "Actions",
                    "",
                    "",
                ] {
                    ui.strong(heading);
                }
                ui.end_row();

                for runner in self.snapshot.docker.runners.clone() {
                    ui.monospace(&runner.name);
                    ui.label(&runner.repository);
                    state_label(ui, &runner.status);
                    ui.small(&runner.image);
                    ui.small(&runner.uptime);

                    let running = is_running(&runner.status);
                    if ui
                        .add_enabled(!running, egui::Button::new("Start"))
                        .clicked()
                    {
                        self.run_runner_action("start", &runner.name);
                    }
                    if ui
                        .add_enabled(running, egui::Button::new("Stop"))
                        .clicked()
                    {
                        self.run_runner_action("stop", &runner.name);
                    }
                    if ui.button("Restart").clicked() {
                        self.run_runner_action("restart", &runner.name);
                    }
                    ui.small(if running {
                        "stop may be reverted by autoscaling"
                    } else {
                        ""
                    });
                    ui.end_row();
                }
            });
    }
}

impl eframe::App for Dashboard {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.last_refresh.elapsed() >= REFRESH_INTERVAL && !self.refreshing {
            self.refresh();
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| self.render_header(ui));

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.render_overview(ui);
                ui.add_space(18.0);
                self.render_configuration(ui);
                ui.add_space(18.0);
                self.render_service_controls(ui);
                ui.add_space(18.0);
                self.render_health(ui);
                ui.add_space(18.0);
                self.render_repositories(ui);
                ui.add_space(18.0);
                self.render_runners(ui);
            });
        });

        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

fn resolve_config_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("GITRUN_CONFIG_FILE") {
        return Some(PathBuf::from(path));
    }

    [
        PathBuf::from("config/gitrun.env"),
        PathBuf::from("/etc/gitrun/gitrun.env"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

fn update_env_file(path: &PathBuf, config: &Config) -> Result<(), String> {
    let original = fs::read_to_string(path)
        .map_err(|error| format!("unable to read {}: {error}", path.display()))?;
    let values = config_env_values(config);
    let managed = MANAGED_CONFIG_KEYS.iter().copied().collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();

    for line in original.lines() {
        let trimmed = line.trim();
        let key = trimmed
            .split_once('=')
            .map(|(key, _)| key.trim());

        if let Some(key) = key.filter(|key| managed.contains(key)) {
            output.push(format!("{key}={}", values[key]));
            seen.insert(key.to_owned());
        } else {
            output.push(line.to_owned());
        }
    }

    if !output.is_empty() && !output.last().is_some_and(|line| line.is_empty()) {
        output.push(String::new());
    }

    for key in MANAGED_CONFIG_KEYS {
        if !seen.contains(key) {
            output.push(format!("{key}={}", values[key]));
        }
    }

    let mut rendered = output.join("\n");
    rendered.push('\n');
    fs::write(path, rendered)
        .map_err(|error| format!("unable to write {}: {error}", path.display()))
}

fn config_env_values(config: &Config) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        ("GITRUN_REPOSITORIES", config.repositories.join(",")),
        ("GITRUN_MIN_RUNNERS", config.min_runners.to_string()),
        ("GITRUN_MAX_RUNNERS", config.max_runners.to_string()),
        ("GITRUN_IDLE_TIMEOUT", config.idle_timeout.to_string()),
        ("GITRUN_POLL_INTERVAL", config.poll_interval.to_string()),
        ("GITRUN_RUNNER_IMAGE", config.runner_image.clone()),
        ("GITRUN_RUNNER_LABELS", config.runner_labels.clone()),
        ("GITRUN_EPHEMERAL", config.ephemeral.to_string()),
        ("GITRUN_STATE_DIR", config.state_dir.clone()),
        ("GITRUN_LOG_DIR", config.log_dir.clone()),
        ("GITRUN_AUTO_CONTAINER_UPDATE", config.auto_container_update.to_string()),
        ("GITRUN_CONTAINER_UPDATE_TIME", config.container_update_time.clone()),
        ("GITRUN_AUTO_CONTAINER_RECOVERY", config.auto_container_recovery.to_string()),
        ("GITRUN_CONTAINER_RECOVERY_COOLDOWN", config.container_recovery_cooldown.to_string()),
    ])
}

fn service_name() -> String {
    env::var("GITRUN_SERVICE_NAME").unwrap_or_else(|_| "gitrun".into())
}

fn read_service_snapshot() -> ServiceSnapshot {
    if !cfg!(target_os = "linux") {
        return ServiceSnapshot {
            supported: false,
            active: false,
            message: "systemd is only available on Linux.".into(),
        };
    }

    let service = service_name();
    match Command::new("systemctl")
        .args(["is-active", &service])
        .output()
    {
        Ok(output) => ServiceSnapshot {
            supported: true,
            active: output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim() == "active",
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        },
        Err(error) => ServiceSnapshot {
            supported: true,
            active: false,
            message: format!("systemctl unavailable: {error}"),
        },
    }
}

fn service_action(action: &str) -> Result<String, String> {
    if !cfg!(target_os = "linux") {
        return Err("System service controls are supported only on Linux.".into());
    }

    let service = service_name();
    let output = Command::new("systemctl")
        .args([action, &service])
        .output()
        .map_err(|error| format!("unable to run systemctl: {error}"))?;

    if output.status.success() {
        Ok(format!("systemctl {action} {service}: completed"))
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(if detail.is_empty() {
            format!("systemctl {action} {service} failed")
        } else {
            format!("systemctl {action} {service} failed: {detail}")
        })
    }
}

fn docker_runner_action(action: &str, runner: &str) -> Result<String, String> {
    let output = Command::new("docker")
        .args([action, runner])
        .output()
        .map_err(|error| format!("unable to run docker: {error}"))?;

    if output.status.success() {
        Ok(format!("docker {action} {runner}: completed"))
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(if detail.is_empty() {
            format!("docker {action} {runner} failed")
        } else {
            format!("docker {action} {runner} failed: {detail}")
        })
    }
}

fn metric(ui: &mut egui::Ui, title: &str, value: String) {
    ui.group(|ui| {
        ui.small(title);
        ui.heading(value);
    });
}

fn status_chip(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.group(|ui| {
        ui.small(label);
        ui.label(value);
    });
}

fn state_label(ui: &mut egui::Ui, status: &str) {
    if is_running(status) {
        ui.colored_label(egui::Color32::from_rgb(70, 150, 90), "RUNNING");
    } else {
        ui.colored_label(egui::Color32::from_rgb(180, 110, 70), "STOPPED");
    }
    ui.small(status);
}

fn is_running(status: &str) -> bool {
    let normalized = status.trim().to_ascii_lowercase();
    normalized.starts_with("up ") || normalized == "up"
}

fn read_docker_snapshot() -> DockerSnapshot {
    let output = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            "label=gitrun.runner=true",
            "--format",
            "{{.Names}}|{{.Status}}|{{.Image}}|{{.RunningFor}}|{{.Label \"gitrun.repo\"}}",
        ])
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let runners = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(parse_runner_line)
                .collect::<Vec<_>>();

            DockerSnapshot {
                available: true,
                daemon_message: String::new(),
                runners,
            }
        }
        Ok(output) => DockerSnapshot {
            available: false,
            daemon_message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            runners: Vec::new(),
        },
        Err(error) => DockerSnapshot {
            available: false,
            daemon_message: format!("docker command unavailable: {error}"),
            runners: Vec::new(),
        },
    }
}

fn parse_runner_line(line: &str) -> Option<RunnerSnapshot> {
    let mut parts = line.split('|');
    let name = parts.next()?.trim();
    let status = parts.next()?.trim();
    let image = parts.next()?.trim();
    let uptime = parts.next()?.trim();
    let repository = parts.next()?.trim();

    if name.is_empty() {
        return None;
    }

    Some(RunnerSnapshot {
        name: name.to_owned(),
        repository: if repository.is_empty() {
            "unknown".into()
        } else {
            repository.to_owned()
        },
        status: status.to_owned(),
        image: image.to_owned(),
        uptime: uptime.to_owned(),
    })
}

#[derive(Default)]
struct SetupWizard {
    token: String,
    repositories: String,
    busy: bool,
    status: String,
    result_rx: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
}

impl SetupWizard {
    fn new() -> Self {
        Self {
            token: String::new(),
            repositories: String::new(),
            busy: false,
            status: "GitRun needs a one-time setup before the dashboard can start.".into(),
            result_rx: None,
        }
    }

    fn begin_setup(&mut self) {
        let token = self.token.trim().to_owned();
        let repositories = self.repositories.trim().to_owned();
        if token.is_empty() {
            self.status = "Enter a GitHub token.".into();
            return;
        }
        if repositories.is_empty() || repositories.split(',').any(|repo| {
            let repo = repo.trim();
            !repo.contains('/') || repo.starts_with('/') || repo.ends_with('/')
        }) {
            self.status = "Enter at least one repository as owner/repository.".into();
            return;
        }
        if Command::new("pkexec").arg("--version").output().is_err() {
            self.status = "pkexec is required for the graphical first-run setup. Run GitRun from a desktop Ubuntu session with PolicyKit enabled.".into();
            return;
        }

        let path = std::env::temp_dir().join(format!(
            "gitrun-setup-{}.conf",
            std::process::id()
        ));
        let payload = format!("{token}
{repositories}
");
        if let Err(error) = fs::write(&path, payload) {
            self.status = format!("Unable to prepare setup request: {error}");
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }

        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                let _ = fs::remove_file(&path);
                self.status = format!("Unable to locate GitRun executable: {error}");
                return;
            }
        };

        let (tx, rx) = std::sync::mpsc::channel();
        self.result_rx = Some(rx);
        self.busy = true;
        self.status = "Installing Docker, preparing GitRun and starting the manager…".into();

        std::thread::spawn(move || {
            let result = Command::new("pkexec")
                .arg(executable)
                .arg("--install-root")
                .arg(&path)
                .output()
                .map_err(|error| error.to_string())
                .and_then(|output| {
                    if output.status.success() {
                        Ok(())
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                        Err(if stderr.is_empty() { stdout } else { stderr })
                    }
                });
            let _ = fs::remove_file(&path);
            let _ = tx.send(result);
        });
    }
}

impl eframe::App for SetupWizard {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(rx) = &self.result_rx {
            match rx.try_recv() {
                Ok(Ok(())) => {
                    self.busy = false;
                    self.status = "GitRun is installed. Launching the dashboard…".into();
                    let _ = Command::new("sg")
                        .args(["docker", "-c", "/usr/local/bin/gitrun dashboard"])
                        .spawn();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Ok(Err(error)) => {
                    self.busy = false;
                    self.status = if error.is_empty() {
                        "Setup failed.".into()
                    } else {
                        format!("Setup failed: {error}")
                    };
                    self.result_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.busy = false;
                    self.status = "Setup process ended unexpectedly.".into();
                    self.result_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(90.0);
                ui.heading("Welcome to GitRun");
                ui.label("One-time setup. After this, launching GitRun opens the dashboard.");
                ui.add_space(24.0);

                egui::Grid::new("first-run-setup")
                    .num_columns(2)
                    .spacing([12.0, 14.0])
                    .show(ui, |ui| {
                        ui.strong("GitHub token");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.token)
                                .desired_width(460.0)
                                .password(true),
                        );
                        ui.end_row();

                        ui.strong("Repositories");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.repositories)
                                .desired_width(460.0)
                                .hint_text("owner/repository,owner/other"),
                        );
                        ui.end_row();
                    });

                ui.add_space(20.0);
                if self.busy {
                    ui.spinner();
                } else if ui.button("Set up GitRun").clicked() {
                    self.begin_setup();
                }
                ui.add_space(14.0);
                ui.small(&self.status);
                ui.add_space(16.0);
                ui.small("The setup installs Docker when necessary, creates the GitRun service, builds the runner image, and starts the manager.");
            });
        });

        if self.busy {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

pub fn run() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 900.0])
            .with_min_inner_size([980.0, 620.0]),
        ..Default::default()
    };
    let first_run = resolve_config_path().is_none();

    eframe::run_native(
        "GitRun",
        native_options,
        Box::new(move |_cc| {
            if first_run {
                Ok(Box::new(SetupWizard::new()))
            } else {
                Ok(Box::new(Dashboard::new()))
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_runner_row() {
        let row = "gitrun-owner-repo-ab12|Up 4 minutes|gitrun-runner:latest|4 minutes|owner/repo";
        let parsed = parse_runner_line(row).unwrap();
        assert_eq!(parsed.name, "gitrun-owner-repo-ab12");
        assert_eq!(parsed.repository, "owner/repo");
        assert!(is_running(&parsed.status));
    }

    #[test]
    fn rejects_empty_runner_name() {
        assert!(parse_runner_line("|Up 1 minute|image|1 minute|owner/repo").is_none());
    }

    #[test]
    fn detects_stopped_runner() {
        assert!(!is_running("Exited (1) 20 seconds ago"));
    }

    #[test]
    fn env_update_preserves_secrets_and_unknown_keys() {
        let config = Config {
            repositories: vec!["owner/repo".into()],
            min_runners: 2,
            max_runners: 4,
            idle_timeout: 90,
            poll_interval: 7,
            runner_image: "gitrun-runner:new".into(),
            runner_labels: "self-hosted,Linux,X64".into(),
            ephemeral: true,
            state_dir: "/var/lib/gitrun".into(),
            log_dir: "/var/log/gitrun".into(),
            auto_container_update: true,
            container_update_time: "03:00".into(),
        };
        let original = "GITHUB_TOKEN=secret\nGITRUN_MIN_RUNNERS=3\nCUSTOM=value\n";
        let path = std::env::temp_dir().join(format!(
            "gitrun-dashboard-config-{}",
            std::process::id()
        ));
        fs::write(&path, original).unwrap();
        update_env_file(&path, &config).unwrap();
        let rendered = fs::read_to_string(&path).unwrap();
        fs::remove_file(path).unwrap();

        assert!(rendered.contains("GITHUB_TOKEN=secret"));
        assert!(rendered.contains("CUSTOM=value"));
        assert!(rendered.contains("GITRUN_MIN_RUNNERS=2"));
        assert!(rendered.contains("GITRUN_EPHEMERAL=true"));
    }
}
