#[path = "../../crates/gitrun-updater/src/policy_logic.rs"]
mod policy_logic;

fn main() {}


#[cfg(kani)]
mod verification {
    use super::policy_logic::{ReleaseType, UpdatePolicy};

    /// Checks that this standalone harness reaches the production policy module.
    #[kani::proof]
    fn standalone_harness_uses_production_policy() {
        assert!(!UpdatePolicy::SecurityOnly.accepts_release_type(ReleaseType::Pre));
        assert!(UpdatePolicy::SecurityOnly.accepts_release_type(ReleaseType::Security));
        assert!(UpdatePolicy::SecurityFull.accepts_release_type(ReleaseType::Full));
        assert!(UpdatePolicy::SecurityFullPre.accepts_release_type(ReleaseType::Pre));
    }
}

#[cfg(verus)]
mod verus_verification {
    use super::policy_logic::{ReleaseType, UpdatePolicy};
    use vstd::prelude::*;

    verus! {
        /// Verifies the imported production policy through this standalone entrypoint.
        proof fn standalone_harness_uses_production_policy() {
            assert!(!UpdatePolicy::SecurityOnly.accepts_release_type(ReleaseType::Pre));
            assert!(UpdatePolicy::SecurityOnly.accepts_release_type(ReleaseType::Security));
            assert!(UpdatePolicy::SecurityFull.accepts_release_type(ReleaseType::Full));
            assert!(UpdatePolicy::SecurityFullPre.accepts_release_type(ReleaseType::Pre));
        }
    }
}
