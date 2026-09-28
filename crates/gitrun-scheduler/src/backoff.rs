//! Centralized rate-limit backoff, shared across every GitHub API call made
//! during a reconcile cycle.
//!
//! Without this, `github.rs`'s `RateLimited` error is just returned to
//! whoever called it — in practice, `reconcile_repo` in `main.rs`, which
//! logs it and lets the whole cycle fail, then tries again at the next poll
//! tick (as little as 5 seconds later by default). Under real rate
//! limiting that's often *shorter* than the time GitHub asked us to wait,
//! so the same repo can keep re-triggering `RateLimited` on every tick,
//! looking like a retry storm even though each individual call handled the
//! error "correctly" in isolation.
//!
//! This module tracks, per repository, whether we're currently in a
//! rate-limit cooldown and until when — checked once before a repo's cycle
//! even *attempts* any GitHub call, so a limited repo is skipped outright
//! instead of generating another doomed request.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimitTracker {
    cooldowns: Mutex<HashMap<String, Instant>>,
}

impl Default for RateLimitTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimitTracker {
    pub fn new() -> Self {
        Self {
            cooldowns: Mutex::new(HashMap::new()),
        }
    }

    /// Records that `repo` hit a rate limit and should not be contacted
    /// again until `retry_after` has elapsed. A missing `retry_after` (some
    /// rate-limit responses don't include one) falls back to a conservative
    /// default rather than not backing off at all.
    pub fn record_rate_limited(&self, repo: &str, retry_after: Option<Duration>) {
        const FALLBACK_COOLDOWN: Duration = Duration::from_secs(60);
        let until = Instant::now() + retry_after.unwrap_or(FALLBACK_COOLDOWN);
        let mut cooldowns = self.cooldowns.lock().unwrap_or_else(|e| e.into_inner());
        // Never shorten an existing cooldown — if two calls for the same
        // repo both hit rate limits close together with different
        // Retry-After values, keep whichever pushes furthest out.
        let entry = cooldowns.entry(repo.to_owned()).or_insert(until);
        if until > *entry {
            *entry = until;
        }
    }

    /// Returns how much longer `repo` should be skipped, or `None` if it's
    /// clear to proceed. Also clears the entry once it's expired, so the
    /// map doesn't grow forever with stale repo names.
    pub fn remaining_cooldown(&self, repo: &str) -> Option<Duration> {
        let mut cooldowns = self.cooldowns.lock().unwrap_or_else(|e| e.into_inner());
        match cooldowns.get(repo) {
            Some(&until) if until > Instant::now() => Some(until - Instant::now()),
            Some(_) => {
                cooldowns.remove(repo);
                None
            }
            None => None,
        }
    }

    pub fn clear(&self, repo: &str) {
        let mut cooldowns = self.cooldowns.lock().unwrap_or_else(|e| e.into_inner());
        cooldowns.remove(repo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_cooldown_by_default() {
        let tracker = RateLimitTracker::new();
        assert!(tracker.remaining_cooldown("owner/repo").is_none());
    }

    #[test]
    fn records_and_reports_active_cooldown() {
        let tracker = RateLimitTracker::new();
        tracker.record_rate_limited("owner/repo", Some(Duration::from_secs(30)));
        let remaining = tracker.remaining_cooldown("owner/repo");
        assert!(remaining.is_some());
        assert!(remaining.unwrap() <= Duration::from_secs(30));
    }

    #[test]
    fn missing_retry_after_uses_fallback_cooldown() {
        let tracker = RateLimitTracker::new();
        tracker.record_rate_limited("owner/repo", None);
        assert!(tracker.remaining_cooldown("owner/repo").is_some());
    }

    #[test]
    fn cooldown_does_not_affect_other_repos() {
        let tracker = RateLimitTracker::new();
        tracker.record_rate_limited("owner/repo-a", Some(Duration::from_secs(30)));
        assert!(tracker.remaining_cooldown("owner/repo-b").is_none());
    }

    #[test]
    fn repeated_rate_limit_extends_but_never_shortens_cooldown() {
        let tracker = RateLimitTracker::new();
        tracker.record_rate_limited("owner/repo", Some(Duration::from_secs(60)));
        let longer = tracker.remaining_cooldown("owner/repo").unwrap();

        // A second, shorter Retry-After should not shorten the existing wait.
        tracker.record_rate_limited("owner/repo", Some(Duration::from_secs(5)));
        let after_shorter = tracker.remaining_cooldown("owner/repo").unwrap();
        assert!(after_shorter <= longer + Duration::from_millis(50)); // allow tiny test timing drift
        assert!(after_shorter > Duration::from_secs(5));
    }

    #[test]
    fn clear_removes_cooldown_immediately() {
        let tracker = RateLimitTracker::new();
        tracker.record_rate_limited("owner/repo", Some(Duration::from_secs(60)));
        tracker.clear("owner/repo");
        assert!(tracker.remaining_cooldown("owner/repo").is_none());
    }
}
