# syntax=docker/dockerfile:1

ARG RUNNER_VERSION=2.337.0
ARG RUNNER_SHA256=70920811a4f8ad4328818682bca5c6469c1c942fab52448868071d0063816613

# GSR is built from the same pinned Rust toolchain as the Workbench profile.
FROM rust:1.99.0-bookworm@sha256:59037199c44290f2befcdd58dcc540164763fc296950255aaefeef096a1866b0 AS gsr-builder

WORKDIR /usr/src/gitrun

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN --mount=type=cache,id=gitrun-runner-cargo-registry-v11,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=gitrun-runner-cargo-git-v11,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=gitrun-runner-target-v11,target=/usr/src/gitrun/target,sharing=locked \
    cargo build --locked --release -p gitrun-gsr --bin gitrun-gsr-agent --bin gitrun-api \
    && cp /usr/src/gitrun/target/release/gitrun-gsr-agent /usr/src/gitrun/gitrun-gsr-agent \
    && cp /usr/src/gitrun/target/release/gitrun-api /usr/src/gitrun/gitrun-api \
    && strip --strip-unneeded /usr/src/gitrun/gitrun-gsr-agent /usr/src/gitrun/gitrun-api \
    && test -x /usr/src/gitrun/gitrun-gsr-agent \
    && test -x /usr/src/gitrun/gitrun-api

# The minimum profile omits language build toolchains, desktop/GUI development
# libraries, PowerShell, and the Docker CLI. Workflows can install their own
# project-specific tools as needed. The runner and GSR remain part of the image.
FROM node:26.10.0-bookworm@sha256:2aaae6d91f99fee84cfc92da9b52c22a185752d247746052bbc3f961e44478c6

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

ARG RUNNER_VERSION
ARG RUNNER_SHA256

ENV DEBIAN_FRONTEND=noninteractive \
    RUNNER_VERSION=${RUNNER_VERSION}

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates curl git jq tar gzip sudo python3 unzip xz-utils \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --shell /bin/bash runner \
    && mkdir -p /home/runner/actions-runner /var/lib/gitrun/shared \
    && chown -R runner:runner /home/runner /var/lib/gitrun/shared

WORKDIR /home/runner/actions-runner

RUN curl -fsSL --proto '=https' --tlsv1.2 \
      -o runner.tar.gz \
      "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz" \
    && echo "${RUNNER_SHA256}  runner.tar.gz" | sha256sum -c - \
    && tar xzf runner.tar.gz \
    && rm runner.tar.gz \
    && ./bin/installdependencies.sh \
    && chown -R runner:runner /home/runner/actions-runner

COPY --from=gsr-builder /usr/src/gitrun/gitrun-gsr-agent /usr/local/bin/gitrun-gsr-agent
COPY --from=gsr-builder /usr/src/gitrun/gitrun-api /usr/local/bin/gitrun-api
RUN chmod 0755 /usr/local/bin/gitrun-gsr-agent /usr/local/bin/gitrun-api \
    && for api in GitVaultRun GitDockRun GitSaveRun GitRegisterRun GitInstallRun GitReadRun GitWriteRun GitVerifyRun GitStatusRun; do \
         ln -s /usr/local/bin/gitrun-api "/usr/local/bin/$api"; \
       done

COPY --chown=root:root docker/runner/entrypoint.sh /entrypoint.sh
RUN chmod 0755 /entrypoint.sh \
    && bash -n /entrypoint.sh \
    && install -d -o root -g root -m 0755 /run/gitrun \
    && printf '%s\n' \
       'GITRUN_GSR_COMMAND_POLICY_ENABLED=true' \
       'GITRUN_GSR_COMMAND_BASELINE_BLACKLIST_ENABLED=true' \
       'GITRUN_GSR_COMMAND_BLACKLIST_ENABLED=false' \
       'GITRUN_GSR_COMMAND_BLACKLIST=' \
       'GITRUN_GSR_COMMAND_WHITELIST_ENABLED=false' \
       'GITRUN_GSR_COMMAND_WHITELIST=' \
       'GITRUN_GSR_VIOLATION_ACTION=kill' \
       > /run/gitrun/gsr-policy.env \
    && chown root:root /run/gitrun/gsr-policy.env \
    && chmod 0444 /run/gitrun/gsr-policy.env

# GSR is mandatory: both standard shell entry points are replaced by the
# agent, while the original shells are retained only as explicit delegates.
USER 0
RUN mv /bin/sh /bin/sh.gitrun-real \
    && mv /bin/bash /bin/bash.gitrun-real \
    && cp /usr/local/bin/gitrun-gsr-agent /bin/sh \
    && cp /usr/local/bin/gitrun-gsr-agent /bin/bash \
    && chmod 0755 /bin/sh /bin/bash /bin/sh.gitrun-real /bin/bash.gitrun-real \
    && test -x /bin/sh.gitrun-real \
    && test -x /bin/bash.gitrun-real \
    && test -x /bin/sh \
    && test -x /bin/bash \
    && /bin/sh -c 'test "$(printf sh)" = sh' \
    && /bin/bash -c 'test "$(printf bash)" = bash' \
    && /usr/bin/sh -c 'test "$(printf sh)" = sh' \
    && /usr/bin/bash -c 'test "$(printf bash)" = bash'

LABEL org.opencontainers.image.title="GitRun Minimum Actions runner" \
      org.opencontainers.image.description="Minimal GitHub Actions runner with mandatory GitSecureRun enforcement" \
      org.opencontainers.image.source="https://github.com/Vider06/GitRun"

ENTRYPOINT ["/entrypoint.sh"]
