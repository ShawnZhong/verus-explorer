// Bundles wasm32-unknown-unknown rmeta files into the wasm binary so the
// vendored rustc crates can resolve extern crates without a real filesystem.
// Bundles, in order:
//   1. libcore / libcompiler_builtins / liballoc — self-built from the
//      `rust-src` component into a staged sysroot here. Self-building (rather
//      than copying rustup's prebuilt rmetas) shaves ~15 MB raw because the
//      shipped rmetas are compiled with `-Z always-encode-mir=yes` for cross-
//      crate inlining, which we don't need.
//   2. libverus_builtin — compiled on demand from Verus's `builtin` crate
//      source to wasm32 rmeta here, so Verus's diagnostic-item lookups
//      (`verus::verus_builtin::*`) resolve at runtime.
//   3. libverus_builtin_macros — `pub macro` stub crate matching the names
//      `vstd` re-exports; the in-wasm proc-macro registry handles the actual
//      expansion (see `src/proc_macros.rs`).
//   4. libvstd — produced by invoking the host `rust_verify` driver against
//      Verus's real `vstd` source for wasm32, so user code can `use
//      vstd::prelude::*;` and the verifier loads the matching VIR.
// Plus `vstd.vir` (bincode-serialized VIR krate), embedded as a separate
// `VSTD_VIR: &[u8]` constant so `pipeline::build_vir` can hand it to
// `Verifier::build_vir_crate` as `other_vir_crates`.
//
// Steps 1, 2, and 4 are shelled out to `scripts/build-wasm-sysroot.sh`
// (base / vstd subcommands). Step 3 stays here because it parses Mach-O
// `.rustc` sections via the `object` crate — awkward to do in shell.
//
// Both verus_builtin and vstd are built with `--sysroot=<staged>` so they
// pick up our self-built libcore (different SVH from rustup's prebuilt) —
// otherwise the bundle's dep chain would mismatch and surface as E0464 /
// E0460 at user-code load time.
//
// Rlib object code is not bundled (we never codegen).
//
// Runs only when building for wasm32. For host/test builds we emit an empty
// `FILES` table — the host has a real filesystem and doesn't need the bundle.

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
const BUILD_SCRIPT: &str = "scripts/build-wasm-sysroot.sh";

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

    // Phase 1: core + compiler_builtins + alloc + verus_builtin.
    run_sysroot_script("base", &staged_sysroot);

    // Phase 2: pull each host proc-macro crate's rmeta out of the dylib's
    // embedded `.rustc` section. Its SVH must match the host dylib that
    // rust_verify uses as `--extern <name>=...` when compiling vstd — by
    // reading the bytes from the same dylib, SVH match is guaranteed.
    for name in HOST_PROC_MACRO_CRATES {
        let staged_name = format!("lib{name}-explorer.rmeta");
        extract_host_macros_rmeta(&staged_lib, name, &staged_name);
    }

    // Phase 3: vstd via the host rust_verify driver. Needs phases 1 + 2
    // already staged — rust_verify passes the host proc-macro dylibs as
    // --extern and resolves vstd's `core`/`alloc`/`compiler_builtins`
    // deps against the rmetas built in phase 1.
    run_sysroot_script("vstd", &staged_sysroot);
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
    let mut src = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    let mut entries: Vec<String> = fs::read_dir(&staged_lib)
        .expect("read staged lib")
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".rmeta"))
        .collect();
    entries.sort();
    for name in &entries {
        let staged = staged_lib.join(name);
        let staged_str = staged.to_str().expect("rmeta path utf8");
        src.push_str(&format!("    ({name:?}, include_bytes!({staged_str:?})),\n"));
    }
    src.push_str("];\n");
    let vir_str = vstd_vir_path.to_str().expect("vstd.vir path utf8");
    src.push_str(&format!("pub static VSTD_VIR: &[u8] = include_bytes!({vir_str:?});\n"));
    fs::write(&bundle_rs, src).expect("write sysroot_bundle.rs");
}

fn run_sysroot_script(sub: &str, staged_sysroot: &Path) {
    let status = Command::new(BUILD_SCRIPT)
        .arg(sub)
        .arg(staged_sysroot)
        .status()
        .unwrap_or_else(|e| panic!("spawn {BUILD_SCRIPT} {sub}: {e}"));
    assert!(status.success(), "{BUILD_SCRIPT} {sub} failed");
}

// Pull the rmeta bytes out of a host proc-macro dylib's embedded `.rustc`
// section. rustc always stores crate metadata there so cargo can load
// proc-macros via `extern crate`. The section layout is a 16-byte header
// (`rust\0\0\0\x0a` magic + u64 LE length) followed by the rmeta bytes —
// which are byte-identical to what `rustc --emit=metadata` would produce.
fn extract_host_macros_rmeta(staged_lib: &Path, crate_name: &str, staged_name: &str) {
    let dylib_ext = if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    };
    let dylib = PathBuf::from(VERUS_HOST_DIR).join(format!("lib{crate_name}.{dylib_ext}"));
    assert!(
        dylib.exists(),
        "missing host proc-macro dylib {:?} — run `make verus-host`",
        dylib
    );
    let bytes = fs::read(&dylib).expect("read host proc-macro dylib");
    let obj = object::File::parse(&*bytes).expect("parse host proc-macro dylib");
    let section = obj
        .section_by_name(".rustc")
        .expect(".rustc section in host proc-macro dylib");
    let data = section.data().expect(".rustc section data");
    assert!(
        data.len() >= 16 && &data[0..8] == b"rust\x00\x00\x00\x0a",
        "unexpected .rustc section header in {:?}",
        dylib
    );
    let staged = staged_lib.join(staged_name);
    fs::write(&staged, &data[16..]).expect("write extracted rmeta");
}
