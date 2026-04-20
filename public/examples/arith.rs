use vstd::prelude::*;

// Specification-level integer arithmetic (`int`, not `i32`). Outside of
// spec context the literals would infer as `i32` and trip E0308.
verus! {

fn main() {
    proof {
        let x: int = 10;
        assert(x * x == 100);
        assert(x + x == 20);
        assert(x * (x - 1) == 90);
        assert(forall|k: int| k > 0 ==> k * x > 0);
    }
}

} // verus!
