//! Entry point for gitrun-gsr-agent.
//!
//! The protected policy snapshot is authoritative. Normal shell invocations
//! use the lightweight shell layer; runner-container startup passes
//! --supervise-runner so PID 1 becomes the kernel-backed GSR supervisor.

use gitrun_core::Config;
use std::path::Path;

const GSR_POLICY_FILE: &str = "/run/gitrun/gsr-policy.env";
const GSR_EVENTS_PATH: &str = "/run/gitrun/gsr-events.jsonl";
const SUPERVISE_ARG: &str = "--supervise-runner";

fn main() {
    let config = match Config::from_env_file(GSR_POLICY_FILE) {
        Ok(config) => config,
        Err(error) => {
            eprintln!(
                "gitrun-gsr-agent: failed to load protected policy from {GSR_POLICY_FILE}, refusing to run: {error}"
            );
            std::process::exit(1);
        }
    };

    let events_path = Path::new(GSR_EVENTS_PATH);
    let supervise = std::env::args().nth(1).as_deref() == Some(SUPERVISE_ARG);

    let exit_code = if supervise {
        gitrun_gsr::agent::supervise_runner(events_path, &config)
    } else {
        gitrun_gsr::agent::run(events_path, &config)
    };

    std::process::exit(exit_code);
}
