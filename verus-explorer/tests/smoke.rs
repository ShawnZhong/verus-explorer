// Node-hosted smoke test for the full Verus pipeline. Z3 is stubbed
// (see `install_z3_stubs` below) so we exercise rustc → HIR → VIR → AIR
// → SMT emission end-to-end; the stub returns `unsat` for every
// `(check-sat)`, so this is structural coverage of the AIR → SMT wiring,
// not a solver correctness check.
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

// Node's built-in `fs`. `wasm-pack test --node` emits CommonJS glue, so
// this resolves to `require('fs')`. Buffers returned by `readFileSync`
// are Uint8Array subclasses — wasm-bindgen marshals them into `Vec<u8>`
// via the standard typed-array path. We read from
// `CARGO_MANIFEST_DIR/../target/libs/` (the bundle `make libs` stages
// via `scripts/build-libs.sh`), resolved at compile time through
// `concat!(env!("CARGO_MANIFEST_DIR"), …)` — no build.rs involved.
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
// ReferenceError. Stub them before `verify` reaches any diagnostic,
// `emit_section`, or `time` call site.
//
// The `verus_dump` stub accumulates each section's joined block content
// into `globalThis._sections` so test assertions can look up specific
// sections by name — `verify` no longer returns a dumped
// String, all pipeline output flows through this callback.
// `reset_sections` clears the accumulator between parse calls so each
// run's assertions see only its own output.
// `verus_bench` routes through `process.stderr.write` so stage timings
// show up in `make test` output alongside the per-call bench line —
// see `bench_log` above.
#[wasm_bindgen(inline_js = "\
    export function install_pipeline_stubs() {\n\
      globalThis._sections = new Map();\n\
      globalThis.verus_diagnostic = () => {};\n\
      globalThis.verus_diagnostic_json = () => {};\n\
      globalThis.verus_dump = (section, contents, _folds) => {\n\
        let body = '';\n\
        for (let i = 0; i < contents.length; i++) {\n\
          if (i > 0 && !body.endsWith('\\n')) body += '\\n';\n\
          body += contents[i];\n\
        }\n\
        globalThis._sections.set(section, body);\n\
      };\n\
      globalThis.verus_bench = (label, ms) => {\n\
        process.stderr.write(`[stage] ${label}=${ms.toFixed(0)}ms\\n`);\n\
      };\n\
    }\n\
    export function reset_sections() { globalThis._sections = new Map(); }\n\
    export function section_body(name) {\n\
      return globalThis._sections.get(name) ?? '';\n\
    }")]
extern "C" {
    fn install_pipeline_stubs();
    fn reset_sections();
    fn section_body(name: &str) -> String;
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
//
// Rust-side capture (smt_log + smt_transcript_log attached as SharedBufs
// in verify_stage.rs) provides the full query / transcript content via
// the standard `verus_dump` path, so the test reads them via
// `section_body("SMT_QUERY")` / `section_body("SMT_TRANSCRIPT")`.
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

    // Stream the libs bundle staged by `make libs` into the wasm
    // instance. Layout: shared rmetas at the root of `target/libs/`,
    // std-mode-only rmetas under `std/`, nostd-mode-only under
    // `nostd/`. The smoke test exercises the nostd bundle — a basic
    // proof that uses only `vstd::prelude` + `assert`, so libstd's
    // surface isn't needed and the smaller bundle loads faster. Path
    // resolved at compile time relative to `CARGO_MANIFEST_DIR`
    // (`verus-explorer/`) so no build-script env var is required.
    verus_explorer::set_std_mode(false);
    let t0 = perf_now();
    const LIBS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/libs");
    let stream_dir = |sub: &str| {
        let dir = format!("{LIBS}/{sub}");
        for entry in readdir_sync(&dir) {
            let name = entry.as_string().expect("readdirSync returns strings");
            if !(name.ends_with(".rmeta") || name == "vstd.vir") {
                continue;
            }
            let bytes = read_file_sync(&format!("{dir}/{name}"));
            verus_explorer::wasm_libs_add_file(name, bytes);
        }
    };
    stream_dir("");
    stream_dir("nostd");
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

    // Three calls in the *same* wasm instance. The JS `freshVerus` pattern
    // (public/index.html) spins up a new instance per click specifically to
    // avoid reusing one; if they succeed here, the pattern is wasted
    // engineering. If they panic, panic=abort on wasm32 aborts the whole
    // test and the panic-hook-captured message (installed by
    // `verus_explorer::init`) surfaces via wasm-pack's output — telling us
    // exactly which global-state invariant trips. #3 confirms whether #2
    // was a one-shot warmup or whether subsequent calls are steady-state.
    //
    // Pipeline output flows through the `verus_dump` stub installed above,
    // not through a return value — `reset_sections()` clears the map so
    // each run's assertions see only its own emissions.
    let mut stage_times = Vec::new();
    for label in ["#1", "#2", "#3"] {
        reset_sections();
        let t = perf_now();
        verus_explorer::verify(src, /* expand_errors */ false);
        stage_times.push(perf_now() - t);
        let vir = section_body("VIR");
        assert!(!vir.is_empty(), "missing VIR section on {label}");
        assert!(
            vir.contains("AssertAssume"),
            "VIR missing AssertAssume on {label} — proof block was erased before VIR:\n{vir}"
        );
        // arch is the very last VIR item (no section wraps it); its
        // banner + content + close sit at the end of the body. Guard
        // it explicitly — fold-range bugs tend to show up at EOF.
        assert!(
            vir.contains(";;v arch word_bits"),
            "VIR missing `;;v arch word_bits` on {label}"
        );
        // Section markers embedded via `air_ctx.section(marker, …)`
        // / `section_close()` must reach the outputs, otherwise
        // the browser's fold scanner has no markers to key off of.
        // `;;>` on the prelude and Context ops, `;;v` on Query ops,
        // `;;<` to close. A regression here usually means markers
        // reverted to plain `;;` or close emits got lost.
        //
        // AIR_INITIAL is the canonical source: `run_queries` does
        // a final `drain_to` after the loop so trailing closes
        // always land in `air_bodies`. (The Z3 pipe path drops
        // trailing closes because nothing flushes `pipe_buffer`
        // after the last `(check-sat)`.)
        let air = section_body("AIR_INITIAL");
        let prelude_count = air.matches(";;> AIR prelude").count();
        assert_eq!(
            prelude_count, 1,
            "expected exactly one `;;> AIR prelude` section on {label}, got {prelude_count}"
        );
        assert!(
            air.contains(";;v Function-Def crate::main"),
            "AIR body missing `;;v Function-Def crate::main` section on {label}"
        );
        // Context-op marker depends on the op's source crate:
        // `;;>` (auto-fold) for external crates, `;;v` (expanded)
        // for local. This tiny smoke source has no vstd *function*
        // dependencies so every Context op here is local.
        assert!(
            air.contains(";;v Function-Specs crate::main")
                || air.contains(";;v Function-Axioms crate::main"),
            "AIR body missing a local Context section on {label}"
        );
        assert!(
            air.contains(";;<"),
            "AIR body missing `;;<` close markers on {label}"
        );
        let is_open = |l: &&str| {
            (l.starts_with(";;>") && (l.len() == 3 || l.as_bytes()[3] == b' '))
                || (l.starts_with(";;v") && (l.len() == 3 || l.as_bytes()[3] == b' '))
        };
        let is_close = |l: &&str| *l == ";;<" || l.starts_with(";;< ");
        let opens = air.lines().filter(is_open).count();
        let closes = air.lines().filter(is_close).count();
        if opens != closes {
            let markers: Vec<&str> = air.lines().filter(|l| is_open(l) || is_close(l)).collect();
            panic!(
                "open/close balance mismatch in AIR_INITIAL on {label}: \
                 {opens} opens vs {closes} closes\nmarkers in order:\n{}",
                markers.join("\n")
            );
        }
        // SMT tabs are fed by Rust-owned `set_smt_log` /
        // `set_smt_transcript_log` channels — `SMT_QUERY` mirrors
        // what Verus sends Z3, `SMT_TRANSCRIPT` interleaves it
        // with the stub Z3's `unsat` / `(:rlimit-count …)` replies.
        let smt_query = section_body("SMT_QUERY");
        assert!(
            smt_query.contains(";;v Function-Def crate::main"),
            "SMT_QUERY missing `;;v Function-Def crate::main` on {label}"
        );
        let smt_transcript = section_body("SMT_TRANSCRIPT");
        // SMT_TRANSCRIPT mirrors SMT_QUERY's op-level wrappers
        // (from `Context::section` tee), plus a `;;v <ms>` fold per
        // Z3 reply. Commands between markers flow as plain content.
        assert!(
            smt_transcript.contains(";;v Function-Def crate::main"),
            "SMT_TRANSCRIPT missing op-level section header on {label}"
        );
        assert!(
            smt_transcript.contains("ms\n"),
            "SMT_TRANSCRIPT missing per-response timing banner on {label}"
        );
        assert!(
            smt_transcript.contains("unsat"),
            "SMT_TRANSCRIPT missing Z3 `unsat` reply on {label} — \
             transcript_log isn't capturing Z3_eval_smtlib2_string output"
        );
    }

    bench_log(&format!(
        "[bench] wasm_libs_setup={:.0}ms parse#1={:.0}ms parse#2={:.0}ms parse#3={:.0}ms",
        t_libs - t0,
        stage_times[0],
        stage_times[1],
        stage_times[2],
    ));
}
