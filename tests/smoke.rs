// Node-hosted smoke test for the rustc → HIR → VIR pipeline. Stops before
// AIR → Z3 (`verify = false`) so no Z3 plumbing is needed.
//
// Runs under `wasm-pack test --node` (see the Makefile). Node is preferred
// over a headless browser because `wasm-bindgen-test-runner`'s server only
// exposes the test bundle — it can't serve the ~60 MB of rmetas + vstd.vir
// that rustc's crate locator needs. In Node mode we have real filesystem
// access via the `fs` module and can read those files from the path
// `build.rs` emits as the `WASM_LIBS_DIR` env var.
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

// `src/lib.rs` declares `verus_diagnostic`, `verus_dump`, and `verus_bench`
// as imports from the JS host (see the `#[wasm_bindgen(js_name = …)]`
// externs there). The browser installs them on `globalThis`; under
// `wasm-pack test --node` nothing does, so every call site would
// ReferenceError. Stub them before `parse_source` reaches any diagnostic,
// `emit_section`, or `time` call site. `verus_bench` routes
// through `process.stderr.write` so stage timings show up in `make test`
// output alongside the per-call bench line — see `bench_log` above.
#[wasm_bindgen(inline_js = "\
    export function install_pipeline_stubs() {\n\
      globalThis.verus_diagnostic = () => {};\n\
      globalThis.verus_dump = () => {};\n\
      globalThis.verus_bench = (label, ms) => {\n\
        process.stderr.write(`[stage] ${label}=${ms.toFixed(0)}ms\\n`);\n\
      };\n\
    }")]
extern "C" {
    fn install_pipeline_stubs();
}

// `console.log` is captured by wasm-bindgen-test (only surfaces on
// failure), so we bypass it by writing directly to `process.stderr`, which
// wasm-pack's test runner forwards verbatim. `perf_now` is reused from
// `verus_explorer` itself — declaring it a second time here would produce
// a duplicate `__wbindgen_describe___wbg_now_*` symbol at link time (each
// wasm-bindgen extern mangles to a hash of name + signature, so the same
// JS import declared in two crates collides).
use verus_explorer::perf_now;

#[wasm_bindgen(inline_js = "\
    export function bench_log(s) { process.stderr.write(s + '\\n'); }")]
extern "C" {
    fn bench_log(s: &str);
}

// The Z3 wasm bridge in `third_party/verus/source/air/src/smt_process.rs`
// imports `Z3_mk_config`/`Z3_mk_context`/`Z3_del_config`/
// `Z3_eval_smtlib2_string`/`Z3_del_context` from globalThis. The browser
// wires them to Emscripten's Z3 build; under Node we stub them. The stub
// answers the two query shapes Verus expects responses to:
//   - `(check-sat)` → `unsat` (makes every proof trivially succeed; this is
//     a structural smoke test of the AIR → SMT wiring, not a solver check).
//   - `(get-info :all-statistics)` → `(:rlimit-count 0)`. Verus calls
//     `smt_get_rlimit_count` three times per query (see smt_verify.rs) and
//     parses the response with `sise::read_into_tree`, which panics on
//     malformed input — so an s-expression, not raw `unsat`, is required.
// Everything else (set-option, declare-fun, push/pop, assert,
// get-info :version with `ignore_unexpected_smt=false` and no expected
// version) produces no output and flows through empty-lines handling.
#[wasm_bindgen(inline_js = "\
    export function install_z3_stubs() {\n\
      let next_id = 1;\n\
      globalThis.Z3_mk_config = () => next_id++;\n\
      globalThis.Z3_mk_context = () => next_id++;\n\
      globalThis.Z3_del_config = () => {};\n\
      globalThis.Z3_del_context = () => {};\n\
      globalThis.Z3_eval_smtlib2_string = (_ctx, s) => {\n\
        let out = '';\n\
        const sats = (s.match(/\\(check-sat\\)/g) || []).length;\n\
        out += 'unsat\\n'.repeat(sats);\n\
        const stats = (s.match(/\\(get-info :all-statistics\\)/g) || []).length;\n\
        out += '(:rlimit-count 0)\\n'.repeat(stats);\n\
        return out;\n\
      };\n\
    }")]
extern "C" {
    fn install_z3_stubs();
}

#[wasm_bindgen_test]
fn pipeline_preserves_ghost_proof_block() {
    // The lib's `#[wasm_bindgen(start)]` only fires when verus-explorer is
    // the final cdylib; here we're an rlib inside wasm-bindgen-test's own
    // cdylib, so install the panic hook + proc-macro shims by hand.
    verus_explorer::init();
    install_pipeline_stubs();
    install_z3_stubs();

    // Stream every rmeta + `vstd.vir` staged by `build.rs` into the
    // wasm-libs bundle. The `.gz` siblings are for the browser loader;
    // here we read the originals directly.
    let t0 = perf_now();
    let dir = env!("WASM_LIBS_DIR");
    for entry in readdir_sync(dir) {
        let name = entry.as_string().expect("readdirSync returns strings");
        if !(name.ends_with(".rmeta") || name == "vstd.vir") {
            continue;
        }
        let bytes = read_file_sync(&format!("{dir}/{name}"));
        verus_explorer::wasm_libs_add_file(name, bytes);
    }
    verus_explorer::wasm_libs_finalize();
    let t_libs = perf_now();

    // `assert(false)` inside a `proof { }` block is a direct witness of
    // ghost preservation: if `cfg_erase()` returns anything but `Keep`, the
    // `verus!` proc-macro strips the proof body at expansion time and VIR
    // sees an empty `fn main() {}` — no `AssertAssume` node lowered. Both
    // `verus_keep_ghost` *and* `verus_keep_ghost_body` rustc `--cfg`s must
    // stay wired up in `pipeline.rs::build_rustc_config` to sustain this.
    let src = "use vstd::prelude::*;\n\
               verus! { fn main() { proof { assert(false); } } }";

    let t1 = perf_now();
    let out1 = verus_explorer::parse_and_verify(src, /* verify */ true, /* expand_errors */ false);
    let t2 = perf_now();

    // Second and third parse_source in the *same* wasm instance. The JS
    // `freshVerus` pattern (public/index.html) spins up a new instance per
    // click specifically to avoid this; if they succeed here, the pattern
    // is wasted engineering. If they panic, panic=abort on wasm32 aborts
    // the whole test and the panic-hook-captured message (installed by
    // `verus_explorer::init`) surfaces via wasm-pack's output — telling
    // us exactly which global-state invariant trips. #3 confirms whether
    // #2 was a one-shot warmup or whether subsequent calls are steady-state.
    let out2 = verus_explorer::parse_and_verify(src, /* verify */ true, /* expand_errors */ false);
    let t3 = perf_now();
    let out3 = verus_explorer::parse_and_verify(src, /* verify */ true, /* expand_errors */ false);
    let t4 = perf_now();

    bench_log(&format!(
        "[bench] wasm_libs_setup={:.0}ms parse#1={:.0}ms parse#2={:.0}ms parse#3={:.0}ms",
        t_libs - t0,
        t2 - t1,
        t3 - t2,
        t4 - t3,
    ));

    for (label, out) in [("#1", &out1), ("#2", &out2), ("#3", &out3)] {
        assert!(!out.contains("panicked:"), "pipeline panicked on {label}:\n{out}");
        assert!(out.contains("=== VIR ==="), "missing VIR section on {label}:\n{out}");
        assert!(
            out.contains("AssertAssume"),
            "VIR missing AssertAssume on {label} — proof block was erased before VIR:\n{out}"
        );
    }
}
