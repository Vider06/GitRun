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
use std::collections::HashSet;
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
    #[error("workflow run ID must be greater than zero")]
    InvalidWorkflowRunId,
    #[error("network error contacting GitHub: {0}")]
    Network(#[from] reqwest::Error),
    #[error("GitHub API rate limit exceeded, retry after {retry_after:?}")]
    RateLimited { retry_after: Option<Duration> },
    #[error("GitHub API error {status}: {detail}")]
    Api { status: u16, detail: String },
    #[error("GitHub pagination repeated a previously visited URL: {0}")]
    PaginationLoop(String),
    #[error("unable to decode workflow content for {file}: {detail}")]
    WorkflowContentDecode { file: String, detail: String },
    #[error("unexpected response shape from GitHub: {0}")]
    Decode(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, GitHubError>;

/// Prevent malformed/repeated Link headers from trapping the scheduler in an
/// infinite pagination loop. URLs have already been restricted to api.github.com.
fn record_page_url(seen: &mut HashSet<String>, url: &str) -> Result<()> {
    if !seen.insert(url.to_owned()) {
        return Err(GitHubError::PaginationLoop(url.to_owned()));
    }
    Ok(())
}

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

#[derive(Debug, Clone)]
pub struct RepositoryActivitySummary {
    pub repository: String,
    pub stars: u64,
    pub forks: u64,
    pub open_pull_requests: u64,
    pub merged_pull_requests: u64,
    pub pushed_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RepositoryMetadataResponse {
    #[serde(default)]
    stargazers_count: u64,
    #[serde(default)]
    forks_count: u64,
    pushed_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchCountResponse {
    #[serde(default)]
    total_count: u64,
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
    workflow_id: u64,
    #[serde(default)]
    path: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    run_started_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowRunSummary {
    pub total: u32,
    pub queued: u32,
    pub in_progress: u32,
    pub succeeded: u32,
    pub failed: u32,
    pub cancelled: u32,
}

/// An active self-hosted job paired with the exact registered runner name
/// reported by GitHub. Used only for optional per-container resource telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveJobOnRunner {
    pub runner_name: String,
    pub job: QueuedJobInfo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedJobInfo {
    pub run_id: u64,
    pub job_id: u64,
    pub workflow_id: u64,
    pub workflow_path: String,
    pub name: String,
    pub labels: Vec<String>,
    /// GitHub's authoritative start timestamp; None while the job is only queued.
    pub started_at_epoch: Option<u64>,
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
    #[serde(default)]
    started_at: Option<String>,
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
    pub started_at_epoch: Option<u64>,
}

/// Parse GitHub's RFC3339 run_started_at timestamp into Unix seconds without
/// accepting partial dates, invalid calendar values, or unbounded numeric fields.
fn job_targets_gitrun_runner(labels: &[String]) -> bool {
    labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case("self-hosted"))
        || gitrun_smart_scheduler::has_workload_routing_marker(labels)
}

fn parse_github_timestamp_epoch(raw: &str) -> Option<u64> {
    let (date, time) = raw.trim().split_once('T')?;
    let mut date_parts = date.split('-');
    let year = date_parts.next()?.parse::<i64>().ok()?;
    let month = date_parts.next()?.parse::<i64>().ok()?;
    let day = date_parts.next()?.parse::<i64>().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=month_days).contains(&day) {
        return None;
    }

    let core_end = time.find(['Z', 'z', '+', '-']).unwrap_or(time.len());
    let core = time.get(..core_end)?;
    let mut time_parts = core.split(':');
    let hour = time_parts.next()?.parse::<i64>().ok()?;
    let minute = time_parts.next()?.parse::<i64>().ok()?;
    let second = time_parts.next()?.split('.').next()?.parse::<i64>().ok()?;
    if time_parts.next().is_some()
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return None;
    }

    let suffix = &time[core_end..];
    let offset_seconds = if suffix.is_empty() || suffix.eq_ignore_ascii_case("z") {
        0
    } else {
        let sign = match suffix.as_bytes().first().copied()? {
            b'+' => 1_i64,
            b'-' => -1_i64,
            _ => return None,
        };
        let (offset_hour, offset_minute) = suffix[1..].split_once(':')?;
        let offset_hour = offset_hour.parse::<i64>().ok()?;
        let offset_minute = offset_minute.parse::<i64>().ok()?;
        if !(0..=23).contains(&offset_hour) || !(0..=59).contains(&offset_minute) {
            return None;
        }
        sign * (offset_hour * 3600 + offset_minute * 60)
    };

    // Howard Hinnant's civil-date conversion, Gregorian calendar.
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)?
        .checked_sub(offset_seconds)?;
    u64::try_from(seconds).ok()
}

#[cfg(test)]
mod timestamp_tests {
    use super::parse_github_timestamp_epoch;

    #[test]
    fn parses_github_utc_and_offset_timestamps() {
        assert_eq!(
            parse_github_timestamp_epoch("1970-01-01T00:00:00Z"),
            Some(0)
        );
        assert_eq!(
            parse_github_timestamp_epoch("1970-01-01T01:00:00+01:00"),
            Some(0)
        );
        assert_eq!(parse_github_timestamp_epoch("2024-02-30T12:00:00Z"), None);
    }
}

/// Splits "owner/repo" into its two parts, validating the shape up front so
/// every call site doesn't have to re-check it.
fn split_repo(repo: &str) -> Result<(&str, &str)> {
    let mut parts = repo.splitn(2, '/');
    match (parts.next(), parts.next()) {
        (Some(owner), Some(name))
            if is_valid_owner_segment(owner) && is_valid_repo_segment(name) =>
        {
            Ok((owner, name))
        }
        _ => Err(GitHubError::InvalidRepository(repo.to_owned())),
    }
}

fn is_valid_owner_segment(segment: &str) -> bool {
    segment.len() <= 39 && is_valid_repo_segment(segment)
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
const MAX_API_ATTEMPTS: usize = 10;

fn parse_api_attempts(value: Option<&str>) -> usize {
    value
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_API_ATTEMPTS)
        .clamp(1, MAX_API_ATTEMPTS)
}

fn api_attempts() -> usize {
    parse_api_attempts(std::env::var("GITRUN_API_RETRIES").ok().as_deref())
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

fn rate_limit_reset_delay(reset_epoch: i64, now_epoch: i64) -> Duration {
    // Header values are untrusted response data. Saturation avoids a debug
    // panic (or release-mode wrap) if reset_epoch is i64::MIN.
    Duration::from_secs(reset_epoch.saturating_sub(now_epoch).max(1) as u64)
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
                            rate_limit_reset_delay(reset_epoch, now)
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
        let mut seen_pages = HashSet::new();
        loop {
            record_page_url(&mut seen_pages, &url)?;
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

    /// Counts the most recent workflow runs by status. This is deliberately
    /// labelled as workflow runs (not individual jobs) by the dashboard.
    pub fn recent_workflow_run_summary(
        &self,
        repo: &str,
        per_page: u32,
    ) -> Result<WorkflowRunSummary> {
        let (owner, name) = split_repo(repo)?;
        let url = format!(
            "{API_BASE}/repos/{}/{}/actions/runs?per_page={}",
            encode_path_segment(owner),
            encode_path_segment(name),
            per_page.clamp(1, 100)
        );
        let parsed: WorkflowRunsResponse = self.request(reqwest::Method::GET, &url)?.json()?;
        let mut summary = WorkflowRunSummary::default();
        for run in parsed.workflow_runs {
            summary.total += 1;
            match run.status.to_ascii_lowercase().as_str() {
                "queued" | "requested" | "waiting" => summary.queued += 1,
                "in_progress" => summary.in_progress += 1,
                "completed" => match run
                    .conclusion
                    .as_deref()
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "success" => summary.succeeded += 1,
                    "failure" | "timed_out" | "startup_failure" => summary.failed += 1,
                    "cancelled" | "action_required" | "stale" | "skipped" => summary.cancelled += 1,
                    _ => {}
                },
                _ => {}
            }
        }
        Ok(summary)
    }

    /// Lists every currently in-progress workflow run, following GitHub pagination.
    /// This is intentionally separate from the recent-run summary: an old but
    /// still-running workflow must remain eligible for explicit force-cancellation.
    pub fn list_active_workflow_runs(&self, repo: &str) -> Result<Vec<WorkflowRunInfo>> {
        let (owner, name) = split_repo(repo)?;
        let mut url = format!(
            "{API_BASE}/repos/{}/{}/actions/runs?status=in_progress&per_page=100",
            encode_path_segment(owner),
            encode_path_segment(name)
        );
        let mut seen = HashSet::new();
        let mut runs = Vec::new();
        loop {
            record_page_url(&mut seen, &url)?;
            let response = self.request(reqwest::Method::GET, &url)?;
            let next_url = parse_next_link(response.headers());
            let parsed: WorkflowRunsResponse = response.json()?;
            runs.extend(
                parsed
                    .workflow_runs
                    .into_iter()
                    .filter(|run| run.status.eq_ignore_ascii_case("in_progress"))
                    .map(|run| WorkflowRunInfo {
                        id: run.id,
                        name: run.name,
                        started_at_epoch: run
                            .run_started_at
                            .as_deref()
                            .and_then(parse_github_timestamp_epoch),
                    }),
            );
            match next_url {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(runs)
    }

    /// Force-cancels an entire workflow run through GitHub's supported API.
    /// GitHub exposes force-cancel at run scope, not individual-job scope, so
    /// callers must identify the run explicitly and warn that all its jobs are affected.
    pub fn force_cancel_workflow_run(&self, repo: &str, run_id: u64) -> Result<()> {
        if run_id == 0 {
            return Err(GitHubError::InvalidWorkflowRunId);
        }
        let (owner, name) = split_repo(repo)?;
        let url = format!(
            "{API_BASE}/repos/{}/{}/actions/runs/{run_id}/force-cancel",
            encode_path_segment(owner),
            encode_path_segment(name)
        );
        self.request(reqwest::Method::POST, &url)?;
        Ok(())
    }

    /// Fetches repository metadata and pull-request totals for dashboard use.
    /// The dashboard caches these results for several minutes so navigation
    /// does not turn into repeated GitHub API polling.
    pub fn repository_activity_summary(&self, repo: &str) -> Result<RepositoryActivitySummary> {
        let (owner, name) = split_repo(repo)?;
        let owner_segment = encode_path_segment(owner);
        let name_segment = encode_path_segment(name);
        let metadata_url = format!("{API_BASE}/repos/{owner_segment}/{name_segment}");
        let open_url = format!(
            "{API_BASE}/search/issues?q=repo%3A{owner_segment}%2F{name_segment}%20is%3Apr%20is%3Aopen&per_page=1"
        );
        let merged_url = format!(
            "{API_BASE}/search/issues?q=repo%3A{owner_segment}%2F{name_segment}%20is%3Apr%20is%3Amerged&per_page=1"
        );

        // All three values are independent. Running the requests together
        // makes the snapshot latency approach the slowest endpoint rather
        // than the sum of all three endpoint latencies.
        let (metadata, open, merged) = std::thread::scope(|scope| {
            let metadata_query = scope.spawn(|| {
                self.request(reqwest::Method::GET, &metadata_url)
                    .and_then(|response| {
                        response
                            .json::<RepositoryMetadataResponse>()
                            .map_err(Into::into)
                    })
            });
            let open_query = scope.spawn(|| {
                self.request(reqwest::Method::GET, &open_url)
                    .and_then(|response| response.json::<SearchCountResponse>().map_err(Into::into))
            });
            let merged_query = scope.spawn(|| {
                self.request(reqwest::Method::GET, &merged_url)
                    .and_then(|response| response.json::<SearchCountResponse>().map_err(Into::into))
            });

            let metadata = metadata_query.join().unwrap_or_else(|_| {
                Err(GitHubError::Api {
                    status: 0,
                    detail: "repository metadata query worker panicked".into(),
                })
            })?;
            let open = open_query.join().unwrap_or_else(|_| {
                Err(GitHubError::Api {
                    status: 0,
                    detail: "open pull-request query worker panicked".into(),
                })
            })?;
            let merged = merged_query.join().unwrap_or_else(|_| {
                Err(GitHubError::Api {
                    status: 0,
                    detail: "merged pull-request query worker panicked".into(),
                })
            })?;
            Ok::<_, GitHubError>((metadata, open, merged))
        })?;

        Ok(RepositoryActivitySummary {
            repository: repo.to_owned(),
            stars: metadata.stargazers_count,
            forks: metadata.forks_count,
            open_pull_requests: open.total_count,
            merged_pull_requests: merged.total_count,
            pushed_at: metadata.pushed_at,
        })
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
                    started_at_epoch: run
                        .run_started_at
                        .as_deref()
                        .and_then(parse_github_timestamp_epoch),
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
        let mut seen_run_pages = HashSet::new();

        loop {
            record_page_url(&mut seen_run_pages, &url)?;
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
                let mut seen_job_pages = HashSet::new();
                loop {
                    record_page_url(&mut seen_job_pages, &jobs_url)?;
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
                detail: format!(
                    "runner {runner_name} is assigned to multiple active workflow jobs"
                ),
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

        let mut matches = Vec::new();
        let mut seen_pages = HashSet::new();
        loop {
            record_page_url(&mut seen_pages, &url)?;
            let response = self.request(reqwest::Method::GET, &url)?;
            let next_url = parse_next_link(response.headers());
            let parsed: JobsResponse = response.json()?;
            matches.extend(parsed.jobs.into_iter().filter(|job| job.name == job_name));

            match next_url {
                Some(next) => url = next,
                None => break,
            }
        }

        match matches.as_slice() {
            [] => Ok(None),
            [job] => Ok(Some(WorkflowJobInfo {
                id: job.id,
                name: job.name.clone(),
                status: job.status.clone(),
                conclusion: job.conclusion.clone(),
                runner_name: job.runner_name.clone(),
            })),
            _ => Err(GitHubError::Api {
                status: 409,
                detail: format!("workflow run {run_id} contains multiple jobs named {job_name:?}"),
            }),
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

    /// Returns queued GitHub Actions jobs with their workflow/run/job metadata.
    /// Queued jobs can live in either a queued run or an already in-progress run
    /// whose earlier jobs have started. Both run states are paginated and
    /// de-duplicated before the job list is returned.
    pub fn queued_self_hosted_jobs_with_info(&self, repo: &str) -> Result<Vec<QueuedJobInfo>> {
        let (owner, name) = split_repo(repo)?;
        let mut jobs = Vec::new();
        let runs_path = format!(
            "/repos/{}/{}/actions/runs",
            encode_path_segment(owner),
            encode_path_segment(name)
        );
        let mut seen_run_ids = HashSet::new();

        for status in ["queued", "in_progress"] {
            let mut url = format!("{API_BASE}{runs_path}?status={status}&per_page=100");
            let mut seen_pages = HashSet::new();
            loop {
                record_page_url(&mut seen_pages, &url)?;
                let response = self.request(reqwest::Method::GET, &url)?;
                let next_url = parse_next_link(response.headers());
                let parsed: WorkflowRunsResponse = response.json()?;

                for run in parsed.workflow_runs {
                    if !seen_run_ids.insert(run.id) {
                        continue;
                    }
                    jobs.extend(self.queued_self_hosted_job_info_for_run(
                        owner,
                        name,
                        run.id,
                        run.workflow_id,
                        &run.path,
                    )?);
                }

                match next_url {
                    Some(next) => url = next,
                    None => break,
                }
            }
        }

        Ok(jobs)
    }

    /// Samples jobs that actually started in a small set of recent runs.
    /// This is intentionally rate-bounded and called on Kitty's slower history
    /// cadence, not on every 30-second reconciliation tick.
    pub fn recent_self_hosted_job_starts(
        &self,
        repo: &str,
        max_runs: usize,
    ) -> Result<Vec<QueuedJobInfo>> {
        let (owner, name) = split_repo(repo)?;
        let url = format!(
            "{API_BASE}/repos/{}/{}/actions/runs?per_page=100",
            encode_path_segment(owner),
            encode_path_segment(name)
        );
        let parsed: WorkflowRunsResponse = self.request(reqwest::Method::GET, &url)?.json()?;
        let recent_runs: Vec<_> = parsed
            .workflow_runs
            .into_iter()
            .filter(|run| {
                run.status.eq_ignore_ascii_case("in_progress")
                    || run.status.eq_ignore_ascii_case("completed")
            })
            .take(max_runs.clamp(1, 5))
            .collect();

        let mut jobs = Vec::new();
        for run in recent_runs {
            jobs.extend(self.started_self_hosted_job_info_for_run(
                owner,
                name,
                run.id,
                run.workflow_id,
                &run.path,
            )?);
        }
        // Prefer the newest authoritative starts when a matrix-heavy run
        // returns more rows than Kitty can persist in one sample. Sort back
        // into chronological order before handing the bounded batch to Kitty.
        jobs.sort_by_key(|job| {
            (
                std::cmp::Reverse(job.started_at_epoch.unwrap_or(0)),
                std::cmp::Reverse(job.run_id),
                std::cmp::Reverse(job.job_id),
            )
        });
        jobs.truncate(256);
        jobs.sort_by_key(|job| {
            (
                job.started_at_epoch.unwrap_or(u64::MAX),
                job.run_id,
                job.job_id,
            )
        });
        Ok(jobs)
    }

    /// Returns active self-hosted jobs with authoritative runner names.
    /// This is optional telemetry and inspects at most two in-progress workflow
    /// runs and one jobs page per run; incomplete history never changes scheduling.
    pub fn active_self_hosted_jobs_on_runners(&self, repo: &str) -> Result<Vec<ActiveJobOnRunner>> {
        const MAX_RUNS_TO_SCAN: usize = 2;
        const MAX_JOB_PAGES_PER_RUN: usize = 1;
        const MAX_ACTIVE_JOBS: usize = 256;
        let (owner, name) = split_repo(repo)?;
        let runs_url = format!(
            "{API_BASE}/repos/{}/{}/actions/runs?status=in_progress&per_page=100",
            encode_path_segment(owner),
            encode_path_segment(name)
        );
        // Bound run and job-page fan-out so 30-second resource telemetry cannot
        // turn a high-cardinality repository into unbounded API work.
        let parsed: WorkflowRunsResponse = self.request(reqwest::Method::GET, &runs_url)?.json()?;
        let mut results = Vec::new();
        for run in parsed.workflow_runs.into_iter().take(MAX_RUNS_TO_SCAN) {
            let mut url = format!(
                "{API_BASE}/repos/{}/{}/actions/runs/{}/jobs?filter=latest&per_page=100",
                encode_path_segment(owner),
                encode_path_segment(name),
                run.id
            );
            let mut seen_pages = HashSet::new();
            let mut job_pages = 0usize;
            loop {
                if job_pages >= MAX_JOB_PAGES_PER_RUN {
                    break;
                }
                job_pages += 1;
                record_page_url(&mut seen_pages, &url)?;
                let response = self.request(reqwest::Method::GET, &url)?;
                let next_url = parse_next_link(response.headers());
                let jobs: JobsResponse = response.json()?;
                for job in jobs.jobs {
                    if !job.status.eq_ignore_ascii_case("in_progress")
                        || job.conclusion.is_some()
                        || !job_targets_gitrun_runner(&job.labels)
                    {
                        continue;
                    }
                    let Some(runner_name) =
                        job.runner_name.filter(|value| !value.trim().is_empty())
                    else {
                        continue;
                    };
                    let Some(started_at_epoch) = job
                        .started_at
                        .as_deref()
                        .and_then(parse_github_timestamp_epoch)
                    else {
                        // Unknown start times cannot provide a valid observed resource sample.
                        continue;
                    };
                    results.push(ActiveJobOnRunner {
                        runner_name,
                        job: QueuedJobInfo {
                            run_id: run.id,
                            job_id: job.id,
                            workflow_id: run.workflow_id,
                            workflow_path: run.path.clone(),
                            name: job.name,
                            labels: job.labels,
                            started_at_epoch: Some(started_at_epoch),
                        },
                    });
                }
                match next_url {
                    Some(next) => url = next,
                    None => break,
                }
                if results.len() >= MAX_ACTIVE_JOBS {
                    results.truncate(MAX_ACTIVE_JOBS);
                    return Ok(results);
                }
            }
            if results.len() >= MAX_ACTIVE_JOBS {
                results.truncate(MAX_ACTIVE_JOBS);
                break;
            }
        }
        Ok(results)
    }

    fn started_self_hosted_job_info_for_run(
        &self,
        owner: &str,
        name: &str,
        run_id: u64,
        workflow_id: u64,
        workflow_path: &str,
    ) -> Result<Vec<QueuedJobInfo>> {
        let mut jobs = Vec::new();
        let mut url = format!(
            "{API_BASE}/repos/{}/{}/actions/runs/{run_id}/jobs?filter=latest&per_page=100",
            encode_path_segment(owner),
            encode_path_segment(name)
        );
        let mut seen_pages = HashSet::new();

        loop {
            record_page_url(&mut seen_pages, &url)?;
            let response = self.request(reqwest::Method::GET, &url)?;
            let next_url = parse_next_link(response.headers());
            let parsed: JobsResponse = response.json()?;
            jobs.extend(parsed.jobs.into_iter().filter_map(|job| {
                if !job_targets_gitrun_runner(&job.labels) {
                    return None;
                }
                let started_at_epoch = job
                    .started_at
                    .as_deref()
                    .and_then(parse_github_timestamp_epoch)?;
                Some(QueuedJobInfo {
                    run_id,
                    job_id: job.id,
                    workflow_id,
                    workflow_path: workflow_path.to_owned(),
                    name: job.name,
                    labels: job.labels,
                    started_at_epoch: Some(started_at_epoch),
                })
            }));

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
        workflow_id: u64,
        workflow_path: &str,
    ) -> Result<Vec<QueuedJobInfo>> {
        let mut jobs = Vec::new();
        let mut url = format!(
            "{API_BASE}/repos/{}/{}/actions/runs/{run_id}/jobs?filter=latest&per_page=100",
            encode_path_segment(owner),
            encode_path_segment(name)
        );
        let mut seen_pages = HashSet::new();

        loop {
            record_page_url(&mut seen_pages, &url)?;
            let response = self.request(reqwest::Method::GET, &url)?;
            let next_url = parse_next_link(response.headers());
            let parsed: JobsResponse = response.json()?;

            jobs.extend(
                parsed
                    .jobs
                    .into_iter()
                    .filter(|job| job.status == "queued" && job_targets_gitrun_runner(&job.labels))
                    .map(|job| QueuedJobInfo {
                        run_id,
                        job_id: job.id,
                        workflow_id,
                        workflow_path: workflow_path.to_owned(),
                        name: job.name,
                        labels: job.labels,
                        started_at_epoch: job
                            .started_at
                            .as_deref()
                            .and_then(parse_github_timestamp_epoch),
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
        let repository_url = format!(
            "{API_BASE}/repos/{}/{}",
            encode_path_segment(owner),
            encode_path_segment(name)
        );
        // Confirm the repository is accessible before treating a 404 for the
        // workflow directory as "there are no workflows". GitHub also returns
        // 404 for private repositories hidden from this token or missing repos;
        // treating that as an empty directory would silently bypass validation.
        self.request(reqwest::Method::GET, &repository_url)?;

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
        let url = &url_part[1..url_part.len() - 1];
        // Link headers are response data. Never let a response redirect the
        // authenticated client to another origin with its bearer token.
        let parsed = reqwest::Url::parse(url).ok()?;
        if parsed.scheme() != "https"
            || parsed.host_str() != Some("api.github.com")
            || parsed.port().is_some()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return None;
        }
        Some(parsed.to_string())
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
    fn rejects_owner_segments_longer_than_github_limit() {
        assert!(split_repo(&format!("{}/repo", "a".repeat(40))).is_err());
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
    fn repeated_pagination_url_is_rejected() {
        let mut seen = HashSet::new();
        assert!(record_page_url(&mut seen, "https://api.github.com/resource?page=1").is_ok());
        let error =
            record_page_url(&mut seen, "https://api.github.com/resource?page=1").unwrap_err();
        assert!(matches!(error, GitHubError::PaginationLoop(url) if url.ends_with("page=1")));
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
    fn pagination_rejects_non_github_origins() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::LINK,
            "<https://attacker.example/steal?page=2>; rel=next"
                .parse()
                .unwrap(),
        );
        assert_eq!(parse_next_link(&headers), None);
    }

    #[test]
    fn pagination_rejects_insecure_or_nonstandard_github_urls() {
        for link in [
            "<http://api.github.com/resource?page=2>; rel=next", // DevSkim: ignore DS137138 because this HTTP URL is intentionally rejected by this negative test.
            "<https://api.github.com:8443/resource?page=2>; rel=next",
            "<https://user@api.github.com/resource?page=2>; rel=next",
        ] {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::LINK, link.parse().unwrap());
            assert_eq!(parse_next_link(&headers), None, "link: {link}");
        }
    }

    #[test]
    fn force_cancel_rejects_zero_run_id_without_network_access() {
        let client = GitHubClient::new("test-token").unwrap();
        assert!(matches!(
            client.force_cancel_workflow_run("owner/repo", 0),
            Err(GitHubError::InvalidWorkflowRunId)
        ));
    }

    #[test]
    fn custom_workload_route_is_a_runner_request_without_self_hosted_label() {
        assert!(job_targets_gitrun_runner(&[
            "gitrun-job-wk-0123456789abcdef".to_owned()
        ]));
        assert!(job_targets_gitrun_runner(&[
            "self-hosted".to_owned(),
            "linux".to_owned()
        ]));
        assert!(!job_targets_gitrun_runner(&["ubuntu-latest".to_owned()]));
        assert!(job_targets_gitrun_runner(&[
            "GitRun-Job-WK-0123456789ABCDEF".to_owned()
        ]));
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
    fn malformed_rate_limit_reset_epoch_cannot_overflow() {
        assert_eq!(
            rate_limit_reset_delay(i64::MIN, 1_700_000_000),
            Duration::from_secs(1)
        );
        assert_eq!(
            rate_limit_reset_delay(1_700_000_030, 1_700_000_000),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn api_attempts_have_safe_defaults_and_a_hard_upper_bound() {
        assert_eq!(parse_api_attempts(None), DEFAULT_API_ATTEMPTS);
        assert_eq!(
            parse_api_attempts(Some("not-a-number")),
            DEFAULT_API_ATTEMPTS
        );
        assert_eq!(parse_api_attempts(Some("0")), 1);
        assert_eq!(parse_api_attempts(Some(" 4 ")), 4);
        assert_eq!(parse_api_attempts(Some("999999")), MAX_API_ATTEMPTS);
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
