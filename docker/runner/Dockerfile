FROM node:22-bookworm

ARG RUNNER_VERSION=2.337.0

ENV DEBIAN_FRONTEND=noninteractive \
    RUNNER_ALLOW_RUNASROOT=1 \
    RUNNER_VERSION=${RUNNER_VERSION}

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates curl git jq tar gzip sudo python3 python3-pip \
       libicu72 libssl3 zlib1g libkrb5-3 libunwind8 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --shell /bin/bash runner \
    && mkdir -p /home/runner/actions-runner \
    && chown -R runner:runner /home/runner

WORKDIR /home/runner/actions-runner

RUN curl -fsSL -o runner.tar.gz \
      "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz" \
    && tar xzf runner.tar.gz \
    && rm runner.tar.gz \
    && ./bin/installdependencies.sh \
    && chown -R runner:runner /home/runner/actions-runner

COPY --chown=runner:runner docker/runner/entrypoint.sh /entrypoint.sh
RUN chmod 0755 /entrypoint.sh

USER runner

ENTRYPOINT ["/entrypoint.sh"]
