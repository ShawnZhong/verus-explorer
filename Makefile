# verus-explorer convenience targets.
#
# Usage:
#   make          # dev build into dist/ (fast, skips wasm-opt)
#   make release  # optimized build into dist/
#   make serve    # dev build + serve dist/ on :8000
#   make deploy   # release build + push dist/ to origin/gh-pages
#   make test     # run headless-browser wasm-bindgen tests (needs chrome/chromedriver)
#   make verus-host  # build the host rust_verify driver (used by build.rs)
#   make clean    # remove build artifacts (keeps third_party/)

.PHONY: dev release serve deploy test verus-host clean

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

# `verus_keep_ghost` is the one cfg load-bearing for our host build: it
# flips `cfg_erase()` in verus_builtin_macros to its smart `expand_expr`
# variant instead of unconditionally erasing every ghost `pub use` — the
# latter makes vstd typecheck fail with ~85 E0603 "private import" errors.
# `--check-cfg` silences the "unexpected cfg" warnings that would otherwise
# fire from deps that don't declare these names.
VERUS_RUSTFLAGS := --cfg=verus_keep_ghost \
                   --check-cfg=cfg(verus_keep_ghost) \
                   --check-cfg=cfg(verus_keep_ghost_body)

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

# Build the host rust_verify driver + its proc-macro dylibs. build.rs
# extracts the rmeta from each dylib's embedded `.rustc` section to bundle
# into the virtual sysroot — that way vstd.rmeta's dep entries (stable
# crate id + SVH) line up exactly with what user-code rustc-in-wasm later
# finds. One `cargo build` covers all three artifacts because rust_verify
# transitively depends on both proc-macro crates.
verus-host: $(VERUS_HOST_ARTIFACTS)
$(VERUS_HOST_ARTIFACTS) &:
	RUSTFLAGS="$(VERUS_RUSTFLAGS)" cargo build \
	  --manifest-path third_party/verus/source/Cargo.toml \
	  -p rust_verify \
	  -p verus_builtin_macros \
	  -p verus_state_machines_macros \
	  --release

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

# Headless-browser run of `tests/smoke.rs`. Tests call `parse_source` with
# `verify = false` so the AIR → Z3 stage is skipped — no Z3 shims needed.
test: verus-host
	wasm-pack test --chrome --headless

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
