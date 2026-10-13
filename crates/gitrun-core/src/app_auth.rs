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
    #[error("invalid GitHub App {0}: expected a positive decimal integer ID")]
    InvalidIdentifier(&'static str),
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
        let app_id = validate_github_id(app_id.into(), "ID")?;
        let installation_id = validate_github_id(installation_id.into(), "installation ID")?;
        let encoding_key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
            .map_err(AppAuthError::InvalidPrivateKey)?;
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()?;
        Ok(Self {
            app_id,
            installation_id,
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
fn validate_github_id(value: String, field: &'static str) -> Result<String> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || !value.parse::<u64>().is_ok_and(|id| id > 0)
    {
        return Err(AppAuthError::InvalidIdentifier(field));
    }
    Ok(value)
}

fn parse_github_timestamp(raw: &str) -> Option<SystemTime> {
    let bytes = raw.as_bytes();
    if bytes.len() < 20
        || !raw.ends_with('Z')
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || !bytes[0..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..10].iter().all(u8::is_ascii_digit)
        || !bytes[11..13].iter().all(u8::is_ascii_digit)
        || !bytes[14..16].iter().all(u8::is_ascii_digit)
        || !bytes[17..19].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    // RFC 3339 permits fractional seconds. GitHub's expiration timestamps
    // normally use whole seconds, but accept a non-empty decimal fraction.
    if bytes.len() > 20
        && (bytes[19] != b'.'
            || bytes[20..bytes.len() - 1].is_empty()
            || !bytes[20..bytes.len() - 1]
                .iter()
                .all(u8::is_ascii_digit))
    {
        return None;
    }

    let year = raw[0..4].parse::<i64>().ok()?;
    let month = raw[5..7].parse::<i64>().ok()?;
    let day = raw[8..10].parse::<i64>().ok()?;
    let hour = raw[11..13].parse::<i64>().ok()?;
    let minute = raw[14..16].parse::<i64>().ok()?;
    let second = raw[17..19].parse::<i64>().ok()?;

    if !(1..=12).contains(&month)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return None;
    }

    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=days_in_month).contains(&day) {
        return None;
    }

    // Days since epoch using checked arithmetic so malformed input can never
    // overflow and panic the caller.
    let y = if month <= 2 { year.checked_sub(1)? } else { year };
    let adjusted_year = if y >= 0 { y } else { y.checked_sub(399)? };
    let era = adjusted_year.checked_div(400)?;
    let yoe = y.checked_sub(era.checked_mul(400)?)?;
    let mp = if month > 2 { month.checked_sub(3)? } else { month.checked_add(9)? };
    let doy = 153_i64
        .checked_mul(mp)?
        .checked_add(2)?
        .checked_div(5)?
        .checked_add(day)?
        .checked_sub(1)?;
    let doe = yoe
        .checked_mul(365)?
        .checked_add(yoe / 4)?
        .checked_sub(yoe / 100)?
        .checked_add(doy)?;
    let days_since_epoch = era
        .checked_mul(146097)?
        .checked_add(doe)?
        .checked_sub(719468)?;
    let total_secs = days_since_epoch
        .checked_mul(86400)?
        .checked_add(hour.checked_mul(3600)?)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)?;
    let delta = Duration::from_secs(total_secs.unsigned_abs());

    if total_secs >= 0 {
        UNIX_EPOCH.checked_add(delta)
    } else {
        UNIX_EPOCH.checked_sub(delta)
    }
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
    fn timestamp_parser_rejects_overflow_and_noncanonical_fields() {
        assert!(parse_github_timestamp("999999999999-09-23T12:00:00Z").is_none());
        assert!(parse_github_timestamp("2026-9-23T12:00:00Z").is_none());
        assert!(parse_github_timestamp("2026-09-23T2:00:00Z").is_none());
        assert!(parse_github_timestamp("2026-09-23T12:00:00.Z").is_none());
        assert!(parse_github_timestamp("2026-09-23T12:00:00.123Z").is_some());
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
    fn app_auth_rejects_invalid_numeric_identifiers_before_key_parsing() {
        let timeout = Duration::from_secs(1);
        assert!(matches!(
            AppAuth::new("abc", "67890", "not a real PEM key", timeout, timeout),
            Err(AppAuthError::InvalidIdentifier("ID"))
        ));
        assert!(matches!(
            AppAuth::new("12345", "12/../../evil", "not a real PEM key", timeout, timeout),
            Err(AppAuthError::InvalidIdentifier("installation ID"))
        ));
        assert!(matches!(
            AppAuth::new("0", "67890", "not a real PEM key", timeout, timeout),
            Err(AppAuthError::InvalidIdentifier("ID"))
        ));
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
