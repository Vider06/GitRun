# Rust migration contract

The Rust workspace is additive. The existing Python autoscaler remains authoritative until each responsibility has a tested replacement.

Migration order:
1. configuration and state;
2. runner lifecycle;
3. updater and recovery (release artifacts, checksum verification, dependency compatibility, version-pinned runner images, rollback);
4. CLI delegation;
5. manager API;
6. dashboard integration.

The repository can still be deployed through the existing Python/Docker path while the Rust core is introduced incrementally.
