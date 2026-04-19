// In-process proc-macro descriptors for `verus_builtin_macros`.
//
// rustc-in-wasm has no dlopen, so the normal `dlsym_proc_macros` path in
// `rustc_metadata::creader` can't load `_rustc_proc_macro_decls_*` from a host
// dylib. Task 21 added an override: `proc_macro_registry::register` stores a
// `&'static [ProcMacro]` keyed by crate name, and `register_crate` consults it
// before falling through to dlsym. Here we build that slice for
// `verus_builtin_macros` by wrapping every pm2-typed helper exposed by
// `verus_builtin_macros_lib` (the regular library mirror from Task 22) in a
// closure that converts pm ↔ pm2 around the call.
//
// The rmeta for `verus_builtin_macros` still needs to be discoverable through
// the virtual sysroot so rustc sees it as a proc-macro crate; this module only
// supplies the expansion functions once rustc resolves the crate.

use std::str::FromStr;

use rustc_proc_macro::TokenStream as PmTokenStream;
use rustc_proc_macro::bridge::client::ProcMacro;
use verus_builtin_macros_lib as vbml;

const CRATE_NAME: &str = "verus_builtin_macros";

// `verus_state_machines_macros` is listed as a vstd dep in vstd.rmeta, so the
// crate loader tries to resolve it whenever vstd is loaded. We bundle its
// rmeta (see build.rs) for name-resolution purposes, but no user-facing
// example invokes its macros — register an empty set here so the creader's
// proc-macro lookup hits the registry and skips the dlopen fallback.
const STATE_MACHINES_CRATE_NAME: &str = "verus_state_machines_macros";
static STATE_MACHINES_MACROS: &[ProcMacro] = &[];

// The rustc-rlibs `rustc_proc_macro` crate and the wasm32-sysroot `proc_macro`
// crate share a source but are distinct crate instances, so their TokenStream
// types are unrelated — there is no `From` / `Into` between them. Meanwhile
// `proc_macro2` wraps the sysroot `proc_macro`. We bridge the two worlds via
// Display+FromStr on each side. This loses spans and hygiene (the tokens are
// round-tripped through strings), which is acceptable for the verus!{}
// expansion surface: its diagnostics point at user source, which survives
// intact through the stringify→reparse because the reparsed tokens carry
// call-site spans the bridge assigns.
fn pm_to_pm2(ts: PmTokenStream) -> proc_macro2::TokenStream {
    proc_macro2::TokenStream::from_str(&ts.to_string())
        .expect("rustc_proc_macro TokenStream must reparse as proc_macro2")
}

fn pm2_to_pm(ts: proc_macro2::TokenStream) -> PmTokenStream {
    PmTokenStream::from_str(&ts.to_string())
        .expect("proc_macro2 TokenStream must reparse as rustc_proc_macro")
}

macro_rules! bang {
    ($name:literal => $fn_path:path) => {
        ProcMacro::bang($name, |input: PmTokenStream| {
            pm2_to_pm($fn_path(pm_to_pm2(input)))
        })
    };
}

macro_rules! attr {
    ($name:literal => $fn_path:path) => {
        ProcMacro::attr($name, |args: PmTokenStream, input: PmTokenStream| {
            pm2_to_pm($fn_path(pm_to_pm2(args), pm_to_pm2(input)))
        })
    };
}

macro_rules! derive {
    ($name:literal, $attrs:expr => $fn_path:path) => {
        ProcMacro::custom_derive($name, $attrs, |input: PmTokenStream| {
            pm2_to_pm($fn_path(pm_to_pm2(input)))
        })
    };
}

// Order MUST match the source order of proc-macro items in
// `verus_builtin_macros/src/lib.rs` (including `decl_derive!`/`decl_attribute!`
// expansions, which emit a `#[proc_macro_*]` item at the invocation site).
// rustc's metadata encoder writes the proc-macro DefIds in that order into
// `proc_macro_data.macros`, and at expansion time `raw_proc_macro(id)` looks
// up this slice by the position of `id` in that list. A mismatched order
// swaps macro kinds across entries and yields E0658 / "expected macro, found
// <kind> `name`" at use sites.
static MACROS: &[ProcMacro] = &[
    derive!("Structural", &[] => vbml::derive_structural),
    derive!("StructuralEq", &[] => vbml::derive_structural_eq),
    attr!("is_variant" => vbml::attribute_is_variant),
    attr!("is_variant_no_deprecation_warning" => vbml::attribute_is_variant_no_deprecation_warning),
    attr!("verus_enum_synthesize" => vbml::verus_enum_synthesize),
    bang!("fndecl" => vbml::fndecl),
    bang!("verus_keep_ghost" => vbml::verus_keep_ghost),
    bang!("verus_erase_ghost" => vbml::verus_erase_ghost),
    bang!("verus" => vbml::verus),
    bang!("verus_impl" => vbml::verus_impl),
    bang!("verus_trait_impl" => vbml::verus_trait_impl),
    bang!("verus_proof_expr" => vbml::verus_proof_expr),
    bang!("verus_exec_expr_keep_ghost" => vbml::verus_exec_expr_keep_ghost),
    bang!("verus_exec_expr_erase_ghost" => vbml::verus_exec_expr_erase_ghost),
    bang!("verus_exec_expr" => vbml::verus_exec_expr),
    bang!("verus_proof_macro_exprs" => vbml::verus_proof_macro_exprs),
    bang!("verus_exec_macro_exprs" => vbml::verus_exec_macro_exprs),
    bang!("verus_exec_inv_macro_exprs" => vbml::verus_exec_inv_macro_exprs),
    bang!("verus_ghost_inv_macro_exprs" => vbml::verus_ghost_inv_macro_exprs),
    bang!("verus_proof_macro_explicit_exprs" => vbml::verus_proof_macro_explicit_exprs),
    bang!("struct_with_invariants" => vbml::struct_with_invariants),
    bang!("atomic_with_ghost_helper" => vbml::atomic_with_ghost_helper),
    bang!("calc_proc_macro" => vbml::calc_proc_macro),
    attr!("verus_verify" => vbml::verus_verify),
    attr!("verus_spec" => vbml::verus_spec),
    bang!("proof_with" => vbml::proof_with),
    bang!("proof" => vbml::proof),
    bang!("proof_decl" => vbml::proof_decl),
    bang!("set_build" => vbml::set_build),
    bang!("set_build_debug" => vbml::set_build_debug),
    attr!("auto_spec" => vbml::auto_spec),
    bang!("exec_spec_verified" => vbml::exec_spec_verified),
    bang!("exec_spec_unverified" => vbml::exec_spec_unverified),
    attr!("make_spec_type" => vbml::make_spec_type),
    attr!("self_view" => vbml::self_view),
];

pub fn install() {
    rustc_metadata::proc_macro_registry::register(CRATE_NAME, MACROS);
    rustc_metadata::proc_macro_registry::register(STATE_MACHINES_CRATE_NAME, STATE_MACHINES_MACROS);
}
