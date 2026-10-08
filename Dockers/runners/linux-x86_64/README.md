## Linux x86_64 GitRun setup runner

Version: 0.1.0

This Dockerfile is a Premade copy of the Dockerfile currently used by the GitRun Linux x86_64 setup path.

The GitRun setup code consumes the repository Dockerfile as its runner image source and provides the GitRun application workspace as the Docker build context. For that reason this Premade copy is a synchronized source artifact, not a standalone Docker build context.

The definition includes the GitHub Actions runner, Docker tooling, Rust tooling, PowerShell, and GitSecureRun enforcement used by GitRun-managed runner containers.

The synchronization test compares this file with main:docker/runner/Dockerfile.
