// verus-explorer — browser-based exploration of Verus's internal representations.
//
// Compiles `vir` and `air` (as-is, via path dependencies) to wasm32 and exposes
// a wasm-bindgen entry point that drives a hand-built `vir::ast::Krate` for
// `proof fn lemma() ensures true {}` through
// `simplify_krate → ast_to_sst → poly → sst_to_air → air::context::Context`.
//
// SMT is routed through the wasm32 `SmtProcess` shim in
// `air/src/smt_process.rs`, which calls the `Z3_*` wrappers installed by
// `public/index.html` on top of the self-hosted single-threaded Z3 wasm.

mod frontend;
mod sysroot;
mod vir_bridge;

use std::sync::{Arc, Mutex};

use air::ast::{Command, CommandX};
use air::context::{Context, SmtSolver, ValidityResult};
use air::messages::Reporter;
use air::printer::{NodeWriter, Printer};
use vir::ast::{
    Arch, ArchWordBits, BodyVisibility, Constant, ExprX, FunX, FunctionAttrsX, FunctionKind,
    FunctionX, ItemKind, KrateX, Mode, ModuleX, Opaqueness, ParamX, Path, PathX, SpannedTyped,
    TypX, VarIdent, VarIdentDisambiguate, VirErr, Visibility,
};
use vir::ast_util::unit_typ;
use vir::context::{Ctx, GlobalCtx};
use vir::def::Spanned;
use vir::messages::{Span, VirMessageInterface};
use vir::printer::COMPACT_TONODEOPTS;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    pub(crate) fn console_error(msg: &str);
}

#[wasm_bindgen(start)]
fn init() {
    std::panic::set_hook(Box::new(|info| console_error(&info.to_string())));
    sysroot::install();
}

/// Dump of the VIR-driven pipeline — surfaced to JS field-by-field (no JSON).
#[wasm_bindgen]
pub struct Query {
    /// S-expression dump of the VIR krate.
    #[wasm_bindgen(getter_with_clone)]
    pub vir: String,
    /// AIR commands produced by the pipeline.
    #[wasm_bindgen(getter_with_clone)]
    pub air: String,
    #[wasm_bindgen(getter_with_clone)]
    pub verdict: String,
    pub proved: bool,
}

fn no_span() -> Span {
    Span {
        raw_span: Arc::new(()),
        id: 0,
        data: vec![],
        as_string: "no_span".to_string(),
    }
}

fn mk_path(segments: &[&str]) -> Path {
    Arc::new(PathX {
        krate: None,
        segments: Arc::new(segments.iter().map(|s| Arc::new(s.to_string())).collect()),
    })
}

fn commands_to_string(commands: &[Command]) -> String {
    let printer = Printer::new(Arc::new(VirMessageInterface {}), false, SmtSolver::Z3);
    let mut writer = NodeWriter::new();
    let mut out = String::new();
    let empty = String::new();
    for cmd in commands {
        match &**cmd {
            CommandX::Push => out.push_str("(push)"),
            CommandX::Pop => out.push_str("(pop)"),
            CommandX::SetOption(k, v) => {
                out.push_str(&format!("(set-option {} {})", k, v));
            }
            CommandX::Global(decl) => {
                out.push_str(&writer.node_to_string_indent(&empty, &printer.decl_to_node(decl)));
            }
            CommandX::CheckValid(query) => {
                out.push_str(&writer.node_to_string_indent(&empty, &printer.query_to_node(query)));
            }
        }
        out.push('\n');
    }
    out
}

/// Spin up an `air::Context` (Z3 backend) and feed it the given commands,
/// classifying the first non-Valid `(check-valid …)` outcome as the verdict.
fn execute(commands: &[Command]) -> (String, bool) {
    let msg = Arc::new(VirMessageInterface {});
    let reporter = Reporter {};
    let mut ctx = Context::new(msg.clone(), SmtSolver::Z3);
    ctx.set_z3_param("air_recommended_options", "true");

    let (mut verdict, mut proved) = (String::from("Valid"), true);
    for command in commands {
        let result = ctx.command(&*msg, &reporter, command, Default::default());
        let is_check = matches!(&**command, CommandX::CheckValid(_));
        if is_check {
            if !matches!(result, ValidityResult::Valid(_)) && proved {
                (verdict, proved) = (format!("{:?}", result), false);
            }
            ctx.finish_query();
        }
    }
    (verdict, proved)
}

/// Hand-built krate for `proof fn lemma() ensures true {}`.
fn build_lemma_krate() -> vir::ast::Krate {
    let span = no_span();
    let module_path = mk_path(&["root"]);
    let bool_typ = Arc::new(TypX::Bool);
    let unit = unit_typ();

    let true_expr = SpannedTyped::new(&span, &bool_typ, ExprX::Const(Constant::Bool(true)));
    let body = SpannedTyped::new(&span, &unit, ExprX::Block(Arc::new(vec![]), None));
    let ret = Spanned::new(
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
    let function = Spanned::new(
        span.clone(),
        FunctionX {
            name: Arc::new(FunX {
                path: mk_path(&["root", "lemma"]),
            }),
            proxy: None,
            kind: FunctionKind::Static,
            visibility: Visibility {
                restricted_to: None,
            },
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
    );
    let module = Spanned::new(
        span,
        ModuleX {
            path: module_path,
            reveals: None,
        },
    );
    Arc::new(KrateX {
        functions: vec![function],
        modules: vec![module],
        reveal_groups: vec![],
        datatypes: vec![],
        traits: vec![],
        trait_impls: vec![],
        assoc_type_impls: vec![],
        external_fns: vec![],
        external_types: vec![],
        path_as_rust_names: vec![],
        arch: Arch {
            word_bits: ArchWordBits::Either32Or64,
        },
        opaque_types: vec![],
    })
}

/// Drive the lemma krate through GlobalCtx::new → simplify_krate → prune →
/// Ctx::new → ast_to_sst_krate → poly → func_*_to_air, then execute via air.
fn run_vir_query() -> Result<Query, VirErr> {
    let krate_in = build_lemma_krate();
    let crate_name = Arc::new("explorer".to_string());
    let module_path = mk_path(&["root"]);

    let mut global_ctx = GlobalCtx::new(
        &krate_in,
        crate_name.clone(),
        no_span(),
        /* rlimit */ 10.0,
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(None)),
        SmtSolver::Z3,
        /* after_simplify */ false,
        /* check_api_safety */ false,
        /* axiom_usage_info */ false,
        /* new_mut_ref */ false,
        /* no_bv_simplify */ false,
        /* report_long_running */ false,
    )?;
    vir::recursive_types::check_traits(&krate_in, &global_ctx)?;
    let krate = vir::ast_simplify::simplify_krate(&mut global_ctx, &krate_in)?;

    let (pruned, prune_info) = vir::prune::prune_krate_for_module_or_krate(
        &krate,
        &crate_name,
        None,
        Some(module_path.clone()),
        None,
        true,
        true,
    );
    let module = pruned
        .modules
        .iter()
        .find(|m| m.x.path == module_path)
        .cloned()
        .expect("module");

    let mut ctx = Ctx::new(
        &pruned,
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

    let reporter = Reporter {};
    let bucket_funs = pruned.functions.iter().map(|f| f.x.name.clone()).collect();
    let krate_sst =
        vir::ast_to_sst_crate::ast_to_sst_krate(&mut ctx, &reporter, &bucket_funs, &pruned)?;
    let krate_sst = vir::poly::poly_krate_for_module(&mut ctx, &krate_sst);

    let mut commands: Vec<Command> = Vec::new();
    commands.extend(ctx.fuel().iter().cloned());
    for f in &krate_sst.functions {
        ctx.fun = vir::ast_to_sst_func::mk_fun_ctx(
            ctx.func_map.get(&f.x.name).expect("function in func_map"),
            false,
        );
        commands.extend(
            vir::sst_to_air_func::func_name_to_air(&ctx, &reporter, f)?
                .iter()
                .cloned(),
        );
        commands.extend(
            vir::sst_to_air_func::func_decl_to_air(&mut ctx, f)?
                .iter()
                .cloned(),
        );
        let (axiom_decls, _) =
            vir::sst_to_air_func::func_axioms_to_air(&mut ctx, f, /* public_body */ true)?;
        commands.extend(axiom_decls.iter().cloned());
        if let Some(body_sst) = &f.x.exec_proof_check {
            let (groups, _) = vir::sst_to_air_func::func_sst_to_air(&ctx, f, body_sst)?;
            for g in groups.iter() {
                commands.extend(g.commands.iter().cloned());
            }
        }
    }
    ctx.fun = None;

    let mut vir_buf: Vec<u8> = Vec::new();
    vir::printer::write_krate(&mut vir_buf, &krate, &COMPACT_TONODEOPTS);
    let vir_dump = String::from_utf8(vir_buf).unwrap_or_default();
    let air_dump = commands_to_string(&commands);

    // Prepend the Verus prelude before executing (not shown in the UI dump).
    let prelude = vir::context::Ctx::prelude(vir::prelude::PreludeConfig {
        arch_word_bits: ctx.arch_word_bits,
        solver: SmtSolver::Z3,
    });
    let mut all = Vec::with_capacity(prelude.len() + commands.len());
    all.extend(prelude.iter().cloned());
    all.extend(commands.iter().cloned());

    let (verdict, proved) = execute(&all);
    Ok(Query {
        vir: vir_dump.trim_end().to_string(),
        air: air_dump.trim_end().to_string(),
        verdict,
        proved,
    })
}

/// Run the rustc front-end on `src` (virtual path), stop after crate-root
/// parsing, and dump the top-level items. Proves rustc_driver::run_compiler
/// runs on wasm.
#[wasm_bindgen]
pub fn parse_source(src: &str) -> String {
    frontend::parse_source(src)
}

#[wasm_bindgen]
pub fn run() -> Query {
    run_vir_query().unwrap_or_else(|e| panic!("VirErr: {:?}", e.note))
}
