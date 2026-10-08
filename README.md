# GitRun Premade

GitRun Premade is the maintained collection of reusable, hardened, versioned resources used by GitRun.

This branch intentionally does not contain the GitRun application itself. It is an artifact/content branch that GitRun can consume independently from the application release cycle.

## Layout

- Core/ — shared conventions, manifests, defaults, and reusable utilities.
- Dockers/ — GitRun-maintained Docker images and build contexts.
- VM-Images/ — hardened virtual-machine images and metadata.
- Workflows/ — reusable GitHub Actions building blocks, split into GitRun-Dependent and GitRun-Independent.
- Testing/ — tests for every Premade resource family and cross-family integration tests.
- Other/ — additional maintained resources that do not fit the primary families.

## Rules

1. No GitRun application source belongs in this branch.
2. Every resource family and resource should document purpose, inputs, outputs, security assumptions, and ownership.
3. Shared values belong in Core instead of being duplicated or hardcoded in consumers.
4. Docker builds use BuildKit/buildx. Legacy docker build is not an accepted Premade build path.
5. Artifacts should have explicit metadata and immutable references where practical.
6. Testing is part of the resource lifecycle and must run through the dedicated Premade CI workflows.

## Status

The branch starts as the Premade scaffold. Artifact families will be added and validated independently.
