use vstd::prelude::*;

// Deliberate verification failure: the assertion is unconditionally
// false, so Z3 returns `sat` and the pipeline emits a failed query.
// Useful for seeing what the diagnostic + VERDICT paths look like.
verus! {

fn main() {
    proof {
        assert(false);
    }
}

} // verus!
