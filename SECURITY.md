# Security

GitRun controls Docker containers and GitHub Actions self-hosted runners. Treat the host running GitRun as trusted infrastructure.

## Reporting

Please report security issues privately through GitHub rather than opening a public issue with exploit details.

## Deployment guidance

- Do not expose the Docker daemon to the public Internet.
- Keep GitHub credentials outside the repository.
- Use a dedicated GitHub token with only the permissions GitRun requires.
- Keep the GitRun environment file readable only by the service account or administrator.
- Prefer ephemeral runners when executing workloads that should not persist between jobs.
- Keep runner images and GitRun releases up to date.
- Review every repository before adding it to a GitRun installation.

GitRun is designed for administrators managing their own repositories. It is not intended to be an open public runner service.
