#!/usr/bin/env bash
# One-time setup for verus-explorer.
#
# Initialises git submodules under third_party/ (verus, emsdk, z3),
# installs the wasm32 rust target, and installs+activates the pinned
# emsdk version. Idempotent — safe to re-run. Run this once after a
# fresh checkout; the Makefile drives everything else.

set -euo pipefail

PROJ_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EMSDK_DIR="${PROJ_ROOT}/third_party/emsdk"
EMSDK_VERSION="3.1.74"

echo "--- syncing submodules"
git -C "${PROJ_ROOT}" submodule update --init --recursive

echo "--- adding wasm32-unknown-unknown rust target"
rustup target add wasm32-unknown-unknown

echo "--- installing+activating emsdk ${EMSDK_VERSION}"
"${EMSDK_DIR}/emsdk" install "${EMSDK_VERSION}"
"${EMSDK_DIR}/emsdk" activate "${EMSDK_VERSION}"

echo "--- setup done"
