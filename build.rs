// Bundles wasm32 rmetas into the binary so in-wasm rustc can resolve
// `extern crate` against a virtual sysroot. Bundles:
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
//      is exposed as a separate `VSTD_VIR: &[u8]` so `pipeline::build_vir`
//      can hand it to `Verifier::build_vir_crate` as `other_vir_crates`.
//
// Everything is built by `scripts/build-wasm-libs.sh` with `--sysroot=<staged>`
// so the SVH chain lines up across all rmetas.
//
// On non-wasm targets we emit empty FILES/VSTD_VIR — the host has a real
// filesystem and doesn't need the bundle.

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

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    let target = env::var("TARGET").unwrap_or_default();
    let bundle_rs = out_dir.join("sysroot_bundle.rs");

    if target != "wasm32-unknown-unknown" {
        fs::write(
            &bundle_rs,
            "pub static FILES: &[(&str, &[u8])] = &[];\n\
             pub static VSTD_VIR: &[u8] = &[];\n",
        )
        .expect("write empty bundle");
        return;
    }

    // Stage a virtual sysroot. rustc's wasm32 crate locator looks for
    // `<sysroot>/lib/rustlib/wasm32-unknown-unknown/lib/lib<name>-*.rmeta`,
    // so the script builds every rmeta into exactly that path. Subsequent
    // `--sysroot=<staged>` invocations resolve extern crates against these.
    let staged_sysroot = out_dir.join("staged_sysroot");
    let staged_lib = staged_sysroot.join("lib/rustlib/wasm32-unknown-unknown/lib");
    // Wipe any stale rmetas from a previous run — we bundle everything in
    // staged_lib unconditionally, so a stale entry would surface as a
    // duplicate (or worse, an SVH-mismatched) crate in the embedded sysroot.
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

    // Emit the bundle: every .rmeta in staged_lib, sorted for stable order.
    let mut entries: Vec<String> = fs::read_dir(&staged_lib)
        .expect("read staged lib")
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".rmeta"))
        .collect();
    entries.sort();
    let mut src = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    for name in &entries {
        let path = staged_lib.join(name);
        src.push_str(&format!("    ({name:?}, include_bytes!({path:?})),\n"));
    }
    src.push_str("];\n");
    src.push_str(&format!(
        "pub static VSTD_VIR: &[u8] = include_bytes!({vstd_vir_path:?});\n"
    ));
    fs::write(&bundle_rs, src).expect("write sysroot_bundle.rs");
}
