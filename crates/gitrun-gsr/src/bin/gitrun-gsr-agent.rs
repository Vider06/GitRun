//! Entry point for `gitrun-gsr-agent` — see `gitrun_gsr::agent` for the
//! full design. This binary is installed inside runner container images as
//! `/bin/sh` (the real shell relocated to `$GITRUN_GSR_REAL_SHELL`, see
//! `docker/runner`), so it must stay a tiny, fast, dependency-light
//! process: it runs on the hot path of every single workflow step.
//!
//! Config and the events path both come from environment variables rather
//! than a config file lookup (`Config::from_env`, `Config::from_env_file`)
//! deliberately: this binary starts fresh once per `run:` step (the runner
//! invokes a new shell per step, not one long-lived shell for the whole
//! job), so re-parsing a file on every single step would add avoidable
//! I/O to a path that already runs many times per job. The runner
//! container's environment already carries `GITRUN_*` config values
//! (injected at container creation - see `gitrun-scheduler::docker`), so
//! reading straight from the process environment is both simpler and
//! faster here than the file-based loaders the host-side binaries use.

fn main() {
    let config = match gitrun_core::Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            // A step must never silently succeed because policy config
            // failed to load — fail loudly and non-zero, same as a real
            // shell would on a syntax error, rather than falling back to
            // "allow everything" (which would make a config bug into a
            // silent security hole).
            eprintln!("gitrun-gsr-agent: failed to load config, refusing to run: {error}");
            std::process::exit(1);
        }
    };
    let events_path = std::env::var("GITRUN_GSR_EVENTS_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| gitrun_gsr::events::default_queue_path(&config.state_dir));

    std::process::exit(gitrun_gsr::agent::run(&events_path, &config));
}
