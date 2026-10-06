//! Authenticated protocol primitives for the private GSR -> gitrun-exe channel.
//!
//! Transport is deliberately separate from authorization and execution.
//! A transport can carry these envelopes over a Unix socket or another
//! protected local channel, while the executor still verifies the GSR MAC.

use crate::AuthorizedOperation;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

const NONCE_LEN: usize = 32;
pub const DEFAULT_MAX_CLOCK_SKEW_SECS: u64 = 30;
const DEFAULT_REPLAY_CACHE_SIZE: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelKey(#[serde(with = "serde_bytes_hex")] pub Vec<u8>);

impl ChannelKey {
    pub fn generate() -> Result<Self, ChannelAuthError> {
        let mut key = vec![0u8; 32];
        getrandom::fill(&mut key).map_err(ChannelAuthError::Randomness)?;
        Ok(Self(key))
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, ChannelAuthError> {
        if bytes.len() < 32 {
            return Err(ChannelAuthError::WeakKey);
        }
        Ok(Self(bytes))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(&self.0)
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
        let mut nonce = vec![0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(ChannelAuthError::Randomness)?;
        let timestamp = now();
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
        if self.nonce.len() != NONCE_LEN {
            return Err(ChannelAuthError::InvalidNonce);
        }

        let current = now();
        if current.abs_diff(self.timestamp) > max_clock_skew_secs {
            return Err(ChannelAuthError::Expired);
        }

        let payload = SigningPayload {
            request: &self.request,
            timestamp: self.timestamp,
            nonce: &self.nonce,
        };
        let bytes = serde_json::to_vec(&payload).map_err(ChannelAuthError::Encode)?;

        let mut mac =
            HmacSha256::new_from_slice(&key.0).map_err(|_| ChannelAuthError::WeakKey)?;
        mac.update(&bytes);
        mac.verify_slice(&self.mac)
            .map_err(|_| ChannelAuthError::Authentication)?;

        if !replay.check_and_record(&self.nonce) {
            return Err(ChannelAuthError::Replay);
        }

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
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| ChannelAuthError::WeakKey)?;
    mac.update(payload);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

#[derive(Debug)]
pub struct ReplayGuard {
    seen: HashSet<Vec<u8>>,
    order: VecDeque<Vec<u8>>,
    max_entries: usize,
}

impl ReplayGuard {
    pub fn new(max_entries: usize) -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            max_entries: max_entries.max(1),
        }
    }

    pub fn check_and_record(&mut self, nonce: &[u8]) -> bool {
        if self.seen.contains(nonce) {
            return false;
        }

        let value = nonce.to_vec();
        self.seen.insert(value.clone());
        self.order.push_back(value);

        while self.order.len() > self.max_entries {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }

        true
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
    #[error("replayed request nonce")]
    Replay,
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
