# GitRun security model

GitRun manages Docker-based GitHub Actions self-hosted runners. The most important security boundary is the runner container.

## Trust model

A GitHub Actions workflow can execute arbitrary code inside its assigned runner. GitRun is therefore intended for repositories whose workflow code you trust.

Managed runners intentionally mount the host Docker Engine socket at `/var/run/docker.sock` so GitRun-compatible CI jobs can build and run Docker workloads. Access to that socket is effectively administrative access to the Docker host. A workflow running inside a managed runner may therefore be able to control the host through Docker.

For that reason, do not attach untrusted public pull requests, unknown repositories, or arbitrary third-party workloads to a GitRun runner pool unless you have independently designed a stronger isolation boundary.

## Container hardening

Runner containers use:

- a read-only image root filesystem;
- a private writable `/tmp`;
- CPU, memory, and PID limits;
- a dedicated writable runner work directory;
- a shared Docker volume for selected build caches.

The writable runner work directory is required by the GitHub Actions runner itself and is kept separate from the image root.

## Credentials

The manager uses a GitHub credential to create and remove runner registrations and inspect queued Actions jobs. Registration tokens are generated on demand.

Never commit runtime credentials or environment files. Rotate credentials immediately if they are exposed.

## Host exposure

Keep the Docker Engine socket private. Do not expose the Docker API directly to the public Internet, and do not give the GitRun host unnecessary administrative access.

## Security reports

Please use GitHub's private security advisory mechanism for vulnerabilities. Do not publish active exploit details in a public issue.
