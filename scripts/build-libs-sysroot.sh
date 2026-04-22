#!/usr/bin/env bash
# Assemble the wasm32 sysroot content of target/libs/lib/rustlib/
# wasm32-unknown-unknown/lib/ — every rmeta except libvstd.rmeta +
# vstd.vir. That split sits at an incremental-cost boundary:
#   * this script is heavy (runs `x.py check` + builds verus_builtin) but
#     its inputs — std source + Verus' verus_builtin crate — almost never
#     change, so make targets invoke it once per `make host-rust` cycle;
#   * the sibling `build-libs-vir.sh` script is cheap (one rust_verify
#     run) and its input — the `vstd` source — changes frequently during
#     Verus hacking, so verus-explorer/build.rs reruns it each iteration.
#
# Needs a clean shell env because `x.py check` shells out to bootstrap
# tools (serde, termcolor, generic-array, …) that inherit RUSTFLAGS/CC
# from the caller. When invoked under cargo's build.rs these flags
# include target-specific ones like `-zstack-size=8388608` that macOS
# clang rejects, so this script must be run from `make`, never from a
# cargo build script.
#
# Produces, in order:
#   1. core + alloc + std (+ their wasm32 dep graph: libc, dlmalloc,
#      hashbrown, unwind, panic_{abort,unwind}, std_detect,
#      rustc_demangle, cfg_if, compiler_builtins) — the check-only
#      rmeta flavor from `x.py check`. Lacks the non-const-fn MIR that
#      x.py's full build bakes in for downstream codegen, so ~5.8 MB
#      gzipped slimmer and safe because rustc-in-wasm never codegens
#      user code.
#   2. verus_builtin — registers the `#[rustc_diagnostic_item = ...]`
#      names Verus looks up.
#   3. verus_builtin_macros, verus_state_machines_macros — `--cfg=
#      stub_only` rmetas exposing only the `pub macro NAME` decl_macro
#      shims, enough for vstd's name resolution. Their full crates
#      (with the `MACROS` descriptor slices) get built separately by
#      cargo for the host (rust_verify) and the explorer's wasm binary.
#
# `rustc --emit=metadata` without `-Cextra-filename` produces plain
# `lib<crate>.rmeta` files (no hash suffix), so the `--extern=...` paths
# below are stable and need no globbing. The in-wasm crate locator
# accepts both hashed (`lib<name>-<hash>.rmeta`) and unhashed names.
#
# $out is both the write destination and the `--sysroot=` value passed
# to subsequent rustc/rust_verify invocations; rmetas land in
# $out/lib/rustlib/wasm32-unknown-unknown/lib/. Default is
# target/libs-sysroot/ — build-libs-vir.sh reads from here as its
# sysroot and copies the contents into target/libs/ alongside the
# vstd rmeta + vir it produces.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
repo="$PWD"

out="${1:-target/libs-sysroot}"
lib="$out/lib/rustlib/wasm32-unknown-unknown/lib"

# Wipe any stale artifacts — otherwise a renamed or removed crate would
# linger as a duplicate (or SVH-mismatched) rmeta.
rm -rf "$out"
mkdir -p "$lib"

RUSTC="$repo/target/host-rust/bin/rustc"
[ -x "$RUSTC" ] || {
    echo "missing $RUSTC — run \`make host-rust\` first." >&2
    exit 1
}
export RUSTC_BOOTSTRAP=1

HOST_TRIPLE=$("$RUSTC" -vV | awk '/^host:/ {print $2}')

# Step 1: run `x.py check` against the already-built stage1 compiler.
# Emits metadata-only rmetas (no non-const-fn MIR) into
# third_party/rust/build/<host>/stage1-std/wasm32-unknown-unknown/
# release/deps/, distinguished from `x.py build`'s rlib+rmeta pairs by
# SVH hash. Incremental — near-no-op on re-run.
set -x
(cd "$repo/third_party/rust" && ./x.py check --stage 1 library --target wasm32-unknown-unknown)
{ set +x; } 2>/dev/null

stage1_std_deps="$repo/third_party/rust/build/$HOST_TRIPLE/stage1-std/wasm32-unknown-unknown/release/deps"
[ -d "$stage1_std_deps" ] || {
    echo "missing $stage1_std_deps — x.py check should have populated it." >&2
    exit 1
}

# Copy check-only rmetas (rmeta without a matching rlib sibling) into
# the sysroot, unhashing filenames on the way. libstd's dep list is
# eagerly loaded by rustc, so we take every check rmeta rather than
# cherry-picking — libtest, libgetopts, librustc_std_workspace_*,
# libproc_macro, etc. get staged here even if public/index.html never
# fetches them; they don't cost anything in $out but prevent E0463.
# Loop body runs without `set -x` tracing — echoing every cp is noisy.
count=0
for src in "$stage1_std_deps"/*.rmeta; do
    rlib="${src%.rmeta}.rlib"
    [ -f "$rlib" ] && continue  # skip build-set (those are in target/host-rust/)
    base=$(basename "$src")
    name=$(echo "$base" | sed -E 's/-[0-9a-f]{16}\.rmeta$/.rmeta/')
    cp "$src" "$lib/$name"
    count=$((count + 1))
done
echo "staged $count check-only rmetas into $lib/"
set -x

# Rewrite `$repo/third_party/verus/source/` → `` in every rustc
# invocation so the spans baked into rmetas end up as `vstd/seq.rs`
# rather than `$repo/third_party/verus/source/vstd/seq.rs` — no
# developer absolute path, no vendored-submodule noise. rustc applies
# this to all `RemapPathScopeComponents` by default, which includes
# DIAGNOSTICS, the scope `span_to_diagnostic_string` reads when
# producing the `span.as_string` field Verus serializes into the VIR.
remap="--remap-path-prefix=$repo/third_party/verus/source/="

# Step 2: verus_builtin. cfg-gated behind `verus_keep_ghost`; feature
# gates come from `#![cfg_attr(verus_keep_ghost, feature(...))]`.
"$RUSTC" --edition=2018 --crate-type=lib --crate-name=verus_builtin \
    --target=wasm32-unknown-unknown --emit=metadata \
    --cfg=verus_keep_ghost \
    --sysroot="$out" \
    "$remap" \
    --out-dir "$lib" \
    "$repo/third_party/verus/source/builtin/src/lib.rs"

# Step 3: stubs-only macro rmetas (`--cfg=stub_only` cfg-gates out the
# proc_macro/syn/quote-using impl fns + `MACROS` slice — see each
# crate's lib.rs header). Each is a wasm32 rmeta exposing only the
# `pub macro NAME` decl_macro stubs, exactly what vstd's build
# (`--extern=...` in build-libs-vir.sh) needs for name resolution.
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
