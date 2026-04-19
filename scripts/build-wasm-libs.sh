#!/usr/bin/env bash
# Build wasm32 rmetas into $out, laid out as a virtual sysroot so rustc-
# in-wasm's crate locator (which hard-codes `<sysroot>/lib/rustlib/
# <triple>/lib/`) resolves `extern crate` at runtime. Produces, in order:
#   core + compiler_builtins + alloc — self-built from the rust-src component.
#     Shaves ~15 MB vs rustup's prebuilt rmetas (which carry
#     `-Z always-encode-mir=yes` we don't need since we never codegen).
#   verus_builtin — registers the `#[rustc_diagnostic_item = ...]` names
#     Verus looks up.
#   vstd.rmeta + vstd.vir — vstd compiled via the host rust_verify driver
#     against our self-built sysroot (SVH chain matches user-code rustc-
#     in-wasm's lookups).
# `-Cextra-filename=-explorer` gives every rmeta a predictable filename
# matching the names the in-wasm crate locator probes for.
#
# $out is both the write destination and the `--sysroot=` value passed to
# subsequent rustc/rust_verify invocations; rmetas land in
# $out/lib/rustlib/wasm32-unknown-unknown/lib/. build.rs passes an
# OUT_DIR-scoped path; the default below is for manual runs.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
repo="$PWD"

out="${1:-target/wasm-libs}"
lib="$out/lib/rustlib/wasm32-unknown-unknown/lib"
mkdir -p "$lib"

RUSTC="${RUSTC:-rustc}"
export RUSTC_BOOTSTRAP=1

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
    -Cextra-filename=-explorer \
    --out-dir "$lib" \
    "$rust_src/core/src/lib.rs"

"$RUSTC" --edition=2024 --crate-type=lib --crate-name=compiler_builtins \
    --target=wasm32-unknown-unknown --emit=metadata \
    -Cextra-filename=-explorer \
    --cfg='feature="compiler-builtins"' \
    --check-cfg='cfg(feature, values("compiler-builtins"))' \
    --cap-lints=allow \
    --sysroot="$out" \
    --out-dir "$lib" \
    "$rust_src/compiler-builtins/compiler-builtins/src/lib.rs"

"$RUSTC" --edition=2024 --crate-type=lib --crate-name=alloc \
    --target=wasm32-unknown-unknown --emit=metadata \
    -Cextra-filename=-explorer \
    --sysroot="$out" \
    --out-dir "$lib" \
    "$rust_src/alloc/src/lib.rs"

# verus_builtin cfg-gated behind `verus_keep_ghost`; feature gates come
# from `#![cfg_attr(verus_keep_ghost, feature(...))]`.
"$RUSTC" --edition=2018 --crate-type=lib --crate-name=verus_builtin \
    --target=wasm32-unknown-unknown --emit=metadata \
    -Cextra-filename=-explorer \
    --cfg=verus_keep_ghost \
    --sysroot="$out" \
    --out-dir "$lib" \
    "$repo/third_party/verus/source/builtin/src/lib.rs"

# verus_builtin_macros, stubs-only mode (`--cfg=stub_only` cfg-gates out
# the proc_macro/syn/quote-using impl fns + `MACROS` slice — see the file
# header). Result is a wasm32 rmeta exposing only the `pub macro NAME`
# decl_macro stubs, exactly what vstd's build (`--extern=verus_builtin_macros`
# below) and the bundled sysroot need for name resolution. The full crate
# (with `MACROS`) gets built separately by cargo for both the host
# (rust_verify) and the explorer's wasm binary (registered at startup via
# `proc_macros::install`); this rmeta isn't used by either.
"$RUSTC" --edition=2018 --crate-type=lib --crate-name=verus_builtin_macros \
    --target=wasm32-unknown-unknown --emit=metadata \
    -Cextra-filename=-explorer \
    --cfg=stub_only \
    --check-cfg='cfg(stub_only)' \
    --check-cfg='cfg(verus_keep_ghost)' \
    --sysroot="$out" \
    --out-dir "$lib" \
    "$repo/third_party/verus/source/builtin_macros/src/lib.rs"

# vstd via host rust_verify. --sysroot=$out resolves core/alloc/
# compiler_builtins against our self-built rmetas (matching SVH with
# what user-code rustc-in-wasm later sees). --is-vstd + VSTD_KIND=IsVstd
# flip proc-macros into "we are vstd" mode. --compile emits rmeta;
# --no-verify / --no-lifetime skip SMT + lifetime passes (we only need
# type info + VIR — the in-wasm verifier never re-checks vstd's bodies).
# `feature="alloc"` (not "std") because the embedded sysroot bundles
# only core + alloc.
{ set +x; } 2>/dev/null
host_dir="$repo/target/verus-host/release"
case "$(uname -s)" in
    Darwin) dylib_ext=dylib ;;
    Linux) dylib_ext=so ;;
    *) dylib_ext=dll ;;
esac
rust_verify="$host_dir/rust_verify"
# verus_builtin_macros's wasm32 stub rmeta was built directly above (lives in
# $lib next to verus_builtin); vstd's --extern points at that.
# verus_state_machines_macros is still a real proc-macro dylib — its host
# build is what we --extern (rustc accepts host-triple proc-macro dylibs as
# `--extern` targets even for cross-compilation, since proc-macros run in
# the compiler).
macros="$lib/libverus_builtin_macros-explorer.rmeta"
sm_macros="$host_dir/libverus_state_machines_macros.$dylib_ext"
for f in "$rust_verify" "$sm_macros"; do
    [ -e "$f" ] || {
        echo "missing host artifact: $f — run \`make verus-host\` first." >&2
        exit 1
    }
done

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
