# verus-explorer convenience targets.
#
# Usage:
#   make           # dev build into dist/ (fast, skips wasm-opt)
#   make release   # optimized build into dist/
#   make serve     # dev build + serve dist/ on :8000
#   make deploy    # release build + push dist/ to origin/gh-pages
#   make test      # run headless-browser wasm-bindgen tests (needs chrome/chromedriver)
#   make host-rust # build patched stage1 rustc → target/host-rust/ (slow, ~10min; rare)
#   make host-verus# build host rust_verify driver → target/host-verus/release/
#   make clean     # remove cargo + wasm-z3 (keeps host-rust + host-verus)
#   make distclean # also remove host-rust + host-verus (full nuke)
#
# Build artifact layout (all under target/):
#   target/cargo/      cargo workspace (debug, release, wasm32-unknown-unknown)
#   target/host-rust/  patched rustc staged sysroot (RUSTC env points here)
#   target/host-verus/ rust_verify + verus macro crates (host build)
#   target/wasm-z3/    z3.{js,wasm} for the in-browser SMT runtime

.PHONY: dev release serve deploy test host-rust host-verus clean distclean

DIST  := dist
WASM_Z3 := target/wasm-z3
# wasm-pack's staging directory, kept separate from $(DIST) so its post-build
# wasm-opt pass only sees our own bundle — otherwise it chokes on
# $(DIST)/z3.wasm, which uses the WebAssembly exception-handling proposal.
PKG   := $(DIST)/pkg

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
dev release: $(DIST)/index.html $(DIST)/z3.js $(DIST)/z3.wasm host-verus
	wasm-pack build --$@ --target web --out-dir $(PKG) --no-typescript
	mv $(PKG)/verus_explorer_bg.wasm $(PKG)/verus_explorer.js $(DIST)/
	rm -rf $(PKG)
	# Copy the virtual-sysroot files (one rmeta per extern crate, plus
	# vstd.vir and manifest.json) that `build.rs` emitted into OUT_DIR.
	# `OUT_DIR` isn't exported by cargo, so reach under target/ directly —
	# the location is stable across cargo versions for this crate layout.
	rm -rf $(DIST)/sysroot
	cp -r target/cargo/wasm32-unknown-unknown/$(if $(filter release,$@),release,debug)/build/verus-explorer-*/out/sysroot $(DIST)/sysroot

# Build the patched stage1 rustc. Slow (~10 min) and rarely needed (only
# when third_party/rust source changes). Phony so each invocation re-runs
# x.py — cargo-style incremental skipping is x.py's job.
host-rust:
	./scripts/build-host-rust.sh

# Build the host rust_verify driver. build.rs's wasm-libs script invokes
# rust_verify to compile vstd → wasm32 rmeta + .vir, both of which get
# bundled into the virtual sysroot. Phony so each invocation re-checks via
# cargo (cargo itself skips work when nothing changed).
host-verus:
	./scripts/build-host-verus.sh

$(DIST)/z3.%: $(WASM_Z3)/z3.% | $(DIST)
	cp $< $@
$(DIST)/index.html: public/index.html | $(DIST)
	cp $< $@

$(DIST):
	git worktree add --orphan -b gh-pages $(DIST)

$(WASM_Z3)/z3.js $(WASM_Z3)/z3.wasm &: scripts/build-z3.sh
	./scripts/build-z3.sh

serve:
	python3 -m http.server --directory $(DIST) 8000

# Node-hosted run of `tests/smoke.rs`. Tests call `parse_source` with
# `verify = false` so the AIR → Z3 stage is skipped — no Z3 shims needed.
# We run under Node (not headless Chrome) because `wasm-bindgen-test-runner`'s
# web server only serves the test bundle, so the browser path can't fetch
# the ~60 MB of staged rmetas + `vstd.vir`. Under Node, the test reads them
# straight off disk from `SYSROOT_DIR` (emitted by `build.rs`).
test: host-verus
	wasm-pack test --node

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
# Use `make distclean` for a full nuke.
clean:
	rm -rf target/cargo $(WASM_Z3)

distclean: clean
	rm -rf target/host-rust target/host-verus
