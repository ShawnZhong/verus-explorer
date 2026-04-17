// Hand-construct a minimal `vir::ast::Krate` for `proof fn lemma() ensures true {}`
// and drive it through `GlobalCtx::new → simplify_krate → prune → Ctx::new →
// ast_to_sst_krate → poly → func_*_to_air` to produce real AIR commands.
//
// This proves the VIR→AIR machinery runs in wasm32 — not just AIR-text-in.

use std::sync::Arc;

use air::ast::{Command, CommandX};
use air::context::SmtSolver;
use air::messages::Reporter;
use vir::ast::{
    Arch, ArchWordBits, BodyVisibility, Constant, ExprX, FunX, Function, FunctionAttrsX,
    FunctionKind, FunctionX, ItemKind, KrateX, Module, ModuleX, Mode, Opaqueness, Param, ParamX,
    Path, PathX, SpannedTyped, TypX, VarIdent, VarIdentDisambiguate, VirErr, Visibility,
};
use vir::ast_util::unit_typ;
use vir::context::{Ctx, GlobalCtx};
use vir::def::Spanned;
use vir::messages::Span;

fn no_span() -> Span {
    Span { raw_span: Arc::new(()), id: 0, data: vec![], as_string: "no_span".to_string() }
}

fn mk_path(segments: &[&str]) -> Path {
    Arc::new(PathX {
        krate: None,
        segments: Arc::new(segments.iter().map(|s| Arc::new(s.to_string())).collect()),
    })
}

fn build_lemma_function(module_path: &Path) -> Function {
    let span = no_span();
    let bool_typ = Arc::new(TypX::Bool);
    let unit = unit_typ();

    let true_expr = SpannedTyped::new(&span, &bool_typ, ExprX::Const(Constant::Bool(true)));
    let body = SpannedTyped::new(&span, &unit, ExprX::Block(Arc::new(vec![]), None));

    let ret: Param = Spanned::new(
        span.clone(),
        ParamX {
            name: VarIdent(Arc::new("_".to_string()), VarIdentDisambiguate::NoBodyParam),
            typ: unit,
            mode: Mode::Proof,
            user_mut: false,
            is_mut: false,
            unwrapped_info: None,
        },
    );

    Spanned::new(
        span,
        FunctionX {
            name: Arc::new(FunX { path: mk_path(&["root", "lemma"]) }),
            proxy: None,
            kind: FunctionKind::Static,
            visibility: Visibility { restricted_to: None },
            body_visibility: BodyVisibility::public(),
            opaqueness: Opaqueness::Opaque,
            owning_module: Some(module_path.clone()),
            mode: Mode::Proof,
            typ_params: Arc::new(vec![]),
            typ_bounds: Arc::new(vec![]),
            params: Arc::new(vec![]),
            ret,
            ens_has_return: false,
            require: Arc::new(vec![]),
            ensure: (Arc::new(vec![true_expr]), Arc::new(vec![])),
            returns: None,
            decrease: Arc::new(vec![]),
            decrease_when: None,
            decrease_by: None,
            fndef_axioms: None,
            mask_spec: None,
            unwind_spec: None,
            item_kind: ItemKind::Function,
            attrs: Arc::new(FunctionAttrsX::default()),
            body: Some(body),
            extra_dependencies: vec![],
            async_ret: None,
        },
    )
}

fn build_lemma_krate() -> vir::ast::Krate {
    let module_path = mk_path(&["root"]);
    let module: Module =
        Spanned::new(no_span(), ModuleX { path: module_path.clone(), reveals: None });
    Arc::new(KrateX {
        functions: vec![build_lemma_function(&module_path)],
        reveal_groups: vec![],
        datatypes: vec![],
        traits: vec![],
        trait_impls: vec![],
        assoc_type_impls: vec![],
        modules: vec![module],
        external_fns: vec![],
        external_types: vec![],
        path_as_rust_names: vec![],
        arch: Arch { word_bits: ArchWordBits::Either32Or64 },
        opaque_types: vec![],
    })
}

/// Result of driving the lemma Krate through the pipeline.
pub struct VirPipelineResult {
    pub commands: Vec<Command>,
    // Read by the wasm caller to build the prelude; the native test doesn't
    // exercise the prelude path, so silence dead-code on host builds.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub arch_word_bits: ArchWordBits,
    /// Stage-by-stage summary; surfaced in the UI alongside the verdict.
    pub trace: String,
}

/// Drive the lemma Krate through GlobalCtx::new → simplify_krate → prune →
/// Ctx::new → ast_to_sst_krate → poly → func_*_to_air, returning the AIR
/// commands that should be fed to `air::Context`.
pub fn run_vir_pipeline() -> Result<VirPipelineResult, VirErr> {
    use std::fmt::Write;
    let mut trace = String::new();

    let krate = build_lemma_krate();
    writeln!(trace, "[1/6] built Krate with {} function(s)", krate.functions.len()).unwrap();

    let crate_name = Arc::new("explorer".to_string());
    let mut global_ctx = GlobalCtx::new(
        &krate,
        crate_name.clone(),
        no_span(),
        /* rlimit */ 10.0,
        /* interpreter_log */ Arc::new(std::sync::Mutex::new(None)),
        /* func_call_graph_log */ Arc::new(std::sync::Mutex::new(None)),
        SmtSolver::Z3,
        /* after_simplify */ false,
        /* check_api_safety */ false,
        /* axiom_usage_info */ false,
        /* new_mut_ref */ false,
        /* no_bv_simplify */ false,
        /* report_long_running */ false,
    )?;
    writeln!(trace, "[2/6] GlobalCtx::new ok").unwrap();

    vir::recursive_types::check_traits(&krate, &global_ctx)?;
    let krate = vir::ast_simplify::simplify_krate(&mut global_ctx, &krate)?;
    writeln!(
        trace,
        "[3/6] simplify_krate ok ({} fn, {} datatypes after)",
        krate.functions.len(),
        krate.datatypes.len()
    )
    .unwrap();

    let module_path = mk_path(&["root"]);
    let (pruned_krate, prune_info) = vir::prune::prune_krate_for_module_or_krate(
        &krate,
        &crate_name,
        None,
        Some(module_path.clone()),
        None,
        true,
        true,
    );
    writeln!(
        trace,
        "[4/6] prune ok ({} fn, {} datatypes after)",
        pruned_krate.functions.len(),
        pruned_krate.datatypes.len()
    )
    .unwrap();

    let module = pruned_krate
        .modules
        .iter()
        .find(|m| m.x.path == module_path)
        .expect("module")
        .clone();

    let mut ctx = Ctx::new(
        &pruned_krate,
        global_ctx,
        module,
        prune_info.mono_abstract_datatypes.unwrap(),
        prune_info.spec_fn_types,
        prune_info.dyn_traits,
        prune_info.used_builtins,
        prune_info.fndef_types,
        prune_info.resolved_typs.unwrap(),
        /* debug */ false,
    )?;
    writeln!(trace, "[5/6] Ctx::new ok").unwrap();

    let reporter = Reporter {};
    let bucket_funs = pruned_krate.functions.iter().map(|f| f.x.name.clone()).collect();
    let krate_sst =
        vir::ast_to_sst_crate::ast_to_sst_krate(&mut ctx, &reporter, &bucket_funs, &pruned_krate)?;
    let krate_sst = vir::poly::poly_krate_for_module(&mut ctx, &krate_sst);
    writeln!(trace, "[6/6] ast_to_sst + poly ok ({} fn-sst)", krate_sst.functions.len()).unwrap();

    // Emit AIR commands the same way verify_bucket does, but only the bits we
    // need for a single trivial proof function.
    let mut commands: Vec<Command> = Vec::new();
    commands.extend(ctx.fuel().iter().cloned());
    for f in &krate_sst.functions {
        ctx.fun = vir::ast_to_sst_func::mk_fun_ctx(
            ctx.func_map.get(&f.x.name).expect("function in func_map"),
            false,
        );
        commands.extend(vir::sst_to_air_func::func_name_to_air(&ctx, &reporter, f)?.iter().cloned());
        commands.extend(vir::sst_to_air_func::func_decl_to_air(&mut ctx, f)?.iter().cloned());
        let (axiom_decls, _axiom_checks) =
            vir::sst_to_air_func::func_axioms_to_air(&mut ctx, f, /* public_body */ true)?;
        commands.extend(axiom_decls.iter().cloned());
        // The actual (check-valid …) for the body lives in the FuncCheckSst
        // attached to the function. For a proof fn this is exec_proof_check.
        if let Some(func_check_sst) = &f.x.exec_proof_check {
            let (cmd_groups, _snap_map) =
                vir::sst_to_air_func::func_sst_to_air(&ctx, f, func_check_sst)?;
            for group in cmd_groups.iter() {
                commands.extend(group.commands.iter().cloned());
            }
        }
    }
    ctx.fun = None;

    let n_decl = commands.iter().filter(|c| matches!(&***c, CommandX::Global(_))).count();
    let n_check = commands.iter().filter(|c| matches!(&***c, CommandX::CheckValid(_))).count();
    writeln!(trace, "AIR: {} commands ({} declarations, {} check-valid)", commands.len(), n_decl, n_check)
        .unwrap();

    Ok(VirPipelineResult { commands, arch_word_bits: ctx.arch_word_bits, trace })
}
