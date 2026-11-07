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
    #[kani::proof]
    fn update_policy_acceptance_is_exact() {
        let policy: UpdatePolicy = kani::any();
        let release_type: ReleaseType = kani::any();
        let expected = match (policy, release_type) {
            (UpdatePolicy::SecurityOnly, ReleaseType::Security) => true,
            (UpdatePolicy::SecurityOnly, _) => false,
            (UpdatePolicy::SecurityFull, ReleaseType::Security | ReleaseType::Full) => true,
            (UpdatePolicy::SecurityFull, ReleaseType::Pre) => false,
            (UpdatePolicy::SecurityFullPre, _) => true,
        };
        assert_eq!(policy.accepts_release_type(release_type), expected);
    }

    /// Proves that every representable release type has a stable wire value.
    #[kani::proof]
    fn release_type_wire_values_are_total() {
        let release_type: ReleaseType = kani::any();
        let value = release_type.as_str();
        match release_type {
            ReleaseType::Pre => assert!(value == "pre"),
            ReleaseType::Full => assert!(value == "full"),
            ReleaseType::Security => assert!(value == "security"),
        }
    }
}
