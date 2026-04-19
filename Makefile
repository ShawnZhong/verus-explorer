# verus-explorer convenience targets.
#
# Usage:
#   make          # dev build into dist/ (fast, skips wasm-opt)
#   make release  # optimized build into dist/
#   make serve    # dev build + serve dist/ on :8000
#   make deploy   # release build + push dist/ to origin/gh-pages
#   make verus-host  # build the host rust_verify driver (used by build.rs)
#   make clean    # remove build artifacts (keeps third_party/)

.PHONY: dev release serve deploy verus-host clean

DIST  := dist
BUILD := build
# wasm-pack's staging directory, kept separate from $(DIST) so its post-build
# wasm-opt pass only sees our own bundle — otherwise it chokes on
# $(DIST)/z3.wasm, which uses the WebAssembly exception-handling proposal.
PKG   := $(DIST)/pkg

# Host artifacts that build.rs invokes against vstd source.
VERUS_HOST_DIR := third_party/verus/source/target/release
VERUS_HOST_ARTIFACTS := \
  $(VERUS_HOST_DIR)/rust_verify \
  $(VERUS_HOST_DIR)/libverus_builtin_macros.dylib \
  $(VERUS_HOST_DIR)/libverus_state_machines_macros.dylib

# `rustc-rlibs` is a wasm32-only path dep of this crate (see Cargo.toml), so
# wasm-pack's single cargo invocation resolves features across both trees
# in one pass and builds every rustc_* wasm32 rlib into
# `target/wasm32-unknown-unknown/<profile>/deps` — where the explorer's
# `extern crate rustc_*;` lookups resolve them via the `-L dependency=...`
# rustflag in `.cargo/config.toml`.
dev release: $(DIST)/index.html $(DIST)/z3.js $(DIST)/z3.wasm verus-host
	wasm-pack build --$@ --target web --out-dir $(PKG) --no-typescript
	mv $(PKG)/verus_explorer_bg.wasm $(PKG)/verus_explorer.js $(DIST)/
	rm -rf $(PKG)

# Build the host rust_verify driver + its proc-macro dylibs. build.rs picks
# these up to compile vstd into a wasm32 rmeta + serialized VIR.
#
# `--cfg=verus_keep_ghost` flips `cfg_erase()` in verus_builtin_macros from
# the always-EraseAll fallback to the smart `expand_expr`-based version that
# consults the target crate's cfg. Without it, vstd typecheck fails with ~85
# E0603 "private import" errors because every ghost-only `pub use` gets
# erased before name resolution can see it.
verus-host: $(VERUS_HOST_ARTIFACTS)
$(VERUS_HOST_ARTIFACTS) &:
	cargo build \
	  --manifest-path third_party/verus/source/Cargo.toml \
	  -p rust_verify \
	  --release
	# Emit `--emit=link,metadata` for the proc-macro crates so a standalone
	# .rmeta lands next to the dylib in target/release/deps/. build.rs picks
	# the verus_builtin_macros .rmeta out of the deps dir and bundles it
	# into the virtual sysroot — that way vstd.rmeta's dependency entry for
	# verus_builtin_macros (with its host-side stable_crate_id + SVH) lines
	# up exactly with what user-code rustc-in-wasm later finds. A wasm32
	# shim of the same source has different hashes and trips E0460/E0786.
	cargo rustc \
	  --manifest-path third_party/verus/source/Cargo.toml \
	  -p verus_builtin_macros \
	  --release \
	  -- --emit=link,metadata
	cargo rustc \
	  --manifest-path third_party/verus/source/Cargo.toml \
	  -p verus_state_machines_macros \
	  --release \
	  -- --emit=link,metadata

$(DIST)/z3.%: $(BUILD)/z3.% | $(DIST)
	cp $< $@
$(DIST)/index.html: public/index.html | $(DIST)
	cp $< $@

$(DIST):
	git worktree add --orphan -b gh-pages $(DIST)

$(BUILD)/z3.js $(BUILD)/z3.wasm &: scripts/build-z3.sh
	./scripts/build-z3.sh

serve:
	python3 -m http.server --directory $(DIST) 8000

# Each deploy re-creates gh-pages as a single-commit orphan branch in dist/
# and force-pushes, so there's no history either locally or remotely.
deploy: release
	cd $(DIST) && \
	git checkout --orphan _deploy && \
	git add -A && \
	git commit -m "deploy $$(git -C .. rev-parse --short HEAD)" && \
	git branch -M gh-pages && \
	git push --force origin gh-pages

clean:
	rm -rf target $(BUILD)
