#!/usr/bin/env bash
# One-time setup for verus-explorer.
#
# Installs the wasm32 rust target, clones emsdk and Z3 into third_party/,
# and installs+activates the pinned emsdk version. Idempotent — safe to
# re-run. Run this once after a fresh checkout; the Makefile drives
# everything else.

set -euo pipefail

PROJ_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
THIRD_PARTY_DIR="${PROJ_ROOT}/third_party"
EMSDK_DIR="${THIRD_PARTY_DIR}/emsdk"
EMSDK_VERSION="3.1.74"
Z3_DIR="${THIRD_PARTY_DIR}/z3"
Z3_TAG="z3-4.16.0"
VERUS_DIR="${THIRD_PARTY_DIR}/verus"
PATCH_FILE="${PROJ_ROOT}/patches/air-wasm.patch"

echo "--- adding wasm32-unknown-unknown rust target"
rustup target add wasm32-unknown-unknown

echo "--- syncing verus submodule"
git -C "${PROJ_ROOT}" submodule update --init --recursive third_party/verus

# Apply the air wasm32 shim patch idempotently. `git apply --reverse --check`
# returns 0 iff the patch is already applied; in that case we skip.
if git -C "${VERUS_DIR}" apply --reverse --check "${PATCH_FILE}" 2>/dev/null; then
    echo "--- air wasm32 patch already applied"
else
    echo "--- applying air wasm32 patch"
    git -C "${VERUS_DIR}" apply "${PATCH_FILE}"
fi

mkdir -p "${THIRD_PARTY_DIR}"

if [[ ! -d "${EMSDK_DIR}" ]]; then
    echo "--- cloning emsdk"
    git clone --depth 1 https://github.com/emscripten-core/emsdk.git "${EMSDK_DIR}"
fi
echo "--- installing+activating emsdk ${EMSDK_VERSION}"
"${EMSDK_DIR}/emsdk" install "${EMSDK_VERSION}"
"${EMSDK_DIR}/emsdk" activate "${EMSDK_VERSION}"

if [[ ! -d "${Z3_DIR}" ]]; then
    echo "--- cloning Z3 ${Z3_TAG}"
    git clone --depth 1 --branch "${Z3_TAG}" https://github.com/Z3Prover/z3.git "${Z3_DIR}"
fi

echo "--- setup done"
