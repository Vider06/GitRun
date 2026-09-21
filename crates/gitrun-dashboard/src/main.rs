use eframe::egui;
use gitrun_core::{Config, HealthReport, StateStore};
use std::{
    collections::BTreeMap,
    env,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

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
        self.runners.iter().filter(|runner| is_running(&runner.status)).count()
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
struct DashboardSnapshot {
    config: Option<Config>,
    config_error: Option<String>,
    health: Option<HealthReport>,
    crash: Option<String>,
    docker: DockerSnapshot,
    refreshed_at: Option<Instant>,
}

struct Dashboard {
    snapshot: DashboardSnapshot,
    state_dir: PathBuf,
    config_path: Option<PathBuf>,
    last_refresh: Instant,
    refreshing: bool,
}

impl Dashboard {
    fn new() -> Self {
        let state_dir = env::var("GITRUN_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("state"));
        let config_path = env::var("GITRUN_CONFIG_FILE").ok().map(PathBuf::from);
        let mut dashboard = Self {
            snapshot: DashboardSnapshot::default(),
            state_dir,
            config_path,
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
            Some(path) => Config::from_env_file(path).err().map(|error| format!("{}: {error}", path.display())),
            None => Config::from_env().err().map(|error| error.to_string()),
        };

        if let Some(config) = &self.snapshot.config {
            self.state_dir = PathBuf::from(&config.state_dir);
        }

        let state_store = StateStore::new(&self.state_dir);
        self.snapshot.health = state_store.read_health().ok().flatten();
        self.snapshot.crash = state_store.read_last_crash().ok().flatten();
        self.snapshot.docker = read_docker_snapshot();
        self.snapshot.refreshed_at = Some(Instant::now());

        self.last_refresh = Instant::now();
        self.refreshing = false;
    }

    fn render_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("GitRun");
            ui.label("Rust dashboard · read-only");
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

            if let Some(instant) = self.snapshot.refreshed_at {
                let age = instant.elapsed().as_secs();
                ui.label(format!("Last refresh: {age}s ago"));
            } else {
                ui.label("Last refresh: never");
            }

            if ui.button("Refresh").clicked() {
                // Refresh only reads local state and Docker metadata.
                // The dashboard never starts/stops/restarts containers.
                self.refresh();
            }
        });

        if !self.snapshot.docker.daemon_message.is_empty() {
            ui.small(&self.snapshot.docker.daemon_message);
        }

        ui.separator();
    }

    fn render_overview(&self, ui: &mut egui::Ui) {
        let config = self.snapshot.config.as_ref();

        ui.heading("Overview");
        ui.columns(4, |columns| {
            metric(&mut columns[0], "Runner containers", self.snapshot.docker.total().to_string());
            metric(&mut columns[1], "Running", self.snapshot.docker.running().to_string());
            metric(&mut columns[2], "Stopped", self.snapshot.docker.stopped().to_string());
            metric(
                &mut columns[3],
                "Configured repositories",
                config.map(|c| c.repositories.len()).unwrap_or(0).to_string(),
            );
        });
        ui.add_space(8.0);

        ui.horizontal_wrapped(|ui| {
            if let Some(config) = config {
                status_chip(ui, "Pool", &format!("{}..{}", config.min_runners, config.max_runners));
                status_chip(ui, "Poll", &format!("{}s", config.poll_interval));
                status_chip(ui, "Idle timeout", &format!("{}s", config.idle_timeout));
                status_chip(ui, "Ephemeral", if config.ephemeral { "yes" } else { "no" });
            } else {
                status_chip(ui, "Pool", "configuration unavailable");
            }

            if let Some(health) = &self.snapshot.health {
                status_chip(ui, "Health", if health.healthy { "healthy" } else { "unhealthy" });
            } else {
                status_chip(ui, "Health", "not recorded");
            }
        });
    }

    fn render_configuration(&self, ui: &mut egui::Ui) {
        ui.heading("Configuration");

        if let Some(config) = &self.snapshot.config {
            egui::Grid::new("configuration")
                .striped(true)
                .num_columns(2)
                .show(ui, |ui| {
                    ui.strong("Repositories");
                    ui.label(if config.repositories.is_empty() {
                        "none".to_owned()
                    } else {
                        config.repositories.join(", ")
                    });
                    ui.end_row();

                    ui.strong("Runner image");
                    ui.monospace(&config.runner_image);
                    ui.end_row();

                    ui.strong("Runner labels");
                    ui.monospace(&config.runner_labels);
                    ui.end_row();

                    ui.strong("Pool");
                    ui.label(format!("{}..{}", config.min_runners, config.max_runners));
                    ui.end_row();

                    ui.strong("Idle timeout");
                    ui.label(format!("{} seconds", config.idle_timeout));
                    ui.end_row();

                    ui.strong("Poll interval");
                    ui.label(format!("{} seconds", config.poll_interval));
                    ui.end_row();

                    ui.strong("Ephemeral");
                    ui.label(if config.ephemeral { "enabled" } else { "disabled" });
                    ui.end_row();

                    ui.strong("State directory");
                    ui.monospace(&config.state_dir);
                    ui.end_row();

                    ui.strong("Log directory");
                    ui.monospace(&config.log_dir);
                    ui.end_row();
                });
        } else {
            ui.label("Configuration unavailable.");
        }
    }

    fn render_health(&self, ui: &mut egui::Ui) {
        ui.heading("Health & recovery");

        if let Some(health) = &self.snapshot.health {
            ui.horizontal(|ui| {
                ui.label(if health.healthy { "● HEALTHY" } else { "● UNHEALTHY" });
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
            ui.colored_label(egui::Color32::from_rgb(180, 70, 70), format!("Configuration: {error}"));
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
                    ui.label(counts.get(&repository).copied().unwrap_or(0).to_string());
                    if let Some(config) = &self.snapshot.config {
                        ui.label(format!("{}..{}", config.min_runners, config.max_runners));
                    } else {
                        ui.label("—");
                    }
                    ui.end_row();
                }
            });
    }

    fn render_runners(&self, ui: &mut egui::Ui) {
        ui.heading("Runner containers");

        if self.snapshot.docker.runners.is_empty() {
            ui.label("No managed runner containers found.");
            return;
        }

        egui::Grid::new("runners")
            .striped(true)
            .num_columns(5)
            .show(ui, |ui| {
                for heading in ["Name", "Repository", "State", "Image", "Uptime"] {
                    ui.strong(heading);
                }
                ui.end_row();

                for runner in &self.snapshot.docker.runners {
                    ui.monospace(&runner.name);
                    ui.label(&runner.repository);
                    state_label(ui, &runner.status);
                    ui.small(&runner.image);
                    ui.small(&runner.uptime);
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
            "{{.Names}}|{{.Status}}|{{.Image}}|{{.RunningFor}}|{{.Label "gitrun.repo"}}",
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
        repository: if repository.is_empty() { "unknown".into() } else { repository.to_owned() },
        status: status.to_owned(),
        image: image.to_owned(),
        uptime: uptime.to_owned(),
    })
}

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([820.0, 560.0]),
        ..Default::default()
    };

    eframe::run_native(
        "GitRun Dashboard",
        native_options,
        Box::new(|_cc| Ok(Box::new(Dashboard::new()))),
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
}
