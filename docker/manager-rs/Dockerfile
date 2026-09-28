# syntax=docker/dockerfile:1

FROM rust:1.98.1-slim-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN --mount=type=cache,id=gitrun-manager-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=gitrun-manager-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=gitrun-manager-target,target=/build/target \
    cargo build --locked --release -p gitrun-scheduler --bin gitrun-autoscaler \
    && cp /build/target/release/gitrun-autoscaler /build/gitrun-autoscaler

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends docker.io ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /build/gitrun-autoscaler /usr/local/bin/gitrun-autoscaler
CMD ["/usr/local/bin/gitrun-autoscaler"]
