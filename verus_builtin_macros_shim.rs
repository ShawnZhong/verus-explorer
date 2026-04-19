// Shim source fed to `build.rs`'s host-rustc subprocess to emit
// `libverus_builtin_macros-explorer.rmeta` for the virtual sysroot.
//
// It is *not* part of the regular Cargo build: Cargo builds
// `verus_builtin_macros_lib` (as a normal wasm32 rlib, for in-process use by
// `src/proc_macros.rs`), and Cargo's proc-macro `verus_builtin_macros` is
// skipped entirely because it can't build for wasm32. The rmeta produced here
// serves a different need: it satisfies rustc-in-wasm's crate locator when
// *user code* does `extern crate verus_builtin_macros;` — and, once resolved,
// lets each `ext = use verus_builtin_macros::<name>;` reference a valid macro
// def in the crate's namespace.
//
// The stubs are *plain* `pub macro` defs with no `#[rustc_builtin_macro]`
// attribute, because the host rustc that produces this rmeta doesn't know the
// names and would reject the attribute with "cannot find a built-in macro". At
// the user-code expand site, rustc-in-wasm hits our patched
// `rustc_resolve::build_reduced_graph::get_macro_by_def_id`, which — for any
// crate registered via `rustc_metadata::proc_macro_registry` — swaps the
// compiled `SyntaxExtensionKind` for a `Bang/Attr/Derive` wrapper around the
// matching `ProcMacro` client from the registry. So the empty bodies below are
// only a parser placeholder; they're never evaluated.
#![no_std]
#![feature(decl_macro)]

// Function-like bang macros.
pub macro fndecl($($tt:tt)*) { }
pub macro verus_keep_ghost($($tt:tt)*) { }
pub macro verus_erase_ghost($($tt:tt)*) { }
pub macro verus($($tt:tt)*) { }
pub macro verus_impl($($tt:tt)*) { }
pub macro verus_trait_impl($($tt:tt)*) { }
pub macro verus_proof_expr($($tt:tt)*) { }
pub macro verus_exec_expr_keep_ghost($($tt:tt)*) { }
pub macro verus_exec_expr_erase_ghost($($tt:tt)*) { }
pub macro verus_exec_expr($($tt:tt)*) { }
pub macro verus_proof_macro_exprs($($tt:tt)*) { }
pub macro verus_exec_macro_exprs($($tt:tt)*) { }
pub macro verus_exec_inv_macro_exprs($($tt:tt)*) { }
pub macro verus_ghost_inv_macro_exprs($($tt:tt)*) { }
pub macro verus_proof_macro_explicit_exprs($($tt:tt)*) { }
pub macro struct_with_invariants($($tt:tt)*) { }
pub macro atomic_with_ghost_helper($($tt:tt)*) { }
pub macro calc_proc_macro($($tt:tt)*) { }
pub macro proof_with($($tt:tt)*) { }
pub macro proof($($tt:tt)*) { }
pub macro proof_decl($($tt:tt)*) { }
pub macro set_build($($tt:tt)*) { }
pub macro set_build_debug($($tt:tt)*) { }
pub macro exec_spec_verified($($tt:tt)*) { }
pub macro exec_spec_unverified($($tt:tt)*) { }

// Attribute macros.
pub macro is_variant($($tt:tt)*) { }
pub macro is_variant_no_deprecation_warning($($tt:tt)*) { }
pub macro verus_enum_synthesize($($tt:tt)*) { }
pub macro verus_verify($($tt:tt)*) { }
pub macro verus_spec($($tt:tt)*) { }
pub macro auto_spec($($tt:tt)*) { }
pub macro make_spec_type($($tt:tt)*) { }
pub macro self_view($($tt:tt)*) { }

// Derive macros.
pub macro Structural($($tt:tt)*) { }
pub macro StructuralEq($($tt:tt)*) { }
