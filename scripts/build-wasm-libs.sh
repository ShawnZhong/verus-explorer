#!/usr/bin/env bash
# Build wasm32 rmetas into $out, laid out as a virtual sysroot so rustc-
# in-wasm's crate locator (which hard-codes `<sysroot>/lib/rustlib/
# <triple>/lib/`) resolves `extern crate` at runtime. Produces, in order:
#   core + compiler_builtins + alloc — self-built from the rust-src component.
#     Shaves ~15 MB vs rustup's prebuilt rmetas (which carry
#     `-Z always-encode-mir=yes` we don't need since we never codegen).
#   verus_builtin — registers the `#[rustc_diagnostic_item = ...]` names
#     Verus looks up.
#   verus_builtin_macros, verus_state_machines_macros — `--cfg=stub_only`
#     rmetas exposing only the `pub macro NAME` decl_macro shims, enough
#     for vstd's name resolution. Their full crates (with the `MACROS`
#     descriptor slices) get built separately by cargo for the host
#     (rust_verify) and the explorer's wasm binary.
#   vstd.rmeta + vstd.vir — vstd compiled via the host rust_verify driver
#     against our self-built sysroot (SVH chain matches user-code rustc-
#     in-wasm's lookups).
#
# `rustc --emit=metadata` without `-Cextra-filename` produces plain
# `lib<crate>.rmeta` files (no hash suffix), so the `--extern=...` paths
# below are stable and need no globbing. The in-wasm crate locator
# accepts both hashed (`lib<name>-<hash>.rmeta`) and unhashed names.
#
# $out is both the write destination and the `--sysroot=` value passed to
# subsequent rustc/rust_verify invocations; rmetas land in
# $out/lib/rustlib/wasm32-unknown-unknown/lib/. build.rs passes
# `target/wasm-libs/` (the default below) so re-running this script
# manually hits the same location.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
repo="$PWD"

out="${1:-target/wasm-libs}"
lib="$out/lib/rustlib/wasm32-unknown-unknown/lib"
mkdir -p "$lib"

# Always use the patched stage1 rustc — never inherit cargo's RUSTC, which on
# build.rs invocations is set to rustup's rustc and would (1) skew SVHs vs.
# what `rustc-in-wasm` later sees and (2) point `--print sysroot` at rustup,
# so DYLD_FALLBACK_LIBRARY_PATH below would miss rust_verify's
# `librustc_driver-*.dylib` (which lives under host-rust's rustlib).
RUSTC="$repo/target/host-rust/bin/rustc"
[ -x "$RUSTC" ] || {
    echo "missing $RUSTC — run \`make host-rust\` first." >&2
    exit 1
}
export RUSTC_BOOTSTRAP=1

# Rewrite `$repo/` → `` in every rustc/rust_verify invocation so the spans
# baked into rmetas and `vstd.vir` are repo-relative instead of leaking the
# developer's absolute path. rustc applies this to all `RemapPathScopeComponents`
# by default, which includes DIAGNOSTICS — the scope `span_to_diagnostic_string`
# reads when producing the `span.as_string` field Verus serializes into the VIR.
remap="--remap-path-prefix=$repo/="

# Chain: core (no deps) → compiler_builtins (deps core) → alloc (deps core
# + compiler_builtins) → verus_builtin (deps core). compiler_builtins'
# `compiler-builtins` feature flips on its `#[compiler_builtins]` attr.
# `--cap-lints=allow` on compiler_builtins silences ~700 unexpected_cfgs
# warnings: cargo's normal build runs compiler_builtins' own build.rs which
# emits `cargo:rustc-check-cfg=cfg(__ashldi3)` etc. for every intrinsic +
# feature it probes for; we bypass cargo so those declarations never fire.
rust_src="$("$RUSTC" --print sysroot)/lib/rustlib/src/rust/library"
[ -f "$rust_src/core/src/lib.rs" ] || {
    echo "rust-src component missing — run \`rustup component add rust-src\`." >&2
    exit 1
}

set -x
"$RUSTC" --edition=2024 --crate-type=lib --crate-name=core \
    --target=wasm32-unknown-unknown --emit=metadata \
    "$remap" \
    --out-dir "$lib" \
    "$rust_src/core/src/lib.rs"

"$RUSTC" --edition=2024 --crate-type=lib --crate-name=compiler_builtins \
    --target=wasm32-unknown-unknown --emit=metadata \
    --cfg='feature="compiler-builtins"' \
    --check-cfg='cfg(feature, values("compiler-builtins"))' \
    --cap-lints=allow \
    --sysroot="$out" \
    "$remap" \
    --out-dir "$lib" \
    "$rust_src/compiler-builtins/compiler-builtins/src/lib.rs"

"$RUSTC" --edition=2024 --crate-type=lib --crate-name=alloc \
    --target=wasm32-unknown-unknown --emit=metadata \
    --sysroot="$out" \
    "$remap" \
    --out-dir "$lib" \
    "$rust_src/alloc/src/lib.rs"

# verus_builtin cfg-gated behind `verus_keep_ghost`; feature gates come
# from `#![cfg_attr(verus_keep_ghost, feature(...))]`.
"$RUSTC" --edition=2018 --crate-type=lib --crate-name=verus_builtin \
    --target=wasm32-unknown-unknown --emit=metadata \
    --cfg=verus_keep_ghost \
    --sysroot="$out" \
    "$remap" \
    --out-dir "$lib" \
    "$repo/third_party/verus/source/builtin/src/lib.rs"

# Stubs-only macro rmetas (`--cfg=stub_only` cfg-gates out the proc_macro/
# syn/quote-using impl fns + `MACROS` slice — see each crate's lib.rs
# header). Each is a wasm32 rmeta exposing only the `pub macro NAME`
# decl_macro stubs, exactly what vstd's build (`--extern=...` below) and the
# bundled sysroot need for name resolution.
build_stub_rmeta() {
    local name=$1 src=$2
    "$RUSTC" --edition=2018 --crate-type=lib --crate-name="$name" \
        --target=wasm32-unknown-unknown --emit=metadata \
        --cfg=stub_only \
        --check-cfg='cfg(stub_only)' \
        --check-cfg='cfg(verus_keep_ghost)' \
        --sysroot="$out" \
        "$remap" \
        --out-dir "$lib" \
        "$src"
}
build_stub_rmeta verus_builtin_macros \
    "$repo/third_party/verus/source/builtin_macros/src/lib.rs"
build_stub_rmeta verus_state_machines_macros \
    "$repo/third_party/verus/source/state_machines_macros/src/lib.rs"

# vstd via host rust_verify. --sysroot=$out resolves core/alloc/
# compiler_builtins against our self-built rmetas (matching SVH with
# what user-code rustc-in-wasm later sees). --is-vstd + VSTD_KIND=IsVstd
# flip proc-macros into "we are vstd" mode. --compile emits rmeta;
# --no-verify / --no-lifetime skip SMT + lifetime passes (we only need
# type info + VIR — the in-wasm verifier never re-checks vstd's bodies).
# `feature="alloc"` (not "std") because the embedded sysroot bundles
# only core + alloc.
{ set +x; } 2>/dev/null
host_dir="$repo/target/host-verus/release"
rust_verify="$host_dir/rust_verify"
# Both macro crates' wasm32 stub rmetas were built directly above (lives in
# $lib next to verus_builtin); vstd's --externs point at those.
macros="$lib/libverus_builtin_macros.rmeta"
sm_macros="$lib/libverus_state_machines_macros.rmeta"
[ -e "$rust_verify" ] || {
    echo "missing host artifact: $rust_verify — run \`make host-verus\` first." >&2
    exit 1
}

# rust_verify's host rustc loads librustc_driver.dylib at launch; DYLD
# fallback exposes the toolchain's sysroot/lib without fighting SIP. The
# rustc_private dylib that rust_verify actually links against lives under
# the host-triple rustlib path (dist rustc-dev places it there), while the
# rustup-shipped variant sits directly in sysroot/lib — include both.
host_triple="$("$RUSTC" -vV | awk '/^host:/ {print $2}')"
sysroot="$("$RUSTC" --print sysroot)"
dyld_lib="$sysroot/lib:$sysroot/lib/rustlib/$host_triple/lib"

set -x
DYLD_FALLBACK_LIBRARY_PATH="$dyld_lib" \
LD_LIBRARY_PATH="$dyld_lib" \
RUST_MIN_STACK=$((10 * 1024 * 1024)) \
VSTD_KIND=IsVstd \
    "$rust_verify" \
    --internal-test-mode \
    --target=wasm32-unknown-unknown \
    --emit=metadata \
    --sysroot="$out" \
    --extern=verus_builtin="$lib/libverus_builtin.rmeta" \
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
    "$remap" \
    "$repo/third_party/verus/source/vstd/vstd.rs"
