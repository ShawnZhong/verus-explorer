# verus-explorer convenience targets.
#
# Usage:
#   make          # dev build into dist/ (fast, skips wasm-opt)
#   make release  # optimized build into dist/
#   make serve    # dev build + serve dist/ on :8000
#   make deploy   # release build + push dist/ to origin/gh-pages
#   make clean    # remove build artifacts (keeps third_party/)

.PHONY: dev release serve deploy clean

# emsdk_env.sh determines its own location via $BASH_SOURCE, so this Makefile
# requires bash, not POSIX sh.
SHELL := /bin/bash

DIST  := dist
BUILD := build
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

# dist/ is itself a git worktree on gh-pages, so the build writes straight
# into the tree we publish. wasm-pack drops an ignore-everything .gitignore
# in pkg/ on each build — remove it so the built artefacts actually commit.
deploy: release
	@sha=$$(git rev-parse --short HEAD); \
	rm -f $(DIST)/pkg/.gitignore; \
	cd $(DIST) && git add -A && git commit -m "deploy $$sha" && \
	  git push origin gh-pages

# libz3.a — the static Z3 library, built with emscripten. Single-threaded so
# the page works without SharedArrayBuffer / COOP+COEP headers. Flag edits
# aren't auto-detected by Make; `make clean` or `rm -rf build` to pick up
# changes to the CMake options below.
$(BUILD)/libz3.a:
	$(EMSDK_ENV) && \
	  emcmake cmake -S $(Z3) -B $(BUILD) \
	    -DZ3_BUILD_LIBZ3_SHARED=OFF \
	    -DZ3_SINGLE_THREADED=ON \
	    -DCMAKE_BUILD_TYPE=Release && \
	  cmake --build $(BUILD) -j$$(nproc) --target libz3

# Link libz3.a into z3.{js,wasm}. emcc needs at least one TU; we feed it
# /dev/null as C. All Z3 API symbols come from libz3.a via EXPORTED_FUNCTIONS.
# ccall handles string marshalling on the wasm stack, so we don't export
# _malloc/_free.
$(DIST)/z3.wasm: $(BUILD)/libz3.a
	$(EMSDK_ENV) && emcc -x c /dev/null $(BUILD)/libz3.a \
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

# Copy (not symlink) because dist/ is a git worktree we push to Pages — a
# symlink to ../public/index.html would dangle on the gh-pages branch.
$(DIST)/index.html: public/index.html
	cp $< $@

clean:
	rm -rf target $(BUILD)
