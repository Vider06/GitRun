//! Shared, byte-oriented GitHub repository-component policy.
//!
//! This is the exact predicate used by CLI repository validation. Keeping it
//! in a dependency-free module lets Kani exhaustively check all byte values
//! and lets Verus verify the executable predicate against its closed contract.

#[cfg(verus)]
use vstd::prelude::{verus_spec, verus_verify, SpecOrd};

#[cfg_attr(verus, verus_verify(dual_spec, closed))]
#[cfg_attr(
    verus,
    verus_spec(returns
        (b'A' <= byte && byte <= b'Z')
            || (b'a' <= byte && byte <= b'z')
            || (b'0' <= byte && byte <= b'9')
            || byte == b'-'
            || byte == b'_'
            || byte == b'.'
    )
)]
pub(crate) fn repo_component_byte_allowed(byte: u8) -> bool {
    #[cfg(verus)]
    {
        (b'A' <= byte && byte <= b'Z')
            || (b'a' <= byte && byte <= b'z')
            || (b'0' <= byte && byte <= b'9')
            || byte == b'-'
            || byte == b'_'
            || byte == b'.'
    }
    #[cfg(not(verus))]
    {
        byte.is_ascii_uppercase()
            || byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'-' | b'_' | b'.')
    }
}

#[cfg(kani)]
mod kani_verification {
    use super::repo_component_byte_allowed;

    #[kani::proof]
    fn repository_component_byte_policy_is_exact() {
        let byte: u8 = kani::any();
        let expected = (b'A' <= byte && byte <= b'Z')
            || (b'a' <= byte && byte <= b'z')
            || (b'0' <= byte && byte <= b'9')
            || matches!(byte, b'-' | b'_' | b'.');
        assert_eq!(repo_component_byte_allowed(byte), expected);
    }
}

#[cfg(kani)]
fn main() {}

#[cfg(verus)]
fn main() {}
