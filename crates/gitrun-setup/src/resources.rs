use super::SetupError;

/// The runner Dockerfile is maintained in GitRun Premade and pinned by the
/// updater crate. Setup fetches that immutable source and applies the same
/// minimal-build-context adaptation used by the existing installer.
pub(crate) fn runner_dockerfile_for_bootstrap() -> Result<String, SetupError> {
    let source = gitrun_updater::fetch_premade_runner_dockerfile(
        &gitrun_updater::canonical_premade_runner_dockerfile(),
    )
    .map_err(|error| {
        SetupError::Command(format!(
            "unable to fetch the canonical Premade runner Dockerfile: {error}"
        ))
    })?;
    runner_dockerfile_from_source(&source)
}

fn runner_dockerfile_from_source(source: &str) -> Result<String, SetupError> {
    const FULL_WORKSPACE_COPY: &str = "COPY Cargo.toml Cargo.lock ./\nCOPY crates ./crates";
    const MINIMAL_WORKSPACE_COPY: &str = concat!(
        "COPY Cargo.toml Cargo.lock ./\n",
        "COPY crates/gitrun-core ./crates/gitrun-core\n",
        "COPY crates/gitrun-exe ./crates/gitrun-exe\n",
        "COPY crates/gitrun-gsr ./crates/gitrun-gsr"
    );

    if !source.contains(FULL_WORKSPACE_COPY) {
        return Err(SetupError::Command(
            "Premade runner Dockerfile is missing the expected workspace marker".into(),
        ));
    }
    let adapted = source.replace(FULL_WORKSPACE_COPY, MINIMAL_WORKSPACE_COPY);

    if adapted.contains(FULL_WORKSPACE_COPY) || adapted.contains("COPY crates ./crates") {
        return Err(SetupError::Command(
            "Premade runner Dockerfile could not be reduced to the setup build context".into(),
        ));
    }
    Ok(adapted)
}

/// Resolve an official, compiled-in runner profile. Arbitrary remote Dockerfiles
/// are never executed by the privileged bootstrap path.
pub(crate) fn runner_dockerfile_for_profile(profile: &str) -> Result<String, SetupError> {
    match profile {
        "minimum" => Ok(include_str!(
            "../../../Core/Dockers/runners/linux-x86_64/minimum/Dockerfile"
        )
        .to_owned()),
        "workbench" => runner_dockerfile_for_bootstrap(),
        _ => Err(SetupError::Command(format!(
            "unsupported runner profile: {profile}"
        ))),
    }
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
members = ["crates/gitrun-core", "crates/gitrun-exe", "crates/gitrun-gsr"]

"#;

pub(crate) const RUNNER_BUILD_FILES: &[(&str, &str, u32)] = &[
    ("Cargo.toml", RUNNER_BUILD_CARGO_MANIFEST, 0o644),
    ("Cargo.lock", include_str!("runner-bootstrap.lock"), 0o644),
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
        "crates/gitrun-exe/Cargo.toml",
        include_str!("../../../crates/gitrun-exe/Cargo.toml"),
        0o644,
    ),
    (
        "crates/gitrun-exe/src/lib.rs",
        include_str!("../../../crates/gitrun-exe/src/lib.rs"),
        0o644,
    ),
    (
        "crates/gitrun-exe/src/ipc.rs",
        include_str!("../../../crates/gitrun-exe/src/ipc.rs"),
        0o644,
    ),
    (
        "crates/gitrun-exe/src/protocol.rs",
        include_str!("../../../crates/gitrun-exe/src/protocol.rs"),
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
    (
        "crates/gitrun-gsr/src/bin/gitrun-api.rs",
        include_str!("../../../crates/gitrun-gsr/src/bin/gitrun-api.rs"),
        0o644,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_dockerfile_uses_minimal_gsr_build_context() {
        let dockerfile =
            runner_dockerfile_from_source(include_str!("../../../docker/runner/Dockerfile"))
                .unwrap();
        assert!(dockerfile.contains("COPY crates/gitrun-core ./crates/gitrun-core"));
        assert!(dockerfile.contains("COPY crates/gitrun-exe ./crates/gitrun-exe"));
        assert!(dockerfile.contains("COPY crates/gitrun-gsr ./crates/gitrun-gsr"));
        assert!(!dockerfile.contains("COPY crates ./crates"));
        assert!(dockerfile.contains("cargo build --locked --release -p gitrun-gsr"));
    }

    #[test]
    fn built_in_runner_profiles_keep_gsr_mandatory() {
        let minimum = runner_dockerfile_for_profile("minimum").unwrap();
        let workbench = runner_dockerfile_for_profile("workbench").unwrap();
        for dockerfile in [&minimum, &workbench] {
            assert!(dockerfile.contains("gitrun-gsr-agent"));
            assert!(dockerfile.contains("GITRUN_GSR_COMMAND_POLICY_ENABLED=true"));
        }
    }

    #[test]
    fn bootstrap_lock_matches_the_minimal_workspace() {
        assert!(RUNNER_BUILD_CARGO_MANIFEST.contains("gitrun-core"));
        assert!(RUNNER_BUILD_CARGO_MANIFEST.contains("gitrun-exe"));
        assert!(RUNNER_BUILD_CARGO_MANIFEST.contains("gitrun-gsr"));
        assert!(!RUNNER_BUILD_CARGO_MANIFEST.contains("[patch.crates-io]"));
        assert!(include_str!("runner-bootstrap.lock").contains("name = \"gitrun-core\""));
        assert!(include_str!("runner-bootstrap.lock").contains("name = \"gitrun-gsr\""));
        assert!(
            !include_str!("runner-bootstrap.lock").contains("name = \"gitrun-dashboard-tauri\"")
        );
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
        assert!(paths.contains(&"crates/gitrun-exe/src/lib.rs"));
        assert!(paths.contains(&"crates/gitrun-exe/src/ipc.rs"));
        assert!(paths.contains(&"crates/gitrun-exe/src/protocol.rs"));
        assert!(paths.contains(&"crates/gitrun-gsr/src/bin/gitrun-gsr-agent.rs"));
        assert!(paths.contains(&"crates/gitrun-gsr/src/bin/gitrun-api.rs"));
        assert!(paths.contains(&"crates/gitrun-gsr/src/exec_supervisor.rs"));
        assert_eq!(
            RUNNER_ENTRYPOINT,
            include_str!("../../../docker/runner/entrypoint.sh")
        );
        assert!(RUNNER_ENTRYPOINT.contains("sudo -P -u runner"));
        assert!(RUNNER_ENTRYPOINT.contains("actions-runner/.gitrun-dock-only"));
        assert!(RUNNER_ENTRYPOINT.contains("gitrun-ci"));
        assert_eq!(
            SYSTEMD_SERVICE,
            include_str!("../../../systemd/gitrun.service")
        );
        assert!(SYSTEMD_SERVICE.contains("ExecStart=/usr/local/bin/gitrun scheduler"));
        assert!(!SYSTEMD_SERVICE.contains("docker compose"));
    }
}
