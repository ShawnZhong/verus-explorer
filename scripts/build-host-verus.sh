#!/usr/bin/env bash
# Build the host rust_verify driver into target/host-verus/release/.
# build-wasm-libs.sh invokes rust_verify to compile vstd → wasm32 rmeta +
# .vir for the bundled virtual sysroot.
#
# `--target-dir target/host-verus` redirects cargo out of Verus's own
# workspace (`third_party/verus/source/target/`) so it sits alongside the
# other consolidated build dirs (host-rust, wasm-z3, cargo) under target/.
#
# `RUSTC` points at the patched stage1 rustc staged by build-host-rust.sh.
#
# `verus_keep_ghost` flips `cfg_erase()` in verus_builtin_macros to its
# smart `expand_expr` variant instead of unconditionally erasing every
# ghost `pub use` — the latter makes vstd typecheck fail with ~85 E0603
# "private import" errors. `--check-cfg` silences "unexpected cfg"
# warnings from deps that don't declare these names.
#
# An env-var RUSTFLAGS overrides the `[build].rustflags` from our
# `.cargo/config.toml` (cargo picks one source, not both), which would drop
# the repo-wide `--allow=unexpected_cfgs` and flood the log with
# `#[cfg(bootstrap)]` warnings from the vendored rustc tree. Repeat the
# allow here so the suppression sticks for this cargo invocation too.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

export RUSTC="$PWD/target/host-rust/bin/rustc"
[ -x "$RUSTC" ] || {
    echo "missing $RUSTC — run \`make host-rust\` first." >&2
    exit 1
}

set -x

RUSTFLAGS="--allow=unexpected_cfgs --cfg=verus_keep_ghost --check-cfg=cfg(verus_keep_ghost) --check-cfg=cfg(verus_keep_ghost_body)" \
  cargo build \
    --manifest-path third_party/verus/source/Cargo.toml \
    --target-dir target/host-verus \
    -p rust_verify \
    -p verus_builtin_macros \
    -p verus_state_machines_macros \
    --release
