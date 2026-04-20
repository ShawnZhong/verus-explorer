#!/usr/bin/env bash
set -euxo pipefail

git submodule update --init --recursive

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
source "${HOME}/.cargo/env"
cargo install wasm-pack

# `wasm-pack test --node` (tests/smoke.rs) drives the pipeline under Node so
# it can read the staged sysroot off disk via the `fs` module. We install the
# official tarball straight into `third_party/node/` (gitignored) so the
# Makefile's `test` target can prepend `third_party/node/bin` to PATH and
# pin the Node version — `wasm-bindgen-test-runner` invokes Node via plain
# `Command::new("node")` (pure PATH lookup, no override env var). No version
# constraint on Node itself; `readFileSync`/`readdirSync` have existed since
# Node 0.x.
if [ ! -x third_party/node/bin/node ]; then
    NODE_VERSION="v24.15.0"
    case "$(uname -s)-$(uname -m)" in
        Darwin-arm64) NODE_ARCH="darwin-arm64" ;;
        Darwin-x86_64) NODE_ARCH="darwin-x64" ;;
        Linux-x86_64) NODE_ARCH="linux-x64" ;;
        Linux-aarch64) NODE_ARCH="linux-arm64" ;;
        *) echo "unsupported host $(uname -s)-$(uname -m) for vendored Node" >&2; exit 1 ;;
    esac
    NODE_DIR="node-${NODE_VERSION}-${NODE_ARCH}"
    curl -fsSL "https://nodejs.org/dist/${NODE_VERSION}/${NODE_DIR}.tar.gz" \
      | tar xz -C third_party
    rm -rf third_party/node
    mv "third_party/${NODE_DIR}" third_party/node
fi

EMSDK_VERSION="3.1.74"
third_party/emsdk/emsdk install "${EMSDK_VERSION}"
third_party/emsdk/emsdk activate "${EMSDK_VERSION}"