use vstd::prelude::*;

// Recursive `spec fn`: lives in the proof world (no runtime code
// emitted) and needs a `decreases` clause to prove termination. Verus
// checks that the measure strictly decreases on every recursive call
// under a well-founded order; here `n: nat` gives that order for free.
verus! {

spec fn triangle(n: nat) -> nat
    decreases n,
{
    if n == 0 {
        0
    } else {
        n + triangle((n - 1) as nat)
    }
}

fn main() {
    proof {
        assert(triangle(0) == 0);
        assert(triangle(1) == 1);
        assert(triangle(3) == 6);
        assert(triangle(5) == 15);
    }
}

} // verus!
