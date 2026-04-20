// In-process proc-macro registration.
//
// rustc-in-wasm has no dlopen, so the normal `dlsym_proc_macros` path in
// `rustc_metadata::creader` can't load `_rustc_proc_macro_decls_*` from a host
// dylib. Both verus macro crates are regular rlibs (not `proc-macro = true`)
// exposing `pub macro NAME` shim stubs for name resolution plus a `MACROS`
// descriptor slice for expansion. Registering swaps each stub's kind via the
// patched `rustc_resolve::build_reduced_graph::get_macro_by_def_id` path.

pub fn install() {
    rustc_metadata::proc_macro_registry::register(
        "verus_builtin_macros",
        verus_builtin_macros::MACROS,
    );
    rustc_metadata::proc_macro_registry::register(
        "verus_state_machines_macros",
        verus_state_machines_macros::MACROS,
    );
}
