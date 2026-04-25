// JS externs + in-wasm filesystem — everything that crosses the
// wasm↔JS boundary.
//
// Externs half: the host-supplied API every pipeline stage calls
// into. The browser (`public/app.js`) installs these on
// `globalThis`; the smoke test in `tests/smoke.rs` installs its
// own stubs. Each `fn` in the `extern "C"` block has a matching
// JS function on the host side, wired up by the `js_name`
// attribute where the Rust and JS names diverge.
//
// Libs half: the in-wasm filesystem for rustc's crate locator.
// Supplies `libcore.rmeta`, `libvstd.rmeta`, and friends so name
// resolution can resolve `extern crate core/alloc/vstd` without
// a real filesystem. Also carries the bincode-serialized
// `vstd.vir` consumed by `build_vir`.
//
// Bytes are not bundled into the wasm via `include_bytes!`. Instead
// the browser loader fetches each rmeta + `vstd.vir` from
// `./libs/` (staged by `make libs` and copied into `dist/` by the
// Makefile) and streams them in one-by-one through
// `wasm_libs_add_file`, then calls `wasm_libs_finalize` to
// register rustc's filesearch callbacks. Keeping ~60 MB of rmetas
// + .vir out of the wasm shrinks the binary (~83 MB → ~23 MB),
// lets HTTP gzip compress each artifact, and gives the browser
// independent cache entries per crate.
//
// The sync contract rustc expects:
//   * `list(dir)` — directory listing for `SearchPath::new` in
//     `rustc_session::search_paths`.
//   * `read(path)` — rmeta bytes for `get_rmeta_metadata_section`
//     in `rustc_metadata::locator`.
//
// `--sysroot=/virtual` is passed by `build_rustc_config`; rustc
// then derives `/virtual/lib/rustlib/wasm32-unknown-unknown/lib`
// as the target-lib path, which is the single directory we answer
// listings for.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

use wasm_bindgen::prelude::*;

// -------- JS externs --------

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    pub(crate) fn console_error(msg: &str);

    // Imported from `public/app.js`. Called synchronously from
    // `DomWriter` so each rustc diagnostic lands in the output panel before
    // rustc's `abort_if_errors` turns into a wasm `unreachable` trap.
    #[wasm_bindgen(js_name = verus_diagnostic)]
    pub(crate) fn verus_diagnostic(msg: &str);

    // Same survivability reasoning as `verus_diagnostic`, but carries the
    // structured JsonEmitter output (one diagnostic per line). The JS side
    // parses it into `byte_start`/`byte_end` + `line`/`col` spans and feeds
    // CM6 `setDiagnostics` — gives us precise squiggle ranges and
    // secondary-label spans without scraping the human-readable text.
    #[wasm_bindgen(js_name = verus_diagnostic_json)]
    pub(crate) fn verus_diagnostic_json(msg: &str);

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
    pub(crate) fn verus_dump(section: &str, contents: Vec<String>, folds: Vec<u8>);

    // Stage-level timing. `time()` emits one call per stage with the elapsed
    // ms. `public/app.js` and `tests/smoke.rs` both install a stub on
    // globalThis (the former logs to console, the latter to stderr). Kept
    // out-of-band from `verus_dump` so timings don't clutter the UI output
    // sections.
    #[wasm_bindgen(js_namespace = performance, js_name = now)]
    pub fn perf_now() -> f64;

    #[wasm_bindgen(js_name = verus_bench)]
    pub(crate) fn verus_bench(label: &str, ms: f64);
}

// -------- In-wasm libs filesystem --------

pub(crate) const VIRTUAL_LIB_DIR: &str = "/virtual/lib/rustlib/wasm32-unknown-unknown/lib";
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

// Flips `build_rustc_config`'s `-Zcrate-attr=no_std` injection and tells
// the JS loader which vstd variant to register as `libvstd.rmeta`. Set
// before `wasm_libs_finalize` from `public/app.js` based on the
// `?std=1` URL param (opt-in). Defaults to `false` to match the page's
// default nostd mode — tests and any out-of-band callers that forget to
// call `set_std_mode` then get the smaller/faster bundle rather than a
// spec mismatch. `AtomicBool` rather than `OnceLock<bool>` so JS can
// flip the flag between `verify` calls if we ever wire a no-reload
// toggle.
static STD_MODE: AtomicBool = AtomicBool::new(false);

#[wasm_bindgen]
pub fn set_std_mode(enabled: bool) {
    STD_MODE.store(enabled, Ordering::Relaxed);
}

pub(crate) fn std_mode() -> bool {
    STD_MODE.load(Ordering::Relaxed)
}

/// Register one libs file (rmeta or `vstd.vir`) fetched by the JS
/// loader from `./libs/<name>`. Call once per manifest entry, then call
/// `wasm_libs_finalize` before the first `verify` invocation.
#[wasm_bindgen]
pub fn wasm_libs_add_file(name: String, bytes: Vec<u8>) {
    // `name` and `bytes` are leaked into `'static` storage, which is fine
    // because this runs at startup on a single-use wasm instance that's
    // discarded after one `verify` call.
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
pub(crate) fn wasm_libs_vstd_vir() -> &'static [u8] {
    wasm_libs().files.iter().find(|(n, _)| *n == VSTD_VIR).map(|(_, d)| *d).unwrap_or_default()
}
