// Headless-browser smoke test for the rustc → HIR → VIR pipeline with
// `verify = false` so the AIR → Z3 stage is skipped — no Z3 shims needed.
//
// Intentionally a single test: rustc's `SESSION_GLOBALS` is a scoped TLS, and
// on wasm32 `panic = abort` doesn't unwind RAII guards, so a panicking test
// leaks session globals and breaks every subsequent test in the same binary.
// Keep assertions additive within one test to dodge that problem.
//
// Run with: wasm-pack test --chrome --headless

use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn pipeline_reaches_vir() {
    let out = verus_explorer::parse_source(
        "fn main() {}",
        /* dump_air_initial */ false,
        /* dump_air_middle */ false,
        /* dump_air_final */ false,
        /* dump_smt */ false,
        /* verify */ false,
    );
    assert!(out.contains("=== AST ==="), "missing AST section:\n{out}");
    assert!(out.contains("=== HIR ==="), "missing HIR section:\n{out}");
    assert!(out.contains("=== VIR ==="), "missing VIR section:\n{out}");
    assert!(
        !out.contains("=== VERDICT ==="),
        "VERDICT should be absent when verify=false:\n{out}"
    );
    assert!(!out.contains("panicked:"), "pipeline panicked:\n{out}");
}
