FROM rust:1.98.1-slim-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p gitrun-scheduler --bin gitrun-autoscaler

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends docker.io ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /build/target/release/gitrun-autoscaler /usr/local/bin/gitrun-autoscaler
CMD ["/usr/local/bin/gitrun-autoscaler"]
