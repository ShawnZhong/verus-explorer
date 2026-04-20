// Stages wasm32 rmetas + vstd.vir as a virtual sysroot under
// `target/wasm-libs/lib/rustlib/wasm32-unknown-unknown/lib/` (the exact
// path rustc's wasm32 crate locator expects when passed `--sysroot=<root>`).
// Each file also gets a `.gz` sibling (gzip -9); the Makefile ships only
// the `.gz` copies to `dist/wasm-libs/`, and `public/index.html` fetches
// `${name}.gz` and decompresses via the native `DecompressionStream('gzip')`
// before handing the bytes back to wasm via `wasm_libs_add_file` /
// `wasm_libs_finalize`. The originals stay on disk so `tests/smoke.rs` can
// read them directly from `WASM_LIBS_DIR`. rustc-in-wasm consumes the
// in-memory bytes through `rustc_session::filesearch::sysroot::Callbacks`
// (installed in `src/wasm_libs.rs`). Keeping them out of the wasm shrinks
// the binary from ~83 MB to ~23 MB, and gzip cuts the remaining ~60 MB of
// rmeta/.vir transfer down to ~13 MB. HTTP/2 multiplexes the per-file
// fetches so the one-roundtrip-per-file cost is negligible compared to
// what we save by letting the browser cache each artifact independently.
//
// The bundled crates:
//   1. core / compiler_builtins / alloc — self-built from `rust-src`.
//      Saves ~15 MB vs rustup's prebuilts (which carry MIR for cross-
//      crate inlining we never use, since we never codegen).
//   2. verus_builtin / verus_builtin_macros / verus_state_machines_macros —
//      wasm32 stub-only rmetas (`--cfg=stub_only`). The `_macros` crates
//      carry only `pub macro NAME` decl_macro stubs in this mode; their full
//      `MACROS` slices are linked into the host (rust_verify) and explorer
//      builds via cargo and registered with the patched
//      `rustc_metadata::proc_macro_registry`.
//   3. vstd.rmeta + vstd.vir — built by host rust_verify against the
//      staged sysroot above. The .vir blob (bincode-serialized VIR krate)
//      is retrieved by `build_vir` via `wasm_libs_vstd_vir()`
//      and handed to `Verifier::build_vir_crate` as `other_vir_crates`.
//
// Everything is built by `scripts/build-wasm-libs.sh` with
// `--sysroot=<target/wasm-libs>` so the SVH chain lines up across all rmetas.
//
// `tests/smoke.rs` (run under `wasm-pack test --node`) locates this directory
// through the `WASM_LIBS_DIR` env var emitted below and reads the files via
// Node's `fs` module at runtime — no `include_bytes!` embedding needed.
// `public/index.html` hardcodes the file names since they're defined by
// structure (core/alloc/compiler_builtins + verus_builtin + two macro stubs
// + vstd.rmeta/.vir), not by data that varies.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const VERUS_BUILTIN_SRC: &str = "third_party/verus/source/builtin/src/lib.rs";
const VSTD_SRC_DIR: &str = "third_party/verus/source/vstd";
const VSTD_RMETA: &str = "libvstd.rmeta";
const VSTD_VIR: &str = "vstd.vir";

const BUILD_SCRIPT: &str = "scripts/build-wasm-libs.sh";

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed={BUILD_SCRIPT}");
    println!("cargo::rerun-if-changed={VERUS_BUILTIN_SRC}");
    println!("cargo::rerun-if-changed={VSTD_SRC_DIR}");

    // Stable, profile-independent path under target/ — matches the pattern
    // used by target/host-rust/ and target/host-verus/. Lets the Makefile
    // reference it directly, avoids duplicate builds across debug/release,
    // and keeps the script runnable by hand against the same location.
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let wasm_libs = manifest_dir.join("target/wasm-libs");
    // rustc's wasm32 crate locator looks for `<sysroot>/lib/rustlib/
    // wasm32-unknown-unknown/lib/lib<name>.rmeta`, so the script builds
    // every rmeta into exactly that path. Subsequent `--sysroot=<wasm_libs>`
    // invocations resolve extern crates against these.
    let lib_dir = wasm_libs.join("lib/rustlib/wasm32-unknown-unknown/lib");

    // Browser loader + integration tests fetch files relative to this dir.
    // Emitted on both host and wasm32 builds so `env!` compiles either way.
    println!("cargo::rustc-env=WASM_LIBS_DIR={}", lib_dir.display());

    let target = env::var("TARGET").unwrap_or_default();
    if target != "wasm32-unknown-unknown" {
        return;
    }

    // Wipe any stale artifacts from a previous run — otherwise a renamed or
    // removed crate would linger as a duplicate (or SVH-mismatched) rmeta.
    let _ = fs::remove_dir_all(&wasm_libs);
    fs::create_dir_all(&lib_dir).expect("mkdir lib dir");

    // Build every rmeta + vstd.vir in one shot.
    let status = Command::new(BUILD_SCRIPT)
        .arg(&wasm_libs)
        .status()
        .unwrap_or_else(|e| panic!("spawn {BUILD_SCRIPT}: {e}"));
    assert!(status.success(), "{BUILD_SCRIPT} failed");
    assert!(
        lib_dir.join(VSTD_RMETA).exists(),
        "expected {VSTD_RMETA} in lib dir after rust_verify --compile"
    );
    assert!(
        lib_dir.join(VSTD_VIR).exists(),
        "expected {VSTD_VIR} in lib dir after rust_verify --export"
    );

    // Gzip each artifact in-place so the Makefile can ship the `.gz` copies
    // to `dist/wasm-libs/`. `-k` keeps the original (needed by smoke.rs +
    // in case of a manual script re-run); `-f` overwrites any stale .gz.
    let names = fs::read_dir(&lib_dir)
        .expect("read lib dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .filter(|n| {
            let s = n.to_string_lossy();
            s.ends_with(".rmeta") || s == VSTD_VIR
        });
    for name in names {
        let src = lib_dir.join(&name);
        let status = Command::new("gzip")
            .args(["-kf9"])
            .arg(&src)
            .status()
            .unwrap_or_else(|e| panic!("spawn gzip: {e}"));
        assert!(status.success(), "gzip {} failed", src.display());
    }
}
