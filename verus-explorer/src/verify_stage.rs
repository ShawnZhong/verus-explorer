// Stage 4: VIR → AIR → Z3.
//
// Drives a fully-simplified Verus VIR krate through prune → Ctx →
// ast_to_sst → poly → AIR generation → Z3, returning the dumped AIR
// text and per-query verdicts. Mirrors `Verifier::verify_bucket` in
// `rust_verify/src/verifier.rs` but skips the bucket / spinoff /
// recommends / progress-bar / multi-thread machinery — the explorer
// only needs the core VIR → AIR → SMT pipeline.
//
// The Z3 backend is `air::context::Context`, which on wasm32 routes
// through the `Z3_*` shims declared in `air/src/smt_process.rs` and
// wired up in `public/app.js`.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io;
use std::sync::{Arc, Mutex};

use air::ast::{Command, CommandX};
use air::context::{Context, SmtSolver, ValidityResult};
use air::messages::{Diagnostics, MessageLevel};
use rust_verify::buckets::Bucket;
use rust_verify::commands::{FunctionOpGenerator, Op, OpGenerator, OpKind, QueryOp, Style};
use rust_verify::expand_errors_driver::ExpandErrorsResult;
use rust_verify::spans::SpanContext;
use rust_verify::verifier::Reporter;
use rustc_interface::interface::Compiler;
use vir::ast::{ArchWordBits, Fun, Krate, VirErr};
use vir::ast_util::fun_as_friendly_rust_name;
use vir::context::{Ctx, GlobalCtx};
use vir::def::ProverChoice;
use vir::messages::{ToAny, VirMessageInterface};
use vir::prelude::PreludeConfig;
use vir::sst::{AssertId, KrateSst};

use crate::externs::verus_z3_annotate;
use crate::util::{Block, Section, emit_section, push_banner, push_item, time};

#[derive(Default)]
pub(crate) struct VerifyOutput {
    /// Right after `ast_to_sst_krate` — VIR AST lowered into SST form
    /// (still polymorphic; function bodies as SST expressions/statements).
    /// One block per top-level item (function / datatype / trait / …),
    /// with `fold: true` on items from external crates so the reader
    /// can collapse them individually. Appended per module by
    /// `dump_sst` in `verify_module`.
    sst_ast_blocks: Vec<Block>,
    /// After `poly::poly_krate_for_module` — monomorphized SST
    /// (polymorphism erased; the form the AIR lowerer consumes). Same
    /// per-item block shape as `sst_ast_blocks`.
    sst_poly_blocks: Vec<Block>,
    /// Block stream per AIR/SMT tab in `[AIR_INITIAL, AIR_MIDDLE,
    /// AIR_FINAL, SMT]` order. `LogBufs::drain_block` pushes one
    /// block per pipeline boundary (prelude, each op), with
    /// `fold: true` for prelude and all Context ops, `fold: false`
    /// for Query ops. Adjacent folded blocks merge into one so the
    /// prelude + axiom / spec / broadcast / trait-impl setup
    /// collapses to a single row with the user's query blocks
    /// expanded beneath.
    air_blocks: [Vec<Block>; 4],
    verdicts: Vec<Verdict>,
}

struct Verdict {
    function: String,
    kind: String,
    outcome: String,
    proved: bool,
}

impl Verdict {
    fn from_result(result: &ValidityResult, function: String, op: QueryOp) -> Self {
        let kind = match op {
            QueryOp::SpecTermination => "spec termination".to_string(),
            QueryOp::Body(Style::Normal) => "body".to_string(),
            QueryOp::Body(Style::RecommendsFollowupFromError) => "recommends".to_string(),
            QueryOp::Body(Style::RecommendsChecked) => "recommends check".to_string(),
            QueryOp::Body(Style::Expanded) => "expanded".to_string(),
            QueryOp::Body(Style::CheckApiSafety) => "api safety".to_string(),
        };
        let (outcome, proved) = match result {
            ValidityResult::Valid(_) => ("valid".to_string(), true),
            ValidityResult::Invalid(_, _, Some(id)) => {
                let id_str = id.iter().map(u64::to_string).collect::<Vec<_>>().join(".");
                (format!("invalid (assert {id_str})"), false)
            }
            ValidityResult::Invalid(_, _, None) => ("invalid".to_string(), false),
            ValidityResult::Canceled => ("timeout".to_string(), false),
            ValidityResult::TypeError(_) => ("type error".to_string(), false),
            ValidityResult::UnexpectedOutput(s) => (format!("solver error: {s}"), false),
        };
        Self { function, kind, outcome, proved }
    }
}

// Box<dyn Write> needs 'static, but we want to drain the captured bytes back
// in the caller. An Arc<Mutex<Vec<u8>>> shared between writer and caller
// gives both: the writer owns its handle, and we drain the bytes at module
// boundaries.
#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    fn new() -> Self {
        SharedBuf(Arc::new(Mutex::new(Vec::new())))
    }
    fn drain_string(&self) -> String {
        let bytes = std::mem::take(&mut *self.0.lock().unwrap());
        String::from_utf8(bytes).unwrap_or_default()
    }
}

impl io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Holds the four shared buffers attached to an AIR `Context` for log capture.
// `attach` creates them and wires them into the context in one step.
struct LogBufs {
    air_initial: SharedBuf,
    air_middle: SharedBuf,
    air_final: SharedBuf,
    smt: SharedBuf,
}

// Attaching each log writer makes the air crate serialize every command to
// text as it's fed. Cheap per-command work (~8ms total for a tiny program),
// so we always attach all four; the browser caches the text on the JS side
// and toggles rendering from the cache instead of re-parsing on every
// checkbox change.
impl LogBufs {
    fn attach(ctx: &mut Context) -> Self {
        let bufs = Self {
            air_initial: SharedBuf::new(),
            air_middle: SharedBuf::new(),
            air_final: SharedBuf::new(),
            smt: SharedBuf::new(),
        };
        ctx.set_air_initial_log(Box::new(bufs.air_initial.clone()));
        ctx.set_air_middle_log(Box::new(bufs.air_middle.clone()));
        ctx.set_air_final_log(Box::new(bufs.air_final.clone()));
        ctx.set_smt_log(Box::new(bufs.smt.clone()));
        bufs
    }

    // Drain all four log buffers as one atomic snapshot and push one
    // block per tab onto `blocks` (indexed [initial, middle, final,
    // smt]). Adjacent folded blocks merge, so consecutive Context
    // ops collapse into the prelude's fold row. Skips the push when
    // the snapshot is empty.
    fn drain_block(&self, blocks: &mut [Vec<Block>; 4], fold: bool) {
        let texts: [String; 4] = [
            self.air_initial.drain_string(),
            self.air_middle.drain_string(),
            self.air_final.drain_string(),
            self.smt.drain_string(),
        ];
        if texts.iter().all(|t| t.trim().is_empty()) {
            return;
        }
        for (i, content) in texts.into_iter().enumerate() {
            if let Some(prev) = blocks[i].last_mut() {
                if fold && prev.fold {
                    if !prev.content.ends_with('\n') {
                        prev.content.push('\n');
                    }
                    prev.content.push_str(&content);
                    continue;
                }
            }
            blocks[i].push(Block { content, fold });
        }
    }
}

// Constants shared across every module-level verify pass. Bundled into a
// struct so `verify_module` and its helpers stay short.
struct ModuleCtx<'a, 'tcx> {
    krate: &'a Krate,
    crate_name: &'a Arc<String>,
    msg: &'a Arc<VirMessageInterface>,
    reporter: &'a Reporter<'tcx>,
    solver: SmtSolver,
    arch_word_bits: ArchWordBits,
    expand_errors: bool,
}

// Bundles the per-module driver state. `feed`/`feed_all` send each command to
// Z3 via `air_ctx.command()`; AIR/SMT dumps are captured by the log writers
// attached to `air_ctx`.
struct Feeder<'a, 'tcx> {
    air_ctx: &'a mut Context,
    msg: &'a Arc<VirMessageInterface>,
    reporter: &'a Reporter<'tcx>,
}

impl<'a, 'tcx> Feeder<'a, 'tcx> {
    fn feed(&mut self, cmd: &Command) -> ValidityResult {
        self.air_ctx.command(&**self.msg, self.reporter, cmd, Default::default())
    }
    fn feed_all(&mut self, cmds: &air::ast::Commands) {
        for cmd in cmds.iter() {
            self.feed(cmd);
        }
    }
}

// Stamp the op's label into the AIR/SMT logs (via `air_ctx.comment`) and
// into the Z3 response buffer (via the JS extern). Both tabs then read as
// per-op stanzas instead of a flat stream. Same payload for each sink so
// the banners line up across tabs.
fn emit_op_banner(feeder: &mut Feeder, op: &Op) {
    let comment = op.to_air_comment();
    feeder.air_ctx.comment(&comment);
    verus_z3_annotate(&comment);
}

// -------- drivers --------

pub(crate) fn verify_simplified_krate<'tcx>(
    krate: Krate,
    mut global_ctx: GlobalCtx,
    crate_name: Arc<String>,
    compiler: &'tcx Compiler,
    spans: &SpanContext,
    expand_errors: bool,
    output: &mut VerifyOutput,
) -> Result<(), VirErr> {
    let msg = Arc::new(VirMessageInterface {});
    // Routes VIR/AIR messages through rustc's `DiagCtxt` — the emitter
    // attached in `psess_created` (a `HumanEmitter` over a shared string
    // buffer) formats them with `error: … --> file:line | source` layout
    // the UI surfaces in its DIAGNOSTICS section.
    let reporter = time("verify.reporter_new", || Reporter::new(spans, compiler));
    let mctx = ModuleCtx {
        krate: &krate,
        crate_name: &crate_name,
        msg: &msg,
        reporter: &reporter,
        solver: SmtSolver::Z3,
        arch_word_bits: krate.arch.word_bits,
        expand_errors,
    };
    // After `build_vir_crate` merges vstd into the local krate, `krate.modules`
    // is ~155 entries; of those only the user's modules need verification.
    // Verus itself filters via `current_crate_modules` (captured before the
    // merge in `rust_verify/src/verifier.rs:2861`). `PathX.krate` is None for
    // local modules and Some(crate_name) for externs — the same distinction,
    // derivable post-merge so we don't have to rework `build_vir`.
    for module in krate.modules.iter().filter(|m| m.x.path.krate.is_none()) {
        match time("verify.module", || verify_module(&mctx, module.x.path.clone(), global_ctx, output)) {
            Ok(gctx) => global_ctx = gctx,
            Err(e) => {
                // Route spanned VirErrs through the reporter → DiagCtxt →
                // HumanEmitter so the UI renders them with the same
                // `error: … --> file:L:C | source` framing as upstream
                // (verifier.rs:3531). Writing `e.note` alone loses every
                // span, producing a bare sentence with no source context.
                reporter.report_as(&e.clone().to_any(), MessageLevel::Error);
                return Err(e);
            }
        }
    }
    Ok(())
}

fn verify_module(
    mctx: &ModuleCtx,
    module_path: vir::ast::Path,
    global_ctx: GlobalCtx,
    output: &mut VerifyOutput,
) -> Result<GlobalCtx, VirErr> {
    let (pruned, prune_info) = time("verify.prune", || vir::prune::prune_krate_for_module_or_krate(
        mctx.krate,
        mctx.crate_name,
        None,
        Some(module_path.clone()),
        None,
        true,
        true,
    ));
    let module = pruned
        .modules
        .iter()
        .find(|m| m.x.path == module_path)
        .cloned()
        .expect("module in pruned krate");

    let mut ctx = time("verify.ctx_new", || Ctx::new(
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
    ))?;

    let bucket_funs: HashSet<Fun> = pruned
        .functions
        .iter()
        .filter(|f| f.x.owning_module.as_ref() == Some(&module_path))
        .map(|f| f.x.name.clone())
        .collect();

    let dump_sst = |blocks: &mut Vec<Block>, k: &KrateSst| {
        use vir::printer::WalkEvent;
        vir::printer::walk_krate_sst(k, &vir::printer::COMPACT_TONODEOPTS, |event| match event {
            WalkEvent::Section(name) => push_banner(blocks, name),
            WalkEvent::Item { krate, span, text } => push_item(blocks, krate, span, text),
        });
    };

    let krate_sst = time("verify.ast_to_sst", || vir::ast_to_sst_crate::ast_to_sst_krate(
        &mut ctx,
        mctx.reporter,
        &bucket_funs,
        &pruned,
    ))?;
    time("dump.sst_ast", || dump_sst(&mut output.sst_ast_blocks, &krate_sst));
    let krate_sst = time("verify.poly", || vir::poly::poly_krate_for_module(&mut ctx, &krate_sst));
    time("dump.sst_poly", || dump_sst(&mut output.sst_poly_blocks, &krate_sst));

    // `Context::new` calls `SmtProcess::launch` → `Z3_mk_config`+`Z3_mk_context`,
    // which on wasm hops into the Emscripten Z3 runtime and spins up a fresh
    // solver context. That's not free — each context is its own Z3 state.
    let mut air_ctx = time("verify.air_ctx_new", || {
        let mut c = Context::new(mctx.msg.clone(), mctx.solver);
        c.set_z3_param("air_recommended_options", "true");
        // Cap each Z3 query at ~60 seconds of solver work (`RLIMIT_PER_SECOND`
        // = 3_000_000 in upstream Verus' `verifier.rs:50`, so 60 * that is
        // roughly the 60-second budget). Upstream's `ArgsX::new` default
        // is `f32::INFINITY`, which is only appropriate when a human can
        // Ctrl-C; a pathological assert in the browser would otherwise
        // hang the tab with no abort path. Bumped from the `--rlimit=10`
        // CLI default because heavier examples (e.g. doubly_linked_xor's
        // bit-vector XOR + quantifier reasoning) hit Z3's "incomplete
        // theory quant" when they run out of budget mid-instantiation.
        c.set_rlimit(60 * 3_000_000);
        c
    });
    let bufs = LogBufs::attach(&mut air_ctx);
    let mut feeder = Feeder { air_ctx: &mut air_ctx, msg: mctx.msg, reporter: mctx.reporter };
    time("verify.queries", || {
        run_queries(&mut feeder, &bufs, &mut ctx, &krate_sst, bucket_funs, output, mctx)
    })?;
    // `ctx.free()` drops the LogBufs-attached Z3 context → `Z3_del_context`.
    // Any deferred solver teardown shows up here.
    Ok(time("verify.ctx_free", || ctx.free()))
}

// Feeds the per-bucket AIR preamble. The prelude is sent first, then the
// `feed_bucket_preamble` helper (upstream patch in `rust_verify::verifier`)
// walks the fuel / trait / datatype / opaque / function-decl sequence in
// the same order `Verifier::verify_bucket` does — by delegating we stay in
// sync if upstream adds a new step.
fn feed_module_decls(
    feeder: &mut Feeder,
    ctx: &mut Ctx,
    krate_sst: &KrateSst,
    mctx: &ModuleCtx,
) -> Result<(), VirErr> {
    feeder.feed_all(&Ctx::prelude(PreludeConfig {
        arch_word_bits: mctx.arch_word_bits,
        solver: mctx.solver,
    }));
    let module = ctx.module_path();
    let preamble = rust_verify::verifier::Verifier::build_bucket_preamble(ctx, krate_sst, &module);
    for (cmds, _label) in preamble.batches() {
        feeder.feed_all(cmds);
    }
    // Per-function name declarations — verify_bucket mirrors this loop
    // alongside its own proof-note bookkeeping; we only need the feeds.
    for f in &krate_sst.functions {
        ctx.fun = vir::ast_to_sst_func::mk_fun_ctx(f, false);
        feeder.feed_all(&vir::sst_to_air_func::func_name_to_air(ctx, mctx.reporter, f)?);
    }
    ctx.fun = None;
    Ok(())
}

// OpGenerator drives the SCC-ordered req/ens decls + axioms + body queries.
// Each `CheckValid` command produces a `Verdict` appended to `verdicts`.
//
// Also runs expand-errors: after a failed Normal-style body op,
// `start_expand_errors_if_possible` arms the driver, and subsequent iterations
// fetch `expand_errors_next` before the regular queue so the driver can feed
// per-conjunct sub-queries back in. When it finishes, it yields the final
// `note: diagnostics via expansion` message which we route through the
// rustc reporter. Mirrors the loop at verifier.rs:1492-1859 but trimmed to
// the pieces the explorer needs (no spinoff, profiler, or progress bars).
fn run_queries(
    feeder: &mut Feeder,
    bufs: &LogBufs,
    ctx: &mut Ctx,
    krate_sst: &KrateSst,
    bucket_funs: HashSet<Fun>,
    output: &mut VerifyOutput,
    mctx: &ModuleCtx,
) -> Result<(), VirErr> {
    // Feed the module-scoped AIR prelude (axioms, fuel, datatype /
    // trait / assoc / function-name decls) that every query depends
    // on, then drain it as a single folded block — it's bulky
    // boilerplate the reader rarely wants expanded. Each op below
    // drains into its own expanded block so queries / context ops
    // read linearly beneath the collapsed prelude.
    verus_z3_annotate("AIR prelude");
    time("verify.feed_decls", || feed_module_decls(feeder, ctx, krate_sst, mctx))?;
    bufs.drain_block(&mut output.air_blocks, /* fold */ true);

    let bucket = Bucket { funs: bucket_funs };
    let mut opgen = OpGenerator::new(ctx, krate_sst, bucket);
    while let Some(mut function_opgen) = opgen.next()? {
        while let Some(op) = next_op(&mut function_opgen, feeder.reporter) {
            handle_op(op, &mut function_opgen, feeder, bufs, output, mctx)?;
        }
    }
    Ok(())
}

// Pull the next op. Preference: first drain any expand-errors sub-query
// (sub-conjunct probes after a failed Normal body), else ask the main
// OpGenerator for the next scheduled op. If expand-errors finished with
// a summary diagnostic rather than a sub-query, report it before
// returning — the subsequent `.next()` is still consulted so we don't
// skip a real queued op on the flush.
fn next_op<'tcx>(
    function_opgen: &mut FunctionOpGenerator,
    reporter: &Reporter<'tcx>,
) -> Option<Op> {
    let mut next_op = None;
    let mut expand_diag = None;
    if let Some(r) = function_opgen.expand_errors_next(None) {
        match r {
            Ok(op) => next_op = Some(op),
            Err(diag) => expand_diag = Some(diag),
        }
    }
    if next_op.is_none() {
        next_op = function_opgen.next();
    }
    if let Some(diag) = expand_diag {
        reporter.report(&diag);
    }
    next_op
}

// Process one op: emit its banner, dispatch on kind, run any probes,
// arm follow-up work (auto-recommends, expand-errors), and drain the
// op's log text into the right blocks. Everything after the OpGenerator
// yields an op happens here.
fn handle_op<'tcx>(
    op: Op,
    function_opgen: &mut FunctionOpGenerator,
    feeder: &mut Feeder<'_, 'tcx>,
    bufs: &LogBufs,
    output: &mut VerifyOutput,
    mctx: &ModuleCtx,
) -> Result<(), VirErr> {
    emit_op_banner(feeder, &op);

    // The explorer always compiles an anonymous crate, so every user
    // function's friendly name starts with `crate::`. Strip it so the
    // verdict detail reads `main: body → valid` instead of the
    // redundant `crate::main: body → valid`.
    let func_name = op
        .function
        .as_ref()
        .map(|f| fun_as_friendly_rust_name(&f.x.name))
        .map(|n| n.strip_prefix("crate::").map(str::to_string).unwrap_or(n))
        .unwrap_or_default();
    let mut any_invalid = false;
    let mut any_timed_out = false;
    let mut default_prover_failed_assert_ids: Vec<AssertId> = vec![];
    let mut retry_kind: Option<QueryOp> = None;
    match &op.kind {
        OpKind::Context(_, commands) => feeder.feed_all(commands),
        OpKind::Query { commands_with_context_list, query_op, .. } => {
            retry_kind = Some(*query_op);
            // Upstream maps each QueryOp to a MessageLevel
            // (`verifier.rs:1558-1563`). Recommends retries emit at
            // Note / Warning (informational context around a real
            // failure), not Error. Routing them through rustc at the
            // right level is what makes the UI render them as muted
            // notes instead of blocking errors.
            let level = match query_op {
                QueryOp::SpecTermination
                | QueryOp::Body(Style::Normal)
                | QueryOp::Body(Style::Expanded)
                | QueryOp::Body(Style::CheckApiSafety) => MessageLevel::Error,
                QueryOp::Body(Style::RecommendsFollowupFromError) => MessageLevel::Note,
                QueryOp::Body(Style::RecommendsChecked) => MessageLevel::Warning,
            };
            // Only Error-level ops count as pass/fail queries in the
            // verdict list. Recommends variants are diagnostic
            // side-channels — if they fail, the reporter surfaces them
            // as notes/warnings alongside the original error; counting
            // them toward `N/M queries failed` would just inflate the
            // tally with informational output.
            let verdict_is_query = level == MessageLevel::Error;
            for cmds in commands_with_context_list.iter() {
                // Contextual advice that Verus attaches to certain
                // proof obligations (e.g. loop-invariant hints). Upstream
                // emits it once per `CheckValid` command, as a `note:`
                // preceding the first failing probe (`verifier.rs:910`).
                // Single-threaded wasm, so the Mutex `.lock()` is free;
                // we clone out so we don't hold the guard across probes.
                let hint = cmds
                    .hint_upon_failure
                    .lock()
                    .expect("hint_upon_failure mutex poisoned")
                    .clone();
                for cmd in cmds.commands.iter() {
                    let mut result = feeder.feed(cmd);
                    if matches!(&**cmd, CommandX::CheckValid(_)) {
                        // Probe for more failing asserts in the same
                        // body via `check_valid_again`. Mirrors the
                        // two-phase loop in Verus'
                        // `verifier.rs:834-982`:
                        //   1. Up to `checks_remaining` "any more
                        //      errors" probes (upstream default:
                        //      `--multiple-errors=2`; bumped here
                        //      because explorer snippets are small
                        //      and users want to see everything).
                        //   2. When the budget runs out, flip
                        //      `only_check_earlier=true`. That pass
                        //      is guaranteed to terminate — AIR
                        //      strictly shrinks the enabled-label
                        //      set each call
                        //      (`smt_verify.rs:149-168`) and returns
                        //      Valid once none remain.
                        let mut checks_remaining: u32 = 8;
                        let mut only_check_earlier = false;
                        let mut is_first_check = true;
                        loop {
                            if let ValidityResult::Invalid(_, Some(err), _) = &result {
                                feeder.reporter.report_as(err, level);
                                // Emit the hint right after the first
                                // failure's error so the user sees
                                // them grouped. Matches upstream's
                                // first-check-only gate.
                                if is_first_check {
                                    if let Some(h) = &hint {
                                        feeder
                                            .reporter
                                            .report_as(&h.clone().to_any(), MessageLevel::Note);
                                    }
                                }
                            }
                            // Only DefaultProver failures get
                            // expand-errors; Nonlinear / BitVector
                            // use solver paths the expansion
                            // machinery doesn't cover.
                            if let ValidityResult::Invalid(_, _, Some(id)) = &result {
                                if cmds.prover_choice == ProverChoice::DefaultProver {
                                    default_prover_failed_assert_ids.push(id.clone());
                                }
                            }
                            if matches!(result, ValidityResult::Canceled) {
                                any_timed_out = true;
                            }
                            let proved = matches!(result, ValidityResult::Valid(_));
                            if !proved {
                                any_invalid = true;
                            }
                            // Push a verdict only for the first check
                            // of an Error-level op, or for any
                            // subsequent Invalid probe (more failing
                            // asserts in the same body). Valid probe
                            // responses are end-of-probe sentinels,
                            // not results — skipping them is what
                            // keeps `1/N queries failed` meaningful.
                            let push = verdict_is_query && (is_first_check || !proved);
                            if push {
                                output.verdicts.push(Verdict::from_result(
                                    &result,
                                    func_name.clone(),
                                    *query_op,
                                ));
                            }

                            // `check_valid_again` panics unless the
                            // previous result was Invalid with both
                            // a model and a message — that's how AIR
                            // stores its continuation state. Any
                            // other variant (Valid, Canceled,
                            // Invalid without a model) terminates
                            // the loop naturally.
                            let can_probe = matches!(
                                &result,
                                ValidityResult::Invalid(Some(_), Some(_), _),
                            );
                            if !can_probe {
                                break;
                            }
                            if !only_check_earlier {
                                checks_remaining -= 1;
                                if checks_remaining == 0 {
                                    only_check_earlier = true;
                                }
                            }
                            is_first_check = false;
                            result = feeder.air_ctx.check_valid_again(
                                feeder.reporter,
                                only_check_earlier,
                                Default::default(),
                            );
                        }
                        feeder.air_ctx.finish_query();
                    }
                }
            }
        }
    }
    // Auto-recommends: on a failed Normal body or spec-termination
    // query, enqueue recommends-retry ops. Mirrors the trigger in
    // Verus' `verifier.rs:1829-1879`.
    if any_invalid
        && matches!(
            retry_kind,
            Some(QueryOp::Body(Style::Normal)) | Some(QueryOp::SpecTermination)
        )
    {
        function_opgen.retry_with_recommends(&op, /* from_error */ true)?;
    }
    // Arm expand-errors on a failed Normal body. Driver starts here;
    // subsequent loop iterations feed its sub-queries in via
    // `expand_errors_next` at the top. Gated by the user-facing
    // "Expand errors" toggle — skipping the sub-queries shaves a
    // couple hundred ms per failed query in the browser.
    if mctx.expand_errors
        && matches!(retry_kind, Some(QueryOp::Body(Style::Normal)))
        && any_invalid
        && !default_prover_failed_assert_ids.is_empty()
    {
        function_opgen.start_expand_errors_if_possible(
            &op,
            default_prover_failed_assert_ids[0].clone(),
        );
    }
    // Report the outcome of each Expanded sub-query so the driver
    // advances through the conjunct tree. Pass/Fail/Timeout controls
    // which branch the next sub-query descends into.
    if matches!(retry_kind, Some(QueryOp::Body(Style::Expanded))) {
        let res = if any_timed_out {
            ExpandErrorsResult::Timeout
        } else if any_invalid {
            ExpandErrorsResult::Fail
        } else {
            ExpandErrorsResult::Pass
        };
        function_opgen.report_expand_error_result(res);
    }
    // Drain this op's text. Mirrors `push_item`'s VIR/SST rule: fold
    // iff the op isn't local user code — context ops for external
    // crates (vstd function-axioms / specs) and no-function setup ops
    // (broadcasts, trait-impl axioms) collapse into the prelude row;
    // context ops for local functions and queries (always tied to a
    // local function) stay expanded so the reader sees the actual
    // check-sat bodies and their immediate setup. `drain_block`
    // merges adjacent folded blocks so vstd runs stay as one row.
    let fold = op.function.as_ref().is_none_or(|f| f.x.name.path.krate.is_some());
    bufs.drain_block(&mut output.air_blocks, fold);
    Ok(())
}

pub(crate) fn write_verify_output(output: VerifyOutput) {
    let VerifyOutput {
        sst_ast_blocks, sst_poly_blocks, air_blocks, verdicts,
    } = output;
    let [ai, am, af, smt] = air_blocks;
    for (name, blocks) in [
        ("SST_AST", sst_ast_blocks),
        ("SST_POLY", sst_poly_blocks),
        ("AIR_INITIAL", ai),
        ("AIR_MIDDLE", am),
        ("AIR_FINAL", af),
        ("SMT", smt),
    ] {
        if blocks.iter().all(|b| b.content.trim().is_empty()) {
            continue;
        }
        emit_section(Section { name, blocks });
    }
    let mut verdict = String::new();
    if verdicts.is_empty() {
        writeln!(verdict, "no queries").unwrap();
    } else if verdicts.iter().all(|v| v.proved) {
        writeln!(verdict, "verified").unwrap();
    } else {
        let n_failed = verdicts.iter().filter(|v| !v.proved).count();
        writeln!(verdict, "{}/{} queries failed", n_failed, verdicts.len()).unwrap();
    }
    for v in &verdicts {
        writeln!(verdict, "{}: {} → {}", v.function, v.kind, v.outcome).unwrap();
    }
    emit_section(Section::single("VERDICT", verdict));
}
