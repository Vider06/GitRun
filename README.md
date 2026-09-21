# GitRun

GitRun is the control plane for Docker-based GitHub Actions self-hosted runners.

## Target architecture

- 3 warm runners always available.
- Scale dynamically up to 20 runners.
- Extra runners stay alive for 120 seconds after becoming idle.
- Runner containers are disposable and isolated.
- Multiple repositories can be managed from one GitRun installation.

## CLI

```text
gitrun overview
gitrun status
gitrun runners
gitrun health
gitrun doctor
gitrun logs
gitrun last-crash
gitrun usage
gitrun connect owner/repo
gitrun start
gitrun stop
gitrun restart
```

## Billing note

GitHub's included Actions minutes apply to GitHub-hosted runners on private repositories. Self-hosted runners are free from GitHub Actions minute charges, so moving a workflow to another private repository does not reset or bypass the included-minute allowance. citehttps://docs.github.com/en/billing/concepts/product-billing/github-actions

## Security

Never commit GitHub tokens, registration tokens, OAuth secrets, or other credentials. GitRun's runtime configuration is intended to live outside the repository.

## Status

GitRun is in initial implementation. The first milestone is the host-side control CLI; runner provisioning, autoscaling, lifecycle isolation, and GitHub queue integration follow.
