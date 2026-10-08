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

use gitrun_core::AppAuth;
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
    #[error("unable to decode workflow content for {file}: {detail}")]
    WorkflowContentDecode { file: String, detail: String },
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
    #[serde(default)]
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedJobInfo {
    pub run_id: u64,
    pub job_id: u64,
    pub name: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct JobsResponse {
    #[serde(default)]
    jobs: Vec<Job>,
}

#[derive(Debug, Deserialize)]
struct Job {
    #[serde(default)]
    id: u64,
    status: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    runner_name: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
}

/// Public job identity needed by GitDockRun to resolve a named job in the
/// current workflow run back to the GitRun-managed runner container that
/// hosted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowJobInfo {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub runner_name: Option<String>,
}

/// Authoritative workflow-run identity fetched from GitHub. Request-supplied
/// workflow metadata is never accepted as authoritative without this lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunInfo {
    pub id: u64,
    pub name: String,
}

/// Splits "owner/repo" into its two parts, validating the shape up front so
/// every call site doesn't have to re-check it.
fn split_repo(repo: &str) -> Result<(&str, &str)> {
    let mut parts = repo.splitn(2, '/');
    match (parts.next(), parts.next()) {
        (Some(owner), Some(name))
            if is_valid_repo_segment(owner) && is_valid_repo_segment(name) =>
        {
            Ok((owner, name))
        }
        _ => Err(GitHubError::InvalidRepository(repo.to_owned())),
    }
}

fn is_valid_repo_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment.len() <= 100
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn encode_path_segment(segment: &str) -> String {
    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    encoded
}

/// Where a `GitHubClient` gets its bearer token from. Either a static PAT
/// (unchanged behavior from before GitHub App support existed) or a GitHub
/// App installation, which mints and auto-refreshes short-lived tokens.
enum TokenSource {
    StaticToken(String),
    App(AppAuth),
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

const DEFAULT_API_ATTEMPTS: usize = 3;

fn api_attempts() -> usize {
    std::env::var("GITRUN_API_RETRIES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_API_ATTEMPTS)
        .max(1)
}

fn retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::REQUEST_TIMEOUT
            | reqwest::StatusCode::INTERNAL_SERVER_ERROR
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT
    )
}

fn retry_delay(attempt: usize) -> Duration {
    Duration::from_secs(
        2_u64
            .saturating_pow(attempt.saturating_sub(1) as u32)
            .min(8),
    )
}

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
        Ok(Self {
            http,
            token: TokenSource::StaticToken(token),
        })
    }

    /// Authenticates as a GitHub App installation instead of a raw PAT. The
    /// returned client mints and auto-refreshes short-lived installation
    /// tokens as needed — the caller never sees or manages the token
    /// directly, same as with a PAT.
    pub fn with_app_auth(
        auth: AppAuth,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()?;
        Ok(Self {
            http,
            token: TokenSource::App(auth),
        })
    }

    fn request(&self, method: reqwest::Method, url: &str) -> Result<reqwest::blocking::Response> {
        let attempts = api_attempts();
        for attempt in 1..=attempts {
            let token = self.token.current_token()?;
            let response = match self
                .http
                .request(method.clone(), url)
                .header("Accept", "application/vnd.github+json")
                .header("Authorization", format!("Bearer {token}"))
                .header("X-GitHub-Api-Version", API_VERSION)
                .header("User-Agent", USER_AGENT)
                .send()
            {
                Ok(response) => response,
                Err(_error) if attempt < attempts => {
                    std::thread::sleep(retry_delay(attempt));
                    continue;
                }
                Err(error) => return Err(error.into()),
            };

            let status = response.status();
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs);
            let rate_limit_remaining_zero = response
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .map(|v| v == "0")
                .unwrap_or(false);

            // GitHub can return 403 for a secondary rate limit as well as for
            // ordinary permission failures. A Retry-After header on a 403 is the
            // decisive signal for the former; a primary rate limit is also
            // identified by X-RateLimit-Remaining: 0. All 429 responses are
            // rate limits by definition.
            let is_rate_limit = status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || (status == reqwest::StatusCode::FORBIDDEN
                    && (retry_after.is_some() || rate_limit_remaining_zero));

            if is_rate_limit {
                let retry_after = retry_after.or_else(|| {
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

            if !status.is_success() {
                if retryable_status(status) && attempt < attempts {
                    std::thread::sleep(retry_delay(attempt));
                    continue;
                }
                let detail = response.text().unwrap_or_default();
                let detail = detail.chars().take(500).collect();
                return Err(GitHubError::Api {
                    status: status.as_u16(),
                    detail,
                });
            }

            return Ok(response);
        }

        unreachable!("api_attempts() is always at least one")
    }
    /// Follows `Link: rel="next"` pagination and parses each runners page
    /// immediately, so a repository with many registered runners does not
    /// require materializing every page as an intermediate JSON value.
    fn get_all_runner_pages(&self, path: &str, per_page: u32) -> Result<Vec<Runner>> {
        let mut url = format!("{API_BASE}{path}?per_page={per_page}");
        let mut runners = Vec::new();
        loop {
            let response = self.request(reqwest::Method::GET, &url)?;
            let next_url = parse_next_link(response.headers());
            let parsed: RunnersResponse = response.json()?;
            runners.extend(parsed.runners);
            match next_url {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(runners)
    }

    pub fn registration_token(&self, repo: &str) -> Result<String> {
        let (owner, name) = split_repo(repo)?;
        let url = format!(
            "{API_BASE}/repos/{}/{}/actions/runners/registration-token",
            encode_path_segment(owner),
            encode_path_segment(name)
        );
        let response = self.request(reqwest::Method::POST, &url)?;
        let parsed: RegistrationTokenResponse = response.json()?;
        Ok(parsed.token)
    }

    pub fn list_runners(&self, repo: &str) -> Result<Vec<Runner>> {
        let (owner, name) = split_repo(repo)?;
        self.get_all_runner_pages(
            &format!(
                "/repos/{}/{}/actions/runners",
                encode_path_segment(owner),
                encode_path_segment(name)
            ),
            100,
        )
    }

    pub fn delete_runner(&self, repo: &str, runner_id: u64) -> Result<()> {
        let (owner, name) = split_repo(repo)?;
        let url = format!(
            "{API_BASE}/repos/{}/{}/actions/runners/{runner_id}",
            encode_path_segment(owner),
            encode_path_segment(name)
        );
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
    /// Finds a named job inside one GitHub Actions workflow run. The
    /// caller uses this to map GitDockRun --job &lt;name&gt; to the runner that
    /// executed that job.
    /// Fetch the workflow run from the repository path. The repository+run
    /// pair is authoritative; request-supplied GITHUB_* values are claims.
    pub fn find_workflow_run(&self, repo: &str, run_id: u64) -> Result<Option<WorkflowRunInfo>> {
        let (owner, name) = split_repo(repo)?;
        let url = format!(
            "{API_BASE}/repos/{}/{}/actions/runs/{run_id}",
            encode_path_segment(owner),
            encode_path_segment(name)
        );
        match self.request(reqwest::Method::GET, &url) {
            Ok(response) => {
                let run: WorkflowRun = response.json()?;
                Ok(Some(WorkflowRunInfo {
                    id: run_id,
                    name: run.name,
                }))
            }
            Err(GitHubError::Api { status: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Finds the single currently running GitHub Actions job assigned to a runner.
    /// Historical completed jobs are ignored. Multiple simultaneous assignments
    /// are treated as a conflict so the caller cannot choose an arbitrary job.
    pub fn find_active_workflow_job_for_runner(
        &self,
        repo: &str,
        runner_name: &str,
    ) -> Result<Option<WorkflowJobInfo>> {
        let (owner, name) = split_repo(repo)?;
        let mut url = format!(
            "{API_BASE}/repos/{}/{}/actions/runs?status=in_progress&per_page=100",
            encode_path_segment(owner),
            encode_path_segment(name)
        );
        let mut matches = Vec::new();

        loop {
            let response = self.request(reqwest::Method::GET, &url)?;
            let next_url = parse_next_link(response.headers());
            let parsed: WorkflowRunsResponse = response.json()?;

            for run in parsed.workflow_runs {
                let mut jobs_url = format!(
                    "{API_BASE}/repos/{}/{}/actions/runs/{}/jobs?filter=latest&per_page=100",
                    encode_path_segment(owner),
                    encode_path_segment(name),
                    run.id
                );
                loop {
                    let response = self.request(reqwest::Method::GET, &jobs_url)?;
                    let jobs_next = parse_next_link(response.headers());
                    let jobs: JobsResponse = response.json()?;
                    for job in jobs.jobs {
                        if job.runner_name.as_deref() == Some(runner_name)
                            && job.status.eq_ignore_ascii_case("in_progress")
                            && job.conclusion.is_none()
                        {
                            matches.push(WorkflowJobInfo {
                                id: job.id,
                                name: job.name,
                                status: job.status,
                                conclusion: job.conclusion,
                                runner_name: job.runner_name,
                            });
                        }
                    }
                    match jobs_next {
                        Some(next) => jobs_url = next,
                        None => break,
                    }
                }
            }

            match next_url {
                Some(next) => url = next,
                None => break,
            }
        }

        match matches.as_slice() {
            [] => Ok(None),
            [job] => Ok(Some(job.clone())),
            _ => Err(GitHubError::Api {
                status: 409,
                detail: format!("runner {runner_name} is assigned to multiple active workflow jobs"),
            }),
        }
    }

    pub fn find_workflow_job(
        &self,
        repo: &str,
        run_id: u64,
        job_name: &str,
    ) -> Result<Option<WorkflowJobInfo>> {
        let (owner, name) = split_repo(repo)?;
        let mut url = format!(
            "{API_BASE}/repos/{}/{}/actions/runs/{run_id}/jobs?per_page=100",
            encode_path_segment(owner),
            encode_path_segment(name)
        );

        loop {
            let response = self.request(reqwest::Method::GET, &url)?;
            let next_url = parse_next_link(response.headers());
            let parsed: JobsResponse = response.json()?;

            if let Some(job) = parsed.jobs.into_iter().find(|job| job.name == job_name) {
                return Ok(Some(WorkflowJobInfo {
                    id: job.id,
                    name: job.name,
                    status: job.status,
                    conclusion: job.conclusion,
                    runner_name: job.runner_name,
                }));
            }

            match next_url {
                Some(next) => url = next,
                None => return Ok(None),
            }
        }
    }

    pub fn queued_self_hosted_jobs(&self, repo: &str) -> Result<u32> {
        Ok(self.queued_self_hosted_jobs_with_labels(repo)?.len() as u32)
    }

    /// Same as `queued_self_hosted_jobs`, but returns each matching job's
    /// full label set instead of just a count — needed by Logic Containers
    /// to decide which backend/image a given queued job should get (see
    /// `logic_containers::resolve`), which a bare count can't support.
    pub fn queued_self_hosted_jobs_with_labels(&self, repo: &str) -> Result<Vec<Vec<String>>> {
        Ok(self
            .queued_self_hosted_jobs_with_info(repo)?
            .into_iter()
            .map(|job| job.labels)
            .collect())
    }

    /// Returns each queued self-hosted job with its workflow run, job ID,
    /// logical job name and labels. The run/job identity is what lets
    /// GitDockRun reserve the exact runner container rather than relying on
    /// positional label matching alone.
    pub fn queued_self_hosted_jobs_with_info(&self, repo: &str) -> Result<Vec<QueuedJobInfo>> {
        let (owner, name) = split_repo(repo)?;
        let mut jobs = Vec::new();
        let runs_path = format!(
            "/repos/{}/{}/actions/runs",
            encode_path_segment(owner),
            encode_path_segment(name)
        );
        let mut url = format!("{API_BASE}{runs_path}?status=queued&per_page=100");

        loop {
            let response = self.request(reqwest::Method::GET, &url)?;
            let next_url = parse_next_link(response.headers());
            let parsed: WorkflowRunsResponse = response.json()?;

            for run in parsed.workflow_runs {
                jobs.extend(self.queued_self_hosted_job_info_for_run(owner, name, run.id)?);
            }

            match next_url {
                Some(next) => url = next,
                None => break,
            }
        }

        Ok(jobs)
    }

    fn queued_self_hosted_job_info_for_run(
        &self,
        owner: &str,
        name: &str,
        run_id: u64,
    ) -> Result<Vec<QueuedJobInfo>> {
        let mut jobs = Vec::new();
        let mut url = format!(
            "{API_BASE}/repos/{}/{}/actions/runs/{run_id}/jobs?filter=latest&per_page=100",
            encode_path_segment(owner),
            encode_path_segment(name)
        );

        loop {
            let response = self.request(reqwest::Method::GET, &url)?;
            let next_url = parse_next_link(response.headers());
            let parsed: JobsResponse = response.json()?;

            jobs.extend(
                parsed
                    .jobs
                    .into_iter()
                    .filter(|job| {
                        job.status == "queued"
                            && job
                                .labels
                                .iter()
                                .any(|label| label.eq_ignore_ascii_case("self-hosted"))
                    })
                    .map(|job| QueuedJobInfo {
                        run_id,
                        job_id: job.id,
                        name: job.name,
                        labels: job.labels,
                    }),
            );

            match next_url {
                Some(next) => url = next,
                None => break,
            }
        }

        Ok(jobs)
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
    /// Returns an empty list (not an error) if the directory doesn't exist.
    /// A repo with no workflows yet is not a validation failure; this mirrors
    /// the same missing-directory treatment used by
    /// `workflow_validation::validate_workflows_dir`.
    pub fn workflow_files(&self, repo: &str) -> Result<Vec<(String, String)>> {
        let (owner, name) = split_repo(repo)?;
        let list_url = format!(
            "{API_BASE}/repos/{}/{}/contents/.github/workflows",
            encode_path_segment(owner),
            encode_path_segment(name)
        );
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
            let file_url = format!(
                "{API_BASE}/repos/{}/{}/contents/.github/workflows/{}",
                encode_path_segment(owner),
                encode_path_segment(name),
                encode_path_segment(&entry.name)
            );
            let file: ContentsFile = self.request(reqwest::Method::GET, &file_url)?.json()?;
            let content = decode_contents_base64(&file.content).map_err(|detail| {
                GitHubError::WorkflowContentDecode {
                    file: entry.name.clone(),
                    detail,
                }
            })?;
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
/// for readability in raw API responses). Returns the decoding error to the
/// caller so a workflow that was not actually retrieved is never mistaken for
/// an empty, successfully fetched file.
fn decode_contents_base64(raw: &str) -> std::result::Result<String, String> {
    use base64::Engine;
    let cleaned: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .map_err(|error| error.to_string())?;
    String::from_utf8(bytes).map_err(|error| error.to_string())
}

/// Parses the `Link` header for a `rel="next"` URL, GitHub's standard
/// pagination mechanism (RFC 8288). Returns `None` when there is no next page.
fn parse_next_link(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let raw = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    split_link_values(raw).into_iter().find_map(|part| {
        let mut segments = part.split(';');
        let url_part = segments.next()?.trim();
        if !url_part.starts_with('<') || !url_part.ends_with('>') {
            return None;
        }
        let is_next = segments.any(|s| {
            let mut pair = s.splitn(2, '=');
            let key = pair.next().map(str::trim);
            let value = pair.next().map(str::trim);
            matches!(
                (key, value),
                (Some("rel"), Some("\"next\"")) | (Some("rel"), Some("next"))
            )
        });
        if !is_next {
            return None;
        }
        Some(url_part[1..url_part.len() - 1].to_owned())
    })
}

/// Splits a Link header into link-values while respecting quoted strings.
/// A comma inside a quoted parameter value therefore cannot accidentally
/// terminate the current link-value.
fn split_link_values(raw: &str) -> Vec<&str> {
    let mut values = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escaped = false;

    for (index, byte) in raw.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if in_quotes => escaped = true,
            b'"' => in_quotes = !in_quotes,
            b',' if !in_quotes => {
                values.push(raw[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    values.push(raw[start..].trim());
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_valid_repository() {
        assert_eq!(
            split_repo("octocat/hello-world").unwrap(),
            ("octocat", "hello-world")
        );
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
    fn rejects_path_traversal_segments() {
        assert!(split_repo("../repo").is_err());
        assert!(split_repo("owner/..").is_err());
        assert!(split_repo("./repo").is_err());
    }

    #[test]
    fn rejects_empty_token() {
        assert!(matches!(
            GitHubClient::new("   "),
            Err(GitHubError::MissingToken)
        ));
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
    fn parses_link_header_with_comma_inside_quoted_parameter() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::LINK,
            "<https://api.github.com/resource?page=2>; title=\"a,b\"; rel=\"next\", <https://api.github.com/resource?page=5>; rel=\"last\""
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
        let runner = Runner {
            id: 1,
            name: "r".into(),
            status: "online".into(),
            busy: false,
        };
        assert!(runner.is_online());
        let offline = Runner {
            id: 1,
            name: "r".into(),
            status: "offline".into(),
            busy: false,
        };
        assert!(!offline.is_online());
    }

    /// Pins the GitHub API version string so a change to it can only happen
    /// via a deliberate edit to this test, not an accidental find-replace or
    /// IDE "helpful" rewrite elsewhere in the file. If GitHub deprecates the
    /// pinned version, update both this assertion and the constant together
    /// in the same change, ideally after checking GitHub's API version
    /// changelog for behavioral differences.
    #[test]
    fn retryable_status_recognizes_transient_http_failures() {
        assert!(retryable_status(reqwest::StatusCode::REQUEST_TIMEOUT));
        assert!(retryable_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR));
        assert!(retryable_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(retryable_status(reqwest::StatusCode::SERVICE_UNAVAILABLE));
        assert!(retryable_status(reqwest::StatusCode::GATEWAY_TIMEOUT));
        assert!(!retryable_status(reqwest::StatusCode::FORBIDDEN));
        assert!(!retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
    }

    #[test]
    fn retry_delay_is_bounded_exponential_backoff() {
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(2), Duration::from_secs(2));
        assert_eq!(retry_delay(3), Duration::from_secs(4));
        assert_eq!(retry_delay(10), Duration::from_secs(8));
    }
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
        assert_eq!(decode_contents_base64(wrapped).unwrap(), "cargo build");
    }

    #[test]
    fn decode_contents_base64_returns_error_on_garbage() {
        assert!(decode_contents_base64("not valid base64!!!").is_err());
    }

    #[test]
    fn encode_path_segment_escapes_reserved_characters() {
        assert_eq!(
            encode_path_segment("hello world#file"),
            "hello%20world%23file"
        );
    }
}
