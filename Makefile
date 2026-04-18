# verus-explorer convenience targets.
#
# Usage:
#   make          # dev build into dist/ (fast, skips wasm-opt)
#   make release  # optimized build into dist/
#   make serve    # dev build + serve dist/ on :8000
#   make deploy   # release build + push dist/ to origin/gh-pages
#   make clean    # remove build artifacts (keeps third_party/)

.PHONY: dev release serve deploy clean

DIST  := dist
BUILD := build
# wasm-pack's staging directory, kept separate from $(DIST) so its post-build
# wasm-opt pass only sees our own bundle — otherwise it chokes on
# $(DIST)/z3.wasm, which uses the WebAssembly exception-handling proposal.
PKG   := $(DIST)/pkg

# Build the rustc-rlibs workspace member first so the wasm32 rlibs of the
# rustc compiler crates exist in target/wasm32-unknown-unknown/<profile>/deps,
# where the explorer's `extern crate rustc_*;` resolves them via the
# `-L dependency=...` rustflag in `.cargo/config.toml`.
dev release: $(DIST)/index.html $(DIST)/z3.js $(DIST)/z3.wasm
	cd rustc-rlibs && cargo build --target wasm32-unknown-unknown $(if $(filter release,$@),--release,)
	wasm-pack build --$@ --target web --out-dir $(PKG) --no-typescript
	mv $(PKG)/verus_explorer_bg.wasm $(PKG)/verus_explorer.js $(DIST)/
	rm -rf $(PKG)

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
