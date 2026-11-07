// This arithmetic proof only smoke-tests the pinned Verus toolchain.
// It is not a proof of GitRun production behavior or source coverage.
use vstd::prelude::*;

verus! {
    fn main() {
        assert(2 + 2 == 4);
    }
}
