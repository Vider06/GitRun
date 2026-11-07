//! Pure updater release-policy types shared by runtime code and Kani proofs.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(kani), derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(not(kani), serde(rename_all = "snake_case"))]
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
#[cfg_attr(not(kani), derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(not(kani), serde(rename_all = "snake_case"))]
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

    pub fn accepts_release_type(self, release_type: ReleaseType) -> bool {
        match self {
            Self::SecurityOnly => release_type == ReleaseType::Security,
            Self::SecurityFull => matches!(release_type, ReleaseType::Security | ReleaseType::Full),
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

    /// Proves that every representable release type has a stable wire value.
    #[kani::proof]
    fn release_type_wire_values_are_total() {
        assert!(ReleaseType::Pre.as_str() == "pre");
        assert!(ReleaseType::Full.as_str() == "full");
        assert!(ReleaseType::Security.as_str() == "security");
    }

    /// Proves that every update policy has a stable wire value.
    #[kani::proof]
    fn update_policy_wire_values_are_total() {
        assert!(UpdatePolicy::SecurityOnly.as_str() == "security_only");
        assert!(UpdatePolicy::SecurityFull.as_str() == "security_full");
        assert!(UpdatePolicy::SecurityFullPre.as_str() == "security_full_pre");
    }
}
