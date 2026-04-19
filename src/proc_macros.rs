// In-process proc-macro registration.
//
// rustc-in-wasm has no dlopen, so the normal `dlsym_proc_macros` path in
// `rustc_metadata::creader` can't load `_rustc_proc_macro_decls_*` from a host
// dylib. `verus_builtin_macros` is a regular rlib (not `proc-macro = true`)
// that exposes `pub macro NAME` shim stubs for name resolution plus a `MACROS`
// descriptor slice for expansion. Registering swaps each stub's kind via the
// patched `rustc_resolve::build_reduced_graph::get_macro_by_def_id` path.
//
// `verus_state_machines_macros` is still a real proc-macro crate — its rmeta
// is listed as a vstd dep so the crate loader tries to resolve it whenever
// vstd is loaded. We bundle its rmeta (see build.rs) and register an empty
// set so the creader-side registry lookup hits before the absent dlsym path.

pub fn install() {
    rustc_metadata::proc_macro_registry::register(
        "verus_builtin_macros",
        verus_builtin_macros::MACROS,
    );
    rustc_metadata::proc_macro_registry::register("verus_state_machines_macros", &[]);
}
