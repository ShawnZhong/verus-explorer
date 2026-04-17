// Embedded sysroot for the wasm build: supplies `libcore.rmeta` (and any
// other bundled rmetas from build.rs) to rustc's crate locator so name
// resolution can resolve `core`/`alloc`/`std` without a real filesystem.
//
// The bytes are embedded via `include_bytes!` in the generated
// `sysroot_bundle.rs` (see `build.rs`). At startup we register two callbacks
// with `rustc_session::filesearch::sysroot`, one for directory listings and
// one for file reads. Both the search-path scanner in
// `rustc_session::search_paths::SearchPath::new` and the rmeta loader in
// `rustc_metadata::locator::get_rmeta_metadata_section` consult these before
// falling through to `fs::read_dir` / `File::open`.
//
// `--sysroot=/virtual` is passed by `frontend::parse_source`; rustc then
// derives `/virtual/lib/rustlib/wasm32-unknown-unknown/lib` as the
// target-lib path, which is the single directory we answer listings for.

use std::path::{Path, PathBuf};

mod bundle {
    include!(concat!(env!("OUT_DIR"), "/sysroot_bundle.rs"));
}

const SYSROOT_LIB_DIR: &str = "/virtual/lib/rustlib/wasm32-unknown-unknown/lib";

fn list(dir: &Path) -> Option<Vec<(String, PathBuf)>> {
    if dir != Path::new(SYSROOT_LIB_DIR) {
        return None;
    }
    Some(
        bundle::FILES
            .iter()
            .map(|(name, _)| ((*name).to_string(), PathBuf::from(format!("{SYSROOT_LIB_DIR}/{name}"))))
            .collect(),
    )
}

fn read(path: &Path) -> Option<&'static [u8]> {
    let name = path.file_name()?.to_str()?;
    bundle::FILES.iter().find(|(n, _)| *n == name).map(|(_, data)| *data)
}

pub fn install() {
    rustc_session::filesearch::sysroot::install(
        rustc_session::filesearch::sysroot::Callbacks { list, read },
    );
}
