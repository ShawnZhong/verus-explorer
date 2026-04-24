// verus-explorer — browser-based exploration of Verus's internal representations.
//
// Compiles `vir` and `air` (as-is, via path dependencies) to wasm32 and
// exposes a wasm-bindgen entry point that runs the rustc front-end on
// Rust source, lowers HIR → simplified VIR, and drives the krate through
// ast_to_sst → poly → sst_to_air → `air::context::Context`. SMT is
// routed through the wasm32 `SmtProcess` shim in `air/src/smt_process.rs`,
// which calls the `Z3_*` wrappers installed by `public/app.js` on top of
// the self-hosted single-threaded Z3 wasm.
//
// `rustc_*` crates are not Cargo deps — they're built as wasm32 rlibs by
// the `rustc-rlibs` workspace member and resolved at link time via the
// `-L dependency=...` rustflag in `.cargo/config.toml`. `extern crate`
// declarations at this crate root propagate to every submodule.
//
// Module layout:
//   externs         — JS externs the host provides.
//   wasm_libs       — in-memory filesystem for rustc's crate locator,
//                     backing the `wasm_libs_*` + `set_std_mode`
//                     wasm-bindgen entry points.
//   util            — `time`, `emit_section`, `Block` / `Section`,
//                     `push_item` / `push_banner` — used by every stage.
//   rustc_stage     — Stage 1-2: `build_rustc_config`, diagnostic
//                     plumbing, AST / HIR dumpers.
//   vir_stage       — Stage 3: `build_vir` + vstd deserialize cache.
//   verify_stage    — Stage 4: SST → AIR → SMT → Z3 driver (the bulk).
//   pipeline        — `run_pipeline` dispatcher that sequences the four
//                     stage modules, plus `dump_vir_and_verify` bridge.

#![feature(rustc_private)]

extern crate rustc_ast;
extern crate rustc_hir;
extern crate rustc_ast_pretty;
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_hir_pretty;
extern crate rustc_interface;
extern crate rustc_metadata;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

use wasm_bindgen::prelude::*;

mod externs;
mod pipeline;
mod rustc_stage;
mod util;
mod verify_stage;
mod vir_stage;
mod wasm_libs;

pub use externs::perf_now;
pub use wasm_libs::{set_std_mode, wasm_libs_add_file, wasm_libs_finalize};
use externs::console_error;
use pipeline::run_pipeline;
use rustc_stage::build_rustc_config;

// `#[wasm_bindgen(start)]` fires when this crate is the final cdylib (the
// browser build via `wasm-pack build`). Integration tests link us as an
// rlib into `wasm-bindgen-test`'s own cdylib, so the start hook doesn't
// run there — those tests call `init()` explicitly.
#[wasm_bindgen(start)]
pub fn init() {
    std::panic::set_hook(Box::new(|info| console_error(&info.to_string())));
    // rustc-in-wasm has no dlopen, so the normal `dlsym_proc_macros` path
    // in `rustc_metadata::creader` can't load `_rustc_proc_macro_decls_*`
    // from a host dylib. Both verus macro crates are regular rlibs (not
    // `proc-macro = true`) exposing `pub macro NAME` shim stubs for name
    // resolution plus a `MACROS` descriptor slice for expansion. Registering
    // swaps each stub's kind via the patched
    // `rustc_resolve::build_reduced_graph::get_macro_by_def_id` path.
    rustc_metadata::proc_macro_registry::register(
        "verus_builtin_macros",
        verus_builtin_macros::MACROS,
    );
    rustc_metadata::proc_macro_registry::register(
        "verus_state_machines_macros",
        verus_state_machines_macros::MACROS,
    );
}

/// Parse `src` via rustc_interface, force HIR lowering, build VIR, then drive
/// the krate through the AIR + Z3 pipeline. Pipeline output is streamed out
/// to the host (browser / test runner) section-by-section via the
/// `verus_dump` / `verus_diagnostic*` / `verus_bench` JS externs — no
/// return value is threaded through.
#[wasm_bindgen]
pub fn verify(src: &str, expand_errors: bool) {
    // vstd is wired into the extern prelude via `--extern=vstd` in
    // `build_rustc_config`, so the user's source is passed through unmodified.
    // Keeping the source 1:1 with what the editor shows is what lets
    // diagnostic line numbers land on the right editor line.
    let src = src.to_string();
    // wasm32 has no unwinding (panic = abort), so `catch_unwind` would be a
    // no-op here — any panic aborts the instance before this returns.
    // Partial state the pipeline already handed off via `verus_dump` /
    // `verus_diagnostic` stays in the host, which is the whole survivability
    // story.
    rustc_interface::interface::run_compiler(build_rustc_config(src), |compiler| {
        run_pipeline(compiler, expand_errors);
    });
}

