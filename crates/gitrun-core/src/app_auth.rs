//! GitHub App authentication: an alternative to the long-lived Personal
//! Access Token GitRun has used so far.
//!
//! Why this exists: a PAT is a single long-lived credential with broad
//! access, visible in plaintext wherever GitRun stores it. A GitHub App's
//! actual working credential — the *installation token* — is short-lived
//! (GitHub expires it after one hour) and scoped only to the repositories
//! the App was installed on. This module holds only the App's private key
//! (RS256, used to sign short-lived JWTs) and exchanges that for
//! installation tokens on demand, refreshing automatically before expiry.
//!
//! This is additive, not a replacement: `GitHubClient` (in `github.rs`)
//! doesn't care how it got a bearer token, so both `AppAuth` here and the
//! existing raw PAT feed it the same way — see `github.rs`'s
//! `GitHubClient::new`/`with_app_auth`.

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppAuthError {
    #[error("invalid GitHub App private key (expected PEM-encoded RSA key): {0}")]
    InvalidPrivateKey(jsonwebtoken::errors::Error),
    #[error("failed to sign App JWT: {0}")]
    Signing(jsonwebtoken::errors::Error),
    #[error("network error contacting GitHub: {0}")]
    Network(#[from] reqwest::Error),
    #[error("GitHub rejected the installation token request ({status}): {detail}")]
    Rejected { status: u16, detail: String },
    #[error("system clock error: {0}")]
    Clock(#[from] std::time::SystemTimeError),
    #[error("invalid GitHub installation token expiration timestamp: {0}")]
    InvalidTimestamp(String),
}

pub type Result<T> = std::result::Result<T, AppAuthError>;

#[derive(Debug, Serialize)]
struct AppClaims {
    /// Issued-at, backdated by 60s to tolerate clock drift between this host
    /// and GitHub's — a JWT that appears to be issued in the future is
    /// rejected outright, so a little slack here is cheap insurance.
    iat: u64,
    /// Expiration — GitHub caps App JWTs at 10 minutes; this uses a
    /// conservative 9 to leave margin.
    exp: u64,
    /// Issuer: the App's numeric ID as a string, per GitHub's spec.
    iss: String,
}

#[derive(Debug, Deserialize)]
struct InstallationTokenResponse {
    token: String,
    expires_at: String,
}

/// A cached installation token plus when it expires, so `token()` can serve
/// repeated calls cheaply and only round-trip to GitHub when actually
/// necessary.
struct CachedToken {
    token: String,
    expires_at: SystemTime,
}

pub struct AppAuth {
    app_id: String,
    installation_id: String,
    encoding_key: EncodingKey,
    http: reqwest::blocking::Client,
    cached: Mutex<Option<CachedToken>>,
}

impl AppAuth {
    /// `private_key_pem` is the App's private key exactly as downloaded from
    /// GitHub (PEM private-key material).
    pub fn new(
        app_id: impl Into<String>,
        installation_id: impl Into<String>,
        private_key_pem: &str,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self> {
        let encoding_key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
            .map_err(AppAuthError::InvalidPrivateKey)?;
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()?;
        Ok(Self {
            app_id: app_id.into(),
            installation_id: installation_id.into(),
            encoding_key,
            http,
            cached: Mutex::new(None),
        })
    }

    /// Returns a valid installation token, minting a new one if there's no
    /// cached token or the cached one is close enough to expiry that using
    /// it risks a mid-request 401.
    pub fn token(&self) -> Result<String> {
        const REFRESH_MARGIN: Duration = Duration::from_secs(120);

        // Keep the cache lock across token minting. Without this, parallel
        // dashboard/API requests can all observe an empty cache and mint
        // duplicate installation tokens at the same time.
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cached.as_ref() {
            let now = SystemTime::now();
            if entry.expires_at > now + REFRESH_MARGIN {
                return Ok(entry.token.clone());
            }
        }

        let fresh = self.mint_installation_token()?;
        *cached = Some(CachedToken {
            token: fresh.0.clone(),
            expires_at: fresh.1,
        });
        Ok(fresh.0)
    }

    fn app_jwt(&self) -> Result<String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let claims = AppClaims {
            iat: now.saturating_sub(60),
            exp: now + 9 * 60,
            iss: self.app_id.clone(),
        };
        let header = Header::new(Algorithm::RS256);
        jsonwebtoken::encode(&header, &claims, &self.encoding_key).map_err(AppAuthError::Signing)
    }

    fn mint_installation_token(&self) -> Result<(String, SystemTime)> {
        let jwt = self.app_jwt()?;
        let url = format!(
            "https://api.github.com/app/installations/{}/access_tokens",
            self.installation_id
        );
        let response = self
            .http
            .post(&url)
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", format!("Bearer {jwt}"))
            .header("X-GitHub-Api-Version", "2026-03-10")
            .header("User-Agent", concat!("GitRun/", env!("CARGO_PKG_VERSION")))
            .send()?;

        let status = response.status();
        if !status.is_success() {
            let detail = response
                .text()
                .unwrap_or_default()
                .chars()
                .take(500)
                .collect();
            return Err(AppAuthError::Rejected {
                status: status.as_u16(),
                detail,
            });
        }
        let parsed: InstallationTokenResponse = response.json()?;
        if parsed.token.trim().is_empty() {
            return Err(AppAuthError::Rejected {
                status: status.as_u16(),
                detail: "GitHub returned an empty installation token".into(),
            });
        }
        let expires_at = parse_github_timestamp(&parsed.expires_at)
            .ok_or_else(|| AppAuthError::InvalidTimestamp(parsed.expires_at.clone()))?;
        Ok((parsed.token, expires_at))
    }
}

/// Parses GitHub's `expires_at` timestamp format (RFC 3339, e.g.
/// "2026-09-23T12:34:56Z") without pulling in `chrono` for one field —
/// same reasoning as the manual date math already used in
/// `gitrun-scheduler/src/main.rs`'s GTUU scheduler.
fn parse_github_timestamp(raw: &str) -> Option<SystemTime> {
    let raw = raw.strip_suffix('Z')?;
    let (date, time) = raw.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }

    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => unreachable!(),
    };
    if !(1..=days_in_month).contains(&day) {
        return None;
    }

    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: f64 = time_parts.next()?.parse().ok()?;
    if time_parts.next().is_some()
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !second.is_finite()
        || !(0.0..60.0).contains(&second)
    {
        return None;
    }

    // Days-since-epoch from a civil date (inverse of the calculation already
    // used in main.rs's chrono_like_now, same source algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_since_epoch = era * 146097 + doe as i64 - 719468;

    let total_secs = days_since_epoch * 86400 + hour * 3600 + minute * 60 + second as i64;
    Some(UNIX_EPOCH + Duration::from_secs(total_secs.max(0) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_github_timestamp() {
        let parsed = parse_github_timestamp("2026-09-23T12:00:00Z");
        assert!(parsed.is_some());
    }

    #[test]
    fn rejects_malformed_timestamp() {
        assert!(parse_github_timestamp("not-a-timestamp").is_none());
        assert!(parse_github_timestamp("2026-09-23 12:00:00").is_none()); // missing T/Z
        assert!(parse_github_timestamp("2026-02-30T12:00:00Z").is_none());
        assert!(parse_github_timestamp("2026-13-01T12:00:00Z").is_none());
        assert!(parse_github_timestamp("2026-09-23T24:00:00Z").is_none());
        assert!(parse_github_timestamp("2026-09-23T12:60:00Z").is_none());
        assert!(parse_github_timestamp("2026-09-23T12:00:60Z").is_none());
    }

    #[test]
    fn timestamp_ordering_is_preserved() {
        let earlier = parse_github_timestamp("2026-01-01T00:00:00Z").unwrap();
        let later = parse_github_timestamp("2026-06-15T08:30:00Z").unwrap();
        assert!(later > earlier);
    }

    #[test]
    fn rejects_garbage_private_key() {
        let result = AppAuth::new(
            "12345",
            "67890",
            "not a real PEM key",
            Duration::from_secs(5),
            Duration::from_secs(20),
        );
        assert!(matches!(result, Err(AppAuthError::InvalidPrivateKey(_))));
    }
}
