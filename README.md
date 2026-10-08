# GitRun Premade

GitRun Premade is the maintained artifact/content branch for reusable GitRun resources.

This branch is intentionally separate from the GitRun application source tree. It contains premade Docker images, VM image definitions, reusable workflows, shared metadata, and tests consumed by GitRun.

## Layout

Core/, Dockers/, VM-Images/, Workflows/, Testing/, and Other/ contain maintained Premade content.

## Rules

Premade must not contain the GitRun application workspace. Every artifact must document its purpose, inputs, outputs, security assumptions, versioning, and ownership. Docker builds use BuildKit/buildx. Artifact references must be explicit and reproducible.

The branch is not a source branch for the GitRun application and must not be merged into application branches.
