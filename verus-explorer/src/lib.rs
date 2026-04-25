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
//   wasm    — wasm↔JS boundary: host-provided JS externs + the
//             in-memory filesystem that backs rustc's crate
//             locator (`wasm_libs_*` + `set_std_mode` entries).
//   util    — `time`, `emit_section`, `Block` / `Section`,
//             `push_item` / `push_banner` — used by every stage.
//   rust    — Stages 1-2 (rust-side): `build_rustc_config`,
//             diagnostic plumbing, AST / HIR dumpers.
//   verus   — Stages 3-4 (verus-side): HIR → VIR (`build_vir` +
//             vstd deserialize cache) and VIR → SST → AIR → SMT
//             → Z3 driver (the bulk).
//
// `verify` (below) is the single wasm-bindgen entry that sequences
// the stages: rustc_parse → dump_ast_pre_expansion → (inside
// create_and_enter_global_ctxt) dump_ast → dump_hir → build_vir →
// dump_vir → verify_simplified_krate → VerifyOutput::write.

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

mod rust;
mod util;
mod verus;
mod wasm;

pub use wasm::{perf_now, set_std_mode, wasm_libs_add_file, wasm_libs_finalize};
use rust::{build_rustc_config, dump_ast, dump_ast_pre_expansion, dump_hir};
use util::time;
use verus::{VerifyOutput, build_vir, dump_vir, verify_simplified_krate};
use wasm::console_error;

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
    //
    // Errors from `build_vir` / `verify_simplified_krate` have already been
    // routed through the vendored `build_vir_crate` reporter (verifier.rs
    // ~L2142) and the per-module reporter → DiagCtxt → HumanEmitter path in
    // `verify_stage`, so they land in the DIAGNOSTICS section — we just
    // swallow the `Result`s here and emit whatever we accumulated.
    rustc_interface::interface::run_compiler(build_rustc_config(src), |compiler| {
        let krate = time("rustc_parse", || rustc_interface::passes::parse(&compiler.sess));
        // Pretty-print the parser output — essentially verbatim source wrapped
        // in `verus! { ... }` (plus the implicit `no_std` / register_tool
        // attributes we injected via `-Zcrate-attr`). Dump before
        // `create_and_enter_global_ctxt` consumes `krate`, so the UI has a
        // before/after pair against the expanded AST showing what the `verus!`
        // macro actually rewrites into.
        dump_ast_pre_expansion(&krate);
        // `create_and_enter_global_ctxt` itself is cheap (~1ms); the expensive
        // work runs lazily via `tcx` queries inside the closure. `dump_ast` is
        // the first thing to call `tcx.resolver_for_lowering()`, which drives
        // `passes::resolver_for_lowering_raw` → `configure_and_expand` —
        // i.e., the `verus!` / `requires!` / `ensures!` / `proof!`
        // proc-macros. That cost lands in `dump.ast`.
        rustc_interface::create_and_enter_global_ctxt(compiler, krate, |tcx| {
            dump_ast(tcx);
            dump_hir(tcx);
            let Ok((raw_krate, krate, global_ctx, crate_name, spans)) =
                time("build_vir", || build_vir(compiler, tcx))
            else {
                return;
            };
            dump_vir(&raw_krate, &krate);
            // `output` threaded in by-ref so dumps from earlier pipeline
            // stages survive a later failure — upstream Verus bails with `?`
            // on the first module error, which would otherwise discard every
            // SST / AIR / SMT section accumulated and leave the UI showing
            // only VIR.
            let mut output = VerifyOutput::default();
            let _ = time("verify", || {
                verify_simplified_krate(
                    krate, global_ctx, crate_name, compiler, &spans, expand_errors, &mut output,
                )
            });
            output.write();
        });
    });
}

