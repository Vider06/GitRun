# Updater release policy

GitRun 1.9.1 and later use signed release classifications and one shared operator policy. The new updater does not interpret release metadata from versions before 1.9.1.

## Release classifications

Every new release is published initially as a Pre-Release.

| Classification | Manifest value | GitHub prerelease |
|---|---|---:|
| Pre | `pre` | `true` |
| Full | `full` | `false` |
| Security | `security` | `false` |

A Full release should consolidate the fixes since the preceding Full release, including security fixes already published separately.

The signed `release-manifest.json` contains `release_type`. The GitHub release description contains one matching marker, for example:

```text
<!-- GITRUN_RELEASE_TYPE: pre -->
```

The manifest signature covers the classification. The updater cross-checks the signed value, description marker and GitHub prerelease flag.

## Operator policies

The updater crate owns persistence for the single shared preference. The CLI calls its API directly; the TUI and desktop GUI reach the same state through `gitrun-management`, which delegates to the updater crate.

| Policy | Stored value | Eligible types |
|---|---|---|
| Security updates only | `security_only` | Security |
| Security + Full | `security_full` | Security, Full |
| Security + Full + Pre | `security_full_pre` | Security, Full, Pre |

The default is `security_full`; Pre releases require explicit opt-in. Security releases remain eligible under every policy.

The GUI exposes the selector in Settings. The Management TUI exposes the same selector under Settings. Both use the same backend and persisted preference.

## Selection safeguards

The `gitrun-updater` engine paginates the GitHub Releases API, excludes drafts, checks the release marker against GitHub metadata, and compares versions using SemVer. It selects only versions newer than the installed version. It never downgrades.

The signed manifest must match the selected GitHub tag and release classification. The engine selects the newest eligible release with an artifact for the host target. An unsupported target may fall back to an older eligible release that supports it; signature or metadata errors fail closed.

## CLI diagnostics

```text
gitrun update --info
gitrun update --info --check
gitrun update --check
gitrun update
```

- `gitrun update --info` reports local version, package, architecture, ABI, selected policy, update lock, required service status, recent operations and recent errors. It does not contact GitHub.
- `gitrun update --info --check` adds a read-only online check and reports the selected version, release type, compatible artifact and selection reason. It does not acquire the update lock or install anything.
- `gitrun update --check` is the concise policy-aware check.
- `--no-cat` suppresses the normal mascot output for scripts and embedded callers.

Update history is bounded and stored in the protected GitRun state directory by `gitrun-updater`. The CLI calls the updater directly. The GUI and TUI use `gitrun-management` as an adapter; on supported systemd installations the updater state API uses the fixed Control Manager IPC actions, while other protected Linux installations use fixed-purpose CLI helpers.

## Reclassifying a published release

Run the manual GitHub Actions workflow **GitRun Release Type Change** with the existing tag and desired type. It downloads the existing manifest, changes only its classification, re-signs and verifies it, updates the description marker, and synchronizes GitHub's prerelease flag. If metadata synchronization fails, it restores the previous manifest asset. Release binaries are not rebuilt or replaced.

The operation is idempotent. It can be run again with the same tag and classification.

## Version boundary

This format begins with GitRun 1.9.1. The new updater requires the typed signed manifest and matching GitHub metadata; it intentionally has no legacy interpretation path for older releases.
