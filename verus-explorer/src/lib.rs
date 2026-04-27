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
// Two wasm-bindgen entries below sequence the per-mode stages:
//   * `verify` — rustc_parse → dump_ast_pre_expansion → (inside
//     create_and_enter_global_ctxt) dump_ast → dump_hir → build_vir
//     → dump_vir → verify_simplified_krate → VerifyOutput::write.
//   * `run`    — rustc_parse → dump_ast_pre_expansion → (inside
//     create_and_enter_global_ctxt) dump_ast → dump_hir → dump_mir
//     → eval_entry (Miri).

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
// Miri's library entry — `eval_entry(tcx, def_id, entry_type, &config, None)`.
// `getrandom` is dragged in transitively by Miri's `rand` dep; the
// `__getrandom_v03_custom` stub in `wasm.rs` wires the custom backend
// (cfg-selected in `.cargo/config.toml`).
extern crate miri;
extern crate getrandom;

use wasm_bindgen::prelude::*;

mod rust;
mod util;
mod verus;
mod wasm;

pub use wasm::{perf_now, set_std_mode, wasm_libs_add_file, wasm_libs_finalize};
use rust::{build_rustc_config, dump_ast, dump_ast_pre_expansion, dump_hir, dump_mir, Mode};
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
pub fn verify(src: &str) {
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
    rustc_interface::interface::run_compiler(build_rustc_config(src, Mode::Verify), |compiler| {
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
                    krate, global_ctx, crate_name, compiler, &spans, &mut output,
                )
            });
            output.write();
        });
    });
}


/// Drives Miri on the user's source — interprets the program's `fn main`.
/// Separate wasm-bindgen entry from `verify` so the JS side can opt in
/// (and so the Miri call site is statically reachable, which prevents the
/// release-profile linker from DCE'ing all of Miri). Uses the same
/// `keep_ghost = false` cfg the compile-mode pass uses, so ghost code is
/// erased at macro expansion before MIR-building. Returns silently when
/// the source has no entry function (e.g., a verify-only library); any
/// Miri-reported exit code is currently dropped.
#[wasm_bindgen]
pub fn run(src: &str) {
    let src = src.to_string();
    // `crate_type = "bin"` flips rustc into binary-crate mode so
    // `tcx.entry_fn(())` resolves the user's `fn main` as the entry.
    // The verify / compile-mode passes use `lib` because we never want
    // rustc to require an entry function for those.
    rustc_interface::interface::run_compiler(
        build_rustc_config(src, Mode::Execute),
        |compiler| {
            let krate = time("run.parse", || rustc_interface::passes::parse(&compiler.sess));
            // Populate the Rust IR tabs (AST_PRE / AST / HIR / HIR_TYPED)
            // with the post-erasure view — what `verus --compile` sees,
            // and what Miri actually interprets. Verify mode populates
            // them with the ghost-present view via the same dumpers in
            // `verify`; user toggling between modes swaps which view
            // shows up.
            dump_ast_pre_expansion(&krate);
            rustc_interface::create_and_enter_global_ctxt(compiler, krate, |tcx| {
                dump_ast(tcx);
                dump_hir(tcx);
                dump_mir(tcx);
                // Skip Miri when its preconditions aren't met — both
                // would otherwise `tcx.dcx().fatal(...)` and trap the
                // wasm instance (panic=abort on wasm32, no recovery).
                // Underlying rustc errors ("can't find crate for
                // `std`", "main function not found in crate") have
                // already streamed through the diagnostics pane.
                if tcx.lang_items().start_fn().is_some()
                    && let Some((entry_id, entry_ty)) = tcx.entry_fn(())
                {
                    // Trade soundness checks for interactive speed —
                    // we're a browser playground, not a UB-finding tool.
                    // Mirrors Rubri's `-Zmiri-disable-validation` style
                    // setup. StackedBorrows alone can add 5-10× to
                    // simple programs; data race + weak memory each
                    // add another constant factor. Leak backtraces
                    // are off for a similar reason.
                    let mut config = miri::MiriConfig::default();
                    config.validation = miri::ValidationMode::No;
                    config.borrow_tracker = None;
                    config.check_alignment = miri::AlignmentCheck::None;
                    config.data_race_detector = false;
                    config.weak_memory_emulation = false;
                    config.collect_leak_backtraces = false;
                    config.ignore_leaks = true;
                    config.preemption_rate = 0.0;
                    config.fixed_scheduling = true;
                    let result = time("run.miri", || {
                        miri::eval_entry(tcx, entry_id, miri::MiriEntryFnType::Rustc(entry_ty), &config, None)
                    });
                    let summary = match result {
                        Ok(()) => "[run] program exited with code 0\n".to_string(),
                        Err(code) => format!("[run] program exited with code {}\n", code.get()),
                    };
                    wasm::verus_run_stderr(&summary);
                }
                // `run_compiler` calls `dcx().abort_if_errors()` after
                // this closure returns — which panics on wasm32 if any
                // diagnostic raised an error during compilation (e.g.
                // a parse error in the user's source). Reset the
                // counter so we exit cleanly. The actual error text
                // already flowed through the JsonEmitter to
                // `verus_diagnostic`, so the user sees it.
                tcx.dcx().reset_err_count();
            });
        },
    );
}

