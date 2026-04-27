// Smoke test for `verus_explorer::run` — drives Miri on a simple
// std-mode `fn main` and asserts captured stdout. Separate cdylib
// from `smoke.rs` because wasm-bindgen-test runs each `tests/*.rs`
// in its own wasm instance, so the `wasm_libs_finalize` once-cell
// (one-shot per instance) doesn't collide between the verify and
// run smoke paths.

use wasm_bindgen::prelude::*;
use wasm_bindgen_test::*;

#[wasm_bindgen(module = "fs")]
extern "C" {
    #[wasm_bindgen(js_name = readFileSync)]
    fn read_file_sync(path: &str) -> Vec<u8>;
    #[wasm_bindgen(js_name = readdirSync)]
    fn readdir_sync(path: &str) -> Vec<JsValue>;
}

// Stubs for the `verus_*` JS externs `src/lib.rs` declares. We don't
// care about diagnostics or VIR/AIR/SMT dumps in this test — we only
// look at stdout (`verus_run_stdout`) and stderr (`verus_run_stderr`).
// Both accumulate into `globalThis._stdout` / `_stderr` so the test
// can read them after `run()` returns.
#[wasm_bindgen(inline_js = "\
    export function install_run_stubs() {\n\
      globalThis._stdout = '';\n\
      globalThis._stderr = '';\n\
      globalThis.verus_run_stdout = (s) => { globalThis._stdout += s; };\n\
      globalThis.verus_run_stderr = (s) => { globalThis._stderr += s; };\n\
      globalThis.verus_diagnostic = (json) => { process.stderr.write('[diag] ' + json + '\\n'); };\n\
      globalThis.verus_dump = () => {};\n\
      globalThis.verus_verdict = () => {};\n\
      globalThis.verus_bench = (label, ms) => { process.stderr.write(`[stage] ${label}=${ms.toFixed(0)}ms\\n`); };\n\
    }\n\
    export function get_stdout() { return globalThis._stdout || ''; }\n\
    export function get_stderr() { return globalThis._stderr || ''; }")]
extern "C" {
    fn install_run_stubs();
    fn get_stdout() -> String;
    fn get_stderr() -> String;
}

#[wasm_bindgen_test]
fn run_executes_main_and_captures_stdout() {
    verus_explorer::init();
    install_run_stubs();

    // Run mode requires libstd (start lang item lives there). Pick std
    // mode + load the std-flavored libs bundle.
    verus_explorer::set_std_mode(true);
    const LIBS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/libs");
    let stream_dir = |sub: &str| {
        let dir = format!("{LIBS}/{sub}");
        for entry in readdir_sync(&dir) {
            let name = entry.as_string().expect("readdirSync returns strings");
            if !(name.ends_with(".rmeta") || name == "vstd.vir") {
                continue;
            }
            let bytes = read_file_sync(&format!("{dir}/{name}"));
            verus_explorer::wasm_libs_add_file(name, bytes);
        }
    };
    stream_dir("");
    stream_dir("std");
    verus_explorer::wasm_libs_finalize();

    // Minimal exec-only program. No `verus!` block — Miri doesn't care
    // about Verus semantics, just MIR. We use Miri's `miri_write_to_*`
    // intrinsics directly to bypass libstd's stdio buffering layer
    // (which is more complex to wire end-to-end). `println!` is also
    // exercised — if our libstd patch in `library/std/src/sys/stdio/
    // unsupported.rs` works, `println!` lands in the same channel.
    let src = r#"
        unsafe extern "Rust" {
            safe fn miri_write_to_stdout(buf: &[u8]);
            safe fn miri_write_to_stderr(buf: &[u8]);
        }
        fn main() {
            miri_write_to_stdout(b"direct stdout\n");
            miri_write_to_stderr(b"direct stderr\n");
            println!("hello from miri");
            eprintln!("hello stderr");
        }
    "#;

    verus_explorer::run(src);

    let stdout = get_stdout();
    let stderr = get_stderr();
    process::stderr_write(&format!("captured stdout = {:?}\n", stdout));
    process::stderr_write(&format!("captured stderr = {:?}\n", stderr));

    assert!(
        stdout.contains("hello from miri"),
        "expected user stdout in capture, got: {stdout:?}"
    );
    assert!(
        stderr.contains("hello stderr"),
        "expected user stderr in capture, got: {stderr:?}"
    );
}

mod process {
    use wasm_bindgen::prelude::*;
    #[wasm_bindgen(inline_js = "export function stderr_write(s) { process.stderr.write(s); }")]
    extern "C" {
        pub fn stderr_write(s: &str);
    }
}
