# verus-explorer convenience targets.
#
# Usage:
#   make            # dev build into dist/ (fast, skips wasm-opt)
#   make release    # optimized build into dist/
#   make serve      # dev build + serve dist/ on :8000
#   make deploy     # release build + push dist/ to origin/gh-pages
#   make test       # run headless-browser wasm-bindgen tests (needs chrome/chromedriver)
#   make host-rust  # build patched stage1 rustc → target/host-rust/ (slow, ~10min; rare)
#   make host-verus # build host rust_verify driver → target/host-verus/release/
#   make clean      # remove cargo + libs + wasm-z3 (keeps host-rust + host-verus)
#   make clean-host # remove host-rust + host-verus (slow to rebuild; use sparingly)
#   make clean-dist # remove dist/ (detaches the gh-pages worktree)
#
# Build artifact layout (all under target/):
#   target/cargo/         cargo workspace (debug, release, wasm32-unknown-unknown)
#   target/host-rust/     patched rustc staged sysroot (RUSTC env points here)
#   target/host-verus/    rust_verify + verus macro crates (host build)
#   target/libs-sysroot/  wasm32 sysroot rmetas (core/alloc/std + deps + verus_builtin + macro stubs)
#   target/libs-vir/      per-mode vstd.rmeta + vstd.vir (std/ and nostd/)
#   target/libs/          browser-shipped bundle (shared rmetas at root, mode-specific under std/ and nostd/, + .gz siblings)
#   target/wasm-z3/       z3.{js,wasm} for the in-browser SMT runtime

.PHONY: dev release serve deploy test libs clean clean-host clean-dist

DIST  := dist
WASM_Z3 := target/wasm-z3

# `public/` holds only browser-servable files — HTML, CSS, examples. The
# whole tree copies into `dist/` verbatim (the `$(DIST)/public.stamp`
# recipe). JS tooling (esbuild entry + npm manifest + node_modules) lives
# in `scripts/editor/` so it doesn't need to be hand-excluded from that copy.
PUBLIC_FILES := $(shell find public -type f)

# Patched rustc staged at target/host-rust/ by build-host-rust.sh. Injected
# into every cargo/wasm-pack invocation here (and the build scripts do the
# same) so cargo uses our rustc without needing rustup-toolchain-link or
# rustup-override. Cargo itself still comes from the rustup-shipped channel
# in rust-toolchain.toml.
export RUSTC := $(CURDIR)/target/host-rust/bin/rustc

# `rustc-rlibs` is a wasm32-only path dep of this crate (see Cargo.toml), so
# wasm-pack's single cargo invocation resolves features across both trees
# in one pass and builds every rustc_* wasm32 rlib into
# `target/cargo/wasm32-unknown-unknown/<profile>/deps` — where the explorer's
# `extern crate rustc_*;` lookups resolve them via the `-L dependency=...`
# rustflag in `.cargo/config.toml`.
#
# wasm-pack's post-build wasm-opt pass runs on every `*.wasm` in its
# `--out-dir`, so we tuck z3.{js,wasm} into `$(DIST)/z3/` to keep them out
# of sight — otherwise wasm-opt chokes on z3.wasm's exception-handling
# tags. Emscripten's MODULARIZE glue resolves z3.wasm relative to z3.js's
# own `document.currentScript?.src`, so the subfolder move is transparent
# as long as `public/index.html` loads `./z3/z3.js`.
# `dev` + `release` share a recipe, differing only in the wasm-pack
# profile flag (`$@` expands to the invoked target name). `rm -f
# dist/{package.json,.gitignore}` drops the bundler-flavored files
# wasm-pack emits for npm publish — noise for static-site hosting.
# The libs staging copies only the gzipped siblings; raw rmetas stay
# in target/libs/ for tests/smoke.rs.
dev release: $(DIST)/public.stamp $(DIST)/editor.js $(DIST)/z3/z3.js $(DIST)/z3/z3.wasm libs
	wasm-pack build verus-explorer --$@ --target web --out-dir $(CURDIR)/$(DIST) --no-typescript
	rm -f $(DIST)/package.json $(DIST)/.gitignore
	rm -rf $(DIST)/libs
	mkdir -p $(DIST)/libs/nostd $(DIST)/libs/std $(DIST)/libs/exec
	cp $(LIBS_DIR)/nostd/*.gz $(DIST)/libs/nostd/
	cp $(LIBS_DIR)/std/*.gz   $(DIST)/libs/std/
	cp $(LIBS_DIR)/exec/*.gz  $(DIST)/libs/exec/

# Build the patched stage1 rustc. Slow (~10 min first time, rare beyond
# that). File target rather than phony, because the script's staging
# copies fail if x.py has been run between `./x.py build` + `./x.py
# dist rustc-dev` in build-host-rust.sh and an intervening `x.py check`
# (from libs-sysroot) that mutated the stage1-host output dir. Once
# target/host-rust/bin/rustc exists, make treats it as up-to-date and
# skips the re-run. Force a rebuild via `make clean-host`.
HOST_RUST_BIN := target/host-rust/bin/rustc
$(HOST_RUST_BIN):
	./scripts/build-host-rust.sh
host-rust: $(HOST_RUST_BIN)
.PHONY: host-rust

# Build the host rust_verify driver. The `libs-vir` target below
# invokes rust_verify to compile vstd → wasm32 rmeta + .vir, both of
# which get bundled into the libs directory. File target rather
# than phony: once the binary exists, make treats it as up-to-date and
# the cargo-rebuild overhead disappears from every `make dev`/`release`.
# If Verus source changes, delete the binary (or `make clean-host`) to
# force a rebuild.
HOST_VERUS_BIN := target/host-verus/release/rust_verify
$(HOST_VERUS_BIN):
	./scripts/build-host-verus.sh
host-verus: $(HOST_VERUS_BIN)

# libs pipeline: single stamp target, orchestration lives in
# `scripts/build-libs.sh` (which calls build-libs-sysroot.sh,
# build-libs-vir.sh std, build-libs-vir.sh nostd, then assembles the
# browser bundle). Dep list is the union of every script input that
# could invalidate the bundle — when any of these is newer than
# $(LIBS_STAMP), Make re-runs the orchestrator which rebuilds every
# stage unconditionally. Stages don't need their own stamps because
# they always rebuild together on any upstream change.
VSTD_SOURCES := $(shell find third_party/verus/source/vstd -name '*.rs' 2>/dev/null)
SYSROOT_EXTRA_SOURCES := $(shell find third_party/verus/source/builtin \
    third_party/verus/source/builtin_macros \
    third_party/verus/source/state_machines_macros \
    -name '*.rs' 2>/dev/null)
LIBS_DIR   := target/libs
LIBS_STAMP := $(LIBS_DIR)/.stamp
$(LIBS_STAMP): $(HOST_RUST_BIN) $(HOST_VERUS_BIN) \
		$(VSTD_SOURCES) $(SYSROOT_EXTRA_SOURCES) \
		scripts/build-libs.sh scripts/build-libs-sysroot.sh scripts/build-libs-vir.sh
	./scripts/build-libs.sh
	@touch $@

libs: $(LIBS_STAMP)

$(DIST)/z3/z3.%: $(WASM_Z3)/z3.% | $(DIST)
	mkdir -p $(DIST)/z3
	cp $< $@
# Short git hash baked into the dist copy of index.html, so the header's
# "Code" link lands on the exact deployed source tree. `|| echo main`
# fallback covers the rare `make` run outside a git checkout.
GIT_HASH := $(shell git rev-parse --short HEAD 2>/dev/null || echo main)
$(DIST)/public.stamp: $(PUBLIC_FILES) | $(DIST)
	cp -R public/. $(DIST)/
	sed -i '' "s|__COMMIT__|$(GIT_HASH)|g" $(DIST)/index.html
	@touch $@

# Bundle CodeMirror 6 straight into `$(DIST)/editor.js` via esbuild. The
# entry point `scripts/editor/editor-src.js` re-exports every CM6 symbol
# that `public/index.html` imports; esbuild resolves the bare specifiers
# against `scripts/editor/node_modules/` and emits one minified ESM bundle
# with all of CM6's transitive deps (~470KB) inlined.
# `scripts/editor/node_modules/.stamp` forces `npm install` on first build.
$(DIST)/editor.js: scripts/editor/editor-src.js scripts/editor/node_modules/.stamp | $(DIST)
	scripts/editor/node_modules/.bin/esbuild $< --bundle --format=esm --outfile=$@ --minify --target=es2022

$(DIST):
	git worktree add --orphan -b gh-pages $(DIST)

$(WASM_Z3)/z3.js $(WASM_Z3)/z3.wasm &: scripts/build-z3.sh
	./scripts/build-z3.sh

# `npm install` gate. `scripts/editor/node_modules/.stamp` is the Make
# witness so the install only reruns when `scripts/editor/package.json`
# bumps. `npm ci` would be stricter (fails on lockfile drift) but
# `npm install` tolerates a missing lockfile on fresh clones and
# refreshes it when deps bump.
scripts/editor/node_modules/.stamp: scripts/editor/package.json
	npm install --prefix scripts/editor --no-audit --no-fund
	@touch $@

serve:
	python3 -m http.server --directory $(DIST) 8000

# Node-hosted run of `tests/smoke.rs`. Tests call `parse_source` with
# `verify = false` so the AIR → Z3 stage is skipped — no Z3 shims needed.
# We run under Node (not headless Chrome) because `wasm-bindgen-test-runner`'s
# web server only serves the test bundle, so the browser path can't fetch
# the ~60 MB of staged rmetas + `vstd.vir`. Under Node, the test reads them
# straight off disk from `target/libs/` (path derived at compile time
# from `CARGO_MANIFEST_DIR`).
#
# `wasm-bindgen-test-runner` invokes `node` via plain `Command::new("node")`
# (pure PATH lookup — no override env var), so prepending the vendored
# `scripts/editor/node/bin` here pins the Node version regardless of what's
# in the user's PATH. The directory is gitignored; populate it by
# extracting an official node tarball, e.g. on Apple Silicon:
#   curl -sL https://nodejs.org/dist/v24.15.0/node-v24.15.0-darwin-arm64.tar.gz \
#     | tar xz -C scripts/editor && mv scripts/editor/node-v24.15.0-darwin-arm64 scripts/editor/node
test: $(HOST_VERUS_BIN)
	PATH="$(CURDIR)/scripts/editor/node/bin:$$PATH" wasm-pack test --node verus-explorer

# Each deploy re-creates gh-pages as a single-commit orphan branch in dist/
# and force-pushes, so there's no history either locally or remotely.
deploy: release
	cd $(DIST) && \
	git checkout --orphan _deploy && \
	git add -A && \
	git commit -m "deploy $$(git -C .. rev-parse --short HEAD)" && \
	git branch -M gh-pages && \
	git push --force origin gh-pages

# Spare host-rust (~10 min stage1 rebuild) and host-verus (~2 min cargo
# build of rust_verify) — both are stable across normal wasm iteration.
# `clean-host` / `clean-dist` are opt-in for their specific nukes.
clean:
	rm -rf target/cargo target/libs target/libs-sysroot target/libs-vir $(WASM_Z3)

clean-host:
	rm -rf target/host-rust target/host-verus

# `dist/` is a git worktree on the gh-pages branch (see the `$(DIST):`
# recipe above), so detach via `git worktree remove` rather than a raw
# `rm -rf`, which would leak the worktree reference in `.git/worktrees/`.
# Also delete the local `gh-pages` branch — otherwise the next
# `make dev`/`release` fails at `git worktree add --orphan -b gh-pages`
# with "a branch named 'gh-pages' already exists". `deploy` force-pushes
# an orphan branch anyway, so nothing of value lives on the local branch.
# Leading `-` on each line so make keeps going if the worktree or branch
# doesn't exist yet.
clean-dist:
	-git worktree remove --force $(DIST)
	-git branch -D gh-pages
	rm -rf $(DIST)
