// Node-hosted smoke test for the rustc → HIR → VIR pipeline. Stops before
// AIR → Z3 (`verify = false`) so no Z3 plumbing is needed.
//
// Runs under `wasm-pack test --node` (see the Makefile). Node is preferred
// over a headless browser because `wasm-bindgen-test-runner`'s server only
// exposes the test bundle — it can't serve the ~60 MB of rmetas + vstd.vir
// that rustc's crate locator needs. In Node mode we have real filesystem
// access via the `fs` module and can read those files from the path
// `build.rs` emits as the `SYSROOT_DIR` env var.
//
// Intentionally a single test: rustc's `SESSION_GLOBALS` is a scoped TLS, and
// on wasm32 `panic = abort` doesn't unwind RAII guards, so a panicking test
// leaks session globals and breaks every subsequent test in the same binary.
// Keep assertions additive within one test to dodge that problem.

use wasm_bindgen::prelude::*;
use wasm_bindgen_test::*;

// Node's built-in `fs`. `wasm-pack test --node` emits CommonJS glue, so this
// resolves to `require('fs')`. Buffers returned by `readFileSync` are
// Uint8Array subclasses — wasm-bindgen marshals them into `Vec<u8>` via the
// standard typed-array path.
#[wasm_bindgen(module = "fs")]
extern "C" {
    #[wasm_bindgen(js_name = readFileSync)]
    fn read_file_sync(path: &str) -> Vec<u8>;

    #[wasm_bindgen(js_name = readdirSync)]
    fn readdir_sync(path: &str) -> Vec<JsValue>;
}

#[wasm_bindgen_test]
fn pipeline_preserves_ghost_proof_block() {
    // The lib's `#[wasm_bindgen(start)]` only fires when verus-explorer is
    // the final cdylib; here we're an rlib inside wasm-bindgen-test's own
    // cdylib, so install the panic hook + proc-macro shims by hand.
    verus_explorer::init();

    // Stream every rmeta + `vstd.vir` staged by `build.rs` into the virtual
    // sysroot. `manifest.json` lives alongside them but isn't an rmeta, so
    // skip it.
    let dir = env!("SYSROOT_DIR");
    for entry in readdir_sync(dir) {
        let name = entry.as_string().expect("readdirSync returns strings");
        if name == "manifest.json" {
            continue;
        }
        let bytes = read_file_sync(&format!("{dir}/{name}"));
        verus_explorer::sysroot_add_file(name, bytes);
    }
    verus_explorer::sysroot_finalize();

    // `assert(false)` inside a `proof { }` block is a direct witness of
    // ghost preservation: if `cfg_erase()` returns anything but `Keep`, the
    // `verus!` proc-macro strips the proof body at expansion time and VIR
    // sees an empty `fn main() {}` — no `AssertAssume` node lowered. Both
    // `verus_keep_ghost` *and* `verus_keep_ghost_body` rustc `--cfg`s must
    // stay wired up in `pipeline.rs::build_rustc_config` to sustain this.
    let out = verus_explorer::pipeline::parse_source(
        "use vstd::prelude::*;\n\
         verus! { fn main() { proof { assert(false); } } }",
        verus_explorer::pipeline::DumpStages { vir: true, ..Default::default() },
        /* verify */ false,
    );
    assert!(!out.contains("panicked:"), "pipeline panicked:\n{out}");
    assert!(out.contains("=== VIR ==="), "missing VIR section:\n{out}");
    assert!(
        out.contains("AssertAssume"),
        "VIR missing AssertAssume — proof block was erased before VIR:\n{out}"
    );
}
