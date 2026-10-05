/// The runner Dockerfile remains the repository source of truth. The bootstrap
/// uses the exact same image definition, with one deliberate adjustment: a
/// packaged GitRun installation does not ship the entire workspace, so the
/// build context is reduced to the two crates needed by the GSR shell agent.
pub(crate) fn runner_dockerfile_for_bootstrap() -> String {
    const FULL_WORKSPACE_COPY: &str = "COPY Cargo.toml Cargo.lock ./\nCOPY crates ./crates";
    const MINIMAL_WORKSPACE_COPY: &str = concat!(
        "COPY Cargo.toml Cargo.lock ./\n",
        "COPY crates/gitrun-core ./crates/gitrun-core\n",
        "COPY crates/gitrun-gsr ./crates/gitrun-gsr"
    );

    let source = include_str!("../../../docker/runner/Dockerfile");
    if !source.contains(FULL_WORKSPACE_COPY) {
        panic!("runner Dockerfile bootstrap adaptation marker is missing");
    }
    let source = source.replace(FULL_WORKSPACE_COPY, MINIMAL_WORKSPACE_COPY);

    if source.contains(FULL_WORKSPACE_COPY) {
        panic!("runner Dockerfile bootstrap adaptation marker was not replaced");
    }
    if source.contains("COPY crates ./crates") {
        panic!("bootstrap Dockerfile must not require the full workspace");
    }
    source
}

pub(crate) const RUNNER_ENTRYPOINT: &str = include_str!("../../../docker/runner/entrypoint.sh");

pub(crate) const SYSTEMD_SERVICE: &str = include_str!("../../../systemd/gitrun.service");

pub(crate) const SYSTEMD_SERVICE_DIRECT: &str = r#"[Unit]
Description=GitRun Rust scheduler
Requires=docker.service
After=docker.service
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/gitrun scheduler
Restart=on-failure
RestartSec=5
TimeoutStopSec=60
KillMode=control-group
EnvironmentFile=/etc/gitrun/gitrun.env

[Install]
WantedBy=multi-user.target
"#;

/// The bootstrap needs a small self-contained Cargo workspace for building the
/// GSR agent after the GitRun package has been installed. This intentionally
/// contains only the crates required by gitrun-gsr-agent; it is not the
/// application's main workspace manifest.
pub(crate) const RUNNER_BUILD_CARGO_MANIFEST: &str = r#"[workspace]
resolver = "2"
members = ["crates/gitrun-core", "crates/gitrun-gsr"]

[patch.crates-io]
# Keep the bootstrap workspace's Cargo resolution identical to the repository
# workspace even though only the GSR-related crates are copied into the image.
glib = { git = "https://github.com/jcfs/gtk-rs-core", rev = "ea720152f28e293ef4362ee844ee5cc499f32d2a" }
glib-sys = { git = "https://github.com/jcfs/gtk-rs-core", rev = "ea720152f28e293ef4362ee844ee5cc499f32d2a" }
gobject-sys = { git = "https://github.com/jcfs/gtk-rs-core", rev = "ea720152f28e293ef4362ee844ee5cc499f32d2a" }
gio-sys = { git = "https://github.com/jcfs/gtk-rs-core", rev = "ea720152f28e293ef4362ee844ee5cc499f32d2a" }
glib-macros = { git = "https://github.com/jcfs/gtk-rs-core", rev = "ea720152f28e293ef4362ee844ee5cc499f32d2a" }
"#;

pub(crate) const RUNNER_BUILD_FILES: &[(&str, &str, u32)] = &[
    ("Cargo.toml", RUNNER_BUILD_CARGO_MANIFEST, 0o644),
    ("Cargo.lock", include_str!("../../../Cargo.lock"), 0o644),
    (
        "crates/gitrun-core/Cargo.toml",
        include_str!("../../../crates/gitrun-core/Cargo.toml"),
        0o644,
    ),
    (
        "crates/gitrun-core/src/lib.rs",
        include_str!("../../../crates/gitrun-core/src/lib.rs"),
        0o644,
    ),
    (
        "crates/gitrun-core/src/app_auth.rs",
        include_str!("../../../crates/gitrun-core/src/app_auth.rs"),
        0o644,
    ),
    (
        "crates/gitrun-core/src/command_policy.rs",
        include_str!("../../../crates/gitrun-core/src/command_policy.rs"),
        0o644,
    ),
    (
        "crates/gitrun-core/src/config.rs",
        include_str!("../../../crates/gitrun-core/src/config.rs"),
        0o644,
    ),
    (
        "crates/gitrun-core/src/github_auth.rs",
        include_str!("../../../crates/gitrun-core/src/github_auth.rs"),
        0o644,
    ),
    (
        "crates/gitrun-core/src/hypervisor_decision.rs",
        include_str!("../../../crates/gitrun-core/src/hypervisor_decision.rs"),
        0o644,
    ),
    (
        "crates/gitrun-core/src/runner.rs",
        include_str!("../../../crates/gitrun-core/src/runner.rs"),
        0o644,
    ),
    (
        "crates/gitrun-core/src/state.rs",
        include_str!("../../../crates/gitrun-core/src/state.rs"),
        0o644,
    ),
    (
        "crates/gitrun-core/src/workflow_validation.rs",
        include_str!("../../../crates/gitrun-core/src/workflow_validation.rs"),
        0o644,
    ),
    (
        "crates/gitrun-gsr/Cargo.toml",
        include_str!("../../../crates/gitrun-gsr/Cargo.toml"),
        0o644,
    ),
    (
        "crates/gitrun-gsr/src/lib.rs",
        include_str!("../../../crates/gitrun-gsr/src/lib.rs"),
        0o644,
    ),
    (
        "crates/gitrun-gsr/src/agent.rs",
        include_str!("../../../crates/gitrun-gsr/src/agent.rs"),
        0o644,
    ),
    (
        "crates/gitrun-gsr/src/events.rs",
        include_str!("../../../crates/gitrun-gsr/src/events.rs"),
        0o644,
    ),
    (
        "crates/gitrun-gsr/src/main.rs",
        include_str!("../../../crates/gitrun-gsr/src/main.rs"),
        0o644,
    ),
    (
        "crates/gitrun-gsr/src/watchdog.rs",
        include_str!("../../../crates/gitrun-gsr/src/watchdog.rs"),
        0o644,
    ),
    (
        "crates/gitrun-gsr/src/exec_supervisor.rs",
        include_str!("../../../crates/gitrun-gsr/src/exec_supervisor.rs"),
        0o644,
    ),
    (
        "crates/gitrun-gsr/src/bin/gitrun-gsr-agent.rs",
        include_str!("../../../crates/gitrun-gsr/src/bin/gitrun-gsr-agent.rs"),
        0o644,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_dockerfile_uses_minimal_gsr_build_context() {
        let dockerfile = runner_dockerfile_for_bootstrap();
        assert!(dockerfile.contains("COPY crates/gitrun-core ./crates/gitrun-core"));
        assert!(dockerfile.contains("COPY crates/gitrun-gsr ./crates/gitrun-gsr"));
        assert!(!dockerfile.contains("COPY crates ./crates"));
        assert!(dockerfile.contains("cargo build --locked --release -p gitrun-gsr"));
    }

    #[test]
    fn bootstrap_manifest_preserves_workspace_patches_for_locked_resolution() {
        assert!(RUNNER_BUILD_CARGO_MANIFEST.contains("[patch.crates-io]"));
        assert!(RUNNER_BUILD_CARGO_MANIFEST.contains("gtk-rs-core"));
        assert!(RUNNER_BUILD_CARGO_MANIFEST.contains(
            "ea720152f28e293ef4362ee844ee5cc499f32d2a"
        ));
    }

    #[test]
    fn bootstrap_resources_include_every_required_source() {
        let paths: Vec<_> = RUNNER_BUILD_FILES
            .iter()
            .map(|(path, _, _)| *path)
            .collect();
        assert!(paths.contains(&"Cargo.toml"));
        assert!(paths.contains(&"Cargo.lock"));
        assert!(paths.contains(&"crates/gitrun-core/src/lib.rs"));
        assert!(paths.contains(&"crates/gitrun-gsr/src/bin/gitrun-gsr-agent.rs"));
        assert!(paths.contains(&"crates/gitrun-gsr/src/exec_supervisor.rs"));
        assert_eq!(
            RUNNER_ENTRYPOINT,
            include_str!("../../../docker/runner/entrypoint.sh")
        );
        assert!(RUNNER_ENTRYPOINT.contains("docker_socket_group"));
        assert!(RUNNER_ENTRYPOINT.contains("gitrun-ci"));
        assert_eq!(
            SYSTEMD_SERVICE,
            include_str!("../../../systemd/gitrun.service")
        );
        assert!(SYSTEMD_SERVICE.contains("ExecStart=/usr/local/bin/gitrun scheduler"));
        assert!(!SYSTEMD_SERVICE.contains("docker compose"));
    }
}
