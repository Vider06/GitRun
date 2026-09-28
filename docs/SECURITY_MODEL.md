# Security model

This document states plainly what GitRun's design trusts, and what an
operator is implicitly granting when they run it. It is not a promise that
every risk here has a mitigation yet — several are tracked as future work
(see `RUST_MIGRATION.md` and the GSR/GitVault design notes) — the point of
this document is that nothing here should be a surprise.

## Docker socket access is host-level privilege

Every GitRun-managed runner container is started with:

```
--volume /var/run/docker.sock:/var/run/docker.sock
--group-add <docker socket GID>
```

**This is equivalent to granting the runner container root-equivalent access
to the Docker host it runs on.** Anyone who can execute arbitrary code inside
a container that holds the Docker socket can, at minimum:

- start new containers with arbitrary mounts, including mounting the host's
  root filesystem (`-v /:/host`) and reading/writing anything on it;
- start privileged containers, which can load kernel modules, modify host
  network configuration, and generally escape the container boundary
  entirely;
- read the configuration and environment variables of every other
  container on the host, including other GitRun runners' registration
  tokens while they're briefly live.

This is not a bug or an oversight — GitRun's runners need Docker access
because CI workflows on GitHub Actions frequently run Docker themselves
(building images, running service containers via `services:`, using
`docker/build-push-action`, etc.), and self-hosted runners are expected to
support that. **But it means every workflow that runs on a GitRun runner
should be treated as having host-level access**, the same way it would on
any self-hosted runner setup with Docker-in-Docker via socket mounting
(as opposed to a rootless/sandboxed Docker-in-Docker approach, which GitRun
does not currently implement).

Practical implications for an operator:

- Only point GitRun at repositories where you trust everyone who can trigger
  a workflow run (i.e. everyone who can open a PR, if your workflows run on
  PRs from forks — **this is the most common way self-hosted runners get
  abused**: a malicious PR from an outside contributor runs arbitrary code
  with runner-level access). If you accept external contributions, restrict
  which workflows run on self-hosted runners, or don't run PR-triggered
  workflows from forks on GitRun at all.
- Runners should not share a host with anything sensitive unless that
  sensitive material is itself protected by other means (see GitVault design
  notes on secrets-at-rest, once implemented).
- The registration token passed to each runner container (`RUNNER_TOKEN`
  environment variable) is a short-lived GitHub token, not the long-lived
  Personal Access Token GitRun itself holds — but it is still visible via
  `docker inspect` to anyone with Docker socket access on the host, which
  circles back to the point above.

## Filesystem isolation

Runner containers run with `--read-only` at the container level, with two
writable exceptions:

- a tmpfs backing `/home/runner/actions-runner` (runner registration state,
  diagnostics, and job checkouts — see `gitrun-scheduler/src/docker.rs` for
  why this needs to be writable, not just `/tmp`)
- a small tmpfs at `/tmp` for general scratch use
- a mounted volume for the shared package-manager cache (Cargo/pip/npm)

This limits (but does not eliminate, given Docker socket access above) what
a compromised job can persist or tamper with on the container's own
filesystem. It does not protect the host filesystem, which remains fully
reachable via the Docker socket as described above.

## What GitRun does *not* currently do

Documented here so it's explicit rather than assumed:

- No network isolation between runner containers and the rest of the host's
  network by default (beyond whatever the Docker bridge network provides).
- No seccomp/AppArmor profile beyond Docker's defaults.
- No secrets-at-rest encryption yet (planned: GitVault).
- No process-level crash/tamper detection independent of the main GitRun
  process (planned: GSR — a separate watchdog process, by design, so it
  survives a hard crash of the main scheduler rather than dying with it).

If your threat model requires stronger isolation than this today, consider
running GitRun's Docker host itself inside a dedicated VM rather than
alongside other workloads, until the above items are addressed.
