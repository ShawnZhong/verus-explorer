#!/usr/bin/env bash
# Build the host rust_verify driver into target/verus-host/release/.
# build.rs's wasm-libs script invokes rust_verify to compile vstd → wasm32
# rmeta + .vir for the bundled virtual sysroot.
#
# `--target-dir target/verus-host` redirects cargo out of Verus's own
# workspace (`third_party/verus/source/target/`) so `make clean`'s
# `rm -rf target` covers these artifacts too.
#
# `verus_keep_ghost` flips `cfg_erase()` in verus_builtin_macros to its
# smart `expand_expr` variant instead of unconditionally erasing every
# ghost `pub use` — the latter makes vstd typecheck fail with ~85 E0603
# "private import" errors. `--check-cfg` silences "unexpected cfg"
# warnings from deps that don't declare these names.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

set -x

RUSTFLAGS="--cfg=verus_keep_ghost --check-cfg=cfg(verus_keep_ghost) --check-cfg=cfg(verus_keep_ghost_body)" \
  cargo build \
    --manifest-path third_party/verus/source/Cargo.toml \
    --target-dir target/verus-host \
    -p rust_verify \
    -p verus_builtin_macros \
    -p verus_state_machines_macros \
    --release
