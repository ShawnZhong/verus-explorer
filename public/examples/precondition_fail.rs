use vstd::prelude::*;

// Precondition violation at the call site. `require_nonneg` asks for
// `x >= 0`; `main` passes `-1`. Verus reports a failed `requires`
// pointing at the call and the clause it couldn't discharge. Useful
// for seeing the cross-span diagnostic shape (primary + related span).
verus! {

fn require_nonneg(x: i32)
    requires
        x >= 0,
{
}

fn main() {
    require_nonneg(-1);
}

} // verus!
