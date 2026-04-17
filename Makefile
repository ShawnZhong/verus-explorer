# verus-explorer convenience targets.
#
# Usage:
#   make          # dev build into dist/ (fast, skips wasm-opt)
#   make release  # optimized build into dist/
#   make serve    # dev build + serve dist/ on :8000
#   make clean    # remove build artifacts (keeps third_party/)

.PHONY: dev release serve clean

# emsdk_env.sh determines its own location via $BASH_SOURCE, so this Makefile
# requires bash, not POSIX sh.
SHELL := /bin/bash

DIST  := dist
EMSDK := third_party/emsdk
Z3    := third_party/z3

# Each recipe that needs emcc/emcmake must source emsdk_env.sh first because
# Make spawns a fresh shell per recipe line.
EMSDK_ENV := source $(EMSDK)/emsdk_env.sh >/dev/null

dev: $(DIST)/index.html $(DIST)/z3.wasm
	wasm-pack build --dev --target web --out-dir $(DIST)/pkg

release: $(DIST)/index.html $(DIST)/z3.wasm
	wasm-pack build --release --target web --out-dir $(DIST)/pkg

serve: dev
	python3 -m http.server --directory $(DIST) 8000

# libz3.a — the static Z3 library, built with emscripten. Single-threaded so
# the page works without SharedArrayBuffer / COOP+COEP headers.
#
# Depends on the Makefile so flag edits (e.g. changing -DZ3_* options) trigger
# a reconfigure + rebuild. Z3 source edits aren't auto-detected — touch the
# Makefile or `make clean` to pick those up.
$(Z3)/build/libz3.a: Makefile
	$(EMSDK_ENV) && \
	  emcmake cmake -S $(Z3) -B $(Z3)/build \
	    -DZ3_BUILD_LIBZ3_SHARED=OFF \
	    -DZ3_SINGLE_THREADED=ON \
	    -DCMAKE_BUILD_TYPE=Release && \
	  cmake --build $(Z3)/build -j$$(nproc) --target libz3

# Link libz3.a into z3.{js,wasm}. emcc needs at least one TU; we feed it
# /dev/null as C. All Z3 API symbols come from libz3.a via EXPORTED_FUNCTIONS.
# ccall handles string marshalling on the wasm stack, so we don't export
# _malloc/_free.
$(DIST)/z3.wasm: $(Z3)/build/libz3.a | $(DIST)
	$(EMSDK_ENV) && emcc -x c /dev/null $(Z3)/build/libz3.a \
	    -O2 \
	    -s WASM_BIGINT \
	    -s MODULARIZE=1 \
	    -s EXPORT_NAME=initZ3 \
	    -s EXPORTED_FUNCTIONS='["_Z3_mk_config","_Z3_mk_context","_Z3_del_config","_Z3_eval_smtlib2_string","_Z3_del_context"]' \
	    -s EXPORTED_RUNTIME_METHODS='["ccall"]' \
	    -s FILESYSTEM=0 \
	    -s INITIAL_MEMORY=64MB \
	    -s ALLOW_MEMORY_GROWTH=1 \
	    -s MAXIMUM_MEMORY=2GB \
	    -s TOTAL_STACK=16MB \
	    -o $(DIST)/z3.js

# Symlink public/index.html into dist/ so HTML edits show up without rebuilding.
$(DIST)/index.html: public/index.html | $(DIST)
	ln -sfn ../public/index.html $@

$(DIST):
	mkdir -p $@

clean:
	rm -rf target $(DIST)
