# syntax=docker/dockerfile:1

ARG RUNNER_VERSION=2.337.0
ARG RUNNER_SHA256=70920811a4f8ad4328818682bca5c6469c1c942fab52448868071d0063816613
ARG DOCKER_CLI_VERSION=5:29.8.1-1~debian.12~bookworm
ARG DOCKER_BUILDX_VERSION=0.37.1-1~debian.12~bookworm
ARG DOCKER_COMPOSE_VERSION=5.5.1-1~debian.12~bookworm
ARG DOCKER_GPG_FINGERPRINT=9DC858229FC7DD38854AE2D88D81803C0EBFCD88
ARG POWERSHELL_VERSION=7.5.9
ARG POWERSHELL_SHA256=492FF26BB958336BF61E597CE19E07648B4003BD2A08659E02F0E3E0446EBFE0

# Build the GSR shell agent with a pinned Rust toolchain. BuildKit cache mounts
# are persisted explicitly by the CI cache-dance step.
FROM rust:1.98.1-bookworm@sha256:9a73a5088750b4c95158ab26629c854c3d6fc4b173cb7bc8079ad252d8ed7bfa AS gsr-builder

WORKDIR /usr/src/gitrun

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN --mount=type=cache,id=gitrun-runner-cargo-registry-v11,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=gitrun-runner-cargo-git-v11,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=gitrun-runner-target-v11,target=/usr/src/gitrun/target,sharing=locked \
    cargo build --locked --release -p gitrun-gsr --bin gitrun-gsr-agent \
    && cp /usr/src/gitrun/target/release/gitrun-gsr-agent /usr/src/gitrun/gitrun-gsr-agent \
    && strip --strip-unneeded /usr/src/gitrun/gitrun-gsr-agent \
    && test -x /usr/src/gitrun/gitrun-gsr-agent

FROM node:26.10.0-bookworm@sha256:2aaae6d91f99fee84cfc92da9b52c22a185752d247746052bbc3f961e44478c6

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

ARG RUNNER_VERSION
ARG RUNNER_SHA256
ARG DOCKER_CLI_VERSION
ARG DOCKER_BUILDX_VERSION
ARG DOCKER_COMPOSE_VERSION
ARG DOCKER_GPG_FINGERPRINT
ARG POWERSHELL_VERSION
ARG POWERSHELL_SHA256

ENV DEBIAN_FRONTEND=noninteractive \
    RUNNER_VERSION=${RUNNER_VERSION} \
    RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:/usr/local/bin:$PATH

# hadolint ignore=DL3008 -- Debian packages intentionally follow the pinned bookworm base image security stream.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates curl git jq tar gzip sudo python3 python3-pip \
       build-essential pkg-config libssl-dev clang cmake shellcheck \
       libx11-dev libxcursor-dev libxrandr-dev libxi-dev libxinerama-dev \
       libwayland-dev libxkbcommon-dev libegl1-mesa-dev libgl1-mesa-dev \
       libfontconfig1-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
       librsvg2-dev libxdo-dev patchelf xdg-utils unzip xz-utils \
       gnupg gh \
    && install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL --proto '=https' --tlsv1.2 \
       https://download.docker.com/linux/debian/gpg \
       -o /etc/apt/keyrings/docker.asc \
    && test "$(gpg --show-keys --with-colons /etc/apt/keyrings/docker.asc | awk -F: '$1 == "fpr" {print $10; exit}')" = "$DOCKER_GPG_FINGERPRINT" \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian bookworm stable" \
       > /etc/apt/sources.list.d/docker.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
       "docker-ce-cli=${DOCKER_CLI_VERSION}" \
       "docker-buildx-plugin=${DOCKER_BUILDX_VERSION}" \
       "docker-compose-plugin=${DOCKER_COMPOSE_VERSION}" \
    && rm -rf /var/lib/apt/lists/*

# Install PowerShell directly from its immutable GitHub release artifact.
RUN curl -fsSL --proto '=https' --tlsv1.2 \
      -o /tmp/powershell.tar.gz \
      "https://github.com/PowerShell/PowerShell/releases/download/v${POWERSHELL_VERSION}/powershell-${POWERSHELL_VERSION}-linux-x64.tar.gz" \
    && echo "${POWERSHELL_SHA256}  /tmp/powershell.tar.gz" | sha256sum -c - \
    && mkdir -p "/opt/microsoft/powershell/${POWERSHELL_VERSION}" \
    && tar -xzf /tmp/powershell.tar.gz -C "/opt/microsoft/powershell/${POWERSHELL_VERSION}" \
    && ln -s "/opt/microsoft/powershell/${POWERSHELL_VERSION}/pwsh" /usr/local/bin/pwsh \
    && rm /tmp/powershell.tar.gz \
    && pwsh -NoLogo -NoProfile -Command "Write-Output \$PSVersionTable.PSVersion.ToString()"

# Reuse the verified Rust toolchain from the pinned official Rust image
# instead of executing a floating remote rustup installer at build time.
COPY --from=gsr-builder /usr/local/rustup /opt/rustup
COPY --from=gsr-builder /usr/local/cargo/bin /opt/cargo/bin
RUN rustc --version \
    && cargo --version

RUN useradd --create-home --shell /bin/bash runner \
    && mkdir -p /home/runner/actions-runner /var/lib/gitrun/shared \
    && chown -R runner:runner /home/runner /var/lib/gitrun/shared /opt/rustup /opt/cargo

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
RUN chmod 0755 /usr/local/bin/gitrun-gsr-agent

COPY --chown=root:root docker/runner/entrypoint.sh /entrypoint.sh
RUN chmod 0755 /entrypoint.sh \
    && bash -n /entrypoint.sh

# The GSR agent requires a protected policy file. This build-time default is
# replaced by the root-owned runtime tmpfs policy before the runner starts.
RUN install -d -o root -g root -m 0755 /run/gitrun \
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

# hadolint ignore=DL3002 -- entrypoint must remain root to prepare the protected policy, Docker socket group, and runner ownership.
USER 0

# Install the GSR agent as both shell entry points. The original binaries are
# retained under explicit names so allowed commands preserve normal shell
# semantics while denied commands never reach them.
RUN mv /bin/sh /bin/sh.gitrun-real \
    && mv /bin/bash /bin/bash.gitrun-real \
    && cp /usr/local/bin/gitrun-gsr-agent /bin/sh \
    && cp /usr/local/bin/gitrun-gsr-agent /bin/bash \
    && chmod 0755 /bin/sh /bin/bash /bin/sh.gitrun-real /bin/bash.gitrun-real \
    && test -x /bin/sh.gitrun-real \
    && test -x /bin/bash.gitrun-real \
    && test -x /bin/sh \
    && test -x /bin/bash

# Smoke-test both delegated shell entry points through the protected
# build-time default policy file.
RUN /bin/sh -c 'test "$(printf sh)" = sh' \
    && /bin/bash -c 'test "$(printf bash)" = bash' \
    && /usr/bin/sh -c 'test "$(printf sh)" = sh' \
    && /usr/bin/bash -c 'test "$(printf bash)" = bash'

LABEL org.opencontainers.image.title="GitRun Actions runner" \
      org.opencontainers.image.description="GitHub Actions runner with GitSecureRun enforcement" \
      org.opencontainers.image.source="https://github.com/Vider06/GitRun"

ENTRYPOINT ["/entrypoint.sh"]
