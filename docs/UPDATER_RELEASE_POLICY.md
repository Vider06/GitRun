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

The `gitrun-updater` engine paginates the GitHub Releases API, excludes drafts, checks the release marker against GitHub metadata, and compares versions using SemVer. Ordinary update checks and `gitrun update` select only versions newer than the installed version.

The signed manifest must match the selected GitHub tag and release classification. The engine selects the newest eligible release with an artifact for the host target. An unsupported target may fall back to an older eligible release only while it remains newer than the installed version; signature or metadata errors fail closed.

## Explicit release-line transition

When an operator changes the saved release policy in the Management TUI or desktop GUI, GitRun asks:

> Would you like to instantly roll to the latest line version?

Accepting runs one immediate transition against the newest classified release allowed by the newly saved policy. If that release is newer than the installed version, this is an ordinary forward update. If it is older (for example, leaving a newer Pre release for the latest Security/Full release), the transition-specific planner permits that one explicitly authorized target. Declining only saves the policy; the current installation stays in place, and subsequent ordinary updates continue to require a strictly newer version.

This is not a general downgrade switch. The transition path:

- cannot be combined with an arbitrary manifest URL, read-only check, runner-only update or `--force`;
- enumerates the signed-classification release line over the GitHub Releases API and selects the newest different version;
- treats an equal latest version as a no-op and never falls back across the installed-version boundary;
- requires the release marker and GitHub prerelease flag to agree with the signed manifest classification and tag;
- continues to require the built-in Ed25519 trust anchor, artifact target compatibility, HTTPS download, SHA-256 verification and the normal install transaction/backup/health checks;
- does not accept releases older than the 1.9.1 typed-classification boundary.

An unsupported host artifact may fall back to an older release only in the same upgrade/downgrade direction established by the newest eligible release. A signature, classification, tag or manifest error fails closed.

## Immediate operations and scheduled work

An operation explicitly launched by the operator from the TUI or GUI runs immediately and remains visible until it completes. The TUI attaches the CLI process to the terminal; the GUI streams process output and waits for its exit status. The Control Manager's scheduled maintenance path remains for tasks that are genuinely intended to run later or in the background, not as the default execution path for an operator's Update action.

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
