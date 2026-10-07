        "--memory".into(),
        spec.memory.into(),
        "--restart".into(),
        "unless-stopped".into(),
    ]);

    if let Some(job_name) = spec.workflow_job_name {
        args.extend(["--label".into(), format!("gitrun.workflow_job={job_name}")]);
    }
    if let Some(run_id) = spec.workflow_run_id {
        args.extend(["--label".into(), format!("gitrun.workflow_run={run_id}")]);
    }

    if spec.is_windows {
        // Windows containers: no --read-only/--tmpfs/--pids-limit/Unix
        // group-add support. The runner's home directory is simply the
        // container's own writable filesystem layer — Windows containers
        // don't get the same read-only-root treatment Linux runners do here
        // yet; hardening Windows containers is tracked as GSR follow-up
        // work, not solved by this function.
        args.push("-e".into());
        args.push("GITRUN_SHARED_CACHE_DIR=C:\\gitrun\\shared".into());
    } else {
        args.extend([
            "--pids-limit".into(),
            spec.pids_limit.into(),
            // Keep the runner root filesystem writable. A general-purpose
            // GitHub Actions runner is expected to install job-local tools
            // and packages, and some package managers need to write outside
            // /home/runner and /tmp. The runner still cannot turn those
            // writes into privilege escalation because no-new-privileges
            // and the capability boundary remain enforced.
        ]);
        match spec.home_backend {
            RunnerHomeBackend::Tmpfs => {
                args.extend([
                    "--tmpfs".into(),
                    format!(
                        "/home/runner/actions-runner:rw,nosuid,nodev,size={}",
                        spec.runner_home_size
                    ),
                ]);
            }
            RunnerHomeBackend::Volume => {
                // Docker creates this named volume automatically on `run`
                // if it doesn't exist yet — no separate `volume create`
                // step needed. One volume per runner container, named
                // after it, so cleanup in `remove_container_on` can find
                // it deterministically. `size=` isn't meaningful for a
                // disk-backed local-driver volume the way it is for
                // tmpfs, so `runner_home_size` is intentionally not
                // applied here — disk space is bounded by the host
                // filesystem, not this setting.
                args.extend([
                    "--mount".into(),
                    format!(
                        "type=volume,source={},target=/home/runner/actions-runner",
                        home_volume_name(spec.name)
                    ),
                ]);
            }
        }
        args.extend([
            "--tmpfs".into(),
            "/tmp:rw,nosuid,nodev,exec,size=256m".into(),
        ]);
        if spec.docker_socket_enabled && !spec.is_windows {
            args.extend([
                "--volume".into(),
                "/var/run/docker.sock:/var/run/docker.sock".into(),
                "--group-add".into(),
                spec.docker_socket_gid.into(),
            ]);
        }
        args.extend([
            "--mount".into(),