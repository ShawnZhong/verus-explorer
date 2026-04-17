// Smoke tests proving vendored rustc_* crates run on wasm32.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustc_errors::emitter::HumanEmitter;
use rustc_errors::registry::Registry;
use rustc_errors::{AutoStream, ColorChoice};
use rustc_session::EarlyDiagCtxt;
use rustc_session::config::{self, ErrorOutputType, Input};
use rustc_span::FileName;
use rustc_span::source_map::FileLoader;

use crate::console_error;

// wasm32 has panic=abort, so `catch_unwind` can't recover from rustc's
// `abort_if_errors`. Route rustc's emitter to `console.error` so diagnostics
// are visible *before* the abort trap fires.
struct ConsoleWriter {
    buf: Vec<u8>,
}

impl ConsoleWriter {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }
}

impl io::Write for ConsoleWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            console_error(&String::from_utf8_lossy(&self.buf));
            self.buf.clear();
        }
        Ok(())
    }
}

impl Drop for ConsoleWriter {
    fn drop(&mut self) {
        let _ = io::Write::flush(self);
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

/// Parse `src` via rustc_interface, force HIR lowering, and dump AST + HIR
/// top-level items. `--sysroot=/virtual` pairs with the virtual sysroot
/// callbacks installed in `lib::init` — rustc's crate locator finds
/// `libcore.rmeta` (and friends), plus our prebuilt `libverus_builtin.rmeta`,
/// in the embedded bundle instead of on disk. We inject `#![no_std]` so
/// only `core` is needed from std, and prepend `extern crate verus_builtin;`
/// so the builtin crate is linked and its `#[rustc_diagnostic_item]`
/// registrations fire — that's what Verus keys its builtin lookups off of.

pub fn parse_source(src: &str) -> String {
    let dump: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let argv: Vec<String> = [
        "--edition=2021",
        "--crate-type=lib",
        "--crate-name=v",
        "--sysroot=/virtual",
        "--cfg=verus_keep_ghost",
        "-Zcrate-attr=no_std",
        "-Zcrate-attr=feature(register_tool)",
        "-Zcrate-attr=register_tool(verus)",
        "-Zcrate-attr=register_tool(verifier)",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    let src = format!("extern crate verus_builtin;\n{src}");

    let dump_clone = dump.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut early_dcx = EarlyDiagCtxt::new(ErrorOutputType::default());
        let matches = rustc_driver::handle_options(&early_dcx, &argv).expect("handle_options");
        let opts = config::build_session_options(&mut early_dcx, &matches);

        let config = rustc_interface::interface::Config {
            opts,
            crate_cfg: vec![],
            crate_check_cfg: vec![],
            input: Input::Str {
                name: FileName::Custom("input.rs".into()),
                input: src.to_string(),
            },
            output_file: None,
            output_dir: None,
            ice_file: None,
            file_loader: Some(Box::new(VirtualFileLoader)),
            locale_resources: rustc_driver::DEFAULT_LOCALE_RESOURCES.to_vec(),
            lint_caps: Default::default(),
            psess_created: Some(Box::new(|psess| {
                let writer: Box<dyn io::Write + Send> = Box::new(ConsoleWriter::new());
                let dst = AutoStream::new(writer, ColorChoice::Never);
                let emitter = HumanEmitter::new(dst, rustc_driver::default_translator())
                    .sm(Some(psess.clone_source_map()));
                psess.dcx().set_emitter(Box::new(emitter));
            })),
            hash_untracked_state: None,
            register_lints: None,
            override_queries: None,
            extra_symbols: vec![],
            make_codegen_backend: None,
            registry: Registry::new(rustc_errors::codes::DIAGNOSTICS),
            using_internal_features: &rustc_driver::USING_INTERNAL_FEATURES,
        };

        rustc_interface::interface::run_compiler(config, |compiler| {
            let krate = rustc_interface::passes::parse(&compiler.sess);
            let mut out = String::new();
            writeln!(out, "=== AST ===").unwrap();
            writeln!(out, "crate items: {}", krate.items.len()).unwrap();
            for item in &krate.items {
                writeln!(
                    out,
                    "  {:?} {}",
                    item.kind.descr(),
                    item.kind.ident().map(|i| i.name.to_string()).unwrap_or_default()
                )
                .unwrap();
            }
            writeln!(out).unwrap();
            writeln!(out, "=== HIR ===").unwrap();
            rustc_interface::create_and_enter_global_ctxt(compiler, krate, |tcx| {
                // Forces macro expansion + name resolution + HIR lowering.
                let _ = tcx.resolver_for_lowering();
                for item_id in tcx.hir_free_items() {
                    let def_id = item_id.owner_id.def_id.to_def_id();
                    writeln!(
                        out,
                        "  {} {}",
                        tcx.def_kind(def_id).descr(def_id),
                        tcx.def_path_str(def_id)
                    )
                    .unwrap();
                }
                writeln!(out).unwrap();
                writeln!(out, "=== VIR ===").unwrap();
                match crate::vir_bridge::build_vir(compiler, tcx) {
                    Ok(krate) => {
                        let mut buf: Vec<u8> = Vec::new();
                        vir::printer::write_krate(
                            &mut buf,
                            &krate,
                            &vir::printer::COMPACT_TONODEOPTS,
                        );
                        out.push_str(&String::from_utf8_lossy(&buf));
                    }
                    Err(errs) => {
                        for e in errs {
                            writeln!(out, "  vir error: {}", e.note).unwrap();
                        }
                    }
                }
            });
            *dump_clone.lock().unwrap() = out;
        });
    }));
    // Always return whatever the closure managed to dump. `run_compiler`
    // post-processing can panic via `abort_if_errors` after our closure writes
    // `dump`, which would otherwise shadow a valid HIR dump with "panicked: …".
    let partial = dump.lock().unwrap().clone();
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
