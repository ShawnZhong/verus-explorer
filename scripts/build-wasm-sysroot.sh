#!/usr/bin/env bash
# Build wasm32 rmetas into <staged>/lib/rustlib/wasm32-unknown-unknown/lib/
# so rustc-in-wasm's crate locator resolves `extern crate` at runtime.
# build.rs invokes this in two phases:
#   base — core + compiler_builtins + alloc (from rust-src) + verus_builtin.
#   vstd — vstd via the host rust_verify driver. Runs after build.rs has
#          staged the host proc-macro rmetas (extracted from each dylib's
#          `.rustc` section) alongside the base rmetas.
# `-Cextra-filename=-explorer` gives every rmeta a predictable filename
# matching the names the in-wasm crate locator probes for. Self-building
# core/alloc/compiler_builtins (rather than copying rustup's prebuilt
# rmetas) shaves ~15 MB — shipped rmetas carry `-Z always-encode-mir=yes`
# metadata that we don't need since we never codegen in wasm.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
repo="$PWD"

sub="${1:?subcommand required (base|vstd)}"
staged="${2:?staged sysroot path required}"
lib="$staged/lib/rustlib/wasm32-unknown-unknown/lib"
mkdir -p "$lib"

RUSTC="${RUSTC:-rustc}"
export RUSTC_BOOTSTRAP=1

case "$sub" in
  base)
    # Chain: core (no deps) → compiler_builtins (deps core) → alloc (deps
    # core + compiler_builtins). compiler_builtins' `compiler-builtins`
    # feature flips on its `#[compiler_builtins]` crate-level attr.
    rust_src="$("$RUSTC" --print sysroot)/lib/rustlib/src/rust/library"
    [ -f "$rust_src/core/src/lib.rs" ] || {
        echo "rust-src component missing — run \`rustup component add rust-src\`." >&2
        exit 1
    }

    set -x
    "$RUSTC" --edition=2024 --crate-type=lib --crate-name=core \
        --target=wasm32-unknown-unknown --emit=metadata \
        -Cextra-filename=-explorer \
        --out-dir "$lib" \
        "$rust_src/core/src/lib.rs"

    "$RUSTC" --edition=2024 --crate-type=lib --crate-name=compiler_builtins \
        --target=wasm32-unknown-unknown --emit=metadata \
        -Cextra-filename=-explorer \
        --cfg='feature="compiler-builtins"' \
        --check-cfg='cfg(feature, values("compiler-builtins"))' \
        --sysroot="$staged" \
        --out-dir "$lib" \
        "$rust_src/compiler-builtins/compiler-builtins/src/lib.rs"

    "$RUSTC" --edition=2024 --crate-type=lib --crate-name=alloc \
        --target=wasm32-unknown-unknown --emit=metadata \
        -Cextra-filename=-explorer \
        --sysroot="$staged" \
        --out-dir "$lib" \
        "$rust_src/alloc/src/lib.rs"

    # verus_builtin: registers the `#[rustc_diagnostic_item = ...]` names
    # Verus looks up. cfg-gated behind `verus_keep_ghost`; feature gates
    # come from `#![cfg_attr(verus_keep_ghost, feature(...))]`.
    "$RUSTC" --edition=2018 --crate-type=lib --crate-name=verus_builtin \
        --target=wasm32-unknown-unknown --emit=metadata \
        -Cextra-filename=-explorer \
        --cfg=verus_keep_ghost \
        --sysroot="$staged" \
        --out-dir "$lib" \
        "$repo/third_party/verus/source/builtin/src/lib.rs"
    ;;

  vstd)
    # Drives the host rust_verify binary over Verus's real vstd source.
    # --sysroot=<staged> resolves core/alloc/compiler_builtins against
    # our self-built rmetas (matching SVH with what user code rustc-in-
    # wasm later sees). --is-vstd + VSTD_KIND=IsVstd flip the proc-
    # macros into "we are vstd" mode. --compile emits rmeta; --no-verify
    # / --no-lifetime skip SMT + lifetime passes (we only need type info
    # + VIR — the in-wasm verifier never re-checks vstd's bodies).
    # `feature="alloc"` (not "std") because the embedded sysroot bundles
    # only core + alloc.
    host_dir="$repo/target/verus-host/release"
    case "$(uname -s)" in
        Darwin) dylib_ext=dylib ;;
        Linux) dylib_ext=so ;;
        *) dylib_ext=dll ;;
    esac
    rust_verify="$host_dir/rust_verify"
    macros="$host_dir/libverus_builtin_macros.$dylib_ext"
    sm_macros="$host_dir/libverus_state_machines_macros.$dylib_ext"
    for f in "$rust_verify" "$macros" "$sm_macros"; do
        [ -e "$f" ] || {
            echo "missing host artifact: $f — run \`make verus-host\` first." >&2
            exit 1
        }
    done

    # rust_verify's host rustc loads librustc_driver.dylib at launch; DYLD
    # fallback exposes the toolchain's sysroot/lib without fighting SIP.
    dyld_lib="$("$RUSTC" --print sysroot)/lib"

    set -x
    DYLD_FALLBACK_LIBRARY_PATH="$dyld_lib" \
    LD_LIBRARY_PATH="$dyld_lib" \
    RUST_MIN_STACK=$((10 * 1024 * 1024)) \
    VSTD_KIND=IsVstd \
        "$rust_verify" \
        --internal-test-mode \
        --target=wasm32-unknown-unknown \
        --emit=metadata \
        --sysroot="$staged" \
        --extern=verus_builtin="$lib/libverus_builtin-explorer.rmeta" \
        --extern=verus_builtin_macros="$macros" \
        --extern=verus_state_machines_macros="$sm_macros" \
        --crate-type=lib \
        --out-dir "$lib" \
        --export "$lib/vstd.vir" \
        --multiple-errors 2 \
        --is-vstd \
        --compile \
        --no-verify \
        --no-lifetime \
        --cfg 'feature="alloc"' \
        "$repo/third_party/verus/source/vstd/vstd.rs"
    ;;

  *)
    echo "unknown subcommand: $sub (expected 'base' or 'vstd')" >&2
    exit 1
    ;;
esac
