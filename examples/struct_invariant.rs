use vstd::prelude::*;

// `int` is Verus's unbounded mathematical integer — ghost-only, with
// no runtime bits. Exec code that tries to handle an `int` directly
// (even via `3 as int`) is rejected by Verus:
//
//   The Verus types 'nat' and 'int' can only be used in ghost code
//   (e.g., in a 'spec' or 'proof' function, inside a 'proof' block,
//    or when assigning to a 'ghost' or 'tracked' variable)
//
// `let ghost` binds the struct in ghost context, so the literals are
// spec-mode and coerce to `int` freely.
verus! {

struct Point {
    x: int,
    y: int,
}

impl Point {
    spec fn len2(&self) -> int {
        self.x * self.x + self.y * self.y
    }
}

fn main() {
    let ghost p = Point { x: 3, y: 4 };
    assert(p.len2() == 25);
}

} // verus!
