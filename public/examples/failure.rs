use vstd::prelude::*;

// Deliberate verification failures with *compound* assertions, so the
// expand-errors note has a tree to show (check "Expand errors" in the
// toolbar). For atomic `assert(false)` the note would just print `false`
// with no breakdown. Each failure lives in its own function because after
// `assert(P)` Verus adds `P` to the SMT context — two failing asserts in
// the same body would only report the first.
verus! {

fn foo() {
    proof {
        let x: int = 3;
        assert(x > 0 && x > 10);
    }
}

fn bar() {
    proof {
        let y: int = 5;
        assert(y > 0 ==> y < 3);
    }
}

fn main() {
    foo();
    bar();
}

} // verus!
