# syntax=docker/dockerfile:1.8
# check=error=true

FROM ubuntu:24.04

LABEL org.opencontainers.image.title="GitRun Linux x86_64 runner base"
LABEL org.opencontainers.image.version="0.1.0"
LABEL org.opencontainers.image.description="Minimal non-root base image for GitRun-managed Linux x86_64 runners"
LABEL org.opencontainers.image.vendor="Vider06"

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates curl git jq tini unzip \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /usr/sbin/nologin --uid 10001 gitrun \
    && chmod 0755 /home/gitrun

WORKDIR /home/gitrun
USER gitrun

STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/bin/sleep", "infinity"]
