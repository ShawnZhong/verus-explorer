use vstd::prelude::*;

verus! {

proof fn test_seq1() {
    let s: Seq<int> = seq![0, 10, 20, 30, 40];
    // FIXME: Length of s is 5
    assert(s.len() == 0);
    assert(s[2] == 20);
    assert(s[3] == 30);
}

proof fn test_set1() {
    let s: Set<int> = set![0, 10, 20, 30, 40];
    // FIXME: s is finite
    assert(!s.finite());
    assert(s.contains(20));
    assert(s.contains(30));
    assert(!s.contains(60));
}

proof fn test_map1() {
    let m: Map<int, int> = map![0 => 0, 10 => 100, 20 => 200, 30 => 300, 40 => 400];
    // FIXME: 42 is in the domain of m
    assert(m.dom().contains(42));
    assert(m.dom().contains(30));
    assert(!m.dom().contains(60));
    assert(m[20] == 200);
    assert(m[30] == 300);
}

} // verus!
