#!/usr/bin/env bash
set -euxo pipefail

git submodule update --init --recursive

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
source "${HOME}/.cargo/env"
cargo install wasm-pack

# `wasm-pack test --node` (tests/smoke.rs) drives the pipeline under Node so
# it can read the staged sysroot off disk via the `fs` module. No version
# constraint — `readFileSync`/`readdirSync` exist since Node 0.x.
command -v node >/dev/null 2>&1 || brew install node

EMSDK_VERSION="3.1.74"
third_party/emsdk/emsdk install "${EMSDK_VERSION}"
third_party/emsdk/emsdk activate "${EMSDK_VERSION}"