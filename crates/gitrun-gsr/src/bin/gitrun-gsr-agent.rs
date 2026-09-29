//! Entry point for `gitrun-gsr-agent`.
//!
//! Security policy is loaded exclusively from the root-owned policy snapshot
//! created by the runner entrypoint before the GitHub Actions runner starts.
//! The workflow environment is intentionally not consulted for policy values:
//! jobs are attacker-controlled input and must never be able to disable or
//! rewrite the enforcement configuration.

use gitrun_core::Config;
use std::path::Path;

const GSR_POLICY_FILE: &str = "/run/gitrun/gsr-policy.env";
const GSR_EVENTS_PATH: &str = "/run/gitrun/gsr-events.jsonl";

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
    std::process::exit(gitrun_gsr::agent::run(events_path, &config));
}
