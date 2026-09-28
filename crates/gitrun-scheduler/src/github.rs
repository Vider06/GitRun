//! GitHub Actions API client for the autoscaler.
//!
//! This replaces the Python `api_request`/`registration_token`/`list_runners`/
//! `queued_jobs`/`remove_runner` functions from `autoscaler/gitrun_manager.py`.
//!
//! Two behavioral fixes versus the Python original:
//! 1. Rate-limit awareness: GitHub sends `X-RateLimit-Remaining` and
//!    `X-RateLimit-Reset` on every response, and `Retry-After` on 403/429
//!    specifically. The Python client ignored all of them and would just
//!    retry blindly on the next 5s poll tick, which can turn a transient
//!    rate limit into a sustained one under load. This client surfaces a
//!    `RateLimited { retry_after }` error variant so the caller (the
//!    reconcile loop) can back off deliberately instead of hammering the API.
//! 2. Pagination: `queued_jobs` in Python only ever requested `per_page=100`
//!    with no follow-up, silently undercounting on repos with more than 100
//!    queued workflow runs. This client follows the `Link: rel="next"`
//!    header until exhausted.

use serde::Deserialize;
use std::time::Duration;
use thiserror::Error;

const API_BASE: &str = "https://api.github.com";
const API_VERSION: &str = "2026-03-10";
const USER_AGENT: &str = concat!("GitRun/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Error)]
pub enum GitHubError {
    #[error("GITHUB_TOKEN is not configured")]
    MissingToken,
    #[error("invalid repository name: {0} (expected owner/repo)")]
    InvalidRepository(String),
    #[error("network error contacting GitHub: {0}")]
    Network(#[from] reqwest::Error),
    #[error("GitHub API rate limit exceeded, retry after {retry_after:?}")]
    RateLimited { retry_after: Option<Duration> },
    #[error("GitHub API error {status}: {detail}")]
    Api { status: u16, detail: String },
    #[error("unexpected response shape from GitHub: {0}")]
    Decode(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, GitHubError>;

#[derive(Debug, Clone, Deserialize)]
pub struct Runner {
    pub id: u64,
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub busy: bool,
}

impl Runner {
    pub fn is_online(&self) -> bool {
        self.status == "online"
    }
}

#[derive(Debug, Deserialize)]
struct RunnersResponse {
    #[serde(default)]
    runners: Vec<Runner>,
}

#[derive(Debug, Deserialize)]
struct RegistrationTokenResponse {
    token: String,
}

#[derive(Debug, Deserialize)]
struct WorkflowRunsResponse {
    #[serde(default)]
    workflow_runs: Vec<WorkflowRun>,
}

#[derive(Debug, Deserialize)]
struct WorkflowRun {
    id: u64,
}

#[derive(Debug, Deserialize)]
struct JobsResponse {
    #[serde(default)]
    jobs: Vec<Job>,
}

#[derive(Debug, Deserialize)]
struct Job {
    status: String,
    #[serde(default)]
    labels: Vec<String>,
}

/// Splits "owner/repo" into its two parts, validating the shape up front so
/// every call site doesn't have to re-check it.
fn split_repo(repo: &str) -> Result<(&str, &str)> {
    let mut parts = repo.splitn(2, '/');
    match (parts.next(), parts.next()) {
        (Some(owner), Some(name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
            Ok((owner, name))
        }
        _ => Err(GitHubError::InvalidRepository(repo.to_owned())),
    }
}

/// Where a `GitHubClient` gets its bearer token from. Either a static PAT
/// (unchanged behavior from before GitHub App support existed) or a GitHub
/// App installation, which mints and auto-refreshes short-lived tokens.
enum TokenSource {
    StaticToken(String),
    App(crate::app_auth::AppAuth),
}

impl TokenSource {
    fn current_token(&self) -> Result<String> {
        match self {
            TokenSource::StaticToken(token) => Ok(token.clone()),
            TokenSource::App(auth) => auth.token().map_err(|error| GitHubError::Api {
                status: 0,
                detail: format!("GitHub App auth failed: {error}"),
            }),
        }
    }
}

pub struct GitHubClient {
    http: reqwest::blocking::Client,
    token: TokenSource,
}

/// Default timeouts used by `GitHubClient::new`. Split into connect vs
/// overall request so a slow-to-connect network (bad DNS, dead route) fails
/// fast without also capping how long a legitimately large paginated
/// response (many queued jobs across many runs) is allowed to take.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

impl GitHubClient {
    pub fn new(token: impl Into<String>) -> Result<Self> {
        Self::with_timeouts(token, DEFAULT_CONNECT_TIMEOUT, DEFAULT_REQUEST_TIMEOUT)
    }

    /// Same as `new`, but with explicit connect/request timeouts instead of
    /// the defaults — for callers (or GSR, later) that want tighter bounds,
    /// e.g. to fail over to a degraded mode faster under known-bad network
    /// conditions.
    pub fn with_timeouts(
        token: impl Into<String>,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(GitHubError::MissingToken);
        }
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()?;
        Ok(Self { http, token: TokenSource::StaticToken(token) })
    }

    /// Authenticates as a GitHub App installation instead of a raw PAT. The
    /// returned client mints and auto-refreshes short-lived installation
    /// tokens as needed — the caller never sees or manages the token
    /// directly, same as with a PAT.
    pub fn with_app_auth(
        auth: crate::app_auth::AppAuth,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()?;
        Ok(Self { http, token: TokenSource::App(auth) })
    }

    fn request(
        &self,
        method: reqwest::Method,
        url: &str,
    ) -> Result<reqwest::blocking::Response> {
        let token = self.token.current_token()?;
        let response = self
            .http
            .request(method, url)
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", format!("Bearer {token}"))
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("User-Agent", USER_AGENT)
            .send()?;

        let status = response.status();
        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let is_rate_limit = response
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .map(|v| v == "0")
                .unwrap_or(status == reqwest::StatusCode::TOO_MANY_REQUESTS);

            if is_rate_limit {
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .or_else(|| {
                        response
                            .headers()
                            .get("x-ratelimit-reset")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse::<i64>().ok())
                            .map(|reset_epoch| {
                                let now = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs() as i64)
                                    .unwrap_or(0);
                                Duration::from_secs((reset_epoch - now).max(1) as u64)
                            })
                    });
                return Err(GitHubError::RateLimited { retry_after });
            }
        }

        if !status.is_success() {
            let detail = response.text().unwrap_or_default();
            let detail = detail.chars().take(500).collect();
            return Err(GitHubError::Api { status: status.as_u16(), detail });
        }

        Ok(response)
    }

    /// Follows `Link: rel="next"` pagination, returning every page's raw JSON
    /// body collected into one `Vec`. (Only `list_runners` uses this
    /// generically; `queued_self_hosted_jobs` and its per-run helper
    /// paginate inline instead, since they need to filter+count each page's
    /// jobs on the fly rather than materialize every page in memory.)
    fn get_all_pages(&self, path: &str, per_page: u32) -> Result<Vec<serde_json::Value>> {
        let mut url = format!("{API_BASE}{path}?per_page={per_page}");
        let mut pages = Vec::new();
        loop {
            let response = self.request(reqwest::Method::GET, &url)?;
            let next_url = parse_next_link(response.headers());
            let body: serde_json::Value = response.json()?;
            pages.push(body);
            match next_url {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(pages)
    }

    pub fn registration_token(&self, repo: &str) -> Result<String> {
        let (owner, name) = split_repo(repo)?;
        let url = format!("{API_BASE}/repos/{owner}/{name}/actions/runners/registration-token");
        let response = self.request(reqwest::Method::POST, &url)?;
        let parsed: RegistrationTokenResponse = response.json()?;
        Ok(parsed.token)
    }

    pub fn list_runners(&self, repo: &str) -> Result<Vec<Runner>> {
        let (owner, name) = split_repo(repo)?;
        let mut runners = Vec::new();
        for page in self.get_all_pages(&format!("/repos/{owner}/{name}/actions/runners"), 100)? {
            let parsed: RunnersResponse = serde_json::from_value(page)?;
            runners.extend(parsed.runners);
        }
        Ok(runners)
    }

    pub fn delete_runner(&self, repo: &str, runner_id: u64) -> Result<()> {
        let (owner, name) = split_repo(repo)?;
        let url = format!("{API_BASE}/repos/{owner}/{name}/actions/runners/{runner_id}");
        match self.request(reqwest::Method::DELETE, &url) {
            Ok(_) => Ok(()),
            // Already gone is not an error for our purposes — mirrors the
            // Python behavior of swallowing 404 specifically.
            Err(GitHubError::Api { status: 404, .. }) => Ok(()),
            Err(other) => Err(other),
        }
    }

    /// Counts queued jobs across all queued workflow runs that request a
    /// self-hosted runner. Paginates both the run list and, implicitly, is
    /// bounded per-run by GitHub's own per-run job count (jobs are paginated
    /// too, followed the same way).
    pub fn queued_self_hosted_jobs(&self, repo: &str) -> Result<u32> {
        Ok(self.queued_self_hosted_jobs_with_labels(repo)?.len() as u32)
    }

    /// Same as `queued_self_hosted_jobs`, but returns each matching job's
    /// full label set instead of just a count — needed by Logic Containers
    /// to decide which backend/image a given queued job should get (see
    /// `logic_containers::resolve`), which a bare count can't support.
    pub fn queued_self_hosted_jobs_with_labels(&self, repo: &str) -> Result<Vec<Vec<String>>> {
        let (owner, name) = split_repo(repo)?;
        let mut all_labels = Vec::new();
        let runs_path = format!("/repos/{owner}/{name}/actions/runs");
        let mut url = format!("{API_BASE}{runs_path}?status=queued&per_page=100");
        loop {
            let response = self.request(reqwest::Method::GET, &url)?;
            let next_url = parse_next_link(response.headers());
            let parsed: WorkflowRunsResponse = response.json()?;
            for run in parsed.workflow_runs {
                all_labels.extend(self.queued_self_hosted_job_labels_for_run(owner, name, run.id)?);
            }
            match next_url {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(all_labels)
    }

    fn queued_self_hosted_job_labels_for_run(&self, owner: &str, name: &str, run_id: u64) -> Result<Vec<Vec<String>>> {
        let mut labels = Vec::new();
        let mut url = format!(
            "{API_BASE}/repos/{owner}/{name}/actions/runs/{run_id}/jobs?filter=latest&per_page=100"
        );
        loop {
            let response = self.request(reqwest::Method::GET, &url)?;
            let next_url = parse_next_link(response.headers());
            let parsed: JobsResponse = response.json()?;
            labels.extend(
                parsed
                    .jobs
                    .into_iter()
                    .filter(|job| job.status == "queued" && job.labels.iter().any(|label| label.eq_ignore_ascii_case("self-hosted")))
                    .map(|job| job.labels),
            );
            match next_url {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(labels)
    }

    /// Fetches every `.yml`/`.yaml` file directly under
    /// `.github/workflows/` in `repo`'s default branch, via the Contents
    /// API — no clone/checkout needed, which matters because this runs
    /// *before* a runner container exists (see `gitrun_core::workflow_validation`
    /// and its `Config::gsr_workflow_validation_enabled` caller in
    /// `main.rs`'s `create_runner`): the actual checkout the official
    /// runner performs happens only after the container starts and picks
    /// up a job, which is too late for a pre-flight check to matter.
    ///
    /// Returns an empty list (not an error) if the directory doesn't exist
    /// - a repo with no workflows yet is not a validation failure, mirroring
    /// `workflow_validation::validate_workflows_dir`'s same treatment of a
    /// missing local directory.
    pub fn workflow_files(&self, repo: &str) -> Result<Vec<(String, String)>> {
        let (owner, name) = split_repo(repo)?;
        let list_url = format!("{API_BASE}/repos/{owner}/{name}/contents/.github/workflows");
        let entries: Vec<ContentsEntry> = match self.request(reqwest::Method::GET, &list_url) {
            Ok(response) => response.json()?,
            Err(GitHubError::Api { status: 404, .. }) => return Ok(Vec::new()),
            Err(other) => return Err(other),
        };

        let mut files = Vec::new();
        for entry in entries {
            let is_yaml = entry.name.ends_with(".yml") || entry.name.ends_with(".yaml");
            if entry.entry_type != "file" || !is_yaml {
                continue;
            }
            let file_url = format!("{API_BASE}/repos/{owner}/{name}/contents/.github/workflows/{}", entry.name);
            let file: ContentsFile = self.request(reqwest::Method::GET, &file_url)?.json()?;
            let content = decode_contents_base64(&file.content).unwrap_or_default();
            files.push((entry.name, content));
        }
        Ok(files)
    }
}

#[derive(Debug, Deserialize)]
struct ContentsEntry {
    name: String,
    #[serde(rename = "type")]
    entry_type: String,
}

#[derive(Debug, Deserialize)]
struct ContentsFile {
    /// Base64-encoded file content, GitHub-style: whitespace/newlines
    /// interspersed for readability in raw API responses, so this must be
    /// stripped before decoding (see `decode_contents_base64`) rather than
    /// fed straight to a base64 decoder.
    content: String,
}

/// Decodes a GitHub Contents API `content` field: base64 with embedded
/// newlines that must be stripped first (GitHub wraps the encoded content
/// for readability in raw API responses). Returns `None` on any malformed
/// input or non-UTF-8 result rather than erroring the whole call, since
/// this is decoding attacker-influenced repository content and a single
/// unreadable workflow file shouldn't take down the rest of validation.
fn decode_contents_base64(raw: &str) -> Option<String> {
    use base64::Engine;
    let cleaned: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD.decode(cleaned).ok()?;
    String::from_utf8(bytes).ok()
}

/// Parses the `Link` header for a `rel="next"` URL, GitHub's standard
/// pagination mechanism (RFC 8288). Returns `None` when there is no next page.
fn parse_next_link(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let raw = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    raw.split(',').find_map(|part| {
        let mut segments = part.split(';');
        let url_part = segments.next()?.trim();
        let is_next = segments.any(|s| s.trim() == "rel=\"next\"");
        if !is_next {
            return None;
        }
        url_part.trim_start_matches('<').trim_end_matches('>').to_owned().into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_valid_repository() {
        assert_eq!(split_repo("octocat/hello-world").unwrap(), ("octocat", "hello-world"));
    }

    #[test]
    fn rejects_missing_slash() {
        assert!(split_repo("octocat").is_err());
    }

    #[test]
    fn rejects_extra_slash() {
        assert!(split_repo("octocat/hello/world").is_err());
    }

    #[test]
    fn rejects_empty_owner_or_name() {
        assert!(split_repo("/hello-world").is_err());
        assert!(split_repo("octocat/").is_err());
    }

    #[test]
    fn rejects_empty_token() {
        assert!(matches!(GitHubClient::new("   "), Err(GitHubError::MissingToken)));
    }

    #[test]
    fn parses_link_header_next() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::LINK,
            "<https://api.github.com/resource?page=2>; rel=\"next\", <https://api.github.com/resource?page=5>; rel=\"last\""
                .parse()
                .unwrap(),
        );
        assert_eq!(
            parse_next_link(&headers).as_deref(),
            Some("https://api.github.com/resource?page=2")
        );
    }

    #[test]
    fn no_link_header_means_no_next_page() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(parse_next_link(&headers), None);
    }

    #[test]
    fn runner_is_online_only_when_status_matches() {
        let runner = Runner { id: 1, name: "r".into(), status: "online".into(), busy: false };
        assert!(runner.is_online());
        let offline = Runner { id: 1, name: "r".into(), status: "offline".into(), busy: false };
        assert!(!offline.is_online());
    }

    /// Pins the GitHub API version string so a change to it can only happen
    /// via a deliberate edit to this test, not an accidental find-replace or
    /// IDE "helpful" rewrite elsewhere in the file. If GitHub deprecates the
    /// pinned version, update both this assertion and the constant together
    /// in the same change, ideally after checking GitHub's API version
    /// changelog for behavioral differences.
    #[test]
    fn api_version_is_pinned_deliberately() {
        assert_eq!(API_VERSION, "2026-03-10");
    }

    #[test]
    fn decodes_github_style_wrapped_base64() {
        // GitHub's Contents API wraps base64 content with embedded
        // newlines for readability; "cargo build" base64-encoded, split
        // across two lines the way a real API response would.
        let wrapped = "Y2FyZ28g\nYnVpbGQ=";
        assert_eq!(decode_contents_base64(wrapped).as_deref(), Some("cargo build"));
    }

    #[test]
    fn decode_contents_base64_returns_none_on_garbage() {
        assert_eq!(decode_contents_base64("not valid base64!!!"), None);
    }
}
