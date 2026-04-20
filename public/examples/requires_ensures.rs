use vstd::prelude::*;

// Function contracts: `requires` constrains what the caller must prove
// before the call; `ensures` states what the function guarantees on
// return. The verifier discharges every call site against `requires`
// and the body against `ensures`.
verus! {

fn octuple(x: i8) -> (r: i8)
    requires
        -16 <= x < 16,
    ensures
        r == 8 * x,
{
    let x2 = x + x;
    let x4 = x2 + x2;
    x4 + x4
}

fn main() {
    let n = octuple(10);
    assert(n == 80);
}

} // verus!
