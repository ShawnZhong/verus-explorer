use vstd::prelude::*;

verus! {

// Map<int, int> is vstd's specification-level finite map. Keys live in
// `dom()` (a `Set<K>`), values are looked up with `m[k]`, and every
// primitive (`empty`, `insert`, `remove`, `len`) has an axiomatic SMT
// encoding the verifier can reason about without running the code.
fn main() {
    proof {
        let m: Map<int, int> = Map::empty().insert(1, 10).insert(2, 20).insert(3, 30);

        assert(m.dom().contains(2));
        assert(m[2] == 20);
        assert(m.len() == 3);

        // `insert` overwrites the prior value at the same key.
        let m2 = m.insert(2, 99);
        assert(m2[2] == 99);
        assert(m2.len() == 3);

        // `remove` shrinks `dom()`; other keys are untouched.
        let m3 = m.remove(2);
        assert(!m3.dom().contains(2));
        assert(m3[1] == 10);
        assert(m3.len() == 2);

        // Quantified property: every key in `m` still maps to the same
        // value in `m.insert(4, 40)` — extensional reasoning the solver
        // discharges from the insert/index axioms.
        assert(forall|k: int| m.dom().contains(k) ==> m.insert(4, 40)[k] == m[k]);
    }
}

} // verus!
