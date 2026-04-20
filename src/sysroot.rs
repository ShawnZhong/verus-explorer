// Virtual sysroot for the wasm build: supplies `libcore.rmeta`, `libvstd.rmeta`,
// and friends to rustc's crate locator so name resolution can resolve
// `extern crate core/alloc/vstd` without a real filesystem. Also carries the
// bincode-serialized `vstd.vir` consumed by `pipeline::build_vir`.
//
// Bytes are no longer bundled into the wasm via `include_bytes!`. Instead the
// browser loader fetches each rmeta + `vstd.vir` from `./sysroot/` (laid out
// by `build.rs`, copied into `dist/` by the Makefile) and streams them in
// one-by-one through `add_file`, then calls `finalize` to register rustc's
// filesearch callbacks. Keeping ~60 MB of rmetas + .vir out of the wasm
// shrinks the binary (~83 MB → ~23 MB), lets HTTP gzip compress each
// artifact, and gives the browser independent cache entries per crate.
//
// The same sync contract rustc expects still holds:
//   * `list(dir)` — directory listing for `SearchPath::new` in
//     `rustc_session::search_paths`.
//   * `read(path)` — rmeta bytes for `get_rmeta_metadata_section` in
//     `rustc_metadata::locator`.
//
// `--sysroot=/virtual` is passed by `pipeline::parse_source`; rustc then
// derives `/virtual/lib/rustlib/wasm32-unknown-unknown/lib` as the
// target-lib path, which is the single directory we answer listings for.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const SYSROOT_LIB_DIR: &str = "/virtual/lib/rustlib/wasm32-unknown-unknown/lib";
const VSTD_VIR: &str = "vstd.vir";

struct Bundle {
    // `name` is `&'static str` because `add_file` leaks the incoming `String`
    // via `Box::leak`; `bytes` is leaked the same way. Both last for the
    // process lifetime, matching the `&'static [u8]` return type of the
    // filesearch `read` callback.
    files: Vec<(&'static str, &'static [u8])>,
}

// Files accumulate here as JS streams them in; `finalize` drains this into
// `BUNDLE`. Wrapped in a `Mutex` only to satisfy static-init — wasm is
// single-threaded, so contention is impossible.
static PENDING: Mutex<Vec<(&'static str, &'static [u8])>> = Mutex::new(Vec::new());
static BUNDLE: OnceLock<Bundle> = OnceLock::new();

fn bundle() -> &'static Bundle {
    BUNDLE.get().expect("sysroot::finalize must be called before rustc runs")
}

fn list(dir: &Path) -> Option<Vec<(String, PathBuf)>> {
    if dir != Path::new(SYSROOT_LIB_DIR) {
        return None;
    }
    Some(
        bundle()
            .files
            .iter()
            .map(|(name, _)| {
                ((*name).to_string(), PathBuf::from(format!("{SYSROOT_LIB_DIR}/{name}")))
            })
            .collect(),
    )
}

fn read(path: &Path) -> Option<&'static [u8]> {
    let name = path.file_name()?.to_str()?;
    bundle().files.iter().find(|(n, _)| *n == name).map(|(_, data)| *data)
}

/// Register one rmeta (or `vstd.vir`) coming from the JS loader. `name` must
/// match what rustc's crate locator expects (e.g. `libcore-<hash>.rmeta`).
/// The bytes are leaked into `'static` storage, which is fine because this
/// runs at startup on a single-use wasm instance that's discarded after one
/// `parse_source` call.
pub fn add_file(name: String, bytes: Vec<u8>) {
    let name: &'static str = Box::leak(name.into_boxed_str());
    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    PENDING.lock().unwrap().push((name, bytes));
}

/// Freeze the accumulated files and wire up rustc's filesearch callbacks.
/// Must be called exactly once, after every `add_file` for this instance,
/// before any `parse_source` invocation.
pub fn finalize() {
    let files = std::mem::take(&mut *PENDING.lock().unwrap());
    BUNDLE.set(Bundle { files }).ok().expect("sysroot::finalize called twice");

    rustc_session::filesearch::sysroot::install(rustc_session::filesearch::sysroot::Callbacks {
        list,
        read,
    });
}

/// Bytes of the bundled `vstd.vir` (bincode-serialized VIR krate), consumed
/// by `pipeline::build_vir`. Returns `&[]` if no such file is in the bundle,
/// which surfaces as a clean bincode deserialization error upstream.
pub fn vstd_vir() -> &'static [u8] {
    bundle().files.iter().find(|(n, _)| *n == VSTD_VIR).map(|(_, d)| *d).unwrap_or_default()
}
