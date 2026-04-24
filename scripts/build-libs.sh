#!/usr/bin/env bash
# Assemble the browser-shipped bundle at target/libs/. Called by the
# `libs` target in the Makefile after the sysroot (build-libs-sysroot.sh)
# and per-mode vstd builds (build-libs-vir.sh) have staged their inputs.
#
# Layout produced:
#   target/libs/
#     lib<shared>.rmeta[.gz]    # needed by both std and nostd modes
#     std/lib<std-only>.rmeta[.gz] + libvstd.rmeta[.gz] + vstd.vir[.gz]
#     nostd/libvstd.rmeta[.gz] + vstd.vir[.gz]
# Shared files sit at the root so they're fetched once regardless of
# mode; the mode-specific subdirs hold the pieces that differ. libstd
# and its wasm32 dep chain live under std/ because only std mode loads
# them (rustc's crate locator walks libstd's declared deps eagerly, so
# they're eager; nostd mode doesn't pull libstd at all).
#
# `cp -l` hardlinks (sysroot-built rmetas are immutable, so the link
# is safe and costs zero disk). `gzip -kf9` keeps the raw files for
# tests/smoke.rs, overwrites stale `.gz` siblings, picks the smallest
# output.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

OUT=target/libs
SYSROOT_LIB=target/libs-sysroot/lib/rustlib/wasm32-unknown-unknown/lib
LIBS_VIR=target/libs-vir

# Full libs pipeline in source order — Make only invokes this script
# when an upstream dep moved, so unconditionally re-running each stage
# here is correct. The stages themselves are internally incremental
# where they can be (`x.py check` in build-libs-sysroot.sh), but the
# top-level `rm -rf + rebuild` they each do is fine because Make's
# dep tracking already gates this script on changes worth rebuilding
# for.
./scripts/build-libs-sysroot.sh
./scripts/build-libs-vir.sh std
./scripts/build-libs-vir.sh nostd

# Needed by both modes. libcore + liballoc + libcompiler_builtins cover
# the no_std dependency graph; the verus_* rmetas are the Verus runtime
# shims (verus_builtin) and decl_macro stubs (the two `*_macros` ones)
# that every vstd / user build needs for name resolution.
SHARED=(
    libcore.rmeta
    liballoc.rmeta
    libcompiler_builtins.rmeta
    libverus_builtin.rmeta
    libverus_builtin_macros.rmeta
    libverus_state_machines_macros.rmeta
)

# libstd plus the wasm32 dep chain rustc's crate locator eagerly walks
# when libstd is present (std-mode only).
STD_ONLY=(
    libstd.rmeta
    libcfg_if.rmeta
    libdlmalloc.rmeta
    libhashbrown.rmeta
    liblibc.rmeta
    librustc_demangle.rmeta
    librustc_std_workspace_alloc.rmeta
    librustc_std_workspace_core.rmeta
    libstd_detect.rmeta
    libunwind.rmeta
)

# `rm -rf` keeps the directory clean: if a name disappears from
# `SHARED` / `STD_ONLY` above, the old `.rmeta(.gz)` sibling doesn't
# linger in dist/ and confuse the rustc-in-wasm crate locator.
rm -rf "$OUT"
mkdir -p "$OUT/std" "$OUT/nostd"

for f in "${SHARED[@]}";   do cp -l "$SYSROOT_LIB/$f" "$OUT/$f";      done
for f in "${STD_ONLY[@]}"; do cp -l "$SYSROOT_LIB/$f" "$OUT/std/$f";  done

cp "$LIBS_VIR/std/libvstd.rmeta"   "$LIBS_VIR/std/vstd.vir"   "$OUT/std/"
cp "$LIBS_VIR/nostd/libvstd.rmeta" "$LIBS_VIR/nostd/vstd.vir" "$OUT/nostd/"

gzip -kf9 "$OUT"/*.rmeta \
          "$OUT/std"/*.rmeta "$OUT/std/vstd.vir" \
          "$OUT/nostd"/*.rmeta "$OUT/nostd/vstd.vir"
