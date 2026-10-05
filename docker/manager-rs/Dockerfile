# syntax=docker/dockerfile:1

ARG DOCKER_CLI_VERSION=5:29.8.1-1~debian.12~bookworm
# BuildKit cache mounts are persisted explicitly by the CI cache-dance step.
# Cargo fingerprints invalidate stale artifacts as source files change.
FROM rust:1.99.0-slim-bookworm@sha256:452176c0cefca88c0b3184ce85a4eb03e3d4fa05d2afb5366abcba853221019e AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN grep -Fq 'pub mod app_auth;' crates/gitrun-core/src/lib.rs \
    && test -f crates/gitrun-core/src/app_auth.rs


RUN --mount=type=cache,id=gitrun-manager-cargo-registry-v11,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=gitrun-manager-cargo-git-v11,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=gitrun-manager-target-v11,target=/build/target,sharing=locked \
    cargo build --locked --release -p gitrun-scheduler --bin gitrun-autoscaler \
    && cp /build/target/release/gitrun-autoscaler /build/gitrun-autoscaler \
    && strip --strip-unneeded /build/gitrun-autoscaler

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251

ARG DOCKER_CLI_VERSION

# hadolint ignore=DL3008 -- base image is pinned by digest; Debian packages follow its security stream.
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
