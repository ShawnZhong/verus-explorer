#!/usr/bin/env bash
# Build Z3 → build/z3.{js,wasm}.
# Stage 1: CMake + build libz3.a (slow, ~5min; skipped if already built).
# Stage 2: emcc links libz3.a into build/z3.{js,wasm} (fast, ~30s).
# Idempotent. `rm -rf build` to force a full rebuild of stage 1.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

source third_party/emsdk/emsdk_env.sh >/dev/null

set -x

emcmake cmake -S third_party/z3 -B build \
  -DZ3_BUILD_LIBZ3_SHARED=OFF \
  -DZ3_SINGLE_THREADED=ON \
  -DZ3_POLLING_TIMER=ON \
  -DZ3_BUILD_EXECUTABLE=OFF \
  -DZ3_BUILD_TEST_EXECUTABLES=OFF \
  -DZ3_ENABLE_EXAMPLE_TARGETS=OFF \
  -DZ3_INCLUDE_GIT_HASH=OFF \
  -DZ3_INCLUDE_GIT_DESCRIBE=OFF \
  -DCMAKE_C_FLAGS="-fwasm-exceptions -flto" \
  -DCMAKE_CXX_FLAGS="-fwasm-exceptions -flto" \
  -DCMAKE_BUILD_TYPE=Release
cmake --build build -j"$(nproc)"

emcc -x c /dev/null build/libz3.a \
    -fwasm-exceptions \
    -flto \
    -Oz \
    -s WASM_BIGINT \
    -s ENVIRONMENT=web \
    -s MODULARIZE=1 \
    -s EXPORT_NAME=initZ3 \
    -s EXPORTED_FUNCTIONS='["_Z3_mk_config","_Z3_mk_context","_Z3_del_config","_Z3_eval_smtlib2_string","_Z3_del_context"]' \
    -s EXPORTED_RUNTIME_METHODS='["cwrap"]' \
    -s FILESYSTEM=0 \
    -s INITIAL_MEMORY=64MB \
    -s ALLOW_MEMORY_GROWTH=1 \
    -s TOTAL_STACK=16MB \
    -o build/z3.js
