// verus-explorer — browser-based exploration of Verus's internal representations.
//
// Compiles `vir` and `air` (as-is, via path dependencies) to wasm32 and exposes
// a wasm-bindgen entry point that runs two kinds of demo queries through
// `air::context::Context` end-to-end:
//
//   1. AIR-text queries — small `(check-valid …)` snippets parsed straight to
//      AIR commands.
//   2. VIR-driven query — a hand-built `vir::ast::Krate` for
//      `proof fn lemma() ensures true {}` driven through
//      `simplify_krate → ast_to_sst → poly → sst_to_air`.
//
// SMT is routed through the wasm32 `SmtProcess` shim in
// `air/src/smt_process.rs`, which calls the `Z3_*` wrappers installed by
// `public/index.html` on top of the self-hosted single-threaded Z3 wasm.
mod frontend;

use std::sync::{Arc, Mutex};

use air::ast::{Command, CommandX};
use air::context::{Context, SmtSolver, ValidityResult};
use air::messages::Reporter;
use air::parser::Parser;
use air::printer::{NodeWriter, Printer};
use sise::Node;
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

/// Demo AIR scripts — already AIR, not Rust source. Third field is the
/// expected verdict (true = provable).
const QUERIES: &[(&str, &str, bool)] = &[
    (
        "commutativity of +",
        r#"
            (check-valid
                (declare-const x Int)
                (declare-const y Int)
                (assert (= (+ x y) (+ y x))))
        "#,
        true,
    ),
    (
        "false claim: x == 0 for all x",
        r#"
            (check-valid
                (declare-const x Int)
                (assert (= x 0)))
        "#,
        false,
    ),
];

// JS-side sink for Rust panics — defined on globalThis by index.html.
#[wasm_bindgen]
extern "C" {
    fn reportPanic(msg: &str);
}

#[wasm_bindgen(start)]
fn init() {
    std::panic::set_hook(Box::new(|info| reportPanic(&info.to_string())));
}

/// Result of one query — surfaced to JS field-by-field (no JSON).
#[wasm_bindgen]
#[derive(Clone)]
pub struct Query {
    #[wasm_bindgen(getter_with_clone)]
    pub label: String,
    /// S-expression dump of the VIR krate (empty for AIR-text queries).
    #[wasm_bindgen(getter_with_clone)]
    pub vir: String,
    /// AIR — input script for AIR-text queries, pipeline-produced commands
    /// for the VIR-driven query.
    #[wasm_bindgen(getter_with_clone)]
    pub air: String,
    #[wasm_bindgen(getter_with_clone)]
    pub verdict: String,
    pub proved: bool,
}

#[wasm_bindgen]
pub struct Output {
    pub all_expected: bool,
    #[wasm_bindgen(getter_with_clone)]
    pub queries: Vec<Query>,
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

fn krate_to_string(krate: &vir::ast::Krate) -> String {
    let mut buf: Vec<u8> = Vec::new();
    vir::printer::write_krate(&mut buf, krate, &COMPACT_TONODEOPTS);
    String::from_utf8(buf).unwrap_or_default()
}

fn commands_to_string(commands: &[Command]) -> String {
    let printer = Printer::new(Arc::new(VirMessageInterface {}), false, SmtSolver::Z3);
    let mut writer = NodeWriter::new();
    let mut out = String::new();
    for cmd in commands {
        let node = match &**cmd {
            CommandX::Push => Node::List(vec![Node::Atom("push".to_string())]),
            CommandX::Pop => Node::List(vec![Node::Atom("pop".to_string())]),
            CommandX::SetOption(k, v) => Node::List(vec![
                Node::Atom("set-option".to_string()),
                Node::Atom((**k).clone()),
                Node::Atom((**v).clone()),
            ]),
            CommandX::Global(decl) => printer.decl_to_node(decl),
            CommandX::CheckValid(query) => printer.query_to_node(query),
        };
        out.push_str(&writer.node_to_string_indent(&String::new(), &node));
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
        match &result {
            ValidityResult::Valid(_) if is_check => {}
            ValidityResult::Invalid(..) if is_check => {
                (verdict, proved) = ("Invalid".to_string(), false);
            }
            ValidityResult::Canceled if is_check => {
                (verdict, proved) = ("Canceled".to_string(), false);
            }
            ValidityResult::UnexpectedOutput(s) if is_check => {
                (verdict, proved) = (format!("UnexpectedOutput({})", s), false);
            }
            ValidityResult::TypeError(e) => {
                (verdict, proved) = (format!("TypeError({:?})", e), false);
            }
            _ => {}
        }
        if is_check {
            ctx.finish_query();
        }
    }
    (verdict, proved)
}

fn run_air_text_query(label: &str, air_script: &str) -> Query {
    // The AIR parser expects a top-level list of commands; wrap in parens.
    let bytes = format!("({})", air_script).into_bytes();
    let mut sise_parser = sise::Parser::new(&bytes);
    let node = sise::read_into_tree(&mut sise_parser).expect("AIR sise parse");
    let Node::List(nodes) = node else {
        panic!("expected list at AIR top level");
    };
    let msg = Arc::new(VirMessageInterface {});
    let commands: Vec<Command> = Parser::new(msg)
        .nodes_to_commands(&nodes)
        .expect("AIR parse")
        .iter()
        .cloned()
        .collect();

    let (verdict, proved) = execute(&commands);
    Query {
        label: label.to_string(),
        vir: String::new(),
        air: air_script.trim().to_string(),
        verdict,
        proved,
    }
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

struct VirPipeline {
    krate: vir::ast::Krate,
    commands: Vec<Command>,
    arch_word_bits: ArchWordBits,
}

/// Drive the lemma krate through GlobalCtx::new → simplify_krate → prune →
/// Ctx::new → ast_to_sst_krate → poly → func_*_to_air.
fn run_vir_pipeline() -> Result<VirPipeline, VirErr> {
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

    Ok(VirPipeline {
        krate,
        commands,
        arch_word_bits: ctx.arch_word_bits,
    })
}

fn run_vir_query() -> Query {
    let label = "VIR-driven: proof fn lemma() ensures true {}".to_string();
    let r = match run_vir_pipeline() {
        Ok(r) => r,
        Err(e) => {
            return Query {
                label,
                vir: String::new(),
                air: String::new(),
                verdict: format!("VirErr: {:?}", e.note),
                proved: false,
            };
        }
    };

    let vir_dump = krate_to_string(&r.krate);
    let air_dump = commands_to_string(&r.commands);

    // Prepend the Verus prelude before executing (not shown in the UI dump).
    let prelude = vir::context::Ctx::prelude(vir::prelude::PreludeConfig {
        arch_word_bits: r.arch_word_bits,
        solver: SmtSolver::Z3,
    });
    let mut all = Vec::with_capacity(prelude.len() + r.commands.len());
    all.extend(prelude.iter().cloned());
    all.extend(r.commands.iter().cloned());

    let (verdict, proved) = execute(&all);
    Query {
        label,
        vir: vir_dump.trim_end().to_string(),
        air: air_dump.trim_end().to_string(),
        verdict,
        proved,
    }
}

/// Tokenize Rust source with the vendored `rustc_lexer` and return a
/// human-readable dump — proves a rustc-internal crate is alive in wasm.
#[wasm_bindgen]
pub fn lex_source(src: &str) -> String {
    frontend::lex_source(src)
}

#[wasm_bindgen]
pub fn run() -> Output {
    let mut all_expected = true;
    let mut queries: Vec<Query> = Vec::new();
    for (label, script, expected) in QUERIES {
        let q = run_air_text_query(label, script);
        if q.proved != *expected {
            all_expected = false;
        }
        queries.push(q);
    }
    let vir_q = run_vir_query();
    if !vir_q.proved {
        all_expected = false;
    }
    queries.push(vir_q);
    Output {
        all_expected,
        queries,
    }
}
