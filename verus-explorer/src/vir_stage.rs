// Stage 3: HIR → VIR.
//
// `build_vir` drives rust_verify's HIR-to-VIR lowering (vendored
// `build_vir_crate` addition) and returns a fully simplified krate ready
// for the Stage 4 verify driver. `vstd_krate` handles the vstd side:
// we feed in the pre-serialized VIR krate from `wasm_libs_vstd_vir()`
// and cache the deserialized form so it costs ~nothing on warm parses.
// `dump_vir` walks the finished krate and emits the VIR output tab.

use std::sync::{Arc, OnceLock};

use rust_verify::cargo_verus_dep_tracker::DepTracker;
use rust_verify::config::ArgsX;
use rust_verify::import_export::CrateWithMetadata;
use rust_verify::spans::SpanContext;
use rust_verify::verifier::Verifier;
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;
use rustc_span::def_id::LOCAL_CRATE;
use vir::ast::{Krate, VirErr};
use vir::context::GlobalCtx;

use crate::util::{Section, emit_section, push_banner, push_item, time};
use crate::wasm_libs::wasm_libs_vstd_vir;


// Drives Verus's HIR→VIR pipeline. `Verifier::build_vir_crate` (vendored
// addition) derives the inputs `construct_vir_crate` needs from (tcx, compiler),
// runs HIR → raw VIR, then the head of `verify_crate_inner` (GlobalCtx +
// check_traits + ast_simplify), returning both the simplified krate and the
// (mutated) GlobalCtx so we can drive the downstream prune → Ctx →
// ast_to_sst → AIR pipeline ourselves.
pub(crate) fn build_vir<'tcx>(
    compiler: &Compiler,
    tcx: TyCtxt<'tcx>,
) -> Result<(Krate, GlobalCtx, Arc<String>, SpanContext), Vec<VirErr>> {
    let mut args = ArgsX::new();
    // `Vstd::Imported` is the default and matches the user's
    // `extern crate vstd;` injection. The vstd VIR is served out of the
    // fetched libs bundle (`wasm_libs_vstd_vir()`) and passed straight
    // in as `other_vir_crates` — `args.import` is path-based and doesn't
    // work on wasm32, so we bypass the filesystem loader.
    // Only non-default override: skip the Polonius-based lifetime check
    // (wasm has no std::thread, and the lifetime pass isn't wasm-friendly).
    // All other knobs — `no_external_by_default`, `no_auto_recommends_check`,
    // etc. — stay at `ArgsX::new()` defaults, matching `cargo verify`. That
    // turns on auto-recommends-on-failure (the `retry_with_recommends` call
    // in `run_queries` below fires without further flag-wrangling).
    args.no_lifetime = true;
    let crate_name = Arc::new(tcx.crate_name(LOCAL_CRATE).as_str().to_owned());
    let vstd_krate = time("build_vir.vstd_deserialize", || vstd_krate())?;
    let (krate, global_ctx, spans) = time("build_vir.build_vir_crate", || {
        Verifier::new(Arc::new(args), None, false, DepTracker::init())
            .build_vir_crate(compiler, tcx, vec!["vstd".to_string()], vec![vstd_krate])
    })?;
    Ok((krate, global_ctx, crate_name, spans))
}

// Deserialize-once cache for the bundled vstd VIR. `bincode::deserialize` of
// the ~20 MB `vstd.vir` is the single biggest substage inside `build_vir`
// (~55% in debug builds, ~135ms of the 244ms steady-state in release).
// `Krate` is `Arc<KrateX>`, so cloning from the cache is an O(1) refcount
// bump. Wasm is single-threaded — no contention on the OnceLock.
static VSTD_KRATE: OnceLock<vir::ast::Krate> = OnceLock::new();

fn vstd_krate() -> Result<vir::ast::Krate, Vec<VirErr>> {
    if let Some(k) = VSTD_KRATE.get() {
        return Ok(k.clone());
    }
    let CrateWithMetadata { krate, .. } = bincode::deserialize(wasm_libs_vstd_vir())
        .map_err(|_| vec![vir::messages::error_bare(
            "failed to deserialize embedded VIR crate — version mismatch?",
        )])?;
    let _ = VSTD_KRATE.set(krate.clone());
    Ok(krate)
}

// Walk the simplified VIR krate and emit the VIR output tab.
// Per-item blocks are folded when they come from external crates
// (vstd, etc.); see `push_item` for the merge rule.
pub(crate) fn dump_vir(krate: &Krate) {
    time("dump.vir", || {
        use vir::printer::WalkEvent;
        let mut blocks = Vec::new();
        vir::printer::walk_krate(krate, &vir::printer::COMPACT_TONODEOPTS, |event| match event {
            WalkEvent::Section(name) => push_banner(&mut blocks, name),
            WalkEvent::Item { krate, span, text } => push_item(&mut blocks, krate, span, text),
        });
        emit_section(Section { name: "VIR", blocks });
    });
}
