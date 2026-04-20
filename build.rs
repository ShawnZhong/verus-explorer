// Stages wasm32 rmetas + vstd.vir as a virtual sysroot under
// `target/wasm-libs/lib/rustlib/wasm32-unknown-unknown/lib/` (the exact
// path rustc's wasm32 crate locator expects when passed `--sysroot=<root>`).
// Also writes a `manifest.json` alongside them listing the names the
// browser loader should fetch. The Makefile copies the directory contents
// into `dist/wasm-libs/`; the browser fetches `manifest.json` and then each
// rmeta/.vir in parallel, then hands the bytes back to wasm via
// `wasm_libs_add_file` / `wasm_libs_finalize`. rustc-in-wasm consumes them
// through `rustc_session::filesearch::sysroot::Callbacks` (installed in
// `src/wasm_libs.rs`). Keeping them out of the wasm shrinks the binary from
// ~83 MB to ~23 MB, and HTTP/2 multiplexes the per-file fetches so the
// one-roundtrip-per-file cost is negligible compared to what we save by
// letting the browser gzip + cache each artifact independently.
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
//      is retrieved by `pipeline::build_vir` via `wasm_libs::vstd_vir()`
//      and handed to `Verifier::build_vir_crate` as `other_vir_crates`.
//
// Everything is built by `scripts/build-wasm-libs.sh` with
// `--sysroot=<target/wasm-libs>` so the SVH chain lines up across all rmetas.
//
// `tests/smoke.rs` (run under `wasm-pack test --node`) locates this directory
// through the `WASM_LIBS_DIR` env var emitted below and reads the files via
// Node's `fs` module at runtime — no `include_bytes!` embedding needed.
//
// On non-wasm targets we only seed an empty `manifest.json` (if missing) so
// host builds don't clobber wasm output that may already live here.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const VERUS_BUILTIN_SRC: &str = "third_party/verus/source/builtin/src/lib.rs";
const VSTD_SRC_DIR: &str = "third_party/verus/source/vstd";
const VSTD_RMETA: &str = "libvstd.rmeta";
const VSTD_VIR: &str = "vstd.vir";

const BUILD_SCRIPT: &str = "scripts/build-wasm-libs.sh";

fn write_manifest(lib_dir: &Path, names: &[String]) {
    let body = names.iter().map(|n| format!("  {n:?}")).collect::<Vec<_>>().join(",\n");
    let json = format!("[\n{body}\n]\n");
    fs::write(lib_dir.join("manifest.json"), json).expect("write manifest.json");
}

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
        // Host build — don't touch existing content (a prior wasm build may
        // have populated it). Only seed an empty manifest if nothing's there.
        fs::create_dir_all(&lib_dir).expect("mkdir lib dir");
        if !lib_dir.join("manifest.json").exists() {
            write_manifest(&lib_dir, &[]);
        }
        return;
    }

    // Wipe any stale rmetas from a previous run — the manifest is built from
    // whatever's in the dir, so a stale entry would surface as a duplicate
    // (or SVH-mismatched) crate in the bundle.
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

    // Collect every .rmeta in lib_dir, plus vstd.vir, sorted for stable order.
    let mut names: Vec<String> = fs::read_dir(&lib_dir)
        .expect("read lib dir")
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".rmeta"))
        .collect();
    names.push(VSTD_VIR.to_string());
    names.sort();

    write_manifest(&lib_dir, &names);
}
