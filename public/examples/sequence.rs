use vstd::prelude::*;

verus! {

// Seq<int> is vstd's specification-level sequence. The verifier knows
// the semantics of empty / push / len / indexing, so every `assert`
// below is discharged automatically via the SMT encoding.
fn main() {
    proof {
        let s: Seq<int> = Seq::empty().push(1).push(2).push(3);
        assert(s.len() == 3);
        assert(s[0] == 1);
        assert(s[1] == 2);
        assert(s[2] == 3);
        assert(s.push(4).len() == 4);
        assert(s.push(4)[3] == 4);
        assert(forall|i: int| 0 <= i < s.len() ==> s.push(4)[i] == s[i]);
    }
}

} // verus!
