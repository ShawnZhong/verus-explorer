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
#      hashbrown, unwind, panic_{adopt,unwind}, std_detect,
#      rustc_demangle, cfg_if, compiler_builtins) — the check-only
#      rmeta flavor from `x.py check`, but with `-Zalways-encode-mir`
#      so the rmeta carries function MIR in addition to the metadata
#      surface. Miri (linked into the wasm crate) needs MIR for every
#      function the user's program transitively calls — without the
#      flag, `create_ecx` tcx.dcx().fatal()s on the libcore sentinel
#      check (`core::ascii::escape_default`'s MIR availability). The
#      cost is about 5.8 MB gzipped on top of the bare check rmetas.
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

# Two-flavor build:
#   * verify sysroot — small, no MIR. Verify mode never interprets MIR
#     so we ship the lean rmeta-only flavor: ~3 MB gzipped libcore.
#   * execute sysroot — MIR-encoded (`-Zalways-encode-mir`) +
#     `--cfg=verus_explorer` so libstd's wasm32 stdio routes through
#     Miri's `miri_write_to_*` shims. ~13 MB gzipped libcore — Miri
#     needs the MIR to interpret std fns the user's program calls.
#
# Usage: $0 [out-dir] [--mir]
#   out-dir defaults to `target/libs-sysroot-verify` (lean) when
#   `--mir` is not passed, `target/libs-sysroot-execute` when it is.
flavor=verify
extra_rustflags=
out=
for a in "$@"; do
    case "$a" in
        --mir) flavor=execute;
               extra_rustflags="-Zalways-encode-mir --cfg=verus_explorer --check-cfg=cfg(verus_explorer)" ;;
        --*)   echo "unknown flag '$a'" >&2; exit 1 ;;
        *)     out="$a" ;;
    esac
done
: "${out:=target/libs-sysroot-$flavor}"
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
# Emits rmetas into third_party/rust/build/<host>/stage1-std/
# wasm32-unknown-unknown/release/deps/, distinguished from `x.py
# build`'s rlib+rmeta pairs by SVH hash. Incremental — near-no-op on
# re-run.
#
# `RUSTFLAGS_NOT_BOOTSTRAP="-Zalways-encode-mir"` forces non-const-fn
# MIR into the rmetas so Miri (linked into our wasm) can interpret
# libcore/liballoc/libstd. Bootstrap's RUSTFLAGS conventions:
# `RUSTFLAGS_BOOTSTRAP` flows to stage 0 (the bootstrap compiler
# itself); `RUSTFLAGS_NOT_BOOTSTRAP` flows to stage 1+ rustc
# invocations of "normal" target crates — which is exactly the stage1
# libstd build we drive here. Plain `RUSTFLAGS` gets scrubbed by
# bootstrap. Set both to be safe in case the conventions ever flip.
# Wipe x.py's stage1-std incremental cache to force a from-scratch
# rebuild every time. Without this, toggling RUSTFLAGS_NOT_BOOTSTRAP
# (e.g., when adding/removing `-Zalways-encode-mir`) leaves a stale
# libstd compiled against the previous flag set's libcore — vstd
# downstream then trips E0460 "found possibly newer version of crate
# core which std depends on" because the SVHs no longer line up.
# x.py's own incremental tracking doesn't catch this since the flag
# is supplied via env, not via its config.toml.
rm -rf "$repo/third_party/rust/build/$HOST_TRIPLE/stage1-std"

# Execute-flavor extras (set above): `-Zalways-encode-mir` so Miri can
# interpret libcore/liballoc/libstd, plus `--cfg=verus_explorer` which
# our patches in `library/std/src/sys/stdio/unsupported.rs` read to
# route wasm32 `Stdout`/`Stderr` through Miri's `miri_write_to_*`
# shims (forwarded to the `__verus_explorer_stdout/stderr` externs).
# Verify flavor builds without these — leaner rmetas, faster cold load.
set -x
(cd "$repo/third_party/rust" && \
    RUSTFLAGS_BOOTSTRAP="$extra_rustflags" \
    RUSTFLAGS_NOT_BOOTSTRAP="$extra_rustflags" \
    ./x.py check --stage 1 library --target wasm32-unknown-unknown)
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
