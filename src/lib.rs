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
//   4. Small utilities               — `time`, `emit_section`,
//                                      `unwrap_dump_or_panic`.
//   5. Pipeline driver               — `parse_and_verify` → `run_pipeline`
//                                      orchestrates the four stages below.
//   6. Stage 1: rustc invocation     — config + `VirtualFileLoader` +
//                                      `DomWriter` diagnostic plumbing.
//   7. Stage 2: HIR dump
//   8. Stage 3: HIR → VIR            — `build_vir` + vstd deserialize cache.
//   9. Stage 4: VIR → AIR → Z3       — the bulk of the file.

#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_interface;
extern crate rustc_metadata;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

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
use rust_verify::import_export::CrateWithMetadata;
use rust_verify::spans::SpanContext;
use rust_verify::verifier::{Reporter, Verifier};
use rustc_errors::emitter::HumanEmitter;
use rustc_errors::registry::Registry;
use rustc_errors::{AutoStream, ColorChoice};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;
use rustc_session::EarlyDiagCtxt;
use rustc_session::config::{self, ErrorOutputType, Input};
use rustc_span::def_id::LOCAL_CRATE;
use rustc_span::source_map::FileLoader;
use rustc_span::{FileName, Symbol};
use vir::ast::{ArchWordBits, Datatype, Fun, Krate, VirErr};
use vir::ast_util::{fun_as_friendly_rust_name, is_visible_to};
use vir::context::{Ctx, GlobalCtx};
use vir::messages::VirMessageInterface;
use vir::prelude::PreludeConfig;
use vir::sst::KrateSst;
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

    // Streams each completed pipeline section (AST / HIR / VIR /
    // AIR_INITIAL / AIR_MIDDLE / AIR_FINAL / SMT / VERDICT) out to the
    // browser as soon as it's formatted. Same survivability reasoning as
    // `verus_diagnostic`: a later stage that traps the wasm instance
    // (rustc's `abort_if_errors` → `unreachable`) would otherwise discard
    // the whole returned String, hiding every section we'd already built.
    #[wasm_bindgen(js_name = verus_dump)]
    fn verus_dump(section: &str, body: &str);

    // Stage-level timing. `time()` emits one call per stage with the elapsed
    // ms. `public/index.html` and `tests/smoke.rs` both install a stub on
    // globalThis (the former logs to console, the latter to stderr). Kept
    // out-of-band from `verus_dump` so timings don't clutter the UI output
    // sections.
    #[wasm_bindgen(js_namespace = performance, js_name = now)]
    pub fn perf_now() -> f64;

    #[wasm_bindgen(js_name = verus_bench)]
    fn verus_bench(label: &str, ms: f64);
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
/// VERDICT) to the browser via `verus_dump`; the JS side caches the bodies
/// and toggles rendering without re-parsing. Returned String mirrors the
/// dumps for non-browser callers (smoke test).
#[wasm_bindgen]
pub fn parse_source(src: &str) -> String {
    parse_and_verify(src, /* verify */ true)
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

// Streams a completed section to the browser and appends it to `out` in the
// same `=== NAME ===\n<body>\n` shape the String-side consumers expect.
// Emitting via `verus_dump` synchronously at each stage boundary means the
// section lands in the DOM before a later stage could trap the wasm instance
// and discard everything we'd built up in `out`. `out` is still populated in
// parallel so the returned String stays usable for `tests/smoke.rs` and for
// the `unwrap_dump_or_panic` fallback.
fn emit_section(out: &mut String, name: &str, body: &str) {
    let body = body.trim_end();
    verus_dump(name, body);
    writeln!(out, "=== {} ===", name).unwrap();
    out.push_str(body);
    out.push('\n');
}

// Always return whatever the closure managed to dump. `run_compiler`
// post-processing can panic via `abort_if_errors` after our closure writes the
// dump, which would otherwise shadow a valid dump with "panicked: …".
fn unwrap_dump_or_panic(result: std::thread::Result<()>, partial: String) -> String {
    match result {
        Ok(()) => partial,
        Err(e) => {
            let msg = e
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| e.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("<opaque>");
            if partial.is_empty() {
                format!("panicked: {msg}")
            } else {
                format!("{partial}\n(post-dump panic: {msg})")
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 5. Pipeline driver
// ═══════════════════════════════════════════════════════════════════════

/// Parse `src` via rustc_interface, force HIR lowering, build VIR, then drive
/// the krate through AIR + Z3. Returns a multi-section `=== NAME ===` string
/// the UI splits on.
///
/// `verify` gates the AIR→Z3 stage. The wasm-bindgen `parse_source` always
/// passes `true`; integration tests in `tests/` can pass `false` so the
/// pipeline stops after VIR and doesn't call into the `Z3_*` shims (which
/// only `public/index.html` installs — not the wasm-bindgen-test harness).
pub fn parse_and_verify(src: &str, verify: bool) -> String {
    // `vstd` isn't in rustc's extern prelude (only `core`/`std` are), so user
    // code has to be told to link it. vstd's own prelude transitively
    // re-exports `verus_builtin` items and the `verus_builtin_macros`
    // proc-macros, so users who do `use vstd::prelude::*;` get everything.
    let src = format!("extern crate vstd;\n{src}");
    let dump: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let dump_clone = dump.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rustc_interface::interface::run_compiler(build_rustc_config(src), |compiler| {
            *dump_clone.lock().unwrap() = run_pipeline(compiler, verify);
        });
    }));
    let partial = dump.lock().unwrap().clone();
    unwrap_dump_or_panic(result, partial)
}

fn run_pipeline(compiler: &Compiler, verify: bool) -> String {
    let krate = time("rustc_parse", || rustc_interface::passes::parse(&compiler.sess));
    let mut out = String::new();
    time("dump.ast", || {
        let mut body = String::new();
        writeln!(body, "crate items: {}", krate.items.len()).unwrap();
        for item in &krate.items {
            writeln!(
                body,
                "  {:?} {}",
                item.kind.descr(),
                item.kind.ident().map(|i| i.name.to_string()).unwrap_or_default()
            )
            .unwrap();
        }
        emit_section(&mut out, "AST", &body);
    });
    // `create_and_enter_global_ctxt` itself is cheap — the expensive work
    // (name resolution, HIR lowering, type-check) happens lazily inside the
    // closure via `tcx` queries fired by `build_vir`. We time the enter call
    // anyway in case global_ctxt setup becomes a hot spot later.
    time("global_ctxt", || {
        rustc_interface::create_and_enter_global_ctxt(compiler, krate, |tcx| {
            dump_hir(&mut out, tcx);
            dump_vir_and_verify(&mut out, compiler, tcx, verify);
        })
    });
    out
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
        "-Zcrate-attr=no_std",
        "-Zcrate-attr=feature(register_tool)",
        // `verus!` expansion emits `#[...]` attributes on expressions
        // (e.g. `#[verus::internal(...)] foo`) — unstable without this.
        "-Zcrate-attr=feature(stmt_expr_attributes)",
        "-Zcrate-attr=feature(proc_macro_hygiene)",
        "-Zcrate-attr=register_tool(verus)",
        "-Zcrate-attr=register_tool(verifier)",
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
            let writer: Box<dyn io::Write + Send> = Box::new(DomWriter::new());
            let dst = AutoStream::new(writer, ColorChoice::Never);
            let emitter = HumanEmitter::new(dst, rustc_driver::default_translator())
                .sm(Some(psess.clone_source_map()));
            psess.dcx().set_emitter(Box::new(emitter));
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
            let trimmed = &self.pending[..idx];
            if !trimmed.is_empty() {
                verus_diagnostic(&String::from_utf8_lossy(trimmed));
            }
            self.pending.drain(..idx + 2);
        }
    }
}

fn find_block_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
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
            verus_diagnostic(&String::from_utf8_lossy(&self.pending));
            self.pending.clear();
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 7. Stage 2: HIR dump
// ═══════════════════════════════════════════════════════════════════════

fn dump_hir(out: &mut String, tcx: TyCtxt<'_>) {
    time("dump.hir", || {
        // Forces macro expansion + name resolution + HIR lowering.
        let _ = tcx.resolver_for_lowering();
        let mut body = String::new();
        for item_id in tcx.hir_free_items() {
            let def_id = item_id.owner_id.def_id.to_def_id();
            writeln!(
                body,
                "  {} {}",
                tcx.def_kind(def_id).descr(def_id),
                tcx.def_path_str(def_id)
            )
            .unwrap();
        }
        emit_section(out, "HIR", &body);
    });
}

// ═══════════════════════════════════════════════════════════════════════
// 8. Stage 3: HIR → VIR
// ═══════════════════════════════════════════════════════════════════════

fn dump_vir_and_verify(
    out: &mut String,
    compiler: &Compiler,
    tcx: TyCtxt<'_>,
    verify: bool,
) {
    // `build_vir` forces HIR lowering + name resolution + ty-check via Verus'
    // `build_vir_crate`, so this stage absorbs most of the rustc front-end
    // work. Split from `verify` below to separate rustc cost from Verus cost.
    let (krate, global_ctx, crate_name, spans) = match time("build_vir", || build_vir(compiler, tcx)) {
        Ok(v) => v,
        Err(errs) => {
            for e in errs {
                writeln!(out, "  vir error: {}", e.note).unwrap();
            }
            return;
        }
    };
    time("dump.vir", || {
        let mut buf: Vec<u8> = Vec::new();
        vir::printer::write_krate(&mut buf, &krate, &vir::printer::COMPACT_TONODEOPTS);
        emit_section(out, "VIR", &String::from_utf8_lossy(&buf));
    });
    if !verify {
        return;
    }
    match time("verify", || verify_simplified_krate(krate, global_ctx, crate_name, compiler, &spans)) {
        Ok(output) => write_verify_output(out, &output),
        Err(e) => writeln!(out, "  verify error: {}", e.note).unwrap(),
    }
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
    /// Raw AIR (Block/Switch/Assert tree, AIR syntax).
    air_initial: String,
    /// After `var_to_const::lower_query` (SSA-style versioning of mutable vars).
    air_middle: String,
    /// After `block_to_assert::lower_query` (whole stmt tree → one big assert).
    air_final: String,
    /// SMT-LIB2 captured from `Context::set_smt_log` — full text sent to Z3
    /// (macros expanded, plus push/pop, `(assert (not …))`, `(check-sat)`).
    smt: String,
    verdicts: Vec<Verdict>,
}

struct Verdict {
    function: String,
    kind: String,
    verdict: String,
    proved: bool,
}

impl Verdict {
    fn from_result(result: &ValidityResult, function: String, kind: String) -> Self {
        match result {
            ValidityResult::Valid(_) => Self { function, kind, verdict: "Valid".into(), proved: true },
            other => Self { function, kind, verdict: format!("{:?}", other), proved: false },
        }
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
struct AirBufs {
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
impl AirBufs {
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

    fn drain_into(&self, out: &mut VerifyOutput) {
        out.air_initial.push_str(&self.air_initial.drain_string());
        out.air_middle.push_str(&self.air_middle.drain_string());
        out.air_final.push_str(&self.air_final.drain_string());
        out.smt.push_str(&self.smt.drain_string());
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
) -> Result<VerifyOutput, VirErr> {
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
    };
    let mut output = VerifyOutput::default();
    // After `build_vir_crate` merges vstd into the local krate, `krate.modules`
    // is ~155 entries; of those only the user's modules need verification.
    // Verus itself filters via `current_crate_modules` (captured before the
    // merge in `rust_verify/src/verifier.rs:2861`). `PathX.krate` is None for
    // local modules and Some(crate_name) for externs — the same distinction,
    // derivable post-merge so we don't have to rework `build_vir`.
    for module in krate.modules.iter().filter(|m| m.x.path.krate.is_none()) {
        global_ctx = time("verify.module", || {
            verify_module(&mctx, module.x.path.clone(), global_ctx, &mut output)
        })?;
    }
    Ok(output)
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

    let krate_sst = time("verify.ast_to_sst", || vir::ast_to_sst_crate::ast_to_sst_krate(
        &mut ctx,
        mctx.reporter,
        &bucket_funs,
        &pruned,
    ))?;
    let krate_sst = time("verify.poly", || vir::poly::poly_krate_for_module(&mut ctx, &krate_sst));

    let visible_dts: Vec<Datatype> = krate_sst
        .datatypes
        .iter()
        .filter(|d| is_visible_to(&d.x.visibility, &module_path))
        .cloned()
        .collect();

    // `Context::new` calls `SmtProcess::launch` → `Z3_mk_config`+`Z3_mk_context`,
    // which on wasm hops into the Emscripten Z3 runtime and spins up a fresh
    // solver context. That's not free — each context is its own Z3 state.
    let mut air_ctx = time("verify.air_ctx_new", || {
        let mut c = Context::new(mctx.msg.clone(), mctx.solver);
        c.set_z3_param("air_recommended_options", "true");
        c
    });
    let bufs = AirBufs::attach(&mut air_ctx);

    let mut feeder = Feeder { air_ctx: &mut air_ctx, msg: mctx.msg, reporter: mctx.reporter };
    time("verify.feed_decls", || {
        feed_module_decls(&mut feeder, &mut ctx, &krate_sst, &visible_dts, mctx)
    })?;
    time("verify.queries", || {
        run_queries(&mut feeder, &mut ctx, &krate_sst, bucket_funs, &mut output.verdicts)
    })?;

    bufs.drain_into(output);
    // `ctx.free()` drops the AirBufs-attached Z3 context → `Z3_del_context`.
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
fn run_queries(
    feeder: &mut Feeder,
    ctx: &mut Ctx,
    krate_sst: &KrateSst,
    bucket_funs: HashSet<Fun>,
    verdicts: &mut Vec<Verdict>,
) -> Result<(), VirErr> {
    let bucket = Bucket { funs: bucket_funs };
    let mut opgen = OpGenerator::new(ctx, krate_sst, bucket);
    while let Some(mut function_opgen) = opgen.next()? {
        while let Some(op) = function_opgen.next() {
            let func_name = op
                .function
                .as_ref()
                .map(|f| fun_as_friendly_rust_name(&f.x.name))
                .unwrap_or_default();
            // Tracked across the (possibly multiple) `CheckValid` commands in
            // this one op so that on a failed proof/spec body we can enqueue
            // the recommends-retry ops. Matches Verus' verifier.rs:1829-1879.
            let mut any_invalid = false;
            let mut retry_kind: Option<QueryOp> = None;
            match &op.kind {
                OpKind::Context(_, commands) => feeder.feed_all(commands),
                OpKind::Query { commands_with_context_list, query_op, .. } => {
                    let kind = format!("{:?}", query_op);
                    retry_kind = Some(*query_op);
                    for cmds in commands_with_context_list.iter() {
                        for cmd in cmds.commands.iter() {
                            let result = feeder.feed(cmd);
                            if matches!(&**cmd, CommandX::CheckValid(_)) {
                                // Route the Verus-supplied `Message` (with source
                                // spans + labels) through the rustc Reporter so
                                // the emitter renders it as a normal spanned
                                // error. Without this the caller only sees our
                                // coarse "Valid / N queries failed" summary.
                                if let ValidityResult::Invalid(_, Some(err), _) = &result {
                                    feeder.reporter.report_as(err, MessageLevel::Error);
                                }
                                let proved = matches!(result, ValidityResult::Valid(_));
                                if !proved {
                                    any_invalid = true;
                                }
                                verdicts.push(Verdict::from_result(
                                    &result,
                                    func_name.clone(),
                                    kind.clone(),
                                ));
                                feeder.air_ctx.finish_query();
                            }
                        }
                    }
                }
            }
            // Auto-recommends: on a failed Normal body or spec-termination
            // query, enqueue recommends-retry ops. Mirrors the trigger in
            // Verus' `verifier.rs:1829-1879`. `check_recommends` attribute on
            // the function is another trigger in Verus, but reading it
            // requires digging into the function's attrs — auto-on-error
            // covers the common case. Only fires on failure, so no cost for
            // passing proofs.
            if any_invalid {
                if matches!(
                    retry_kind,
                    Some(QueryOp::Body(Style::Normal)) | Some(QueryOp::SpecTermination)
                ) {
                    function_opgen.retry_with_recommends(&op, /* from_error */ true)?;
                }
            }
        }
    }
    Ok(())
}

fn write_verify_output(out: &mut String, output: &VerifyOutput) {
    for (name, body) in [
        ("AIR_INITIAL", &output.air_initial),
        ("AIR_MIDDLE", &output.air_middle),
        ("AIR_FINAL", &output.air_final),
        ("SMT", &output.smt),
    ] {
        if body.is_empty() {
            continue;
        }
        emit_section(out, name, body);
    }
    let mut verdict = String::new();
    if output.verdicts.is_empty() {
        writeln!(verdict, "no queries").unwrap();
    } else if output.verdicts.iter().all(|v| v.proved) {
        writeln!(verdict, "Valid").unwrap();
    } else {
        let n_failed = output.verdicts.iter().filter(|v| !v.proved).count();
        writeln!(verdict, "{}/{} queries failed", n_failed, output.verdicts.len()).unwrap();
    }
    for v in &output.verdicts {
        let result = if v.proved { "proved" } else { v.verdict.as_str() };
        writeln!(verdict, "{}: {} → {}", v.function, v.kind, result).unwrap();
    }
    emit_section(out, "VERDICT", &verdict);
}
