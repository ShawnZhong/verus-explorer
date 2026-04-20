// Stages wasm32 rmetas + vstd.vir and copies them (plus a manifest.json
// listing their names) into `$OUT_DIR/sysroot/`. The Makefile copies the
// whole directory into `dist/sysroot/`; the browser loader fetches the
// manifest and then each rmeta/.vir in parallel, then hands the bytes back
// to wasm via `sysroot_add_file` / `sysroot_finalize`. rustc-in-wasm consumes
// them through `rustc_session::filesearch::sysroot::Callbacks` (installed in
// `src/sysroot.rs`). Keeping them out of the wasm shrinks the binary from
// ~83 MB to ~23 MB, and HTTP/2 multiplexes the per-file fetches so the
// one-roundtrip-per-file cost is negligible compared to what we save by
// letting the browser gzip + cache each artifact independently.
//
// The `sysroot/` layout includes:
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
//      is retrieved by `pipeline::build_vir` via `sysroot::vstd_vir()`
//      and handed to `Verifier::build_vir_crate` as `other_vir_crates`.
//
// Everything is built by `scripts/build-wasm-libs.sh` with `--sysroot=<staged>`
// so the SVH chain lines up across all rmetas.
//
// `tests/smoke.rs` (run under `wasm-pack test --node`) locates this directory
// through the `SYSROOT_DIR` env var emitted below and reads the files via
// Node's `fs` module at runtime — no `include_bytes!` embedding needed.
//
// On non-wasm targets we create an empty `sysroot/` — the host has a real
// filesystem and doesn't need this.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const VERUS_BUILTIN_SRC: &str = "third_party/verus/source/builtin/src/lib.rs";
const VSTD_SRC_DIR: &str = "third_party/verus/source/vstd";
const VSTD_RMETA: &str = "libvstd.rmeta";
const VSTD_VIR: &str = "vstd.vir";

const BUILD_SCRIPT: &str = "scripts/build-wasm-libs.sh";

fn write_manifest(sysroot_out: &Path, names: &[String]) {
    let body = names.iter().map(|n| format!("  {n:?}")).collect::<Vec<_>>().join(",\n");
    let json = format!("[\n{body}\n]\n");
    fs::write(sysroot_out.join("manifest.json"), json).expect("write manifest.json");
}

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed={BUILD_SCRIPT}");
    println!("cargo::rerun-if-changed={VERUS_BUILTIN_SRC}");
    println!("cargo::rerun-if-changed={VSTD_SRC_DIR}");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let sysroot_out = out_dir.join("sysroot");
    let _ = fs::remove_dir_all(&sysroot_out);
    fs::create_dir_all(&sysroot_out).expect("mkdir sysroot out");

    // Expose the staged-sysroot path to integration tests via `env!("SYSROOT_DIR")`.
    // Emitted for both host and wasm32 builds so `env!` compiles either way.
    println!("cargo::rustc-env=SYSROOT_DIR={}", sysroot_out.display());

    let target = env::var("TARGET").unwrap_or_default();
    if target != "wasm32-unknown-unknown" {
        // Empty layout on host — keep consumers that unconditionally read the
        // manifest compiling without `#[cfg]` fences.
        write_manifest(&sysroot_out, &[]);
        return;
    }

    // Stage a virtual sysroot. rustc's wasm32 crate locator looks for
    // `<sysroot>/lib/rustlib/wasm32-unknown-unknown/lib/lib<name>-*.rmeta`,
    // so the script builds every rmeta into exactly that path. Subsequent
    // `--sysroot=<staged>` invocations resolve extern crates against these.
    let staged_sysroot = out_dir.join("staged_sysroot");
    let staged_lib = staged_sysroot.join("lib/rustlib/wasm32-unknown-unknown/lib");
    // Wipe any stale rmetas from a previous run — we copy everything in
    // staged_lib unconditionally, so a stale entry would surface as a
    // duplicate (or worse, an SVH-mismatched) crate in the bundle.
    let _ = fs::remove_dir_all(&staged_sysroot);
    fs::create_dir_all(&staged_lib).expect("mkdir staged sysroot lib");

    // Stage all wasm32 rmetas + vstd.vir in one shot.
    let status = Command::new(BUILD_SCRIPT)
        .arg(&staged_sysroot)
        .status()
        .unwrap_or_else(|e| panic!("spawn {BUILD_SCRIPT}: {e}"));
    assert!(status.success(), "{BUILD_SCRIPT} failed");
    let vstd_vir_path = staged_lib.join(VSTD_VIR);
    assert!(
        staged_lib.join(VSTD_RMETA).exists(),
        "expected {VSTD_RMETA} in staged sysroot lib after rust_verify --compile"
    );
    assert!(
        vstd_vir_path.exists(),
        "expected {VSTD_VIR} in staged sysroot lib after rust_verify --export"
    );

    // Collect every .rmeta in staged_lib, plus vstd.vir, sorted for stable order.
    let mut names: Vec<String> = fs::read_dir(&staged_lib)
        .expect("read staged lib")
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".rmeta"))
        .collect();
    names.push(VSTD_VIR.to_string());
    names.sort();

    for name in &names {
        fs::copy(staged_lib.join(name), sysroot_out.join(name))
            .unwrap_or_else(|e| panic!("copy {name} into sysroot out: {e}"));
    }
    write_manifest(&sysroot_out, &names);
}
