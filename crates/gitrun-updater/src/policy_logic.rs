//! Pure updater release-policy types shared by runtime code and formal proofs.

#[cfg(verus)]
use vstd::prelude::{verus_spec, verus_verify};

#[cfg(verus)]
use vstd::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(verus, verus_verify)]
#[cfg_attr(
    all(not(kani), not(verus)),
    derive(serde::Serialize, serde::Deserialize)
)]
#[cfg_attr(all(not(kani), not(verus)), serde(rename_all = "snake_case"))]
pub enum ReleaseType {
    Pre,
    Full,
    Security,
}

impl ReleaseType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pre => "pre",
            Self::Full => "full",
            Self::Security => "security",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(verus, verus_verify)]
#[cfg_attr(
    all(not(kani), not(verus)),
    derive(serde::Serialize, serde::Deserialize)
)]
#[cfg_attr(all(not(kani), not(verus)), serde(rename_all = "snake_case"))]
pub enum UpdatePolicy {
    SecurityOnly,
    SecurityFull,
    SecurityFullPre,
}

impl UpdatePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SecurityOnly => "security_only",
            Self::SecurityFull => "security_full",
            Self::SecurityFullPre => "security_full_pre",
        }
    }

    #[cfg_attr(verus, verus_verify(dual_spec, open))]
    #[cfg_attr(
        verus,
        verus_spec(returns self.accepts_release_type(release_type))
    )]
    pub fn accepts_release_type(self, release_type: ReleaseType) -> bool {
        match self {
            Self::SecurityOnly => match release_type {
                ReleaseType::Security => true,
                ReleaseType::Pre | ReleaseType::Full => false,
            },
            Self::SecurityFull => match release_type {
                ReleaseType::Security | ReleaseType::Full => true,
                ReleaseType::Pre => false,
            },
            Self::SecurityFullPre => true,
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// Exhaustively proves the updater policy's release-type allowlist.
    ///
    /// Both enums are finite, so enumerate their complete domains explicitly.
    /// This avoids relying on a custom Arbitrary implementation for either enum.
    #[kani::proof]
    fn update_policy_acceptance_is_exact() {
        assert!(!UpdatePolicy::SecurityOnly.accepts_release_type(ReleaseType::Pre));
        assert!(!UpdatePolicy::SecurityOnly.accepts_release_type(ReleaseType::Full));
        assert!(UpdatePolicy::SecurityOnly.accepts_release_type(ReleaseType::Security));

        assert!(!UpdatePolicy::SecurityFull.accepts_release_type(ReleaseType::Pre));
        assert!(UpdatePolicy::SecurityFull.accepts_release_type(ReleaseType::Full));
        assert!(UpdatePolicy::SecurityFull.accepts_release_type(ReleaseType::Security));

        assert!(UpdatePolicy::SecurityFullPre.accepts_release_type(ReleaseType::Pre));
        assert!(UpdatePolicy::SecurityFullPre.accepts_release_type(ReleaseType::Full));
        assert!(UpdatePolicy::SecurityFullPre.accepts_release_type(ReleaseType::Security));
    }

    // Avoid str equality in CBMC: its lowering calls an unbounded memcmp loop.
    // Compare the finite wire representation byte-by-byte instead.
    fn wire_value_matches(value: &str, expected: &[u8]) -> bool {
        let actual = value.as_bytes();
        if actual.len() != expected.len() {
            return false;
        }

        let mut index = 0;
        while index < expected.len() {
            if actual[index] != expected[index] {
                return false;
            }
            index += 1;
        }
        true
    }

    /// Proves that every representable release type has a stable wire value.
    #[kani::proof]
    #[kani::unwind(18)]
    fn release_type_wire_values_are_total() {
        assert!(wire_value_matches(ReleaseType::Pre.as_str(), b"pre"));
        assert!(wire_value_matches(ReleaseType::Full.as_str(), b"full"));
        assert!(wire_value_matches(ReleaseType::Security.as_str(), b"security"));
    }

    /// Proves that every update policy has a stable wire value.
    #[kani::proof]
    #[kani::unwind(18)]
    fn update_policy_wire_values_are_total() {
        assert!(wire_value_matches(
            UpdatePolicy::SecurityOnly.as_str(),
            b"security_only"
        ));
        assert!(wire_value_matches(
            UpdatePolicy::SecurityFull.as_str(),
            b"security_full"
        ));
        assert!(wire_value_matches(
            UpdatePolicy::SecurityFullPre.as_str(),
            b"security_full_pre"
        ));
    }
}

#[cfg(verus)]
mod verus_verification {
    use super::{ReleaseType, UpdatePolicy};
    use vstd::prelude::*;

    verus! {
        /// Verifies the release allowlist against the actual production function body.
        proof fn update_policy_acceptance_is_exact() {
            assert(!UpdatePolicy::SecurityOnly.accepts_release_type(ReleaseType::Pre));
            assert(!UpdatePolicy::SecurityOnly.accepts_release_type(ReleaseType::Full));
            assert(UpdatePolicy::SecurityOnly.accepts_release_type(ReleaseType::Security));

            assert(!UpdatePolicy::SecurityFull.accepts_release_type(ReleaseType::Pre));
            assert(UpdatePolicy::SecurityFull.accepts_release_type(ReleaseType::Full));
            assert(UpdatePolicy::SecurityFull.accepts_release_type(ReleaseType::Security));

            assert(UpdatePolicy::SecurityFullPre.accepts_release_type(ReleaseType::Pre));
            assert(UpdatePolicy::SecurityFullPre.accepts_release_type(ReleaseType::Full));
            assert(UpdatePolicy::SecurityFullPre.accepts_release_type(ReleaseType::Security));
        }
    }
}

#[cfg(kani)]
#[allow(dead_code)]
fn main() {}

#[cfg(verus)]
fn main() {}
