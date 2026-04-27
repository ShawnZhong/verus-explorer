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
LIBS_VIR=target/libs-vir
VERIFY_SYSROOT=target/libs-sysroot-verify/lib/rustlib/wasm32-unknown-unknown/lib
EXEC_SYSROOT=target/libs-sysroot-execute/lib/rustlib/wasm32-unknown-unknown/lib

# Layout produced (one subdir per `(mode, std-flag)` combination —
# JS picks based on `?std=1` + the mode radio):
#
#   target/libs/
#     nostd/  — verify, no_std       — lean rmetas + alloc-flavor vstd
#     std/    — verify, with libstd  — lean rmetas + std-flavor vstd
#     exec/   — execute, with libstd — MIR-encoded rmetas + libpanic_abort
#                                    + std-flavor vstd (no MIR)
#
# `nostd` and `std` share a verify-flavor sysroot (no MIR). `exec`
# uses the execute-flavor sysroot (MIR-encoded). `exec` ships vstd
# too — `use vstd::prelude::*` and `verus!{}` need to type-check /
# macro-expand even when run-mode code never calls a vstd item at
# runtime. vstd itself is built without MIR (rust_verify doesn't
# thread `-Zalways-encode-mir` today), so a Miri call into a vstd
# fn would fail — rare for run-mode programs.
#
# Two `x.py check` invocations + two vstd builds. Cold rebuild ~5 min
# more than single-flavor; incremental near-zero (x.py's own caches
# subset by RUSTFLAGS). Worth it: verify-only users (the majority)
# save ~22 MB gzipped on cold load by not carrying execute-flavor MIR.
./scripts/build-libs-sysroot.sh
./scripts/build-libs-sysroot.sh --mir
SYSROOT=target/libs-sysroot-verify  ./scripts/build-libs-vir.sh std
SYSROOT=target/libs-sysroot-verify  ./scripts/build-libs-vir.sh nostd
# Execute-flavor vstd: built against the execute sysroot so libstd
# SVHs line up with the rest of the exec/ bundle. Output dir is
# `target/libs-vir/exec` (3rd arg to build-libs-vir.sh); without it
# we'd clobber the verify-flavor `target/libs-vir/std/`.
SYSROOT=target/libs-sysroot-execute ./scripts/build-libs-vir.sh std target/libs-vir/exec

# `verus_*` rmetas (verus_builtin + the two macro stubs) plus the
# nostd dep graph (libcore / liballoc / libcompiler_builtins) — needed
# by every variant.
NOSTD_BASE=(
    libcore.rmeta
    liballoc.rmeta
    libcompiler_builtins.rmeta
    libverus_builtin.rmeta
    libverus_builtin_macros.rmeta
    libverus_state_machines_macros.rmeta
)
# libstd plus the wasm32 dep chain rustc's crate locator eagerly walks
# when libstd is present.
STD_EXTRAS=(
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
# Execute mode adds the panic runtime — `--crate-type=bin` requires
# one even at `--emit=metadata`, and wasm32's default is `panic_abort`.
EXEC_EXTRAS=("${STD_EXTRAS[@]}" libpanic_abort.rmeta)

# `rm -rf` keeps the directory clean: if a name disappears from
# the lists above, the old `.rmeta(.gz)` sibling doesn't linger in
# dist/ and confuse the rustc-in-wasm crate locator.
rm -rf "$OUT"
mkdir -p "$OUT/nostd" "$OUT/std" "$OUT/exec"

# nostd: verify sysroot, alloc-flavor vstd.
for f in "${NOSTD_BASE[@]}"; do cp -l "$VERIFY_SYSROOT/$f" "$OUT/nostd/$f"; done
cp "$LIBS_VIR/nostd/libvstd.rmeta" "$LIBS_VIR/nostd/vstd.vir" "$OUT/nostd/"

# std (verify): verify sysroot, std-flavor vstd.
for f in "${NOSTD_BASE[@]}";  do cp -l "$VERIFY_SYSROOT/$f" "$OUT/std/$f";   done
for f in "${STD_EXTRAS[@]}"; do cp -l "$VERIFY_SYSROOT/$f" "$OUT/std/$f";   done
cp "$LIBS_VIR/std/libvstd.rmeta"   "$LIBS_VIR/std/vstd.vir"   "$OUT/std/"

# exec: execute sysroot (MIR-encoded everything) + execute-flavor vstd.
for f in "${NOSTD_BASE[@]}"; do cp -l "$EXEC_SYSROOT/$f" "$OUT/exec/$f"; done
for f in "${EXEC_EXTRAS[@]}"; do cp -l "$EXEC_SYSROOT/$f" "$OUT/exec/$f"; done
cp "$LIBS_VIR/exec/libvstd.rmeta"  "$LIBS_VIR/exec/vstd.vir"  "$OUT/exec/"

gzip -kf9 "$OUT/nostd"/*.rmeta "$OUT/nostd/vstd.vir" \
          "$OUT/std"/*.rmeta   "$OUT/std/vstd.vir" \
          "$OUT/exec"/*.rmeta  "$OUT/exec/vstd.vir"
