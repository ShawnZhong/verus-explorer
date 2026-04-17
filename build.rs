// Bundles wasm32-unknown-unknown rmeta files into the wasm binary so the
// vendored rustc crates can resolve extern crates without a real filesystem.
// Bundles two groups:
//   1. libcore / liballoc / libcompiler_builtins — from the host sysroot,
//      enough for `no_std` name resolution + typeck.
//   2. libverus_builtin — compiled on demand from Verus's `builtin` crate
//      source to wasm32 rmeta here, so Verus's diagnostic-item lookups
//      (`verus::verus_builtin::*`) resolve at runtime.
// Rlib object code is not bundled (we never codegen).
//
// Runs only when building for wasm32. For host/test builds we emit an empty
// `FILES` table — the host has a real filesystem and doesn't need the bundle.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const CRATES_TO_BUNDLE: &[&str] = &[
    "libcore",
    "libcompiler_builtins",
    "liballoc",
];

const VERUS_BUILTIN_SRC: &str = "third_party/verus/source/builtin/src/lib.rs";
const VERUS_BUILTIN_RMETA: &str = "libverus_builtin-explorer.rmeta";

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed={VERUS_BUILTIN_SRC}");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    let target = env::var("TARGET").unwrap_or_default();
    let bundle_rs = out_dir.join("sysroot_bundle.rs");

    if target != "wasm32-unknown-unknown" {
        fs::write(&bundle_rs, "pub static FILES: &[(&str, &[u8])] = &[];\n")
            .expect("write empty bundle");
        return;
    }

    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let sysroot = rustc_sysroot(&rustc);
    let target_lib = sysroot.join("lib/rustlib/wasm32-unknown-unknown/lib");
    println!("cargo::rerun-if-changed={}", target_lib.display());

    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(&target_lib).expect("read target lib dir") {
        let entry = entry.expect("read_dir entry");
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !name.ends_with(".rmeta") {
            continue;
        }
        if !CRATES_TO_BUNDLE
            .iter()
            .any(|prefix| name.starts_with(&format!("{prefix}-")))
        {
            continue;
        }
        entries.push((name, path));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut src = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    for (name, path) in &entries {
        let staged = out_dir.join(name);
        fs::copy(path, &staged).expect("copy rmeta");
        src.push_str(&format!("    ({name:?}, include_bytes!({name:?})),\n"));
    }

    // Compile verus_builtin to wasm32 rmeta straight into OUT_DIR so it lands
    // next to the copied libcore/liballoc rmetas. The `cfg(verus_keep_ghost)`
    // blocks in the source register the `#[rustc_diagnostic_item = ...]` names
    // Verus looks up; the feature gates they require are declared inline via
    // `#![cfg_attr(verus_keep_ghost, feature(...))]`.
    compile_verus_builtin(&rustc, &out_dir);
    src.push_str(&format!("    ({VERUS_BUILTIN_RMETA:?}, include_bytes!({VERUS_BUILTIN_RMETA:?})),\n"));

    src.push_str("];\n");
    fs::write(&bundle_rs, src).expect("write sysroot_bundle.rs");
}

fn compile_verus_builtin(rustc: &str, out_dir: &Path) {
    let status = Command::new(rustc)
        .arg("--edition=2018")
        .arg("--crate-type=lib")
        .arg("--crate-name=verus_builtin")
        .arg("--target=wasm32-unknown-unknown")
        .arg("--emit=metadata")
        .arg("--cfg=verus_keep_ghost")
        // Produces `libverus_builtin-explorer.rmeta` (must match VERUS_BUILTIN_RMETA).
        .arg("-Cextra-filename=-explorer")
        .arg("--out-dir")
        .arg(out_dir)
        .arg(VERUS_BUILTIN_SRC)
        .status()
        .expect("spawn rustc for verus_builtin");
    assert!(status.success(), "verus_builtin compile failed");
    assert!(
        out_dir.join(VERUS_BUILTIN_RMETA).exists(),
        "expected {VERUS_BUILTIN_RMETA} in OUT_DIR"
    );
}

fn rustc_sysroot(rustc: &str) -> PathBuf {
    let out = Command::new(rustc)
        .arg("--print")
        .arg("sysroot")
        .output()
        .expect("rustc --print sysroot");
    let s = String::from_utf8(out.stdout).expect("sysroot utf8");
    PathBuf::from(s.trim())
}
