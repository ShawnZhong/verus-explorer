use vstd::prelude::*;

// Loop verification: `invariant` pins down properties that hold at every
// iteration's head; `decreases` proves termination by giving Verus a
// well-founded measure that strictly decreases each time through.
verus! {

fn count_up_to(n: u32) -> (r: u32)
    ensures
        r == n,
{
    let mut i: u32 = 0;
    while i < n
        // FIXME: i is at most n
        // invariant
        //     i <= n,
        decreases n - i,
    {
        i = i + 1;
    }
    i
}

fn main() {
    let x = count_up_to(10);
    assert(x == 10);
}

} // verus!
