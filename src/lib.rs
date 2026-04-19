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
#![feature(proc_macro_internals)]

extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_interface;
extern crate rustc_metadata;
extern crate rustc_middle;
extern crate rustc_proc_macro;
extern crate rustc_session;
extern crate rustc_span;

mod pipeline;
mod proc_macros;
mod sysroot;

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    pub(crate) fn console_error(msg: &str);
}

#[wasm_bindgen(start)]
fn init() {
    std::panic::set_hook(Box::new(|info| console_error(&info.to_string())));
    sysroot::install();
    proc_macros::install();
}

/// Run the rustc front-end on `src`, lower HIR → simplified VIR, then drive
/// the krate through the AIR generation + Z3 pipeline. Returns a multi-section
/// string of AST/HIR/VIR/AIR + verdicts so the existing UI can split and
/// display each section.
#[wasm_bindgen]
pub fn parse_source(src: &str) -> String {
    pipeline::parse_source(src)
}
