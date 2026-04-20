// verus-explorer — browser-based exploration of Verus's internal representations.
//
// Compiles `vir` and `air` (as-is, via path dependencies) to wasm32 and exposes
// a wasm-bindgen entry point that runs the rustc front-end on Rust source,
// lowers HIR → simplified VIR, and drives the krate through ast_to_sst → poly →
// sst_to_air → `air::context::Context`. SMT is routed through the wasm32
// `SmtProcess` shim in `air/src/smt_process.rs`, which calls the `Z3_*`
// wrappers installed by `public/index.html` on top of the self-hosted
// single-threaded Z3 wasm.
//
// `rustc_*` crates are not Cargo deps — they're built as wasm32 rlibs by the
// `rustc-rlibs` workspace member and resolved at link time via the
// `-L dependency=...` rustflag in `.cargo/config.toml`.

#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_interface;
extern crate rustc_metadata;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

// `pub` (not `pub(crate)`) so integration tests in `tests/` — which link this
// crate as a plain rlib — can drive the pipeline with SMT turned off, since
// `wasm-bindgen-test`'s harness doesn't install the `Z3_*` JS shims that
// `public/index.html` installs for the browser build.
pub mod pipeline;
mod proc_macros;
mod sysroot;

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    pub(crate) fn console_error(msg: &str);

    // Imported from `public/index.html`. Called synchronously from
    // `pipeline::ConsoleWriter` so each rustc diagnostic lands in the
    // output panel before rustc's `abort_if_errors` turns into a wasm
    // `unreachable` trap (see the comment on `ConsoleWriter`).
    #[wasm_bindgen(js_name = verus_diagnostic)]
    pub(crate) fn verus_diagnostic(msg: &str);
}

// `#[wasm_bindgen(start)]` fires when this crate is the final cdylib (the
// browser build via `wasm-pack build`). Integration tests link us as an
// rlib into `wasm-bindgen-test`'s own cdylib, so the start hook doesn't
// run there — those tests call `init()` explicitly.
#[wasm_bindgen(start)]
pub fn init() {
    std::panic::set_hook(Box::new(|info| console_error(&info.to_string())));
    proc_macros::install();
}

/// Register one virtual-sysroot file (rmeta or `vstd.vir`) fetched by the
/// JS loader from `./sysroot/<name>`. Call once per manifest entry, then
/// call `sysroot_finalize` before the first `parse_source` invocation.
#[wasm_bindgen]
pub fn sysroot_add_file(name: String, bytes: Vec<u8>) {
    sysroot::add_file(name, bytes);
}

/// Freeze the registered files and wire up rustc's filesearch callbacks.
/// Must be called after all `sysroot_add_file` calls for this wasm instance.
#[wasm_bindgen]
pub fn sysroot_finalize() {
    sysroot::finalize();
}

/// Run the rustc front-end on `src`, lower HIR → simplified VIR, then drive
/// the krate through the AIR generation + Z3 pipeline. Returns a multi-section
/// `=== NAME ===` string. The verdict is always emitted; each `dump_*` flag
/// gates the corresponding intermediate-representation section.
#[wasm_bindgen]
pub fn parse_source(
    src: &str,
    dump_ast: bool,
    dump_hir: bool,
    dump_vir: bool,
    dump_air_initial: bool,
    dump_air_middle: bool,
    dump_air_final: bool,
    dump_smt: bool,
) -> String {
    pipeline::parse_source(
        src,
        pipeline::DumpStages {
            ast: dump_ast,
            hir: dump_hir,
            vir: dump_vir,
            air_initial: dump_air_initial,
            air_middle: dump_air_middle,
            air_final: dump_air_final,
            smt: dump_smt,
        },
        /* verify */ true,
    )
}
