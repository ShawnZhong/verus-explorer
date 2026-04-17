// Drives Verus's HIR→VIR pipeline. `Verifier::build_vir_crate` (vendored
// addition) derives the inputs construct_vir_crate needs from (tcx, compiler),
// runs HIR → raw VIR, then the head of `verify_crate_inner` (GlobalCtx +
// check_traits + ast_simplify). Stops before bucket/AIR/SMT work, so the
// krate is ready for downstream ast_to_sst → poly → sst_to_air lowering.

use std::sync::Arc;

use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;

use rust_verify::cargo_verus_dep_tracker::DepTracker;
use rust_verify::config::{ArgsX, Vstd};
use rust_verify::verifier::Verifier;

use vir::ast::{Krate, VirErr};

pub fn build_vir<'tcx>(compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Result<Krate, Vec<VirErr>> {
    let mut args = ArgsX::new();
    args.vstd = Vstd::NoVstd;
    args.no_lifetime = true;
    args.no_verify = true;
    args.no_external_by_default = true;
    Verifier::new(Arc::new(args), None, false, DepTracker::init()).build_vir_crate(compiler, tcx)
}
