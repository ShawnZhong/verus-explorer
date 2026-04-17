#!/usr/bin/env bash
set -euxo pipefail

git submodule update --init --recursive

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
source "${HOME}/.cargo/env"
cargo install wasm-pack

EMSDK_VERSION="3.1.74"
third_party/emsdk/emsdk install "${EMSDK_VERSION}"
third_party/emsdk/emsdk activate "${EMSDK_VERSION}"