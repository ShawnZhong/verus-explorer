// Bundles wasm32 rmetas into the binary so in-wasm rustc can resolve
// `extern crate` against a virtual sysroot. Bundles:
//   1. core / compiler_builtins / alloc — self-built from `rust-src`.
//      Saves ~15 MB vs rustup's prebuilts (which carry MIR for cross-
//      crate inlining we never use, since we never codegen).
//   2. verus_builtin — wasm32 rmeta for Verus's diagnostic-item lookups
//      (`verus::verus_builtin::*`).
//   3. verus_builtin_macros + verus_state_machines_macros — host rmeta
//      extracted from each proc-macro dylib's `.rustc` section. Can't
//      rebuild for wasm32 (proc-macros are host-only) and can't rebuild
//      a separate host copy either: rust_verify links these dylibs into
//      vstd.rmeta's dep table by SVH, and a from-scratch rebuild would
//      drift on any flag/env difference and surface as E0464 at user-
//      code load. Extracting from the same dylib pins SVH for free.
//   4. vstd.rmeta + vstd.vir — built by host rust_verify against the
//      staged sysroot above. The .vir blob (bincode-serialized VIR krate)
//      is exposed as a separate `VSTD_VIR: &[u8]` so `pipeline::build_vir`
//      can hand it to `Verifier::build_vir_crate` as `other_vir_crates`.
//
// Steps 1/2/4 run in `scripts/build-wasm-libs.sh`. Step 3 stays here
// because it parses Mach-O / ELF `.rustc` sections via the `object` crate.
// Everything is built with `--sysroot=<staged>` so the SVH chain lines up.
//
// On non-wasm targets we emit empty FILES/VSTD_VIR — the host has a real
// filesystem and doesn't need the bundle.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use object::{Object, ObjectSection};

const VERUS_BUILTIN_SRC: &str = "third_party/verus/source/builtin/src/lib.rs";

// Proc-macro crates we bundle the host-built rmeta for. Each lands in the
// virtual sysroot as `lib<name>-explorer.rmeta`. The actual macro expansion is
// handled by `src/proc_macros.rs` (in-process registry), so only name
// resolution consumes these rmetas.
const HOST_PROC_MACRO_CRATES: &[&str] = &["verus_builtin_macros", "verus_state_machines_macros"];

const VSTD_SRC_DIR: &str = "third_party/verus/source/vstd";
const VSTD_RMETA: &str = "libvstd.rmeta";
const VSTD_VIR: &str = "vstd.vir";

const VERUS_HOST_DIR: &str = "target/verus-host/release";
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

    // Stage all wasm32 rmetas + vstd.vir in one shot: core, compiler_builtins,
    // alloc, verus_builtin, vstd.
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

    // Pull each proc-macro dylib's `.rustc` section out as a sibling rmeta in
    // staged_lib. SVH is guaranteed to match what rust_verify already wrote
    // into vstd.rmeta's dep table because the bytes come from the same dylib.
    // Order vs. the script above doesn't matter — only ordering vs. the
    // staged_lib enumeration below does.
    for name in HOST_PROC_MACRO_CRATES {
        extract_host_macros_rmeta(&staged_lib, name);
    }

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

// Pull rmeta bytes out of a host proc-macro dylib's `.rustc` section.
// rustc stores crate metadata there for `extern crate` loading; the layout
// is an 8-byte magic (`rust\0\0\0\x0a`) + u64 LE length, then bytes that
// are identical to what `rustc --emit=metadata` would emit.
fn extract_host_macros_rmeta(staged_lib: &Path, crate_name: &str) {
    let dylib_ext = if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    };
    let dylib = PathBuf::from(VERUS_HOST_DIR).join(format!("lib{crate_name}.{dylib_ext}"));
    assert!(dylib.exists(), "missing {dylib:?} — run `make verus-host`");
    let bytes = fs::read(&dylib).expect("read host proc-macro dylib");
    let obj = object::File::parse(&*bytes).expect("parse host proc-macro dylib");
    let data = obj
        .section_by_name(".rustc")
        .expect(".rustc section")
        .data()
        .expect(".rustc section data");
    assert!(
        data.len() >= 16 && &data[0..8] == b"rust\x00\x00\x00\x0a",
        "unexpected .rustc header in {dylib:?}"
    );
    let staged = staged_lib.join(format!("lib{crate_name}-explorer.rmeta"));
    fs::write(&staged, &data[16..]).expect("write extracted rmeta");
}
