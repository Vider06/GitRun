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

#[cfg(not(verus))]
use std::collections::HashMap;
#[cfg(not(verus))]
use std::sync::Mutex;
#[cfg(not(verus))]
use std::time::{Duration, Instant};
#[cfg(verus)]
use vstd::prelude::*;

#[cfg(not(verus))]
pub struct RateLimitTracker {
    cooldowns: Mutex<HashMap<String, Instant>>,
}

/// Do not let an untrusted or malformed Retry-After value suspend a repo
/// for an effectively unbounded period. GitHub's normal reset windows are
/// far shorter than this defensive ceiling.
const MAX_COOLDOWN_SECONDS: u64 = 24 * 60 * 60;
#[cfg(not(verus))]
const MAX_COOLDOWN: Duration = Duration::from_secs(MAX_COOLDOWN_SECONDS);

/// Clamp an untrusted Retry-After value to the repository cooldown ceiling.
#[cfg_attr(verus, verus_verify(dual_spec, open))]
#[cfg_attr(
    verus,
    verus_spec(returns cap_cooldown_seconds(seconds))
)]
pub(crate) fn cap_cooldown_seconds(seconds: u64) -> u64 {
    seconds.min(MAX_COOLDOWN_SECONDS)
}

#[cfg(not(verus))]
impl Default for RateLimitTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(verus))]
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
        let now = Instant::now();
        // Retry-After is parsed from an HTTP response and can contain any u64
        // number of seconds. Adding an unbounded Duration to Instant can panic
        // on overflow; cap it before constructing the deadline.
        let requested_cooldown = retry_after.unwrap_or(FALLBACK_COOLDOWN);
        let cooldown = if requested_cooldown > MAX_COOLDOWN {
            MAX_COOLDOWN
        } else {
            Duration::from_secs(cap_cooldown_seconds(requested_cooldown.as_secs()))
                + Duration::from_nanos(u64::from(requested_cooldown.subsec_nanos()))
        };
        let until = now + cooldown;
        let mut cooldowns = self.cooldowns.lock().unwrap_or_else(|e| e.into_inner());

        // Drop expired entries opportunistically before adding the new one,
        // keeping this bounded even when a repo is rate-limited only once and
        // never checked again afterward.
        cooldowns.retain(|_, deadline| *deadline > now);

        // Never shorten an existing cooldown — if two calls for the same
        // repo both hit rate limits close together with different
        // Retry-After values, keep whichever pushes furthest out.
        let entry = cooldowns.entry(repo.to_owned()).or_insert(until);
        if until > *entry {
            *entry = until;
        }
    }

    /// Returns how much longer `repo` should be skipped, or `None` if it's
    /// clear to proceed. Also clears the entry once it's expired.
    pub fn remaining_cooldown(&self, repo: &str) -> Option<Duration> {
        let now = Instant::now();
        let mut cooldowns = self.cooldowns.lock().unwrap_or_else(|e| e.into_inner());
        match cooldowns.get(repo).copied() {
            Some(until) if until > now => Some(until - now),
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

#[cfg(all(test, not(verus)))]
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
    fn enormous_retry_after_is_capped_without_panicking() {
        let tracker = RateLimitTracker::new();
        tracker.record_rate_limited("owner/repo", Some(Duration::from_secs(u64::MAX)));
        let remaining = tracker.remaining_cooldown("owner/repo").unwrap();
        assert!(remaining <= MAX_COOLDOWN);
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
        let before = tracker.cooldowns.lock().unwrap().get("owner/repo").copied();

        // A second, shorter Retry-After must not replace the original deadline.
        tracker.record_rate_limited("owner/repo", Some(Duration::from_secs(5)));
        let after = tracker.cooldowns.lock().unwrap().get("owner/repo").copied();

        assert_eq!(after, before);
    }

    #[test]
    fn expired_entries_are_pruned_when_recording_new_cooldown() {
        let tracker = RateLimitTracker::new();
        tracker.record_rate_limited("owner/old", Some(Duration::ZERO));
        tracker.record_rate_limited("owner/new", Some(Duration::from_secs(30)));

        let cooldowns = tracker.cooldowns.lock().unwrap();
        assert!(!cooldowns.contains_key("owner/old"));
        assert!(cooldowns.contains_key("owner/new"));
    }

    #[test]
    fn clear_removes_cooldown_immediately() {
        let tracker = RateLimitTracker::new();
        tracker.record_rate_limited("owner/repo", Some(Duration::from_secs(60)));
        tracker.clear("owner/repo");
        assert!(tracker.remaining_cooldown("owner/repo").is_none());
    }
}

#[cfg(kani)]
mod kani_verification {
    use super::{cap_cooldown_seconds, MAX_COOLDOWN_SECONDS};

    /// Proves the production Retry-After cap for every u64 input.
    #[kani::proof]
    fn cooldown_seconds_are_bounded() {
        let seconds: u64 = kani::any();
        let capped = cap_cooldown_seconds(seconds);

        assert!(capped <= MAX_COOLDOWN_SECONDS);
        assert!(capped <= seconds);
        if seconds <= MAX_COOLDOWN_SECONDS {
            assert_eq!(capped, seconds);
        } else {
            assert_eq!(capped, MAX_COOLDOWN_SECONDS);
        }
    }
}

#[cfg(verus)]
mod verus_verification {
    use super::{cap_cooldown_seconds, MAX_COOLDOWN_SECONDS};
    use vstd::prelude::*;

    verus! {
        /// Proves the production clamp's upper bound and monotonic lower bound.
        proof fn cooldown_seconds_are_bounded(seconds: u64)
            ensures
                cap_cooldown_seconds(seconds) <= MAX_COOLDOWN_SECONDS,
                cap_cooldown_seconds(seconds) <= seconds,
                seconds <= MAX_COOLDOWN_SECONDS ==> cap_cooldown_seconds(seconds) == seconds,
                seconds > MAX_COOLDOWN_SECONDS
                    ==> cap_cooldown_seconds(seconds) == MAX_COOLDOWN_SECONDS,
        {
        }
    }
}

#[cfg(kani)]
fn main() {}

#[cfg(verus)]
fn main() {}
