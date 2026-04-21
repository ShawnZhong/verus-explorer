// verus-explorer — browser-based exploration of Verus's internal representations.
//
// Compiles `vir` and `air` (as-is, via path dependencies) to wasm32 and exposes
// a wasm-bindgen entry point that runs the rustc front-end on Rust source,
// lowers HIR → simplified VIR, and drives the krate through ast_to_sst → poly →
// sst_to_air → `air::context::Context`. SMT is routed through the wasm32
// `SmtProcess` shim in `air/src/smt_process.rs`, which calls the `Z3_*`
// wrappers installed by `public/index.html` on top of the self-hosted
// single-threaded Z3 wasm.
//
// `rustc_*` crates are not Cargo deps — they're built as wasm32 rlibs by the
// `rustc-rlibs` workspace member and resolved at link time via the
// `-L dependency=...` rustflag in `.cargo/config.toml`.
//
// ── File layout (top-down, follows the pipeline) ─────────────────────────
//   1. JS externs                    — what the browser host provides.
//   2. Public entry points           — the `#[wasm_bindgen]` surface.
//   3. Wasm-libs implementation      — in-memory filesystem for rustc's
//                                      crate locator; backs the wasm_libs_*
//                                      entry points directly above.
//   4. Small utilities               — `time`, `emit_section`.
//   5. Pipeline driver               — `parse_and_verify` → `run_pipeline`
//                                      orchestrates the four stages below.
//   6. Stage 1: rustc invocation     — config + `VirtualFileLoader` +
//                                      `DomWriter` diagnostic plumbing.
//   7. Stage 2: HIR dump
//   8. Stage 3: HIR → VIR            — `build_vir` + vstd deserialize cache.
//   9. Stage 4: VIR → AIR → Z3       — the bulk of the file.

#![feature(rustc_private)]

extern crate rustc_ast;
extern crate rustc_hir;
extern crate rustc_ast_pretty;
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_hir_pretty;
extern crate rustc_interface;
extern crate rustc_metadata;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

use std::cell::Cell;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use air::ast::{Command, CommandX};
use air::context::{Context, SmtSolver, ValidityResult};
use air::messages::{Diagnostics, MessageLevel};
use rust_verify::buckets::Bucket;
use rust_verify::cargo_verus_dep_tracker::DepTracker;
use rust_verify::commands::{OpGenerator, OpKind, QueryOp, Style};
use rust_verify::config::ArgsX;
use rust_verify::expand_errors_driver::ExpandErrorsResult;
use rust_verify::import_export::CrateWithMetadata;
use rust_verify::spans::SpanContext;
use rust_verify::verifier::{Reporter, Verifier};
use rustc_errors::DiagInner;
use rustc_errors::emitter::{ColorConfig, Emitter, HumanEmitter, HumanReadableErrorType};
use rustc_errors::json::JsonEmitter;
use rustc_errors::registry::Registry;
use rustc_errors::translation::Translator;
use rustc_errors::{AutoStream, ColorChoice};
use rustc_span::source_map::SourceMap;
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;
use rustc_session::EarlyDiagCtxt;
use rustc_session::config::{self, ErrorOutputType, Input};
use rustc_span::def_id::LOCAL_CRATE;
use rustc_span::source_map::FileLoader;
use rustc_span::{FileName, Symbol};
use vir::ast::{ArchWordBits, Datatype, Fun, Krate, Path as VirPath, VirErr};
use vir::ast_util::{fun_as_friendly_rust_name, is_visible_to};
use vir::context::{Ctx, GlobalCtx};
use vir::messages::{ToAny, VirMessageInterface};
use vir::prelude::PreludeConfig;
use vir::def::ProverChoice;
use vir::sst::{AssertId, KrateSst};
use wasm_bindgen::prelude::*;

// ═══════════════════════════════════════════════════════════════════════
// 1. JS externs
// ═══════════════════════════════════════════════════════════════════════

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(msg: &str);

    // Imported from `public/index.html`. Called synchronously from
    // `DomWriter` so each rustc diagnostic lands in the output panel before
    // rustc's `abort_if_errors` turns into a wasm `unreachable` trap.
    #[wasm_bindgen(js_name = verus_diagnostic)]
    fn verus_diagnostic(msg: &str);

    // Same survivability reasoning as `verus_diagnostic`, but carries the
    // structured JsonEmitter output (one diagnostic per line). The JS side
    // parses it into `byte_start`/`byte_end` + `line`/`col` spans and feeds
    // CM6 `setDiagnostics` — gives us precise squiggle ranges and
    // secondary-label spans without scraping the human-readable text.
    #[wasm_bindgen(js_name = verus_diagnostic_json)]
    fn verus_diagnostic_json(msg: &str);

    // Streams each completed pipeline section (AST / HIR / VIR /
    // AIR_INITIAL / AIR_MIDDLE / AIR_FINAL / SMT / VERDICT) out to the
    // browser as soon as it's formatted. Same survivability reasoning as
    // `verus_diagnostic`: a later stage that traps the wasm instance
    // (rustc's `abort_if_errors` → `unreachable`) would otherwise discard
    // the whole returned String, hiding every section we'd already built.
    //
    // Content is passed as two parallel arrays describing ordered blocks
    // that JS concatenates into one body: `contents[i]` is the block
    // text, `folds[i]` is 1 when the block should auto-fold on render.
    // No JS-inserted chrome — the natural `;;` comments that AIR / Verus
    // already emit (`;; AIR prelude`, `;; Function-Def foo`, the
    // explorer-inserted `;; vstd` separator on VIR / SST) serve as the
    // visible first line of each block, and the fold range is
    // [end-of-first-line, end-of-block]. Rust owns all section boundary
    // decisions; JS only concatenates and folds.
    #[wasm_bindgen(js_name = verus_dump)]
    fn verus_dump(section: &str, contents: Vec<String>, folds: Vec<u8>);

    // Stage-level timing. `time()` emits one call per stage with the elapsed
    // ms. `public/index.html` and `tests/smoke.rs` both install a stub on
    // globalThis (the former logs to console, the latter to stderr). Kept
    // out-of-band from `verus_dump` so timings don't clutter the UI output
    // sections.
    #[wasm_bindgen(js_namespace = performance, js_name = now)]
    pub fn perf_now() -> f64;

    #[wasm_bindgen(js_name = verus_bench)]
    fn verus_bench(label: &str, ms: f64);

    // Stamp a `;; <label>` banner into the Z3 response buffer. Called from
    // `run_queries` right before each op's commands are fed to Z3 so the
    // replies (sat / unsat / empty / errors) read as per-op stanzas in the
    // Z3 tab instead of a flat positional stream.
    #[wasm_bindgen(js_name = verus_z3_annotate)]
    fn verus_z3_annotate(label: &str);
}

// ═══════════════════════════════════════════════════════════════════════
// 2. Public entry points (#[wasm_bindgen])
// ═══════════════════════════════════════════════════════════════════════

// `#[wasm_bindgen(start)]` fires when this crate is the final cdylib (the
// browser build via `wasm-pack build`). Integration tests link us as an
// rlib into `wasm-bindgen-test`'s own cdylib, so the start hook doesn't
// run there — those tests call `init()` explicitly.
#[wasm_bindgen(start)]
pub fn init() {
    std::panic::set_hook(Box::new(|info| console_error(&info.to_string())));
    // rustc-in-wasm has no dlopen, so the normal `dlsym_proc_macros` path
    // in `rustc_metadata::creader` can't load `_rustc_proc_macro_decls_*`
    // from a host dylib. Both verus macro crates are regular rlibs (not
    // `proc-macro = true`) exposing `pub macro NAME` shim stubs for name
    // resolution plus a `MACROS` descriptor slice for expansion. Registering
    // swaps each stub's kind via the patched
    // `rustc_resolve::build_reduced_graph::get_macro_by_def_id` path.
    rustc_metadata::proc_macro_registry::register(
        "verus_builtin_macros",
        verus_builtin_macros::MACROS,
    );
    rustc_metadata::proc_macro_registry::register(
        "verus_state_machines_macros",
        verus_state_machines_macros::MACROS,
    );
}

/// Register one wasm-libs file (rmeta or `vstd.vir`) fetched by the JS
/// loader from `./wasm-libs/<name>`. Call once per manifest entry, then call
/// `wasm_libs_finalize` before the first `parse_source` invocation.
#[wasm_bindgen]
pub fn wasm_libs_add_file(name: String, bytes: Vec<u8>) {
    // `name` and `bytes` are leaked into `'static` storage, which is fine
    // because this runs at startup on a single-use wasm instance that's
    // discarded after one `parse_source` call.
    let name: &'static str = Box::leak(name.into_boxed_str());
    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    WASM_LIBS_PENDING.lock().unwrap().push((name, bytes));
}

/// Freeze the registered files and wire up rustc's filesearch callbacks.
/// Must be called after all `wasm_libs_add_file` calls for this wasm instance.
#[wasm_bindgen]
pub fn wasm_libs_finalize() {
    let files = std::mem::take(&mut *WASM_LIBS_PENDING.lock().unwrap());
    WASM_LIBS_BUNDLE
        .set(WasmLibs { files })
        .ok()
        .expect("wasm_libs_finalize called twice");
    rustc_session::filesearch::sysroot::install(
        rustc_session::filesearch::sysroot::Callbacks {
            list: wasm_libs_list,
            read: wasm_libs_read,
        },
    );
}

/// Run the rustc front-end on `src`, lower HIR → simplified VIR, then drive
/// the krate through the AIR generation + Z3 pipeline. Streams each IR
/// section (AST / HIR / VIR / AIR_INITIAL / AIR_MIDDLE / AIR_FINAL / SMT /
/// VERDICT) to the host via the `verus_dump` JS extern; the browser caches
/// the bodies and toggles rendering without re-parsing.
#[wasm_bindgen]
pub fn parse_source(src: &str, expand_errors: bool) {
    parse_and_verify(src, /* verify */ true, expand_errors)
}

// ═══════════════════════════════════════════════════════════════════════
// 3. Wasm-libs: in-memory filesystem for rustc's crate locator
// ═══════════════════════════════════════════════════════════════════════
//
// Supplies `libcore.rmeta`, `libvstd.rmeta`, and friends to rustc's crate
// locator so name resolution can resolve `extern crate core/alloc/vstd`
// without a real filesystem. Also carries the bincode-serialized `vstd.vir`
// consumed by `build_vir`.
//
// Bytes are not bundled into the wasm via `include_bytes!`. Instead the
// browser loader fetches each rmeta + `vstd.vir` from `./wasm-libs/` (laid
// out by `build.rs`, copied into `dist/` by the Makefile) and streams them
// in one-by-one through `wasm_libs_add_file`, then calls `wasm_libs_finalize`
// to register rustc's filesearch callbacks. Keeping ~60 MB of rmetas + .vir
// out of the wasm shrinks the binary (~83 MB → ~23 MB), lets HTTP gzip
// compress each artifact, and gives the browser independent cache entries
// per crate.
//
// The same sync contract rustc expects still holds:
//   * `list(dir)` — directory listing for `SearchPath::new` in
//     `rustc_session::search_paths`.
//   * `read(path)` — rmeta bytes for `get_rmeta_metadata_section` in
//     `rustc_metadata::locator`.
//
// `--sysroot=/virtual` is passed by `build_rustc_config`; rustc then
// derives `/virtual/lib/rustlib/wasm32-unknown-unknown/lib` as the target-
// lib path, which is the single directory we answer listings for.

const VIRTUAL_LIB_DIR: &str = "/virtual/lib/rustlib/wasm32-unknown-unknown/lib";
const VSTD_VIR: &str = "vstd.vir";

struct WasmLibs {
    // Names and bytes are `&'static` because `wasm_libs_add_file` leaks them
    // via `Box::leak` — both last for the process lifetime, matching the
    // `&'static [u8]` return type of the filesearch `read` callback.
    files: Vec<(&'static str, &'static [u8])>,
}

// Files accumulate here as JS streams them in; `wasm_libs_finalize` drains
// this into `WASM_LIBS_BUNDLE`. Wrapped in a `Mutex` only to satisfy
// static-init — wasm is single-threaded, so contention is impossible.
static WASM_LIBS_PENDING: Mutex<Vec<(&'static str, &'static [u8])>> = Mutex::new(Vec::new());
static WASM_LIBS_BUNDLE: OnceLock<WasmLibs> = OnceLock::new();

fn wasm_libs() -> &'static WasmLibs {
    WASM_LIBS_BUNDLE
        .get()
        .expect("wasm_libs_finalize must be called before rustc runs")
}

fn wasm_libs_list(dir: &Path) -> Option<Vec<(String, PathBuf)>> {
    if dir != Path::new(VIRTUAL_LIB_DIR) {
        return None;
    }
    Some(
        wasm_libs()
            .files
            .iter()
            .map(|(name, _)| {
                ((*name).to_string(), PathBuf::from(format!("{VIRTUAL_LIB_DIR}/{name}")))
            })
            .collect(),
    )
}

fn wasm_libs_read(path: &Path) -> Option<&'static [u8]> {
    let name = path.file_name()?.to_str()?;
    wasm_libs().files.iter().find(|(n, _)| *n == name).map(|(_, data)| *data)
}

/// Bytes of the bundled `vstd.vir` (bincode-serialized VIR krate), consumed
/// by `build_vir`. Returns `&[]` if no such file is in the bundle, which
/// surfaces as a clean bincode deserialization error upstream.
fn wasm_libs_vstd_vir() -> &'static [u8] {
    wasm_libs().files.iter().find(|(n, _)| *n == VSTD_VIR).map(|(_, d)| *d).unwrap_or_default()
}

// ═══════════════════════════════════════════════════════════════════════
// 4. Small utilities
// ═══════════════════════════════════════════════════════════════════════

// Wrap a pipeline stage with a wall-clock timer. Result: one `verus_bench`
// call per stage, forwarded to console (browser) or stderr (smoke test).
// Kept synchronous + infallible so it composes cleanly around both closures
// and plain expressions; `perf_now` is a raw JS import so the overhead is
// two foreign calls per stage — negligible next to the stages themselves.
fn time<T>(label: &'static str, f: impl FnOnce() -> T) -> T {
    let t0 = perf_now();
    let result = f();
    verus_bench(label, perf_now() - t0);
    result
}

// A chunk of content within a `Section`. The content's own first line
// is what stays visible when folded — so callers that want a visible
// label (AIR's `;; AIR prelude`, Verus's `;; Function-Def foo`, or the
// explorer-inserted `;; vstd` on VIR / SST) put it at the top of the
// content. `fold: true` asks JS to auto-collapse the block from that
// first line's end to end-of-content.
struct Block {
    content: String,
    fold: bool,
}

// An ordered list of `Block`s that together form one logical output tab.
// Most sections are a single `Block` with no fold; VIR / SST_AST /
// SST_POLY use two (vstd-with-`;; vstd` header + user) and AIR / SMT
// use many (prelude + one per op).
struct Section {
    name: &'static str,
    blocks: Vec<Block>,
}

impl Section {
    // Shorthand for the common single-block, no-fold case.
    fn single(name: &'static str, content: String) -> Self {
        Section { name, blocks: vec![Block { content, fold: false }] }
    }
}

// Streams a completed section to the browser via the `verus_dump` JS
// extern. Synchronous by design: a later stage that traps the wasm
// instance (rustc's `abort_if_errors` → `unreachable`) can't discard
// sections already handed off to JS. Callers of this crate (the browser
// and `tests/smoke.rs`) observe pipeline output exclusively through the
// JS callbacks — no String accumulator is threaded through.
fn emit_section(section: Section) {
    let n = section.blocks.len();
    let mut contents = Vec::with_capacity(n);
    let mut folds = Vec::with_capacity(n);
    for b in section.blocks {
        let mut c = b.content;
        c.truncate(c.trim_end().len());
        contents.push(c);
        folds.push(b.fold as u8);
    }
    verus_dump(section.name, contents, folds);
}

// ═══════════════════════════════════════════════════════════════════════
// 5. Pipeline driver
// ═══════════════════════════════════════════════════════════════════════

/// Parse `src` via rustc_interface, force HIR lowering, build VIR, then drive
/// the krate through the AIR + Z3 pipeline. Pipeline output is streamed out
/// to the host (browser / test runner) section-by-section via the
/// `verus_dump` / `verus_diagnostic*` / `verus_bench` JS externs — no
/// return value is threaded through.
///
/// `verify` gates the AIR→Z3 stage. The wasm-bindgen `parse_source` always
/// passes `true`; integration tests in `tests/` can pass `false` so the
/// pipeline stops after VIR and doesn't call into the `Z3_*` shims (which
/// only `public/index.html` installs — not the wasm-bindgen-test harness).
pub fn parse_and_verify(src: &str, verify: bool, expand_errors: bool) {
    // vstd is wired into the extern prelude via `--extern=vstd` in
    // `build_rustc_config`, so the user's source is passed through unmodified.
    // Keeping the source 1:1 with what the editor shows is what lets
    // diagnostic line numbers land on the right editor line.
    let src = src.to_string();
    // wasm32 has no unwinding (panic = abort), so `catch_unwind` would be a
    // no-op here — any panic aborts the instance before this returns.
    // Partial state the pipeline already handed off via `verus_dump` /
    // `verus_diagnostic` stays in the host, which is the whole survivability
    // story.
    rustc_interface::interface::run_compiler(build_rustc_config(src), |compiler| {
        run_pipeline(compiler, verify, expand_errors);
    });
}

fn run_pipeline(compiler: &Compiler, verify: bool, expand_errors: bool) {
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
        dump_vir_and_verify(compiler, tcx, verify, expand_errors);
    });
}

fn dump_ast_pre_expansion(krate: &rustc_ast::Crate) {
    time("dump.ast_pre", || {
        let body = rustc_ast_pretty::pprust::crate_to_string_for_macros(krate);
        emit_section(Section::single("AST_PRE", body));
    });
}

// ═══════════════════════════════════════════════════════════════════════
// 6. Stage 1: rustc invocation + diagnostic plumbing
// ═══════════════════════════════════════════════════════════════════════

// `--sysroot=/virtual` pairs with the filesearch callbacks installed by
// `wasm_libs_finalize` — rustc's crate locator finds `libcore.rmeta` (and
// friends), plus our prebuilt `libverus_builtin.rmeta`, in the wasm-libs
// bundle instead of on disk. `#![no_std]` keeps std out (only `core` is
// needed), and the caller prepends `extern crate verus_builtin;` so that
// crate is linked and its `#[rustc_diagnostic_item]` registrations fire —
// Verus keys its builtin lookups off those.
fn build_rustc_config(src: String) -> rustc_interface::interface::Config {
    let argv: Vec<String> = [
        "--edition=2021",
        "--crate-type=lib",
        "--crate-name=v",
        "--sysroot=/virtual",
        // `--extern=vstd` puts vstd in the edition-2018+ extern prelude so
        // user code can `use vstd::prelude::*;` directly. We used to prepend
        // `extern crate vstd;\n` to the source instead, but that shifted
        // every diagnostic's line number by one — breaking the in-editor
        // error-line highlight. No `=PATH` needed: rustc's crate locator
        // finds `libvstd.rmeta` via the wasm-libs sysroot bundle.
        "--extern=vstd",
        "-Zcrate-attr=no_std",
        "-Zcrate-attr=feature(register_tool)",
        // `verus!` expansion emits `#[...]` attributes on expressions
        // (e.g. `#[verus::internal(...)] foo`) — unstable without this.
        "-Zcrate-attr=feature(stmt_expr_attributes)",
        "-Zcrate-attr=feature(proc_macro_hygiene)",
        "-Zcrate-attr=register_tool(verus)",
        "-Zcrate-attr=register_tool(verifier)",
        // `verus!` macro expansion triggers a pile of rustc lints that are
        // false positives against the *source* (e.g. `unused_parens` on
        // `x * (x - 1)` where dropping the parens changes precedence;
        // `non_shorthand_field_patterns` on `MyStruct { f }` rewritten to
        // `{ f: f }`). This is the same list Verus's own driver suppresses
        // — see `rust_verify/src/driver.rs`.
        "-Aunused_imports", "-Aunused_variables", "-Aunused_assignments",
        "-Aunreachable_patterns", "-Aunused_parens", "-Aunused_braces",
        "-Adead_code", "-Aunreachable_code", "-Aunused_mut", "-Aunused_labels",
        "-Aunused_attributes", "-Anon_shorthand_field_patterns",
    ]
    .into_iter()
    .map(String::from)
    .collect();

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
            let human_writer: Box<dyn io::Write + Send> = Box::new(DomWriter::new());
            let human_dst = AutoStream::new(human_writer, ColorChoice::Never);
            let human = HumanEmitter::new(human_dst, translator.clone()).sm(Some(sm.clone()));
            let json_writer: Box<dyn io::Write + Send> = Box::new(JsonDomWriter::new());
            // `pretty: false` → one JSON object per line, matching the
            // `\n`-delimited contract `JsonDomWriter` relies on. `json_rendered`
            // controls the `rendered` field's formatting; we don't consume it
            // on the JS side but keeping `short: false` keeps the shape
            // stable in case we want it later.
            let json = JsonEmitter::new(
                json_writer,
                Some(sm),
                translator,
                /* pretty */ false,
                HumanReadableErrorType { short: false, unicode: false },
                ColorConfig::Never,
            );
            psess.dcx().set_emitter(Box::new(MultiEmitter { human, json }));
            // `verus_keep_ghost` alone keeps ghost *stubs* (enough for typeck)
            // but the `verus!` proc-macro's `cfg_erase()` strips ghost bodies
            // unless `verus_keep_ghost_body` is also on — see
            // builtin_macros/src/lib.rs. `cfg_erase` evaluates these via
            // `expand_expr`, which reads `psess.config`.
            psess.config.insert((Symbol::intern("verus_keep_ghost"), None));
            psess.config.insert((Symbol::intern("verus_keep_ghost_body"), None));
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

// wasm32 has panic=abort, so `catch_unwind` can't recover from rustc's
// `abort_if_errors` (which fires on return from `run_compiler` whenever a
// diagnostic was emitted). That panic degrades to `unreachable` and traps
// the wasm instance, so `parse_source` never returns and any error text
// buffered into the return String is lost.
//
// To work around it, we push each diagnostic *synchronously* out to
// `public/index.html`'s imported `verus_diagnostic` JS function, which
// appends a styled block to the output panel. Bytes that reach JS before
// the trap stay in the DOM regardless.
//
// rustc's `HumanEmitter` writes a single diagnostic in several
// `write_all`+`flush` cycles (header, source span, suggestions). Flushing
// each cycle separately would chop one diagnostic into many UI entries. We
// coalesce by emitting only on the blank-line separator rustc inserts
// between diagnostics — anything else is held until the next flush. Drop
// emits the trailing partial buffer so nothing is lost on abort-after-emit.
struct DomWriter {
    pending: Vec<u8>,
}

impl DomWriter {
    fn new() -> Self {
        Self { pending: Vec::new() }
    }
    fn emit_complete_blocks(&mut self) {
        while let Some(idx) = find_block_end(&self.pending) {
            emit_block(&self.pending[..idx]);
            self.pending.drain(..idx + 2);
        }
    }
}

fn find_block_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

// Forward a completed diagnostic block to the UI, with one exception: rustc's
// session-teardown footer `error: aborting due to N previous error[s]` is pure
// duplication of our verdict headline (`N/M queries failed`), so drop it.
// Emitted by `DiagCtxtInner::print_error_count` through the same HumanEmitter
// we attached in `psess_created`, which is why it shows up here at all.
fn emit_block(block: &[u8]) {
    if block.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(block);
    if text.starts_with("error: aborting due to ") {
        return;
    }
    verus_diagnostic(&text);
}

impl io::Write for DomWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        self.emit_complete_blocks();
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.emit_complete_blocks();
        Ok(())
    }
}

impl Drop for DomWriter {
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            emit_block(&self.pending);
            self.pending.clear();
        }
    }
}

// Sister of `DomWriter` for the JSON side. `JsonEmitter` writes one object
// per diagnostic terminated by `\n` (see `json.rs:94`). Buffer bytes until
// we see `\n`, then hand the complete line to `verus_diagnostic_json`.
// Unlike the human path we don't need to coalesce multiple writer cycles
// into one UI entry — the JSON emitter produces one line per diagnostic.
struct JsonDomWriter {
    pending: Vec<u8>,
}

impl JsonDomWriter {
    fn new() -> Self {
        Self { pending: Vec::new() }
    }
    fn emit_complete_lines(&mut self) {
        while let Some(idx) = self.pending.iter().position(|&b| b == b'\n') {
            let line = &self.pending[..idx];
            if !line.is_empty() {
                verus_diagnostic_json(&String::from_utf8_lossy(line));
            }
            self.pending.drain(..idx + 1);
        }
    }
}

impl io::Write for JsonDomWriter {
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

impl Drop for JsonDomWriter {
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            verus_diagnostic_json(&String::from_utf8_lossy(&self.pending));
            self.pending.clear();
        }
    }
}

// Fan-out emitter: every diagnostic goes to both the HumanEmitter (text →
// DIAGNOSTICS pane) and the JsonEmitter (structured → CM6 inline squiggles).
// `DiagInner` is `Clone`, so we can hand each side its own copy.
// `source_map`/`translator` both delegate to the human side because the
// `Emitter` trait expects single references; the JSON side carries its own
// clones internally.
struct MultiEmitter {
    human: HumanEmitter,
    json: JsonEmitter,
}

impl Emitter for MultiEmitter {
    fn emit_diagnostic(&mut self, diag: DiagInner, registry: &Registry) {
        self.json.emit_diagnostic(diag.clone(), registry);
        self.human.emit_diagnostic(diag, registry);
    }
    fn source_map(&self) -> Option<&SourceMap> {
        self.human.source_map()
    }
    fn translator(&self) -> &Translator {
        self.human.translator()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 7. Stage 2: HIR dump
// ═══════════════════════════════════════════════════════════════════════

// Post-expansion AST: the `ast::Crate` held by `resolver_for_lowering` *after*
// macro expansion (`configure_and_expand` in `passes::resolver_for_lowering_raw`).
// Must run before `dump_hir` because `hir_free_items` / HIR lowering consumes
// the AST via `Steal`. We only dump the expanded form — the pre-expansion AST
// is just `verus! { <token tree> }` wrapping source the user can already see
// in the editor, so it wouldn't add anything for the reader.
fn dump_ast(tcx: TyCtxt<'_>) {
    time("dump.ast", || {
        let borrow = tcx.resolver_for_lowering().borrow();
        let (_, krate) = &*borrow;
        let body = rustc_ast_pretty::pprust::crate_to_string_for_macros(krate);
        emit_section(Section::single("AST", body));
    });
}

fn dump_hir(tcx: TyCtxt<'_>) {
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
        emit_section(Section::single("HIR", body));
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
        emit_section(Section::single("HIR_TYPED", body));
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

// ═══════════════════════════════════════════════════════════════════════
// 8. Stage 3: HIR → VIR
// ═══════════════════════════════════════════════════════════════════════

// Feed one per-item (crate, span, text) tuple from `walk_krate*` into
// `blocks`. Fold iff the item isn't local user code — external-crate
// items (`krate.is_some()`) and Verus-generated synthetics (`span ==
// "no location"`) collapse; items in the default crate stay expanded.
// Parallels the AIR/SMT drain rule in `run_queries`. Adjacent folded
// entries merge so vstd runs collapse into one row.
fn push_item(blocks: &mut Vec<Block>, krate: Option<Arc<String>>, span: &str, text: String) {
    let fold = krate.is_some() || span == "no location";
    let content =
        if span.is_empty() { text } else { format!(";; {}\n{}", span, text) };
    if let Some(prev) = blocks.last_mut() {
        if fold && prev.fold {
            prev.content.push('\n');
            prev.content.push_str(&content);
            return;
        }
    }
    blocks.push(Block { content, fold });
}

// Push a `;; <name>` section header that starts a fresh folded block.
// Subsequent `push_item` calls with `fold: true` merge into it so the
// banner sits at the top of the fold region and becomes the visible
// label when collapsed. Unlike `push_item`, never merges with the
// previous block — keeps each section's fold self-contained.
fn push_banner(blocks: &mut Vec<Block>, name: &str) {
    blocks.push(Block { content: format!(";; {}", name), fold: true });
}

fn dump_vir_and_verify(
    compiler: &Compiler,
    tcx: TyCtxt<'_>,
    verify: bool,
    expand_errors: bool,
) {
    // `build_vir` forces HIR lowering + name resolution + ty-check via Verus'
    // `build_vir_crate`, so this stage absorbs most of the rustc front-end
    // work. Split from `verify` below to separate rustc cost from Verus cost.
    //
    // Any `VirErr` that escapes `build_vir` has already been routed through
    // the vendored `build_vir_crate`'s reporter (verifier.rs ~L2142),
    // matching upstream's `after_expansion` handler. The DIAGNOSTICS section
    // will render it with full span context; the rest of the pipeline (SST,
    // AIR, SMT) has nothing to dump from a failed build, so we just return.
    let Ok((krate, global_ctx, crate_name, spans)) =
        time("build_vir", || build_vir(compiler, tcx))
    else {
        return;
    };
    time("dump.vir", || {
        use vir::printer::WalkEvent;
        let mut blocks = Vec::new();
        vir::printer::walk_krate(&krate, &vir::printer::COMPACT_TONODEOPTS, |event| match event {
            WalkEvent::Section(name) => push_banner(&mut blocks, name),
            WalkEvent::Item { krate, span, text } => push_item(&mut blocks, krate, span, text),
        });
        emit_section(Section { name: "VIR", blocks });
    });
    if !verify {
        return;
    }
    // Thread `output` in by-ref so dumps from earlier modules / earlier
    // pipeline stages survive a later failure. Upstream Verus bails with `?`
    // on the first module error, which would otherwise discard every SST /
    // AIR / SMT section accumulated up to that point and leave the UI
    // showing only VIR.
    let mut output = VerifyOutput::default();
    // Any `VirErr` that bubbles out has already been routed through the
    // reporter → DiagCtxt inside `verify_simplified_krate`, matching
    // upstream Verus' `finish_verus` handler (verifier.rs:3531). So we
    // just write whatever dumps we managed to accumulate and let the
    // HumanEmitter-backed DIAGNOSTICS section render the error.
    let _ = time("verify", || {
        verify_simplified_krate(krate, global_ctx, crate_name, compiler, &spans, expand_errors, &mut output)
    });
    write_verify_output(output);
}

// Drives Verus's HIR→VIR pipeline. `Verifier::build_vir_crate` (vendored
// addition) derives the inputs `construct_vir_crate` needs from (tcx, compiler),
// runs HIR → raw VIR, then the head of `verify_crate_inner` (GlobalCtx +
// check_traits + ast_simplify), returning both the simplified krate and the
// (mutated) GlobalCtx so we can drive the downstream prune → Ctx →
// ast_to_sst → AIR pipeline ourselves.
fn build_vir<'tcx>(
    compiler: &Compiler,
    tcx: TyCtxt<'tcx>,
) -> Result<(Krate, GlobalCtx, Arc<String>, SpanContext), Vec<VirErr>> {
    let mut args = ArgsX::new();
    // `Vstd::Imported` is the default and matches the user's
    // `extern crate vstd;` injection. The vstd VIR is served out of the
    // fetched wasm-libs bundle (`wasm_libs_vstd_vir()`) and passed straight
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

// ═══════════════════════════════════════════════════════════════════════
// 9. Stage 4: VIR → AIR → Z3
// ═══════════════════════════════════════════════════════════════════════
//
// Drives a fully-simplified Verus VIR krate through prune → Ctx → ast_to_sst →
// poly → AIR generation → Z3, returning the dumped AIR text and per-query
// verdicts. Mirrors `Verifier::verify_bucket` in `rust_verify/src/verifier.rs`
// but skips the bucket/spinoff/recommends/progress-bar/multi-thread machinery —
// the explorer only needs the core VIR→AIR→SMT pipeline.
//
// The Z3 backend is `air::context::Context`, which on wasm32 routes through
// the `Z3_*` shims declared in `air/src/smt_process.rs`.

// -------- data types --------

#[derive(Default)]
struct VerifyOutput {
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
    fn feed_all(&mut self, cmds: &[Command]) {
        for cmd in cmds {
            self.feed(cmd);
        }
    }
}

// -------- drivers --------

fn verify_simplified_krate<'tcx>(
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
        // Cap each Z3 query at ~10 seconds of solver work. Matches upstream
        // Verus' documented `--rlimit=10` CLI default (`RLIMIT_PER_SECOND`
        // = 3_000_000 in `verifier.rs:50`). Upstream's `ArgsX::new` default
        // is `f32::INFINITY`, but that's only appropriate when a human can
        // Ctrl-C; a pathological assert in the browser would otherwise hang
        // the tab with no abort path. 10s is generous for the small snippets
        // the explorer serves.
        c.set_rlimit(10 * 3_000_000);
        c
    });
    let bufs = LogBufs::attach(&mut air_ctx);
    let mut feeder = Feeder { air_ctx: &mut air_ctx, msg: mctx.msg, reporter: mctx.reporter };
    time("verify.queries", || {
        run_queries(&mut feeder, &bufs, &mut ctx, &krate_sst, bucket_funs, output, &module_path, mctx)
    })?;
    // `ctx.free()` drops the LogBufs-attached Z3 context → `Z3_del_context`.
    // Any deferred solver teardown shows up here.
    Ok(time("verify.ctx_free", || ctx.free()))
}

// Prelude, fuel, trait/assoc/datatype/opaque decls, plus per-function symbol
// declarations. Order matches `Verifier::verify_bucket`.
fn feed_module_decls(
    feeder: &mut Feeder,
    ctx: &mut Ctx,
    krate_sst: &KrateSst,
    visible_dts: &Vec<Datatype>,
    mctx: &ModuleCtx,
) -> Result<(), VirErr> {
    feeder.feed_all(&Ctx::prelude(PreludeConfig {
        arch_word_bits: mctx.arch_word_bits,
        solver: mctx.solver,
    }));
    feeder.feed_all(&ctx.fuel());
    feeder.feed_all(&vir::traits::trait_decls_to_air(ctx, krate_sst));
    feeder.feed_all(&vir::assoc_types_to_air::assoc_type_decls_to_air(ctx, &krate_sst.traits));
    feeder.feed_all(&vir::datatype_to_air::datatypes_and_primitives_to_air(ctx, visible_dts));
    feeder.feed_all(&vir::traits::trait_bound_axioms(ctx, &krate_sst.traits));
    feeder.feed_all(&vir::assoc_types_to_air::assoc_type_impls_to_air(ctx, &krate_sst.assoc_type_impls));
    feeder.feed_all(&vir::opaque_type_to_air::opaque_types_to_air(ctx, &krate_sst.opaque_types));
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
    module_path: &VirPath,
    mctx: &ModuleCtx,
) -> Result<(), VirErr> {
    // Feed the module-scoped AIR prelude (axioms, fuel, datatype /
    // trait / assoc / function-name decls) that every query depends
    // on, then drain it as a single folded block — it's bulky
    // boilerplate the reader rarely wants expanded. Each op below
    // drains into its own expanded block so queries / context ops
    // read linearly beneath the collapsed prelude.
    let visible_dts: Vec<Datatype> = krate_sst.datatypes.iter()
        .filter(|d| is_visible_to(&d.x.visibility, module_path))
        .cloned()
        .collect();
    verus_z3_annotate("AIR prelude");
    time("verify.feed_decls", || feed_module_decls(feeder, ctx, krate_sst, &visible_dts, mctx))?;
    bufs.drain_block(&mut output.air_blocks, /* fold */ true);

    let bucket = Bucket { funs: bucket_funs };
    let mut opgen = OpGenerator::new(ctx, krate_sst, bucket);
    while let Some(mut function_opgen) = opgen.next()? {
        loop {
            // The expand-errors driver produces either the next expansion op to
            // run or, once it's exhausted all sub-queries, the final diagnostic
            // to print. Only yields when armed by a prior failure.
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
                feeder.reporter.report(&diag);
            }
            let Some(op) = next_op else { break };

            // Emit the op's label comment via `air_ctx.comment(...)` so
            // every log gets a `;; <OpKind> <func-path>` line right
            // before the op's commands. That becomes the natural
            // first-line label of the drain block below. Also stamp the
            // same label into the Z3 response buffer so the Z3 tab
            // (which otherwise is a flat stream of sat/unsat/empty
            // replies) reads as per-op stanzas.
            let air_comment = op.to_air_comment();
            feeder.air_ctx.comment(&air_comment);
            verus_z3_annotate(&air_comment);

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
                                    let push = verdict_is_query
                                        && (is_first_check || !proved);
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
            // Drain this op's text. Mirrors `push_item`'s VIR/SST rule:
            // fold iff the op isn't local user code — context ops for
            // external crates (vstd function-axioms / specs) and
            // no-function setup ops (broadcasts, trait-impl axioms)
            // collapse into the prelude row; context ops for local
            // functions and queries (always tied to a local function)
            // stay expanded so the reader sees the actual check-sat
            // bodies and their immediate setup. `drain_block` merges
            // adjacent folded blocks so vstd runs stay as one row.
            let fold = op.function.as_ref().is_none_or(|f| f.x.name.path.krate.is_some());
            bufs.drain_block(&mut output.air_blocks, fold);
        }
    }
    Ok(())
}

fn write_verify_output(output: VerifyOutput) {
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
