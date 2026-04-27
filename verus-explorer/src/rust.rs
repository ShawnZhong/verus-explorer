// Stage 1: rustc invocation + diagnostic plumbing.
// Stage 2: Rust IR dumps (AST_PRE / AST / HIR / HIR_TYPED).
//
// `build_rustc_config` wires our virtual sysroot + the `verus!`-friendly
// flag set into a rustc `Config`. `VirtualFileLoader` routes the single
// in-memory source buffer; `DiagnosticWriter` bridges rustc's
// `JsonEmitter` to the `verus_diagnostic` JS callback (one JSON object
// per `\n`-terminated line). The three AST/HIR dumps emit their
// sections via `emit_section`.

use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustc_errors::ColorConfig;
use rustc_errors::emitter::HumanReadableErrorType;
use rustc_errors::json::JsonEmitter;
use rustc_errors::registry::Registry;
use rustc_middle::ty::TyCtxt;
use rustc_session::EarlyDiagCtxt;
use rustc_session::config::{self, ErrorOutputType, Input};
use rustc_span::source_map::FileLoader;
use rustc_span::{FileName, Symbol};

use crate::wasm::verus_diagnostic;
use crate::util::{emit_section, time};
use crate::wasm::std_mode;

pub(crate) fn dump_ast_pre_expansion(krate: &rustc_ast::Crate) {
    time("dump.ast_pre", || {
        let body = rustc_ast_pretty::pprust::crate_to_string_for_macros(krate);
        emit_section("AST_PRE", body);
    });
}

// ── Stage 1: rustc invocation + diagnostic plumbing ──────────────────

// `--sysroot=/virtual` pairs with the filesearch callbacks installed by
// `wasm_libs_finalize` — rustc's crate locator finds `libcore.rmeta` (and
// friends), plus our prebuilt `libverus_builtin.rmeta`, in the libs bundle
// instead of on disk. `#![no_std]` keeps std out (only `core` is needed).
//
// `Mode` picks between the two pipelines we expose:
//   * `Verify`  — preserves ghost code through HIR/VIR/AIR for the
//     verifier. Inserts both `verus_keep_ghost` and `verus_keep_ghost_body`
//     (mirroring `rust_verify::driver` line 270-272). `cfg_erase()`
//     returns `EraseGhost::Keep` — full ghost surface survives parse →
//     AST → HIR → VIR → AIR → SMT. `--crate-type=lib`.
//   * `Execute` — erases ghost bodies before MIR-building so Miri only
//     interprets run-mode logic. Inserts only `verus_keep_ghost`
//     (driver.rs:162); `cfg_erase()` returns `EraseGhost::Erase`. Sets
//     `--crate-type=bin` (so Miri finds `tcx.entry_fn()` for `fn main`)
//     plus `--emit=metadata` (short-circuits the bin path before link,
//     since our sysroot ships rmetas and Miri uses tcx queries directly).
//     Forces libstd available regardless of the no_std toggle because
//     Miri needs the `start` lang item and most run-mode programs use
//     `println!` / `eprintln!`.
// Verus has a third upstream "EraseGhost::EraseAll" mode for third-party
// deps Verus never touched; we don't expose it.
#[derive(Copy, Clone)]
pub(crate) enum Mode { Verify, Execute }

pub(crate) fn build_rustc_config(src: String, mode: Mode) -> rustc_interface::interface::Config {
    let exec_mode = matches!(mode, Mode::Execute);
    let keep_ghost = !exec_mode;
    // libstd is shipped in std-mode (verify) and always in execute mode;
    // the JS side mirrors this via `LIBS_VARIANT`. Drives both the
    // `#![no_std]` injection below and the `feature = "std"` cfg.
    let libstd_available = std_mode() || exec_mode;
    // Put `vstd` and `verus_builtin` in the edition-2018+ extern prelude so
    // user code can `use vstd::prelude::*;` / `use verus_builtin::*;` directly
    // — same flags native Verus's driver and test harness pass. We used to
    // prepend `extern crate vstd;\n` to the source, but that shifted every
    // diagnostic's line number by one, breaking the editor's error-line
    // highlight. No `=PATH` needed: rustc's crate locator finds the rmetas
    // via the libs sysroot bundle.
    let crate_type = if exec_mode { "--crate-type=bin" } else { "--crate-type=lib" };
    let mut argv: Vec<String> = ["--edition=2021", crate_type, "--crate-name=verus_explorer",
        "--sysroot=/virtual", "--extern=vstd", "--extern=verus_builtin"]
        .into_iter().map(String::from).collect();
    // `--crate-type=bin` would otherwise drive rustc all the way through
    // codegen + link, which fails because our sysroot ships rmetas (not
    // rlibs). Miri uses tcx queries directly — it doesn't need linked
    // output — so `--emit=metadata` short-circuits before codegen.
    if exec_mode {
        argv.push("--emit=metadata".into());
    }
    // Feature gates, `register_tool(...)`, and the native subset of lint
    // allows come straight from `rust_verify::config`, so any upstream rustc
    // flag drift tracks automatically instead of requiring a hand-maintained
    // mirror. `syntax_macro = true` because user input always runs through
    // `verus!`. `erase_ghost` is currently ignored by the upstream
    // function (parameter is `_`-prefixed); we still pass `!keep_ghost`
    // so intent is correct if Verus ever wires it up.
    rust_verify::config::enable_default_features_and_verus_attr(
        &mut argv, /* syntax_macro */ true, /* erase_ghost */ !keep_ghost,
    );
    // Explorer-specific additions on top of the upstream set:
    //   * `proc_macro_hygiene` — the wasm shim registers Verus macros via
    //     `rustc_metadata::proc_macro_registry` (see `init`) instead of
    //     dlopen'ing a host dylib; this gate keeps rustc from rejecting the
    //     resulting hygiene pattern.
    //   * Extra `-A lint` flags for false positives observed on standalone
    //     snippets that aren't wired into a larger crate (the native driver
    //     doesn't suppress these because cargo's own rustc run does).
    argv.extend([
        "-Zcrate-attr=feature(proc_macro_hygiene)",
        "-Aunused_variables", "-Aunused_assignments", "-Aunreachable_patterns",
        "-Adead_code", "-Aunreachable_code", "-Aunused_labels",
        "-Aunused_attributes", "-Anon_shorthand_field_patterns",
    ].into_iter().map(String::from));
    // Nostd mode: inject `#![no_std]` at the crate root so rustc bypasses
    // the std prelude. The JS loader fetches a vstd variant built with
    // `feature="alloc"` but NOT `feature="std"`, so user code can still
    // `use vstd::prelude::*` but can't reach into std::*. With libstd
    // available, the attr is omitted; rustc then injects the std prelude
    // and the full-fat vstd bundle is active.
    if !libstd_available {
        argv.push("-Zcrate-attr=no_std".into());
    }

    let mut early_dcx = EarlyDiagCtxt::new(ErrorOutputType::default());
    let matches = rustc_driver::handle_options(&early_dcx, &argv).expect("handle_options");
    let opts = config::build_session_options(&mut early_dcx, &matches);

    rustc_interface::interface::Config {
        opts,
        // `crate_cfg` is intentionally empty — `parse_cfg` constructs a fresh
        // `ParseSess` per entry, which builds a `SourceMap` with the default
        // `RealFileLoader`, and `current_directory()` traps on wasm32. Inject
        // the cfgs from `psess_created` instead, where the SourceMap is already
        // wired to our `VirtualFileLoader`.
        crate_cfg: vec![],
        crate_check_cfg: vec![],
        input: Input::Str { name: FileName::Custom("input.rs".into()), input: src },
        output_file: None,
        output_dir: None,
        ice_file: None,
        file_loader: Some(Box::new(VirtualFileLoader)),
        locale_resources: rustc_driver::DEFAULT_LOCALE_RESOURCES.to_vec(),
        lint_caps: Default::default(),
        psess_created: Some(Box::new(move |psess| {
            let sm = psess.clone_source_map();
            let translator = rustc_driver::default_translator();
            let writer: Box<dyn io::Write + Send> = Box::new(DiagnosticWriter::new());
            // `pretty: false` → one JSON object per line, matching the
            // `\n`-delimited contract `DiagnosticWriter` relies on.
            // `HumanReadableErrorType` controls the embedded `rendered`
            // field — kept short: false / unicode: false / Color::Never
            // so the JS side can drop it into the DIAGNOSTICS pane
            // without further parsing.
            let json = JsonEmitter::new(
                writer,
                Some(sm),
                translator,
                /* pretty */ false,
                HumanReadableErrorType { short: false, unicode: false },
                ColorConfig::Never,
            );
            psess.dcx().set_emitter(Box::new(json));
            // Mirrors the `--cfg` flags native Verus passes in its verify
            // phase (`rust_verify/src/driver.rs:270-274`). We can't add these
            // via `--cfg` in argv because rustc's `parse_cfg` would construct
            // a fresh `ParseSess` with the default `RealFileLoader` and
            // `current_directory()` traps on wasm32 (same reason `crate_cfg`
            // is kept empty above).
            //
            //  * `verus_keep_ghost` keeps ghost *stubs* through typeck; alone,
            //    the `verus!` proc-macro's `cfg_erase()` still strips ghost
            //    bodies — see builtin_macros/src/lib.rs. `cfg_erase` evaluates
            //    these via `expand_expr`, which reads `psess.config`.
            //  * `verus_keep_ghost_body` keeps those bodies too, so VIR
            //    construction has real code to lower.
            //  * `verus_only` is user-facing: `#[cfg(verus_only)]` / attrs
            //    like `#[cfg_attr(verus_only, verus::loop_isolation(false))]`.
            //    Omit and pasted Verus snippets silently hit the
            //    `not(verus_only)` branch.
            // `verus_keep_ghost` is set in both modes — only the body cfg
            // differs. See doc comment on `build_rustc_config`.
            psess.config.insert((Symbol::intern("verus_keep_ghost"), None));
            if keep_ghost {
                psess.config.insert((Symbol::intern("verus_keep_ghost_body"), None));
            }
            psess.config.insert((Symbol::intern("verus_only"), None));
            // Match vstd's own feature flags so user code can gate on
            // `#[cfg(feature = "std")]` / `#[cfg(feature = "alloc")]`
            // the way Cargo would. `alloc` is always on (every variant
            // ships liballoc); `std` follows `libstd_available`.
            psess.config.insert((Symbol::intern("feature"), Some(Symbol::intern("alloc"))));
            if libstd_available {
                psess.config.insert((Symbol::intern("feature"), Some(Symbol::intern("std"))));
            }
        })),
        hash_untracked_state: None,
        register_lints: None,
        override_queries: None,
        extra_symbols: vec![],
        make_codegen_backend: None,
        registry: Registry::new(rustc_errors::codes::DIAGNOSTICS),
        using_internal_features: &rustc_driver::USING_INTERNAL_FEATURES,
    }
}

// rustc's `SourceMap::with_inputs` eagerly calls `current_directory()` during
// session setup, but wasm32 `std::env::current_dir()` returns Unsupported.
// Supplying our own FileLoader with a dummy cwd avoids the panic. We feed
// source via `Input::Str`, so read_* is never invoked.
struct VirtualFileLoader;

impl FileLoader for VirtualFileLoader {
    fn file_exists(&self, _: &Path) -> bool {
        false
    }
    fn read_file(&self, _: &Path) -> io::Result<String> {
        Err(io::Error::new(io::ErrorKind::NotFound, "no fs on wasm"))
    }
    fn read_binary_file(&self, _: &Path) -> io::Result<Arc<[u8]>> {
        Err(io::Error::new(io::ErrorKind::NotFound, "no fs on wasm"))
    }
    fn current_directory(&self) -> io::Result<PathBuf> {
        Ok(PathBuf::from("/"))
    }
}


// JsonEmitter writes one diagnostic object per `\n`-terminated line
// (see rustc's `json.rs:94`). Buffer bytes until we see `\n`, then
// hand the complete line to `verus_diagnostic`. Same survivability
// reasoning as before: each diagnostic reaches the host
// synchronously, so a later wasm trap can't lose what already
// flushed. Drop emits the trailing partial buffer so nothing is lost
// on abort-after-emit.
struct DiagnosticWriter {
    pending: Vec<u8>,
}

impl DiagnosticWriter {
    fn new() -> Self {
        Self { pending: Vec::new() }
    }
    fn emit_complete_lines(&mut self) {
        while let Some(idx) = self.pending.iter().position(|&b| b == b'\n') {
            let line = &self.pending[..idx];
            if !line.is_empty() {
                verus_diagnostic(&String::from_utf8_lossy(line));
            }
            self.pending.drain(..idx + 1);
        }
    }
}

impl io::Write for DiagnosticWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        self.emit_complete_lines();
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.emit_complete_lines();
        Ok(())
    }
}

impl Drop for DiagnosticWriter {
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            verus_diagnostic(&String::from_utf8_lossy(&self.pending));
            self.pending.clear();
        }
    }
}

// ── Stage 2: HIR dump ────────────────────────────────────────────────

// Post-expansion AST: the `ast::Crate` held by `resolver_for_lowering` *after*
// macro expansion (`configure_and_expand` in `passes::resolver_for_lowering_raw`).
// Must run before `dump_hir` because `hir_free_items` / HIR lowering consumes
// the AST via `Steal`. We only dump the expanded form — the pre-expansion AST
// is just `verus! { <token tree> }` wrapping source the user can already see
// in the editor, so it wouldn't add anything for the reader.
pub(crate) fn dump_ast(tcx: TyCtxt<'_>) {
    time("dump.ast", || {
        let borrow = tcx.resolver_for_lowering().borrow();
        let (_, krate) = &*borrow;
        let body = rustc_ast_pretty::pprust::crate_to_string_for_macros(krate);
        emit_section("AST", body);
    });
}

pub(crate) fn dump_hir(tcx: TyCtxt<'_>) {
    // Forces macro expansion + name resolution + HIR lowering. Shared setup
    // between the plain dump and the typed variant below; `typeck_body`
    // downstream implicitly depends on it.
    let _ = tcx.resolver_for_lowering();

    time("dump.hir", || {
        // Can't use `print_crate` on wasm: its `Comments::new` path
        // constructs a fresh `SourceMap` with the default `RealFileLoader`,
        // whose `current_directory()` traps on wasm. `item_to_string` goes
        // through the low-level `to_string` (`comments: None`) and skips
        // that path — trade-off: no pretty-printing of source comments,
        // which we don't want in the HIR dump anyway.
        //
        // `hir_free_items` only yields top-level items, but `print_item`
        // recurses through `Nested::{ImplItem,TraitItem,ForeignItem}` via
        // the `PpAnn::nested` dispatch, so impl methods, trait items, and
        // extern blocks are all reached from here.
        let mut body = String::new();
        for item_id in tcx.hir_free_items() {
            let item = tcx.hir_item(item_id);
            body.push_str(&rustc_hir_pretty::item_to_string(&tcx, item));
            body.push('\n');
        }
        emit_section("HIR", body);
    });

    // Typed variant: mirror rustc's `-Zunpretty=hir-typed`
    // (rustc_driver_impl::pretty::HirTypedAnn at pretty.rs:142). Each
    // expression gets wrapped in `(expr as T)` using the inferred type from
    // `typeck_body(body)`. Wrapped in `dep_graph.with_ignore` because
    // typeck is a query and pretty-printing isn't a legal dep-graph node.
    time("dump.hir_typed", || {
        let ann = HirTypedAnn { tcx, typeck_results: Cell::new(None) };
        let mut body = String::new();
        tcx.dep_graph.with_ignore(|| {
            for item_id in tcx.hir_free_items() {
                let item = tcx.hir_item(item_id);
                body.push_str(&rustc_hir_pretty::item_to_string(&ann, item));
                body.push('\n');
            }
        });
        emit_section("HIR_TYPED", body);
    });
}

// Walk top-level functions and pretty-print each one's MIR at three
// lowering stages, the form Miri ultimately interprets being the last:
//   * `mir_built` — raw MIR right after THIR lowering. Borrows still
//     implicit, drops not yet inserted.
//   * `mir_drops_elaborated_and_const_checked` — drops made explicit,
//     const-checking applied. The "ready to optimize" form.
//   * `optimized_mir` — post all MIR opt passes. Exactly what Miri
//     consumes via `eval_entry`.
// Order matters: each query returns `&Steal<Body>` and the next stage
// steals the previous one's output. Once stolen, accessing the
// earlier query panics — so we walk the stages in lowering order and
// don't hold a `Ref` across query calls. Methods inside `impl` blocks
// are reached via `hir_impl_item` so they don't silently drop.
// Constants / statics also have MIR but we skip them for now — most
// user programs care about fn bodies.
pub(crate) fn dump_mir(tcx: TyCtxt<'_>) {
    use rustc_hir::{ItemKind, def_id::LocalDefId};
    use rustc_middle::mir::pretty::MirWriter;
    time("dump.mir", || {
        // Collect mir-available fn LocalDefIds once; the per-stage
        // loops below all walk the same set in the same order.
        let mut def_ids = Vec::<LocalDefId>::new();
        let mut collect = |did: LocalDefId| {
            if tcx.is_mir_available(did.to_def_id()) {
                def_ids.push(did);
            }
        };
        for item_id in tcx.hir_free_items() {
            let item = tcx.hir_item(item_id);
            match item.kind {
                ItemKind::Fn { .. } => collect(item.owner_id.def_id),
                ItemKind::Impl(impl_) => {
                    for impl_item_ref in impl_.items {
                        let impl_item = tcx.hir_impl_item(*impl_item_ref);
                        if let rustc_hir::ImplItemKind::Fn(_, _) = impl_item.kind {
                            collect(impl_item.owner_id.def_id);
                        }
                    }
                }
                _ => {}
            }
        }

        let writer = MirWriter::new(tcx);
        let emit_stage = |section: &'static str, write: &mut dyn FnMut(LocalDefId, &mut Vec<u8>)| {
            let mut buf = Vec::<u8>::new();
            for &did in &def_ids {
                write(did, &mut buf);
                buf.extend_from_slice(b"\n");
            }
            emit_section(section, String::from_utf8_lossy(&buf).into_owned());
        };
        emit_stage("MIR_BUILT", &mut |did, buf| {
            let _ = writer.write_mir_fn(&tcx.mir_built(did).borrow(), buf);
        });
        emit_stage("MIR_DROPS", &mut |did, buf| {
            let _ = writer.write_mir_fn(&tcx.mir_drops_elaborated_and_const_checked(did).borrow(), buf);
        });
        emit_stage("MIR_OPT", &mut |did, buf| {
            let _ = writer.write_mir_fn(tcx.optimized_mir(did.to_def_id()), buf);
        });
    });
}

// Annotator for the `HIR_TYPED` dump. `maybe_typeck_results` tracks the
// currently-active body so nested exprs pick up the right `TypeckResults`
// (bodies can contain inner bodies via closures / async). The fallback in
// `post` via `hir_maybe_body_owned_by` handles the rare case where an expr
// is reached outside a Body nesting (e.g. const evaluation paths).
struct HirTypedAnn<'tcx> {
    tcx: TyCtxt<'tcx>,
    typeck_results: Cell<Option<&'tcx rustc_middle::ty::TypeckResults<'tcx>>>,
}

impl<'tcx> rustc_hir_pretty::PpAnn for HirTypedAnn<'tcx> {
    fn nested(&self, state: &mut rustc_hir_pretty::State<'_>, nested: rustc_hir_pretty::Nested) {
        let prev = self.typeck_results.get();
        if let rustc_hir_pretty::Nested::Body(id) = nested {
            self.typeck_results.set(Some(self.tcx.typeck_body(id)));
        }
        // Delegate the actual nested-node printing to the TyCtxt PpAnn impl
        // (which routes Nested::ImplItem / TraitItem / ForeignItem / Body
        // to the right printer). Casting through `&dyn HirTyCtxt` picks up
        // the `impl PpAnn for &dyn HirTyCtxt<'_>` in rustc_hir_pretty:58.
        let tcx_ann: &dyn rustc_hir::intravisit::HirTyCtxt<'_> = &self.tcx;
        tcx_ann.nested(state, nested);
        self.typeck_results.set(prev);
    }

    fn pre(&self, s: &mut rustc_hir_pretty::State<'_>, node: rustc_hir_pretty::AnnNode<'_>) {
        if matches!(node, rustc_hir_pretty::AnnNode::Expr(_)) {
            s.popen();
        }
    }

    fn post(&self, s: &mut rustc_hir_pretty::State<'_>, node: rustc_hir_pretty::AnnNode<'_>) {
        if let rustc_hir_pretty::AnnNode::Expr(expr) = node {
            let tr = self.typeck_results.get().or_else(|| {
                self.tcx
                    .hir_maybe_body_owned_by(expr.hir_id.owner.def_id)
                    .map(|body| self.tcx.typeck_body(body.id()))
            });
            if let Some(tr) = tr {
                s.space();
                s.word("as");
                s.space();
                s.word(tr.expr_ty(expr).to_string());
            }
            s.pclose();
        }
    }
}
