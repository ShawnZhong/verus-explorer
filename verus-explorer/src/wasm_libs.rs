// In-wasm filesystem for rustc's crate locator.
//
// Supplies `libcore.rmeta`, `libvstd.rmeta`, and friends so name
// resolution can resolve `extern crate core/alloc/vstd` without a real
// filesystem. Also carries the bincode-serialized `vstd.vir` consumed
// by `build_vir`.
//
// Bytes are not bundled into the wasm via `include_bytes!`. Instead the
// browser loader fetches each rmeta + `vstd.vir` from `./libs/` (staged
// by `make libs` and copied into `dist/` by the Makefile) and streams
// them in one-by-one through `wasm_libs_add_file`, then calls
// `wasm_libs_finalize` to register rustc's filesearch callbacks.
// Keeping ~60 MB of rmetas + .vir out of the wasm shrinks the binary
// (~83 MB → ~23 MB), lets HTTP gzip compress each artifact, and gives
// the browser independent cache entries per crate.
//
// The sync contract rustc expects:
//   * `list(dir)` — directory listing for `SearchPath::new` in
//     `rustc_session::search_paths`.
//   * `read(path)` — rmeta bytes for `get_rmeta_metadata_section` in
//     `rustc_metadata::locator`.
//
// `--sysroot=/virtual` is passed by `build_rustc_config`; rustc then
// derives `/virtual/lib/rustlib/wasm32-unknown-unknown/lib` as the
// target-lib path, which is the single directory we answer listings for.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

use wasm_bindgen::prelude::*;

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
