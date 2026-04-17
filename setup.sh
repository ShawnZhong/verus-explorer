#!/usr/bin/env bash
# One-time setup for verus-explorer.
#
# Initialises git submodules under third_party/ (verus, emsdk, z3) and
# installs+activates the pinned emsdk version. The wasm32 rust target is
# declared in rust-toolchain.toml, so rustup auto-installs it on first
# build. Idempotent — safe to re-run. Run this once after a fresh
# checkout; the Makefile drives everything else.

set -euo pipefail

PROJ_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EMSDK_DIR="${PROJ_ROOT}/third_party/emsdk"
EMSDK_VERSION="3.1.74"

echo "--- syncing submodules"
git -C "${PROJ_ROOT}" submodule update --init --recursive

echo "--- installing+activating emsdk ${EMSDK_VERSION}"
"${EMSDK_DIR}/emsdk" install "${EMSDK_VERSION}"
"${EMSDK_DIR}/emsdk" activate "${EMSDK_VERSION}"

# dist/ is a git worktree on gh-pages — builds write straight into the tree
# we push to Pages. Idempotent: skip if dist/ is already a valid worktree.
if [[ ! -e "${PROJ_ROOT}/dist/.git" ]]; then
  echo "--- creating dist/ as a gh-pages worktree"
  git -C "${PROJ_ROOT}" worktree prune
  if git -C "${PROJ_ROOT}" fetch origin gh-pages 2>/dev/null; then
    git -C "${PROJ_ROOT}" worktree add -B gh-pages dist origin/gh-pages
  else
    git -C "${PROJ_ROOT}" worktree add --orphan -b gh-pages dist
  fi
fi

echo "--- setup done"
