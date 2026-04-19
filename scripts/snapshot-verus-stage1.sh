#!/usr/bin/env bash
# Snapshot the patched stage1 rustc into a stable rustup-compatible toolchain
# directory. Required because `./x.py build library` and `./x.py dist
# rustc-dev` mutually wipe each other's outputs in build/host/stage1/lib —
# library --target wasm32 erases rustc-dev libs, and dist rustc-dev erases
# the wasm32 sysroot. Snapshot captures both into a side directory rustup
# points at, so subsequent x.py invocations can churn freely.
#
# Run after any combination of:
#   ./x.py build --stage 1 library --target aarch64-apple-darwin,wasm32-unknown-unknown
#   ./x.py dist rustc-dev --stage 1
# (in either order — this script picks up wasm32 from the raw stage1-std
# build dir, which neither wipes.)
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
RUST_BUILD="$repo/third_party/rust/build/host/stage1"
RAW_STD="$repo/third_party/rust/build/aarch64-apple-darwin/stage1-std"
SNAP="$repo/third_party/rust/build/verus-stage1"
RUSTUP_STABLE="$HOME/.rustup/toolchains/1.94.0-aarch64-apple-darwin"
HOST_TRIPLE="aarch64-apple-darwin"

[ -d "$RUST_BUILD" ] || { echo "missing $RUST_BUILD — run x.py first" >&2; exit 1; }

rm -rf "$SNAP"
mkdir -p "$SNAP/bin" \
    "$SNAP/lib/rustlib/$HOST_TRIPLE/lib" \
    "$SNAP/lib/rustlib/$HOST_TRIPLE/bin" \
    "$SNAP/lib/rustlib/wasm32-unknown-unknown/lib"

# rustc binary (hardlink — same filesystem)
ln "$RUST_BUILD/bin/rustc" "$SNAP/bin/rustc"
# cargo from rustup stable (the stage1 build doesn't produce its own cargo)
ln -s "$RUSTUP_STABLE/bin/cargo" "$SNAP/bin/cargo"

# Top-level dylibs: librustc_driver-<rustup-style-hash>.dylib (the dlopen
# variant) + sanitizer rt libs.
for f in "$RUST_BUILD"/lib/*.dylib; do
    [ -e "$f" ] && ln "$f" "$SNAP/lib/$(basename "$f")"
done

# Host sysroot lib/: std + rustc-dev (rustc_driver, rustc_macros, etc.)
# Whatever's there at snapshot time is what we get — caller is responsible
# for running the right x.py invocations first.
for f in "$RUST_BUILD/lib/rustlib/$HOST_TRIPLE/lib/"*; do
    base=$(basename "$f")
    if [ -d "$f" ]; then
        cp -R "$f" "$SNAP/lib/rustlib/$HOST_TRIPLE/lib/"
    else
        ln "$f" "$SNAP/lib/rustlib/$HOST_TRIPLE/lib/$base" 2>/dev/null \
            || cp "$f" "$SNAP/lib/rustlib/$HOST_TRIPLE/lib/$base"
    fi
done

# Host sysroot bin/: llvm-tools (llvm-objcopy, opt, etc.) — these come from
# ci-llvm so they're stable across x.py library rebuilds, but they live
# under stage1's tree so we capture them here.
for f in "$RUST_BUILD/lib/rustlib/$HOST_TRIPLE/bin/"*; do
    [ -e "$f" ] && ln "$f" "$SNAP/lib/rustlib/$HOST_TRIPLE/bin/$(basename "$f")" \
        2>/dev/null || cp "$f" "$SNAP/lib/rustlib/$HOST_TRIPLE/bin/$(basename "$f")"
done

# Tools that stage1 doesn't build but rustc/cargo expect: rust-lld (wasm32
# linker), llvm-strip, wasm-component-ld, gcc-ld wrappers. Borrow from
# rustup stable — these are LLVM tools and don't track rustc version.
for tool in rust-lld llvm-strip wasm-component-ld; do
    src="$RUSTUP_STABLE/lib/rustlib/$HOST_TRIPLE/bin/$tool"
    [ -e "$src" ] && ln "$src" "$SNAP/lib/rustlib/$HOST_TRIPLE/bin/$tool" \
        2>/dev/null || cp "$src" "$SNAP/lib/rustlib/$HOST_TRIPLE/bin/$tool"
done
[ -d "$RUSTUP_STABLE/lib/rustlib/$HOST_TRIPLE/bin/gcc-ld" ] && \
    cp -R "$RUSTUP_STABLE/lib/rustlib/$HOST_TRIPLE/bin/gcc-ld" \
        "$SNAP/lib/rustlib/$HOST_TRIPLE/bin/gcc-ld"

# wasm32 sysroot from the raw build dir — survives x.py dist invocations.
for f in "$RAW_STD/wasm32-unknown-unknown/release/deps/"*; do
    base=$(basename "$f")
    case "$base" in
        *.rlib|*.rmeta)
            ln "$f" "$SNAP/lib/rustlib/wasm32-unknown-unknown/lib/$base" \
                2>/dev/null || cp "$f" "$SNAP/lib/rustlib/wasm32-unknown-unknown/lib/$base"
            ;;
    esac
done

# rustlib/src + rustlib/rustc-src (rustc_private builds want std's source).
[ -d "$RUST_BUILD/lib/rustlib/src" ] && cp -R "$RUST_BUILD/lib/rustlib/src" "$SNAP/lib/rustlib/src"
[ -d "$RUST_BUILD/lib/rustlib/rustc-src" ] && cp -R "$RUST_BUILD/lib/rustlib/rustc-src" "$SNAP/lib/rustlib/rustc-src"

# Re-link rustup so `cargo`/`rustc` in this repo resolve to the snapshot.
rustup toolchain link verus-stage1 "$SNAP" >/dev/null

echo "snapshot built at $SNAP"
echo "host lib: $(ls "$SNAP/lib/rustlib/$HOST_TRIPLE/lib/" | wc -l | tr -d ' ') files"
echo "wasm32 lib: $(ls "$SNAP/lib/rustlib/wasm32-unknown-unknown/lib/" | wc -l | tr -d ' ') files"
