// Smoke tests proving vendored rustc_* crates run on wasm32.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustc_errors::registry::Registry;
use rustc_lexer::{FrontmatterAllowed, tokenize};
use rustc_session::EarlyDiagCtxt;
use rustc_session::config::{self, ErrorOutputType, Input};
use rustc_span::source_map::FileLoader;
use rustc_span::{FileName, create_session_globals_then, edition::Edition};

/// Tokenize `src` with rustc_lexer, dumping span + kind per token.
pub fn lex_source(src: &str) -> String {
    create_session_globals_then(Edition::Edition2024, &[], None, || {
        let mut out = String::new();
        let mut offset = 0usize;
        for tok in tokenize(src, FrontmatterAllowed::No) {
            let end = offset + tok.len as usize;
            writeln!(out, "{offset:>4}..{end:<4} {:?}", tok.kind).unwrap();
            offset = end;
        }
        out
    })
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

/// Parse `src` via rustc_interface and dump top-level AST item kinds/names.
pub fn parse_source(src: &str) -> String {
    let dump: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    // `--sysroot` is mandatory on wasm32: default_sysroot() hits
    // current_dll_path() which we stub to a dummy (see filesearch.rs patch).
    let argv: Vec<String> = [
        "--edition=2021",
        "--crate-type=lib",
        "--crate-name=v",
        "--sysroot=/virtual",
    ]
    .into_iter()
    .map(String::from)
    .collect();

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
            psess_created: None,
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
            *dump_clone.lock().unwrap() = out;
        });
    }));
    match result {
        Ok(()) => dump.lock().unwrap().clone(),
        Err(e) => format!(
            "panicked: {}",
            e.downcast_ref::<&str>()
                .copied()
                .or_else(|| e.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("<opaque>")
        ),
    }
}
