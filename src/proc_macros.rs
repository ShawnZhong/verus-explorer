// In-process proc-macro descriptors for `verus_builtin_macros`.
//
// rustc-in-wasm has no dlopen, so the normal `dlsym_proc_macros` path in
// `rustc_metadata::creader` can't load `_rustc_proc_macro_decls_*` from a host
// dylib. Task 21 added an override: `proc_macro_registry::register` stores a
// `&'static [ProcMacro]` keyed by crate name, and `register_crate` consults it
// before falling through to dlsym. Here we build that slice for
// `verus_builtin_macros` by pointing each entry directly at the matching
// helper exposed by `verus_builtin_macros_lib`.
//
// The vendored `compiler/rustc_proc_macro` crate is a shim that re-exports the
// sysroot `proc_macro`, so `rustc_proc_macro::TokenStream` and the
// `proc_macro::TokenStream` that `vbml::*` uses are the same type — no
// conversion (and therefore no span loss) at the boundary.

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

// Order MUST match the source order of proc-macro items in
// `verus_builtin_macros/src/lib.rs` (including `decl_derive!`/`decl_attribute!`
// expansions, which emit a `#[proc_macro_*]` item at the invocation site).
// rustc's metadata encoder writes the proc-macro DefIds in that order into
// `proc_macro_data.macros`, and at expansion time `raw_proc_macro(id)` looks
// up this slice by the position of `id` in that list. A mismatched order
// swaps macro kinds across entries and yields E0658 / "expected macro, found
// <kind> `name`" at use sites.
static MACROS: &[ProcMacro] = &[
    ProcMacro::custom_derive("Structural", &[], vbml::derive_structural),
    ProcMacro::custom_derive("StructuralEq", &[], vbml::derive_structural_eq),
    ProcMacro::attr("is_variant", vbml::attribute_is_variant),
    ProcMacro::attr(
        "is_variant_no_deprecation_warning",
        vbml::attribute_is_variant_no_deprecation_warning,
    ),
    ProcMacro::attr("verus_enum_synthesize", vbml::verus_enum_synthesize),
    ProcMacro::bang("fndecl", vbml::fndecl),
    ProcMacro::bang("verus_keep_ghost", vbml::verus_keep_ghost),
    ProcMacro::bang("verus_erase_ghost", vbml::verus_erase_ghost),
    ProcMacro::bang("verus", vbml::verus),
    ProcMacro::bang("verus_impl", vbml::verus_impl),
    ProcMacro::bang("verus_trait_impl", vbml::verus_trait_impl),
    ProcMacro::bang("verus_proof_expr", vbml::verus_proof_expr),
    ProcMacro::bang("verus_exec_expr_keep_ghost", vbml::verus_exec_expr_keep_ghost),
    ProcMacro::bang("verus_exec_expr_erase_ghost", vbml::verus_exec_expr_erase_ghost),
    ProcMacro::bang("verus_exec_expr", vbml::verus_exec_expr),
    ProcMacro::bang("verus_proof_macro_exprs", vbml::verus_proof_macro_exprs),
    ProcMacro::bang("verus_exec_macro_exprs", vbml::verus_exec_macro_exprs),
    ProcMacro::bang("verus_exec_inv_macro_exprs", vbml::verus_exec_inv_macro_exprs),
    ProcMacro::bang("verus_ghost_inv_macro_exprs", vbml::verus_ghost_inv_macro_exprs),
    ProcMacro::bang("verus_proof_macro_explicit_exprs", vbml::verus_proof_macro_explicit_exprs),
    ProcMacro::bang("struct_with_invariants", vbml::struct_with_invariants),
    ProcMacro::bang("atomic_with_ghost_helper", vbml::atomic_with_ghost_helper),
    ProcMacro::bang("calc_proc_macro", vbml::calc_proc_macro),
    ProcMacro::attr("verus_verify", vbml::verus_verify),
    ProcMacro::attr("verus_spec", vbml::verus_spec),
    ProcMacro::bang("proof_with", vbml::proof_with),
    ProcMacro::bang("proof", vbml::proof),
    ProcMacro::bang("proof_decl", vbml::proof_decl),
    ProcMacro::bang("set_build", vbml::set_build),
    ProcMacro::bang("set_build_debug", vbml::set_build_debug),
    ProcMacro::attr("auto_spec", vbml::auto_spec),
    ProcMacro::bang("exec_spec_verified", vbml::exec_spec_verified),
    ProcMacro::bang("exec_spec_unverified", vbml::exec_spec_unverified),
    ProcMacro::attr("make_spec_type", vbml::make_spec_type),
    ProcMacro::attr("self_view", vbml::self_view),
];

pub fn install() {
    rustc_metadata::proc_macro_registry::register(CRATE_NAME, MACROS);
    rustc_metadata::proc_macro_registry::register(STATE_MACHINES_CRATE_NAME, STATE_MACHINES_MACROS);
}
