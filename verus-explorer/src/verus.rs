// Stages 3-4: HIR → VIR → AIR → Z3 — the verus-side pipeline.
//
// Stage 3 (`build_vir`, `dump_vir`, `vstd_krate`) drives Verus's
// HIR-to-VIR lowering, caches the pre-serialized vstd VIR across
// runs, and emits the VIR output tab.
//
// Stage 4 (`verify_simplified_krate`, `write_verify_output`, and
// the op loop) drives the simplified VIR krate through prune →
// Ctx → ast_to_sst → poly → AIR → Z3, returning the per-query
// verdicts and populating `VerifyOutput`. Mirrors
// `Verifier::verify_bucket` in `rust_verify/src/verifier.rs` but
// skips the bucket / spinoff / recommends / progress-bar /
// multi-thread machinery — the explorer only needs the core
// VIR → AIR → SMT pipeline.
//
// The Z3 backend is `air::context::Context`, which on wasm32
// routes through the `Z3_*` shims declared in
// `air/src/smt_process.rs` and wired up in `public/app.js`.

use std::collections::HashSet;
use std::io;
use std::sync::{Arc, Mutex, OnceLock};

use air::ast::{Command, CommandX};
use air::context::{Context, SmtSolver, ValidityResult};
use air::messages::{Diagnostics, MessageLevel};
use rust_verify::buckets::Bucket;
use rust_verify::cargo_verus_dep_tracker::DepTracker;
use rust_verify::commands::{FunctionOpGenerator, Op, OpGenerator, OpKind, QueryOp, Style};
use rust_verify::config::ArgsX;
use rust_verify::expand_errors_driver::ExpandErrorsResult;
use rust_verify::import_export::CrateWithMetadata;
use rust_verify::spans::{SpanContext, SpanContextX};
use rust_verify::verifier::{Reporter, Verifier};
use rust_verify::verus_items;
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;
use rustc_span::def_id::LOCAL_CRATE;
use vir::ast::{ArchWordBits, Fun, Krate, VirErr};
use vir::ast_util::fun_as_friendly_rust_name;
use vir::context::{Ctx, GlobalCtx};
use vir::def::ProverChoice;
use vir::messages::{ToAny, VirMessageInterface};
use vir::prelude::PreludeConfig;
use vir::sst::{AssertId, KrateSst};

use crate::util::{WalkBuilder, emit_section, time};
use crate::wasm::{verus_verdict, wasm_libs_vstd_vir};

// ==================== Stage 3: HIR → VIR ====================


// Drives Verus's HIR→VIR pipeline. Sets up the Verus-side context
// (`SpanContext`, `VerusItems`, `Reporter`, `Verifier`), then walks
// the same sequence upstream's driver runs:
//   1. `Verifier::construct_vir_crate` — HIR → raw VIR (stashed on
//      `verifier.vir_crate`); also stamps `verifier.air_no_span`.
//   2. `GlobalCtx::new` — global VIR context.
//   3. `recursive_types::check_traits` — coherence pass.
//   4. `ast_simplify::simplify_krate` — sugar lowering, returns a
//      fresh krate. `raw_krate` is left untouched as the pre-simplify
//      form so the explorer can dump VIR_RAW.
// Returns both krates and the GlobalCtx + crate_name + SpanContext
// for the downstream prune → Ctx → ast_to_sst → AIR pipeline.
//
// Why force `tcx.resolver_for_lowering()` first: the explorer enters
// rustc via `create_and_enter_global_ctxt` without the upstream
// driver's `after_expansion` callback, so the resolver hasn't run
// yet. Calling `all_diagnostic_items` (inside `construct_vir_crate`
// via `from_diagnostic_items`) freezes the cstore via
// `all_crate_nums`; if resolution fires after that, its first
// `cstore_mut()` call panics against the frozen lock.
pub(crate) fn build_vir<'tcx>(
    compiler: &Compiler,
    tcx: TyCtxt<'tcx>,
) -> Result<(Krate, Krate, GlobalCtx, Arc<String>, SpanContext), Vec<VirErr>> {
    let _ = tcx.resolver_for_lowering();
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
    let verus_items = Arc::new(verus_items::from_diagnostic_items(
        &tcx.all_diagnostic_items(()),
    ));
    let spans = SpanContextX::new(
        tcx,
        tcx.stable_crate_id(LOCAL_CRATE),
        compiler.sess.source_map(),
        std::collections::HashMap::new(),
        None,
    );
    let reporter = Reporter::new(&spans, compiler);
    let args_arc = Arc::new(args);
    let mut verifier = Verifier::new(args_arc.clone(), None, false, DepTracker::init());
    let raw_krate = time("build_vir.construct_vir_crate", || {
        match verifier.construct_vir_crate(
            tcx, verus_items, &spans,
            vec!["vstd".to_string()], vec![vstd_krate],
            &reporter, (*crate_name).clone(),
        ) {
            Ok(true) => Ok(verifier.vir_crate.clone().expect("vir_crate set")),
            // `construct_vir_crate` returns `Ok(false)` when rustc raised
            // diagnostics through `DiagCtxt` — bail with empty errs so
            // the caller stops without double-emitting.
            Ok(false) => Err(vec![]),
            Err((errs, diagnostics)) => {
                // Span every VirErr through the reporter so the explorer
                // UI shows source-located diagnostics instead of bare
                // notes. Mirrors the upstream driver's `after_expansion`
                // error-spanning pass.
                for diag in diagnostics {
                    let (level, err) = match diag {
                        vir::ast::VirErrAs::Warning(e) => (MessageLevel::Warning, e),
                        vir::ast::VirErrAs::Note(e) => (MessageLevel::Note, e),
                        vir::ast::VirErrAs::NonBlockingError(e, _)
                        | vir::ast::VirErrAs::NonFatalError(e, _) => (MessageLevel::Error, e),
                    };
                    reporter.report_as(&err.to_any(), level);
                }
                for err in &errs {
                    reporter.report_as(&err.clone().to_any(), MessageLevel::Error);
                }
                Err(errs)
            }
        }
    })?;
    let air_no_span = verifier.air_no_span.clone().expect("air_no_span set");
    let mut global_ctx = time("build_vir.global_ctx", || vir::context::GlobalCtx::new(
        &raw_krate,
        crate_name.clone(),
        air_no_span,
        args_arc.rlimit,
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(None)),
        args_arc.solver,
        false,
        args_arc.check_api_safety,
        args_arc.axiom_usage_info,
        args_arc.new_mut_ref,
        args_arc.no_bv_simplify,
        args_arc.report_long_running,
    ).map_err(|e| vec![e]))?;
    time("build_vir.check_traits", || {
        vir::recursive_types::check_traits(&raw_krate, &global_ctx).map_err(|e| vec![e])
    })?;
    let krate = time("build_vir.simplify_krate", || {
        vir::ast_simplify::simplify_krate(&mut global_ctx, &raw_krate).map_err(|e| vec![e])
    })?;
    Ok((raw_krate, krate, global_ctx, crate_name, spans))
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

// Walk the VIR krate(s) and emit the Verus IR tabs. Two stages:
//   * VIR_RAW    — direct output of `rust_to_vir` (HIR → VIR), before
//                  `ast_simplify::simplify_krate` lowers sugar (chained
//                  comparisons, struct-update tails, pattern matching,
//                  CoerceMode, tuple types, etc.).
//   * VIR_SIMPLE — what gets fed downstream to SST → AIR → Z3.
// Ignores the walk's per-kind section headers — `WalkBuilder` emits a
// `;;> <kind> <name> <span>` banner per item, which subsumes the
// outline info. External items nest inside a per-crate outer fold;
// local items flow flat.
pub(crate) fn dump_vir(raw: &Krate, simple: &Krate) {
    let dump_one = |krate: &Krate| -> String {
        use vir::printer::WalkItem;
        let mut b = WalkBuilder::new();
        vir::printer::walk_krate(krate, &vir::printer::COMPACT_TONODEOPTS, |item: WalkItem<'_>| {
            b.add_item(item.kind, &item.name, item.krate, item.span, item.text);
        });
        b.finish()
    };
    time("dump.vir_raw", || emit_section("VIR_RAW", dump_one(raw)));
    time("dump.vir_simple", || emit_section("VIR_SIMPLE", dump_one(simple)));
}

// ==================== Stage 4: VIR → AIR → Z3 ====================

// One `String` per output tab. SST entries are populated by `dump_sst`
// in `verify_module` (one `WalkBuilder` pass per module, appended
// end-to-end); the AIR / SMT entries are populated by
// `LogBufs::drain_to` as each pipeline boundary fires (prelude, each
// op, and for SMT each Z3 round trip). All seven share the same marker
// conventions (`;;>` / `;;v` / `;;<`) so the browser's
// `finalizeBannerBody` scanner handles them uniformly.
#[derive(Default)]
pub(crate) struct VerifyOutput {
    // Per-module pruned VIR (post `prune_krate_for_module_or_krate`).
    // Each module appends its own walk into this single body —
    // `WalkBuilder`'s per-crate banners keep the modules visually
    // separated. Useful for "what did module X actually pull in?".
    vir_pruned_body: String,
    sst_ast_body: String,
    sst_poly_body: String,
    air_initial_body: String,
    air_middle_body: String,
    air_final_body: String,
    // Single SMT exchange buffer — `;;> response …\n…\n;;<`
    // blocks live inline alongside commands. SMT_QUERY is derived
    // from this at `write()` time by stripping those response
    // blocks; SMT_TRANSCRIPT emits the buffer verbatim.
    smt_body: String,
}

// Minimal JSON string escape — covers the cases that show up in our
// verdict fields (quotes, backslashes, control chars). serde_json
// would be cleaner but we don't depend on it elsewhere; one helper
// + one `format!` is cheaper than pulling the crate.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

struct Verdict {
    function: String,
    // Function's source span (`<input.rs>:5:1: 5:30 (#0)` style from
    // `vir::messages::Span::as_string`). Empty when the op had no
    // associated function (rare — Context-only paths).
    span: String,
    kind: String,
    outcome: String,
    proved: bool,
}

impl Verdict {
    fn from_result(
        result: &ValidityResult,
        function: String,
        span: String,
        op: QueryOp,
    ) -> Self {
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
            // The `Some(AssertId)` carries an internal AIR-level
            // assertion identifier (a `Vec<u64>`, e.g. `0` or `0.1.2`)
            // that's only meaningful when correlated against the
            // matching `:named` label in the AIR tabs. Not useful to
            // end users — the diagnostic block below the verdict
            // panel already pinpoints the failing source line.
            ValidityResult::Invalid(_, _, _) => ("invalid".to_string(), false),
            ValidityResult::Canceled => ("timeout".to_string(), false),
            ValidityResult::TypeError(_) => ("type error".to_string(), false),
            ValidityResult::UnexpectedOutput(s) => (format!("solver error: {s}"), false),
        };
        Self { function, span, kind, outcome, proved }
    }

    // Encode this verdict as a single JSON object — one line per call,
    // hand-formatted so we don't pull in serde_json. Pairs with the
    // `verus_verdict` JS extern (see `wasm.rs`). The frontend's
    // `_verdicts[]` array accumulates these as they arrive and
    // `renderMeta` builds the per-function pass/fail panel directly
    // from that stream — no end-of-run text summary round trip.
    fn to_json(&self) -> String {
        format!(
            r#"{{"function":"{}","span":"{}","kind":"{}","outcome":"{}","proved":{}}}"#,
            json_escape(&self.function),
            json_escape(&self.span),
            json_escape(&self.kind),
            json_escape(&self.outcome),
            self.proved,
        )
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

// Shared buffers attached to an AIR `Context` for log capture.
// Four destinations:
//   * air_initial / air_middle / air_final — the three AIR
//     lowering stages (before/during/after type specialization).
//   * smt — single buffer attached as `smt_transcript_log`,
//     which (post-Option-C in `air/src/emitter.rs`) receives
//     both the command stream + section markers at emission
//     time *and* the per-response timing banners + Z3 replies
//     after each round trip. SMT_TRANSCRIPT is the buffer
//     verbatim; SMT_QUERY / SMT_RESPONSE are projections at
//     `VerifyOutput::write()` time.
struct LogBufs {
    air_initial: SharedBuf,
    air_middle: SharedBuf,
    air_final: SharedBuf,
    smt: SharedBuf,
}

// Attaching each log writer makes the air crate serialize every command to
// text as it's fed. Cheap per-command work (~8ms total for a tiny program),
// so we always attach all four; the browser caches the text on the JS
// side and toggles rendering from the cache instead of re-parsing on every
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
        // Single attachment: the transcript writer receives both
        // halves of the SMT exchange (commands at emission time,
        // responses after each round trip via `Context::log_smt_
        // response`). `set_smt_log` would only get the commands
        // half — we don't need a separate replayable `.smt2` from
        // the explorer.
        ctx.set_smt_transcript_log(Box::new(bufs.smt.clone()));
        bufs
    }

    // Drain all five log buffers and append each into the matching
    // field of `output`. Fold structure is computed JS-side by
    // scanning markers — no per-op block boundaries or merge logic.
    fn drain_to(&self, output: &mut VerifyOutput) {
        output.air_initial_body.push_str(&self.air_initial.drain_string());
        output.air_middle_body.push_str(&self.air_middle.drain_string());
        output.air_final_body.push_str(&self.air_final.drain_string());
        output.smt_body.push_str(&self.smt.drain_string());
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
    // Per-module pruned VIR: walk the (per-module) pruned krate and
    // append into the shared body. Each module gets its own item set
    // so the reader can see exactly which deps were pulled in for
    // this verify pass. `WalkBuilder`'s crate-grouped banners keep
    // multiple modules' walks visually separated in one stream.
    time("dump.vir_pruned", || {
        use vir::printer::WalkItem;
        let mut b = WalkBuilder::new();
        vir::printer::walk_krate(&pruned, &vir::printer::COMPACT_TONODEOPTS, |item: WalkItem<'_>| {
            b.add_item(item.kind, &item.name, item.krate, item.span, item.text);
        });
        output.vir_pruned_body.push_str(&b.finish());
    });
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

    let dump_sst = |body: &mut String, k: &KrateSst| {
        use vir::printer::WalkItem;
        let mut b = WalkBuilder::new();
        vir::printer::walk_krate_sst(k, &vir::printer::COMPACT_TONODEOPTS, |item: WalkItem<'_>| {
            b.add_item(item.kind, &item.name, item.krate, item.span, item.text);
        });
        body.push_str(&b.finish());
    };

    let krate_sst = time("verify.ast_to_sst", || vir::ast_to_sst_crate::ast_to_sst_krate(
        &mut ctx,
        mctx.reporter,
        &bucket_funs,
        &pruned,
    ))?;
    time("dump.sst_ast", || dump_sst(&mut output.sst_ast_body, &krate_sst));
    let krate_sst = time("verify.poly", || vir::poly::poly_krate_for_module(&mut ctx, &krate_sst));
    time("dump.sst_poly", || dump_sst(&mut output.sst_poly_body, &krate_sst));

    // `Context::new` calls `SmtProcess::launch` → `Z3_mk_config`+`Z3_mk_context`,
    // which on wasm hops into the Emscripten Z3 runtime and spins up a fresh
    // solver context. That's not free — each context is its own Z3 state.
    //
    // Attach `LogBufs` *before* the `set_z3_param` / `set_rlimit` calls so
    // the resulting `(set-option …)` lines (and the implicit ones the
    // `air_recommended_options` macro expands to) flow into the SMT_QUERY
    // and SMT_TRANSCRIPT tabs. Pre-attach writes would only land in the
    // pipe buffer and silently drop on the log side, hiding the early
    // solver-config preamble from the UI.
    let mut air_ctx = time("verify.air_ctx_new", || Context::new(mctx.msg.clone(), mctx.solver));
    let bufs = LogBufs::attach(&mut air_ctx);
    air_ctx.set_z3_param("air_recommended_options", "true");
    // Cap each Z3 query at ~60 seconds of solver work (`RLIMIT_PER_SECOND`
    // = 3_000_000 in upstream Verus' `verifier.rs:50`, so 60 * that is
    // roughly the 60-second budget). Upstream's `ArgsX::new` default
    // is `f32::INFINITY`, which is only appropriate when a human can
    // Ctrl-C; a pathological assert in the browser would otherwise
    // hang the tab with no abort path. Bumped from the `--rlimit=10`
    // CLI default because heavier examples (e.g. doubly_linked_xor's
    // bit-vector XOR + quantifier reasoning) hit Z3's "incomplete
    // theory quant" when they run out of budget mid-instantiation.
    air_ctx.set_rlimit(60 * 3_000_000);
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
    // `;;> AIR prelude` is emitted upstream at context.rs:415 —
    // close it here after the prelude decls have been fed.
    time("verify.feed_decls", || feed_module_decls(feeder, ctx, krate_sst, mctx))?;
    feeder.air_ctx.section_close();
    bufs.drain_to(output);

    let bucket = Bucket { funs: bucket_funs };
    let mut opgen = OpGenerator::new(ctx, krate_sst, bucket);
    // Wrap runs of same-external-crate ops in an outer `;;> <crate>`
    // section so the reader sees one collapsible "vstd" group
    // instead of ~50 flat sibling sections. Local-crate ops flow
    // flat at the top level — they're the user's own code and don't
    // benefit from an extra header. `wrapper_krate` tracks the
    // currently-open external wrapper (or `None` when we're at the
    // flat level); rotating wrappers on each crate transition keeps
    // them balanced even when local ops interleave with external
    // runs.
    //
    // Emit per-op open/close here (not inside `handle_op`) so the
    // pairing survives any `?`-early-return paths — a bare `?`
    // inside a manually-paired open/close would leak an open.
    let mut wrapper_krate: Option<Arc<String>> = None;
    let result: Result<(), VirErr> = (|| {
        while let Some(mut function_opgen) = opgen.next()? {
            while let Some(op) = next_op(&mut function_opgen, feeder.reporter) {
                let cur_krate = op.function.as_ref().and_then(|f| f.x.name.path.krate.clone());
                if wrapper_krate != cur_krate {
                    if wrapper_krate.is_some() {
                        feeder.air_ctx.section_close();
                    }
                    if let Some(k) = &cur_krate {
                        feeder.air_ctx.section('>', k);
                    }
                    wrapper_krate = cur_krate;
                }
                // Auto-fold only external-crate Context ops — they're
                // vstd boilerplate the reader rarely wants open. Local
                // Context ops (the user's own specs / axioms) stay
                // expanded, as do all Query ops (the actual check-sat
                // bodies). Each gets its own gutter marker so the
                // user can fold individually if they want.
                let op_marker = match op.kind {
                    OpKind::Context(..) if wrapper_krate.is_some() => '>',
                    _ => 'v',
                };
                feeder.air_ctx.section(op_marker, &op.to_air_comment());
                let r = handle_op(op, &mut function_opgen, feeder, mctx);
                feeder.air_ctx.section_close();
                r?;
            }
        }
        Ok(())
    })();
    // Close the final crate wrapper (if one was open) whether or not
    // an op errored — keeps open/close balanced on the error path.
    if wrapper_krate.is_some() {
        feeder.air_ctx.section_close();
    }
    // Final drain: catches the per-op section_close emits (they
    // land in pipe_buffer AFTER handle_op returns) and the closing
    // crate wrapper above.
    bufs.drain_to(output);
    result
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
    mctx: &ModuleCtx,
) -> Result<(), VirErr> {
    // Per-op `;;>`/`;;v` open + `;;<` close are emitted by the
    // caller (`run_queries`) so the pairing survives any `?` early-
    // return in this function.

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
    let func_span = op
        .function
        .as_ref()
        .map(|f| f.span.as_string.clone())
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
                                // Stream the verdict as JSON (one line per
                                // call) so the frontend can update its
                                // pass/fail table progressively. No
                                // Rust-side accumulator — the UI is the
                                // sole consumer.
                                let verdict = Verdict::from_result(
                                    &result,
                                    func_name.clone(),
                                    func_span.clone(),
                                    *query_op,
                                );
                                verus_verdict(&verdict.to_json());
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
    // Drain this op's text into the AIR bodies. JS computes fold
    // structure from scanning `;;` banners — see `finalizeBanner-
    // Body` in `public/app.js`. AIR and Z3 Query / Reply fold in
    // lockstep because they receive the same section stream
    // (`air_ctx.section` writes to all four logs). `drain_to` runs
    // at the end of `run_queries` so per-op section_close emits in
    // the caller land in `log_bodies` too.
    Ok(())
}

impl VerifyOutput {
    // Stream each populated body out to the browser via `emit_section` →
    // `verus_dump`. Consumes `self` so the per-field `String`s move
    // directly into the emission path without extra allocation. Empty
    // bodies skip emit — saves a tab slot when a stage produced nothing.
    //
    // Verdicts don't go through here — they stream live via the
    // `verus_verdict` JS extern as each Z3 query lands (see `handle_op`).
    // The frontend builds its verdict panel from that stream directly.
    pub(crate) fn write(self) {
        // Three views projected from `smt_body`:
        //   * SMT_QUERY     — strip `;;> response …\n…\n;;<` blocks
        //   * SMT_RESPONSE  — keep section markers + response blocks
        //   * SMT_TRANSCRIPT — verbatim
        // Build the projections first (borrows self.smt_body), then
        // move everything into the emission loop.
        let smt_query_body = project_smt(&self.smt_body, /* keep_responses */ false);
        let smt_response_body = project_smt(&self.smt_body, /* keep_responses */ true);
        for (name, body) in [
            ("VIR_PRUNED", self.vir_pruned_body),
            ("SST_AST", self.sst_ast_body),
            ("SST_POLY", self.sst_poly_body),
            ("AIR_INITIAL", self.air_initial_body),
            ("AIR_MIDDLE", self.air_middle_body),
            ("AIR_FINAL", self.air_final_body),
            ("SMT_QUERY", smt_query_body),
            ("SMT_RESPONSE", smt_response_body),
            ("SMT_TRANSCRIPT", self.smt_body),
        ] {
            if body.trim().is_empty() {
                continue;
            }
            emit_section(name, body);
        }
    }
}

// Project an SMT transcript into either:
//   * `keep_responses = false` → drop the `;;> response …\n…\n;;<`
//     blocks; keep commands + section markers (the SMT_QUERY view).
//   * `keep_responses = true`  → drop command lines; keep section
//     markers and the contents of every response block (the
//     SMT_RESPONSE view, which the JS scanner further folds away
//     empty op sections from).
// Section markers (`;;>`/`;;v`/`;;<`) always pass through so the
// fold structure is preserved across both views.
fn project_smt(transcript: &str, keep_responses: bool) -> String {
    let mut out = String::with_capacity(transcript.len());
    let mut in_response = false;
    for line in transcript.lines() {
        let is_marker = line.starts_with(";;>") || line.starts_with(";;v") || line == ";;<";
        if line.starts_with(";;> response ") {
            in_response = true;
            if keep_responses {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        if in_response && line == ";;<" {
            in_response = false;
            if keep_responses {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        // Body line of a response: keep iff we're projecting the
        // response view; drop iff we're projecting the query view.
        if in_response {
            if keep_responses {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        // Outside a response block — keep iff projecting query, OR
        // it's a section marker (always preserved for fold structure).
        if !keep_responses || is_marker {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

