# Core

Core contains shared GitRun Premade conventions and resources that should be consumed by GitRun and other Premade components instead of being duplicated or hardcoded.

Areas:
- defaults/ — versioned policy and configuration defaults.
- manifests/ — machine-readable Premade metadata and schemas.
- utils/ — reusable helpers for resolving and validating Premade resources.

Keep this directory platform-neutral unless a resource is explicitly platform-scoped.
