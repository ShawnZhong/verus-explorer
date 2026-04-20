use vstd::prelude::*;

// Struct + spec-mode method: `spec fn len2` is available in `ensures`
// clauses and `assert`s but emits no runtime code. The `rotate_90`
// proof needs a nonlinear hint because SMT is incomplete on products
// of symbolic terms — `by(nonlinear_arith)` flips the solver into its
// dedicated nonlinear mode for that one fact.
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

fn rotate_90(p: Point) -> (o: Point)
    ensures o.len2() == p.len2(),
{
    let o = Point { x: -p.y, y: p.x };
    assert((-p.y) * (-p.y) == p.y * p.y) by(nonlinear_arith);
    o
}

fn main() {
    let p = Point { x: 3, y: 4 };
    let q = rotate_90(p);
    assert(q.len2() == 25);
}

} // verus!
