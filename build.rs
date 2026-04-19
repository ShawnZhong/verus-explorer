// Bundles wasm32-unknown-unknown rmeta files into the wasm binary so the
// vendored rustc crates can resolve extern crates without a real filesystem.
// Bundles, in order:
//   1. libcore / liballoc / libcompiler_builtins — from the host sysroot,
//      enough for `no_std` name resolution + typeck.
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

// Proc-macro crates we bundle the host-built rmeta for. Each lands in the
// virtual sysroot as `lib<name>-explorer.rmeta`. The actual macro expansion is
// handled by `src/proc_macros.rs` (in-process registry), so only name
// resolution consumes these rmetas.
const HOST_PROC_MACRO_CRATES: &[&str] = &["verus_builtin_macros", "verus_state_machines_macros"];

const VSTD_SRC_DIR: &str = "third_party/verus/source/vstd";
const VSTD_SRC_ENTRY: &str = "third_party/verus/source/vstd/vstd.rs";
const VSTD_RMETA: &str = "libvstd.rmeta";
const VSTD_VIR: &str = "vstd.vir";

const VERUS_HOST_DIR: &str = "third_party/verus/source/target/release";

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
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

    // Pull each host proc-macro crate's rmeta straight out of the host Cargo
    // target dir — its SVH must match the host dylib that rust_verify uses
    // as `--extern <name>=...` when compiling vstd. A shim built from
    // different source has a different SVH and trips E0460/E0786 when
    // user-code rustc-in-wasm later loads vstd.rmeta. The matching
    // standalone .rmeta is produced by `make verus-host`'s
    // `cargo rustc -- --emit=link,metadata` invocation.
    for name in HOST_PROC_MACRO_CRATES {
        let staged_name = format!("lib{name}-explorer.rmeta");
        copy_host_macros_rmeta(&out_dir, name, &staged_name);
        src.push_str(&format!(
            "    ({staged_name:?}, include_bytes!({staged_name:?})),\n"
        ));
    }

    // Real vstd, compiled via the host rust_verify driver. Produces both
    // `libvstd.rmeta` (wasm32 metadata for the embedded sysroot) and
    // `vstd.vir` (bincode-serialized VIR krate for the verifier).
    compile_vstd_via_rust_verify(&out_dir);
    src.push_str(&format!(
        "    ({VSTD_RMETA:?}, include_bytes!({VSTD_RMETA:?})),\n"
    ));

    src.push_str("];\n");
    src.push_str(&format!("pub static VSTD_VIR: &[u8] = include_bytes!({VSTD_VIR:?});\n"));
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

fn copy_host_macros_rmeta(out_dir: &Path, crate_name: &str, staged_name: &str) {
    let deps_dir = PathBuf::from(VERUS_HOST_DIR).join("deps");
    let dylib = PathBuf::from(VERUS_HOST_DIR).join(if cfg!(target_os = "macos") {
        format!("lib{crate_name}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{crate_name}.dll")
    } else {
        format!("lib{crate_name}.so")
    });
    // The canonical $VERUS_HOST_DIR/lib<crate>.dylib symlinks (or
    // otool-references) to one specific `deps/lib<crate>-<hash>.dylib`.
    // Pair it with the matching `.rmeta` of the same hash — that's the
    // standalone metadata file produced by `cargo rustc --emit=link,metadata`.
    let canonical_dep = read_dylib_install_name(&dylib, crate_name).unwrap_or_else(|| {
        panic!(
            "couldn't determine which deps/ dylib backs {:?} — did `make verus-host` run?",
            dylib
        )
    });
    let dep_filename = canonical_dep
        .file_name()
        .and_then(|n| n.to_str())
        .expect("install name has filename");
    let rmeta_name = dep_filename
        .strip_suffix(".dylib")
        .or_else(|| dep_filename.strip_suffix(".so"))
        .or_else(|| dep_filename.strip_suffix(".dll"))
        .map(|stem| format!("{stem}.rmeta"))
        .expect("dylib filename has expected suffix");
    let rmeta_path = deps_dir.join(&rmeta_name);
    assert!(
        rmeta_path.exists(),
        "expected matching {rmeta_name} alongside dylib — \
         re-run `make verus-host` to emit it via --emit=link,metadata"
    );
    let staged = out_dir.join(staged_name);
    fs::copy(&rmeta_path, &staged).expect("copy host proc-macro rmeta");
}

// Read the `LC_LOAD_DYLIB` install name out of a Mach-O dylib. On macOS
// `cargo build` writes a tiny "wrapper" dylib at $target/release/<name>.dylib
// that points at the hashed deps/<name>-<hash>.dylib via this load command.
// On Linux/Windows there's no wrapper — the canonical path *is* the hashed
// file; in that case we fall back to using the path we were given.
fn read_dylib_install_name(dylib: &Path, crate_name: &str) -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return Some(dylib.to_path_buf());
    }
    let out = Command::new("otool").arg("-L").arg(dylib).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8(out.stdout).ok()?;
    // `otool -L` output: header line, then `\t<path> (compat ..., current ...)`
    // for each LC_LOAD_DYLIB. The first such entry on a wrapper is the
    // hashed deps/ dylib.
    for line in stdout.lines().skip(1) {
        let trimmed = line.trim_start();
        if let Some(end) = trimmed.find(" (") {
            let path = PathBuf::from(&trimmed[..end]);
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.contains(crate_name) && n.contains('-'))
                .unwrap_or(false)
            {
                return Some(path);
            }
        }
    }
    None
}

// Drives the host `rust_verify` (built by `make verus-host`) on Verus's vstd
// source to emit a wasm32 rmeta + serialized VIR krate into OUT_DIR.
//
// rust_verify wraps a host rustc that loads:
//  - `verus_builtin` as a wasm32 rmeta (the same one we compile above for the
//    embedded sysroot — keeps stable_crate_ids consistent so vstd.rmeta's
//    dependency entry matches what user code's rustc-in-wasm later finds).
//  - `verus_builtin_macros` and `verus_state_machines_macros` as host dylibs
//    (needed for proc-macro expansion during compilation).
//
// `--is-vstd` + `VSTD_KIND=IsVstd` switch the proc-macros into "we are vstd"
// mode (different name lookups, different prelude). `--compile` triggers the
// post-verify pass that emits rmeta. `--no-verify --no-lifetime` skip the
// SMT/lifetime passes — we only need the type info and VIR; the verifier in
// the wasm runtime never re-checks vstd's bodies.
//
// `RUSTFLAGS=--cfg=verus_keep_ghost` had to be set on the host build of
// verus_builtin_macros for `cfg_erase()` to consult the target crate's cfg
// instead of unconditionally erasing — otherwise vstd typecheck fails with
// ~85 E0603 "private import" errors. (See `make verus-host`.)
fn compile_vstd_via_rust_verify(out_dir: &Path) {
    let host_dir = PathBuf::from(VERUS_HOST_DIR);
    let rust_verify_bin = host_dir.join("rust_verify");
    let dylib_ext = if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    };
    let macros_dylib = host_dir.join(format!("libverus_builtin_macros.{dylib_ext}"));
    let sm_macros_dylib = host_dir.join(format!("libverus_state_machines_macros.{dylib_ext}"));
    let builtin_rmeta = out_dir.join(VERUS_BUILTIN_RMETA);

    for (label, path) in [
        ("rust_verify binary", &rust_verify_bin),
        ("verus_builtin_macros dylib", &macros_dylib),
        ("verus_state_machines_macros dylib", &sm_macros_dylib),
    ] {
        assert!(
            path.exists(),
            "missing {label} at {:?} — run `make verus-host` first",
            path
        );
    }

    // rust_verify's host rustc needs librustc_driver.dylib visible at
    // load time. The toolchain's sysroot/lib has it via the rustc-dev
    // component; the dyld fallback path is the simplest way to expose it
    // without modifying SIP-restricted env vars.
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let sysroot = rustc_sysroot(&rustc);
    let dyld_lib = sysroot.join("lib");

    let abs_out_dir = out_dir.canonicalize().expect("canonicalize OUT_DIR");
    let abs_builtin = builtin_rmeta.canonicalize().expect("canonicalize verus_builtin rmeta");
    let abs_macros = macros_dylib.canonicalize().expect("canonicalize macros dylib");
    let abs_sm_macros = sm_macros_dylib.canonicalize().expect("canonicalize state_machines_macros dylib");
    let abs_rust_verify = rust_verify_bin.canonicalize().expect("canonicalize rust_verify");
    let abs_vstd_entry = PathBuf::from(VSTD_SRC_ENTRY).canonicalize().expect("canonicalize vstd entry");
    let vstd_vir_path = abs_out_dir.join(VSTD_VIR);

    let status = Command::new(&abs_rust_verify)
        .env("DYLD_FALLBACK_LIBRARY_PATH", &dyld_lib)
        .env("LD_LIBRARY_PATH", &dyld_lib)
        .env("RUSTC_BOOTSTRAP", "1")
        .env("RUST_MIN_STACK", (10 * 1024 * 1024).to_string())
        .env("VSTD_KIND", "IsVstd")
        .arg("--internal-test-mode")
        .arg("--target=wasm32-unknown-unknown")
        .arg("--emit=metadata")
        .arg(format!("--extern=verus_builtin={}", abs_builtin.display()))
        .arg(format!("--extern=verus_builtin_macros={}", abs_macros.display()))
        .arg(format!("--extern=verus_state_machines_macros={}", abs_sm_macros.display()))
        .arg("--crate-type=lib")
        .arg("--out-dir")
        .arg(&abs_out_dir)
        .arg("--export")
        .arg(&vstd_vir_path)
        .arg("--multiple-errors")
        .arg("2")
        .arg("--is-vstd")
        .arg("--compile")
        .arg("--no-verify")
        .arg("--no-lifetime")
        // vstd's `std` feature pulls in `extern crate std;`, but our embedded
        // sysroot bundles only `core` + `alloc`. Build alloc-only — that
        // covers Seq/Map/Set/Vec specifications without forcing libstd into
        // the bundle.
        .arg("--cfg")
        .arg("feature=\"alloc\"")
        .arg(&abs_vstd_entry)
        .status()
        .expect("spawn rust_verify for vstd");
    assert!(status.success(), "vstd compile via rust_verify failed");
    assert!(
        out_dir.join(VSTD_RMETA).exists(),
        "expected {VSTD_RMETA} in OUT_DIR after rust_verify --compile"
    );
    assert!(
        vstd_vir_path.exists(),
        "expected {VSTD_VIR} in OUT_DIR after rust_verify --export"
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
