//! Authenticated protocol primitives for the private GSR -> gitrun-exe channel.
//!
//! Transport is deliberately separate from authorization and execution.
//! A transport can carry these envelopes over a Unix socket or another
//! protected local channel, while the executor still verifies the GSR MAC.

#[cfg(not(any(kani, verus)))]
use crate::AuthorizedOperation;
#[cfg(not(any(kani, verus)))]
use hmac::{digest::KeyInit, Hmac, Mac};
#[cfg(not(any(kani, verus)))]
use serde::{Deserialize, Serialize};
#[cfg(not(any(kani, verus)))]
use sha2::Sha256;
#[cfg(not(any(kani, verus)))]
use std::collections::{HashMap, VecDeque};
#[cfg(not(any(kani, verus)))]
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(not(any(kani, verus)))]
use thiserror::Error;

#[cfg(not(any(kani, verus)))]
type HmacSha256 = Hmac<Sha256>;

const NONCE_LEN: usize = 32;
const MAC_LEN: usize = 32;
pub const DEFAULT_MAX_CLOCK_SKEW_SECS: u64 = 30;
const DEFAULT_REPLAY_CACHE_SIZE: usize = 4096;

const MIN_CHANNEL_KEY_LEN: usize = 32;

#[cfg_attr(verus, verus_verify(dual_spec, open))]
#[cfg_attr(verus, verus_spec(returns length >= MIN_CHANNEL_KEY_LEN))]
fn channel_key_length_is_valid(length: usize) -> bool {
    length >= MIN_CHANNEL_KEY_LEN
}

#[cfg_attr(verus, verus_verify(dual_spec, open))]
#[cfg_attr(verus, verus_spec(returns length == NONCE_LEN))]
fn nonce_length_is_valid(length: usize) -> bool {
    length == NONCE_LEN
}

#[cfg_attr(verus, verus_verify(dual_spec, open))]
#[cfg_attr(verus, verus_spec(returns length == MAC_LEN))]
fn mac_length_is_valid(length: usize) -> bool {
    length == MAC_LEN
}

#[cfg_attr(verus, verus_verify(dual_spec, open))]
#[cfg_attr(verus, verus_spec(returns current.abs_diff(timestamp) <= maximum_skew))]
fn timestamp_is_within_skew(current: u64, timestamp: u64, maximum_skew: u64) -> bool {
    current.abs_diff(timestamp) <= maximum_skew
}



#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg(not(any(kani, verus)))]
pub struct ChannelKey(#[serde(with = "serde_bytes_hex")] Vec<u8>);

#[cfg(not(any(kani, verus)))]
impl ChannelKey {
    pub fn generate() -> Result<Self, ChannelAuthError> {
        let mut key = vec![0u8; MIN_CHANNEL_KEY_LEN];
        getrandom::fill(&mut key).map_err(ChannelAuthError::Randomness)?;
        Ok(Self(key))
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, ChannelAuthError> {
        if !channel_key_length_is_valid(bytes.len()) {
            return Err(ChannelAuthError::WeakKey);
        }
        Ok(Self(bytes))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(&self.0)
    }

    fn validate(&self) -> Result<(), ChannelAuthError> {
        if !channel_key_length_is_valid(self.0.len()) {
            return Err(ChannelAuthError::WeakKey);
        }
        Ok(())
    }
}

#[cfg(not(any(kani, verus)))]
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
#[cfg(not(any(kani, verus)))]
pub struct SignedRequest {
    pub request: AuthorizedOperation,
    pub timestamp: u64,
    #[serde(
        serialize_with = "serde_bytes_hex::serialize",
        deserialize_with = "deserialize_nonce"
    )]
    pub nonce: Vec<u8>,
    #[serde(
        serialize_with = "serde_bytes_hex::serialize",
        deserialize_with = "deserialize_mac"
    )]
    pub mac: Vec<u8>,
}

#[cfg(not(any(kani, verus)))]
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
        if !nonce_length_is_valid(self.nonce.len()) {
            return Err(ChannelAuthError::InvalidNonce);
        }

        let current = now()?;
        if !timestamp_is_within_skew(current, self.timestamp, max_clock_skew_secs) {
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
        if !mac_length_is_valid(self.mac.len()) {
            return Err(ChannelAuthError::Authentication);
        }
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
#[cfg(not(any(kani, verus)))]
struct SigningPayload<'a> {
    request: &'a AuthorizedOperation,
    timestamp: u64,
    nonce: &'a [u8],
}

#[cfg(not(any(kani, verus)))]
fn compute_mac(key: &[u8], payload: &[u8]) -> Result<Vec<u8>, ChannelAuthError> {
    if !channel_key_length_is_valid(key.len()) {
        return Err(ChannelAuthError::WeakKey);
    }
    let mut mac =
        <HmacSha256 as KeyInit>::new_from_slice(key).map_err(|_| ChannelAuthError::WeakKey)?;
    mac.update(payload);
    Ok(mac.finalize().into_bytes().to_vec())
}

#[cfg(not(any(kani, verus)))]
fn now() -> Result<u64, ChannelAuthError> {
    unix_timestamp(SystemTime::now())
}

#[cfg(not(any(kani, verus)))]
fn unix_timestamp(time: SystemTime) -> Result<u64, ChannelAuthError> {
    time.duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .map_err(|_| ChannelAuthError::ClockUnavailable)
}

#[derive(Debug)]
#[cfg(not(any(kani, verus)))]
pub struct ReplayGuard {
    expires_at: HashMap<Vec<u8>, u64>,
    order: VecDeque<(Vec<u8>, u64)>,
    max_entries: usize,
}

#[cfg(not(any(kani, verus)))]
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

#[cfg(not(any(kani, verus)))]
impl Default for ReplayGuard {
    fn default() -> Self {
        Self::new(DEFAULT_REPLAY_CACHE_SIZE)
    }
}

#[derive(Debug, Error)]
#[cfg(not(any(kani, verus)))]
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

#[cfg(not(any(kani, verus)))]
fn deserialize_nonce<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    let bytes = hex::decode(value).map_err(serde::de::Error::custom)?;
    if bytes.len() != NONCE_LEN {
        return Err(serde::de::Error::custom("invalid nonce length"));
    }
    Ok(bytes)
}

#[cfg(not(any(kani, verus)))]
fn deserialize_mac<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    let bytes = hex::decode(value).map_err(serde::de::Error::custom)?;
    if bytes.len() != MAC_LEN {
        return Err(serde::de::Error::custom("invalid MAC length"));
    }
    Ok(bytes)
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
    fn deserialization_rejects_wrong_nonce_and_mac_lengths() {
        let key = ChannelKey::generate().unwrap();
        let signed = SignedRequest::sign(request(), &key).unwrap();
        let mut value = serde_json::to_value(&signed).unwrap();

        value["nonce"] = serde_json::json!("00");
        assert!(serde_json::from_value::<SignedRequest>(value).is_err());

        let mut value = serde_json::to_value(&signed).unwrap();
        value["mac"] = serde_json::json!("00");
        assert!(serde_json::from_value::<SignedRequest>(value).is_err());
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

#[cfg(kani)]
mod kani_verification {
    use super::{
        channel_key_length_is_valid, mac_length_is_valid, nonce_length_is_valid,
        timestamp_is_within_skew, MAC_LEN, MIN_CHANNEL_KEY_LEN, NONCE_LEN,
    };

    #[kani::proof]
    fn channel_key_nonce_and_mac_lengths_are_exact() {
        let key_length: usize = kani::any();
        let nonce_length: usize = kani::any();
        let mac_length: usize = kani::any();
        assert_eq!(channel_key_length_is_valid(key_length), key_length >= MIN_CHANNEL_KEY_LEN);
        assert_eq!(nonce_length_is_valid(nonce_length), nonce_length == NONCE_LEN);
        assert_eq!(mac_length_is_valid(mac_length), mac_length == MAC_LEN);
    }

    #[kani::proof]
    fn timestamp_skew_check_is_symmetric_and_overflow_safe() {
        let current: u64 = kani::any();
        let timestamp: u64 = kani::any();
        let maximum_skew: u64 = kani::any();
        assert_eq!(
            timestamp_is_within_skew(current, timestamp, maximum_skew),
            current.abs_diff(timestamp) <= maximum_skew
        );
        assert_eq!(
            timestamp_is_within_skew(current, timestamp, maximum_skew),
            timestamp_is_within_skew(timestamp, current, maximum_skew)
        );
    }
}

#[cfg(verus)]
mod verus_verification {
    use super::{
        channel_key_length_is_valid, mac_length_is_valid, nonce_length_is_valid,
        timestamp_is_within_skew, MAC_LEN, MIN_CHANNEL_KEY_LEN, NONCE_LEN,
    };
    use vstd::prelude::*;

    verus! {
        proof fn channel_key_nonce_and_mac_lengths_are_exact(
            key_length: usize,
            nonce_length: usize,
            mac_length: usize,
        )
            ensures
                channel_key_length_is_valid(key_length) == (key_length >= MIN_CHANNEL_KEY_LEN),
                nonce_length_is_valid(nonce_length) == (nonce_length == NONCE_LEN),
                mac_length_is_valid(mac_length) == (mac_length == MAC_LEN),
        {
        }

        proof fn timestamp_skew_check_is_symmetric_and_overflow_safe(
            current: u64,
            timestamp: u64,
            maximum_skew: u64,
        )
            ensures
                timestamp_is_within_skew(current, timestamp, maximum_skew)
                    == (current.abs_diff(timestamp) <= maximum_skew),
                timestamp_is_within_skew(current, timestamp, maximum_skew)
                    == timestamp_is_within_skew(timestamp, current, maximum_skew),
        {
        }
    }
}

#[cfg(kani)]
fn main() {}

#[cfg(verus)]
fn main() {}
