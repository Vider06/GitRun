# syntax=docker/dockerfile:1

ARG DOCKER_CLI_VERSION=5:29.8.1-1~debian.12~bookworm
# Keep the persistent Cargo target cache isolated per source revision. BuildKit
# cache mounts outlive Dockerfile COPY layers, so sharing target artifacts
# across commits can make Cargo reuse a stale fingerprint after branch changes.
ARG GITRUN_SOURCE_REV=local

FROM rust:1.98.1-slim-bookworm@sha256:ebd900bae66fd508b466cef82d64a83a5fb34682e4c8b2797a42908bddc95a57 AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN --mount=type=cache,id=gitrun-manager-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=gitrun-manager-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=gitrun-manager-target-${GITRUN_SOURCE_REV},target=/build/target,sharing=locked \
    cargo build --locked --release -p gitrun-scheduler --bin gitrun-autoscaler \
    && cp /build/target/release/gitrun-autoscaler /build/gitrun-autoscaler \
    && strip --strip-unneeded /build/gitrun-autoscaler

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251

ARG DOCKER_CLI_VERSION

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl gnupg passwd util-linux \
    && install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc \
    && chmod a+r /etc/apt/keyrings/docker.asc \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian bookworm stable" > /etc/apt/sources.list.d/docker.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends "docker-ce-cli=${DOCKER_CLI_VERSION}" \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 gitrun-manager \
    && useradd --system --uid 10001 --gid 10001 --home-dir /nonexistent --shell /usr/sbin/nologin gitrun-manager

COPY --from=build /build/gitrun-autoscaler /usr/local/bin/gitrun-autoscaler
COPY docker/manager-rs/entrypoint.sh /usr/local/bin/gitrun-manager-entrypoint

RUN chmod 0755 /usr/local/bin/gitrun-manager-entrypoint \
    && sh -n /usr/local/bin/gitrun-manager-entrypoint

LABEL org.opencontainers.image.title="GitRun Rust manager" \
      org.opencontainers.image.description="GitRun autoscaler and runner manager" \
      org.opencontainers.image.source="https://github.com/Vider06/GitRun"

ENTRYPOINT ["/usr/local/bin/gitrun-manager-entrypoint"]
