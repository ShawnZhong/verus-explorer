// In-process proc-macro registration.
//
// rustc-in-wasm has no dlopen, so the normal `dlsym_proc_macros` path in
// `rustc_metadata::creader` can't load `_rustc_proc_macro_decls_*` from a host
// dylib. The patched `proc_macro_registry::register` lets us instead hand
// rustc a `&'static [ProcMacro]` keyed by crate name, which `register_crate`
// consults before falling through to dlsym.
//
// `verus_builtin_macros_lib::MACROS` is the matching descriptor slice — it
// lives next to the function definitions in the verus tree so the source
// order coupling (rustc indexes this slice by DefId position) is local to one
// crate.
//
// `verus_state_machines_macros` is listed as a vstd dep in vstd.rmeta, so the
// crate loader tries to resolve it whenever vstd is loaded. We bundle its
// rmeta (see build.rs) for name-resolution purposes, but no user-facing
// example invokes its macros — register an empty set so the lookup hits the
// registry and skips the dlopen fallback.

pub fn install() {
    rustc_metadata::proc_macro_registry::register(
        "verus_builtin_macros",
        verus_builtin_macros_lib::MACROS,
    );
    rustc_metadata::proc_macro_registry::register("verus_state_machines_macros", &[]);
}
