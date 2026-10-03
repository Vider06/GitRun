//! GSR entry point: a standalone binary, deliberately separate from
//! `gitrun-autoscaler`, so it keeps running (and can observe) even if the
//! process it watches crashes hard. Intended to run as its own systemd unit
//! (not written in this pass — see `systemd/gitrun.service` for the
//! existing pattern to follow), independent of the manager container.

use gitrun_core::Config;
use gitrun_gsr::WatchConfig;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn main() {
    let stopping = Arc::new(AtomicBool::new(false));
    if let Err(error) = install_signal_handlers(&stopping) {
        eprintln!("gitrun-gsr: failed to install signal handlers: {error}");
        std::process::exit(1);
    }

    let config = match load_config() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("gitrun-gsr: invalid configuration: {error}");
            std::process::exit(2);
        }
    };

    let watch_config = WatchConfig {
        pid_file: std::path::PathBuf::from(&config.state_dir).join("gitrun-autoscaler.pid"),
        events_path: gitrun_gsr::events::default_queue_path(&config.state_dir),
        poll_interval: Duration::from_secs(5),
        watched_name: "gitrun-autoscaler".into(),
    };

    println!(
        "gitrun-gsr: watching {} (pid file: {})",
        watch_config.watched_name,
        watch_config.pid_file.display()
    );
    gitrun_gsr::run_watchdog(&watch_config, || stopping.load(Ordering::Relaxed));
    println!("gitrun-gsr: stopped");
}

fn load_config() -> Result<Config, gitrun_core::ConfigError> {
    match std::env::var("GITRUN_CONFIG_FILE") {
        Ok(path) => Config::from_env_file(path),
        Err(_) => Config::from_env(),
    }
}

fn install_signal_handlers(stopping: &Arc<AtomicBool>) -> Result<(), std::io::Error> {
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stopping.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, stopping.clone())?;
    Ok(())
}
