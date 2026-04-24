// Orchestrates the rustc_interface driver and the per-stage dispatchers.
// `run_pipeline` is the single entry from `verify()` — it forces rustc
// parsing, emits the pre-expansion AST dump, then enters the global
// context and walks the four stage modules in order:
//
//   dump_ast → dump_hir → (build_vir → dump_vir) → verify_simplified_krate
//
// Any `VirErr` that escapes `build_vir` has already been routed through
// the vendored `build_vir_crate`'s reporter (verifier.rs ~L2142),
// matching upstream's `after_expansion` handler. Errors from
// `verify_simplified_krate` likewise go through the reporter → DiagCtxt
// and land in the DIAGNOSTICS section — so we just swallow the `Result`
// here and emit whatever we managed to accumulate.

use crate::rustc_stage::{dump_ast, dump_ast_pre_expansion, dump_hir};
use crate::util::time;
use crate::verify_stage::{VerifyOutput, verify_simplified_krate, write_verify_output};
use crate::vir_stage::{build_vir, dump_vir};

pub(crate) fn run_pipeline(compiler: &rustc_interface::interface::Compiler, expand_errors: bool) {
    let krate = time("rustc_parse", || rustc_interface::passes::parse(&compiler.sess));
    // Parser output — pretty-prints essentially verbatim source wrapped in
    // `verus! { ... }` (plus the implicit `no_std` / register_tool
    // attributes we injected via `-Zcrate-attr`). Dumping it here, before
    // `create_and_enter_global_ctxt` moves `krate`, gives the UI a
    // before/after pair against the expanded AST so the reader can see
    // what the `verus!` macro actually rewrites into.
    dump_ast_pre_expansion(&krate);
    // `create_and_enter_global_ctxt` itself is cheap (~1ms); the expensive
    // work runs lazily via `tcx` queries inside the closure. `dump_ast` is
    // the first thing to call `tcx.resolver_for_lowering()`, which drives
    // `passes::resolver_for_lowering_raw` → `configure_and_expand` —
    // i.e., the `verus!` / `requires!` / `ensures!` / `proof!` proc-macros.
    // That cost is attributed to `dump.ast` (no separate timer needed).
    rustc_interface::create_and_enter_global_ctxt(compiler, krate, |tcx| {
        dump_ast(tcx);
        dump_hir(tcx);
        let Ok((krate, global_ctx, crate_name, spans)) =
            time("build_vir", || build_vir(compiler, tcx))
        else {
            return;
        };
        dump_vir(&krate);
        // `output` threaded in by-ref so dumps from earlier modules /
        // pipeline stages survive a later failure — upstream Verus
        // bails with `?` on the first module error, which would
        // otherwise discard every SST / AIR / SMT section accumulated
        // up to that point and leave the UI showing only VIR.
        let mut output = VerifyOutput::default();
        let _ = time("verify", || {
            verify_simplified_krate(
                krate, global_ctx, crate_name, compiler, &spans, expand_errors, &mut output,
            )
        });
        write_verify_output(output);
    });
}
