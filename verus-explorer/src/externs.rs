// JS externs — the host-supplied API every pipeline stage calls into.
// The browser (`public/app.js`) installs these on `globalThis`; the
// smoke test in `tests/smoke.rs` installs its own stubs. Each `fn`
// here has a matching JS function on the host side, wired up by the
// `js_name` attribute where the Rust name and JS name diverge.

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    pub(crate) fn console_error(msg: &str);

    // Imported from `public/app.js`. Called synchronously from
    // `DomWriter` so each rustc diagnostic lands in the output panel before
    // rustc's `abort_if_errors` turns into a wasm `unreachable` trap.
    #[wasm_bindgen(js_name = verus_diagnostic)]
    pub(crate) fn verus_diagnostic(msg: &str);

    // Same survivability reasoning as `verus_diagnostic`, but carries the
    // structured JsonEmitter output (one diagnostic per line). The JS side
    // parses it into `byte_start`/`byte_end` + `line`/`col` spans and feeds
    // CM6 `setDiagnostics` — gives us precise squiggle ranges and
    // secondary-label spans without scraping the human-readable text.
    #[wasm_bindgen(js_name = verus_diagnostic_json)]
    pub(crate) fn verus_diagnostic_json(msg: &str);

    // Streams each completed pipeline section (AST / HIR / VIR /
    // AIR_INITIAL / AIR_MIDDLE / AIR_FINAL / SMT / VERDICT) out to the
    // browser as soon as it's formatted. Same survivability reasoning as
    // `verus_diagnostic`: a later stage that traps the wasm instance
    // (rustc's `abort_if_errors` → `unreachable`) would otherwise discard
    // the whole returned String, hiding every section we'd already built.
    //
    // Content is passed as two parallel arrays describing ordered blocks
    // that JS concatenates into one body: `contents[i]` is the block
    // text, `folds[i]` is 1 when the block should auto-fold on render.
    // No JS-inserted chrome — the natural `;;` comments that AIR / Verus
    // already emit (`;; AIR prelude`, `;; Function-Def foo`, the
    // explorer-inserted `;; vstd` separator on VIR / SST) serve as the
    // visible first line of each block, and the fold range is
    // [end-of-first-line, end-of-block]. Rust owns all section boundary
    // decisions; JS only concatenates and folds.
    #[wasm_bindgen(js_name = verus_dump)]
    pub(crate) fn verus_dump(section: &str, contents: Vec<String>, folds: Vec<u8>);

    // Stage-level timing. `time()` emits one call per stage with the elapsed
    // ms. `public/app.js` and `tests/smoke.rs` both install a stub on
    // globalThis (the former logs to console, the latter to stderr). Kept
    // out-of-band from `verus_dump` so timings don't clutter the UI output
    // sections.
    #[wasm_bindgen(js_namespace = performance, js_name = now)]
    pub fn perf_now() -> f64;

    #[wasm_bindgen(js_name = verus_bench)]
    pub(crate) fn verus_bench(label: &str, ms: f64);

    // Stamp a `;; <label>` banner into the Z3 response buffer. Called from
    // `run_queries` right before each op's commands are fed to Z3 so the
    // replies (sat / unsat / empty / errors) read as per-op stanzas in the
    // Z3 tab instead of a flat positional stream.
    #[wasm_bindgen(js_name = verus_z3_annotate)]
    pub(crate) fn verus_z3_annotate(label: &str);
}
