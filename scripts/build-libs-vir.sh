#!/usr/bin/env bash
# Produce libvstd.rmeta + vstd.vir into $out (default
# target/libs-vir/<mode>/) by invoking host rust_verify against the
# libs-sysroot staged by build-libs-sysroot.sh.
#
# Usage: $0 <mode> [out-dir]
#   mode=std   — vstd built with `feature="alloc"` + `feature="std"`.
#                Unlocks `PPtr::{new,empty}`, `HashMap`, `println!`, …
#                User code compiles without `#![no_std]`, so its std
#                prelude is available.
#   mode=nostd — vstd built with `feature="alloc"` only. No libstd dep.
#                User code is `#![no_std]`. ~20% faster warm verify,
#                ~3 MB smaller bundle. Use when a proof doesn't need
#                std items.
#
# The Makefile's `libs-vir` target calls us once per mode.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
repo="$PWD"

mode="${1:?usage: $0 <std|nostd> [out-dir]}"
case "$mode" in
    std)   extra_cfg=(--cfg 'feature="alloc"' --cfg 'feature="std"') ;;
    nostd) extra_cfg=(--cfg 'feature="alloc"') ;;
    *)     echo "unknown mode '$mode' (expected: std | nostd)" >&2; exit 1 ;;
esac

sysroot_dir="${SYSROOT:-target/libs-sysroot}"
sysroot_lib="$sysroot_dir/lib/rustlib/wasm32-unknown-unknown/lib"
[ -d "$sysroot_lib" ] || {
    echo "missing $sysroot_lib — run \`make libs-sysroot\` first." >&2
    exit 1
}

out="${2:-target/libs-vir/$mode}"
rm -rf "$out"
mkdir -p "$out"

# Always use the patched stage1 rustc — never inherit cargo's RUSTC,
# which on build.rs invocations is set to rustup's rustc and would
# (1) skew SVHs vs. what `rustc-in-wasm` later sees and (2) point
# `--print sysroot` at rustup, so DYLD_FALLBACK_LIBRARY_PATH below
# would miss rust_verify's `librustc_driver-*.dylib` (which lives under
# host-rust's rustlib).
RUSTC="$repo/target/host-rust/bin/rustc"
[ -x "$RUSTC" ] || {
    echo "missing $RUSTC — run \`make host-rust\` first." >&2
    exit 1
}
export RUSTC_BOOTSTRAP=1

# Rewrite `$repo/third_party/verus/source/` → `` so the spans baked
# into vstd.vir end up as `vstd/seq.rs` rather than
# `$repo/third_party/verus/source/vstd/seq.rs`.
remap="--remap-path-prefix=$repo/third_party/verus/source/="

rust_verify="$repo/target/host-verus/release/rust_verify"
macros="$sysroot_lib/libverus_builtin_macros.rmeta"
sm_macros="$sysroot_lib/libverus_state_machines_macros.rmeta"
[ -e "$rust_verify" ] || {
    echo "missing host artifact: $rust_verify — run \`make host-verus\` first." >&2
    exit 1
}
[ -e "$macros" ] || {
    echo "missing $macros — build-libs-sysroot.sh should have staged it." >&2
    exit 1
}

# rust_verify's host rustc loads librustc_driver.dylib at launch; DYLD
# fallback exposes the toolchain's sysroot/lib without fighting SIP.
# The rustc_private dylib that rust_verify actually links against
# lives under the host-triple rustlib path (dist rustc-dev places it
# there), while the rustup-shipped variant sits directly in
# sysroot/lib — include both.
host_triple="$("$RUSTC" -vV | awk '/^host:/ {print $2}')"
sysroot="$("$RUSTC" --print sysroot)"
dyld_lib="$sysroot/lib:$sysroot/lib/rustlib/$host_triple/lib"

# vstd via host rust_verify. --sysroot=$sysroot_dir resolves core/
# alloc/std/verus_builtin/macro-stubs against the rmetas
# build-libs-sysroot.sh staged. --is-vstd + VSTD_KIND=IsVstd flip
# proc-macros into "we are vstd" mode. --compile emits rmeta;
# --no-verify / --no-lifetime skip SMT + lifetime passes (we only
# need type info + VIR — the in-wasm verifier never re-checks vstd's
# bodies). `$extra_cfg` selects the std / nostd feature set above.
# Silent invocation — `set -x` would dump the 20+-argument rust_verify
# command line on every build, which clutters dev-iteration output.
# Errors still surface via rust_verify's own stderr.
echo "building libvstd.rmeta + vstd.vir ($mode) via rust_verify..."
DYLD_FALLBACK_LIBRARY_PATH="$dyld_lib" \
LD_LIBRARY_PATH="$dyld_lib" \
RUST_MIN_STACK=$((10 * 1024 * 1024)) \
VSTD_KIND=IsVstd \
    "$rust_verify" \
    --internal-test-mode \
    --target=wasm32-unknown-unknown \
    --emit=metadata \
    --sysroot="$sysroot_dir" \
    --extern=verus_builtin="$sysroot_lib/libverus_builtin.rmeta" \
    --extern=verus_builtin_macros="$macros" \
    --extern=verus_state_machines_macros="$sm_macros" \
    --crate-type=lib \
    --out-dir "$out" \
    --export "$out/vstd.vir" \
    --multiple-errors 2 \
    --is-vstd \
    --compile \
    --no-verify \
    --no-lifetime \
    "${extra_cfg[@]}" \
    "$remap" \
    "$repo/third_party/verus/source/vstd/vstd.rs"
