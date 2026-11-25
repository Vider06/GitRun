//! Standalone Verus contract for the CLI repository-component byte policy.
//!
//! Keep this proof independent from the interactive CLI binary: Verus builds
//! intentionally omit runtime-only APIs from the application dependency graph.

use vstd::prelude::*;

verus! {
    open spec fn repo_component_byte_allowed_spec(byte: u8) -> bool {
        (b'A' <= byte && byte <= b'Z')
            || (b'a' <= byte && byte <= b'z')
            || (b'0' <= byte && byte <= b'9')
            || byte == b'-'
            || byte == b'_'
            || byte == b'.'
    }

    proof fn repository_component_contract_rejects_separators()
        ensures
            !repo_component_byte_allowed_spec(b'/'),
            !repo_component_byte_allowed_spec(b' '),
            !repo_component_byte_allowed_spec(b'@'),
            !repo_component_byte_allowed_spec(0),
            repo_component_byte_allowed_spec(b'A'),
            repo_component_byte_allowed_spec(b'Z'),
            repo_component_byte_allowed_spec(b'a'),
            repo_component_byte_allowed_spec(b'z'),
            repo_component_byte_allowed_spec(b'0'),
            repo_component_byte_allowed_spec(b'9'),
    {
    }
}
