FROM node:22-bookworm

ARG RUNNER_VERSION=2.337.0

ENV DEBIAN_FRONTEND=noninteractive \
    RUNNER_ALLOW_RUNASROOT=1 \
    RUNNER_VERSION=${RUNNER_VERSION} \
    RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:$PATH

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates curl git jq tar gzip sudo python3 python3-pip \
       build-essential pkg-config libssl-dev clang cmake shellcheck \
       libx11-dev libxcursor-dev libxrandr-dev libxi-dev libxinerama-dev \
       libwayland-dev libxkbcommon-dev libegl1-mesa-dev libgl1-mesa-dev \
       libfontconfig1-dev unzip xz-utils \
       gnupg lsb-release gh \
    && install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/debian/gpg | gpg --dearmor -o /etc/apt/keyrings/docker.gpg \
    && chmod a+r /etc/apt/keyrings/docker.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/debian $(. /etc/os-release && echo $VERSION_CODENAME) stable" > /etc/apt/sources.list.d/docker.list \
    && curl -fsSL -o /tmp/packages-microsoft-prod.deb https://packages.microsoft.com/config/debian/12/packages-microsoft-prod.deb \
    && dpkg -i /tmp/packages-microsoft-prod.deb \
    && rm /tmp/packages-microsoft-prod.deb \
    && apt-get update \
    && apt-get install -y --no-install-recommends docker-ce-cli docker-buildx-plugin docker-compose-plugin powershell \
    && rm -rf /var/lib/apt/lists/* \
    && pwsh -NoLogo -NoProfile -Command '$PSVersionTable.PSVersion.ToString()'

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable \
    && rustc --version \
    && cargo --version

RUN useradd --create-home --shell /bin/bash runner \
    && mkdir -p /home/runner/actions-runner /var/lib/gitrun/shared /opt/rustup /opt/cargo \
    && chown -R runner:runner /home/runner /var/lib/gitrun/shared /opt/rustup /opt/cargo

WORKDIR /home/runner/actions-runner

RUN curl -fsSL -o runner.tar.gz \
      "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz" \
    && tar xzf runner.tar.gz \
    && rm runner.tar.gz \
    && ./bin/installdependencies.sh \
    && chown -R runner:runner /home/runner/actions-runner

COPY --chown=root:root docker/runner/entrypoint.sh /entrypoint.sh
RUN chmod 0755 /entrypoint.sh

USER root

ENTRYPOINT ["/entrypoint.sh"]
