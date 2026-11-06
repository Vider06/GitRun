use super::{
    compare_versions, fetch_manifest, release_manifest_download_url, ReleaseManifest, UpdateError,
};
use reqwest::blocking::Client;
use serde::Deserialize;
use std::cmp::Ordering;
use std::time::Duration;

const RELEASE_TYPE_MARKER: &str = "<!-- GITRUN_RELEASE_TYPE:";
const RELEASES_PER_PAGE: usize = 100;
const MAX_RELEASE_PAGES: usize = 100;
// Signed release classification begins with 1.9.1; older tags have no typed trust metadata.
const MIN_CLASSIFIED_RELEASE_VERSION: &str = "1.9.1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseType {
    Pre,
    Full,
    Security,
}

impl ReleaseType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pre => "pre",
            Self::Full => "full",
            Self::Security => "security",
        }
    }

    pub fn from_release_body(body: &str) -> Result<Self, UpdateError> {
        let mut found = None;
        for line in body.lines().map(str::trim) {
            if let Some(value) = line.strip_prefix(RELEASE_TYPE_MARKER) {
                let value = value
                    .strip_suffix("-->")
                    .ok_or_else(|| {
                        UpdateError::InvalidManifest("malformed release type marker".into())
                    })?
                    .trim();
                if found.is_some() {
                    return Err(UpdateError::InvalidManifest(
                        "release description contains duplicate release type markers".into(),
                    ));
                }
                found = Some(match value {
                    "pre" => Self::Pre,
                    "full" => Self::Full,
                    "security" => Self::Security,
                    _ => {
                        return Err(UpdateError::InvalidManifest(format!(
                            "unknown release type marker: {value}"
                        )))
                    }
                });
            }
        }
        found.ok_or_else(|| {
            UpdateError::InvalidManifest(
                "release description is missing its GitRun release type marker".into(),
            )
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePolicy {
    SecurityOnly,
    SecurityFull,
    SecurityFullPre,
}

impl UpdatePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SecurityOnly => "security_only",
            Self::SecurityFull => "security_full",
            Self::SecurityFullPre => "security_full_pre",
        }
    }

    pub fn accepts_release_type(self, release_type: ReleaseType) -> bool {
        match self {
            Self::SecurityOnly => release_type == ReleaseType::Security,
            Self::SecurityFull => matches!(release_type, ReleaseType::Security | ReleaseType::Full),
            Self::SecurityFullPre => true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    body: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedRelease {
    pub version: String,
    pub release_type: ReleaseType,
    pub manifest_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyUpdate {
    pub release: SelectedRelease,
    pub manifest: ReleaseManifest,
}

/// Enumerate classified releases accepted by the selected policy, newest SemVer first.
/// Releases before the signed classification boundary are never candidates.
fn select_policy_releases(
    repository: &str,
    policy: UpdatePolicy,
) -> Result<Vec<SelectedRelease>, UpdateError> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(30))
        .user_agent("GitRun-Updater")
        .build()?;
    let mut candidates = Vec::new();
    for page in 1..=MAX_RELEASE_PAGES {
        let url = format!(
            "https://api.github.com/repos/{}/releases?per_page={RELEASES_PER_PAGE}&page={page}",
            repository.trim_end_matches('/')
        );
        let response = client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()?
            .error_for_status()?;
        let releases: Vec<GitHubRelease> = response.json()?;
        if releases.is_empty() {
            break;
        }
        for release in &releases {
            if release.draft {
                continue;
            }
            let version = release
                .tag_name
                .strip_prefix('v')
                .unwrap_or(&release.tag_name);
            if compare_versions(version, MIN_CLASSIFIED_RELEASE_VERSION)? == Ordering::Less {
                continue;
            }
            let body = release.body.as_deref().unwrap_or_default();
            let release_type = ReleaseType::from_release_body(body)?;
            if release.prerelease != (release_type == ReleaseType::Pre) {
                return Err(UpdateError::InvalidManifest(format!(
                    "GitHub prerelease flag disagrees with release type for tag {}",
                    release.tag_name
                )));
            }
            if policy.accepts_release_type(release_type) {
                candidates.push(SelectedRelease {
                    version: version.to_owned(),
                    release_type,
                    manifest_url: release_manifest_download_url(repository, version)?,
                });
            }
        }
        if releases.len() < RELEASES_PER_PAGE {
            break;
        }
        if page == MAX_RELEASE_PAGES {
            return Err(UpdateError::InvalidManifest(
                "GitHub release pagination exceeded the configured safety limit".into(),
            ));
        }
    }
    candidates.sort_by(|a, b| compare_versions(&b.version, &a.version).unwrap_or(Ordering::Equal));
    Ok(candidates)
}

/// Keep only candidates that move in the direction of the latest release in
/// the selected line. If the newest line release is already installed, an
/// older release must not be treated as a reason to downgrade.
fn releases_for_line_transition(
    candidates: Vec<SelectedRelease>,
    installed_version: &str,
) -> Result<Vec<SelectedRelease>, UpdateError> {
    let Some(latest) = candidates.first() else {
        return Ok(Vec::new());
    };
    let direction = compare_versions(&latest.version, installed_version)?;
    if direction == Ordering::Equal {
        return Ok(Vec::new());
    }
    let mut selected = Vec::new();
    for candidate in candidates {
        if compare_versions(&candidate.version, installed_version)? != direction {
            break;
        }
        selected.push(candidate);
    }
    Ok(selected)
}

/// Normal updates are strictly monotonic: only versions newer than installed qualify.
pub fn select_releases(
    repository: &str,
    installed_version: &str,
    policy: UpdatePolicy,
) -> Result<Vec<SelectedRelease>, UpdateError> {
    let candidates = select_policy_releases(repository, policy)?;
    let mut selected = Vec::new();
    for candidate in candidates {
        if compare_versions(&candidate.version, installed_version)? == Ordering::Greater {
            selected.push(candidate);
        }
    }
    Ok(selected)
}

/// Select candidates only for an explicit, operator-approved release-line transition.
pub fn select_line_transition_releases(
    repository: &str,
    installed_version: &str,
    policy: UpdatePolicy,
) -> Result<Vec<SelectedRelease>, UpdateError> {
    releases_for_line_transition(
        select_policy_releases(repository, policy)?,
        installed_version,
    )
}

pub fn select_release(
    repository: &str,
    installed_version: &str,
    policy: UpdatePolicy,
) -> Result<Option<SelectedRelease>, UpdateError> {
    Ok(select_releases(repository, installed_version, policy)?
        .into_iter()
        .next())
}

/// Resolve and cryptographically validate the manifest belonging to the
/// selected GitHub release. The manifest's own release type and version must
/// agree with the release metadata before it can be used.
pub fn resolve_policy_update(
    repository: &str,
    installed_version: &str,
    policy: UpdatePolicy,
) -> Result<Option<PolicyUpdate>, UpdateError> {
    let Some(release) = select_release(repository, installed_version, policy)? else {
        return Ok(None);
    };
    let manifest = fetch_manifest(&release.manifest_url)?;
    if manifest
        .version
        .strip_prefix('v')
        .unwrap_or(&manifest.version)
        != release.version.as_str()
        || manifest.release_type != release.release_type
    {
        return Err(UpdateError::InvalidManifest(
            "release manifest version/type does not match its GitHub release".into(),
        ));
    }
    Ok(Some(PolicyUpdate { release, manifest }))
}

/// Resolve the newest allowed release that contains an artifact for this host.
/// Signature and metadata errors fail closed; an unsupported target may fall back
/// to an older eligible release that still supports the installed architecture.
pub fn resolve_policy_update_for_target(
    repository: &str,
    installed_version: &str,
    policy: UpdatePolicy,
    target: &str,
) -> Result<Option<PolicyUpdate>, UpdateError> {
    for release in select_releases(repository, installed_version, policy)? {
        let manifest = fetch_manifest(&release.manifest_url)?;
        if manifest
            .version
            .strip_prefix('v')
            .unwrap_or(&manifest.version)
            != release.version.as_str()
            || manifest.release_type != release.release_type
        {
            return Err(UpdateError::InvalidManifest(
                "release manifest version/type does not match its GitHub release".into(),
            ));
        }
        match super::build_plan(&manifest, installed_version, target, &[]) {
            Ok(_) => return Ok(Some(PolicyUpdate { release, manifest })),
            Err(UpdateError::UnsupportedTarget(_)) => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

/// Resolve a release-line transition after the operator explicitly accepts it.
/// The latest release in the selected policy determines upgrade/downgrade direction;
/// equal versions are a no-op and target fallback never crosses that direction.
pub fn resolve_policy_update_for_line_transition_target(
    repository: &str,
    installed_version: &str,
    policy: UpdatePolicy,
    target: &str,
) -> Result<Option<PolicyUpdate>, UpdateError> {
    for release in select_line_transition_releases(repository, installed_version, policy)? {
        let manifest = fetch_manifest(&release.manifest_url)?;
        if manifest
            .version
            .strip_prefix('v')
            .unwrap_or(&manifest.version)
            != release.version.as_str()
            || manifest.release_type != release.release_type
        {
            return Err(UpdateError::InvalidManifest(
                "release manifest version/type does not match its GitHub release".into(),
            ));
        }
        match super::build_line_transition_plan(&manifest, installed_version, target, &[]) {
            Ok(_) => return Ok(Some(PolicyUpdate { release, manifest })),
            Err(UpdateError::UnsupportedTarget(_)) => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_type_marker_is_strict_and_unique() {
        assert_eq!(
            ReleaseType::from_release_body("Notes\n<!-- GITRUN_RELEASE_TYPE: security -->")
                .unwrap(),
            ReleaseType::Security
        );
        assert!(ReleaseType::from_release_body("no marker").is_err());
        assert!(ReleaseType::from_release_body(
            "<!-- GITRUN_RELEASE_TYPE: pre -->\n<!-- GITRUN_RELEASE_TYPE: full -->"
        )
        .is_err());
        assert!(ReleaseType::from_release_body("<!-- GITRUN_RELEASE_TYPE: beta -->").is_err());
    }

    #[test]
    fn release_order_uses_semver_and_never_admits_equal_or_older_versions() {
        assert_eq!(
            compare_versions("v1.9.1", "1.9.0").unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions("1.9.2-rc.1", "1.9.2").unwrap(),
            Ordering::Less
        );
        assert_eq!(
            compare_versions("1.10.0", "1.9.99").unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions("1.9.1+build.7", "1.9.1").unwrap(),
            Ordering::Equal
        );
    }

    #[test]
    fn update_policies_accept_exact_release_sets() {
        assert!(UpdatePolicy::SecurityOnly.accepts_release_type(ReleaseType::Security));
        assert!(!UpdatePolicy::SecurityOnly.accepts_release_type(ReleaseType::Full));
        assert!(!UpdatePolicy::SecurityOnly.accepts_release_type(ReleaseType::Pre));
        assert!(UpdatePolicy::SecurityFull.accepts_release_type(ReleaseType::Full));
        assert!(UpdatePolicy::SecurityFull.accepts_release_type(ReleaseType::Security));
        assert!(!UpdatePolicy::SecurityFull.accepts_release_type(ReleaseType::Pre));
        assert!(UpdatePolicy::SecurityFullPre.accepts_release_type(ReleaseType::Pre));
    }
    fn selected_release(version: &str) -> SelectedRelease {
        SelectedRelease {
            version: version.into(),
            release_type: ReleaseType::Full,
            manifest_url: format!("https://example.invalid/{version}/release-manifest.json"),
        }
    }

    #[test]
    fn line_transition_candidates_only_move_in_latest_release_direction() {
        let newer = releases_for_line_transition(
            vec![
                selected_release("1.11.0"),
                selected_release("1.10.0"),
                selected_release("1.9.9"),
            ],
            "1.10.5",
        )
        .unwrap();
        assert_eq!(
            newer.iter().map(|r| r.version.as_str()).collect::<Vec<_>>(),
            vec!["1.11.0"]
        );
        let older = releases_for_line_transition(
            vec![
                selected_release("1.10.4"),
                selected_release("1.10.3"),
                selected_release("1.10.2"),
            ],
            "1.10.5",
        )
        .unwrap();
        assert_eq!(
            older.iter().map(|r| r.version.as_str()).collect::<Vec<_>>(),
            vec!["1.10.4", "1.10.3", "1.10.2"]
        );
    }

    #[test]
    fn line_transition_does_not_downgrade_when_latest_line_version_is_already_installed() {
        let selected = releases_for_line_transition(
            vec![selected_release("1.10.5"), selected_release("1.10.4")],
            "1.10.5",
        )
        .unwrap();
        assert!(selected.is_empty());
    }
}
