// verus-explorer — browser-based exploration of Verus's internal representations.
//
// Compiles `vir` and `air` (as-is, via path dependencies) to wasm32 and exposes
// a wasm-bindgen entry point that runs two kinds of demo queries through
// `air::context::Context` end-to-end:
//
//   1. AIR-text queries — small `(check-valid …)` snippets parsed straight to
//      AIR commands (see `QUERIES`).
//   2. VIR-driven query — a hand-built `vir::ast::Krate` for
//      `proof fn lemma() ensures true {}` driven through
//      `simplify_krate → ast_to_sst → poly → sst_to_air` (see `vir_query`).
//
// SMT is routed through the wasm32 `SmtProcess` shim in
// `air/src/smt_process.rs`, which calls the `Z3_*` wrappers installed by
// `public/index.html` on top of the self-hosted single-threaded Z3 wasm.

mod vir_query;

use std::sync::Arc;

use air::ast::{Command, CommandX};
use air::context::{Context, SmtSolver, ValidityResult};
use air::messages::Reporter;
use air::parser::Parser;
use vir::messages::VirMessageInterface;
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

// JS-side sink for Rust panics — defined on globalThis by index.html, which
// appends the message to #out so the user sees it in the page, not just the
// devtools console. Without a hook, panics abort as "unreachable executed"
// with no message.
#[wasm_bindgen]
extern "C" {
    fn reportPanic(msg: &str);
}

// Runs automatically when the wasm module is instantiated (wasm-bindgen
// wires this up so JS's `await init()` triggers it exactly once).
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
    #[wasm_bindgen(getter_with_clone)]
    pub air: String,
    #[wasm_bindgen(getter_with_clone)]
    pub verdict: String,
    pub proved: bool,
}

/// Aggregate result of `run`.
#[wasm_bindgen]
pub struct Output {
    pub all_expected: bool,
    #[wasm_bindgen(getter_with_clone)]
    pub queries: Vec<Query>,
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
    let sise::Node::List(nodes) = node else {
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
    Query { label: label.to_string(), air: air_script.trim().to_string(), verdict, proved }
}

fn run_vir_query() -> Query {
    let label = "VIR-driven: proof fn lemma() ensures true {}".to_string();
    let r = match vir_query::run_vir_pipeline() {
        Ok(r) => r,
        Err(e) => {
            return Query {
                label,
                air: String::new(),
                verdict: format!("VirErr: {:?}", e.note),
                proved: false,
            };
        }
    };

    // Send the Verus prelude first, then the pipeline-produced commands.
    let prelude = vir::context::Ctx::prelude(vir::prelude::PreludeConfig {
        arch_word_bits: r.arch_word_bits,
        solver: SmtSolver::Z3,
    });
    let mut all = Vec::with_capacity(prelude.len() + r.commands.len());
    all.extend(prelude.iter().cloned());
    all.extend(r.commands.iter().cloned());

    let (verdict, proved) = execute(&all);
    Query { label, air: r.trace.trim_end().to_string(), verdict, proved }
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
    Output { all_expected, queries }
}
