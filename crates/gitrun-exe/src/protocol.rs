//! Authenticated protocol primitives for the private GSR -> gitrun-exe channel.
//!
//! Transport is deliberately separate from authorization and execution.
//! A transport can carry these envelopes over a Unix socket or another
//! protected local channel, while the executor still verifies the GSR MAC.

use crate::AuthorizedOperation;
use hmac::{digest::KeyInit, Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

const NONCE_LEN: usize = 32;
pub const DEFAULT_MAX_CLOCK_SKEW_SECS: u64 = 30;
const DEFAULT_REPLAY_CACHE_SIZE: usize = 4096;

const MIN_CHANNEL_KEY_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChannelKey(#[serde(with = "serde_bytes_hex")] Vec<u8>);

impl ChannelKey {
    pub fn generate() -> Result<Self, ChannelAuthError> {
        let mut key = vec![0u8; MIN_CHANNEL_KEY_LEN];
        getrandom::fill(&mut key).map_err(ChannelAuthError::Randomness)?;
        Ok(Self(key))
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, ChannelAuthError> {
        if bytes.len() < MIN_CHANNEL_KEY_LEN {
            return Err(ChannelAuthError::WeakKey);
        }
        Ok(Self(bytes))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(&self.0)
    }

    fn validate(&self) -> Result<(), ChannelAuthError> {
        if self.0.len() < MIN_CHANNEL_KEY_LEN {
            return Err(ChannelAuthError::WeakKey);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ChannelKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bytes = serde_bytes_hex::deserialize(deserializer)?;
        Self::from_bytes(bytes).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedRequest {
    pub request: AuthorizedOperation,
    pub timestamp: u64,
    #[serde(with = "serde_bytes_hex")]
    pub nonce: Vec<u8>,
    #[serde(with = "serde_bytes_hex")]
    pub mac: Vec<u8>,
}

impl SignedRequest {
    pub fn sign(request: AuthorizedOperation, key: &ChannelKey) -> Result<Self, ChannelAuthError> {
        key.validate()?;
        let mut nonce = vec![0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(ChannelAuthError::Randomness)?;
        let timestamp = now()?;
        let payload = SigningPayload {
            request: &request,
            timestamp,
            nonce: &nonce,
        };
        let bytes = serde_json::to_vec(&payload).map_err(ChannelAuthError::Encode)?;
        let mac = compute_mac(&key.0, &bytes)?;

        Ok(Self {
            request,
            timestamp,
            nonce,
            mac,
        })
    }

    pub fn verify(
        &self,
        key: &ChannelKey,
        max_clock_skew_secs: u64,
        replay: &mut ReplayGuard,
    ) -> Result<(), ChannelAuthError> {
        key.validate()?;
        if self.nonce.len() != NONCE_LEN {
            return Err(ChannelAuthError::InvalidNonce);
        }

        let current = now()?;
        if current.abs_diff(self.timestamp) > max_clock_skew_secs {
            return Err(ChannelAuthError::Expired);
        }

        let payload = SigningPayload {
            request: &self.request,
            timestamp: self.timestamp,
            nonce: &self.nonce,
        };
        let bytes = serde_json::to_vec(&payload).map_err(ChannelAuthError::Encode)?;

        let mut mac = <HmacSha256 as KeyInit>::new_from_slice(&key.0)
            .map_err(|_| ChannelAuthError::WeakKey)?;
        mac.update(&bytes);
        mac.verify_slice(&self.mac)
            .map_err(|_| ChannelAuthError::Authentication)?;

        replay.check_and_record(
            &self.nonce,
            self.timestamp.saturating_add(max_clock_skew_secs),
            current,
        )?;

        Ok(())
    }
}

#[derive(Serialize)]
struct SigningPayload<'a> {
    request: &'a AuthorizedOperation,
    timestamp: u64,
    nonce: &'a [u8],
}

fn compute_mac(key: &[u8], payload: &[u8]) -> Result<Vec<u8>, ChannelAuthError> {
    if key.len() < MIN_CHANNEL_KEY_LEN {
        return Err(ChannelAuthError::WeakKey);
    }
    let mut mac =
        <HmacSha256 as KeyInit>::new_from_slice(key).map_err(|_| ChannelAuthError::WeakKey)?;
    mac.update(payload);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn now() -> Result<u64, ChannelAuthError> {
    unix_timestamp(SystemTime::now())
}

fn unix_timestamp(time: SystemTime) -> Result<u64, ChannelAuthError> {
    time.duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .map_err(|_| ChannelAuthError::ClockUnavailable)
}

#[derive(Debug)]
pub struct ReplayGuard {
    expires_at: HashMap<Vec<u8>, u64>,
    order: VecDeque<(Vec<u8>, u64)>,
    max_entries: usize,
}

impl ReplayGuard {
    pub fn new(max_entries: usize) -> Self {
        Self {
            expires_at: HashMap::new(),
            order: VecDeque::new(),
            max_entries: max_entries.max(1),
        }
    }

    /// Records a nonce until its signed request can no longer pass timestamp
    /// validation. Expired entries are pruned, but live entries are never
    /// evicted to make room: a full cache fails closed.
    pub fn check_and_record(
        &mut self,
        nonce: &[u8],
        valid_until: u64,
        current: u64,
    ) -> Result<(), ChannelAuthError> {
        let expired: Vec<(Vec<u8>, u64)> = self
            .order
            .iter()
            .filter(|entry| entry.1 < current)
            .cloned()
            .collect();
        if !expired.is_empty() {
            self.order.retain(|(_, expiry)| *expiry >= current);
            for (old_nonce, old_expiry) in expired {
                if self.expires_at.get(&old_nonce) == Some(&old_expiry) {
                    self.expires_at.remove(&old_nonce);
                }
            }
        }

        if self.expires_at.contains_key(nonce) {
            return Err(ChannelAuthError::Replay);
        }
        if self.expires_at.len() >= self.max_entries {
            return Err(ChannelAuthError::ReplayCacheFull);
        }

        let value = nonce.to_vec();
        self.expires_at.insert(value.clone(), valid_until);
        self.order.push_back((value, valid_until));
        Ok(())
    }
}

impl Default for ReplayGuard {
    fn default() -> Self {
        Self::new(DEFAULT_REPLAY_CACHE_SIZE)
    }
}

#[derive(Debug, Error)]
pub enum ChannelAuthError {
    #[error("secure randomness unavailable: {0}")]
    Randomness(#[source] getrandom::Error),
    #[error("invalid channel key")]
    WeakKey,
    #[error("unable to encode authenticated payload: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("invalid nonce")]
    InvalidNonce,
    #[error("request timestamp is outside the allowed clock skew")]
    Expired,
    #[error("system clock is before the Unix epoch")]
    ClockUnavailable,
    #[error("replayed request nonce")]
    Replay,
    #[error("replay cache is full of requests that are still valid")]
    ReplayCacheFull,
    #[error("GSR authentication failed")]
    Authentication,
}

mod serde_bytes_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&hex::encode(value))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        hex::decode(value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitrun_core::{GitRunApi, GitRunOperation};
    use std::collections::BTreeMap;

    fn request() -> AuthorizedOperation {
        AuthorizedOperation {
            request_id: "request-1".into(),
            api: GitRunApi::GitStatusRun,
            operation: GitRunOperation::Status,
            repository: "owner/repo".into(),
            workflow: "ci.yml".into(),
            run_id: Some(123),
            job: "build".into(),
            resource: None,
            arguments: BTreeMap::new(),
        }
    }

    #[test]
    fn weak_channel_keys_are_rejected_by_all_entry_points() {
        let weak = ChannelKey(vec![1]);
        assert!(matches!(
            SignedRequest::sign(request(), &weak),
            Err(ChannelAuthError::WeakKey)
        ));

        let valid_key = ChannelKey::generate().unwrap();
        let signed = SignedRequest::sign(request(), &valid_key).unwrap();
        let mut replay = ReplayGuard::default();
        assert!(matches!(
            signed.verify(&weak, DEFAULT_MAX_CLOCK_SKEW_SECS, &mut replay),
            Err(ChannelAuthError::WeakKey)
        ));

        let encoded = serde_json::json!(hex::encode([1u8, 2, 3]));
        let decoded = serde_json::from_value::<ChannelKey>(encoded);
        assert!(
            decoded.is_err(),
            "deserialization must enforce minimum key length"
        );
    }

    fn fresh_test_nonce() -> Vec<u8> {
        SignedRequest::sign(
            request(),
            &ChannelKey::generate().expect("test key generation requires OS randomness"),
        )
        .expect("test request signing requires OS randomness")
        .nonce
    }

    #[test]
    fn replay_cache_does_not_evict_live_nonces_when_full() {
        // Use production-generated random nonces so the test does not seed the
        // replay cache with fixed cryptographic values.
        let first_nonce = fresh_test_nonce();
        let second_nonce = fresh_test_nonce();
        assert_ne!(first_nonce, second_nonce);

        let mut replay = ReplayGuard::new(1);
        replay.check_and_record(&first_nonce, 200, 100).unwrap();
        assert!(matches!(
            replay.check_and_record(&second_nonce, 200, 100),
            Err(ChannelAuthError::ReplayCacheFull)
        ));
        assert!(matches!(
            replay.check_and_record(&first_nonce, 200, 100),
            Err(ChannelAuthError::Replay)
        ));
        replay.check_and_record(&second_nonce, 300, 201).unwrap();
    }

    #[test]
    fn clock_before_unix_epoch_fails_closed() {
        let before_epoch = UNIX_EPOCH - std::time::Duration::from_secs(1);
        assert!(matches!(
            unix_timestamp(before_epoch),
            Err(ChannelAuthError::ClockUnavailable)
        ));
    }

    #[test]
    fn signed_request_verifies() {
        let key = ChannelKey::generate().unwrap();
        let signed = SignedRequest::sign(request(), &key).unwrap();
        let mut replay = ReplayGuard::default();
        assert!(signed
            .verify(&key, DEFAULT_MAX_CLOCK_SKEW_SECS, &mut replay)
            .is_ok());
    }

    #[test]
    fn tampering_fails_authentication() {
        let key = ChannelKey::generate().unwrap();
        let mut signed = SignedRequest::sign(request(), &key).unwrap();
        signed.request.job = "attacker".into();
        let mut replay = ReplayGuard::default();
        assert!(matches!(
            signed.verify(&key, DEFAULT_MAX_CLOCK_SKEW_SECS, &mut replay),
            Err(ChannelAuthError::Authentication)
        ));
    }

    #[test]
    fn replay_is_rejected() {
        let key = ChannelKey::generate().unwrap();
        let signed = SignedRequest::sign(request(), &key).unwrap();
        let mut replay = ReplayGuard::default();

        assert!(signed
            .verify(&key, DEFAULT_MAX_CLOCK_SKEW_SECS, &mut replay)
            .is_ok());
        assert!(matches!(
            signed.verify(&key, DEFAULT_MAX_CLOCK_SKEW_SECS, &mut replay),
            Err(ChannelAuthError::Replay)
        ));
    }

    #[test]
    fn malformed_nonce_is_rejected_without_consuming_the_valid_nonce() {
        let key = ChannelKey::generate().unwrap();
        let signed = SignedRequest::sign(request(), &key).unwrap();
        let mut malformed = signed.clone();
        malformed.nonce.pop();

        let mut replay = ReplayGuard::default();
        assert!(matches!(
            malformed.verify(&key, DEFAULT_MAX_CLOCK_SKEW_SECS, &mut replay),
            Err(ChannelAuthError::InvalidNonce)
        ));

        // The invalid envelope must fail before the nonce enters the replay cache.
        assert!(signed
            .verify(&key, DEFAULT_MAX_CLOCK_SKEW_SECS, &mut replay)
            .is_ok());
    }

    #[test]
    fn expired_past_timestamp_is_rejected() {
        let key = ChannelKey::generate().unwrap();
        let mut signed = SignedRequest::sign(request(), &key).unwrap();
        signed.timestamp = now()
            .unwrap()
            .saturating_sub(DEFAULT_MAX_CLOCK_SKEW_SECS + 1);

        let mut replay = ReplayGuard::default();
        assert!(matches!(
            signed.verify(&key, DEFAULT_MAX_CLOCK_SKEW_SECS, &mut replay),
            Err(ChannelAuthError::Expired)
        ));
    }

    #[test]
    fn expired_future_timestamp_is_rejected() {
        let key = ChannelKey::generate().unwrap();
        let mut signed = SignedRequest::sign(request(), &key).unwrap();
        signed.timestamp = now()
            .unwrap()
            .saturating_add(DEFAULT_MAX_CLOCK_SKEW_SECS + 1);

        let mut replay = ReplayGuard::default();
        assert!(matches!(
            signed.verify(&key, DEFAULT_MAX_CLOCK_SKEW_SECS, &mut replay),
            Err(ChannelAuthError::Expired)
        ));
    }

    #[test]
    fn timestamp_mutation_inside_clock_window_fails_mac_verification() {
        let key = ChannelKey::generate().unwrap();
        let mut signed = SignedRequest::sign(request(), &key).unwrap();
        signed.timestamp = signed.timestamp.saturating_add(1);

        let mut replay = ReplayGuard::default();
        assert!(matches!(
            signed.verify(&key, DEFAULT_MAX_CLOCK_SKEW_SECS, &mut replay),
            Err(ChannelAuthError::Authentication)
        ));
    }

    #[test]
    fn invalid_signed_request_does_not_consume_nonce() {
        let key = ChannelKey::generate().unwrap();
        let mut signed = SignedRequest::sign(request(), &key).unwrap();
        signed.mac[0] ^= 0xff;

        let mut replay = ReplayGuard::default();
        assert!(matches!(
            signed.verify(&key, DEFAULT_MAX_CLOCK_SKEW_SECS, &mut replay),
            Err(ChannelAuthError::Authentication)
        ));

        let fresh = SignedRequest::sign(request(), &key).unwrap();
        assert!(fresh
            .verify(&key, DEFAULT_MAX_CLOCK_SKEW_SECS, &mut replay)
            .is_ok());
    }
}
