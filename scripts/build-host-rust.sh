#!/usr/bin/env bash
# Build the patched host rustc and stage it as a self-contained sysroot at
# target/host-rust/. Required because rust_verify links against patched
# `rustc_metadata::proc_macro_registry` symbols that don't exist in the
# rustup-shipped stable toolchain. Other entry points (Makefile + scripts)
# inject `RUSTC=target/host-rust/bin/rustc` so cargo uses this rustc — no
# `rustup toolchain link` or `rustup override` involvement.
#
# Three steps in one shot:
#   1. ./x.py build --stage 1 library --target <host>,wasm32-unknown-unknown
#   2. ./x.py dist rustc-dev --stage 1
#   3. stage build/host/stage1 + raw stage1-std into target/host-rust/.
#      Required because steps 1 and 2 mutually wipe each other's outputs in
#      build/host/stage1/lib (library --target wasm32 regenerates the sysroot
#      fresh and drops rustc-dev libs; dist rustc-dev drops the wasm32 sysroot
#      the other way). The wasm32 std survives in the raw build/<host>/
#      stage1-std dir, which neither command touches.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
repo="$PWD"

# Host triple from rustc; stable channel from rust-toolchain.toml. We borrow
# cargo/rust-lld/llvm-strip/wasm-component-ld from the rustup-shipped stable
# toolchain — stage1 doesn't build its own copies, and the protocol versions
# need to match stage1 rustc's source version.
HOST_TRIPLE=$(rustc -vV | awk '/^host:/ {print $2}')
STABLE_CHANNEL=$(awk -F'"' '/^channel/ {print $2}' rust-toolchain.toml)
RUSTUP_STABLE="$HOME/.rustup/toolchains/$STABLE_CHANNEL-$HOST_TRIPLE"
[ -d "$RUSTUP_STABLE" ] || {
    echo "missing rustup toolchain $STABLE_CHANNEL — run \`rustup toolchain install $STABLE_CHANNEL\`." >&2
    exit 1
}

set -x
cd "$repo/third_party/rust"
./x.py build --stage 1 library --target "$HOST_TRIPLE,wasm32-unknown-unknown"
./x.py dist rustc-dev --stage 1
{ set +x; } 2>/dev/null

RUST_BUILD="$repo/third_party/rust/build/host/stage1"
RAW_STD="$repo/third_party/rust/build/$HOST_TRIPLE/stage1-std"
SNAP="$repo/target/host-rust"

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

echo "staged rustc at $SNAP"
echo "host lib: $(ls "$SNAP/lib/rustlib/$HOST_TRIPLE/lib/" | wc -l | tr -d ' ') files"
echo "wasm32 lib: $(ls "$SNAP/lib/rustlib/wasm32-unknown-unknown/lib/" | wc -l | tr -d ' ') files"
